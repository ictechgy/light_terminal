use crate::paths;
use crate::protocol::{
    DaemonStatus, MAX_SEND_DATA_BYTES, PROTOCOL_VERSION, Request, Response, SessionInfo,
    StatusTheme, WaitContainsResult, WaitExitResult,
};
use crate::sanitize;
use anyhow::{Context, Result, anyhow, bail};
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use crossterm::terminal::ClearType;
use crossterm::{cursor, execute, queue, terminal};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::json;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, BufWriter, ErrorKind, IsTerminal, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const MAX_RPC_RESPONSE_BYTES: u64 = 64 * 1024 * 1024;
const ATTACH_RESPONSE_HEADER_LIMIT: usize = 64 * 1024;
const MAX_DAEMON_LOG_BYTES: u64 = 10 * 1024 * 1024;
const DEFAULT_TRACE_MAX_BYTES: u64 = 16 * 1024 * 1024;
const TRACE_FORMAT: &str = "lterm-trace-jsonl";
const TRACE_SCHEMA_VERSION: &str = "1.0";
const MAX_TRACE_JSONL_LINE_BYTES: usize = 1024 * 1024;
const MAX_TRACE_REPLAY_CHUNK_BYTES: u64 = 1024 * 1024;
const MAX_TRACE_REPLAY_TOTAL_BYTES: u64 = DEFAULT_TRACE_MAX_BYTES;
const MAX_TRACE_REPLAY_CHUNKS: u64 = 16 * 1024;
const MAX_TRACE_REPLAY_DELAY_MS: u64 = 60 * 1000;
const RPC_TIMEOUT: Duration = Duration::from_secs(5);
static VERSION_STATUS_CHECKED: AtomicBool = AtomicBool::new(false);
/// Status bar self-heal 주기. cmux/Termius 등에서 다른 앱→복귀 시 외부에서 DECSTBM이
/// 리셋되어도 사용자 인지 한계(약 100~300ms) 안에 scroll region을 재확립하고 status를
/// 재그린다. PTY가 활발히 출력 중일 때 idle heartbeat는 status_dirty가 클리어되어야 발화하므로,
/// busy 출력 시에는 [`STATUS_HEARTBEAT_FORCED`] 가 dirty 여부와 무관하게 강제 redraw한다.
const STATUS_HEARTBEAT: Duration = Duration::from_millis(250);
/// busy PTY 출력으로 [`STATUS_HEARTBEAT`] idle 경로가 차단된 경우(WouldBlock이 fire하지
/// 않아 status_dirty가 클리어되지 않음) self-heal이 영원히 막히지 않게 강제 발화하는 상한.
/// 사용자 보고: cmux/agent prompt 입력 중 status repaint가 시각적 깜빡임으로 보일 수
/// 있어 busy-output self-heal은 idle redraw보다 훨씬 느리게 둔다. 평소에는
/// `ATTACH_OUTPUT_IDLE_TIMEOUT` 경로가 출력이 잠잠해진 직후 status를 복구한다.
const STATUS_HEARTBEAT_FORCED: Duration = Duration::from_secs(2);
/// Attach output idle wakeup. This bounds status-bar redraw latency without the
/// previous 30ms hot poll; heartbeat logic still owns the actual redraw cadence.
const ATTACH_OUTPUT_IDLE_TIMEOUT: Duration = Duration::from_millis(100);
const PS_CANDIDATES: &[&str] = &["/bin/ps", "/usr/bin/ps"];
const STATUS_THEME_PROTOCOL_VERSION: u32 = 2;
const WAIT_PROTOCOL_VERSION: u32 = 3;

pub fn ensure_server() -> Result<()> {
    // Validate the socket path before the optimistic ping. If the socket leaf is
    // a symlink or otherwise untrusted, retrying via auto-spawn only converts a
    // path-level refusal into a slow timeout.
    let _ = paths::socket_path()?;
    if rpc::<serde_json::Value>(&Request::Ping).is_ok() {
        warn_daemon_version_mismatch();
        return Ok(());
    }

    let exe = std::env::current_exe().context("resolve current executable")?;
    let log = paths::log_path()?;
    rotate_log_if_large(&log)?;
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .with_context(|| format!("open daemon log {}", log.display()))?;
    let log_file_err = log_file.try_clone().context("clone daemon log")?;

    let mut daemon = Command::new(exe);
    daemon
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_file_err));
    unsafe {
        daemon.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    daemon.spawn().context("spawn lterm daemon")?;

    let deadline = Instant::now() + Duration::from_secs(3);
    let mut last_err = None;
    while Instant::now() < deadline {
        match rpc::<serde_json::Value>(&Request::Ping) {
            Ok(_) => {
                warn_daemon_version_mismatch();
                return Ok(());
            }
            Err(err) => {
                last_err = Some(err);
                thread::sleep(Duration::from_millis(80));
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("daemon did not become ready. Run `lterm doctor` to inspect daemon state or `lterm shutdown && lterm list` to retry a clean start.")))
}

pub fn daemon_status() -> Result<DaemonStatus> {
    rpc(&Request::Status)
}

pub fn daemon_ping() -> Result<()> {
    rpc::<serde_json::Value>(&Request::Ping).map(|_| ())
}

fn warn_daemon_version_mismatch() {
    if VERSION_STATUS_CHECKED.swap(true, Ordering::SeqCst) {
        return;
    }
    match daemon_status() {
        Ok(status) => {
            if status.version != env!("CARGO_PKG_VERSION")
                || status.protocol_version != PROTOCOL_VERSION
            {
                eprintln!(
                    "warning: lterm client {} (protocol {}) is talking to daemon {} (protocol {}); run `lterm shutdown` and retry after upgrades",
                    env!("CARGO_PKG_VERSION"),
                    PROTOCOL_VERSION,
                    sanitize::terminal_text(&status.version),
                    status.protocol_version
                );
            }
        }
        Err(err) => {
            eprintln!(
                "warning: lterm daemon answered ping but did not return status ({}); it may be from an older lterm build; run `lterm shutdown` and retry after upgrades",
                sanitize::terminal_text(&err.to_string())
            );
        }
    }
}

fn rotate_log_if_large(log: &Path) -> Result<()> {
    if let Ok(meta) = std::fs::metadata(log) {
        if meta.len() > MAX_DAEMON_LOG_BYTES {
            let rotated = log.with_extension("log.1");
            let _ = std::fs::remove_file(&rotated);
            std::fs::rename(log, &rotated)
                .or_else(|_| std::fs::write(log, b""))
                .with_context(|| format!("rotate daemon log {}", log.display()))?;
        }
    }
    Ok(())
}

// 데몬 소켓에 connect하지 못했을 때 모든 호출 지점이 보여줘야 할 사용자 가이드.
// 동일 문구를 RPC 경로와 attach handshake 경로 양쪽에서 일관되게 사용한다.
fn daemon_connect_context(path: &Path) -> String {
    format!(
        "connect to lterm daemon at {} (is the daemon running? it usually auto-starts on the next `lterm` command; run `lterm doctor` if it keeps failing)",
        path.display()
    )
}

pub fn rpc<T: DeserializeOwned>(request: &Request) -> Result<T> {
    rpc_with_read_timeout(request, Some(RPC_TIMEOUT))
}

fn rpc_with_read_timeout<T: DeserializeOwned>(
    request: &Request,
    read_timeout: Option<Duration>,
) -> Result<T> {
    let path = paths::socket_path()?;
    let mut stream = UnixStream::connect(&path).with_context(|| daemon_connect_context(&path))?;
    stream
        .set_read_timeout(read_timeout)
        .context("set rpc read timeout")?;
    stream
        .set_write_timeout(Some(RPC_TIMEOUT))
        .context("set rpc write timeout")?;
    let payload = serde_json::to_vec(request).context("serialize request")?;
    stream.write_all(&payload).context("write request")?;
    stream.write_all(b"\n").context("write request newline")?;
    stream.shutdown(std::net::Shutdown::Write).ok();

    let mut bytes = Vec::new();
    let mut limited = stream.take(MAX_RPC_RESPONSE_BYTES + 1);
    limited.read_to_end(&mut bytes).context("read response")?;
    if bytes.len() as u64 > MAX_RPC_RESPONSE_BYTES {
        bail!(
            "lterm daemon response exceeded {} bytes",
            MAX_RPC_RESPONSE_BYTES
        );
    }
    let response: Response = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse response: {}", String::from_utf8_lossy(&bytes)))?;
    if !response.ok {
        bail!(
            response
                .error
                .unwrap_or_else(|| "lterm daemon error".to_string())
        );
    }
    let value = response.result.unwrap_or(serde_json::Value::Null);
    serde_json::from_value(value).context("decode response result")
}

pub fn new_session(
    name: Option<String>,
    command: Option<String>,
    cwd: Option<String>,
    mut env: std::collections::HashMap<String, String>,
    status_theme: Option<StatusTheme>,
    tmux: bool,
) -> Result<SessionInfo> {
    ensure_server()?;
    if status_theme.is_some() {
        require_status_theme_protocol()?;
    }
    let cwd = Some(resolve_client_cwd(cwd)?);
    let parent = current_parent_request();
    inherit_terminal_capability_env(&mut env);
    rpc(&Request::New {
        name,
        command,
        cwd,
        rows: terminal_rows(),
        cols: terminal_cols(),
        parent_pane_id: parent.as_ref().map(|parent| parent.pane_id.clone()),
        parent_token: parent.map(|parent| parent.token),
        env,
        status_theme,
        tmux,
    })
}

fn inherit_terminal_capability_env(env: &mut std::collections::HashMap<String, String>) {
    for key in TERMINAL_CAPABILITY_ENV {
        if env.contains_key(*key) {
            continue;
        }
        if let Ok(value) = std::env::var(key) {
            if !value.is_empty() {
                env.insert((*key).to_string(), value);
            }
        }
    }
}

const TERMINAL_CAPABILITY_ENV: &[&str] = &[
    "TERM",
    "COLORTERM",
    "TERM_PROGRAM",
    "TERM_PROGRAM_VERSION",
    "LC_TERMINAL",
    "LC_TERMINAL_VERSION",
    "TERMINAL_EMULATOR",
    "VTE_VERSION",
    "KITTY_WINDOW_ID",
    "WEZTERM_EXECUTABLE",
    "WT_SESSION",
    "ITERM_SESSION_ID",
    "TERM_SESSION_ID",
    "NO_COLOR",
    "FORCE_COLOR",
    "CLICOLOR",
    "CLICOLOR_FORCE",
];

pub fn attach_or_new(target: &str) -> Result<SessionInfo> {
    ensure_server()?;
    let parent = current_parent_request();
    rpc(&Request::AttachOrNew {
        target: target.to_string(),
        cwd: Some(resolve_client_cwd(None)?),
        parent_pane_id: parent.as_ref().map(|parent| parent.pane_id.clone()),
        parent_token: parent.map(|parent| parent.token),
        status_theme: None,
    })
}

pub fn set_status_theme(target: &str, status_theme: Option<StatusTheme>) -> Result<SessionInfo> {
    ensure_server()?;
    require_status_theme_protocol()?;
    rpc(&Request::SetStatusTheme {
        target: target.to_string(),
        status_theme,
    })
}

pub fn wait_exit(target: &str, timeout: Option<Duration>) -> Result<WaitExitResult> {
    ensure_server()?;
    require_wait_protocol()?;
    let timeout_ms = timeout.map(duration_millis_u64);
    let read_timeout = timeout.map(|duration| duration.saturating_add(RPC_TIMEOUT));
    rpc_with_read_timeout(
        &Request::WaitExit {
            target: target.to_string(),
            timeout_ms,
        },
        read_timeout,
    )
}

pub fn wait_contains(
    target: &str,
    needle: &str,
    start: Option<i32>,
    timeout: Option<Duration>,
) -> Result<WaitContainsResult> {
    ensure_server()?;
    require_wait_protocol()?;
    let timeout_ms = timeout.map(duration_millis_u64);
    let read_timeout = timeout.map(|duration| duration.saturating_add(RPC_TIMEOUT));
    rpc_with_read_timeout(
        &Request::WaitContains {
            target: target.to_string(),
            needle: needle.to_string(),
            start,
            timeout_ms,
        },
        read_timeout,
    )
}

fn require_status_theme_protocol() -> Result<()> {
    let status = daemon_status().context("check lterm daemon protocol for status themes")?;
    if let Some(message) = status_theme_protocol_error(&status) {
        bail!(message);
    }
    Ok(())
}

fn require_wait_protocol() -> Result<()> {
    let status = daemon_status().context("check lterm daemon protocol for wait/watch")?;
    if let Some(message) = wait_protocol_error(&status) {
        bail!(message);
    }
    Ok(())
}

fn status_theme_protocol_error(status: &DaemonStatus) -> Option<String> {
    (status.protocol_version < STATUS_THEME_PROTOCOL_VERSION).then(|| {
        format!(
            "lterm daemon protocol {} does not support status themes (requires protocol {}); run `lterm shutdown` and retry after upgrading",
            status.protocol_version, STATUS_THEME_PROTOCOL_VERSION
        )
    })
}

fn wait_protocol_error(status: &DaemonStatus) -> Option<String> {
    (status.protocol_version < WAIT_PROTOCOL_VERSION).then(|| {
        format!(
            "lterm daemon protocol {} does not support wait/watch (requires protocol {}); run `lterm shutdown` and retry after upgrading",
            status.protocol_version, WAIT_PROTOCOL_VERSION
        )
    })
}

fn duration_millis_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

pub fn default_trace_max_bytes() -> u64 {
    DEFAULT_TRACE_MAX_BYTES
}

struct ParentRequest {
    pane_id: String,
    token: String,
}

fn current_parent_request() -> Option<ParentRequest> {
    let pane_id = std::env::var("LTERM_PANE")
        .ok()
        .filter(|pane_id| is_lterm_pane_id(pane_id))?;
    let token = std::env::var("LTERM_PARENT_TOKEN")
        .ok()
        .filter(|token| !token.is_empty())?;
    Some(ParentRequest { pane_id, token })
}

fn is_lterm_pane_id(value: &str) -> bool {
    value
        .strip_prefix('%')
        .is_some_and(|digits| !digits.is_empty() && digits.chars().all(|ch| ch.is_ascii_digit()))
}

fn resolve_client_cwd(cwd: Option<String>) -> Result<String> {
    let cwd = match cwd {
        Some(cwd) => PathBuf::from(cwd),
        None => std::env::current_dir().context("resolve current working directory")?,
    };
    let cwd = if cwd.is_absolute() {
        cwd
    } else {
        std::env::current_dir()
            .context("resolve current working directory")?
            .join(cwd)
    };
    cwd.into_os_string()
        .into_string()
        .map_err(|_| anyhow!("lterm cwd must be valid UTF-8"))
}

pub fn list_sessions() -> Result<Vec<SessionInfo>> {
    ensure_server()?;
    rpc(&Request::List)
}

pub fn info(target: &str) -> Result<SessionInfo> {
    ensure_server()?;
    rpc(&Request::Info {
        target: target.to_string(),
    })
}

/// Rename an existing session target and return its updated metadata.
pub fn rename_session(target: &str, name: &str) -> Result<SessionInfo> {
    ensure_server()?;
    rpc(&Request::Rename {
        target: target.to_string(),
        name: name.to_string(),
    })
}

pub fn kill(target: &str) -> Result<()> {
    ensure_server()?;
    rpc::<serde_json::Value>(&Request::Kill {
        target: target.to_string(),
    })?;
    Ok(())
}

pub fn send(target: &str, data: Vec<u8>) -> Result<()> {
    ensure_server()?;
    if data.len() > MAX_SEND_DATA_BYTES {
        bail!("send data exceeds {} bytes", MAX_SEND_DATA_BYTES);
    }
    rpc::<serde_json::Value>(&Request::Send {
        target: target.to_string(),
        data,
    })?;
    Ok(())
}

pub fn capture(target: &str, start: Option<i32>) -> Result<String> {
    capture_range(target, start, None)
}

pub fn capture_range(target: &str, start: Option<i32>, end: Option<i32>) -> Result<String> {
    ensure_server()?;
    rpc(&Request::Capture {
        target: target.to_string(),
        start,
        end,
    })
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TraceEvent {
    Start {
        schema_version: &'static str,
        format: &'static str,
        trace_id: String,
        producer: &'static str,
        client_version: &'static str,
        client_protocol_version: u32,
        target: String,
        created_at_unix_ms: Option<u64>,
        duration_ms: u64,
        max_bytes: u64,
        rows: u16,
        cols: u16,
        raw_stream_policy: &'static str,
    },
    Output {
        chunk_index: u64,
        elapsed_ms: u64,
        direction: &'static str,
        len: usize,
        bytes_hex: String,
    },
    End {
        elapsed_ms: u64,
        reason: &'static str,
        bytes_recorded: u64,
        chunks_recorded: u64,
    },
}

#[derive(Debug, Default, Serialize)]
struct TraceFileSummary {
    path: String,
    schema_version: Option<String>,
    format: Option<String>,
    trace_id: Option<String>,
    producer: Option<String>,
    client_version: Option<String>,
    client_protocol_version: Option<u64>,
    target: Option<String>,
    created_at_unix_ms: Option<u64>,
    duration_ms: Option<u64>,
    max_bytes: Option<u64>,
    rows: Option<u64>,
    cols: Option<u64>,
    raw_stream_policy: Option<String>,
    event_count: u64,
    output_chunks: u64,
    output_bytes: u64,
    first_output_elapsed_ms: Option<u64>,
    last_output_elapsed_ms: Option<u64>,
    end_elapsed_ms: Option<u64>,
    end_reason: Option<String>,
    end_bytes_recorded: Option<u64>,
    end_chunks_recorded: Option<u64>,
    unknown_events: u64,
}

#[derive(Debug)]
struct TraceReplayChunk {
    line_number: usize,
    elapsed_ms: u64,
    bytes: Vec<u8>,
}

#[derive(Debug, Default)]
struct TraceReplayPlan {
    chunks: Vec<TraceReplayChunk>,
    total_bytes: u64,
}

pub fn trace_output(
    target: &str,
    output_path: &Path,
    duration: Duration,
    max_bytes: u64,
    force: bool,
) -> Result<()> {
    ensure_server()?;
    if max_bytes == 0 {
        bail!("trace --max-bytes must be greater than zero");
    }

    let (cols, rows) = terminal_size();
    let path = paths::socket_path()?;
    let mut stream = UnixStream::connect(&path).with_context(|| daemon_connect_context(&path))?;
    stream
        .set_read_timeout(Some(RPC_TIMEOUT))
        .context("set trace handshake read timeout")?;
    stream
        .set_write_timeout(Some(RPC_TIMEOUT))
        .context("set trace handshake write timeout")?;
    let request = Request::Attach {
        target: target.to_string(),
        rows,
        cols,
    };
    stream.write_all(&serde_json::to_vec(&request)?)?;
    stream.write_all(b"\n")?;

    let mut reader = BufReader::with_capacity(8192, stream);
    let header = read_attach_response_header(&mut reader)?;
    let response: Response =
        serde_json::from_slice(&header).context("parse trace attach header")?;
    if !response.ok {
        bail!(
            response
                .error
                .unwrap_or_else(|| "trace attach failed".to_string())
        );
    }
    let mut output_options = OpenOptions::new();
    output_options.write(true).mode(0o600);
    if force {
        ensure_trace_force_target_private(output_path)?;
        output_options.create(true).truncate(true);
    } else {
        output_options.create_new(true);
    }
    output_options.custom_flags(libc::O_NOFOLLOW);
    let output_file = output_options
        .open(output_path)
        .with_context(|| trace_output_open_context(output_path, force))?;
    let mut output = BufWriter::with_capacity(64 * 1024, output_file);

    let started = Instant::now();
    write_trace_event(
        &mut output,
        &TraceEvent::Start {
            schema_version: TRACE_SCHEMA_VERSION,
            format: TRACE_FORMAT,
            trace_id: uuid::Uuid::new_v4().to_string(),
            producer: "lterm",
            client_version: env!("CARGO_PKG_VERSION"),
            client_protocol_version: PROTOCOL_VERSION,
            target: target.to_string(),
            created_at_unix_ms: current_unix_ms(),
            duration_ms: duration_millis_u64(duration),
            max_bytes,
            rows,
            cols,
            raw_stream_policy: "raw-transparent",
        },
    )?;
    output.flush().context("flush trace start event")?;

    let deadline = started + duration;
    let mut reason = "duration";
    let mut raw_bytes_recorded = 0_u64;
    let mut chunks_recorded = 0_u64;
    let mut buf = [0_u8; 8192];
    while Instant::now() < deadline {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let remaining_duration = deadline.saturating_duration_since(now);
        let read_timeout = remaining_duration.min(ATTACH_OUTPUT_IDLE_TIMEOUT);
        reader
            .get_ref()
            .set_read_timeout(Some(read_timeout))
            .context("set trace output read timeout")?;
        match reader.read(&mut buf) {
            Ok(0) => {
                reason = "eof";
                break;
            }
            Ok(n) => {
                let remaining_bytes = max_bytes.saturating_sub(raw_bytes_recorded);
                if remaining_bytes == 0 {
                    reason = "max_bytes";
                    break;
                }
                let to_write = n.min(usize::try_from(remaining_bytes).unwrap_or(usize::MAX));
                write_trace_event(
                    &mut output,
                    &TraceEvent::Output {
                        chunk_index: chunks_recorded,
                        elapsed_ms: duration_millis_u64(started.elapsed()),
                        direction: "stdout",
                        len: to_write,
                        bytes_hex: hex_encode(&buf[..to_write]),
                    },
                )?;
                raw_bytes_recorded += u64::try_from(to_write).unwrap_or(u64::MAX);
                chunks_recorded = chunks_recorded.saturating_add(1);
                if to_write < n || raw_bytes_recorded >= max_bytes {
                    reason = "max_bytes";
                    break;
                }
            }
            Err(err)
                if err.kind() == ErrorKind::Interrupted
                    || err.kind() == ErrorKind::WouldBlock
                    || err.kind() == ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(err) => return Err(err).context("read trace output"),
        }
    }
    write_trace_event(
        &mut output,
        &TraceEvent::End {
            elapsed_ms: duration_millis_u64(started.elapsed()),
            reason,
            bytes_recorded: raw_bytes_recorded,
            chunks_recorded,
        },
    )?;
    output.flush().context("flush trace output")?;
    Ok(())
}

pub fn replay_trace(input_path: &Path, timing: bool) -> Result<()> {
    let plan = validate_trace_replay(input_path, timing)?;
    let mut stdout = std::io::stdout().lock();
    let mut previous_elapsed_ms = 0_u64;

    for chunk in plan.chunks {
        if timing {
            let delay_ms = chunk.elapsed_ms.saturating_sub(previous_elapsed_ms);
            if delay_ms > 0 {
                thread::sleep(Duration::from_millis(delay_ms));
            }
        }
        previous_elapsed_ms = chunk.elapsed_ms;
        stdout.write_all(&chunk.bytes).with_context(|| {
            format!(
                "replay trace bytes from {} line {}",
                input_path.display(),
                chunk.line_number
            )
        })?;
        if timing {
            stdout.flush().with_context(|| {
                format!("flush timed trace chunk from {}", input_path.display())
            })?;
        }
    }

    stdout
        .flush()
        .with_context(|| format!("flush replayed trace {}", input_path.display()))
}

fn validate_trace_replay(input_path: &Path, timing: bool) -> Result<TraceReplayPlan> {
    let file = std::fs::File::open(input_path)
        .with_context(|| format!("open trace file {}", input_path.display()))?;
    let mut reader = BufReader::new(file);
    let mut start_seen = false;
    let mut end_seen = false;
    let mut previous_elapsed_ms = 0_u64;
    let mut expected_chunk_index = 0_u64;
    let mut replayed_bytes = 0_u64;
    let mut line_number = 0_usize;
    let mut plan = TraceReplayPlan::default();

    while let Some(line) = read_trace_jsonl_line(&mut reader, &mut line_number, input_path)? {
        if line.trim().is_empty() {
            continue;
        }
        let event: serde_json::Value = serde_json::from_str(&line).with_context(|| {
            format!(
                "parse trace line {line_number} from {}",
                input_path.display()
            )
        })?;
        match required_trace_str(&event, "type", input_path, line_number)? {
            "start" => {
                if start_seen {
                    bail!(
                        "trace line {} from {} contains a duplicate start event",
                        line_number,
                        input_path.display()
                    );
                }
                if end_seen || expected_chunk_index != 0 {
                    bail!(
                        "trace line {} from {} contains start after output/end",
                        line_number,
                        input_path.display()
                    );
                }
                validate_trace_start_event(&event, input_path, line_number)?;
                required_trace_u64(&event, "duration_ms", input_path, line_number)?;
                start_seen = true;
            }
            "output" => {
                if !start_seen {
                    bail!(
                        "trace line {} from {} contains output before start",
                        line_number,
                        input_path.display()
                    );
                }
                if end_seen {
                    bail!(
                        "trace line {} from {} contains output after end",
                        line_number,
                        input_path.display()
                    );
                }
                let direction = required_trace_str(&event, "direction", input_path, line_number)?;
                if direction != "stdout" {
                    bail!(
                        "trace line {} from {} has unsupported output direction {:?}",
                        line_number,
                        input_path.display(),
                        direction
                    );
                }
                if let Some(chunk_index) = optional_trace_u64(&event, "chunk_index") {
                    if chunk_index != expected_chunk_index {
                        bail!(
                            "trace line {} from {} has chunk_index {} but expected {}",
                            line_number,
                            input_path.display(),
                            chunk_index,
                            expected_chunk_index
                        );
                    }
                } else if event.get("chunk_index").is_some() {
                    bail!(
                        "trace line {} from {} has non-u64 field chunk_index",
                        line_number,
                        input_path.display()
                    );
                }
                if expected_chunk_index >= MAX_TRACE_REPLAY_CHUNKS {
                    bail!(
                        "trace replay chunk count exceeds safety cap {}",
                        MAX_TRACE_REPLAY_CHUNKS
                    );
                }
                let elapsed_ms = required_trace_u64(&event, "elapsed_ms", input_path, line_number)?;
                if elapsed_ms < previous_elapsed_ms {
                    bail!(
                        "trace line {} from {} has non-monotonic elapsed_ms {} after {}",
                        line_number,
                        input_path.display(),
                        elapsed_ms,
                        previous_elapsed_ms
                    );
                }
                let bytes_hex = required_trace_str(&event, "bytes_hex", input_path, line_number)?;
                let encoded_len = hex_encoded_len(bytes_hex).with_context(|| {
                    format!(
                        "validate bytes_hex on trace line {line_number} from {}",
                        input_path.display()
                    )
                })?;
                if encoded_len > MAX_TRACE_REPLAY_CHUNK_BYTES {
                    bail!(
                        "trace line {} from {} decodes to {} bytes, exceeding replay chunk limit {}",
                        line_number,
                        input_path.display(),
                        encoded_len,
                        MAX_TRACE_REPLAY_CHUNK_BYTES
                    );
                }
                let expected_len = required_trace_u64(&event, "len", input_path, line_number)?;
                if expected_len != encoded_len {
                    bail!(
                        "trace line {} from {} has len {} but bytes_hex decodes to {} bytes",
                        line_number,
                        input_path.display(),
                        expected_len,
                        encoded_len
                    );
                }
                if plan.total_bytes.saturating_add(expected_len) > MAX_TRACE_REPLAY_TOTAL_BYTES {
                    bail!(
                        "trace replay total bytes exceed safety cap {}",
                        MAX_TRACE_REPLAY_TOTAL_BYTES
                    );
                }
                let bytes = hex_decode(bytes_hex).with_context(|| {
                    format!(
                        "decode bytes_hex on trace line {line_number} from {}",
                        input_path.display()
                    )
                })?;
                if timing {
                    let delay_ms = elapsed_ms.saturating_sub(previous_elapsed_ms);
                    if delay_ms > MAX_TRACE_REPLAY_DELAY_MS {
                        bail!(
                            "trace replay delay {}ms on line {} exceeds safety cap {}ms; replay without --timing for long idle gaps",
                            delay_ms,
                            line_number,
                            MAX_TRACE_REPLAY_DELAY_MS
                        );
                    }
                }
                previous_elapsed_ms = elapsed_ms;
                plan.total_bytes = plan.total_bytes.saturating_add(expected_len);
                replayed_bytes = replayed_bytes.saturating_add(expected_len);
                expected_chunk_index = expected_chunk_index.saturating_add(1);
                plan.chunks.push(TraceReplayChunk {
                    line_number,
                    elapsed_ms,
                    bytes,
                });
            }
            "end" => {
                if !start_seen {
                    bail!(
                        "trace line {} from {} contains end before start",
                        line_number,
                        input_path.display()
                    );
                }
                if end_seen {
                    bail!(
                        "trace line {} from {} contains a duplicate end event",
                        line_number,
                        input_path.display()
                    );
                }
                if let Some(chunks_recorded) = optional_trace_u64(&event, "chunks_recorded") {
                    if chunks_recorded != expected_chunk_index {
                        bail!(
                            "trace line {} from {} records {} chunks but replay saw {}",
                            line_number,
                            input_path.display(),
                            chunks_recorded,
                            expected_chunk_index
                        );
                    }
                } else if event.get("chunks_recorded").is_some() {
                    bail!(
                        "trace line {} from {} has non-u64 field chunks_recorded",
                        line_number,
                        input_path.display()
                    );
                }
                if let Some(bytes_recorded) = optional_trace_u64(&event, "bytes_recorded") {
                    if bytes_recorded > MAX_TRACE_REPLAY_TOTAL_BYTES {
                        bail!(
                            "trace replay total bytes exceed safety cap {}",
                            MAX_TRACE_REPLAY_TOTAL_BYTES
                        );
                    }
                    if bytes_recorded != replayed_bytes {
                        bail!(
                            "trace line {} from {} records {} bytes but replay saw {}",
                            line_number,
                            input_path.display(),
                            bytes_recorded,
                            replayed_bytes
                        );
                    }
                } else if event.get("bytes_recorded").is_some() {
                    bail!(
                        "trace line {} from {} has non-u64 field bytes_recorded",
                        line_number,
                        input_path.display()
                    );
                }
                end_seen = true;
            }
            event_type => {
                bail!(
                    "trace line {} from {} has unsupported event type {:?}",
                    line_number,
                    input_path.display(),
                    event_type
                );
            }
        }
    }
    if !start_seen {
        bail!(
            "trace file {} is missing a start event",
            input_path.display()
        );
    }
    if !end_seen {
        bail!(
            "trace file {} is missing an end event",
            input_path.display()
        );
    }
    Ok(plan)
}

pub fn print_trace_info(input_path: &Path, json_output: bool) -> Result<()> {
    let summary = trace_file_summary(input_path)?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else {
        print_trace_summary_text(&summary);
    }
    Ok(())
}

fn trace_file_summary(input_path: &Path) -> Result<TraceFileSummary> {
    let file = std::fs::File::open(input_path)
        .with_context(|| format!("open trace file {}", input_path.display()))?;
    let mut reader = BufReader::new(file);
    let mut summary = TraceFileSummary {
        path: input_path.display().to_string(),
        ..TraceFileSummary::default()
    };
    let mut line_number = 0_usize;
    let mut start_seen = false;
    let mut end_seen = false;

    while let Some(line) = read_trace_jsonl_line(&mut reader, &mut line_number, input_path)? {
        if line.trim().is_empty() {
            continue;
        }
        summary.event_count = summary.event_count.saturating_add(1);
        let event: serde_json::Value = match serde_json::from_str(&line) {
            Ok(event) => event,
            Err(_) => {
                summary.unknown_events = summary.unknown_events.saturating_add(1);
                continue;
            }
        };
        match event.get("type").and_then(|value| value.as_str()) {
            Some("start") => {
                if start_seen {
                    summary.unknown_events = summary.unknown_events.saturating_add(1);
                    continue;
                }
                start_seen = true;
                summary.schema_version = optional_trace_string(&event, "schema_version");
                summary.format = optional_trace_string(&event, "format");
                summary.trace_id = optional_trace_string(&event, "trace_id");
                summary.producer = optional_trace_string(&event, "producer");
                summary.client_version = optional_trace_string(&event, "client_version");
                summary.client_protocol_version =
                    optional_trace_u64(&event, "client_protocol_version");
                summary.target = optional_trace_string(&event, "target");
                summary.created_at_unix_ms = optional_trace_u64(&event, "created_at_unix_ms");
                summary.duration_ms = optional_trace_u64(&event, "duration_ms");
                summary.max_bytes = optional_trace_u64(&event, "max_bytes");
                summary.rows = optional_trace_u64(&event, "rows");
                summary.cols = optional_trace_u64(&event, "cols");
                summary.raw_stream_policy = optional_trace_string(&event, "raw_stream_policy");
            }
            Some("output") => {
                let Ok(len) = trace_output_event_len(&event, input_path, line_number) else {
                    summary.unknown_events = summary.unknown_events.saturating_add(1);
                    continue;
                };
                summary.output_chunks = summary.output_chunks.saturating_add(1);
                summary.output_bytes = summary.output_bytes.saturating_add(len);
                if let Some(elapsed_ms) = optional_trace_u64(&event, "elapsed_ms") {
                    summary.first_output_elapsed_ms =
                        summary.first_output_elapsed_ms.or(Some(elapsed_ms));
                    summary.last_output_elapsed_ms = Some(elapsed_ms);
                }
            }
            Some("end") => {
                if end_seen {
                    summary.unknown_events = summary.unknown_events.saturating_add(1);
                    continue;
                }
                end_seen = true;
                summary.end_elapsed_ms = optional_trace_u64(&event, "elapsed_ms");
                summary.end_reason = optional_trace_string(&event, "reason");
                summary.end_bytes_recorded = optional_trace_u64(&event, "bytes_recorded");
                summary.end_chunks_recorded = optional_trace_u64(&event, "chunks_recorded");
            }
            Some(_) | None => {
                summary.unknown_events = summary.unknown_events.saturating_add(1);
            }
        }
    }
    Ok(summary)
}

fn read_trace_jsonl_line(
    reader: &mut impl BufRead,
    line_number: &mut usize,
    input_path: &Path,
) -> Result<Option<String>> {
    let mut line = Vec::new();
    loop {
        let (take, done) = {
            let available = reader.fill_buf().with_context(|| {
                format!(
                    "read trace line {} from {}",
                    *line_number + 1,
                    input_path.display()
                )
            })?;
            if available.is_empty() {
                if line.is_empty() {
                    return Ok(None);
                }
                (0, true)
            } else if let Some(pos) = available.iter().position(|byte| *byte == b'\n') {
                let take = pos + 1;
                if line.len().saturating_add(take) > MAX_TRACE_JSONL_LINE_BYTES {
                    bail!(
                        "trace line {} from {} exceeds maximum JSONL line length of {} bytes",
                        *line_number + 1,
                        input_path.display(),
                        MAX_TRACE_JSONL_LINE_BYTES
                    );
                }
                line.extend_from_slice(&available[..take]);
                (take, true)
            } else {
                let take = available.len();
                if line.len().saturating_add(take) > MAX_TRACE_JSONL_LINE_BYTES {
                    bail!(
                        "trace line {} from {} exceeds maximum JSONL line length of {} bytes",
                        *line_number + 1,
                        input_path.display(),
                        MAX_TRACE_JSONL_LINE_BYTES
                    );
                }
                line.extend_from_slice(available);
                (take, false)
            }
        };
        if take > 0 {
            reader.consume(take);
        }
        if done {
            break;
        }
    }
    *line_number += 1;
    if line.last() == Some(&b'\n') {
        line.pop();
    }
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    String::from_utf8(line)
        .with_context(|| {
            format!(
                "trace line {} from {} is not valid UTF-8 JSONL",
                *line_number,
                input_path.display()
            )
        })
        .map(Some)
}

fn validate_trace_start_event(
    event: &serde_json::Value,
    input_path: &Path,
    line_number: usize,
) -> Result<()> {
    let schema_version = required_trace_str(event, "schema_version", input_path, line_number)?;
    if schema_version != TRACE_SCHEMA_VERSION {
        bail!(
            "trace line {} from {} has unsupported trace schema_version {:?}",
            line_number,
            input_path.display(),
            schema_version
        );
    }
    if let Some(format) = optional_trace_string(event, "format") {
        if format != TRACE_FORMAT {
            bail!(
                "trace line {} from {} has unsupported trace format {:?}",
                line_number,
                input_path.display(),
                format
            );
        }
    } else if event.get("format").is_some() {
        bail!(
            "trace line {} from {} has non-string field format",
            line_number,
            input_path.display()
        );
    }
    Ok(())
}

fn print_trace_summary_text(summary: &TraceFileSummary) {
    print_trace_summary_string("path", Some(&summary.path));
    print_trace_summary_string("format", summary.format.as_deref());
    print_trace_summary_string("schema_version", summary.schema_version.as_deref());
    print_trace_summary_string("trace_id", summary.trace_id.as_deref());
    print_trace_summary_string("producer", summary.producer.as_deref());
    print_trace_summary_string("client_version", summary.client_version.as_deref());
    print_trace_summary_u64("client_protocol_version", summary.client_protocol_version);
    print_trace_summary_string("target", summary.target.as_deref());
    print_trace_summary_u64("created_at_unix_ms", summary.created_at_unix_ms);
    print_trace_summary_u64("duration_ms", summary.duration_ms);
    print_trace_summary_u64("max_bytes", summary.max_bytes);
    print_trace_summary_u64("rows", summary.rows);
    print_trace_summary_u64("cols", summary.cols);
    print_trace_summary_string("raw_stream_policy", summary.raw_stream_policy.as_deref());
    println!("event_count\t{}", summary.event_count);
    println!("output_chunks\t{}", summary.output_chunks);
    println!("output_bytes\t{}", summary.output_bytes);
    print_trace_summary_u64("first_output_elapsed_ms", summary.first_output_elapsed_ms);
    print_trace_summary_u64("last_output_elapsed_ms", summary.last_output_elapsed_ms);
    print_trace_summary_u64("end_elapsed_ms", summary.end_elapsed_ms);
    print_trace_summary_string("end_reason", summary.end_reason.as_deref());
    print_trace_summary_u64("end_bytes_recorded", summary.end_bytes_recorded);
    print_trace_summary_u64("end_chunks_recorded", summary.end_chunks_recorded);
    println!("unknown_events\t{}", summary.unknown_events);
}

fn print_trace_summary_string(key: &str, value: Option<&str>) {
    println!(
        "{}\t{}",
        key,
        sanitize::terminal_text(value.unwrap_or("unknown"))
    );
}

fn print_trace_summary_u64(key: &str, value: Option<u64>) {
    match value {
        Some(value) => println!("{key}\t{value}"),
        None => println!("{key}\tunknown"),
    }
}

fn required_trace_str<'a>(
    event: &'a serde_json::Value,
    field: &str,
    input_path: &Path,
    line_number: usize,
) -> Result<&'a str> {
    event
        .get(field)
        .and_then(|value| value.as_str())
        .with_context(|| {
            format!(
                "trace line {line_number} from {} is missing string field {field}",
                input_path.display()
            )
        })
}

fn optional_trace_string(event: &serde_json::Value, field: &str) -> Option<String> {
    event
        .get(field)
        .and_then(|value| value.as_str())
        .map(str::to_string)
}

fn optional_trace_u64(event: &serde_json::Value, field: &str) -> Option<u64> {
    event.get(field).and_then(|value| value.as_u64())
}

fn required_trace_u64(
    event: &serde_json::Value,
    field: &str,
    input_path: &Path,
    line_number: usize,
) -> Result<u64> {
    match event.get(field) {
        Some(value) => value.as_u64().with_context(|| {
            format!(
                "trace line {line_number} from {} has non-u64 field {field}",
                input_path.display()
            )
        }),
        None => bail!(
            "trace line {} from {} is missing u64 field {}",
            line_number,
            input_path.display(),
            field
        ),
    }
}

fn trace_output_event_len(
    event: &serde_json::Value,
    input_path: &Path,
    line_number: usize,
) -> Result<u64> {
    let bytes_hex = required_trace_str(event, "bytes_hex", input_path, line_number)?;
    let encoded_len = hex_encoded_len(bytes_hex).with_context(|| {
        format!(
            "validate bytes_hex on trace line {line_number} from {}",
            input_path.display()
        )
    })?;
    match event.get("len") {
        Some(value) => {
            let len = value.as_u64().with_context(|| {
                format!(
                    "trace line {line_number} from {} has non-u64 field len",
                    input_path.display()
                )
            })?;
            if len != encoded_len {
                bail!(
                    "trace line {} from {} has len {} but bytes_hex encodes {} bytes",
                    line_number,
                    input_path.display(),
                    len,
                    encoded_len
                );
            }
            Ok(len)
        }
        None => Ok(encoded_len),
    }
}

fn trace_output_open_context(output_path: &Path, force: bool) -> String {
    if force {
        format!(
            "create or truncate private trace output {}",
            output_path.display()
        )
    } else {
        format!(
            "create private trace output {} (pass --force to overwrite an existing file)",
            output_path.display()
        )
    }
}

fn ensure_trace_force_target_private(output_path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(output_path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                bail!(
                    "refusing to overwrite symlink trace output {}",
                    output_path.display()
                );
            }
            if !metadata.file_type().is_file() {
                bail!(
                    "refusing to overwrite non-file trace output {}",
                    output_path.display()
                );
            }
            let mode = metadata.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                bail!(
                    "refusing to overwrite trace output {} with permissions {:03o}; chmod 600 or remove it first",
                    output_path.display(),
                    mode
                );
            }
        }
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => {
            return Err(err)
                .with_context(|| format!("inspect trace output {}", output_path.display()));
        }
    }
    Ok(())
}

fn write_trace_event(output: &mut impl Write, event: &TraceEvent) -> Result<()> {
    serde_json::to_writer(&mut *output, event).context("serialize trace event")?;
    output.write_all(b"\n").context("write trace event")
}

fn current_unix_ms() -> Option<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn hex_encoded_len(value: &str) -> Result<u64> {
    if value.len() % 2 != 0 {
        bail!("hex string has odd length");
    }
    if !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("hex string contains a non-hex digit");
    }
    Ok(u64::try_from(value.len() / 2).unwrap_or(u64::MAX))
}

fn hex_decode(value: &str) -> Result<Vec<u8>> {
    if value.len() % 2 != 0 {
        bail!("hex string has odd length");
    }
    let decoded_len = u64::try_from(value.len() / 2).unwrap_or(u64::MAX);
    if decoded_len > MAX_TRACE_REPLAY_CHUNK_BYTES {
        bail!(
            "hex string decodes to {} bytes, exceeding replay chunk limit {}",
            decoded_len,
            MAX_TRACE_REPLAY_CHUNK_BYTES
        );
    }
    let mut bytes = Vec::with_capacity(value.len() / 2);
    for pair in value.as_bytes().chunks_exact(2) {
        let high =
            hex_nibble(pair[0]).ok_or_else(|| anyhow!("hex string contains a non-hex digit"))?;
        let low =
            hex_nibble(pair[1]).ok_or_else(|| anyhow!("hex string contains a non-hex digit"))?;
        bytes.push((high << 4) | low);
    }
    Ok(bytes)
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[derive(Debug, Clone)]
pub struct ComposeOptions {
    pub tail: usize,
    pub refresh: Duration,
    pub once: bool,
    pub message: Option<String>,
    pub append_enter: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachMode {
    Auto,
    Raw,
    Mobile,
}

impl AttachMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "raw" => Some(Self::Raw),
            "mobile" | "transcript" => Some(Self::Mobile),
            _ => None,
        }
    }

    pub fn allowed_values() -> &'static str {
        "auto, raw, mobile"
    }
}

#[derive(Debug, Clone)]
pub struct MobileTranscriptOptions {
    pub tail: usize,
    pub refresh: Duration,
    pub read_only: bool,
    pub append_enter: bool,
    pub banner: bool,
}

impl Default for MobileTranscriptOptions {
    fn default() -> Self {
        Self {
            tail: 120,
            refresh: Duration::from_millis(500),
            read_only: false,
            append_enter: true,
            banner: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AttachPolicyOptions {
    pub mode: AttachMode,
    pub transcript: MobileTranscriptOptions,
}

pub fn compose(target: &str, options: ComposeOptions) -> Result<()> {
    let tail_start = compose_tail_start(options.tail)?;
    if options.once {
        let message = options
            .message
            .as_deref()
            .context("--message is required with --once")?;
        let output = capture_range(target, Some(tail_start), None)?;
        print!("{output}");
        std::io::stdout().flush().context("flush compose output")?;
        send(target, compose_commit_bytes(message, options.append_enter))?;
        return Ok(());
    }
    if options.message.is_some() {
        bail!("--message requires --once");
    }
    let refresh = compose_refresh_interval(options.refresh)?;
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("interactive compose requires a terminal; use --once --message for automation");
    }
    run_interactive_compose(target, tail_start, refresh, options.append_enter)
}

pub fn resolve_attach_mode(explicit: Option<AttachMode>) -> Result<AttachMode> {
    if let Some(mode) = explicit {
        return Ok(mode);
    }
    match std::env::var("LTERM_ATTACH_MODE") {
        Ok(value) if !value.trim().is_empty() => AttachMode::parse(&value).ok_or_else(|| {
            anyhow!(
                "invalid LTERM_ATTACH_MODE {:?}; expected {}",
                value,
                AttachMode::allowed_values()
            )
        }),
        _ => Ok(AttachMode::Auto),
    }
}

pub fn should_mobile_transcript_auto(info: &SessionInfo) -> bool {
    mobile_client_detected() && likely_agent_session(info)
}

pub fn mobile_client_detected() -> bool {
    env_flag_enabled("LTERM_MOBILE") || is_termius_session()
}

pub fn likely_agent_session(info: &SessionInfo) -> bool {
    if info
        .agent_name
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty())
    {
        return true;
    }
    known_agent_name_from_session(&info.name) || known_agent_name_from_command(&info.command)
}

fn known_agent_name_from_session(name: &str) -> bool {
    let Some(base) = name.strip_suffix("-lterm") else {
        return false;
    };
    known_agent_name(base)
}

fn known_agent_name_from_command(command: &str) -> bool {
    let first = shlex::split(command)
        .and_then(|parts| parts.into_iter().next())
        .unwrap_or_else(|| command.split_whitespace().next().unwrap_or("").to_string());
    let basename = first.rsplit('/').next().unwrap_or(&first);
    known_agent_name(basename)
}

fn known_agent_name(name: &str) -> bool {
    matches!(
        name,
        "claude"
            | "codex"
            | "opencode"
            | "copilot"
            | "cursor-agent"
            | "agy"
            | "jules"
            | "kiro"
            | "kiro-cli"
            | "aider"
            | "goose"
            | "amp"
            | "crush"
            | "gemini"
            | "kimi"
            | "qwen"
            | "omx"
            | "omc"
    )
}

pub fn attach_with_policy(
    target: &str,
    show_status: bool,
    stdin_eof: AttachStdinEof,
    options: AttachPolicyOptions,
) -> Result<()> {
    let info = info(target)?;
    attach_info_with_policy(&info, show_status, stdin_eof, options)
}

pub fn attach_info_with_policy(
    info: &SessionInfo,
    show_status: bool,
    stdin_eof: AttachStdinEof,
    options: AttachPolicyOptions,
) -> Result<()> {
    let use_mobile = match options.mode {
        AttachMode::Raw => false,
        AttachMode::Mobile => true,
        AttachMode::Auto => should_mobile_transcript_auto(info),
    };
    if use_mobile {
        mobile_transcript(&info.pane_id, options.transcript)
    } else if options.transcript.read_only {
        bail!("--read-only requires mobile transcript mode");
    } else {
        attach(&info.pane_id, show_status, stdin_eof)
    }
}

const COMPOSE_MIN_REFRESH: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ComposeRenderAction {
    RemoteCapture,
    PromptOnly,
    None,
}

fn compose_render_action(
    remote_dirty: bool,
    prompt_dirty: bool,
    elapsed: Duration,
    refresh: Duration,
) -> ComposeRenderAction {
    if remote_dirty || elapsed >= refresh {
        ComposeRenderAction::RemoteCapture
    } else if prompt_dirty {
        ComposeRenderAction::PromptOnly
    } else {
        ComposeRenderAction::None
    }
}

fn run_interactive_compose(
    target: &str,
    tail_start: i32,
    refresh: Duration,
    append_enter: bool,
) -> Result<()> {
    let mut stdout = std::io::stdout();
    let _guard = ComposeTerminalGuard::enter(&mut stdout)?;
    let mut input = String::new();
    let mut last_refresh = Instant::now();
    let mut remote_dirty = true;
    let mut prompt_dirty = true;

    loop {
        match compose_render_action(remote_dirty, prompt_dirty, last_refresh.elapsed(), refresh) {
            ComposeRenderAction::RemoteCapture => {
                let capture = capture_range(target, Some(tail_start), None)?;
                render_compose_snapshot(&capture, &input, &mut stdout)?;
                last_refresh = Instant::now();
                remote_dirty = false;
                prompt_dirty = false;
            }
            ComposeRenderAction::PromptOnly => {
                render_compose_prompt(&input, &mut stdout)?;
                prompt_dirty = false;
            }
            ComposeRenderAction::None => {}
        }
        let poll_timeout = refresh
            .checked_sub(last_refresh.elapsed())
            .unwrap_or_else(|| Duration::from_millis(50))
            .min(Duration::from_millis(100));
        if !event::poll(poll_timeout).context("poll compose input")? {
            continue;
        }
        let mut pending_event = Some(event::read().context("read compose input")?);
        let mut exit = false;
        while let Some(event) = pending_event.take() {
            let key = match event {
                Event::Key(key) => key,
                Event::Resize(_, _) => {
                    remote_dirty = true;
                    prompt_dirty = true;
                    if event::poll(Duration::ZERO).context("poll queued compose input")? {
                        pending_event = Some(event::read().context("read queued compose input")?);
                    }
                    continue;
                }
                Event::Paste(text) => {
                    compose_push_paste(&mut input, &text);
                    prompt_dirty = true;
                    if event::poll(Duration::ZERO).context("poll queued compose input")? {
                        pending_event = Some(event::read().context("read queued compose input")?);
                    }
                    continue;
                }
                _ => {
                    if event::poll(Duration::ZERO).context("poll queued compose input")? {
                        pending_event = Some(event::read().context("read queued compose input")?);
                    }
                    continue;
                }
            };
            if key.kind != KeyEventKind::Press {
                if event::poll(Duration::ZERO).context("poll queued compose input")? {
                    pending_event = Some(event::read().context("read queued compose input")?);
                }
                continue;
            }
            if compose_is_local_exit_key(&key) {
                exit = true;
                break;
            }
            match key.code {
                KeyCode::Enter => {
                    if compose_should_commit(&input, append_enter) {
                        send(target, compose_commit_bytes(&input, append_enter))?;
                        input.clear();
                    }
                    prompt_dirty = true;
                }
                KeyCode::Backspace => {
                    compose_pop_grapheme(&mut input);
                    prompt_dirty = true;
                }
                KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    input.push(ch);
                    prompt_dirty = true;
                }
                _ => {}
            }
            if event::poll(Duration::ZERO).context("poll queued compose input")? {
                pending_event = Some(event::read().context("read queued compose input")?);
            }
        }
        if exit {
            break;
        }
    }
    Ok(())
}

fn render_compose_snapshot(capture: &str, input: &str, stdout: &mut impl Write) -> Result<()> {
    let (cols, rows) = terminal_size();
    let width = cols.saturating_sub(1).max(1) as usize;
    let body_rows = rows.saturating_sub(1) as usize;
    let lines: Vec<&str> = capture.lines().collect();
    let start = lines.len().saturating_sub(body_rows);

    let visible_lines = &lines[start..];
    for row_idx in 0..body_rows {
        let row = u16::try_from(row_idx).unwrap_or(u16::MAX);
        queue!(
            stdout,
            cursor::MoveTo(0, row),
            terminal::Clear(ClearType::CurrentLine)
        )
        .context("position compose body")?;
        if let Some(line) = visible_lines.get(row_idx) {
            write!(stdout, "{}", compose_sanitized_display_line(line, width))
                .context("write compose body")?;
        }
    }
    render_compose_prompt_at(input, width, rows.saturating_sub(1), stdout)?;
    stdout.flush().context("flush compose screen")?;
    Ok(())
}

fn render_compose_prompt(input: &str, stdout: &mut impl Write) -> Result<()> {
    let (cols, rows) = terminal_size();
    let width = cols.saturating_sub(1).max(1) as usize;
    render_compose_prompt_at(input, width, rows.saturating_sub(1), stdout)?;
    stdout.flush().context("flush compose prompt")?;
    Ok(())
}

fn render_compose_prompt_at(
    input: &str,
    width: usize,
    prompt_row: u16,
    stdout: &mut impl Write,
) -> Result<()> {
    let (prompt, cursor_col) = compose_prompt_line(input, width);
    queue!(
        stdout,
        cursor::MoveTo(0, prompt_row),
        terminal::Clear(ClearType::CurrentLine)
    )
    .context("position compose prompt")?;
    write!(stdout, "{}", compose_display_line(&prompt, width)).context("write compose prompt")?;
    queue!(stdout, cursor::MoveTo(cursor_col, prompt_row), cursor::Show)
        .context("position compose cursor")?;
    Ok(())
}

pub fn mobile_transcript(target: &str, options: MobileTranscriptOptions) -> Result<()> {
    ensure_server()?;
    let tail_start = compose_tail_start(options.tail)?;
    let refresh = compose_refresh_interval(options.refresh)?;
    let mut stdout = std::io::stdout();
    let mut last_capture = String::new();

    if options.banner {
        let info = info(target)?;
        writeln!(
            stdout,
            "lterm mobile transcript · target={} · pane={} · raw attach: lterm attach --raw {}",
            sanitize::terminal_text(&info.name),
            sanitize::terminal_text(&info.pane_id),
            sanitize::terminal_text(&info.name)
        )
        .context("write mobile transcript banner")?;
    }
    wait_and_render_initial_mobile_transcript(target, tail_start, refresh, &mut last_capture)?;

    if options.read_only {
        follow_mobile_transcript_read_only(target, tail_start, refresh, &mut last_capture)?;
        return Ok(());
    }
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Ok(());
    }
    run_interactive_mobile_transcript(target, tail_start, refresh, options, last_capture)
}

fn wait_and_render_initial_mobile_transcript(
    target: &str,
    tail_start: i32,
    refresh: Duration,
    last_capture: &mut String,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let capture = capture_range(target, Some(tail_start), None)?;
        let info = info(target)?;
        let wrote = write_mobile_transcript_update(last_capture, &capture, &mut std::io::stdout())?;
        if wrote || !info.alive || Instant::now() >= deadline {
            return Ok(());
        }
        thread::sleep(refresh.min(Duration::from_millis(100)));
    }
}

fn follow_mobile_transcript_read_only(
    target: &str,
    tail_start: i32,
    refresh: Duration,
    last_capture: &mut String,
) -> Result<()> {
    loop {
        thread::sleep(refresh);
        let capture = capture_range(target, Some(tail_start), None)?;
        write_mobile_transcript_update(last_capture, &capture, &mut std::io::stdout())?;
        if !info(target)?.alive {
            return Ok(());
        }
    }
}

fn run_interactive_mobile_transcript(
    target: &str,
    tail_start: i32,
    refresh: Duration,
    options: MobileTranscriptOptions,
    mut last_capture: String,
) -> Result<()> {
    let (input_tx, input_rx) = mpsc::channel();
    thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut stdin = BufReader::new(stdin);
        loop {
            let mut input = String::new();
            match stdin.read_line(&mut input) {
                Ok(0) => {
                    let _ = input_tx.send(Ok(None));
                    return;
                }
                Ok(_) => {
                    if input_tx.send(Ok(Some(input))).is_err() {
                        return;
                    }
                }
                Err(err) => {
                    let _ = input_tx.send(Err(err.to_string()));
                    return;
                }
            }
        }
    });
    let mut stdout = std::io::stdout();
    write!(stdout, "> ").context("write mobile prompt")?;
    stdout.flush().context("flush mobile prompt")?;
    loop {
        match input_rx.recv_timeout(refresh) {
            Ok(Ok(Some(input))) => {
                let input = trim_line_endings(&input);
                match input {
                    "/exit" | "/quit" => return Ok(()),
                    "/refresh" => {
                        let capture = capture_range(target, Some(tail_start), None)?;
                        last_capture.clear();
                        write_mobile_transcript_update(&mut last_capture, &capture, &mut stdout)?;
                    }
                    "/raw" => {
                        writeln!(
                            stdout,
                            "raw attach: lterm attach --raw {}",
                            sanitize::terminal_text(target)
                        )
                        .context("write raw attach hint")?;
                    }
                    _ => {
                        send(target, compose_commit_bytes(input, options.append_enter))?;
                        let capture = capture_range(target, Some(tail_start), None)?;
                        write_mobile_transcript_update(&mut last_capture, &capture, &mut stdout)?;
                    }
                }
                write!(stdout, "\n> ").context("redraw mobile prompt")?;
                stdout.flush().context("flush mobile prompt")?;
            }
            Ok(Ok(None)) => return Ok(()),
            Ok(Err(err)) => bail!("read mobile transcript input: {err}"),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let capture = capture_range(target, Some(tail_start), None)?;
                if mobile_transcript_capture_changed(&last_capture, &capture) {
                    writeln!(stdout).context("separate mobile prompt from transcript update")?;
                    write_mobile_transcript_update(&mut last_capture, &capture, &mut stdout)?;
                    write!(stdout, "\n> ").context("redraw mobile prompt")?;
                    stdout.flush().context("flush mobile prompt")?;
                }
                if !info(target)?.alive {
                    return Ok(());
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        }
    }
}

fn mobile_transcript_capture_changed(previous: &str, next: &str) -> bool {
    sanitize::terminal_capture(next.as_bytes()) != previous
}

fn trim_line_endings(value: &str) -> &str {
    value.trim_end_matches(['\r', '\n'])
}

fn write_mobile_transcript_update(
    previous: &mut String,
    next: &str,
    stdout: &mut impl Write,
) -> Result<bool> {
    let next = sanitize::terminal_capture(next.as_bytes());
    if next == *previous {
        return Ok(false);
    }
    if previous.is_empty() {
        write!(stdout, "{next}").context("write mobile transcript")?;
    } else if let Some(suffix) = mobile_transcript_incremental_suffix(previous, &next) {
        write!(stdout, "{suffix}").context("write mobile transcript suffix")?;
    } else {
        writeln!(stdout, "\n--- lterm transcript refresh ---")
            .context("write mobile transcript refresh separator")?;
        write!(stdout, "{next}").context("write mobile transcript refresh")?;
    }
    stdout.flush().context("flush mobile transcript")?;
    previous.clear();
    previous.push_str(&next);
    Ok(true)
}

fn mobile_transcript_incremental_suffix<'a>(previous: &str, next: &'a str) -> Option<&'a str> {
    if let Some(suffix) = next.strip_prefix(previous) {
        return Some(suffix);
    }

    let starts = previous
        .char_indices()
        .filter_map(|(index, _)| (index > 0 && previous[..index].ends_with('\n')).then_some(index))
        .collect::<Vec<_>>();
    for start in starts {
        let overlap = &previous[start..];
        if overlap.is_empty() || overlap.len() > next.len() || !next.is_char_boundary(overlap.len())
        {
            continue;
        }
        if let Some(suffix) = next.strip_prefix(overlap) {
            return Some(suffix);
        }
    }
    None
}

fn compose_refresh_interval(refresh: Duration) -> Result<Duration> {
    if refresh < COMPOSE_MIN_REFRESH {
        bail!(
            "--refresh must be at least {}ms",
            COMPOSE_MIN_REFRESH.as_millis()
        );
    }
    Ok(refresh)
}

fn compose_tail_start(tail: usize) -> Result<i32> {
    if tail == 0 {
        bail!("--tail must be greater than zero");
    }
    let tail = i32::try_from(tail).context("--tail exceeds supported scrollback range")?;
    Ok(-tail)
}

fn compose_commit_bytes(message: &str, append_enter: bool) -> Vec<u8> {
    let mut bytes = message.as_bytes().to_vec();
    if append_enter {
        bytes.push(b'\r');
    }
    bytes
}

fn compose_should_commit(input: &str, append_enter: bool) -> bool {
    append_enter || !input.is_empty()
}

fn compose_is_local_exit_key(key: &KeyEvent) -> bool {
    match &key.code {
        KeyCode::Esc => true,
        KeyCode::Char('c' | 'C' | 'd' | 'D') => key.modifiers.contains(KeyModifiers::CONTROL),
        _ => false,
    }
}

fn compose_pop_grapheme(input: &mut String) {
    use unicode_segmentation::UnicodeSegmentation;

    if let Some((index, _)) = input.grapheme_indices(true).next_back() {
        input.truncate(index);
    }
}

fn compose_push_paste(input: &mut String, text: &str) {
    input.push_str(text);
}

fn compose_sanitized_display_line(value: &str, width: usize) -> String {
    compose_display_line(&sanitize::terminal_text(value), width)
}

fn compose_display_line(value: &str, width: usize) -> String {
    compose_truncate_start(value, width)
}

fn compose_prompt_line(input: &str, width: usize) -> (String, u16) {
    use unicode_width::UnicodeWidthStr;

    let prompt = format!("> {}", sanitize::terminal_text(input));
    if prompt.width() <= width {
        let cursor_col = u16::try_from(prompt.width()).unwrap_or(u16::MAX);
        return (prompt, cursor_col);
    }
    let prompt = compose_truncate_end(&prompt, width);
    let cursor_col = u16::try_from(prompt.width()).unwrap_or(u16::MAX);
    (prompt, cursor_col)
}

fn compose_truncate_start(value: &str, width: usize) -> String {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;

    let mut used = 0_usize;
    let mut output = String::new();
    for cluster in value.graphemes(true) {
        let cluster_width = cluster.width();
        if used + cluster_width > width {
            break;
        }
        output.push_str(cluster);
        used += cluster_width;
    }
    output
}

fn compose_truncate_end(value: &str, width: usize) -> String {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;

    let mut used = 0_usize;
    let mut clusters = Vec::new();
    for cluster in value.graphemes(true).rev() {
        let cluster_width = cluster.width();
        if used + cluster_width > width {
            break;
        }
        clusters.push(cluster);
        used += cluster_width;
    }
    clusters.reverse();
    clusters.concat()
}

struct ComposeTerminalGuard {
    _panic_cleanup: AttachActiveGuard,
}

impl ComposeTerminalGuard {
    fn enter(stdout: &mut impl Write) -> Result<Self> {
        ensure_panic_terminal_cleanup_hook();
        terminal::enable_raw_mode().context("enable compose raw mode")?;
        let panic_cleanup = AttachActiveGuard::enter();
        if let Err(err) = compose_terminal_enter_sequence(stdout) {
            let _ = compose_terminal_leave_sequence(stdout);
            drop(panic_cleanup);
            let _ = terminal::disable_raw_mode();
            return Err(err).context("enter compose screen");
        }
        Ok(Self {
            _panic_cleanup: panic_cleanup,
        })
    }
}

impl Drop for ComposeTerminalGuard {
    fn drop(&mut self) {
        let mut stdout = std::io::stdout();
        let _ = compose_terminal_leave_sequence(&mut stdout);
        let _ = terminal::disable_raw_mode();
    }
}

fn compose_terminal_enter_sequence(stdout: &mut impl Write) -> std::io::Result<()> {
    execute!(
        stdout,
        terminal::EnterAlternateScreen,
        EnableBracketedPaste,
        cursor::Hide,
        terminal::Clear(ClearType::All)
    )
}

fn compose_terminal_leave_sequence(stdout: &mut impl Write) -> std::io::Result<()> {
    execute!(
        stdout,
        DisableBracketedPaste,
        terminal::Clear(ClearType::All),
        cursor::Show,
        terminal::LeaveAlternateScreen
    )
}

/// PTY resize 요청. `subscriber_id` 가 `Some(id)` 면 server 가 해당 attach client 의
/// per-client geometry 를 갱신한 뒤 모든 attach 의 min 으로 PTY 사이즈를 재계산
/// 한다 (PR #15 clamp-to-smallest). `None` 이면 legacy 경로 — `lterm resize` CLI
/// 와 tmux-compat shim 처럼 attach 가 아닌 컨트롤 채널에서 직접 PTY 사이즈를
/// 강제하는 케이스에서 사용한다.
pub fn resize(target: &str, rows: u16, cols: u16, subscriber_id: Option<u64>) -> Result<()> {
    ensure_server()?;
    rpc::<serde_json::Value>(&Request::Resize {
        target: target.to_string(),
        rows,
        cols,
        subscriber_id,
    })?;
    Ok(())
}

pub fn shutdown() -> Result<()> {
    ensure_server()?;
    rpc::<serde_json::Value>(&Request::Shutdown)?;
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct ProcessInfo {
    pub session: String,
    pub pane_id: String,
    pub depth: usize,
    pub pid: u32,
    pub ppid: u32,
    pub process_group_id: Option<i32>,
    pub orphan: bool,
    pub stat: String,
    pub cpu_percent: f32,
    pub mem_percent: f32,
    pub rss_kib: u64,
    pub elapsed: String,
    pub command: String,
}

pub fn process_tree(target: Option<&str>, include_orphans: bool) -> Result<Vec<ProcessInfo>> {
    let sessions = if let Some(target) = target {
        vec![info(target)?]
    } else {
        list_sessions()?
    };
    let processes = read_process_table()?;
    let mut by_parent: std::collections::HashMap<u32, Vec<ProcessRow>> =
        std::collections::HashMap::new();
    let mut by_pid = std::collections::HashMap::new();
    for process in processes {
        by_pid.insert(process.pid, process.clone());
        by_parent.entry(process.ppid).or_default().push(process);
    }
    for children in by_parent.values_mut() {
        children.sort_by_key(|p| p.pid);
    }

    let mut builder = ProcessTreeBuilder::new(&by_parent, &by_pid);
    for session in sessions {
        if let Some(root) = session.process_id {
            builder.append(&session.name, &session.pane_id, root, 0);
        }
        if include_orphans {
            builder.append_orphans(&session, 1);
        }
    }
    Ok(builder.into_processes())
}

struct ProcessTreeBuilder<'a> {
    by_parent: &'a std::collections::HashMap<u32, Vec<ProcessRow>>,
    by_pid: &'a std::collections::HashMap<u32, ProcessRow>,
    seen: std::collections::HashSet<u32>,
    processes: Vec<ProcessInfo>,
}

impl<'a> ProcessTreeBuilder<'a> {
    fn new(
        by_parent: &'a std::collections::HashMap<u32, Vec<ProcessRow>>,
        by_pid: &'a std::collections::HashMap<u32, ProcessRow>,
    ) -> Self {
        Self {
            by_parent,
            by_pid,
            seen: std::collections::HashSet::new(),
            processes: Vec::new(),
        }
    }

    fn append(&mut self, session: &str, pane_id: &str, pid: u32, depth: usize) {
        if !self.seen.insert(pid) {
            return;
        }
        if let Some(row) = self.by_pid.get(&pid) {
            self.processes.push(ProcessInfo {
                session: session.to_string(),
                pane_id: pane_id.to_string(),
                depth,
                pid: row.pid,
                ppid: row.ppid,
                process_group_id: Some(row.pgid),
                orphan: false,
                stat: row.stat.clone(),
                cpu_percent: row.cpu_percent,
                mem_percent: row.mem_percent,
                rss_kib: row.rss_kib,
                elapsed: row.elapsed.clone(),
                command: row.command.clone(),
            });
        }
        if let Some(children) = self.by_parent.get(&pid) {
            for child in children {
                self.append(session, pane_id, child.pid, depth + 1);
            }
        }
    }

    fn append_orphans(&mut self, session: &SessionInfo, depth: usize) {
        let Some(process_group_id) = session.process_group_id else {
            return;
        };
        let mut rows: Vec<_> = self
            .by_pid
            .values()
            .filter(|row| row.pgid == process_group_id && !self.seen.contains(&row.pid))
            .collect();
        rows.sort_by_key(|row| row.pid);
        for row in rows {
            if !self.seen.insert(row.pid) {
                continue;
            }
            self.processes.push(ProcessInfo {
                session: session.name.clone(),
                pane_id: session.pane_id.clone(),
                depth,
                pid: row.pid,
                ppid: row.ppid,
                process_group_id: Some(row.pgid),
                orphan: true,
                stat: row.stat.clone(),
                cpu_percent: row.cpu_percent,
                mem_percent: row.mem_percent,
                rss_kib: row.rss_kib,
                elapsed: row.elapsed.clone(),
                command: row.command.clone(),
            });
        }
    }

    fn into_processes(self) -> Vec<ProcessInfo> {
        self.processes
    }
}

#[derive(Debug, Clone)]
struct ProcessRow {
    pid: u32,
    ppid: u32,
    pgid: i32,
    stat: String,
    cpu_percent: f32,
    mem_percent: f32,
    rss_kib: u64,
    elapsed: String,
    command: String,
}

fn read_process_table() -> Result<Vec<ProcessRow>> {
    let output = Command::new(ps_path()?)
        .args([
            "-axo",
            "pid=,ppid=,pgid=,stat=,%cpu=,%mem=,rss=,etime=,command=",
        ])
        .output()
        .context("run ps")?;
    if !output.status.success() {
        bail!("ps exited with {}", output.status);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut rows = Vec::new();
    for line in text.lines() {
        let fields: Vec<_> = line.split_whitespace().take(8).collect();
        if fields.len() < 8 {
            continue;
        }
        let Some(command_start) = nth_field_start(line, 8) else {
            continue;
        };
        let Some(pid) = parse_nonzero_u32(fields[0]) else {
            continue;
        };
        let Some(ppid) = parse_u32(fields[1]) else {
            continue;
        };
        let Some(pgid) = parse_i32(fields[2]) else {
            continue;
        };
        let Some(cpu_percent) = parse_f32(fields[4]) else {
            continue;
        };
        let Some(mem_percent) = parse_f32(fields[5]) else {
            continue;
        };
        let Some(rss_kib) = parse_u64(fields[6]) else {
            continue;
        };
        rows.push(ProcessRow {
            pid,
            ppid,
            pgid,
            stat: fields[3].to_string(),
            cpu_percent,
            mem_percent,
            rss_kib,
            elapsed: fields[7].to_string(),
            command: line[command_start..].to_string(),
        });
    }
    Ok(rows)
}

fn ps_path() -> Result<&'static str> {
    PS_CANDIDATES
        .iter()
        .copied()
        .find(|path| Path::new(path).is_file())
        .with_context(|| format!("find ps binary in {}", PS_CANDIDATES.join(", ")))
}

fn parse_nonzero_u32(value: &str) -> Option<u32> {
    parse_u32(value).filter(|value| *value != 0)
}

fn parse_u32(value: &str) -> Option<u32> {
    value.trim().parse().ok()
}

fn parse_i32(value: &str) -> Option<i32> {
    value.trim().parse().ok()
}

fn parse_u64(value: &str) -> Option<u64> {
    value.trim().parse().ok()
}

fn parse_f32(value: &str) -> Option<f32> {
    value.trim().parse().ok()
}

fn nth_field_start(line: &str, field_index: usize) -> Option<usize> {
    let mut in_field = false;
    let mut field = 0;
    for (idx, ch) in line.char_indices() {
        if ch.is_whitespace() {
            in_field = false;
            continue;
        }
        if !in_field {
            if field == field_index {
                return Some(idx);
            }
            field += 1;
            in_field = true;
        }
    }
    None
}

#[derive(Debug, Clone, Copy)]
pub enum AttachStdinEof {
    Detach,
    KeepAttached,
}

fn read_attach_response_header(reader: &mut impl BufRead) -> Result<Vec<u8>> {
    let mut header = Vec::new();
    loop {
        let available = reader.fill_buf().context("read attach header")?;
        if available.is_empty() {
            bail!("daemon closed attach before header");
        }
        let newline_pos = available.iter().position(|byte| *byte == b'\n');
        let take = newline_pos.map_or(available.len(), |pos| pos + 1);
        let remaining = ATTACH_RESPONSE_HEADER_LIMIT.saturating_sub(header.len());
        if take > remaining {
            bail!("attach header too large");
        }
        header.extend_from_slice(&available[..take]);
        reader.consume(take);
        if newline_pos.is_some() {
            return Ok(header);
        }
    }
}

pub fn attach(target: &str, show_status: bool, stdin_eof: AttachStdinEof) -> Result<()> {
    ensure_server()?;
    ensure_panic_terminal_cleanup_hook();
    let status_enabled = status_bar_supported(show_status);
    // status bar 는 SessionInfo 의 메타데이터 (이름/명령 등) 가 필요하므로 켜졌을 때만
    // info() 를 호출한다. PR #14 의 client-side first-attach guard 가 사라졌으므로
    // attached_clients 를 미리 읽을 이유는 더 이상 없다 — server 가 자체 clamp-to-
    // smallest 로 사이즈 정책을 결정한다 (PR #15).
    let status_info = if status_enabled {
        Some(info(target)?)
    } else {
        None
    };
    let (cols, rows) = terminal_size();
    let pty_rows = attach_pty_rows(rows, status_enabled);

    let path = paths::socket_path()?;
    let mut stream = UnixStream::connect(&path).with_context(|| daemon_connect_context(&path))?;
    stream
        .set_read_timeout(Some(RPC_TIMEOUT))
        .context("set attach handshake read timeout")?;
    stream
        .set_write_timeout(Some(RPC_TIMEOUT))
        .context("set attach handshake write timeout")?;
    // PR #15: attach 시점의 클라이언트 geometry 를 함께 보낸다. server 는 이 값을
    // 바로 Subscriber 에 박아 clamp-to-smallest 정책의 인풋으로 쓴다.
    let request = Request::Attach {
        target: target.to_string(),
        rows: pty_rows,
        cols,
    };
    stream.write_all(&serde_json::to_vec(&request)?)?;
    stream.write_all(b"\n")?;

    let mut reader = BufReader::with_capacity(8192, stream);
    let header = read_attach_response_header(&mut reader)?;
    let response: Response = serde_json::from_slice(&header).context("parse attach header")?;
    if !response.ok {
        bail!(
            response
                .error
                .unwrap_or_else(|| "attach failed".to_string())
        );
    }
    // PR #15: 응답에 박힌 subscriber id 를 꺼낸다. 이후 resize_thread 가 이 id 를
    // Resize 요청에 실어 보내야 server 가 우리 per-client geometry 를 정확히
    // 갱신할 수 있다. 응답 모양이 깨졌으면 attach 자체를 중단해 stale id 가
    // 흘러들어가는 것을 막는다.
    let subscriber_id = response
        .result
        .as_ref()
        .and_then(|v| v.get("subscriber_id"))
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow::anyhow!("attach response missing subscriber_id"))?;

    reader
        .get_ref()
        .set_write_timeout(None)
        .context("clear attach stream write timeout")?;
    reader
        .get_ref()
        .set_read_timeout(None)
        .context("clear attach stream read timeout")?;

    // RawModeGuard 먼저 → AttachActiveGuard 가 raw mode가 실제 세팅된 이후에만 활성.
    // Drop 역순: status_bar → _attach_active(flag false) → _raw(raw mode 복원).
    // 정상 종료 시 hook이 raw mode 복원 *후* 에 fire 되어 escape sequence를 emit
    // 하는 의미 없는 window를 제거한다.
    let _raw = RawModeGuard::enter()?;
    let _attach_active = AttachActiveGuard::enter();
    let alt_screen_state = Arc::new(AltScreenState::default());
    let mut terminal_output_tracker = TerminalOutputTracker::new(
        _raw.keyboard_protocol_restore_state(),
        Arc::clone(&alt_screen_state),
    );
    let running = Arc::new(AtomicBool::new(true));

    let mut writer = reader
        .get_ref()
        .try_clone()
        .context("clone attach stream writer")?;
    let input_running = Arc::clone(&running);
    let detach_on_stdin_eof = matches!(stdin_eof, AttachStdinEof::Detach);
    let input_thread = thread::spawn(move || -> Result<()> {
        let result = (|| -> Result<()> {
            let mut stdin = std::io::stdin();
            let stdin_fd = stdin.as_raw_fd();
            let mut buf = [0_u8; 8192];
            while input_running.load(Ordering::SeqCst) {
                if !stdin_has_input(stdin_fd, Duration::from_millis(100))? {
                    continue;
                }
                match stdin.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if let Err(err) = writer.write_all(&buf[..n]) {
                            input_running.store(false, Ordering::SeqCst);
                            let _ = writer.shutdown(std::net::Shutdown::Write);
                            return Err(err).context("write pty input");
                        }
                    }
                    Err(err) if err.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(err) if err.kind() == ErrorKind::Interrupted => {}
                    Err(err) => return Err(err).context("read stdin"),
                }
            }
            Ok(())
        })();
        if detach_on_stdin_eof {
            let _ = writer.shutdown(std::net::Shutdown::Write);
        }
        result
    });

    let resize_running = Arc::clone(&running);
    let resize_target = target.to_string();
    let (resize_tx, resize_rx) = mpsc::sync_channel(1);
    let resize_thread = thread::spawn(move || {
        let mut last = terminal_size();
        while resize_running.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(250));
            let current = terminal_size();
            if current == last {
                continue;
            }
            // PR #15: server-side clamp-to-smallest 가 PTY 사이즈의 source of truth.
            // SIGWINCH 가 감지되면 우리 subscriber id 를 실어 Resize 를 보낸다. server
            // 는 이 client 의 per-client geometry 만 갱신한 뒤 모든 attach 의 min 으로
            // PTY 를 재계산하므로, 더 이상 client-side 에서 "다른 attach 가 있는가" 를
            // 판단할 필요가 없다 (PR #14 단기 가드 폐기).
            let resize_result = resize(
                &resize_target,
                attach_pty_rows(current.1, status_enabled),
                current.0,
                Some(subscriber_id),
            );
            match handle_resize_tick(resize_result) {
                ResizeTickOutcome::Advance => {
                    // 우리 화면 cols/rows 가 변했으므로 status row 는 새 폭으로 다시
                    // 그려야 한다. status refresh 는 always 보낸다.
                    let _ = resize_tx.try_send(());
                    last = current;
                }
                ResizeTickOutcome::Retry => {
                    // PR #14 의 "info() 실패 시 last 갱신 보류" 와 같은 패턴 — transient
                    // RPC failure 일 가능성이 높으므로 last 를 advance 하지 않아 다음
                    // tick 에서 재시도되게 한다. status refresh 는 폭 변화가 사용자 화면
                    // 에는 이미 반영됐으므로 그대로 보낸다.
                    let _ = resize_tx.try_send(());
                }
                ResizeTickOutcome::StaleSubscriberId => {
                    // server 가 우리 subscriber id 를 더 이상 모른다고 응답했다 — attach
                    // 가 구조적으로 죽은 상태다. main thread 의 output loop 를 깨워
                    // 빠져나갈 수 있도록 running 플래그를 내리고 자체 종료한다.
                    resize_running.store(false, Ordering::SeqCst);
                    let _ = resize_tx.try_send(());
                    break;
                }
            }
        }
    });

    let mut stdout = std::io::stdout();
    let status_style = status_enabled
        .then(|| resolve_status_style(status_info.as_ref().and_then(|info| info.status_theme)));
    let mut status_bar = StatusBar::enter(status_info.as_ref(), status_style, &mut stdout)?;
    if status_enabled {
        reader
            .get_ref()
            .set_read_timeout(Some(ATTACH_OUTPUT_IDLE_TIMEOUT))
            .context("set attach output read timeout")?;
    }
    let mut buf = [0_u8; 8192];
    let mut status_dirty = false;
    let mut last_status_refresh = Instant::now();
    let mut prev_alt_screen_active = false;
    let output_result = (|| -> Result<()> {
        loop {
            if !running.load(Ordering::SeqCst) {
                break;
            }
            let alt_screen_active = alt_screen_state.active.load(Ordering::Relaxed);

            // alt-screen 종료 즉시 refresh: alt buffer로 흘러갔던 status는 폐기되었으므로
            // 다음 heartbeat까지 빈 상태가 되지 않게 한 번 redraw한다. 이 redraw가 PTY의
            // main-buffer redraw와 시점이 겹치면 미세한 깜빡임이 가능하나, scroll region
            // (rows-1)이 PTY 본문을 status row와 분리하므로 실용적 문제는 없다.
            if status_enabled && prev_alt_screen_active && !alt_screen_active {
                refresh_status(
                    &mut status_bar,
                    &mut stdout,
                    &mut status_dirty,
                    &mut last_status_refresh,
                )?;
            }
            prev_alt_screen_active = alt_screen_active;

            while resize_rx.try_recv().is_ok() {
                // alt-screen 동안 refresh하면 alt buffer로 출력되어 vim 등과 충돌한다.
                // 리사이즈 자체는 daemon-side resize 호출이 이미 처리했으므로, alt-screen
                // 종료 후 edge refresh가 새 크기로 다시 그린다.
                if !alt_screen_active {
                    refresh_status(
                        &mut status_bar,
                        &mut stdout,
                        &mut status_dirty,
                        &mut last_status_refresh,
                    )?;
                }
            }
            // heartbeat는 idle(STATUS_HEARTBEAT) + forced(STATUS_HEARTBEAT_FORCED) 두 경로를
            // 모두 가진다. busy PTY 출력 중에도 forced 경로로 self-heal이 발화한다.
            // 자세한 조건은 `heartbeat_due` 도큐먼트 참조.
            if status_enabled
                && !alt_screen_active
                && heartbeat_due(last_status_refresh.elapsed(), status_dirty)
            {
                refresh_status(
                    &mut status_bar,
                    &mut stdout,
                    &mut status_dirty,
                    &mut last_status_refresh,
                )?;
            }
            let n = match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(err) if err.kind() == ErrorKind::Interrupted => continue,
                Err(err)
                    if status_enabled
                        && (err.kind() == ErrorKind::WouldBlock
                            || err.kind() == ErrorKind::TimedOut) =>
                {
                    // alt-screen 동안 refresh하면 alt buffer로 출력되어 vim 등과 충돌한다.
                    if status_dirty && !alt_screen_active {
                        refresh_status(
                            &mut status_bar,
                            &mut stdout,
                            &mut status_dirty,
                            &mut last_status_refresh,
                        )?;
                    }
                    continue;
                }
                Err(err) => return Err(err).context("read pty output"),
            };
            terminal_output_tracker.observe(&buf[..n]);
            if let Err(err) = stdout.write_all(&buf[..n]) {
                if err.kind() == ErrorKind::BrokenPipe {
                    break;
                }
                return Err(err).context("write stdout");
            }
            if let Err(err) = stdout.flush() {
                if err.kind() == ErrorKind::BrokenPipe {
                    break;
                }
                return Err(err).context("flush stdout");
            }
            if status_enabled {
                status_dirty = true;
            }
        }
        if status_dirty && !prev_alt_screen_active {
            refresh_status(
                &mut status_bar,
                &mut stdout,
                &mut status_dirty,
                &mut last_status_refresh,
            )?;
        }
        Ok(())
    })();

    running.store(false, Ordering::SeqCst);
    let _ = input_thread.join();
    let _ = resize_thread.join();
    output_result
}

fn attach_pty_rows(rows: u16, show_status: bool) -> u16 {
    if show_status && rows > 1 {
        rows - 1
    } else {
        rows.max(1)
    }
}

/// `resize_thread` 한 tick 의 처리 결과. RPC 결과는 세 가지 의미상 분기로 나뉘는데,
/// 결정 자체는 RPC 호출과 무관한 순수 함수로 분리해 단위 테스트가 가능하도록 한다.
#[derive(Debug, PartialEq, Eq)]
enum ResizeTickOutcome {
    /// 성공 — `last` 를 새 사이즈로 advance.
    Advance,
    /// 일시적 RPC 실패 — `last` 를 advance 하지 않고 다음 tick 에서 재시도.
    Retry,
    /// daemon 이 우리 subscriber id 를 모른다고 응답 — attach 가 구조적으로 dead.
    /// 호출자는 running 플래그를 내려 main thread 가 빠져나오도록 해야 한다.
    StaleSubscriberId,
}

/// `resize_thread` 의 tick 본문에서 RPC 결과를 분류한다. PR #15 quad-review HIGH
/// 후속(#2): PR #14 의 "info() 실패 시 last 갱신 보류" 패턴이 PR #15 에서 제거됐던
/// 것을 복원하면서, daemon 이 stale-subscriber-id 응답을 보내는 케이스 (`Some(id)`
/// Resize 인데 그 id 가 더 이상 attach 되어 있지 않을 때 server 가 명시적 에러로
/// surface 하는 경로) 에 대한 처리도 동시에 도입한다.
///
/// stale 판정은 daemon 의 `with_context` 메시지에 포함된 `"subscriber id"` 부분
/// 문자열로 이루어진다 — 와이어 레벨에는 별도 에러 코드가 없어 메시지 매칭이
/// 차선이지만, daemon/client 가 같은 버전으로 빌드되는 본 코드베이스 컨벤션
/// (`AGENTS.md`) 안에서는 충분히 안정적이다. 미래에 코드화된 에러를 도입하면
/// 이 매칭 로직을 한 곳에서만 바꾸면 된다.
fn handle_resize_tick(resize_result: Result<()>) -> ResizeTickOutcome {
    match resize_result {
        Ok(()) => ResizeTickOutcome::Advance,
        Err(err) => {
            let message = format!("{err:#}");
            if message.contains("subscriber id") {
                ResizeTickOutcome::StaleSubscriberId
            } else {
                ResizeTickOutcome::Retry
            }
        }
    }
}

/// status bar refresh + flush + dirty/last-refresh 플래그 동기화를 한 곳에 묶는다.
/// 호출자는 stdout, status_dirty, last_status_refresh 의 mutable 참조를 넘긴다.
/// (4개 호출 지점이 동일 4줄을 반복하던 것을 한 곳으로 모아, 향후 새 경로 추가 시
///  플래그 갱신 누락을 방지한다.)
fn refresh_status(
    status_bar: &mut StatusBar,
    stdout: &mut std::io::Stdout,
    status_dirty: &mut bool,
    last_status_refresh: &mut Instant,
) -> Result<()> {
    status_bar.refresh(stdout)?;
    stdout.flush().context("flush stdout")?;
    *status_dirty = false;
    *last_status_refresh = Instant::now();
    Ok(())
}

/// heartbeat **timing/dirty 서브 게이트**만 평가한다. **호출자는 반드시 `status_enabled`
/// 와 `!alt_screen_active` 가드를 별도로 평가해야 한다** — alt-screen 중에 forced redraw가
/// alt buffer로 새는 회귀를 방지하기 위한 분리. 함수명이 "heartbeat 전체 게이트"로 오인되지
/// 않도록 `heartbeat_due`로 둔다.
///
/// - **idle 경로**: `!status_dirty` 이고 `STATUS_HEARTBEAT` 경과 시 발화 — PTY가 잠잠한
///   동안 외부 DECSTBM 리셋(다른 앱 백그라운드 등)을 self-heal.
/// - **forced 경로**: `STATUS_HEARTBEAT_FORCED` 경과 시 dirty 여부와 무관하게 발화 —
///   PTY가 연속 출력 중이면 read()가 매번 Ok(n)을 반환해 WouldBlock 분기가 fire하지
///   않으므로 status_dirty가 영원히 클리어되지 않는다. 이 경로가 없으면 cmux pane swap /
///   Termius 백그라운드 복귀 후 status 영역 자가복구가 무한히 차단된다.
fn heartbeat_due(elapsed: Duration, status_dirty: bool) -> bool {
    if !status_dirty && elapsed >= STATUS_HEARTBEAT {
        return true;
    }
    elapsed >= STATUS_HEARTBEAT_FORCED
}

fn status_bar_supported(show_status: bool) -> bool {
    let (cols, rows) = terminal_size();
    show_status
        && !status_bar_disabled_by_env()
        && rows > 1
        && cols > 0
        && std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
}

fn status_bar_disabled_by_env() -> bool {
    env_flag_enabled("LTERM_NO_STATUS") || env_flag_disabled("LTERM_STATUS")
}

fn env_flag_enabled(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .is_some_and(|value| matches_env_bool(&value, true))
}

fn env_flag_disabled(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .is_some_and(|value| matches_env_bool(&value, false))
}

fn matches_env_bool(value: &str, expected: bool) -> bool {
    let normalized = value.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "1" | "true" | "yes" | "on" => expected,
        "0" | "false" | "no" | "off" => !expected,
        _ => false,
    }
}

/// Status bar 시각 스타일. cmux/iTerm처럼 SGR을 잘 처리하는 데스크톱 환경에서는
/// Full(theme별 allowlisted SGR)로 강조하고, Termius 같은 모바일 SSH에서는
/// Minimal(plain text)로 폴백해 색 매핑 충돌과 시각 노이즈를 줄인다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StatusStyle {
    Full(StatusTheme),
    Minimal,
}

impl StatusStyle {
    fn sgr(self) -> &'static str {
        match self {
            StatusStyle::Full(StatusTheme::Blue) => "\x1b[0;30;104m",
            StatusStyle::Full(StatusTheme::Green) => "\x1b[0;30;102m",
            StatusStyle::Full(StatusTheme::Magenta) => "\x1b[0;30;105m",
            StatusStyle::Full(StatusTheme::Cyan) => "\x1b[0;30;106m",
            StatusStyle::Full(StatusTheme::Amber) => "\x1b[0;30;103m",
            StatusStyle::Full(StatusTheme::Red) => "\x1b[0;97;41m",
            StatusStyle::Full(StatusTheme::Gray) => "\x1b[0;97;100m",
            StatusStyle::Full(StatusTheme::Plain) | StatusStyle::Minimal => "\x1b[0m",
        }
    }
}

fn resolve_status_style(session_theme: Option<StatusTheme>) -> StatusStyle {
    let (theme, theme_explicit) = resolve_status_theme(session_theme);
    if let Ok(value) = std::env::var("LTERM_STATUS_STYLE") {
        if let Some(style) = parse_status_style(&value) {
            return match style {
                StatusStyle::Full(_) => StatusStyle::Full(theme),
                StatusStyle::Minimal => StatusStyle::Minimal,
            };
        }
    }
    if theme_explicit || !prefers_minimal_status_style() {
        StatusStyle::Full(theme)
    } else {
        StatusStyle::Minimal
    }
}

fn resolve_status_theme(session_theme: Option<StatusTheme>) -> (StatusTheme, bool) {
    if let Some(theme) = session_theme {
        return (theme, true);
    }
    if let Ok(value) = std::env::var("LTERM_STATUS_THEME")
        && let Some(theme) = StatusTheme::parse(&value)
    {
        return (theme, true);
    }
    (StatusTheme::Blue, false)
}

fn parse_status_style(value: &str) -> Option<StatusStyle> {
    match value.trim().to_ascii_lowercase().as_str() {
        "full" => Some(StatusStyle::Full(StatusTheme::Blue)),
        "minimal" => Some(StatusStyle::Minimal),
        _ => None,
    }
}

fn is_ssh_session() -> bool {
    ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"]
        .iter()
        .any(|name| std::env::var(name).is_ok_and(|v| !v.is_empty()))
}

fn prefers_minimal_status_style() -> bool {
    is_ssh_session() || is_termius_session()
}

fn is_termius_session() -> bool {
    ["TERM_PROGRAM", "LC_TERMINAL", "TERMINAL_EMULATOR"]
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .any(|value| value.to_ascii_lowercase().contains("termius"))
}

struct StatusBar {
    session_name: String,
    pane_id: String,
    /// None 이면 status bar를 그리지 않는다 (--no-status / LTERM_NO_STATUS=1 등).
    style: Option<StatusStyle>,
    /// status bar 를 그린 적 있는 terminal rows. cmux/Termius 리사이즈 뒤 새 bottom
    /// row 에 다시 그리기 전에 예전 bottom row 를 지우지 않으면, 그 row 가 본문
    /// 영역으로 편입되며 파란 status line 잔상이 여러 개 남는다.
    ///
    /// 하나의 `last_drawn_row` 만 보관하면 24→20→30 같은 shrink-then-grow 에서
    /// row 24 가 화면 밖으로 밀린 동안 row 20 으로 덮여, 다시 커졌을 때 row 24
    /// 잔상을 지울 기회를 잃는다. 화면 밖 row 는 보관했다가 다시 visible 해지는
    /// redraw/restore 에서 clearing 한다.
    drawn_status_rows: Vec<u16>,
}

impl StatusBar {
    fn enter(
        info: Option<&SessionInfo>,
        style: Option<StatusStyle>,
        stdout: &mut impl Write,
    ) -> Result<Self> {
        let (session_name, pane_id) = info
            .map(|info| {
                (
                    sanitize::terminal_text(&info.name),
                    sanitize::terminal_text(&info.pane_id),
                )
            })
            .unwrap_or_else(|| ("unknown".to_string(), "?".to_string()));
        let mut status = Self {
            session_name,
            pane_id,
            style,
            drawn_status_rows: Vec::new(),
        };
        // attach 시작 시점에 rows를 한 번만 읽어 reserve와 cursor clamp가 동일 값을 본다.
        // 이전에는 reserve_terminal_area와 cursor clamp helper가 각각 terminal_size()를
        // 호출해 둘 사이에 SIGWINCH가 끼면 scroll bottom과 clamp target이 어긋날 수 있는
        // 좁은 race window가 있었다 (Codex quad-review LOW). 또한 `rows-1`이 두 곳에
        // 독립적으로 하드코딩돼 향후 drift 위험도 있었다 (Claude quad-review MEDIUM).
        let (_, rows) = terminal_size();
        status.reserve_terminal_area(stdout, rows)?;
        // reserve_terminal_area의 `\x1b7...\x1b8` cursor save/restore wrap은 사용자의
        // pre-attach cursor 위치를 그대로 복원하는데, 만약 그 위치가 row=rows(=status row)였다면
        // 복원 직후 PTY raw output(셸 echo 등)이 status row를 덮어써 quad-review에서 보고된
        // "커서가 status 영역과 겹침" / "PTY 출력이 status 위에 그려짐" 증상이 발생한다.
        // refresh 시에는 PTY 앱이 자체 cursor 관리하므로 적용하지 않는다.
        // style이 None(status disabled)이면 reserve도 no-op이므로 clamp도 같이 skip한다.
        if status.style.is_some() {
            if let Some(seq) = cursor_clamp_into_scroll_region(rows) {
                stdout
                    .write_all(seq.as_bytes())
                    .context("clamp cursor inside scroll region at attach start")?;
            }
        }
        status.draw(stdout)?;
        stdout.flush().context("flush stdout")?;
        Ok(status)
    }

    fn refresh(&mut self, stdout: &mut impl Write) -> Result<()> {
        let (_, rows) = terminal_size();
        self.reserve_terminal_area(stdout, rows)?;
        self.draw(stdout)
    }

    fn reserve_terminal_area(&self, stdout: &mut impl Write, rows: u16) -> Result<()> {
        if self.style.is_none() {
            return Ok(());
        }
        if rows <= 1 {
            return Ok(());
        }
        let scroll_bottom = rows - 1;
        write!(stdout, "\x1b7\x1b[1;{scroll_bottom}r\x1b8")
            .context("reserve lterm status bar row")?;
        Ok(())
    }

    fn draw(&mut self, stdout: &mut impl Write) -> Result<()> {
        let (cols, rows) = terminal_size();
        self.draw_at_size(stdout, cols, rows)
    }

    fn draw_at_size(&mut self, stdout: &mut impl Write, cols: u16, rows: u16) -> Result<()> {
        // cols<=1이면 마지막 칸을 비우고도 그릴 공간이 없어 autowrap 회피 의미가 사라진다.
        if rows <= 1 || cols <= 1 {
            return Ok(());
        }
        // 마지막 칸까지 채우면 일부 모바일 터미널(예: Termius)에서 deferred-wrap 미구현으로
        // 즉시 스크롤이 발생해 status line이 본문으로 밀려 올라간다. cols-1만 그린다.
        let safe_width = cols.saturating_sub(1).max(1);
        let line = format_status_line(&self.session_name, &self.pane_id, safe_width);
        // \x1b[2K로 행을 먼저 비워야 옛 상태(긴 세션명 잔재)가 남지 않는다.
        // 두 모드 모두 \x1b[0m로 시작해 이전 PTY rendition(bold/italic/inverse 등)이
        // status line으로 새는 것을 차단한다. Full은 theme enum에서 고른 고정 SGR만
        // 적용하므로 사용자 입력 escape sequence가 status row에 주입되지 않는다.
        // (bold(1)은 두 모드 모두에서 사용하지 않는다: bold+black을 흰색으로 렌더하는 터미널이 있다.)
        let sgr = match self.style {
            Some(style) => style.sgr(),
            None => return Ok(()),
        };
        // SGR + cursor save/restore + 본문을 단일 String 으로 buffer 후 write_all 1회 호출.
        // 이는 strict atomicity 보장은 아니다 (write_all은 내부적으로 여러 syscall 가능).
        // TTY/PTY는 POSIX PIPE_BUF atomicity 적용 대상이 아니므로 partial-write 가능성 잔존.
        // 그러나 write! 매크로는 placeholder 마다 write_fmt 가 분할 syscall을 일으켜 SGR sequence
        // 중간이 다른 출력과 interleave 될 위험이 컸다 — buffered write 로 그 위험을 줄인다.
        let rows_to_clear = self.visible_previous_status_rows(rows);
        let mut payload = String::from("\x1b7");
        for previous_row in &rows_to_clear {
            // 새 terminal height 에서 예전 status row 가 보이는 경우 먼저 default 배경으로
            // 지운다. 그렇지 않으면 pane grow / mobile rotate 뒤 예전 status row 가 본문
            // 중간에 남아 "statusline 여러 개"처럼 보인다.
            payload.push_str(&format!("\x1b[{previous_row};1H\x1b[0m\x1b[2K"));
        }
        let current_row_clear = if self.drawn_status_rows.contains(&rows) {
            ""
        } else {
            "\x1b[2K"
        };
        payload.push_str(&format!(
            "\x1b[{rows};1H{current_row_clear}{sgr}{line}\x1b[0m\x1b[K\x1b8"
        ));
        stdout
            .write_all(payload.as_bytes())
            .context("draw lterm status bar")?;
        self.remember_status_row(rows, &rows_to_clear);
        Ok(())
    }

    fn restore(&self, stdout: &mut impl Write) -> Result<()> {
        if self.style.is_none() {
            return Ok(());
        }
        let (_, rows) = terminal_size();
        let rows_to_clear = self.visible_previous_status_rows(rows);
        let mut payload = String::from("\x1b7\x1b[r");
        for previous_row in &rows_to_clear {
            payload.push_str(&format!("\x1b[{previous_row};1H\x1b[0m\x1b[2K"));
        }
        payload.push_str(&format!("\x1b[{rows};1H\x1b[0m\x1b[2K\x1b8"));
        stdout
            .write_all(payload.as_bytes())
            .context("restore terminal after lterm status bar")?;
        stdout.flush().ok();
        Ok(())
    }

    fn visible_previous_status_rows(&self, rows: u16) -> Vec<u16> {
        self.drawn_status_rows
            .iter()
            .copied()
            .filter(|previous| *previous < rows)
            .collect()
    }

    fn remember_status_row(&mut self, rows: u16, rows_to_clear: &[u16]) {
        self.drawn_status_rows
            .retain(|row| *row >= rows || !rows_to_clear.contains(row));
        if !self.drawn_status_rows.contains(&rows) {
            self.drawn_status_rows.push(rows);
        }
    }
}

impl Drop for StatusBar {
    fn drop(&mut self) {
        if self.style.is_some() {
            let mut stdout = std::io::stdout();
            let _ = self.restore(&mut stdout);
        }
    }
}

fn format_status_line(session_name: &str, pane_id: &str, cols: u16) -> String {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;

    let width = cols as usize;
    if width == 0 {
        // 호출자가 cols=0를 차단해야 하지만, 향후 호출 추가 시의 underflow를
        // 방지하기 위한 방어선.
        return String::new();
    }
    let line = format!(" lterm  {session_name}  {pane_id} ");

    // 한글/CJK/이모지(ZWJ family, 국기 regional indicator pair, variation selector,
    // 결합 문자 등)를 grapheme cluster 단위로 truncate해 부분 cluster 잔존을 막는다.
    // 폭 계산도 cluster 단위 UnicodeWidthStr::width로 누적해 wide char가 잘리지 않게 한다.
    //
    // 잘림이 발생한 경우 사용자에게 시각적 단서로 `…`(U+2026, width 1)을 추가한다.
    // 기존 구현은 잘린 wide cluster를 공백으로만 메워서 사용자가 "한글 끝글자 잘림"을
    // 모바일 SSH 렌더링 버그로 오인했다 (quad-review 사용자 보고).
    let mut truncated = if line.width() > width {
        // `…` 한 칸을 둘 자리를 미리 확보. width=1 일 때는 의도적으로 ellipsis를 생략해
        // 첫 cluster 한 글자라도 보여주는 쪽을 택한다 — `…` 자체는 width 1이지만 그것만
        // 표시하면 사용자에게 정보가 0이고, status row가 한 칸뿐이라면 그 한 칸을 어떻게든
        // 콘텐츠로 쓰는 편이 낫다는 판단. (Codex quad-review LOW 코멘트에 대한 명시화.)
        let ellipsis_margin: usize = if width >= 2 { 1 } else { 0 };
        let target = width.saturating_sub(ellipsis_margin);
        let mut acc = 0_usize;
        let mut buf = String::new();
        for cluster in line.graphemes(true) {
            let w = cluster.width();
            if acc + w > target {
                break;
            }
            buf.push_str(cluster);
            acc += w;
        }
        if ellipsis_margin > 0 {
            buf.push('…');
        }
        buf
    } else {
        line
    };

    let display_len = truncated.width();
    truncated.push_str(&" ".repeat(width.saturating_sub(display_len)));
    truncated
}

/// reserve_terminal_area의 cursor save/restore wrap이 attach 시작 시점에 사용자의 pre-attach
/// cursor를 status row(=`rows`)에 복원할 수 있다. 이 경우 PTY raw output(셸 echo 등)이
/// status를 덮어쓴다. 안전한 마지막 row(`rows-1`, scroll region 안쪽 마지막 줄)로 cursor를
/// 강제 이동하는 escape sequence를 반환한다. `rows<=1`이면 의미가 없어 None.
///
/// 호출자는 attach **시작 시점에 한 번만** 사용해야 한다 — refresh 경로에서 호출하면
/// 매 250ms 주기로 cursor가 깜빡이며 본문 사용 흐름을 방해한다.
fn cursor_clamp_into_scroll_region(rows: u16) -> Option<String> {
    if rows <= 1 {
        return None;
    }
    Some(format!("\x1b[{};1H", rows - 1))
}

#[derive(Default)]
struct KeyboardProtocolRestoreState {
    kitty_push_depth: AtomicI32,
    kitty_direct_flags: AtomicU32,
}

/// PTY가 alternate screen buffer에 진입했는지 추적한다. true 동안에는 host-side
/// status bar 그리기를 일시 중단해 vim/htop 같은 alt-screen 앱과 화면 충돌을 피한다.
/// PTY 출력 스트림에서 `\x1b[?1049h/47h/1047h` (enter) 와 대응하는 `l` (exit)을 관찰.
///
/// `Arc<AtomicBool>` + `Ordering::Relaxed` 사용 근거: 현재 단일 attach 스레드에서
/// observe(write)와 attach 루프(read)가 모두 일어나므로 ordering 요구는 없다.
/// `Arc`는 향후 PTY reader/observer 분리를 대비한 형태이며, 그 때에는 publishing
/// data가 동반되지 않으면 Relaxed로 충분하다.
#[derive(Default)]
struct AltScreenState {
    active: AtomicBool,
}

struct TerminalOutputTracker {
    restore_state: Arc<KeyboardProtocolRestoreState>,
    alt_screen: Arc<AltScreenState>,
    tail: Vec<u8>,
}

impl TerminalOutputTracker {
    fn new(
        restore_state: Arc<KeyboardProtocolRestoreState>,
        alt_screen: Arc<AltScreenState>,
    ) -> Self {
        Self {
            restore_state,
            alt_screen,
            tail: Vec::new(),
        }
    }

    fn observe(&mut self, bytes: &[u8]) {
        const TAIL_LIMIT: usize = 64;
        let old_tail = std::mem::take(&mut self.tail);
        if !old_tail.is_empty() && !bytes.is_empty() {
            let prefix_len = bytes.len().min(TAIL_LIMIT);
            let mut boundary = Vec::with_capacity(old_tail.len() + prefix_len);
            boundary.extend_from_slice(&old_tail);
            boundary.extend_from_slice(&bytes[..prefix_len]);
            observe_keyboard_protocol_sequences_after(
                &boundary,
                old_tail.len(),
                &self.restore_state,
            );
            observe_alt_screen_sequences_after(&boundary, old_tail.len(), &self.alt_screen);
        }

        observe_keyboard_protocol_sequences(bytes, &self.restore_state);
        observe_alt_screen_sequences(bytes, &self.alt_screen);

        if bytes.len() >= TAIL_LIMIT {
            self.tail
                .extend_from_slice(&bytes[bytes.len() - TAIL_LIMIT..]);
        } else {
            let old_keep = old_tail.len().min(TAIL_LIMIT - bytes.len());
            self.tail
                .extend_from_slice(&old_tail[old_tail.len() - old_keep..]);
            self.tail.extend_from_slice(bytes);
        }
    }
}

fn observe_keyboard_protocol_sequences(bytes: &[u8], state: &KeyboardProtocolRestoreState) {
    observe_keyboard_protocol_sequences_after(bytes, 0, state);
}

fn observe_keyboard_protocol_sequences_after(
    bytes: &[u8],
    min_final_index: usize,
    state: &KeyboardProtocolRestoreState,
) {
    let mut i = 0;
    while i + 3 < bytes.len() {
        if min_final_index > 0 && i >= min_final_index {
            break;
        }
        if bytes[i] != 0x1b || bytes[i + 1] != b'[' {
            i += 1;
            continue;
        }
        let kind = bytes[i + 2];
        if kind != b'>' && kind != b'<' && kind != b'=' {
            i += 1;
            continue;
        }

        let scan_end = bytes.len().min(i + 64);
        let mut j = i + 3;
        while j < scan_end {
            let byte = bytes[j];
            if (0x40..=0x7e).contains(&byte) {
                if byte == b'u' && j >= min_final_index {
                    let params = &bytes[i + 3..j];
                    match kind {
                        b'>' => observe_kitty_push(state),
                        b'<' => {
                            if let Some(count) = keyboard_protocol_pop_count(params) {
                                observe_kitty_pop(state, count);
                            }
                        }
                        b'=' => {
                            if let Some(change) = keyboard_protocol_direct_change(params) {
                                observe_kitty_direct(state, change);
                            }
                        }
                        _ => {}
                    }
                }
                break;
            }
            j += 1;
        }
        i += 1;
    }
}

const MAX_KEYBOARD_PROTOCOL_RESTORE_POPS: i32 = 16;

fn observe_kitty_push(state: &KeyboardProtocolRestoreState) {
    let _ = state
        .kitty_push_depth
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |depth| {
            Some((depth + 1).min(MAX_KEYBOARD_PROTOCOL_RESTORE_POPS))
        });
}

fn observe_kitty_pop(state: &KeyboardProtocolRestoreState, count: i32) {
    if count <= 0 {
        return;
    }
    let _ = state
        .kitty_push_depth
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |depth| {
            Some(depth.saturating_sub(count))
        });
}

fn observe_kitty_direct(
    state: &KeyboardProtocolRestoreState,
    change: KeyboardProtocolDirectChange,
) {
    if state.kitty_push_depth.load(Ordering::Relaxed) > 0 {
        return;
    }
    let _ =
        state
            .kitty_direct_flags
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                Some(match change.mode {
                    1 => change.flags,
                    2 => current | change.flags,
                    3 => current & !change.flags,
                    _ => current,
                })
            });
}

struct KeyboardProtocolDirectChange {
    flags: u32,
    mode: u8,
}

fn keyboard_protocol_pop_count(params: &[u8]) -> Option<i32> {
    if params.is_empty() {
        return Some(1);
    }
    if !params.iter().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let count = std::str::from_utf8(params).ok()?.parse::<i32>().ok()?;
    Some(count.clamp(0, MAX_KEYBOARD_PROTOCOL_RESTORE_POPS))
}

fn keyboard_protocol_direct_change(params: &[u8]) -> Option<KeyboardProtocolDirectChange> {
    let mut parts = params.split(|byte| *byte == b';');
    let flags = parse_csi_u_number(parts.next().unwrap_or_default())?;
    let mode = match parts.next() {
        Some(part) => parse_csi_u_number(part)?,
        None => 1,
    };
    if !(1..=3).contains(&mode) {
        return None;
    }
    Some(KeyboardProtocolDirectChange {
        flags,
        mode: mode as u8,
    })
}

fn parse_csi_u_number(params: &[u8]) -> Option<u32> {
    if params.is_empty() || !params.iter().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    std::str::from_utf8(params).ok()?.parse::<u32>().ok()
}

fn observe_alt_screen_sequences(bytes: &[u8], alt_screen: &AltScreenState) {
    observe_alt_screen_sequences_after(bytes, 0, alt_screen);
}

/// PTY 출력 바이트를 byte-pattern 으로 스캔해 alt-screen mode set/reset
/// (`\x1b[?47h/l`, `\x1b[?1047h/l`, `\x1b[?1049h/l`)을 추적한다.
///
/// 알려진 한계 (실용적으로 무시 가능):
/// - CSI intermediate byte (`0x20..=0x2f`)는 파싱하지 않음. 드물게 등장하는 private
///   mode + intermediate 조합은 매치되지 않는다 (alt-screen에서는 사용 사례 없음).
/// - OSC/DCS/PM/APC string-control payload 안의 bytes 가 우연히 `\x1b[?1049h` 같이
///   보이면 false-toggle 가능. 단, 정상 OSC payload는 \x1b를 거의 포함하지 않으며
///   포함하더라도 alt-screen 모드 토글은 사용자가 즉시 인지할 수 있는 상태이므로
///   현 구현 수용.
/// - chunk 경계 분할은 [`TerminalOutputTracker`]가 tail buffer 로 처리한다.
///
/// PTY 출력에서 alternate screen buffer 진입/종료 시퀀스(`CSI ? 47 / 1047 / 1049 h|l`)를
/// 관찰해 `alt_screen.active`를 갱신한다. 청크 경계로 잘린 시퀀스는 호출자가 tail 버퍼를
/// 합쳐서 다시 부르며, 그 경우 `min_final_index`로 이전 청크에 이미 본 종결자(`h`/`l`)를
/// 다시 처리하지 않게 막는다.
fn observe_alt_screen_sequences_after(
    bytes: &[u8],
    min_final_index: usize,
    alt_screen: &AltScreenState,
) {
    let mut i = 0;
    while i + 3 < bytes.len() {
        if min_final_index > 0 && i >= min_final_index {
            break;
        }
        if bytes[i] != 0x1b || bytes[i + 1] != b'[' || bytes[i + 2] != b'?' {
            i += 1;
            continue;
        }
        // kbd observer(i+64)와 동일한 scan window를 적용한다. 과거 i+32였으나
        // `?47;1047;1049h`처럼 그룹 set 매개변수가 32바이트를 넘기면 종결자를
        // 찾지 못해 alt-screen 토글이 silently drop되는 비대칭이 있었다.
        // TAIL_LIMIT(64)와 kbd scan(i+64)에 정렬한다.
        let scan_end = bytes.len().min(i + 64);
        let mut j = i + 3;
        while j < scan_end {
            let byte = bytes[j];
            if (0x40..=0x7e).contains(&byte) {
                if (byte == b'h' || byte == b'l') && j >= min_final_index {
                    let params = &bytes[i + 3..j];
                    if alt_screen_param_matches(params) {
                        alt_screen.active.store(byte == b'h', Ordering::Relaxed);
                    }
                }
                break;
            }
            j += 1;
        }
        i += 1;
    }
}

fn alt_screen_param_matches(params: &[u8]) -> bool {
    // xterm은 `?47;1049h` 처럼 여러 private mode를 한 CSI에 묶어 보낼 수 있다.
    // `;`로만 split한다. `:`는 ECMA-48 subparameter separator라 `?47:5h` ("mode 47
    // 의 subparameter 5")는 mode 5와 47을 둘 다 가진 시퀀스가 아니다. 과거에는
    // `:`도 함께 split했으나 false-positive 매치를 유발해 제거했다.
    params
        .split(|byte| *byte == b';')
        .any(|param| matches!(param, b"47" | b"1047" | b"1049"))
}

struct RawModeGuard {
    active: bool,
    keyboard_protocol_restore_state: Arc<KeyboardProtocolRestoreState>,
}

impl RawModeGuard {
    fn enter() -> Result<Self> {
        let active = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
        if active {
            crossterm::terminal::enable_raw_mode().context("enable raw mode")?;
        }
        Ok(Self {
            active,
            keyboard_protocol_restore_state: Arc::new(KeyboardProtocolRestoreState::default()),
        })
    }

    fn keyboard_protocol_restore_state(&self) -> Arc<KeyboardProtocolRestoreState> {
        Arc::clone(&self.keyboard_protocol_restore_state)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if self.active {
            restore_keyboard_protocols(&self.keyboard_protocol_restore_state);
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
}

fn restore_keyboard_protocols(state: &KeyboardProtocolRestoreState) {
    let restore = keyboard_protocol_restore_bytes(state);
    if restore.is_empty() {
        return;
    }

    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(&restore);
    let _ = stdout.flush();
}

fn keyboard_protocol_restore_bytes(state: &KeyboardProtocolRestoreState) -> Vec<u8> {
    let push_depth = state
        .kitty_push_depth
        .load(Ordering::Relaxed)
        .clamp(0, MAX_KEYBOARD_PROTOCOL_RESTORE_POPS);
    let direct_flags = state.kitty_direct_flags.load(Ordering::Relaxed);
    let mut restore = Vec::new();
    for _ in 0..push_depth {
        restore.extend_from_slice(b"\x1b[<u");
    }
    if direct_flags != 0 {
        restore.extend_from_slice(b"\x1b[=0u");
    }
    restore
}

/// Panic context에서 터미널을 안전한 상태로 되돌리기 위한 최소 byte sequence.
///
/// 순서가 중요하다: 먼저 alt-screen에서 빠져나오고(메인 버퍼에서 후속 리셋이 적용되도록),
/// 그 다음 scroll region 등을 리셋한다. 이전 구현은 `\x1b[r`를 먼저 emit해 alt 버퍼에
/// scroll region이 적용되고 메인 버퍼는 그대로 status row가 reserved 상태로 남는 회귀가
/// 있었다 (Codex quad-review HIGH).
///
/// - `\x1b[?1049l`: xterm alt-screen (1049) 종료 — 표준
/// - `\x1b[?47l`  : 구식 alt-screen (47) 종료 — 일부 vim/less 옛 빌드 호환
/// - `\x1b[?1047l`: xterm alt-screen (1047) 종료 — clear 변종
/// - `\x1b[r`     : DECSTBM 리셋 (scroll region 전체 화면) — alt 종료 이후 메인 버퍼에 적용
/// - `\x1b[?25h`  : DECTCEM 커서 보이기
/// - `\x1b[?2004l`: bracketed paste 비활성 (compose panic cleanup)
/// - `\x1b[<u` ×16: kitty keyboard protocol stack pop. 정상 경로의
///   [`MAX_KEYBOARD_PROTOCOL_RESTORE_POPS`] 와 동일한 상한으로 정렬 — panic context에서
///   user push depth를 알 수 없으므로 tracked path가 허용한 최대치까지 시도한다.
///   스택 바닥 이상은 no-op이므로 추가 비용은 byte 수만(~36 bytes)이며, 단일 libc::write
///   범위 안에 머문다 (총 ~93 bytes).
/// - `\x1b[=0u`   : kitty direct mode 비활성 — push 스택을 비운 다음 적용해야 의미 있음
/// - `\x1b[0m`    : SGR 리셋
/// - `\r\n`       : CR + LF (raw mode에서 ONLCR이 꺼져 있어 `\n`만으로는 column 1로 안 감)
///
/// Quad-review 합의 (Claude C2 CRITICAL / Codex 2 HIGH / Forge 2 HIGH): 기존 sequence는
/// kitty keyboard protocol을 복원하지 못해 패닉 후 셸 입력이 모바일 SSH에서 변형되어 들어오는
/// 증상의 직접 원인이었다.
fn panic_terminal_cleanup_bytes() -> &'static [u8] {
    // 16 kitty pops = MAX_KEYBOARD_PROTOCOL_RESTORE_POPS 와 정렬 (정상 경로와 동일 상한).
    b"\x1b[?1049l\x1b[?47l\x1b[?1047l\x1b[r\x1b[?25h\x1b[?2004l\
      \x1b[<u\x1b[<u\x1b[<u\x1b[<u\x1b[<u\x1b[<u\x1b[<u\x1b[<u\
      \x1b[<u\x1b[<u\x1b[<u\x1b[<u\x1b[<u\x1b[<u\x1b[<u\x1b[<u\
      \x1b[=0u\x1b[0m\r\n"
}

/// Panic 발생 시 호출되는 cleanup. Rust stdio mutex를 우회하기 위해 libc::write 직접 호출.
/// stdio mutex는 panic context에서 poison 되어 있을 수 있어 println!/eprint! 가 재패닉을 일으킬 수 있다.
///
/// EINTR / partial-write 시 재시도하여 cleanup sequence 절단을 막는다.
/// panic context이므로 무한 루프를 피하기 위해 최대 8회로 제한 — sequence가 ~93 bytes라
/// 단일 write 통상 1회로 충분하고, 8회는 시그널 폭주에도 보수적.
fn emit_panic_terminal_cleanup() {
    let bytes = panic_terminal_cleanup_bytes();
    let mut written = 0usize;
    let mut attempts = 0;
    while written < bytes.len() && attempts < 8 {
        // SAFETY: STDOUT_FILENO은 정적 fd. write(2)는 async-signal-safe.
        let n = unsafe {
            libc::write(
                libc::STDOUT_FILENO,
                bytes.as_ptr().add(written) as *const libc::c_void,
                bytes.len() - written,
            )
        };
        if n > 0 {
            written += n as usize;
            attempts = 0; // 진행 시 재시도 카운터 리셋
        } else if n == 0 {
            // EOF or fd closed → 더 시도 무의미
            break;
        } else {
            // 음수 → errno 검사. EINTR/EAGAIN 만 재시도.
            // SAFETY: errno_location() / __error() 는 async-signal-safe.
            #[cfg(target_os = "macos")]
            let err = unsafe { *libc::__error() };
            #[cfg(not(target_os = "macos"))]
            let err = unsafe { *libc::__errno_location() };
            if err == libc::EINTR || err == libc::EAGAIN {
                attempts += 1;
                continue;
            }
            break;
        }
    }
}

/// 활성 `lterm attach` 깊이(refcount). 0이면 비활성, >=1이면 panic hook이 cleanup byte를 emit.
/// AtomicBool이었을 때는 nested attach (cmux/Claude Code 안에서 다시 `lterm omx` 등으로 attach
/// 하는 경우)에서 inner Drop이 outer 활성 상태를 false로 덮어써 panic 시 cleanup이 누락되는
/// 버그가 있었다. refcount로 바꿔 nested 시점에 깊이가 누적되고 outer 종료 전에는 0으로
/// 떨어지지 않도록 한다 (quad-review MEDIUM 합의 — Claude C2, Codex 2, Forge 2).
static ATTACH_ACTIVE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// 첫 attach() 호출 시 한 번만 panic hook을 설치한다. 이후 호출은 no-op.
/// hook은 process 종료까지 유지되고, ATTACH_ACTIVE 깊이가 1 이상일 때만 cleanup byte를 emit.
fn ensure_panic_terminal_cleanup_hook() {
    static INSTALLED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    INSTALLED.get_or_init(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // cleanup을 가장 먼저 실행 — previous hook이 stdio mutex poison 등으로
            // 재패닉/abort 하더라도 터미널 복구는 보장된다.
            if ATTACH_ACTIVE.load(std::sync::atomic::Ordering::Acquire) > 0 {
                emit_panic_terminal_cleanup();
            }
            // previous hook을 catch_unwind 로 감싸 chain 단계 panic을 흡수.
            // double-panic → abort 회피로 process가 정상적으로 default 종료 흐름을 따름.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| previous(info)));
        }));
    });
}

/// attach() 시작 시 ATTACH_ACTIVE 깊이를 1 증가시키고, Drop 시 1 감소시킨다.
/// nested attach에서도 outer가 살아있는 동안에는 깊이가 0으로 떨어지지 않으므로 panic hook의
/// cleanup gate가 일관되게 동작한다. panic으로 unwind 시에도 Drop이 호출되어 안전.
/// abort/SIGKILL은 panic hook이 처리한다.
struct AttachActiveGuard;

impl AttachActiveGuard {
    fn enter() -> Self {
        ATTACH_ACTIVE.fetch_add(1, std::sync::atomic::Ordering::Release);
        Self
    }
}

impl Drop for AttachActiveGuard {
    fn drop(&mut self) {
        let prev = ATTACH_ACTIVE.fetch_update(
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
            |prev| prev.checked_sub(1),
        );
        // refcount underflow는 unique-owner 계약 위반(생성 경로 외에서 Drop이 발생).
        // wrapping으로 usize::MAX가 되면 panic hook이 영구히 cleanup을 emit해 디버깅이
        // 어려워지므로 release 빌드에서도 0에 고정하고, dev/test 단계에서는 즉시 잡는다.
        debug_assert!(
            prev.is_ok(),
            "AttachActiveGuard underflow: refcount went below 0"
        );
    }
}

fn stdin_has_input(fd: RawFd, timeout: Duration) -> Result<bool> {
    let timeout_ms = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
    let mut pollfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        let rc = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };
        if rc > 0 {
            if pollfd.revents & libc::POLLERR != 0 {
                bail!("stdin poll reported error events: {:#x}", pollfd.revents);
            }
            if pollfd.revents & libc::POLLNVAL != 0 {
                bail!("stdin poll reported invalid fd: {:#x}", pollfd.revents);
            }
            return Ok((pollfd.revents & (libc::POLLIN | libc::POLLHUP)) != 0);
        }
        if rc == 0 {
            return Ok(false);
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        return Err(err).context("poll stdin");
    }
}

pub fn terminal_size() -> (u16, u16) {
    crossterm::terminal::size().unwrap_or((80, 24))
}

pub fn terminal_cols() -> Option<u16> {
    Some(terminal_size().0)
}

pub fn terminal_rows() -> Option<u16> {
    Some(terminal_size().1)
}

pub fn shell_join(args: &[String]) -> Result<String> {
    shlex::try_join(args.iter().map(String::as_str)).context("quote shell command")
}

pub fn command_exists(name: &str) -> bool {
    find_command(name).is_some()
}

pub fn find_command(name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        return executable_command_path(PathBuf::from(name));
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|p| p.join(name))
            .find_map(executable_command_path)
    })
}

fn executable_command_path(path: PathBuf) -> Option<PathBuf> {
    if !is_executable_file(&path) {
        return None;
    }
    if path.is_absolute() {
        Some(path)
    } else {
        std::env::current_dir().ok().map(|cwd| cwd.join(path))
    }
}

fn is_executable_file(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

pub fn json_pretty<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| json!(value).to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        ATTACH_ACTIVE, ATTACH_OUTPUT_IDLE_TIMEOUT, ATTACH_RESPONSE_HEADER_LIMIT, AltScreenState,
        AttachActiveGuard, AttachMode, ComposeRenderAction, DaemonStatus,
        KeyboardProtocolRestoreState, MAX_KEYBOARD_PROTOCOL_RESTORE_POPS, ResizeTickOutcome,
        STATUS_HEARTBEAT, STATUS_HEARTBEAT_FORCED, StatusBar, StatusStyle, StatusTheme,
        TerminalOutputTracker, alt_screen_param_matches, attach_pty_rows, compose_commit_bytes,
        compose_display_line, compose_is_local_exit_key, compose_pop_grapheme, compose_prompt_line,
        compose_push_paste, compose_refresh_interval, compose_render_action,
        compose_sanitized_display_line, compose_should_commit, compose_tail_start,
        compose_terminal_enter_sequence, compose_terminal_leave_sequence,
        cursor_clamp_into_scroll_region, ensure_panic_terminal_cleanup_hook, format_status_line,
        handle_resize_tick, heartbeat_due, keyboard_protocol_restore_bytes, likely_agent_session,
        matches_env_bool, mobile_client_detected, mobile_transcript_capture_changed,
        observe_keyboard_protocol_sequences, panic_terminal_cleanup_bytes, parse_status_style,
        read_attach_response_header, resolve_attach_mode, resolve_status_style,
        should_mobile_transcript_auto, status_theme_protocol_error, write_mobile_transcript_update,
    };
    use std::io::{BufReader, Cursor, Read};
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    #[test]
    fn compose_commit_bytes_match_input_enter_semantics() {
        assert_eq!(compose_commit_bytes("", true), b"\r");
        assert_eq!(compose_commit_bytes("hello", true), b"hello\r");
        assert_eq!(compose_commit_bytes("hello", false), b"hello");
    }

    #[test]
    fn compose_tail_start_rejects_unsupported_offsets() {
        assert_eq!(compose_tail_start(1).expect("tail 1"), -1);
        assert!(compose_tail_start(0).is_err());
        assert!(compose_tail_start((i32::MAX as usize) + 1).is_err());
    }

    #[test]
    fn compose_refresh_interval_rejects_tight_loops() {
        assert!(compose_refresh_interval(Duration::from_millis(49)).is_err());
        assert_eq!(
            compose_refresh_interval(Duration::from_millis(50)).expect("minimum refresh"),
            Duration::from_millis(50)
        );
    }

    #[test]
    fn attach_mode_parse_accepts_public_values() {
        assert_eq!(AttachMode::parse("auto"), Some(AttachMode::Auto));
        assert_eq!(AttachMode::parse(" RAW "), Some(AttachMode::Raw));
        assert_eq!(AttachMode::parse("mobile"), Some(AttachMode::Mobile));
        assert_eq!(AttachMode::parse("transcript"), Some(AttachMode::Mobile));
        assert_eq!(AttachMode::parse("copy"), None);
    }

    #[test]
    fn resolve_attach_mode_prefers_explicit_then_env_then_auto() {
        let _guard = crate::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let _env_guard = EnvGuard::capture(&["LTERM_ATTACH_MODE"]);

        // SAFETY: crate::TEST_ENV_LOCK is held; EnvGuard restores on drop.
        unsafe {
            std::env::set_var("LTERM_ATTACH_MODE", "mobile");
        }
        assert_eq!(resolve_attach_mode(None).unwrap(), AttachMode::Mobile);
        assert_eq!(
            resolve_attach_mode(Some(AttachMode::Raw)).unwrap(),
            AttachMode::Raw,
            "CLI flags must override LTERM_ATTACH_MODE"
        );

        // SAFETY: crate::TEST_ENV_LOCK is held; EnvGuard restores on drop.
        unsafe {
            std::env::set_var("LTERM_ATTACH_MODE", "bogus");
        }
        assert!(resolve_attach_mode(None).is_err());

        // SAFETY: crate::TEST_ENV_LOCK is held; EnvGuard restores on drop.
        unsafe {
            std::env::remove_var("LTERM_ATTACH_MODE");
        }
        assert_eq!(resolve_attach_mode(None).unwrap(), AttachMode::Auto);
    }

    #[test]
    fn mobile_client_detection_is_explicit_or_termius_not_plain_ssh() {
        let _guard = crate::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let _env_guard = EnvGuard::capture(&[
            "LTERM_MOBILE",
            "TERM_PROGRAM",
            "LC_TERMINAL",
            "TERMINAL_EMULATOR",
            "SSH_CONNECTION",
            "SSH_CLIENT",
            "SSH_TTY",
        ]);

        // SAFETY: crate::TEST_ENV_LOCK is held; EnvGuard restores on drop.
        unsafe {
            std::env::remove_var("LTERM_MOBILE");
            std::env::remove_var("TERM_PROGRAM");
            std::env::remove_var("LC_TERMINAL");
            std::env::remove_var("TERMINAL_EMULATOR");
            std::env::set_var("SSH_TTY", "/dev/ttys001");
        }
        assert!(
            !mobile_client_detected(),
            "plain SSH must not be treated as mobile"
        );

        // SAFETY: crate::TEST_ENV_LOCK is held; EnvGuard restores on drop.
        unsafe {
            std::env::set_var("TERM_PROGRAM", "Termius");
        }
        assert!(mobile_client_detected());

        // SAFETY: crate::TEST_ENV_LOCK is held; EnvGuard restores on drop.
        unsafe {
            std::env::remove_var("TERM_PROGRAM");
            std::env::set_var("LTERM_MOBILE", "1");
        }
        assert!(mobile_client_detected());
    }

    fn sample_session_info(
        name: &str,
        command: &str,
        agent_name: Option<&str>,
    ) -> crate::protocol::SessionInfo {
        crate::protocol::SessionInfo {
            id: format!("{name}-id"),
            name: name.to_string(),
            pane_id: "%test".to_string(),
            parent_pane_id: None,
            parent_session_id: None,
            command: command.to_string(),
            cwd: "/tmp".to_string(),
            created_unix_ms: 0,
            alive: true,
            exit_code: None,
            rows: 24,
            cols: 80,
            attached_clients: 0,
            process_id: None,
            process_group_id: None,
            status_theme: None,
            agent_name: agent_name.map(str::to_string),
        }
    }

    #[test]
    fn likely_agent_session_prefers_metadata_and_uses_conservative_fallbacks() {
        assert!(
            likely_agent_session(&sample_session_info(
                "repo-review-session",
                "/bin/sh",
                Some("repo-review")
            )),
            "persisted LTERM_AGENT metadata should identify configured agents"
        );
        assert!(likely_agent_session(&sample_session_info(
            "omx-lterm",
            "/bin/sh",
            None
        )));
        assert!(likely_agent_session(&sample_session_info(
            "plain",
            "/usr/local/bin/codex --model gpt",
            None
        )));
        assert!(
            !likely_agent_session(&sample_session_info("plain", "/bin/zsh -l", None)),
            "ordinary shells must stay on raw attach in auto mode"
        );
    }

    #[test]
    fn mobile_auto_requires_both_mobile_client_and_likely_agent() {
        let _guard = crate::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let _env_guard = EnvGuard::capture(&[
            "LTERM_MOBILE",
            "TERM_PROGRAM",
            "LC_TERMINAL",
            "TERMINAL_EMULATOR",
        ]);

        // SAFETY: crate::TEST_ENV_LOCK is held; EnvGuard restores on drop.
        unsafe {
            std::env::remove_var("LTERM_MOBILE");
            std::env::remove_var("TERM_PROGRAM");
            std::env::remove_var("LC_TERMINAL");
            std::env::remove_var("TERMINAL_EMULATOR");
        }
        let agent = sample_session_info("codex-lterm", "/bin/sh", Some("codex"));
        assert!(!should_mobile_transcript_auto(&agent));

        // SAFETY: crate::TEST_ENV_LOCK is held; EnvGuard restores on drop.
        unsafe {
            std::env::set_var("LTERM_MOBILE", "1");
        }
        assert!(should_mobile_transcript_auto(&agent));
        assert!(!should_mobile_transcript_auto(&sample_session_info(
            "shell", "/bin/zsh", None
        )));
    }

    #[test]
    fn mobile_transcript_update_appends_suffix_and_refreshes_on_divergence() {
        let mut previous = String::new();
        let mut out = Vec::new();
        assert!(write_mobile_transcript_update(&mut previous, "one\n", &mut out).unwrap());
        assert_eq!(String::from_utf8(out.clone()).unwrap(), "one\n");
        assert_eq!(previous, "one\n");

        out.clear();
        assert!(
            write_mobile_transcript_update(&mut previous, "one\ntwo\n", &mut out).unwrap(),
            "longer capture should write only the suffix"
        );
        assert_eq!(String::from_utf8(out.clone()).unwrap(), "two\n");

        out.clear();
        assert!(!write_mobile_transcript_update(&mut previous, "one\ntwo\n", &mut out).unwrap());
        assert!(out.is_empty());

        out.clear();
        assert!(write_mobile_transcript_update(&mut previous, "fresh\n", &mut out).unwrap());
        let rendered = String::from_utf8(out).unwrap();
        assert!(rendered.contains("--- lterm transcript refresh ---"));
        assert!(rendered.ends_with("fresh\n"));
        assert!(
            !rendered.contains("\x1b[?1049h"),
            "normal-screen transcript helper must not enter alternate screen"
        );
    }

    #[test]
    fn mobile_transcript_update_sanitizes_controls_and_handles_tail_rollover() {
        let mut previous = String::new();
        let mut out = Vec::new();
        assert!(
            write_mobile_transcript_update(
                &mut previous,
                "safe \x1b[31mred\x1b[0m \x1b]52;c;secret\x07done\n",
                &mut out,
            )
            .unwrap()
        );
        let rendered = String::from_utf8(out.clone()).unwrap();
        assert_eq!(rendered, "safe red done\n");
        assert!(!rendered.contains('\x1b'));
        assert!(!rendered.contains("secret"));
        assert_eq!(previous, "safe red done\n");

        previous = "one\ntwo\nthree\n".to_string();
        out.clear();
        assert!(
            write_mobile_transcript_update(&mut previous, "two\nthree\nfour\n", &mut out).unwrap(),
            "tail-window rollover should append only unseen complete-line suffix"
        );
        assert_eq!(String::from_utf8(out.clone()).unwrap(), "four\n");
        assert_eq!(previous, "two\nthree\nfour\n");

        previous = "alpha\nrepeat\nrepeat\n".to_string();
        out.clear();
        assert!(
            write_mobile_transcript_update(&mut previous, "repeat\nrepeat\nomega\n", &mut out)
                .unwrap(),
            "tail-window rollover should prefer the longest repeated-line overlap"
        );
        assert_eq!(String::from_utf8(out).unwrap(), "omega\n");
        assert_eq!(previous, "repeat\nrepeat\nomega\n");
    }

    #[test]
    fn mobile_transcript_capture_changed_compares_sanitized_text() {
        assert!(
            !mobile_transcript_capture_changed("red\n", "\x1b[31mred\x1b[0m\n"),
            "stable raw terminal controls must not force repeated transcript redraws"
        );
        assert!(mobile_transcript_capture_changed(
            "red\n",
            "\x1b[31mred\x1b[0m\nnext\n"
        ));
    }

    #[test]
    fn compose_render_action_keeps_local_prompt_redraws_off_capture_path() {
        assert_eq!(
            compose_render_action(
                false,
                true,
                Duration::from_millis(10),
                Duration::from_millis(100)
            ),
            ComposeRenderAction::PromptOnly,
            "local typing/backspace/paste should redraw only the prompt"
        );
    }

    #[test]
    fn compose_render_action_refreshes_remote_capture_on_timer_or_resize() {
        assert_eq!(
            compose_render_action(
                false,
                false,
                Duration::from_millis(100),
                Duration::from_millis(100)
            ),
            ComposeRenderAction::RemoteCapture,
            "timed refresh must still request remote capture"
        );
        assert_eq!(
            compose_render_action(
                true,
                false,
                Duration::from_millis(0),
                Duration::from_millis(100)
            ),
            ComposeRenderAction::RemoteCapture,
            "resize/full-screen dirtiness must redraw the remote body"
        );
    }

    #[test]
    fn attach_output_idle_timeout_bounds_polling_without_missing_heartbeat() {
        assert!(
            ATTACH_OUTPUT_IDLE_TIMEOUT >= Duration::from_millis(50),
            "attach output loop should not hot-poll at the old 30ms cadence"
        );
        assert!(
            ATTACH_OUTPUT_IDLE_TIMEOUT <= STATUS_HEARTBEAT,
            "idle wakeup must still give heartbeat_due chances before the visible heartbeat window"
        );
    }

    #[test]
    fn compose_enter_commit_policy_allows_blank_enter() {
        assert!(compose_should_commit("", true));
        assert!(compose_should_commit("hello", true));
        assert!(!compose_should_commit("", false));
        assert!(compose_should_commit("hello", false));
    }

    #[test]
    fn compose_exit_keys_are_local_controls() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        assert!(compose_is_local_exit_key(&KeyEvent::new(
            KeyCode::Esc,
            KeyModifiers::NONE
        )));
        assert!(compose_is_local_exit_key(&KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL
        )));
        assert!(compose_is_local_exit_key(&KeyEvent::new(
            KeyCode::Char('d'),
            KeyModifiers::CONTROL
        )));
        assert!(!compose_is_local_exit_key(&KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::NONE
        )));
    }

    #[test]
    fn compose_display_line_truncates_to_display_width() {
        assert_eq!(compose_display_line("abcdef", 3), "abc");
        assert_eq!(compose_display_line("abcdef", 0), "");
        assert_eq!(compose_display_line("한글abc", 4), "한글");
        assert_eq!(compose_display_line("👨‍👩‍👧‍👦abc", 3), "👨‍👩‍👧‍👦a");
    }

    #[test]
    fn compose_display_line_sanitizes_controls() {
        assert_eq!(compose_sanitized_display_line("A\u{0007}B", 10), "AB");
    }

    #[test]
    fn compose_backspace_removes_one_grapheme_cluster() {
        let mut input = String::from("a👨‍👩‍👧‍👦");
        compose_pop_grapheme(&mut input);
        assert_eq!(input, "a");

        let mut combining = String::from("e\u{0301}");
        compose_pop_grapheme(&mut combining);
        assert_eq!(combining, "");
    }

    #[test]
    fn compose_paste_appends_text_to_input_buffer() {
        let mut input = String::from("pre");
        compose_push_paste(&mut input, "\t붙여넣기\n");
        assert_eq!(input, "pre\t붙여넣기\n");
    }

    #[test]
    fn compose_terminal_sequences_toggle_bracketed_paste() {
        let mut enter = Vec::new();
        compose_terminal_enter_sequence(&mut enter).expect("compose enter sequence");
        assert!(
            enter
                .windows(b"\x1b[?2004h".len())
                .any(|w| w == b"\x1b[?2004h"),
            "compose enter must enable bracketed paste: {enter:?}"
        );

        let mut leave = Vec::new();
        compose_terminal_leave_sequence(&mut leave).expect("compose leave sequence");
        assert!(
            leave
                .windows(b"\x1b[?2004l".len())
                .any(|w| w == b"\x1b[?2004l"),
            "compose leave must disable bracketed paste: {leave:?}"
        );
    }

    #[test]
    fn compose_prompt_line_keeps_input_tail_visible() {
        let (line, cursor_col) = compose_prompt_line("abcdef", 5);
        assert_eq!(line, "bcdef");
        assert_eq!(cursor_col, 5);
    }

    /// 지정된 환경 변수의 현재 값을 저장하고, Drop 시 원래 값(또는 unset 상태)으로 복원한다.
    /// crate::TEST_ENV_LOCK을 잡은 상태에서만 사용해야 한다.
    struct EnvGuard {
        saved: Vec<(&'static str, Option<String>)>,
    }

    impl EnvGuard {
        fn capture(names: &[&'static str]) -> Self {
            let saved = names
                .iter()
                .map(|name| (*name, std::env::var(name).ok()))
                .collect();
            Self { saved }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: 호출자는 crate::TEST_ENV_LOCK을 잡고 있어야 한다 (테스트 컨벤션).
            unsafe {
                for (name, value) in &self.saved {
                    match value {
                        Some(v) => std::env::set_var(name, v),
                        None => std::env::remove_var(name),
                    }
                }
            }
        }
    }

    struct AttachActiveStateGuard {
        saved: usize,
    }

    impl AttachActiveStateGuard {
        fn reset_to_zero() -> Self {
            let saved = ATTACH_ACTIVE.swap(0, Ordering::AcqRel);
            Self { saved }
        }
    }

    impl Drop for AttachActiveStateGuard {
        fn drop(&mut self) {
            ATTACH_ACTIVE.store(self.saved, Ordering::Release);
        }
    }

    #[test]
    fn status_bar_reserves_one_terminal_row_when_possible() {
        assert_eq!(attach_pty_rows(24, true), 23);
        assert_eq!(attach_pty_rows(1, true), 1);
        assert_eq!(attach_pty_rows(24, false), 24);
    }

    #[test]
    fn attach_header_reader_preserves_buffered_pty_tail() {
        let mut reader =
            BufReader::with_capacity(8, Cursor::new(b"{\"ok\":true}\nPTY-TAIL".to_vec()));

        let header = read_attach_response_header(&mut reader).expect("read attach header");
        assert_eq!(header, b"{\"ok\":true}\n");

        let mut tail = String::new();
        reader
            .read_to_string(&mut tail)
            .expect("read buffered tail");
        assert_eq!(
            tail, "PTY-TAIL",
            "chunked handshake must not drop PTY bytes"
        );
    }

    #[test]
    fn attach_header_reader_enforces_header_cap_including_newline() {
        let mut too_large = vec![b'a'; ATTACH_RESPONSE_HEADER_LIMIT];
        too_large.push(b'\n');
        let mut reader = BufReader::new(Cursor::new(too_large));

        let err = read_attach_response_header(&mut reader).expect_err("oversized header");
        assert!(
            err.to_string().contains("attach header too large"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn attach_header_reader_accepts_newline_at_cap_boundary() {
        let mut at_limit = vec![b'a'; ATTACH_RESPONSE_HEADER_LIMIT - 1];
        at_limit.push(b'\n');
        let mut reader = BufReader::new(Cursor::new(at_limit));

        let header = read_attach_response_header(&mut reader).expect("header at cap boundary");
        assert_eq!(header.len(), ATTACH_RESPONSE_HEADER_LIMIT);
        assert_eq!(header.last(), Some(&b'\n'));
    }

    #[test]
    fn status_bar_redraw_clears_previous_row_after_resize() {
        let mut status_bar = StatusBar {
            session_name: "omx-lterm".to_string(),
            pane_id: "%0".to_string(),
            style: Some(StatusStyle::Full(StatusTheme::Blue)),
            drawn_status_rows: Vec::new(),
        };
        let mut output = Vec::new();

        status_bar
            .draw_at_size(&mut output, 80, 20)
            .expect("initial draw");
        output.clear();
        status_bar
            .draw_at_size(&mut output, 80, 24)
            .expect("resized draw");

        let payload = String::from_utf8(output).expect("status payload should be utf8");
        assert!(
            payload.contains("\x1b[20;1H\x1b[0m\x1b[2K"),
            "old status row must be cleared when it becomes body text: {payload:?}"
        );
        assert!(
            payload.contains("\x1b[24;1H\x1b[2K\x1b[0;30;104m"),
            "new status row must still be drawn: {payload:?}"
        );
        status_bar.style = None;
    }

    #[test]
    fn status_bar_redraw_clears_rows_hidden_by_shrink_then_growth() {
        let mut status_bar = StatusBar {
            session_name: "omx-lterm".to_string(),
            pane_id: "%0".to_string(),
            style: Some(StatusStyle::Full(StatusTheme::Blue)),
            drawn_status_rows: Vec::new(),
        };
        let mut output = Vec::new();

        status_bar
            .draw_at_size(&mut output, 80, 24)
            .expect("initial tall draw");
        output.clear();
        status_bar
            .draw_at_size(&mut output, 80, 20)
            .expect("shrink draw");
        let shrink_payload = String::from_utf8(output.clone()).expect("utf8 payload");
        assert!(
            !shrink_payload.contains("\x1b[24;1H\x1b[0m\x1b[2K"),
            "off-screen old status row is retained, not cleared during shrink"
        );

        output.clear();
        status_bar
            .draw_at_size(&mut output, 80, 30)
            .expect("growth draw");

        let grow_payload = String::from_utf8(output).expect("status payload should be utf8");
        assert!(
            grow_payload.contains("\x1b[24;1H\x1b[0m\x1b[2K"),
            "old tall status row must be cleared once it becomes visible again: {grow_payload:?}"
        );
        assert!(
            grow_payload.contains("\x1b[20;1H\x1b[0m\x1b[2K"),
            "intermediate shrink status row must also be cleared: {grow_payload:?}"
        );
        assert!(
            grow_payload.contains("\x1b[30;1H\x1b[2K\x1b[0;30;104m"),
            "new status row must still be drawn: {grow_payload:?}"
        );
        status_bar.style = None;
    }

    #[test]
    fn status_bar_same_row_redraw_avoids_clear_to_reduce_flicker() {
        let mut status_bar = StatusBar {
            session_name: "omx-lterm".to_string(),
            pane_id: "%0".to_string(),
            style: Some(StatusStyle::Full(StatusTheme::Blue)),
            drawn_status_rows: Vec::new(),
        };
        let mut output = Vec::new();

        status_bar
            .draw_at_size(&mut output, 80, 24)
            .expect("initial draw");
        output.clear();
        status_bar
            .draw_at_size(&mut output, 80, 24)
            .expect("same-row redraw");

        let payload = String::from_utf8(output).expect("status payload should be utf8");
        assert!(
            payload.contains("\x1b[24;1H\x1b[0;30;104m"),
            "same-row redraw should repaint in place with the status style: {payload:?}"
        );
        assert!(
            !payload.contains("\x1b[24;1H\x1b[2K"),
            "same-row heartbeat redraws should not clear the current row and visibly flicker: {payload:?}"
        );
        assert!(
            payload.contains("\x1b[0m\x1b[K\x1b8"),
            "same-row redraws should still clear from the padded status text to line end, covering the intentionally unwritten final column without a full-row clear: {payload:?}"
        );
        status_bar.style = None;
    }

    #[test]
    fn new_sessions_inherit_current_terminal_capability_env_without_overwriting_explicit_values() {
        let _lock = crate::TEST_ENV_LOCK.lock().unwrap();
        let _env_guard = EnvGuard::capture(&[
            "TERM",
            "COLORTERM",
            "TERM_PROGRAM",
            "LC_TERMINAL",
            "LTERM_AGENT",
        ]);

        // SAFETY: crate::TEST_ENV_LOCK is held; EnvGuard restores on drop.
        unsafe {
            std::env::set_var("TERM", "xterm-256color");
            std::env::set_var("COLORTERM", "truecolor");
            std::env::set_var("TERM_PROGRAM", "Termius");
            std::env::set_var("LC_TERMINAL", "iTerm2");
            std::env::set_var("LTERM_AGENT", "host-value");
        }

        let mut env = std::collections::HashMap::from([
            ("LTERM_AGENT".to_string(), "omx".to_string()),
            ("LC_TERMINAL".to_string(), "explicit-client".to_string()),
        ]);
        super::inherit_terminal_capability_env(&mut env);

        assert_eq!(env.get("TERM").map(String::as_str), Some("xterm-256color"));
        assert_eq!(env.get("COLORTERM").map(String::as_str), Some("truecolor"));
        assert_eq!(env.get("TERM_PROGRAM").map(String::as_str), Some("Termius"));
        assert_eq!(
            env.get("LC_TERMINAL").map(String::as_str),
            Some("explicit-client"),
            "caller-supplied session env should stay authoritative"
        );
        assert_eq!(
            env.get("LTERM_AGENT").map(String::as_str),
            Some("omx"),
            "only terminal capability keys should be inherited"
        );
    }

    /// PR #15 quad-review HIGH 후속(#2): 성공 응답은 last 를 advance 시키는 Outcome
    /// 으로 매핑되어야 한다. resize_thread 의 정상 경로가 흔들리지 않는지 핀 박는다.
    #[test]
    fn handle_resize_tick_success_advances_last() {
        let outcome = handle_resize_tick(Ok(()));
        assert_eq!(outcome, ResizeTickOutcome::Advance);
    }

    /// PR #15 quad-review HIGH 후속(#2): "subscriber id" 키워드를 포함한 에러는 stale
    /// 분기로 매핑되어야 한다. 호출자는 이를 받아 running 플래그를 내리고 종료한다.
    /// daemon 측 메시지 (`server.rs`: `"resize: subscriber id {id} no longer attached"`)
    /// 와 `with_context` 가 만든 chained 메시지를 모두 매칭하도록 substring 매치를
    /// 사용한다.
    #[test]
    fn handle_resize_tick_stale_subscriber_id_signals_break() {
        let err: anyhow::Error = anyhow::anyhow!("resize: subscriber id 7 no longer attached");
        let outcome = handle_resize_tick(Err(err));
        assert_eq!(outcome, ResizeTickOutcome::StaleSubscriberId);
    }

    /// `with_context` 로 wrap 된 anyhow chain 도 `{err:#}` 포맷으로 substring 이 잡혀야
    /// 한다 — daemon 응답이 `Error::root_cause` 가 아닌 chain 의 일부로 도착하는
    /// 케이스 (RPC 어댑터 측의 wrap) 를 시뮬레이션한다.
    #[test]
    fn handle_resize_tick_stale_id_in_chained_anyhow_context_still_matches() {
        let inner: anyhow::Error = anyhow::anyhow!("resize: subscriber id 99 no longer attached");
        let chained = inner.context("rpc dispatch failed");
        let outcome = handle_resize_tick(Err(chained));
        assert_eq!(outcome, ResizeTickOutcome::StaleSubscriberId);
    }

    /// PR #15 quad-review HIGH 후속(#2): 그 밖의 transient 실패는 Retry 로 매핑되어
    /// 호출자가 last 를 advance 하지 않고 다음 tick 에서 재시도하도록 한다 (PR #14
    /// 의 "info() 실패 시 last 갱신 보류" 패턴 복원).
    #[test]
    fn handle_resize_tick_transient_failure_is_retry() {
        let err: anyhow::Error = anyhow::anyhow!("connection refused");
        let outcome = handle_resize_tick(Err(err));
        assert_eq!(outcome, ResizeTickOutcome::Retry);
    }

    #[test]
    fn cursor_clamp_emits_position_at_scroll_region_bottom() {
        // 일반 24행 터미널: scroll region은 1..23이므로 안전한 마지막 row는 23(=rows-1).
        assert_eq!(
            cursor_clamp_into_scroll_region(24),
            Some("\x1b[23;1H".to_string())
        );
        // 2행 (status bar 활성 최소): scroll region은 1행만, cursor를 row 1로.
        assert_eq!(
            cursor_clamp_into_scroll_region(2),
            Some("\x1b[1;1H".to_string())
        );
        // rows<=1: clamp 자체가 의미 없음 (status bar도 미활성).
        assert_eq!(cursor_clamp_into_scroll_region(1), None);
        assert_eq!(cursor_clamp_into_scroll_region(0), None);
    }

    #[test]
    fn status_line_is_exact_terminal_width() {
        // truncate 시 마지막에 ellipsis(`…`, width 1)가 단서로 붙는다.
        assert_eq!(format_status_line("recovery", "%0", 12), " lterm  rec…");
        // 잘림 없는 경우는 여전히 공백으로 정확히 패딩.
        assert_eq!(format_status_line("api", "%1", 16), " lterm  api  %1 ");
        assert_eq!(format_status_line("api", "%1", 18), " lterm  api  %1   ");
    }

    #[test]
    fn status_line_truncation_appends_ellipsis_for_cjk() {
        use unicode_width::UnicodeWidthStr;
        // CJK 잘림 시 ellipsis가 표시되어 "끝글자 잘림" UX 오인을 방지한다.
        // 잘림 위치는 반드시 콘텐츠의 끝(trailing padding 직전)이어야 한다.
        let truncated = format_status_line("매우긴이름매우긴이름", "%1", 20);
        assert_eq!(truncated.width(), 20);
        // contains만으로는 잘못된 위치(중간 등)에 삽입돼도 통과하므로,
        // trim_end (trailing 공백 제거) 후 ends_with로 placement까지 검증.
        assert!(
            truncated.trim_end().ends_with('…'),
            "ellipsis는 콘텐츠 끝에 와야 함: {truncated:?}"
        );
        // 잘림이 일어나지 않는 충분한 width에서는 ellipsis가 추가되지 않아야 한다.
        let untruncated = format_status_line("api", "%1", 24);
        assert!(
            !untruncated.contains('…'),
            "잘림 없는 경우 ellipsis 없어야 함: {untruncated:?}"
        );
    }

    #[test]
    fn status_line_handles_zero_and_one_cols() {
        // cols=0: 안전하게 빈 문자열. (caller가 차단하지만 방어선 검증)
        assert_eq!(format_status_line("anything", "%1", 0), "");
        // cols=1: ellipsis margin이 0이라 `…`가 추가되지 않고 공백만 채워진다.
        let one = format_status_line("anything", "%1", 1);
        assert_eq!(one.chars().count(), 1);
        assert!(!one.contains('…'));
    }

    #[test]
    fn status_line_width_two_is_minimum_ellipsis_active() {
        use unicode_width::UnicodeWidthStr;
        // width=2는 ellipsis 활성 최소 boundary. target=1이라 첫 cluster 한 칸 + `…` = 2칸.
        // 누가 `if width >= 2`를 `>= 3`으로 바꾸면 회귀로 잡힘.
        let two = format_status_line("anything", "%1", 2);
        assert_eq!(two.width(), 2);
        assert!(two.contains('…'), "width=2는 ellipsis 활성 최소값: {two:?}");
        assert!(
            two.trim_end().ends_with('…'),
            "ellipsis 위치 정확해야: {two:?}"
        );
    }

    #[test]
    fn status_line_handles_cjk_display_width() {
        use unicode_width::UnicodeWidthStr;
        // 한글 4글자 = 디스플레이 폭 8 cells.
        // " lterm  사용자명  %1 " = 7 + 8 + 2 + 2 + 1 = 20 cells.
        let line = format_status_line("사용자명", "%1", 24);
        assert_eq!(line.width(), 24);
        // 너비 16에 맞추면 일부 한글이 잘려야 한다.
        let truncated = format_status_line("사용자명", "%1", 16);
        assert!(truncated.width() <= 16);
    }

    #[test]
    fn status_line_handles_emoji_width() {
        use unicode_width::UnicodeWidthStr;
        // 이모지는 보통 폭 2.
        let line = format_status_line("🚀ok", "%2", 24);
        assert_eq!(line.width(), 24);
    }

    #[test]
    fn status_line_keeps_zwj_emoji_family_intact() {
        use unicode_width::UnicodeWidthStr;
        // 👨‍👩‍👧 = 5 codepoints, 1 grapheme cluster, display width 2.
        let line = format_status_line("👨\u{200d}👩\u{200d}👧", "%1", 24);
        assert_eq!(line.width(), 24);
        // ZWJ가 잘려서 단독 👨 가 결과에 남는 일이 없어야 한다.
        // (truncate가 char 단위였다면 발생) — 본 case는 width 24 > 콘텐츠 폭이라 truncate 없음.
        assert!(line.contains("👨\u{200d}👩\u{200d}👧"));
    }

    #[test]
    fn status_line_keeps_regional_indicator_flag_intact() {
        use unicode_width::UnicodeWidthStr;
        // 🇰🇷 = 2 regional indicators, 1 grapheme cluster, display width 2.
        let line = format_status_line("🇰🇷", "%1", 16);
        assert_eq!(line.width(), 16);
        assert!(line.contains("🇰🇷"));
    }

    #[test]
    fn status_line_truncate_does_not_split_grapheme_cluster() {
        use unicode_width::UnicodeWidthStr;
        // 한글 4자 (각 width 2 = 8) + " lterm  " prefix(8) + "  %1 "(5) = 21 cells.
        // width 14 로 자르면 " lterm  " (8) + "사" (2) = 10 또는 " lterm  사" (10) 까지만 들어가고
        // 결합 base + 변형 selector 케이스에서도 부분 cluster가 남지 않아야 한다.
        let combining = "e\u{301}"; // é = 1 grapheme, width 1
        let line = format_status_line(combining, "%1", 24);
        assert_eq!(line.width(), 24);
        // 결합 마크가 base 와 떨어지면 안 됨
        assert!(line.contains("e\u{301}"));
    }

    #[test]
    fn status_env_bool_parser_accepts_common_values() {
        assert!(matches_env_bool("1", true));
        assert!(matches_env_bool("true", true));
        assert!(matches_env_bool("YES", true));
        assert!(matches_env_bool("off", false));
        assert!(!matches_env_bool("maybe", true));
    }

    #[test]
    fn observes_kitty_keyboard_protocol_enable_sequences() {
        let state = KeyboardProtocolRestoreState::default();
        observe_keyboard_protocol_sequences(b"before\x1b[>1uafter", &state);
        assert_eq!(state.kitty_push_depth.load(Ordering::Relaxed), 1);
        assert_eq!(state.kitty_direct_flags.load(Ordering::Relaxed), 0);
        assert_eq!(keyboard_protocol_restore_bytes(&state), b"\x1b[<u");

        let state = KeyboardProtocolRestoreState::default();
        observe_keyboard_protocol_sequences(b"\x1b[=3;1u", &state);
        assert_eq!(state.kitty_push_depth.load(Ordering::Relaxed), 0);
        assert_eq!(state.kitty_direct_flags.load(Ordering::Relaxed), 3);
        assert_eq!(keyboard_protocol_restore_bytes(&state), b"\x1b[=0u");
    }

    #[test]
    fn balances_kitty_keyboard_protocol_push_pop_sequences() {
        let state = KeyboardProtocolRestoreState::default();
        observe_keyboard_protocol_sequences(b"\x1b[>1uinside\x1b[<u", &state);
        assert_eq!(state.kitty_push_depth.load(Ordering::Relaxed), 0);
        assert!(keyboard_protocol_restore_bytes(&state).is_empty());
    }

    #[test]
    fn observes_kitty_keyboard_protocol_disable_sequences() {
        let state = KeyboardProtocolRestoreState::default();
        observe_keyboard_protocol_sequences(b"\x1b[=3u\x1b[=0u", &state);
        assert_eq!(state.kitty_direct_flags.load(Ordering::Relaxed), 0);
        assert!(keyboard_protocol_restore_bytes(&state).is_empty());
    }

    #[test]
    fn restores_push_without_clobbering_direct_mode() {
        let state = KeyboardProtocolRestoreState::default();
        observe_keyboard_protocol_sequences(b"\x1b[>1u\x1b[=3u", &state);
        assert_eq!(keyboard_protocol_restore_bytes(&state), b"\x1b[<u");
    }

    #[test]
    fn restores_direct_mode_after_unmatched_push_pop() {
        let state = KeyboardProtocolRestoreState::default();
        observe_keyboard_protocol_sequences(b"\x1b[=3u\x1b[>1u", &state);
        assert_eq!(keyboard_protocol_restore_bytes(&state), b"\x1b[<u\x1b[=0u");
    }

    #[test]
    fn direct_disable_inside_push_does_not_clear_outer_restore() {
        let state = KeyboardProtocolRestoreState::default();
        observe_keyboard_protocol_sequences(b"\x1b[=3u\x1b[>1u\x1b[=0u", &state);
        assert_eq!(keyboard_protocol_restore_bytes(&state), b"\x1b[<u\x1b[=0u");
    }

    #[test]
    fn observes_kitty_keyboard_protocol_pop_counts() {
        let state = KeyboardProtocolRestoreState::default();
        observe_keyboard_protocol_sequences(b"\x1b[>1u\x1b[>1u\x1b[<2u", &state);
        assert_eq!(state.kitty_push_depth.load(Ordering::Relaxed), 0);
        assert!(keyboard_protocol_restore_bytes(&state).is_empty());
    }

    #[test]
    fn observes_kitty_keyboard_protocol_direct_modes() {
        let state = KeyboardProtocolRestoreState::default();
        observe_keyboard_protocol_sequences(b"\x1b[=1;3u", &state);
        assert_eq!(state.kitty_direct_flags.load(Ordering::Relaxed), 0);
        assert!(keyboard_protocol_restore_bytes(&state).is_empty());

        observe_keyboard_protocol_sequences(b"\x1b[=1;2u", &state);
        assert_eq!(state.kitty_direct_flags.load(Ordering::Relaxed), 1);
        observe_keyboard_protocol_sequences(b"\x1b[=1;3u", &state);
        assert_eq!(state.kitty_direct_flags.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn panic_terminal_cleanup_bytes_emits_safe_recovery_sequence() {
        let bytes = panic_terminal_cleanup_bytes();
        // 정확한 sequence (순서 중요): alt-screen(1049/47/1047) 종료 → scroll region 리셋 →
        // 커서 visible → bracketed paste disable → kitty pop ×16 → kitty direct disable →
        // SGR 리셋 → CR+LF.
        // alt-screen 종료가 \x1b[r 보다 먼저 와서 reset이 메인 버퍼에 적용되어야 한다.
        // pop 16회 = MAX_KEYBOARD_PROTOCOL_RESTORE_POPS 와 정렬 (스택 바닥 이상은 no-op).
        let expected = b"\x1b[?1049l\x1b[?47l\x1b[?1047l\x1b[r\x1b[?25h\x1b[?2004l\
                         \x1b[<u\x1b[<u\x1b[<u\x1b[<u\x1b[<u\x1b[<u\x1b[<u\x1b[<u\
                         \x1b[<u\x1b[<u\x1b[<u\x1b[<u\x1b[<u\x1b[<u\x1b[<u\x1b[<u\
                         \x1b[=0u\x1b[0m\r\n";
        assert_eq!(bytes, expected);
        // pop 시퀀스가 정확히 16번 등장하는지 검증 (회귀 시 즉시 catch)
        let pop_count = bytes.windows(4).filter(|w| *w == b"\x1b[<u").count();
        assert_eq!(
            pop_count, MAX_KEYBOARD_PROTOCOL_RESTORE_POPS as usize,
            "kitty pop은 MAX_KEYBOARD_PROTOCOL_RESTORE_POPS와 일치"
        );
        // alt-screen 종료가 scroll region reset보다 먼저 위치하는지 명시 검증
        let pos_alt = bytes
            .windows(b"\x1b[?1049l".len())
            .position(|w| w == b"\x1b[?1049l")
            .expect("1049l in cleanup");
        let pos_scroll = bytes
            .windows(b"\x1b[r".len())
            .position(|w| w == b"\x1b[r")
            .expect("scroll reset in cleanup");
        assert!(
            pos_alt < pos_scroll,
            "alt-screen exit must precede scroll region reset (otherwise reset applies to alt buffer)"
        );
        // 모든 escape sequence는 ESC([)로 시작
        assert!(bytes.starts_with(b"\x1b["));
        // 마지막은 LF, 그 직전은 CR (raw mode에서 ONLCR이 꺼져 있어 \n 단독으로는 column 1로 못 감)
        let len = bytes.len();
        assert_eq!(bytes[len - 1], b'\n');
        assert_eq!(bytes[len - 2], b'\r');
        // kitty keyboard protocol pop과 direct disable이 포함되어야 함
        assert!(bytes.windows(4).any(|w| w == b"\x1b[<u"));
        assert!(bytes.windows(5).any(|w| w == b"\x1b[=0u"));
        assert!(bytes.windows(8).any(|w| w == b"\x1b[?2004l"));
    }

    #[test]
    fn ensure_panic_terminal_cleanup_hook_is_idempotent() {
        // OnceLock 으로 보호되어 다회 호출이 안전해야 한다
        ensure_panic_terminal_cleanup_hook();
        ensure_panic_terminal_cleanup_hook();
        ensure_panic_terminal_cleanup_hook();
        // 도달 = no panic. 추가 검증은 panic을 실제로 발생시켜야 하므로 통합 테스트로 미룸.
    }

    #[test]
    fn attach_active_guard_increments_and_decrements_depth() {
        let _guard = crate::TEST_ATTACH_FLAG_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());

        // 사전 capture (다른 코드 경로에서 set 했을 수 있음)는 RAII로 복원한다.
        let _state_guard = AttachActiveStateGuard::reset_to_zero();

        assert_eq!(ATTACH_ACTIVE.load(Ordering::Acquire), 0);
        {
            let _g = AttachActiveGuard::enter();
            assert_eq!(ATTACH_ACTIVE.load(Ordering::Acquire), 1);
        }
        assert_eq!(ATTACH_ACTIVE.load(Ordering::Acquire), 0);
    }

    #[test]
    fn attach_active_guard_supports_nested_attach() {
        // nested attach (예: cmux 안에서 lterm omx로 attach 후 그 안에서 다시 lterm attach)
        // 시 inner Drop이 outer의 활성 상태를 무효화하지 않아야 한다. AtomicBool이었을 때의
        // 회귀를 회귀 테스트로 잡는다 (quad-review MEDIUM 합의).
        let _guard = crate::TEST_ATTACH_FLAG_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let _state_guard = AttachActiveStateGuard::reset_to_zero();

        let outer = AttachActiveGuard::enter();
        assert_eq!(ATTACH_ACTIVE.load(Ordering::Acquire), 1);
        {
            let _inner = AttachActiveGuard::enter();
            assert_eq!(ATTACH_ACTIVE.load(Ordering::Acquire), 2);
        }
        // inner Drop 후에도 outer는 살아있어 깊이는 1 유지 — panic hook이 cleanup 발화 가능.
        assert_eq!(ATTACH_ACTIVE.load(Ordering::Acquire), 1);
        drop(outer);
        assert_eq!(ATTACH_ACTIVE.load(Ordering::Acquire), 0);
    }

    #[test]
    fn observes_keyboard_protocol_sequences_split_across_chunks_once() {
        let state = Arc::new(KeyboardProtocolRestoreState::default());
        let alt = Arc::new(AltScreenState::default());
        let mut tracker = TerminalOutputTracker::new(Arc::clone(&state), Arc::clone(&alt));
        tracker.observe(b"\x1b[>");
        tracker.observe(b"1u");
        tracker.observe(b"plain output");
        assert_eq!(state.kitty_push_depth.load(Ordering::Relaxed), 1);
        assert_eq!(keyboard_protocol_restore_bytes(&state), b"\x1b[<u");
    }

    #[test]
    fn observes_whole_keyboard_protocol_sequences_once_after_tail() {
        let state = Arc::new(KeyboardProtocolRestoreState::default());
        let alt = Arc::new(AltScreenState::default());
        let mut tracker = TerminalOutputTracker::new(Arc::clone(&state), Arc::clone(&alt));
        tracker.observe(b"plain output");
        tracker.observe(b"\x1b[>1u");
        assert_eq!(state.kitty_push_depth.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn alt_screen_tracker_observes_enter_and_exit() {
        let state = Arc::new(AltScreenState::default());
        let kbd = Arc::new(KeyboardProtocolRestoreState::default());
        let mut tracker = TerminalOutputTracker::new(Arc::clone(&kbd), Arc::clone(&state));

        assert!(!state.active.load(Ordering::Relaxed));
        tracker.observe(b"plain\x1b[?1049hinside vim");
        assert!(state.active.load(Ordering::Relaxed));
        tracker.observe(b"more vim output");
        assert!(state.active.load(Ordering::Relaxed));
        tracker.observe(b"\x1b[?1049lback to shell");
        assert!(!state.active.load(Ordering::Relaxed));
    }

    #[test]
    fn alt_screen_tracker_handles_47_and_1047() {
        let state = Arc::new(AltScreenState::default());
        let kbd = Arc::new(KeyboardProtocolRestoreState::default());
        let mut tracker = TerminalOutputTracker::new(Arc::clone(&kbd), Arc::clone(&state));

        tracker.observe(b"\x1b[?47h");
        assert!(state.active.load(Ordering::Relaxed));
        tracker.observe(b"\x1b[?47l");
        assert!(!state.active.load(Ordering::Relaxed));

        tracker.observe(b"\x1b[?1047h");
        assert!(state.active.load(Ordering::Relaxed));
        tracker.observe(b"\x1b[?1047l");
        assert!(!state.active.load(Ordering::Relaxed));
    }

    #[test]
    fn alt_screen_tracker_ignores_unrelated_private_modes() {
        let state = Arc::new(AltScreenState::default());
        let kbd = Arc::new(KeyboardProtocolRestoreState::default());
        let mut tracker = TerminalOutputTracker::new(Arc::clone(&kbd), Arc::clone(&state));

        // ?25l (cursor hide), ?2004h (bracketed paste), ?1004h (focus events) → 무시
        tracker.observe(b"\x1b[?25l\x1b[?2004h\x1b[?1004h");
        assert!(!state.active.load(Ordering::Relaxed));
        // 그리고 alt_screen_param_matches가 비-alt 매개변수를 거부하는지 한 번 더 확인
        assert!(!alt_screen_param_matches(b"25"));
        assert!(!alt_screen_param_matches(b"2004"));
        assert!(!alt_screen_param_matches(b"1004"));
        assert!(alt_screen_param_matches(b"47"));
        assert!(alt_screen_param_matches(b"1047"));
        assert!(alt_screen_param_matches(b"1049"));
    }

    #[test]
    fn alt_screen_tracker_handles_semicolon_grouped_params() {
        let state = Arc::new(AltScreenState::default());
        let kbd = Arc::new(KeyboardProtocolRestoreState::default());
        let mut tracker = TerminalOutputTracker::new(Arc::clone(&kbd), Arc::clone(&state));

        // xterm 그룹 set: ?47;1049h → 1049 매치 → enter
        tracker.observe(b"\x1b[?47;1049h");
        assert!(state.active.load(Ordering::Relaxed));

        // 그룹 reset: ?1049;25l → 1049 매치 → exit
        tracker.observe(b"\x1b[?1049;25l");
        assert!(!state.active.load(Ordering::Relaxed));

        // 첫번째 파라미터가 무관해도 두번째가 alt-screen이면 매치
        tracker.observe(b"\x1b[?25;1049h");
        assert!(state.active.load(Ordering::Relaxed));
    }

    #[test]
    fn alt_screen_sequence_split_across_chunks_observed_once() {
        let state = Arc::new(AltScreenState::default());
        let kbd = Arc::new(KeyboardProtocolRestoreState::default());
        let mut tracker = TerminalOutputTracker::new(Arc::clone(&kbd), Arc::clone(&state));

        tracker.observe(b"prefix\x1b[?10");
        tracker.observe(b"49h");
        assert!(state.active.load(Ordering::Relaxed));
    }

    #[test]
    fn alt_screen_tracker_handles_long_grouped_params() {
        // scan_end가 i+32였을 때 매개변수가 32바이트를 넘기면 종결자 미발견으로 silently
        // drop되던 회귀를 방지한다. xterm 스펙은 ?47;1047;1049 같은 그룹 set 길이에
        // 명시적 상한이 없으므로 i+64까지 스캔한다.
        let state = Arc::new(AltScreenState::default());
        let kbd = Arc::new(KeyboardProtocolRestoreState::default());
        let mut tracker = TerminalOutputTracker::new(Arc::clone(&kbd), Arc::clone(&state));

        // ?25;47;1047;1049;1004;2004;1006h — i+3..j 사이가 32바이트 초과
        tracker.observe(b"\x1b[?25;47;1047;1049;1004;2004;1006h");
        assert!(
            state.active.load(Ordering::Relaxed),
            "long grouped alt-screen set must still toggle on"
        );

        tracker.observe(b"\x1b[?25;47;1047;1049;1004;2004;1006l");
        assert!(
            !state.active.load(Ordering::Relaxed),
            "long grouped alt-screen reset must still toggle off"
        );
    }

    #[test]
    fn alt_screen_tracker_handles_long_grouped_params_across_chunks() {
        // Quad-review LOW finding (Claude + Codex): scan-window 회귀가 boundary
        // 모드(`TerminalOutputTracker::observe`가 tail+new chunk를 합성하는 경로)에서도
        // 잡히는지 검증. tail에 `\x1b[?25;47;1047;1049;1004;2004;1` 까지 들어오고
        // 다음 청크가 `006h`로 끝나는 시나리오 — 종결자 위치가 boundary 윈도우의
        // 후반(>32바이트) 영역에 떨어지는 케이스.
        let state = Arc::new(AltScreenState::default());
        let kbd = Arc::new(KeyboardProtocolRestoreState::default());
        let mut tracker = TerminalOutputTracker::new(Arc::clone(&kbd), Arc::clone(&state));

        tracker.observe(b"\x1b[?25;47;1047;1049;1004;2004;1");
        assert!(
            !state.active.load(Ordering::Relaxed),
            "no terminator yet — must not toggle"
        );
        tracker.observe(b"006h");
        assert!(
            state.active.load(Ordering::Relaxed),
            "terminator arrived in next chunk — boundary path must catch it"
        );
    }

    #[test]
    fn alt_screen_param_matches_rejects_colon_subparameter() {
        // ECMA-48상 `:`는 sub-parameter separator라 `?47:5h`는 mode 47의 subparameter 5
        // 의미이지 mode 47과 5를 동시에 set하는 의미가 아니다. 과거 구현은 `:`도
        // split해 false-positive 매치를 만들었다.
        assert!(!alt_screen_param_matches(b"47:5"));
        assert!(!alt_screen_param_matches(b"1049:0"));
        assert!(!alt_screen_param_matches(b"5:47"));
        // semicolon은 정상적으로 분리한다
        assert!(alt_screen_param_matches(b"5;47"));
        assert!(alt_screen_param_matches(b"1049;25"));
    }

    #[test]
    fn heartbeat_idle_path_fires_only_when_clean_and_interval_elapsed() {
        // idle 경로: status_dirty=false 이고 STATUS_HEARTBEAT 경과 시 발화한다.
        // attach 루프 시작 직후 ZERO elapsed는 두 경로 모두 false여야 한다.
        assert!(!heartbeat_due(Duration::ZERO, false));
        assert!(!heartbeat_due(Duration::ZERO, true));
        assert!(!heartbeat_due(
            STATUS_HEARTBEAT - Duration::from_millis(1),
            false
        ));
        assert!(heartbeat_due(STATUS_HEARTBEAT, false));
        assert!(heartbeat_due(
            STATUS_HEARTBEAT + Duration::from_millis(50),
            false
        ));
    }

    #[test]
    fn heartbeat_dirty_blocks_idle_path_until_forced_threshold() {
        // status_dirty=true 면 STATUS_HEARTBEAT 경과만으로는 발화하지 않고,
        // STATUS_HEARTBEAT_FORCED 경과 후에 강제 발화한다. 이 경로가 없으면
        // PTY 연속 출력 중 외부 DECSTBM 리셋을 영원히 회복하지 못한다.
        assert!(!heartbeat_due(STATUS_HEARTBEAT, true));
        assert!(!heartbeat_due(
            STATUS_HEARTBEAT_FORCED - Duration::from_millis(1),
            true
        ));
        assert!(heartbeat_due(STATUS_HEARTBEAT_FORCED, true));
        assert!(heartbeat_due(
            STATUS_HEARTBEAT_FORCED + Duration::from_millis(100),
            true
        ));
    }

    #[test]
    fn heartbeat_forced_threshold_is_strictly_greater_than_idle() {
        // forced 경로가 idle보다 늦게 발화해야 PTY busy 출력 시 redraw 폭주가 없다.
        assert!(STATUS_HEARTBEAT_FORCED > STATUS_HEARTBEAT);
        assert!(
            STATUS_HEARTBEAT_FORCED >= Duration::from_secs(2),
            "busy prompt repaint should not redraw the status row at sub-second cadence"
        );
    }

    #[test]
    fn ignores_non_keyboard_csi_sequences() {
        let state = KeyboardProtocolRestoreState::default();
        observe_keyboard_protocol_sequences(b"\x1b[?25l\x1b[>4;1m\x1b[31m", &state);
        assert_eq!(state.kitty_push_depth.load(Ordering::Relaxed), 0);
        assert_eq!(state.kitty_direct_flags.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn status_theme_protocol_guard_rejects_old_daemon() {
        let old = DaemonStatus {
            version: "0.1.2".to_string(),
            protocol_version: 1,
            session_count: 0,
            active_connections: 0,
            shutting_down: false,
            // 옛 데몬은 doctor 신규 필드를 보내지 않는다. backward-compat 시뮬레이션.
            daemon_uid: None,
            started_at_unix_secs: None,
        };
        let current = DaemonStatus {
            protocol_version: super::STATUS_THEME_PROTOCOL_VERSION,
            ..old.clone()
        };

        assert!(
            status_theme_protocol_error(&old)
                .expect("old daemon should be rejected")
                .contains("does not support status themes")
        );
        assert_eq!(status_theme_protocol_error(&current), None);
    }

    #[test]
    fn status_style_env_takes_precedence_over_ssh() {
        // 다른 env-touching 테스트와 충돌하지 않도록 crate 공용 TEST_ENV_LOCK으로 직렬화한 뒤,
        // EnvGuard로 테스트가 끝나면 원래 환경 변수를 복원한다.
        let _guard = crate::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let _env_guard = EnvGuard::capture(&[
            "LTERM_STATUS_STYLE",
            "LTERM_STATUS_THEME",
            "SSH_CONNECTION",
            "SSH_CLIENT",
            "SSH_TTY",
            "TERM_PROGRAM",
            "LC_TERMINAL",
            "TERMINAL_EMULATOR",
        ]);

        // SAFETY: crate::TEST_ENV_LOCK is held; EnvGuard restores on drop.
        unsafe {
            std::env::remove_var("LTERM_STATUS_STYLE");
            std::env::remove_var("LTERM_STATUS_THEME");
            std::env::remove_var("SSH_CONNECTION");
            std::env::remove_var("SSH_CLIENT");
            std::env::remove_var("SSH_TTY");
            std::env::remove_var("TERM_PROGRAM");
            std::env::remove_var("LC_TERMINAL");
            std::env::remove_var("TERMINAL_EMULATOR");

            std::env::set_var("SSH_CONNECTION", "1.2.3.4 22 5.6.7.8 22");
            std::env::set_var("LTERM_STATUS_STYLE", "full");
        }
        assert_eq!(
            resolve_status_style(None),
            StatusStyle::Full(StatusTheme::Blue)
        );

        unsafe {
            std::env::set_var("LTERM_STATUS_STYLE", "minimal");
        }
        assert_eq!(resolve_status_style(None), StatusStyle::Minimal);

        unsafe {
            std::env::remove_var("LTERM_STATUS_STYLE");
        }
        // SSH only → Minimal
        assert_eq!(resolve_status_style(None), StatusStyle::Minimal);

        unsafe {
            std::env::remove_var("SSH_CONNECTION");
        }
        // No SSH, no style → Full
        assert_eq!(
            resolve_status_style(None),
            StatusStyle::Full(StatusTheme::Blue)
        );

        unsafe {
            std::env::set_var("TERM_PROGRAM", "Termius");
        }
        assert_eq!(
            resolve_status_style(None),
            StatusStyle::Minimal,
            "Termius-style mobile terminals should default to plain status rendering"
        );

        // EnvGuard 가 drop 되면서 원래 환경 변수 값을 복원한다.
    }

    #[test]
    fn mobile_terminal_identity_envs_prefer_minimal_status_style() {
        let _guard = crate::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let _env_guard = EnvGuard::capture(&[
            "LTERM_STATUS_STYLE",
            "LTERM_STATUS_THEME",
            "SSH_CONNECTION",
            "SSH_CLIENT",
            "SSH_TTY",
            "TERM_PROGRAM",
            "LC_TERMINAL",
            "TERMINAL_EMULATOR",
        ]);

        for (name, value) in [
            ("TERM_PROGRAM", "Termius"),
            ("LC_TERMINAL", "com.termius.ssh"),
            ("TERMINAL_EMULATOR", "termius-mobile"),
        ] {
            // SAFETY: crate::TEST_ENV_LOCK is held; EnvGuard restores on drop.
            unsafe {
                std::env::remove_var("LTERM_STATUS_STYLE");
                std::env::remove_var("LTERM_STATUS_THEME");
                std::env::remove_var("SSH_CONNECTION");
                std::env::remove_var("SSH_CLIENT");
                std::env::remove_var("SSH_TTY");
                std::env::remove_var("TERM_PROGRAM");
                std::env::remove_var("LC_TERMINAL");
                std::env::remove_var("TERMINAL_EMULATOR");
                std::env::set_var(name, value);
            }
            assert_eq!(
                resolve_status_style(None),
                StatusStyle::Minimal,
                "{name}={value} should select plain mobile-safe status rendering"
            );
        }

        // SAFETY: crate::TEST_ENV_LOCK is held; EnvGuard restores on drop.
        unsafe {
            std::env::set_var("LTERM_STATUS_THEME", "green");
        }
        assert_eq!(
            resolve_status_style(None),
            StatusStyle::Full(StatusTheme::Green),
            "explicit status themes should remain authoritative even on mobile terminals"
        );
    }

    #[test]
    fn resolve_status_theme_prefers_session_then_env() {
        let _guard = crate::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let _env_guard = EnvGuard::capture(&[
            "LTERM_STATUS_STYLE",
            "LTERM_STATUS_THEME",
            "SSH_CONNECTION",
            "SSH_CLIENT",
            "SSH_TTY",
            "TERM_PROGRAM",
            "LC_TERMINAL",
            "TERMINAL_EMULATOR",
        ]);

        // SAFETY: crate::TEST_ENV_LOCK is held; EnvGuard restores on drop.
        unsafe {
            std::env::remove_var("LTERM_STATUS_STYLE");
            std::env::remove_var("SSH_CONNECTION");
            std::env::remove_var("SSH_CLIENT");
            std::env::remove_var("SSH_TTY");
            std::env::remove_var("TERM_PROGRAM");
            std::env::remove_var("LC_TERMINAL");
            std::env::remove_var("TERMINAL_EMULATOR");
            std::env::set_var("LTERM_STATUS_THEME", "green");
        }
        assert_eq!(
            resolve_status_style(None),
            StatusStyle::Full(StatusTheme::Green)
        );
        assert_eq!(
            resolve_status_style(Some(StatusTheme::Red)),
            StatusStyle::Full(StatusTheme::Red)
        );

        unsafe {
            std::env::set_var("SSH_CONNECTION", "1.2.3.4 22 5.6.7.8 22");
        }
        assert_eq!(
            resolve_status_style(Some(StatusTheme::Cyan)),
            StatusStyle::Full(StatusTheme::Cyan),
            "explicit per-session themes should remain colored over SSH"
        );

        unsafe {
            std::env::set_var("LTERM_STATUS_STYLE", "minimal");
        }
        assert_eq!(
            resolve_status_style(Some(StatusTheme::Cyan)),
            StatusStyle::Minimal
        );
    }

    #[test]
    fn status_theme_sgr_uses_whitelisted_sequences() {
        assert_eq!(StatusStyle::Full(StatusTheme::Blue).sgr(), "\x1b[0;30;104m");
        assert_eq!(
            StatusStyle::Full(StatusTheme::Green).sgr(),
            "\x1b[0;30;102m"
        );
        assert_eq!(
            StatusStyle::Full(StatusTheme::Plain).sgr(),
            StatusStyle::Minimal.sgr()
        );
        assert_eq!(StatusTheme::parse("purple"), Some(StatusTheme::Magenta));
        assert_eq!(StatusTheme::parse("yellow"), Some(StatusTheme::Amber));
        assert_eq!(StatusTheme::parse("\x1b[31m"), None);
    }

    #[test]
    fn parse_status_style_accepts_known_values() {
        assert_eq!(
            parse_status_style("full"),
            Some(StatusStyle::Full(StatusTheme::Blue))
        );
        assert_eq!(parse_status_style("Minimal"), Some(StatusStyle::Minimal));
        assert_eq!(
            parse_status_style(" full "),
            Some(StatusStyle::Full(StatusTheme::Blue))
        );
        assert_eq!(parse_status_style("off"), None);
        assert_eq!(parse_status_style(""), None);
    }
}
