use crate::paths;
use crate::protocol::{
    CAPABILITY_PROTOCOL_VERSION, CHILD_COLOR_POLICY_ENV, CMUX_CONTEXT_ENV, CapabilityAction,
    CapabilityToken, DaemonStatus, ExitListScope, InstrumentSnapshot, IssueInputCapabilityResult,
    MAX_CAPABILITY_INPUT_BYTES, MAX_INPUT_CAPABILITY_BUDGET, MAX_RECENT_EXITS_LIMIT,
    MAX_SEND_DATA_BYTES, MetadataHistoryResult, MetadataPurgeResult, MetadataStepResult,
    PROTOCOL_VERSION, RecentSessionExit, Request, Response, SensitiveCapabilityRequest,
    SessionInfo, SessionLifecycleState, StatusTheme, WaitContainsResult, WaitExitResult,
};
use crate::sanitize;
use anyhow::{Context, Result, anyhow, bail};
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use crossterm::terminal::ClearType;
use crossterm::{cursor, execute, queue, terminal};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, ErrorKind, IsTerminal, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const MAX_RPC_RESPONSE_BYTES: u64 = 64 * 1024 * 1024;
const RPC_PARSE_ERROR_PREVIEW_BYTES: usize = 4 * 1024;
const ATTACH_RESPONSE_HEADER_LIMIT: usize = 64 * 1024;
const MAX_DAEMON_LOG_BYTES: u64 = 10 * 1024 * 1024;
const DEFAULT_TRACE_MAX_BYTES: u64 = 16 * 1024 * 1024;
const TRACE_FORMAT: &str = "lterm-trace-jsonl";
const TRACE_SCHEMA_VERSION: &str = "1.0";
const RECONNECT_STATE_SCHEMA_VERSION: u32 = 1;
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
/// 확정 손상(ED/DECSTBM reset/RIS) 감지 시 self-heal deadline. `observe()`가 status row
/// 손상을 정밀 판정한 경우(`status_area_dirty`)에만 활성화되는 fast lane이다. 이 값은 동시에
/// rate-limit 역할도 한다: 연속 출력 중 매 프레임 repaint 폭주를 막으면서도 forced 2초 백스톱
/// 대신 ~50ms 안에 색/잔상 손상을 복구한다. 50ms는 체감상 즉시이면서 폭주를 억제하는 균형값.
const STATUS_DAMAGE_HEARTBEAT: Duration = Duration::from_millis(50);
/// Attach output idle wakeup. This bounds status-bar redraw latency without the
/// previous 30ms hot poll; heartbeat logic still owns the actual redraw cadence.
const ATTACH_OUTPUT_IDLE_TIMEOUT: Duration = Duration::from_millis(100);
/// Row-off agent sessions cannot safely use the host status row, and Codex-like
/// TUIs may overwrite the one-shot terminal title during startup. Re-emit the
/// sanitized title only after the PTY has been idle for a short while so the cue
/// remains available without consuming a row or touching SGR color state.
const AGENT_TITLE_REFRESH: Duration = Duration::from_secs(2);
/// Poll live session metadata while attached so host-side status text follows
/// external `lterm rename` calls without mixing metadata frames into the raw
/// PTY stream. The poll runs on a side RPC thread; the attach output loop only
/// consumes best-effort updates from a bounded channel.
const STATUS_METADATA_POLL: Duration = Duration::from_millis(500);
const STATUS_METADATA_RPC_TIMEOUT: Duration = Duration::from_millis(250);
const STATUS_METADATA_CHANNEL_LIMIT: usize = 4;
/// status 폴링 스레드의 interval 대기를 쪼개는 청크 길이. `running`이 꺼졌을 때
/// teardown의 `join()`이 최대 이 길이만 블로킹되도록 보장한다. interval이 최대 1시간
/// (`LTERM_STATUS_INTERVAL`)까지 늘어나도 detach 시 즉시 깨어나야 하기 때문이다.
const STATUS_POLL_INTERRUPT_CHUNK: Duration = Duration::from_millis(100);
const NESTED_AGENT_POLL: Duration = Duration::from_millis(500);
const NESTED_AGENT_DETECTION_CHANNEL_LIMIT: usize = 4;
const NESTED_AGENT_STABLE_POLLS: u8 = 2;
/// Mobile transcript writes sanitized text, but the containing terminal may
/// still be in a colored SGR state left by a previous raw TUI attach. Emit a
/// narrow local reset before transcript UI text so sanitized output does not
/// inherit stale foreground/background colors.
const MOBILE_TRANSCRIPT_SGR_RESET: &str = "\x1b[0m";
/// Raw attach intentionally passes PTY bytes through unchanged, but the host
/// terminal can already be in a colored SGR state before lterm attaches (for
/// example after a mobile SSH renderer or a previous status row).  Emit only a
/// host-local SGR reset on lterm-owned UI boundaries so the first raw PTY bytes
/// do not inherit stale foreground/background colors.
const HOST_TERMINAL_SGR_RESET: &[u8] = b"\x1b[0m";
const PS_CANDIDATES: &[&str] = &["/bin/ps", "/usr/bin/ps"];
const STATUS_THEME_PROTOCOL_VERSION: u32 = 2;
const WAIT_PROTOCOL_VERSION: u32 = 3;
const INSTRUMENT_PROTOCOL_VERSION: u32 = 4;
const METADATA_PROTOCOL_VERSION: u32 = 6;
const TMUX_PARENT_PANE_PROTOCOL_VERSION: u32 = 7;
const RECENT_EXITS_PROTOCOL_VERSION: u32 = 8;
const CAPABILITY_FILE_PREFIX: &[u8] = b"lterm-input-capability-v1\n";
const MAX_CAPABILITY_FILE_BYTES: u64 = 128;
const CAPABILITY_RESPONSE_HEADER_LIMIT: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CapabilityFileIdentity {
    device: u64,
    inode: u64,
}

#[derive(Clone)]
struct ValidatedCapabilityFile {
    token: CapabilityToken,
    identity: CapabilityFileIdentity,
}

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
        .with_context(|| format!("parse response: {}", rpc_parse_error_preview(&bytes)))?;
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

fn rpc_parse_error_preview(bytes: &[u8]) -> String {
    let preview_len = bytes.len().min(RPC_PARSE_ERROR_PREVIEW_BYTES);
    let mut preview = sanitize::strip_controls(&sanitize::terminal_capture(&bytes[..preview_len]));
    if bytes.len() > preview_len {
        preview.push_str(&format!(
            "… ({} bytes omitted)",
            bytes.len().saturating_sub(preview_len)
        ));
    }
    preview
}

pub fn new_session(
    name: Option<String>,
    command: Option<String>,
    cwd: Option<String>,
    mut env: std::collections::HashMap<String, String>,
    status_theme: Option<StatusTheme>,
    tmux: bool,
    tmux_parent_pane_id: Option<String>,
) -> Result<SessionInfo> {
    ensure_server()?;
    if tmux_parent_pane_id.is_some() {
        require_tmux_parent_pane_protocol()?;
    }
    if status_theme.is_some() {
        require_status_theme_protocol()?;
    }
    let cwd = Some(resolve_client_cwd(cwd)?);
    let parent = if tmux_parent_pane_id.is_some() {
        None
    } else {
        current_parent_request()
    };
    inherit_client_session_home_env(&mut env);
    inherit_terminal_capability_env(&mut env);
    inherit_child_color_policy_env_unless_agent(&mut env);
    if tmux {
        inherit_cmux_context_env(&mut env);
    }
    rpc(&Request::New {
        name,
        command,
        cwd,
        rows: terminal_rows(),
        cols: terminal_cols(),
        parent_pane_id: parent.as_ref().map(|parent| parent.pane_id.clone()),
        parent_token: parent.map(|parent| parent.token),
        tmux_parent_pane_id,
        env,
        status_theme,
        tmux,
    })
}

fn inherit_client_session_home_env(env: &mut std::collections::HashMap<String, String>) {
    for key in CLIENT_SESSION_HOME_ENV {
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

fn inherit_child_color_policy_env_unless_agent(
    env: &mut std::collections::HashMap<String, String>,
) {
    if session_env_declares_agent(env) {
        return;
    }
    for key in CHILD_COLOR_POLICY_ENV {
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

fn session_env_declares_agent(env: &std::collections::HashMap<String, String>) -> bool {
    env.get("LTERM_AGENT")
        .is_some_and(|value| !value.trim().is_empty())
}

fn inherit_cmux_context_env(env: &mut std::collections::HashMap<String, String>) {
    for key in CMUX_CONTEXT_ENV {
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

const CLIENT_SESSION_HOME_ENV: &[&str] = &["CODEX_HOME"];

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
];

pub fn attach_or_new(target: &str) -> Result<SessionInfo> {
    ensure_server()?;
    let parent = current_parent_request();
    let mut env = std::collections::HashMap::new();
    inherit_client_session_home_env(&mut env);
    rpc(&Request::AttachOrNew {
        target: target.to_string(),
        cwd: Some(resolve_client_cwd(None)?),
        parent_pane_id: parent.as_ref().map(|parent| parent.pane_id.clone()),
        parent_token: parent.map(|parent| parent.token),
        env,
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

fn require_instrument_protocol() -> Result<()> {
    let status = daemon_status().context("check lterm daemon protocol for instrument snapshots")?;
    if let Some(message) = instrument_protocol_error(&status) {
        bail!(message);
    }
    Ok(())
}

fn require_capability_protocol() -> Result<()> {
    let status = daemon_status().context("check lterm daemon protocol for capabilities")?;
    if status.protocol_version < CAPABILITY_PROTOCOL_VERSION {
        bail!(
            "lterm daemon protocol {} does not support capabilities (requires protocol {}); run `lterm shutdown` and retry after upgrading",
            status.protocol_version,
            CAPABILITY_PROTOCOL_VERSION
        );
    }
    Ok(())
}

fn require_metadata_protocol() -> Result<()> {
    let status = daemon_status().context("check lterm daemon protocol for metadata history")?;
    if status.protocol_version < METADATA_PROTOCOL_VERSION {
        bail!(
            "lterm daemon protocol {} does not support metadata history (requires protocol {}); run `lterm shutdown` and retry after upgrading",
            status.protocol_version,
            METADATA_PROTOCOL_VERSION
        );
    }
    Ok(())
}

fn require_tmux_parent_pane_protocol() -> Result<()> {
    let status =
        daemon_status().context("check lterm daemon protocol for explicit tmux parent panes")?;
    if let Some(message) = tmux_parent_pane_protocol_error(&status) {
        bail!(message);
    }
    Ok(())
}

fn require_recent_exits_protocol() -> Result<()> {
    let status = daemon_status().context("check lterm daemon protocol for recent exits")?;
    if let Some(message) = recent_exits_protocol_error(&status) {
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

fn instrument_protocol_error(status: &DaemonStatus) -> Option<String> {
    (status.protocol_version < INSTRUMENT_PROTOCOL_VERSION).then(|| {
        format!(
            "lterm daemon protocol {} does not support instrument snapshots (requires protocol {}); run `lterm shutdown` and retry after upgrading",
            status.protocol_version, INSTRUMENT_PROTOCOL_VERSION
        )
    })
}

fn tmux_parent_pane_protocol_error(status: &DaemonStatus) -> Option<String> {
    (status.protocol_version < TMUX_PARENT_PANE_PROTOCOL_VERSION).then(|| {
        format!(
            "lterm daemon protocol {} does not support explicit tmux parent panes (requires protocol {}); run `lterm shutdown` and retry after upgrading",
            status.protocol_version, TMUX_PARENT_PANE_PROTOCOL_VERSION
        )
    })
}

fn recent_exits_protocol_error(status: &DaemonStatus) -> Option<String> {
    (status.protocol_version < RECENT_EXITS_PROTOCOL_VERSION).then(|| {
        format!(
            "lterm daemon protocol {} does not support recent exit evidence (requires protocol {}); upgrade/restart is required, and no live session was modified",
            status.protocol_version, RECENT_EXITS_PROTOCOL_VERSION
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
    let mut sessions: Vec<SessionInfo> = rpc(&Request::List)?;
    sessions.retain(SessionInfo::is_live_work);
    Ok(sessions)
}

pub fn recent_exits(
    target: Option<&str>,
    limit: u16,
    scope: ExitListScope,
) -> Result<Vec<RecentSessionExit>> {
    if limit == 0 || limit > MAX_RECENT_EXITS_LIMIT {
        bail!("recent exit limit must be between 1 and {MAX_RECENT_EXITS_LIMIT}");
    }
    ensure_server()?;
    require_recent_exits_protocol()?;
    rpc(&Request::RecentExits {
        target: target.map(str::to_string),
        limit,
        scope,
    })
}

pub fn info(target: &str) -> Result<SessionInfo> {
    ensure_server()?;
    rpc(&Request::Info {
        target: target.to_string(),
    })
}

pub fn instrument(target: &str) -> Result<InstrumentSnapshot> {
    ensure_server()?;
    require_instrument_protocol()?;
    rpc(&Request::Instrument {
        target: target.to_string(),
    })
}

pub fn metadata_history(target: &str) -> Result<MetadataHistoryResult> {
    ensure_server()?;
    require_metadata_protocol()?;
    rpc(&Request::MetadataHistory {
        target: target.to_string(),
    })
}

pub fn metadata_undo(target: &str) -> Result<MetadataStepResult> {
    ensure_server()?;
    require_metadata_protocol()?;
    rpc(&Request::MetadataUndo {
        target: target.to_string(),
    })
}

pub fn metadata_redo(target: &str) -> Result<MetadataStepResult> {
    ensure_server()?;
    require_metadata_protocol()?;
    rpc(&Request::MetadataRedo {
        target: target.to_string(),
    })
}

pub fn metadata_purge_history(
    target: &str,
    irreversible: bool,
    session_id: &str,
) -> Result<MetadataPurgeResult> {
    ensure_server()?;
    require_metadata_protocol()?;
    rpc(&Request::MetadataPurgeHistory {
        target: target.to_string(),
        irreversible,
        session_id: session_id.to_string(),
    })
}

pub fn issue_input_capability(target: &str, byte_budget: u64, path: &Path) -> Result<()> {
    ensure_server()?;
    require_capability_protocol()?;
    if !(1..=MAX_INPUT_CAPABILITY_BUDGET).contains(&byte_budget) {
        bail!("input capability byte budget must be between 1 and {MAX_INPUT_CAPABILITY_BUDGET}");
    }

    let mut file = create_private_capability_file(path)?;
    let file_identity = capability_file_identity(
        &file
            .metadata()
            .context("stat newly created capability file")?,
    );
    let issued = match issue_input_capability_rpc(target, byte_budget) {
        Ok(issued) => issued,
        Err(err) => {
            drop(file);
            let _ = unlink_capability_path_if_identity_matches(path, file_identity);
            return Err(err);
        }
    };
    let persistence = (|| -> Result<()> {
        file.write_all(CAPABILITY_FILE_PREFIX)
            .context("write capability file header")?;
        file.write_all(issued.token.as_str().as_bytes())
            .context("write capability token")?;
        file.write_all(b"\n").context("terminate capability file")?;
        file.sync_all().context("sync capability file")?;
        validate_capability_metadata(&file.metadata().context("stat capability file")?)
    })();
    if let Err(err) = persistence {
        let _ = capability_exchange(
            CapabilityAction::Revoke,
            SensitiveCapabilityRequest::Revoke {
                token: issued.token,
            },
        );
        drop(file);
        let _ = unlink_capability_path_if_identity_matches(path, file_identity);
        return Err(err).with_context(|| format!("persist capability file {}", path.display()));
    }
    Ok(())
}

pub fn input_with_capability(path: &Path, input: &mut impl Read) -> Result<()> {
    let mut data = Vec::new();
    input
        .take((MAX_CAPABILITY_INPUT_BYTES + 1) as u64)
        .read_to_end(&mut data)
        .context("read capability input from stdin")?;
    if data.is_empty() {
        bail!("capability input must not be empty");
    }
    if data.len() > MAX_CAPABILITY_INPUT_BYTES {
        bail!("capability input exceeds {MAX_CAPABILITY_INPUT_BYTES} bytes");
    }
    capability_exchange_from_file(path, CapabilityAction::Input, |token| {
        SensitiveCapabilityRequest::Input { token, data }
    })
    .map(|_| ())
}

pub fn revoke_capability(path: &Path) -> Result<()> {
    let validated = capability_exchange_from_file(path, CapabilityAction::Revoke, |token| {
        SensitiveCapabilityRequest::Revoke { token }
    })?;
    unlink_revalidated_capability_path(path, &validated)
}

fn issue_input_capability_rpc(
    target: &str,
    byte_budget: u64,
) -> Result<IssueInputCapabilityResult> {
    let path = paths::socket_path()?;
    let mut stream = UnixStream::connect(&path).with_context(|| daemon_connect_context(&path))?;
    stream.set_read_timeout(Some(RPC_TIMEOUT))?;
    stream.set_write_timeout(Some(RPC_TIMEOUT))?;
    let request = Request::IssueInputCapability {
        target: target.to_string(),
        byte_budget,
    };
    serde_json::to_writer(&mut stream, &request).context("serialize capability issue request")?;
    stream
        .write_all(b"\n")
        .context("write capability issue request")?;
    stream.shutdown(std::net::Shutdown::Write).ok();
    let mut bytes = Vec::new();
    stream
        .take(MAX_RPC_RESPONSE_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("read capability issue response")?;
    if bytes.len() as u64 > MAX_RPC_RESPONSE_BYTES {
        bail!("capability issue response exceeded limit");
    }
    let response: Response = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse capability issue response ({} bytes)", bytes.len()))?;
    if !response.ok {
        bail!(
            response
                .error
                .unwrap_or_else(|| "capability issue rejected".to_string())
        );
    }
    let value = response.result.unwrap_or(serde_json::Value::Null);
    serde_json::from_value(value).context("decode capability issue result")
}

fn capability_exchange_from_file<F>(
    path: &Path,
    action: CapabilityAction,
    build: F,
) -> Result<ValidatedCapabilityFile>
where
    F: FnOnce(CapabilityToken) -> SensitiveCapabilityRequest,
{
    ensure_server()?;
    require_capability_protocol()?;
    let socket = paths::socket_path()?;
    let mut stream =
        UnixStream::connect(&socket).with_context(|| daemon_connect_context(&socket))?;
    stream.set_read_timeout(Some(RPC_TIMEOUT))?;
    stream.set_write_timeout(Some(RPC_TIMEOUT))?;
    serde_json::to_writer(&mut stream, &Request::CapabilityChannel { action })
        .context("serialize capability channel hello")?;
    stream
        .write_all(b"\n")
        .context("write capability channel hello")?;
    stream.flush().context("flush capability channel hello")?;
    let mut reader = BufReader::new(stream);
    parse_capability_ready(read_private_response_header(&mut reader)?)?;

    // The token is read only after a protocol-v5 daemon has acknowledged the
    // nonsecret hello on this exact connection.
    let validated = read_validated_capability_file(path)?;
    let sensitive = build(validated.token.clone());
    serde_json::to_writer(reader.get_mut(), &sensitive)
        .context("serialize sensitive capability request")?;
    reader
        .get_mut()
        .write_all(b"\n")
        .context("write sensitive capability request")?;
    reader.get_mut().shutdown(std::net::Shutdown::Write).ok();
    parse_capability_response(
        read_private_response_header(&mut reader)?,
        "capability operation",
    )?;
    Ok(validated)
}

fn capability_exchange(
    action: CapabilityAction,
    sensitive: SensitiveCapabilityRequest,
) -> Result<()> {
    let socket = paths::socket_path()?;
    let mut stream =
        UnixStream::connect(&socket).with_context(|| daemon_connect_context(&socket))?;
    stream.set_read_timeout(Some(RPC_TIMEOUT))?;
    stream.set_write_timeout(Some(RPC_TIMEOUT))?;
    serde_json::to_writer(&mut stream, &Request::CapabilityChannel { action })?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    let mut reader = BufReader::new(stream);
    parse_capability_ready(read_private_response_header(&mut reader)?)?;
    serde_json::to_writer(reader.get_mut(), &sensitive)?;
    reader.get_mut().write_all(b"\n")?;
    reader.get_mut().shutdown(std::net::Shutdown::Write).ok();
    parse_capability_response(
        read_private_response_header(&mut reader)?,
        "capability operation",
    )
}

fn read_private_response_header(reader: &mut impl BufRead) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take((CAPABILITY_RESPONSE_HEADER_LIMIT + 1) as u64)
        .read_until(b'\n', &mut bytes)
        .context("read capability response")?;
    if bytes.len() > CAPABILITY_RESPONSE_HEADER_LIMIT {
        bail!("capability response exceeded limit");
    }
    if !bytes.ends_with(b"\n") {
        bail!("capability response missing newline");
    }
    Ok(bytes)
}

fn parse_capability_response(bytes: Vec<u8>, frame_type: &str) -> Result<()> {
    let response: Response = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse {frame_type} response ({} bytes)", bytes.len()))?;
    if !response.ok {
        bail!(
            response
                .error
                .unwrap_or_else(|| "capability operation rejected".to_string())
        );
    }
    Ok(())
}

fn parse_capability_ready(bytes: Vec<u8>) -> Result<()> {
    let response: Response = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse capability ready response ({} bytes)", bytes.len()))?;
    if !response.ok {
        bail!(
            response
                .error
                .unwrap_or_else(|| "capability channel rejected".to_string())
        );
    }
    let value = response.result.unwrap_or(serde_json::Value::Null);
    let ready = value.get("ready").and_then(serde_json::Value::as_bool) == Some(true);
    let protocol = value
        .get("protocol_version")
        .and_then(serde_json::Value::as_u64);
    if !ready || protocol != Some(u64::from(CAPABILITY_PROTOCOL_VERSION)) {
        bail!("daemon did not acknowledge a protocol-v5 capability channel");
    }
    Ok(())
}

fn create_private_capability_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true).mode(0o600);
    options.custom_flags(libc::O_NOFOLLOW);
    let file = options
        .open(path)
        .with_context(|| format!("create private capability file {}", path.display()))?;
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .with_context(|| format!("chmod capability file {}", path.display()))?;
    validate_capability_metadata(&file.metadata().context("stat new capability file")?)?;
    Ok(file)
}

#[cfg(test)]
fn read_private_capability_file(path: &Path) -> Result<CapabilityToken> {
    Ok(read_validated_capability_file(path)?.token)
}

fn read_validated_capability_file(path: &Path) -> Result<ValidatedCapabilityFile> {
    let mut options = OpenOptions::new();
    options.read(true).custom_flags(libc::O_NOFOLLOW);
    let file = options
        .open(path)
        .with_context(|| format!("open private capability file {}", path.display()))?;
    let metadata = file.metadata().context("stat capability file")?;
    validate_capability_metadata(&metadata)?;
    if metadata.len() == 0 || metadata.len() > MAX_CAPABILITY_FILE_BYTES {
        bail!("unsafe or malformed capability file");
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_CAPABILITY_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("read capability file")?;
    if bytes.len() as u64 != metadata.len() || bytes.len() as u64 > MAX_CAPABILITY_FILE_BYTES {
        bail!("unsafe or malformed capability file");
    }
    let Some(token_bytes) = bytes
        .strip_prefix(CAPABILITY_FILE_PREFIX)
        .and_then(|rest| rest.strip_suffix(b"\n"))
    else {
        bail!("unsafe or malformed capability file");
    };
    let token = std::str::from_utf8(token_bytes)
        .ok()
        .and_then(|value| CapabilityToken::from_canonical(value.to_string()))
        .ok_or_else(|| anyhow!("unsafe or malformed capability file"))?;
    Ok(ValidatedCapabilityFile {
        token,
        identity: capability_file_identity(&metadata),
    })
}

fn capability_file_identity(metadata: &fs::Metadata) -> CapabilityFileIdentity {
    CapabilityFileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    }
}

fn unlink_capability_path_if_identity_matches(
    path: &Path,
    expected: CapabilityFileIdentity,
) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("revalidate capability path {}", path.display()))?;
    if !metadata.file_type().is_file() || capability_file_identity(&metadata) != expected {
        bail!("capability path changed; refusing to unlink");
    }
    fs::remove_file(path).with_context(|| format!("remove capability file {}", path.display()))
}

fn unlink_revalidated_capability_path(
    path: &Path,
    expected: &ValidatedCapabilityFile,
) -> Result<()> {
    let current = read_validated_capability_file(path)?;
    if current.identity != expected.identity || current.token != expected.token {
        bail!("capability path changed; refusing to unlink");
    }
    // POSIX has no portable fd-relative compare-and-unlink primitive. Recheck
    // the leaf immediately before remove and refuse known replacements; a
    // malicious same-UID process can still race after this final check.
    unlink_capability_path_if_identity_matches(path, expected.identity)
}

fn validate_capability_metadata(metadata: &fs::Metadata) -> Result<()> {
    // SAFETY: geteuid(2) is POSIX-required thread-safe and infallible.
    let euid = unsafe { libc::geteuid() };
    if !metadata.file_type().is_file()
        || metadata.uid() != euid
        || metadata.nlink() != 1
        || metadata.mode() & 0o7777 != 0o600
    {
        bail!("unsafe capability file metadata");
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReconnectState {
    schema_version: u32,
    session_id: String,
    pane_id: String,
    session_name: String,
    recorded_at_unix_ms: u64,
}

impl ReconnectState {
    fn from_session_info(info: &SessionInfo) -> Self {
        Self {
            schema_version: RECONNECT_STATE_SCHEMA_VERSION,
            session_id: info.id.clone(),
            pane_id: info.pane_id.clone(),
            session_name: info.name.clone(),
            recorded_at_unix_ms: current_unix_ms().unwrap_or(0),
        }
    }

    fn is_usable(&self) -> bool {
        self.schema_version == RECONNECT_STATE_SCHEMA_VERSION
            && !self.session_id.is_empty()
            && !self.pane_id.is_empty()
            && !self.session_name.is_empty()
    }
}

pub fn reconnect_or_new(fallback_target: &str) -> Result<SessionInfo> {
    reconnect_target(fallback_target, true)
}

pub fn reconnect_existing_or_fallback_info(fallback_target: &str) -> Result<SessionInfo> {
    reconnect_target(fallback_target, false)
}

fn reconnect_target(fallback_target: &str, create_fallback: bool) -> Result<SessionInfo> {
    ensure_server()?;
    if let Some(state) = load_reconnect_state_best_effort() {
        if let Some(info) = resolve_reconnect_state(&state) {
            return Ok(info);
        }
    }
    let fallback = if create_fallback {
        attach_or_new(fallback_target)
    } else {
        info(fallback_target)
    }?;
    ensure_automatic_reconnect_candidate(&fallback)?;
    Ok(fallback)
}

fn resolve_reconnect_state(state: &ReconnectState) -> Option<SessionInfo> {
    for target in [&state.pane_id, &state.session_name] {
        let Ok(info) = info(target) else {
            continue;
        };
        if info.id == state.session_id && automatic_reconnect_candidate(&info) {
            return Some(info);
        }
    }
    None
}

fn automatic_reconnect_candidate(info: &SessionInfo) -> bool {
    matches!(info.lifecycle_state(), SessionLifecycleState::Healthy) && info.is_live_work()
}

fn ensure_automatic_reconnect_candidate(info: &SessionInfo) -> Result<()> {
    match info.lifecycle_state() {
        SessionLifecycleState::Healthy if info.is_live_work() => Ok(()),
        SessionLifecycleState::MonitorFailed => bail!(
            "automatic reconnect skipped session {} because leader state is unknown; use `lterm resume -- {}` for an explicit best-effort attach",
            sanitize::terminal_text(&info.id),
            sanitize::terminal_text(&info.name)
        ),
        SessionLifecycleState::Ending { trigger } => bail!(
            "automatic reconnect skipped ending session {} ({trigger}); select another live target",
            sanitize::terminal_text(&info.id)
        ),
        SessionLifecycleState::Healthy => bail!(
            "automatic reconnect skipped session {} because its lifecycle fields are inconsistent",
            sanitize::terminal_text(&info.id)
        ),
    }
}

fn remember_reconnect_target_best_effort(info: &SessionInfo) {
    let Ok(path) = paths::reconnect_state_path() else {
        return;
    };
    remember_reconnect_target_best_effort_at_path(info, &path);
}

fn remember_reconnect_target_best_effort_at_path(info: &SessionInfo, path: &Path) {
    let state = ReconnectState::from_session_info(info);
    let _ = write_reconnect_state_to_path(path, &state);
}

fn load_reconnect_state_best_effort() -> Option<ReconnectState> {
    let path = paths::reconnect_state_path().ok()?;
    read_reconnect_state_best_effort_from_path(&path)
}

fn read_reconnect_state_best_effort_from_path(path: &Path) -> Option<ReconnectState> {
    read_reconnect_state_from_path(path).ok().flatten()
}

fn read_reconnect_state_from_path(path: &Path) -> Result<Option<ReconnectState>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
    };
    let state: ReconnectState =
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
    Ok(state.is_usable().then_some(state))
}

fn write_reconnect_state_to_path(path: &Path, state: &ReconnectState) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("reconnect state path has no parent: {}", path.display()))?;
    let tmp = parent.join(format!(".reconnect-state.{}.tmp", std::process::id()));
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true).mode(0o600);
    options.custom_flags(libc::O_NOFOLLOW);
    let mut file = options
        .open(&tmp)
        .with_context(|| format!("open temporary reconnect state {}", tmp.display()))?;
    let mut permissions = file
        .metadata()
        .with_context(|| format!("stat temporary reconnect state {}", tmp.display()))?
        .permissions();
    permissions.set_mode(0o600);
    file.set_permissions(permissions)
        .with_context(|| format!("chmod temporary reconnect state {}", tmp.display()))?;
    serde_json::to_writer_pretty(&mut file, state).context("serialize reconnect state")?;
    file.write_all(b"\n")
        .context("terminate reconnect state JSON")?;
    file.sync_all()
        .with_context(|| format!("sync temporary reconnect state {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| {
        format!(
            "replace reconnect state {} with {}",
            path.display(),
            tmp.display()
        )
    })?;
    Ok(())
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

const MAX_EXTRACTED_URLS: usize = 256;
const MAX_EXTRACTED_URL_BYTES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlExtraction {
    pub urls: Vec<String>,
    pub last: Option<String>,
}

pub fn capture_urls(target: &str, tail: usize) -> Result<UrlExtraction> {
    let tail_start = compose_tail_start(tail)?;
    let capture = capture_range(target, Some(tail_start), None)?;
    Ok(extract_urls(&capture))
}

pub fn capture_search_matches(target: &str, tail: usize, query: &str) -> Result<Vec<String>> {
    if query.is_empty() {
        bail!("search query cannot be empty");
    }
    let tail_start = compose_tail_start(tail)?;
    let capture = capture_range(target, Some(tail_start), None)?;
    Ok(extract_search_matches(&capture, query))
}

pub fn extract_search_matches(text: &str, query: &str) -> Vec<String> {
    if query.is_empty() {
        return Vec::new();
    }
    let sanitized = sanitize::terminal_capture(text.as_bytes());
    sanitized
        .lines()
        .filter(|line| line.contains(query))
        .map(sanitize::terminal_text)
        .collect()
}

pub fn write_numbered_search_matches(matches: &[String], stdout: &mut impl Write) -> Result<()> {
    for (index, line) in matches.iter().enumerate() {
        writeln!(stdout, "{}\t{}", index + 1, sanitize::terminal_text(line))
            .context("write search match")?;
    }
    Ok(())
}

pub fn extract_urls(text: &str) -> UrlExtraction {
    let mut urls = Vec::new();
    let mut seen = HashSet::new();
    let mut last = None;
    let mut offset = 0;

    while let Some(start) = find_next_url_scheme(text, offset) {
        let mut end = text.len();
        for (relative, ch) in text[start..].char_indices() {
            if relative == 0 {
                continue;
            }
            if is_url_terminator(ch) {
                end = start + relative;
                break;
            }
        }

        let raw_candidate = &text[start..end];
        if raw_candidate.len() > MAX_EXTRACTED_URL_BYTES {
            offset = if end > start { end } else { start + 1 };
            continue;
        }
        let candidate = trim_url_candidate(raw_candidate);
        if url_is_extractable(candidate) {
            let url = candidate.to_string();
            last = Some(url.clone());
            if urls.len() < MAX_EXTRACTED_URLS && seen.insert(url.clone()) {
                urls.push(url);
            }
        }
        offset = if end > start { end } else { start + 1 };
    }

    UrlExtraction { urls, last }
}

pub fn write_numbered_urls(urls: &[String], stdout: &mut impl Write) -> Result<()> {
    for (index, url) in urls.iter().enumerate() {
        writeln!(stdout, "{}\t{}", index + 1, sanitize::terminal_text(url))
            .context("write extracted url")?;
    }
    Ok(())
}

fn find_next_url_scheme(text: &str, offset: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut index = offset.min(bytes.len());
    while index < bytes.len() {
        if ascii_starts_with_ignore_case(bytes, index, b"http://")
            || ascii_starts_with_ignore_case(bytes, index, b"https://")
        {
            return Some(index);
        }
        index += 1;
    }
    None
}

fn ascii_starts_with_ignore_case(value: &[u8], offset: usize, needle: &[u8]) -> bool {
    value
        .get(offset..offset.saturating_add(needle.len()))
        .is_some_and(|candidate| candidate.eq_ignore_ascii_case(needle))
}

fn is_url_terminator(ch: char) -> bool {
    ch.is_control() || ch.is_whitespace() || matches!(ch, '<' | '>' | '"' | '\'' | '`')
}

fn trim_url_candidate(candidate: &str) -> &str {
    let mut end = candidate.len();
    let mut delimiters = DelimiterCounts::scan(candidate);
    while end > 0 {
        let current = &candidate[..end];
        let Some(ch) = current.chars().next_back() else {
            break;
        };
        let trim =
            matches!(ch, '.' | ',' | ';' | ':' | '!' | '?') || delimiters.unmatched_closing(ch);
        if !trim {
            break;
        }
        delimiters.remove(ch);
        end -= ch.len_utf8();
    }
    &candidate[..end]
}

#[derive(Debug, Default)]
struct DelimiterCounts {
    open_parens: usize,
    close_parens: usize,
    open_brackets: usize,
    close_brackets: usize,
    open_braces: usize,
    close_braces: usize,
}

impl DelimiterCounts {
    fn scan(value: &str) -> Self {
        let mut counts = Self::default();
        for ch in value.chars() {
            match ch {
                '(' => counts.open_parens += 1,
                ')' => counts.close_parens += 1,
                '[' => counts.open_brackets += 1,
                ']' => counts.close_brackets += 1,
                '{' => counts.open_braces += 1,
                '}' => counts.close_braces += 1,
                _ => {}
            }
        }
        counts
    }

    fn unmatched_closing(&self, ch: char) -> bool {
        match ch {
            ')' => self.close_parens > self.open_parens,
            ']' => self.close_brackets > self.open_brackets,
            '}' => self.close_braces > self.open_braces,
            _ => false,
        }
    }

    fn remove(&mut self, ch: char) {
        match ch {
            '(' => self.open_parens = self.open_parens.saturating_sub(1),
            ')' => self.close_parens = self.close_parens.saturating_sub(1),
            '[' => self.open_brackets = self.open_brackets.saturating_sub(1),
            ']' => self.close_brackets = self.close_brackets.saturating_sub(1),
            '{' => self.open_braces = self.open_braces.saturating_sub(1),
            '}' => self.close_braces = self.close_braces.saturating_sub(1),
            _ => {}
        }
    }
}

fn url_is_extractable(url: &str) -> bool {
    url.is_ascii() && url.len() <= MAX_EXTRACTED_URL_BYTES && url_has_scheme_body(url)
}

fn url_has_scheme_body(url: &str) -> bool {
    let bytes = url.as_bytes();
    let scheme_len = if ascii_starts_with_ignore_case(bytes, 0, b"http://") {
        "http://".len()
    } else if ascii_starts_with_ignore_case(bytes, 0, b"https://") {
        "https://".len()
    } else {
        return false;
    };
    bytes.len() > scheme_len
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
    print!("{}", trace_summary_text(summary));
}

fn trace_summary_text(summary: &TraceFileSummary) -> String {
    let mut out = String::new();
    push_trace_summary_string(&mut out, "path", Some(&summary.path));
    push_trace_summary_string(&mut out, "format", summary.format.as_deref());
    push_trace_summary_string(
        &mut out,
        "schema_version",
        summary.schema_version.as_deref(),
    );
    push_trace_summary_string(&mut out, "trace_id", summary.trace_id.as_deref());
    push_trace_summary_string(&mut out, "producer", summary.producer.as_deref());
    push_trace_summary_string(
        &mut out,
        "client_version",
        summary.client_version.as_deref(),
    );
    push_trace_summary_u64(
        &mut out,
        "client_protocol_version",
        summary.client_protocol_version,
    );
    push_trace_summary_string(&mut out, "target", summary.target.as_deref());
    push_trace_summary_u64(&mut out, "created_at_unix_ms", summary.created_at_unix_ms);
    push_trace_summary_u64(&mut out, "duration_ms", summary.duration_ms);
    push_trace_summary_u64(&mut out, "max_bytes", summary.max_bytes);
    push_trace_summary_u64(&mut out, "rows", summary.rows);
    push_trace_summary_u64(&mut out, "cols", summary.cols);
    push_trace_summary_string(
        &mut out,
        "raw_stream_policy",
        summary.raw_stream_policy.as_deref(),
    );
    push_trace_summary_u64(&mut out, "event_count", Some(summary.event_count));
    push_trace_summary_u64(&mut out, "output_chunks", Some(summary.output_chunks));
    push_trace_summary_u64(&mut out, "output_bytes", Some(summary.output_bytes));
    push_trace_summary_u64(
        &mut out,
        "first_output_elapsed_ms",
        summary.first_output_elapsed_ms,
    );
    push_trace_summary_u64(
        &mut out,
        "last_output_elapsed_ms",
        summary.last_output_elapsed_ms,
    );
    push_trace_summary_u64(&mut out, "end_elapsed_ms", summary.end_elapsed_ms);
    push_trace_summary_string(&mut out, "end_reason", summary.end_reason.as_deref());
    push_trace_summary_u64(&mut out, "end_bytes_recorded", summary.end_bytes_recorded);
    push_trace_summary_u64(&mut out, "end_chunks_recorded", summary.end_chunks_recorded);
    push_trace_summary_u64(&mut out, "unknown_events", Some(summary.unknown_events));
    out
}

fn push_trace_summary_string(out: &mut String, key: &str, value: Option<&str>) {
    out.push_str(key);
    out.push('\t');
    out.push_str(&sanitize::terminal_text(value.unwrap_or("unknown")));
    out.push('\n');
}

fn push_trace_summary_u64(out: &mut String, key: &str, value: Option<u64>) {
    out.push_str(key);
    out.push('\t');
    match value {
        Some(value) => out.push_str(&value.to_string()),
        None => out.push_str("unknown"),
    }
    out.push('\n');
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

/// Raw-attach row-presence policy. This is intentionally separate from
/// `AttachMode`: attach mode chooses the transport surface (raw vs mobile
/// transcript), while this policy only controls whether the raw attach path may
/// reserve a host-side status row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusPresencePolicy {
    /// Start with the row on, but allow best-effort suspension when a nested
    /// known agent is detected. Used by ordinary shell sessions and custom
    /// agent profiles.
    RowAuto,
    /// Keep the raw attach surface full-height. Used by built-in agent profiles
    /// and explicit `--no-status`.
    RowOff,
    /// Request the row even for a direct agent launcher. The global
    /// env/TTY/geometry gate still wins for safety.
    ForceRow,
}

impl StatusPresencePolicy {
    fn requests_row(self) -> bool {
        matches!(self, Self::RowAuto | Self::ForceRow)
    }

    fn allows_nested_suspend(self) -> bool {
        matches!(self, Self::RowAuto)
    }

    pub fn from_legacy_show_status(show_status: bool) -> Self {
        if show_status {
            Self::RowAuto
        } else {
            Self::RowOff
        }
    }
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
    let parts = shlex::split(command).unwrap_or_else(|| {
        command
            .split_whitespace()
            .map(ToString::to_string)
            .collect()
    });
    let Some(executable_index) = effective_command_executable_index(&parts) else {
        return false;
    };
    let executable = &parts[executable_index];
    if known_agent_command_executable_token(executable) {
        return true;
    }
    known_agent_wrapper_command_contains_agent_payload(&parts, executable_index)
}

fn effective_command_executable_index(parts: &[String]) -> Option<usize> {
    let mut index = 0;
    while index < parts.len() {
        while parts
            .get(index)
            .is_some_and(|token| env_assignment_token(token))
        {
            index += 1;
        }

        let token = parts.get(index)?;
        if command_token_stem(token) != "env" {
            return Some(index);
        }

        index += 1;
        while let Some(token) = parts.get(index) {
            if token == "--" {
                index += 1;
                break;
            }
            if env_assignment_token(token) {
                index += 1;
                continue;
            }
            if token.starts_with('-') {
                let option_takes_value =
                    matches!(token.as_str(), "-u" | "--unset" | "-C" | "--chdir" | "-S");
                index += 1;
                if option_takes_value {
                    index += 1;
                }
                continue;
            }
            break;
        }
    }
    None
}

fn env_assignment_token(token: &str) -> bool {
    !token.starts_with('-') && token.contains('=')
}

fn known_agent_command_executable_token(token: &str) -> bool {
    if token.starts_with('-') || token.contains('=') {
        return false;
    }
    known_agent_name(command_token_stem(token))
}

fn known_agent_wrapper_command_contains_agent_payload(
    parts: &[String],
    executable_index: usize,
) -> bool {
    let executable = command_token_stem(&parts[executable_index]);
    let mut payload_start = executable_index + 1;
    match executable {
        "node" | "deno" => {}
        "npx" | "bunx" => {
            payload_start = skip_wrapper_options(parts, payload_start);
        }
        "npm" => {
            let Some(after_subcommand) =
                package_manager_execution_payload_start(parts, payload_start, &["exec", "x"])
            else {
                return false;
            };
            payload_start = after_subcommand;
        }
        "pnpm" | "yarn" => {
            let Some(after_subcommand) =
                package_manager_execution_payload_start(parts, payload_start, &["dlx", "exec"])
            else {
                return false;
            };
            payload_start = after_subcommand;
        }
        "bun" => {
            let Some(after_subcommand) =
                package_manager_execution_payload_start(parts, payload_start, &["x", "dlx"])
            else {
                return false;
            };
            payload_start = after_subcommand;
        }
        _ => return false,
    }

    parts
        .iter()
        .skip(payload_start)
        .any(|part| known_agent_command_script_token(part))
}

fn package_manager_execution_payload_start(
    parts: &[String],
    mut index: usize,
    execution_subcommands: &[&str],
) -> Option<usize> {
    index = skip_wrapper_options(parts, index);
    let subcommand = parts.get(index)?;
    if execution_subcommands.contains(&subcommand.as_str()) {
        Some(skip_wrapper_options(parts, index + 1))
    } else {
        None
    }
}

fn skip_wrapper_options(parts: &[String], mut index: usize) -> usize {
    while let Some(token) = parts.get(index) {
        if token == "--" {
            return index + 1;
        }
        if !token.starts_with('-') {
            return index;
        }
        index += 1;
    }
    index
}

fn known_agent_command_script_token(token: &str) -> bool {
    if token.starts_with('-') || token.contains('=') {
        return false;
    }
    let stem = command_token_stem(token);
    if known_agent_name(stem) {
        return true;
    }
    let lower = token.to_ascii_lowercase();
    lower.contains("claude-code") || lower.contains("@openai/codex")
}

fn command_token_stem(token: &str) -> &str {
    let basename = token.rsplit('/').next().unwrap_or(token);
    basename
        .strip_suffix(".js")
        .or_else(|| basename.strip_suffix(".mjs"))
        .or_else(|| basename.strip_suffix(".cjs"))
        .or_else(|| basename.strip_suffix(".sh"))
        .unwrap_or(basename)
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

/// 세션 command line에서 **알려진 에이전트 이름 문자열**(예: "codex"/"claude"/"gemini")을
/// 추출한다. `known_agent_name_from_command`는 bool만 돌려주므로, command-backed status
/// payload의 `agent` 필드를 채우려면 이름 자체가 필요하다. 토큰 파싱 로직
/// (`effective_command_executable_index`/`command_token_stem`/`known_agent_name`)을
/// 재활용해 중복을 최소화한다.
///
/// 파라미터:
/// - `command`: 세션의 원시 command line(`SessionInfo::command`).
///
/// 반환값: 인식된 에이전트 이름(소문자 stem). 알 수 없으면 `None`.
///
/// 주의: shell 미경유 직접 추출이며, 실행과 무관한 순수 파싱이라 안전하다.
fn agent_name_from_command(command: &str) -> Option<String> {
    let parts = shlex::split(command).unwrap_or_else(|| {
        command
            .split_whitespace()
            .map(ToString::to_string)
            .collect()
    });
    let executable_index = effective_command_executable_index(&parts)?;
    let executable = &parts[executable_index];
    // 직접 실행 형태(`codex ...`, `/usr/bin/claude ...`)면 executable stem이 곧 이름.
    if known_agent_command_executable_token(executable) {
        return Some(command_token_stem(executable).to_string());
    }
    // node/npx/npm/pnpm/yarn/bun 등 wrapper 형태면 payload 토큰에서 이름을 찾는다.
    agent_name_from_wrapper_command(&parts, executable_index)
}

/// `known_agent_wrapper_command_contains_agent_payload`의 이름-추출 변형.
/// wrapper(node/npx/npm/…) payload 토큰 중 알려진 에이전트 stem을 가진 첫 토큰의
/// 이름을 반환한다. 미상이면 `None`.
fn agent_name_from_wrapper_command(parts: &[String], executable_index: usize) -> Option<String> {
    let executable = command_token_stem(&parts[executable_index]);
    let mut payload_start = executable_index + 1;
    match executable {
        "node" | "deno" => {}
        "npx" | "bunx" => {
            payload_start = skip_wrapper_options(parts, payload_start);
        }
        "npm" => {
            payload_start =
                package_manager_execution_payload_start(parts, payload_start, &["exec", "x"])?;
        }
        "pnpm" | "yarn" => {
            payload_start =
                package_manager_execution_payload_start(parts, payload_start, &["dlx", "exec"])?;
        }
        "bun" => {
            payload_start =
                package_manager_execution_payload_start(parts, payload_start, &["x", "dlx"])?;
        }
        _ => return None,
    }

    parts
        .iter()
        .skip(payload_start)
        .find(|part| known_agent_command_script_token(part))
        .map(|part| agent_name_from_script_token(part))
}

/// wrapper payload script 토큰에서 표시용 에이전트 이름을 정한다. stem이 알려진
/// 에이전트면 그 stem을, 아니면(예: `@openai/codex`, `...claude-code...`) 포함된
/// 표준 이름을 매핑한다.
fn agent_name_from_script_token(token: &str) -> String {
    let stem = command_token_stem(token);
    if known_agent_name(stem) {
        return stem.to_string();
    }
    let lower = token.to_ascii_lowercase();
    if lower.contains("@openai/codex") {
        return "codex".to_string();
    }
    if lower.contains("claude-code") {
        return "claude".to_string();
    }
    stem.to_string()
}

fn nested_known_agent_present(target: &str) -> Result<bool> {
    let processes = process_tree(Some(target), true)?;
    Ok(nested_known_agent_present_in_processes(&processes))
}

fn nested_known_agent_present_in_processes(processes: &[ProcessInfo]) -> bool {
    processes
        .iter()
        .any(|process| known_agent_name_from_command(&process.command))
}

pub fn attach_info_with_policy(
    info: &SessionInfo,
    presence_policy: StatusPresencePolicy,
    stdin_eof: AttachStdinEof,
    options: AttachPolicyOptions,
    explicit_no_status: bool,
) -> Result<()> {
    match info.lifecycle_state() {
        SessionLifecycleState::Healthy if info.is_live_work() => {}
        SessionLifecycleState::MonitorFailed if info.is_live_work() => eprintln!(
            "warning: session {} leader state is unknown; attempting an explicit best-effort attach",
            sanitize::terminal_text(&info.id)
        ),
        SessionLifecycleState::Ending { trigger } => bail!(
            "session {} is ending ({trigger}) and is not reconnectable; inspect `lterm exits -- {}`",
            sanitize::terminal_text(&info.id),
            sanitize::terminal_text(&info.id)
        ),
        _ => bail!(
            "session {} has inconsistent lifecycle fields and is not safe to attach",
            sanitize::terminal_text(&info.id)
        ),
    }
    remember_reconnect_target_best_effort(info);
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
        let agent_presence_cue = agent_presence_cue_for_info(info, presence_policy);
        if let Some(cue) = agent_presence_cue.as_ref() {
            cue.emit_initial(&mut std::io::stdout())?;
        }
        attach_with_presence_and_cue(
            &info.pane_id,
            presence_policy,
            stdin_eof,
            info,
            agent_presence_cue,
            explicit_no_status,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AgentPresenceCue {
    session: String,
    pane: String,
    agent: String,
}

impl AgentPresenceCue {
    fn emit_initial(&self, stdout: &mut impl Write) -> Result<()> {
        // Callers only construct this cue after `stdout.is_terminal()` succeeds;
        // the reset is host-local terminal state and must not be written to pipes.
        reset_host_terminal_sgr(stdout).context("reset host terminal style before agent cue")?;
        self.write_title(stdout)?;
        if agent_presence_banner_enabled() {
            self.write_banner(stdout)?;
        }
        stdout.flush().context("flush lterm agent presence cue")?;
        Ok(())
    }

    fn refresh_title(&self, stdout: &mut impl Write) -> Result<()> {
        self.write_title(stdout)?;
        stdout
            .flush()
            .context("flush lterm terminal title refresh")?;
        Ok(())
    }

    fn write_title(&self, stdout: &mut impl Write) -> Result<()> {
        write_lterm_title_cue(stdout, &self.session, &self.pane, &self.agent)
    }

    fn write_banner(&self, stdout: &mut impl Write) -> Result<()> {
        write_lterm_agent_presence_banner(stdout, &self.session, &self.pane, &self.agent)
    }
}

fn agent_presence_cue_for_info(
    info: &SessionInfo,
    presence_policy: StatusPresencePolicy,
) -> Option<AgentPresenceCue> {
    if presence_policy.requests_row()
        || !likely_agent_session(info)
        || !agent_presence_cue_enabled()
        || !std::io::stdout().is_terminal()
    {
        return None;
    }
    let session = sanitize::terminal_text(&info.name);
    let pane = sanitize::terminal_text(&info.pane_id);
    let agent = info
        .agent_name
        .as_deref()
        .map(sanitize::terminal_text)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "agent".to_string());
    Some(AgentPresenceCue {
        session,
        pane,
        agent,
    })
}

fn agent_presence_cue_enabled() -> bool {
    !env_flag_disabled("LTERM_AGENT_CUE")
}

fn reset_host_terminal_sgr(stdout: &mut impl Write) -> Result<()> {
    stdout
        .write_all(HOST_TERMINAL_SGR_RESET)
        .context("write host terminal SGR reset")
}

fn write_lterm_title_cue(
    stdout: &mut impl Write,
    session: &str,
    pane: &str,
    agent: &str,
) -> Result<()> {
    let session = sanitize::terminal_text(session);
    let pane = sanitize::terminal_text(pane);
    let agent = sanitize::terminal_text(agent);
    let title = format!("lt:{session}:{pane} · {agent}");
    write!(stdout, "\x1b]0;{title}\x07").context("emit lterm terminal title cue")
}

fn agent_presence_banner_enabled() -> bool {
    !env_flag_disabled("LTERM_AGENT_BANNER")
}

fn write_lterm_agent_presence_banner(
    stdout: &mut impl Write,
    session: &str,
    pane: &str,
    agent: &str,
) -> Result<()> {
    let session = sanitize::terminal_text(session);
    let pane = sanitize::terminal_text(pane);
    let agent = sanitize::terminal_text(agent);
    write!(
        stdout,
        "\r[lterm] {session} {pane} · {agent} (status row hidden for agent TUI; use --status to show it)\r\n"
    )
    .context("emit lterm agent presence banner")
}

struct AgentTitleCueRuntime {
    cue: AgentPresenceCue,
    last_refresh: Instant,
    last_pty_output: Option<Instant>,
}

impl AgentTitleCueRuntime {
    fn new(cue: AgentPresenceCue) -> Self {
        Self {
            cue,
            last_refresh: Instant::now(),
            last_pty_output: None,
        }
    }

    fn observe_pty_output(&mut self) {
        self.last_pty_output = Some(Instant::now());
    }

    fn refresh_due(&self) -> bool {
        self.last_pty_output.is_some_and(|last_pty_output| {
            last_pty_output.elapsed() >= AGENT_TITLE_REFRESH
                && self.last_refresh.elapsed() >= AGENT_TITLE_REFRESH
        })
    }

    fn refresh_title(&mut self, stdout: &mut impl Write) -> Result<()> {
        self.cue.refresh_title(stdout)?;
        self.last_refresh = Instant::now();
        self.last_pty_output = None;
        Ok(())
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
            "{MOBILE_TRANSCRIPT_SGR_RESET}lterm mobile transcript · target={} · pane={} · raw attach: {}",
            sanitize::terminal_text(&info.name),
            sanitize::terminal_text(&info.pane_id),
            raw_attach_command_hint(&info.name)?
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
    write_mobile_transcript_prompt(&mut stdout)?;
    loop {
        match input_rx.recv_timeout(refresh) {
            Ok(Ok(Some(input))) => {
                let input = trim_line_endings(&input);
                if !handle_mobile_transcript_input(
                    input,
                    MobileTranscriptInputContext {
                        target,
                        tail_start,
                        append_enter: options.append_enter,
                    },
                    &mut last_capture,
                    &mut stdout,
                    capture_range,
                    send,
                    raw_attach_command_hint,
                )? {
                    return Ok(());
                }
                writeln!(stdout).context("separate mobile prompt")?;
                write_mobile_transcript_prompt(&mut stdout)?;
            }
            Ok(Ok(None)) => return Ok(()),
            Ok(Err(err)) => bail!("read mobile transcript input: {err}"),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let capture = capture_range(target, Some(tail_start), None)?;
                if mobile_transcript_capture_changed(&last_capture, &capture) {
                    writeln!(stdout).context("separate mobile prompt from transcript update")?;
                    write_mobile_transcript_update(&mut last_capture, &capture, &mut stdout)?;
                    writeln!(stdout).context("separate mobile prompt")?;
                    write_mobile_transcript_prompt(&mut stdout)?;
                }
                if !info(target)?.alive {
                    return Ok(());
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        }
    }
}

#[derive(Clone, Copy)]
struct MobileTranscriptInputContext<'a> {
    target: &'a str,
    tail_start: i32,
    append_enter: bool,
}

fn handle_mobile_transcript_input<C, S, R, W>(
    input: &str,
    context: MobileTranscriptInputContext<'_>,
    last_capture: &mut String,
    stdout: &mut W,
    mut capture: C,
    mut send_input: S,
    mut raw_hint: R,
) -> Result<bool>
where
    C: FnMut(&str, Option<i32>, Option<i32>) -> Result<String>,
    S: FnMut(&str, Vec<u8>) -> Result<()>,
    R: FnMut(&str) -> Result<String>,
    W: Write,
{
    if let Some(query) = mobile_transcript_grep_query(input) {
        if query.is_empty() {
            writeln!(stdout, "{MOBILE_TRANSCRIPT_SGR_RESET}Usage: /grep QUERY")
                .context("write mobile transcript grep usage")?;
            return Ok(true);
        }
        let capture = capture(context.target, Some(context.tail_start), None)?;
        write_mobile_transcript_search(&capture, query, stdout)?;
        return Ok(true);
    }

    match input {
        "/exit" | "/quit" => Ok(false),
        "/refresh" => {
            let capture = capture(context.target, Some(context.tail_start), None)?;
            last_capture.clear();
            write_mobile_transcript_update(last_capture, &capture, stdout)?;
            Ok(true)
        }
        "/raw" => {
            writeln!(
                stdout,
                "{MOBILE_TRANSCRIPT_SGR_RESET}raw attach: {}",
                raw_hint(context.target)?
            )
            .context("write raw attach hint")?;
            Ok(true)
        }
        "/links" | "/urls" => {
            let capture = capture(context.target, Some(context.tail_start), None)?;
            write_mobile_transcript_urls(&capture, stdout)?;
            Ok(true)
        }
        _ => {
            send_input(
                context.target,
                compose_commit_bytes(input, context.append_enter),
            )?;
            let capture = capture(context.target, Some(context.tail_start), None)?;
            write_mobile_transcript_update(last_capture, &capture, stdout)?;
            Ok(true)
        }
    }
}

fn mobile_transcript_grep_query(input: &str) -> Option<&str> {
    if input == "/grep" {
        return Some("");
    }
    let rest = input.strip_prefix("/grep")?;
    if rest
        .chars()
        .next()
        .is_some_and(|ch| ch.is_ascii_whitespace())
    {
        Some(rest.trim_start_matches(|ch: char| ch.is_ascii_whitespace()))
    } else {
        None
    }
}

fn write_mobile_transcript_prompt(stdout: &mut impl Write) -> Result<()> {
    write!(stdout, "{MOBILE_TRANSCRIPT_SGR_RESET}> ").context("write mobile prompt")?;
    stdout.flush().context("flush mobile prompt")?;
    Ok(())
}

fn write_mobile_transcript_urls(capture: &str, stdout: &mut impl Write) -> Result<()> {
    let extraction = extract_urls(capture);
    if extraction.urls.is_empty() {
        writeln!(
            stdout,
            "{MOBILE_TRANSCRIPT_SGR_RESET}No URLs found in current transcript."
        )
        .context("write empty mobile transcript urls message")?;
        return Ok(());
    }
    write_numbered_urls(&extraction.urls, stdout)
}

fn write_mobile_transcript_search(
    capture: &str,
    query: &str,
    stdout: &mut impl Write,
) -> Result<()> {
    let matches = extract_search_matches(capture, query);
    if matches.is_empty() {
        writeln!(
            stdout,
            "{MOBILE_TRANSCRIPT_SGR_RESET}No matches found in current transcript."
        )
        .context("write empty mobile transcript search message")?;
        return Ok(());
    }
    write_numbered_search_matches(&matches, stdout)
}

fn mobile_transcript_capture_changed(previous: &str, next: &str) -> bool {
    sanitize::terminal_capture(next.as_bytes()) != previous
}

fn trim_line_endings(value: &str) -> &str {
    value.trim_end_matches(['\r', '\n'])
}

fn raw_attach_command_hint(target: &str) -> Result<String> {
    shell_join(&[
        "lterm".to_string(),
        "attach".to_string(),
        "--raw".to_string(),
        "--".to_string(),
        sanitize::terminal_text(target),
    ])
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
    write!(stdout, "{MOBILE_TRANSCRIPT_SGR_RESET}").context("reset mobile transcript style")?;
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
    Ok(build_process_tree_from_rows(
        sessions,
        processes,
        include_orphans,
    ))
}

fn build_process_tree_from_rows(
    sessions: Vec<SessionInfo>,
    processes: Vec<ProcessRow>,
    include_orphans: bool,
) -> Vec<ProcessInfo> {
    let mut by_parent: std::collections::HashMap<u32, Vec<ProcessRow>> =
        std::collections::HashMap::new();
    let mut by_pid = std::collections::HashMap::new();
    for process in processes {
        by_pid.insert(process.pid, process.clone());
        by_parent.entry(process.ppid).or_default().push(process);
    }
    for children in by_parent.values_mut() {
        // Keep process reports deterministic across ps and HashMap iteration order.
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
    builder.into_processes()
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
        // Same-pgid escapees are collected from a HashMap; sort for stable reports.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusPresenceState {
    Disabled,
    Active,
    Suspended,
    Transitioning,
}

#[derive(Debug)]
struct StatusPresenceRuntime {
    state: StatusPresenceState,
}

#[derive(Clone, Debug)]
struct StatusPresenceRuntimeHandle {
    inner: Arc<Mutex<StatusPresenceRuntime>>,
}

impl StatusPresenceRuntimeHandle {
    fn new(active: bool) -> Self {
        let state = if active {
            StatusPresenceState::Active
        } else {
            StatusPresenceState::Disabled
        };
        Self {
            inner: Arc::new(Mutex::new(StatusPresenceRuntime { state })),
        }
    }

    fn is_active(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .state
            == StatusPresenceState::Active
    }

    fn can_draw_status(&self) -> bool {
        self.is_active()
    }

    fn pty_rows_for(&self, rows: u16) -> u16 {
        let guard = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        attach_pty_rows(rows, guard.state == StatusPresenceState::Active)
    }

    fn status_scroll_bottom_for(&self, rows: u16) -> Option<u16> {
        let guard = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        status_scroll_bottom_for_terminal_rows(rows, guard.state == StatusPresenceState::Active)
    }

    fn with_locked<R>(&self, f: impl FnOnce(&mut StatusPresenceRuntime) -> R) -> R {
        let mut guard = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        f(&mut guard)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NestedAgentTransition {
    Suspend,
    Resume,
}

type NestedAgentPresence = std::result::Result<bool, String>;

struct NestedAgentDetector {
    positive_polls: u8,
    negative_polls: u8,
    suppressed: bool,
}

impl NestedAgentDetector {
    fn new() -> Self {
        Self {
            positive_polls: 0,
            negative_polls: 0,
            suppressed: false,
        }
    }

    fn apply_presence_poll(
        &mut self,
        present: NestedAgentPresence,
    ) -> Option<NestedAgentTransition> {
        let present = match present {
            Ok(present) => present,
            Err(_) => {
                self.positive_polls = 0;
                if self.suppressed {
                    self.negative_polls = self.negative_polls.saturating_add(1);
                    if self.negative_polls >= NESTED_AGENT_STABLE_POLLS {
                        self.suppressed = false;
                        return Some(NestedAgentTransition::Resume);
                    }
                }
                return None;
            }
        };
        if present {
            self.positive_polls = self.positive_polls.saturating_add(1);
            self.negative_polls = 0;
            if !self.suppressed && self.positive_polls >= NESTED_AGENT_STABLE_POLLS {
                self.suppressed = true;
                return Some(NestedAgentTransition::Suspend);
            }
        } else {
            self.negative_polls = self.negative_polls.saturating_add(1);
            self.positive_polls = 0;
            if self.suppressed && self.negative_polls >= NESTED_AGENT_STABLE_POLLS {
                self.suppressed = false;
                return Some(NestedAgentTransition::Resume);
            }
        }
        None
    }

    fn retry_transition(&mut self, transition: NestedAgentTransition) {
        match transition {
            NestedAgentTransition::Suspend => {
                self.suppressed = false;
            }
            NestedAgentTransition::Resume => {
                self.suppressed = true;
            }
        }
    }
}

pub fn attach(target: &str, show_status: bool, stdin_eof: AttachStdinEof) -> Result<()> {
    // legacy show_status=false는 "agent 기본 RowOff"에 해당하며 사용자가 --no-status를
    // 명시한 것이 아니다 → explicit_no_status=false. cmux pill sink는 명시적 비활성에서만
    // 꺼지므로 이 구분이 중요하다(설계 §4.1).
    attach_with_presence(
        target,
        StatusPresencePolicy::from_legacy_show_status(show_status),
        stdin_eof,
        false,
    )
}

pub fn attach_with_presence(
    target: &str,
    presence_policy: StatusPresencePolicy,
    stdin_eof: AttachStdinEof,
    explicit_no_status: bool,
) -> Result<()> {
    let original_info = info(target)?;
    attach_with_presence_and_cue(
        &original_info.pane_id,
        presence_policy,
        stdin_eof,
        &original_info,
        None,
        explicit_no_status,
    )
}

fn attach_with_presence_and_cue(
    target: &str,
    presence_policy: StatusPresencePolicy,
    stdin_eof: AttachStdinEof,
    original_info: &SessionInfo,
    agent_presence_cue: Option<AgentPresenceCue>,
    // 사용자가 `--no-status`를 명시했는지. 정책 `RowOff`는 "agent 기본"과 "--no-status"를
    // 구분 못 하므로, cmux pill sink 게이트(`sink_enabled`)는 이 신호로 명시적 비활성을 존중한다.
    explicit_no_status: bool,
) -> Result<()> {
    ensure_server()?;
    ensure_panic_terminal_cleanup_hook();
    // status 백엔드 라우팅: 환경 스냅샷으로 가장 안전한 백엔드를 고른다.
    // in-pane DECSTBM row를 그릴지는 `in_grid` 게이트가, off-grid cmux pill sink를 켤지는
    // `sink_enabled` 게이트가 각각 결정한다(아래). NativeChrome/DelegatedSurface(Tmux)/
    // TitleCueDelegation의 실렌더 배선은 후속 단계.
    let status_backend = select_status_backend(presence_policy, &gather_status_env_snapshot());
    // 라우팅 정합성(설계 §4.1): in-grid DECSTBM row와 off-grid cmux pill sink를 별도 게이트로
    // 분리한다. R8 상호배타(sink_enabled ⟹ in_grid==false)는 compute_in_grid의 `!sink_enabled`
    // 항으로 구조적으로 보장된다.
    //
    // sink_enabled를 먼저 계산한 뒤 in_grid를 결정한다(in_grid가 sink_enabled에 의존).
    // status 명령 설정을 게이트 밖으로 호이스트(설계 §4.1, Critic m1): `sink_enabled`가
    // `status_command_config.is_some()`(= LTERM_STATUS_COMMAND 설정됨)을 읽어야 하므로.
    // argv 파싱은 env-only라 부작용 없음.
    let status_command_config = StatusCommandConfig::from_env();
    // off-grid cmux pill sink 게이트(설계 §4.1): backend==Cmux + 콘텐츠 명령 구성됨 +
    // 명시적 비활성(--no-status) 아님. `requests_row()`에 종속되지 않아 codex(RowOff)에서도 켜진다.
    let sink_enabled = compute_sink_enabled(
        status_backend,
        status_command_config.is_some(),
        explicit_no_status,
    );
    // pill 활성 세션만 off-grid(in_grid=false). cmux 셸 세션(sink_enabled=false)은 기존
    // DECSTBM 동작 보존. sink_enabled=true면 구조적으로 in_grid=false가 보장된다(R8).
    let in_grid = compute_in_grid(status_backend, presence_policy, sink_enabled);
    // cmux pill sink가 켜지면 attach 시점 1회 워크스페이스 컨텍스트를 확보한다(설계 §4.2,
    // UUID 우선·stored 우선). 이 컨텍스트는 아래에서 `CmuxStatusSink::new`로 소비된다.
    // sink가 꺼져 있으면 `cmux identify`를 호출하지 않아 비-cmux 환경의 불필요한 서브프로세스
    // 스폰을 피한다. 식별 실패(`None`)면 sink를 만들지 않는다(blackout — 별도 처리 없음).
    let cmux_status_context = sink_enabled
        .then(|| crate::tmux_compat::cmux_status_identity(target))
        .flatten();
    // 콘텐츠/메타/poll은 in-grid든 sink든 모두 필요하다. StatusBar(enter/refresh)와 DECSTBM
    // row 예약은 in-grid 경로에서만 한다(아래 개별 분기).
    let content_active = in_grid || sink_enabled;
    let mut title_cue_runtime = agent_presence_cue.map(AgentTitleCueRuntime::new);
    let idle_wakeup_enabled = content_active || title_cue_runtime.is_some();
    // status bar 는 SessionInfo 의 메타데이터 (이름/명령 등) 가 필요하므로 켜졌을 때만
    // info() 를 호출한다. PR #14 의 client-side first-attach guard 가 사라졌으므로
    // attached_clients 를 미리 읽을 이유는 더 이상 없다 — server 가 자체 clamp-to-
    // smallest 로 사이즈 정책을 결정한다 (PR #15).
    let status_info = if content_active {
        Some(original_info.clone())
    } else {
        None
    };
    // off-grid cmux pill sink(설계 §3.1/§4.3). 식별된 워크스페이스 컨텍스트가 있을 때만 생성하며,
    // 라이프타임은 attach 함수 스코프에 결박된다(끝에서 `Drop`이 자기 키를 전부 청소 — 누수 1차
    // 방어). 생성 직후 1회 `reconcile_orphans`로 직전 하드킬/abort 잔재(자기 prefix)를 청소한다
    // (누수 2차 방어 — list-status 실패 시 best-effort 생략). 키 prefix는 안정적인 canonical
    // pane id(`status_info.pane_id`)로 만들어 다중 세션 키 충돌을 차단한다.
    let mut cmux_status_sink = cmux_status_context.map(|context| {
        let pane_id = status_info
            .as_ref()
            .map(|info| info.pane_id.as_str())
            .unwrap_or(target);
        let mut sink = crate::tmux_compat::CmuxStatusSink::new(context, pane_id);
        sink.reconcile_orphans();
        sink
    });
    let nested_monitor_enabled = in_grid
        && presence_policy.allows_nested_suspend()
        && status_info
            .as_ref()
            .is_some_and(|info| info.agent_name.is_none());
    // row_runtime은 in-grid DECSTBM row 예약(pty rows-1, scroll-region)을 관장한다.
    // sink(cmux pill)는 off-grid라 풀 rows를 PTY에 넘겨야 하므로 in_grid만 전달한다.
    let row_runtime = StatusPresenceRuntimeHandle::new(in_grid);
    // `--status` maps to ForceRow: the user explicitly chose to trade a host
    // row for agent visibility.  Some agent launch modes (for example OMC
    // madmax) enter alt-screen during startup; if repaint is skipped while the
    // alt buffer is active, that startup clear erases the initially drawn row
    // and the explicit status request appears to do nothing.  Keep the old
    // conservative behavior for RowAuto shell sessions (vim/fullscreen apps),
    // but let ForceRow self-heal inside alt-screen.
    let draw_status_during_alt_screen = presence_policy == StatusPresencePolicy::ForceRow;
    let (cols, rows) = terminal_size();
    let pty_rows = row_runtime.pty_rows_for(rows);

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

    // RawAttachTerminalGuards owns raw-mode restoration and host-side cleanup
    // ordering: attach_active drops first, then the guard drops RawModeGuard,
    // then its HostTerminalCleanupGuard field emits host stdout cleanup. The
    // child PTY stream is never used for this cleanup.
    let alt_screen_state = Arc::new(AltScreenState::default());
    let terminal_guards = RawAttachTerminalGuards::enter(Arc::clone(&alt_screen_state))?;
    let _attach_active = AttachActiveGuard::enter();
    let mut terminal_output_tracker = TerminalOutputTracker::new(
        terminal_guards.keyboard_protocol_restore_state(),
        Arc::clone(&alt_screen_state),
        row_runtime.status_scroll_bottom_for(rows),
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
                match stdin_input_state(stdin_fd, Duration::from_millis(100))? {
                    StdinInputState::Pending => continue,
                    StdinInputState::Ready => {}
                    StdinInputState::InvalidFd { .. } => break,
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
        if result.is_err() {
            input_running.store(false, Ordering::SeqCst);
            let _ = writer.shutdown(std::net::Shutdown::Both);
        } else if detach_on_stdin_eof {
            let _ = writer.shutdown(std::net::Shutdown::Write);
        }
        result
    });

    let resize_running = Arc::clone(&running);
    let resize_target = target.to_string();
    let resize_row_runtime = row_runtime.clone();
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
            let resize_result = resize_row_runtime.with_locked(|runtime| {
                if runtime.state == StatusPresenceState::Transitioning {
                    return Ok(());
                }
                resize(
                    &resize_target,
                    attach_pty_rows(current.1, runtime.state == StatusPresenceState::Active),
                    current.0,
                    Some(subscriber_id),
                )
            });
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
    // in-grid status row 전용 host SGR reset. off-grid sink는 in-grid row를 그리지 않으므로
    // in_grid 경로에서만 reset한다(기존 status_enabled 의미 보존).
    reset_raw_attach_initial_sgr_if_needed(in_grid, stdout.is_terminal(), &mut stdout)?;
    // status_style은 in-grid DECSTBM row의 시각 스타일이다. sink 경로(in_grid=false)에서는
    // None이 되어 StatusBar::enter의 reserve_terminal_area가 no-op이 된다(DECSTBM 미출현 보장).
    let status_style = in_grid
        .then(|| resolve_status_style(status_info.as_ref().and_then(|info| info.status_theme)));
    let mut status_bar = StatusBar::enter(
        status_info.as_ref(),
        status_style,
        Some(Arc::clone(&alt_screen_state)),
        &mut stdout,
    )?;
    // metadata/command 스레드 생명·일시정지 토글. 콘텐츠가 흐르는 경로(in_grid 또는 sink)에서
    // 활성화한다. sink는 아직 소비처(C6)가 없어 콘텐츠는 무시되지만, 폴링 파이프라인 자체는
    // C5/C6가 받을 수 있도록 켜둔다.
    let status_metadata_enabled = Arc::new(AtomicBool::new(content_active));
    let status_metadata = status_info.as_ref().map(|info| {
        spawn_status_metadata_thread(
            info.pane_id.clone(),
            Arc::clone(&running),
            Arc::clone(&status_metadata_enabled),
        )
    });
    // command-backed status: status가 켜져 있고 LTERM_STATUS_COMMAND가 설정됐을 때만
    // 외부 명령 폴링 스레드를 띄운다. 미설정/파싱 실패면 None이 되어 기존 metadata-only
    // 동작(format_status_line fallback)을 그대로 유지한다. spec §5.1/§5.3.
    // Phase 2: CLI 플래그(`--status-command`) 우선 적용은 추후 추가. 현재는 env-only.
    let status_command = status_info
        .as_ref()
        .filter(|_| content_active)
        .zip(status_command_config)
        .map(|(info, config)| {
            // allow_color는 draw 분기에서 테마 bg on/off를 결정하므로 미리 반영한다.
            status_bar.command_allow_color = config.allow_color;
            // 생명주기/일시정지 게이트는 metadata 스레드와 동일 토글에 연동한다.
            // status_metadata_enabled는 nested-agent suspend/resume 시 false/true로
            // 토글되므로 동일 Arc를 공유해 command 스레드도 함께 pause/resume된다.
            // alt-screen(vim 등) 게이트는 별도로 alt_screen_state를 공유해, 활성 중에는
            // 그리지도 않을 출력을 위한 외부 프로세스 spawn을 건너뛴다(draw 가드와 일관).
            spawn_status_command_thread(
                info.pane_id.clone(),
                config.argv,
                config.interval,
                config.allow_color,
                config.debug,
                Arc::clone(&running),
                Arc::clone(&status_metadata_enabled),
                Arc::clone(&alt_screen_state),
                draw_status_during_alt_screen,
                status_backend.surface_format(),
            )
        });
    let nested_detection = nested_monitor_enabled
        .then(|| spawn_nested_agent_detection_thread(target.to_string(), Arc::clone(&running)));
    if idle_wakeup_enabled {
        let output_idle_timeout = if content_active {
            ATTACH_OUTPUT_IDLE_TIMEOUT
        } else {
            AGENT_TITLE_REFRESH
        };
        reader
            .get_ref()
            .set_read_timeout(Some(output_idle_timeout))
            .context("set attach output read timeout")?;
    }
    let mut buf = [0_u8; 8192];
    let mut status_dirty = false;
    // `observe()`가 status row 손상(ED/DECSTBM reset/RIS)을 확정 감지하면 true가 되어 heartbeat
    // fast lane(STATUS_DAMAGE_HEARTBEAT)을 활성화한다. 성공 repaint로 status_dirty가 클리어되면
    // (루프 상단에서) 같이 클리어한다. 손상은 항상 status_dirty도 같이 set하므로 두 플래그는
    // 동기화 상태로 유지된다.
    let mut damage_pending = false;
    let mut last_status_refresh = Instant::now();
    // content-dedup 백스톱: dedup이 idle에서 실제 redraw를 "내용 변경 시에만"으로 줄였기에
    // 주기적 redraw가 수행하던 host-side 손상 자가복구(scroll-region 재확인 + 추적 고스트 청소)가
    // 사라진다. STATUS_HEARTBEAT_FORCED(2초)마다 1번 force_redraw로 dedup을 우회해 실제 redraw를
    // 강제, 이 자가복구를 복원한다. ~0.5Hz라 4Hz 커서 깜빡임은 재발하지 않는다.
    let mut last_forced_redraw = Instant::now();
    let mut prev_alt_screen_active = false;
    let status_metadata_rx = status_metadata.as_ref().map(|(rx, _)| rx);
    let status_command_rx = status_command.as_ref().map(|(rx, _)| rx);
    let nested_detection_rx = nested_detection.as_ref().map(|(rx, _)| rx);
    let mut nested_detector = NestedAgentDetector::new();
    let output_result = (|| -> Result<()> {
        'output: loop {
            if !running.load(Ordering::SeqCst) {
                break;
            }
            let alt_screen_active = alt_screen_state.active.load(Ordering::Relaxed);
            // 백스톱 클리어: idle/alt-exit/resume 등 출력이 잠잠한 repaint 경로는 status_dirty를
            // 클리어한 뒤 다시 set하지 않으므로, 여기서 damage_pending도 함께 내린다. can_draw_status가
            // false인 suspend 구간에서 set된 damage_pending(이때는 status_dirty가 set되지 않음)도
            // 여기서 안전하게 정리된다. 단, busy 출력 경로는 forward가 매 iteration status_dirty를
            // 다시 set해 이 지점이 발화하지 못하므로, fast lane 분기가 복구 시점에 직접 클리어한다(아래).
            if !status_dirty {
                damage_pending = false;
            }

            if let Some(rx) = nested_detection_rx {
                while let Ok(presence) = rx.try_recv() {
                    if let Some(transition) = nested_detector.apply_presence_poll(presence) {
                        let transition_result = match transition {
                            NestedAgentTransition::Suspend => suspend_status_row(
                                target,
                                subscriber_id,
                                &row_runtime,
                                &mut status_bar,
                                &mut stdout,
                                &mut terminal_output_tracker,
                                &status_metadata_enabled,
                                &mut status_dirty,
                                &mut last_status_refresh,
                            )?,
                            NestedAgentTransition::Resume => resume_status_row(
                                target,
                                subscriber_id,
                                &row_runtime,
                                &mut status_bar,
                                &mut stdout,
                                &mut terminal_output_tracker,
                                &status_metadata_enabled,
                                &mut status_dirty,
                                &mut last_status_refresh,
                            )?,
                        };
                        match transition_result {
                            StatusTransitionResult::Applied => {}
                            StatusTransitionResult::Ignored => {
                                nested_detector.retry_transition(transition);
                            }
                            StatusTransitionResult::Detached => break 'output,
                        }
                    }
                }
            }

            if apply_pending_status_metadata(status_metadata_rx, &mut status_bar)
                && row_runtime.can_draw_status()
            {
                status_dirty = true;
                if (!alt_screen_active || draw_status_during_alt_screen)
                    && refresh_status_or_detached(
                        &mut status_bar,
                        &mut stdout,
                        &mut status_dirty,
                        &mut last_status_refresh,
                    )?
                {
                    break;
                }
            }

            // command-backed status: 새 명령 출력(understatus stdout)이 도착하면 두 경로로 분기한다.
            // - cmux pill sink 활성(`cmux_status_sink`가 Some): off-grid pill 경로다. 출력을
            //   `sink.apply`로 넘겨 cmux 사이드바 pill만 갱신하고 **StatusBar(`command_line`)와
            //   DECSTBM redraw(`refresh_status_or_detached`)에는 절대 진입하지 않는다**(설계 §3.1
            //   in_grid 전용 경로 우회 — Cmux pill 경로는 StatusBar를 건드리지 않는다).
            // - sink 비활성(None): 기존 in-grid 경로 그대로. 직전 값과 다르면 `command_line`을
            //   갱신하고 metadata와 동일한 redraw 경로로 status row를 다시 그린다(바이트 불변).
            if let Some(latest) = apply_pending_status_command(status_command_rx) {
                if let Some(sink) = cmux_status_sink.as_mut() {
                    sink.apply(&latest);
                } else if status_bar.command_line.as_deref() != Some(latest.as_str()) {
                    status_bar.command_line = Some(latest);
                    if row_runtime.can_draw_status() {
                        status_dirty = true;
                        if (!alt_screen_active || draw_status_during_alt_screen)
                            && refresh_status_or_detached(
                                &mut status_bar,
                                &mut stdout,
                                &mut status_dirty,
                                &mut last_status_refresh,
                            )?
                        {
                            break;
                        }
                    }
                }
            }

            // alt-screen 종료 즉시 refresh: alt buffer로 흘러갔던 status는 폐기되었으므로
            // 다음 heartbeat까지 빈 상태가 되지 않게 한 번 redraw한다. 이 redraw가 PTY의
            // main-buffer redraw와 시점이 겹치면 미세한 깜빡임이 가능하나, scroll region
            // (rows-1)이 PTY 본문을 status row와 분리하므로 실용적 문제는 없다.
            if row_runtime.can_draw_status() && prev_alt_screen_active && !alt_screen_active {
                // alt-screen에서 status 행이 숨겨졌다 복귀한 것이라, 그릴 본문이 직전과
                // 동일해도 content-dedup을 우회해 반드시 한 번 그려야 한다.
                status_bar.force_redraw = true;
                if refresh_status_or_detached(
                    &mut status_bar,
                    &mut stdout,
                    &mut status_dirty,
                    &mut last_status_refresh,
                )? {
                    break;
                }
            }
            prev_alt_screen_active = alt_screen_active;

            while resize_rx.try_recv().is_ok() {
                let (_, current_rows) = terminal_size();
                terminal_output_tracker
                    .set_status_scroll_bottom(row_runtime.status_scroll_bottom_for(current_rows));
                // resize는 reserve(DECSTBM scroll-region, rows 의존)를 반드시 재발행해야 하므로
                // force_redraw를 명시한다. dedup의 draw-body 변화 감지에 간접 의존하지 않고 직접
                // 보장하며, alt-exit/resume/damage 경로와 일관된다(alt-screen 중이면 refresh가
                // skip되지만 플래그는 sticky라 종료 후 edge refresh가 reserve 포함으로 소비).
                status_bar.force_redraw = true;
                // alt-screen 동안 refresh하면 alt buffer로 출력되어 vim 등과 충돌한다.
                // 리사이즈 자체는 daemon-side resize 호출이 이미 처리했으므로, alt-screen
                // 종료 후 edge refresh가 새 크기로 다시 그린다.
                if row_runtime.can_draw_status()
                    && (!alt_screen_active || draw_status_during_alt_screen)
                    && refresh_status_or_detached(
                        &mut status_bar,
                        &mut stdout,
                        &mut status_dirty,
                        &mut last_status_refresh,
                    )?
                {
                    break 'output;
                }
            }
            // heartbeat는 idle(STATUS_HEARTBEAT) + 손상 fast lane(STATUS_DAMAGE_HEARTBEAT) +
            // forced(STATUS_HEARTBEAT_FORCED) 세 경로를 가진다. busy PTY 출력 중 확정 손상은
            // fast lane으로 ~50ms 내, 그 외 dirty는 forced 경로로 self-heal이 발화한다.
            // 자세한 조건은 `heartbeat_due` 도큐먼트 참조.
            // content-dedup 백스톱 게이트: idle에서 heartbeat가 발화해 refresh를 시도할 때,
            // 2초마다 한 번 force_content_redraw=true로 dedup을 우회해 **내용만** redraw(고스트
            // 청소+커서 숨김)한다. reserve(DECSTBM scroll-region 재설정)는 내보내지 않아 codex 등이
            // 쓰는 자체 scroll-region을 덮어쓰지 않는다(주기적 reserve 재확인이 codex idle 레이아웃을
            // 침범해 입력칸이 늘어나던 회귀를 차단). 실제 화면 손상은 fast lane/force_redraw 경로가
            // reserve 포함 redraw로 따로 복구한다. 나머지 시도는 dedup 생략을 유지해 커서를 건드리지 않는다.
            if last_forced_redraw.elapsed() >= STATUS_HEARTBEAT_FORCED {
                status_bar.force_content_redraw = true;
                last_forced_redraw = Instant::now();
            }
            if row_runtime.can_draw_status()
                && (!alt_screen_active || draw_status_during_alt_screen)
                && heartbeat_due(last_status_refresh.elapsed(), status_dirty, damage_pending)
            {
                // 이 repaint가 status row 손상을 복구하므로 fast lane 플래그를 여기서 내린다.
                // busy 출력은 forward가 매 iteration status_dirty를 다시 set해 루프 상단의
                // `!status_dirty` 백스톱이 발화하지 못한다 — 복구 시점에 직접 클리어하지 않으면
                // 단발 손상 후에도 출력이 끝날 때까지 50ms마다 불필요한 repaint가 지속된다.
                // 새 손상이 오면 observe()가 다음 청크에서 다시 set한다.
                damage_pending = false;
                if refresh_status_or_detached(
                    &mut status_bar,
                    &mut stdout,
                    &mut status_dirty,
                    &mut last_status_refresh,
                )? {
                    break;
                }
            }
            let n = match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(err) if err.kind() == ErrorKind::Interrupted => continue,
                Err(err)
                    if idle_wakeup_enabled
                        && (err.kind() == ErrorKind::WouldBlock
                            || err.kind() == ErrorKind::TimedOut) =>
                {
                    // alt-screen 동안 refresh하면 alt buffer로 출력되어 vim 등과 충돌한다.
                    if status_dirty
                        && row_runtime.can_draw_status()
                        && (!alt_screen_active || draw_status_during_alt_screen)
                        && refresh_status_or_detached(
                            &mut status_bar,
                            &mut stdout,
                            &mut status_dirty,
                            &mut last_status_refresh,
                        )?
                    {
                        break;
                    }
                    if let Some(title_cue) = title_cue_runtime.as_mut()
                        && title_cue.refresh_due()
                    {
                        match title_cue.refresh_title(&mut stdout) {
                            Ok(()) => {}
                            Err(err) if anyhow_error_is_broken_pipe(&err) => break,
                            Err(err) => return Err(err),
                        }
                    }
                    continue;
                }
                Err(err) => return Err(err).context("read pty output"),
            };
            let output_effects = terminal_output_tracker.observe(&buf[..n]);
            if output_effects.status_area_dirty {
                // 확정 손상: heartbeat fast lane을 활성화해 forced 2초 대신 ~50ms 내 self-heal한다.
                damage_pending = true;
                // 손상은 그릴 본문이 직전과 동일해도 화면이 깨진 상태라 content-dedup을 우회해
                // 반드시 한 번 redraw해야 한다. force_redraw로 다음 refresh가 dedup을 강제 통과한다.
                status_bar.force_redraw = true;
            }
            if let Some(title_cue) = title_cue_runtime.as_mut() {
                title_cue.observe_pty_output();
            }
            if forward_pty_output_frame_or_detached(
                &mut stdout,
                &buf[..n],
                row_runtime.can_draw_status(),
                &mut status_dirty,
            )? {
                break;
            }
        }
        if status_dirty
            && row_runtime.can_draw_status()
            && (!prev_alt_screen_active || draw_status_during_alt_screen)
            && refresh_status_or_detached(
                &mut status_bar,
                &mut stdout,
                &mut status_dirty,
                &mut last_status_refresh,
            )?
        {
            return Ok(());
        }
        Ok(())
    })();

    running.store(false, Ordering::SeqCst);
    let input_result = join_attach_input_thread(input_thread);
    let _ = resize_thread.join();
    if let Some((_, status_metadata_thread)) = status_metadata {
        let _ = status_metadata_thread.join();
    }
    // command/nested-monitor thread는 공유 running 플래그가 이미 false라
    // interruptible_sleep이 다음 청크(≤100ms) 내에 깨어 종료된다. 긴 interval에서도
    // teardown이 블로킹되지 않는다.
    if let Some((_, status_command_thread)) = status_command {
        let _ = status_command_thread.join();
    }
    if let Some((_, nested_detection_thread)) = nested_detection {
        let _ = nested_detection_thread.join();
    }
    let should_diagnose = output_result
        .as_ref()
        .err()
        .is_some_and(anyhow_error_is_broken_pipe)
        || input_result
            .as_ref()
            .err()
            .is_some_and(anyhow_error_is_broken_pipe);
    let attach_result = finish_attach_results(output_result, input_result);

    // Lifecycle RPCs and user-facing error rendering must happen only after all
    // lterm-owned terminal surfaces have restored raw mode, keyboard state,
    // status rows, cursor visibility, and any cmux status artifact.
    drop(status_bar);
    drop(cmux_status_sink);
    drop(title_cue_runtime);
    drop(_attach_active);
    drop(terminal_guards);

    match attach_result {
        Err(error) if should_diagnose => Err(diagnose_attach_failure(error, original_info)),
        result => result,
    }
}

fn join_attach_input_thread(handle: thread::JoinHandle<Result<()>>) -> Result<()> {
    match handle.join() {
        Ok(result) => result.context("attach input thread failed"),
        Err(_) => bail!("attach input thread panicked"),
    }
}

fn finish_attach_results(output_result: Result<()>, input_result: Result<()>) -> Result<()> {
    match (output_result, input_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Ok(()), Err(input_err)) => Err(input_err),
        (Err(output_err), Ok(())) => Err(output_err),
        (Err(output_err), Err(input_err)) => Err(anyhow!(
            "{output_err:#}; attach input thread also failed: {input_err:#}"
        )),
    }
}

fn diagnose_attach_failure(primary: anyhow::Error, original: &SessionInfo) -> anyhow::Error {
    let live = rpc::<SessionInfo>(&Request::Info {
        target: original.id.clone(),
    })
    .ok();
    let matching_live = live.as_ref().filter(|info| info.id == original.id);
    let needs_exit_lookup = matching_live.is_none()
        || matching_live.is_some_and(|info| {
            matches!(info.lifecycle_state(), SessionLifecycleState::Ending { .. })
        });
    let recent = if needs_exit_lookup {
        rpc::<Vec<RecentSessionExit>>(&Request::RecentExits {
            target: Some(original.id.clone()),
            limit: 1,
            scope: ExitListScope::All,
        })
        .unwrap_or_default()
    } else {
        Vec::new()
    };

    match format_attach_failure_diagnosis(original, matching_live, &recent) {
        Some(suffix) => anyhow!("{primary:#}; {suffix}"),
        None => primary,
    }
}

fn format_attach_failure_diagnosis(
    original: &SessionInfo,
    live: Option<&SessionInfo>,
    recent: &[RecentSessionExit],
) -> Option<String> {
    let live = live.filter(|info| info.id == original.id);
    if let Some(info) = live {
        match info.lifecycle_state() {
            SessionLifecycleState::Healthy if info.is_live_work() => {
                let hint = shell_join(&[
                    "lterm".to_string(),
                    "resume".to_string(),
                    "--".to_string(),
                    original.name.clone(),
                ])
                .ok();
                let mut message = format!(
                    "attach transport lost; session remains alive (session_id={})",
                    sanitize::terminal_text(&original.id)
                );
                if let Some(hint) = hint {
                    message.push_str(&format!("; resume with `{hint}`"));
                }
                return Some(message);
            }
            SessionLifecycleState::MonitorFailed if info.is_live_work() => {
                return Some(format!(
                    "attach transport lost; leader state is unknown (session_id={})",
                    sanitize::terminal_text(&original.id)
                ));
            }
            SessionLifecycleState::Ending { trigger } => {
                let mut message = format!(
                    "session is ending and is not reconnectable (session_id={}, trigger={})",
                    sanitize::terminal_text(&original.id),
                    sanitize::terminal_text(&trigger.to_string())
                );
                if let Some(exit) = recent.iter().find(|exit| exit.session_id == original.id) {
                    message.push_str(&format_recent_exit_outcome(exit));
                }
                return Some(message);
            }
            _ => {
                return Some(format!(
                    "attach transport lost; leader state is unknown (session_id={})",
                    sanitize::terminal_text(&original.id)
                ));
            }
        }
    }

    recent
        .iter()
        .find(|exit| exit.session_id == original.id)
        .map(|exit| {
            format!(
                "session ended during attach (session_id={}, trigger={}){}",
                sanitize::terminal_text(&exit.session_id),
                sanitize::terminal_text(&exit.trigger.to_string()),
                format_recent_exit_outcome(exit)
            )
        })
}

fn format_recent_exit_outcome(exit: &RecentSessionExit) -> String {
    let mut details = format!("; outcome={}", exit.outcome_state.as_str());
    if let Some(exit_code) = exit.exit_code {
        details.push_str(&format!("; exit_code={exit_code}"));
    }
    if let Some(signal) = exit.signal.as_deref() {
        details.push_str(&format!("; signal={}", sanitize::terminal_text(signal)));
    }
    details
}

fn attach_pty_rows(rows: u16, show_status: bool) -> u16 {
    if show_status && rows > 1 {
        rows - 1
    } else {
        rows.max(1)
    }
}

fn status_scroll_bottom_for_terminal_rows(rows: u16, show_status: bool) -> Option<u16> {
    (show_status && rows > 1).then_some(rows - 1)
}

/// `total` 만큼 대기하되, [`STATUS_POLL_INTERRUPT_CHUNK`] 단위로 쪼개 매 청크마다
/// `running`을 확인한다. `running`이 `false`가 되면 즉시 중단해 teardown의 `join()`이
/// 긴 interval(최대 1시간) 내내 블로킹되지 않게 한다.
///
/// 반환값: 끝까지 대기하면 `true`(정상 tick 진행), 중간에 `running`이 꺼져 조기
/// 중단됐으면 `false`(루프 종료 신호).
fn interruptible_sleep(total: Duration, running: &AtomicBool) -> bool {
    let mut remaining = total;
    while !remaining.is_zero() {
        if !running.load(Ordering::SeqCst) {
            return false;
        }
        let chunk = remaining.min(STATUS_POLL_INTERRUPT_CHUNK);
        thread::sleep(chunk);
        remaining -= chunk;
    }
    running.load(Ordering::SeqCst)
}

fn spawn_status_metadata_thread(
    target: String,
    running: Arc<AtomicBool>,
    enabled: Arc<AtomicBool>,
) -> (mpsc::Receiver<SessionInfo>, thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::sync_channel(STATUS_METADATA_CHANNEL_LIMIT);
    let handle = thread::spawn(move || {
        while running.load(Ordering::SeqCst) {
            if !interruptible_sleep(STATUS_METADATA_POLL, &running) {
                break;
            }
            if !enabled.load(Ordering::SeqCst) {
                continue;
            }
            let result: Result<SessionInfo> = rpc_with_read_timeout(
                &Request::Info {
                    target: target.clone(),
                },
                Some(STATUS_METADATA_RPC_TIMEOUT),
            );
            if let Ok(info) = result {
                let _ = tx.try_send(info);
            }
        }
    });
    (rx, handle)
}

fn spawn_nested_agent_detection_thread(
    target: String,
    running: Arc<AtomicBool>,
) -> (mpsc::Receiver<NestedAgentPresence>, thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::sync_channel(NESTED_AGENT_DETECTION_CHANNEL_LIMIT);
    let handle = thread::spawn(move || {
        run_nested_agent_detection_loop(&running, &tx, || {
            nested_known_agent_present(&target).map_err(|err| format!("{err:#}"))
        });
    });
    (rx, handle)
}

fn run_nested_agent_detection_loop(
    running: &AtomicBool,
    tx: &mpsc::SyncSender<NestedAgentPresence>,
    mut poll_presence: impl FnMut() -> NestedAgentPresence,
) {
    while running.load(Ordering::SeqCst) {
        let _ = tx.try_send(poll_presence());
        if !interruptible_sleep(NESTED_AGENT_POLL, running) {
            break;
        }
    }
}

fn apply_pending_status_metadata(
    rx: Option<&mpsc::Receiver<SessionInfo>>,
    status_bar: &mut StatusBar,
) -> bool {
    let Some(rx) = rx else {
        return false;
    };
    let mut changed = false;
    while let Ok(info) = rx.try_recv() {
        changed |= status_bar.update_info(&info);
    }
    changed
}

// ---------------------------------------------------------------------------
// command-backed status (attach 출력 루프와 draw_at_size에 배선됨)
// ---------------------------------------------------------------------------

/// command-backed status interval 기본값(초). spec §5.1.
const STATUS_COMMAND_DEFAULT_INTERVAL_SECS: u64 = 2;
/// interval 클램프 하한(초). 0/음수 입력이나 과도한 폴링을 막는다.
const STATUS_COMMAND_MIN_INTERVAL_SECS: u64 = 1;
/// interval 클램프 상한(초, 1시간).
const STATUS_COMMAND_MAX_INTERVAL_SECS: u64 = 3600;
/// 외부 status 명령 stdout 수용 상한(바이트). 초과분은 절단한다. spec §6.4.
const STATUS_COMMAND_MAX_OUTPUT_BYTES: usize = 64 * 1024;
/// 외부 status 명령 1회 실행 타임아웃 상한. interval과 함께 min을 취한다.
const STATUS_COMMAND_MAX_TIMEOUT: Duration = Duration::from_millis(1500);
/// 타임아웃 폴링 sleep 간격. try_wait 폴링 주기.
const STATUS_COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(10);
/// command-backed status 채널 깊이. metadata 스레드와 동일하게 최신 1건만 의미 있다.
const STATUS_COMMAND_CHANNEL_LIMIT: usize = 4;

/// command-backed status 기능의 env/flag 설정. spec §5.1.
///
/// `LTERM_STATUS_COMMAND`가 없으면 `from_env`가 `None`을 반환하고, attach 호출부는
/// 기존 metadata-only 동작(format_status_line fallback)을 유지한다.
///
/// 필드:
/// - `argv`: shell 미경유로 실행할 명령 argv(첫 원소가 실행 파일).
/// - `interval`: 폴링 주기(클램프 적용 후).
/// - `allow_color`: true면 stdout의 SGR 색을 status row로 통과시킨다.
/// - `debug`: true면 실패를 stderr 1줄로 보고한다.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StatusCommandConfig {
    argv: Vec<String>,
    interval: Duration,
    allow_color: bool,
    debug: bool,
}

impl StatusCommandConfig {
    /// 환경 변수에서 설정을 읽는다. `LTERM_STATUS_COMMAND` 미설정이면 `None`.
    ///
    /// 파싱 자체는 env에 의존하지 않는 [`StatusCommandConfig::from_raw_parts`]로 분리해,
    /// 단위 테스트는 env를 건드리지 않고 순수 함수만 검증할 수 있게 한다.
    fn from_env() -> Option<Self> {
        let command = std::env::var("LTERM_STATUS_COMMAND").ok()?;
        Self::from_raw_parts(
            &command,
            std::env::var("LTERM_STATUS_INTERVAL").ok().as_deref(),
            std::env::var("LTERM_STATUS_ANSI").ok().as_deref(),
            std::env::var("LTERM_STATUS_DEBUG").ok().as_deref(),
        )
    }

    /// env에서 분리된 순수 파싱 로직. raw 문자열 입력만으로 설정을 구성한다.
    ///
    /// 파라미터:
    /// - `command`: `LTERM_STATUS_COMMAND` 원시 값.
    /// - `interval_raw`: `LTERM_STATUS_INTERVAL`(초) 원시 값. None/파싱실패 → 기본 2.
    /// - `ansi_raw`: `LTERM_STATUS_ANSI` 원시 값. None → 기본 true(색 통과).
    /// - `debug_raw`: `LTERM_STATUS_DEBUG` 원시 값. None → 기본 false.
    ///
    /// 반환값: 유효 설정. command가 빈 문자열이거나 `shlex::split` 실패(따옴표 미닫힘 등)
    /// 또는 argv가 비면 `None`(기능 안전 비활성). debug면 파싱 실패를 stderr로 경고.
    fn from_raw_parts(
        command: &str,
        interval_raw: Option<&str>,
        ansi_raw: Option<&str>,
        debug_raw: Option<&str>,
    ) -> Option<Self> {
        let debug = parse_status_command_bool(debug_raw, false);
        let Some(argv) = shlex::split(command) else {
            if debug {
                eprintln!(
                    "lterm: LTERM_STATUS_COMMAND 파싱 실패(따옴표 미닫힘 등) — status 명령 비활성"
                );
            }
            return None;
        };
        if argv.is_empty() {
            if debug {
                eprintln!("lterm: LTERM_STATUS_COMMAND가 비어 있어 status 명령 비활성");
            }
            return None;
        }
        Some(Self {
            argv,
            interval: parse_status_command_interval(interval_raw),
            allow_color: parse_status_command_bool(ansi_raw, true),
            debug,
        })
    }
}

/// `LTERM_STATUS_INTERVAL`(초)을 파싱하고 `[1, 3600]`으로 클램프한다.
/// None이거나 정수 파싱 실패 시 기본값 2를 반환한다. spec §5.1.
fn parse_status_command_interval(raw: Option<&str>) -> Duration {
    let secs = raw
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(STATUS_COMMAND_DEFAULT_INTERVAL_SECS)
        .clamp(
            STATUS_COMMAND_MIN_INTERVAL_SECS,
            STATUS_COMMAND_MAX_INTERVAL_SECS,
        );
    Duration::from_secs(secs)
}

/// command-backed status용 bool env 파싱. None이면 `default`, 그 외에는
/// `matches_env_bool`로 명시적 true/false만 인정하고 알 수 없는 값은 `default` 유지.
/// `LTERM_STATUS_ANSI`는 default=true(색 통과)이고 "0"/"false"에서만 false가 된다.
fn parse_status_command_bool(raw: Option<&str>, default: bool) -> bool {
    let Some(value) = raw else {
        return default;
    };
    if matches_env_bool(value, true) {
        true
    } else if matches_env_bool(value, false) {
        false
    } else {
        default
    }
}

/// status payload 필드 길이 상한(바이트). 자식이 stdin을 읽지 않아도
/// `write_all`이 OS 파이프 버퍼(보통 64KB)에 다 들어가 블로킹되지 않도록,
/// payload 총량을 파이프 버퍼보다 충분히 작게(수 KB) 유지한다. spec §6.4(견고화).
/// session/pane은 식별자라 짧게, cwd는 경로라 가장 넉넉히 둔다.
const STATUS_PAYLOAD_SESSION_CAP: usize = 128;
const STATUS_PAYLOAD_PANE_CAP: usize = 128;
const STATUS_PAYLOAD_AGENT_CAP: usize = 64;
const STATUS_PAYLOAD_CWD_CAP: usize = 1024;

/// 문자열을 최대 `max_bytes` 바이트로 char 경계에서 안전하게 절단한다.
/// 멀티바이트 문자를 쪼개지 않도록 경계 직전까지만 남긴다. 이미 한도 이하면 그대로 반환.
fn truncate_to_byte_cap(value: String, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value;
    }
    // max_bytes 이하이면서 char 경계인 가장 큰 인덱스를 찾는다.
    let mut boundary = max_bytes;
    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value[..boundary].to_string()
}

/// command-backed status payload 직렬화 스키마(spec §4.1). 수동 문자열 조립 대신
/// serde_json이 escape를 책임지도록 전용 구조체를 쓴다.
#[derive(Debug, Serialize)]
struct StatusCommandPayload<'a> {
    /// 고정 소스 식별자. 항상 "lterm".
    source: &'a str,
    /// payload 스키마 버전. 현재 1.
    version: u32,
    /// 콘텐츠 명령이 내야 할 출력 포맷(설계 §3.3, additive-optional).
    /// backend==DelegatedSurface(Cmux)면 `"cmux-status"`(pill JSON), 그 외엔 `"oneline"`(기존 SGR 한 줄).
    /// 구버전 understatus는 이 필드를 무시하고 oneline을 내며, lterm sink가 non-JSON으로 무해 처리한다.
    surface_format: &'a str,
    /// 세션 이름(제어문자 제거됨).
    session: String,
    /// pane id(제어문자 제거됨).
    pane: String,
    /// `"<session>/<pane>"` 합성 키.
    session_key: String,
    /// 인식된 에이전트 이름 또는 null.
    agent: Option<String>,
    /// 세션 cwd(제어문자 제거됨) 또는 null(빈 값일 때).
    cwd: Option<String>,
    /// 현재 터미널 열 수.
    cols: u16,
    /// 현재 터미널 행 수.
    rows: u16,
}

/// `SessionInfo`와 현재 터미널 크기로 status 명령 stdin에 전달할 JSON payload를 만든다.
/// spec §4.1.
///
/// 파라미터:
/// - `info`: 대상 세션 메타데이터.
/// - `cols`/`rows`: 현재 터미널 크기.
/// - `surface_format`: 콘텐츠 명령이 낼 출력 포맷(`"cmux-status"` 또는 `"oneline"`). 설계 §3.3.
///   신뢰되는 내부 상수(backend에서 파생)라 escape 대상이 아니지만 serde_json이 처리한다.
///
/// 반환값: 한 줄 JSON 문자열.
///
/// 보안: `name`/`pane_id`/`cwd`는 [`sanitize::terminal_text`]로 제어문자를 먼저 제거한다.
/// serde_json이 따옴표/백슬래시는 escape하지만, raw 제어문자가 JSON에 주입되는 것을
/// 사전에 차단한다. `agent`는 신뢰되는 allowlist 토큰이라 그대로 둔다. cwd가 비면 null.
fn build_status_payload(info: &SessionInfo, cols: u16, rows: u16, surface_format: &str) -> String {
    // 제어문자 제거 후 필드별 길이 cap을 적용해 payload 총량이 OS 파이프 버퍼보다
    // 충분히 작게(수 KB) 유지되도록 한다. 그러면 자식이 stdin을 안 읽어도
    // run_status_command의 write_all이 블로킹되지 않는다(H1 방어). cap은 char 경계 안전.
    let session = truncate_to_byte_cap(
        sanitize::terminal_text(&info.name),
        STATUS_PAYLOAD_SESSION_CAP,
    );
    let pane = truncate_to_byte_cap(
        sanitize::terminal_text(&info.pane_id),
        STATUS_PAYLOAD_PANE_CAP,
    );
    let session_key = format!("{session}/{pane}");
    let cwd = truncate_to_byte_cap(sanitize::terminal_text(&info.cwd), STATUS_PAYLOAD_CWD_CAP);
    let agent = agent_name_from_command(&info.command)
        .map(|agent| truncate_to_byte_cap(agent, STATUS_PAYLOAD_AGENT_CAP));
    let payload = StatusCommandPayload {
        source: "lterm",
        version: 1,
        surface_format,
        session,
        pane,
        session_key,
        agent,
        cwd: (!cwd.is_empty()).then_some(cwd),
        cols,
        rows,
    };
    // 전용 Serialize 구조체이므로 직렬화는 실패하지 않는다(모든 필드가 직렬화 가능).
    serde_json::to_string(&payload).unwrap_or_else(|_| String::from("{}"))
}

/// stdout 드레인 후 reader 스레드 join을 시도할 최대 grace(타임아웃 도달 후).
/// 정상 경로(child.kill로 stdout fd가 닫힘)에서는 reader가 즉시 끝나므로 거의 즉시 join된다.
/// 자손이 stdout 파이프를 점유한 드문 케이스에서만 이 grace를 소진한 뒤 결과를 포기한다.
const STATUS_COMMAND_READER_JOIN_GRACE: Duration = Duration::from_millis(100);

/// 외부 status 명령을 **shell 미경유**로 실행하고 stdout을 회수한다. spec §5.3/§6.4.
///
/// 파라미터:
/// - `argv`: 실행 명령(첫 원소가 실행 파일, 나머지는 인자). shell을 거치지 않아
///   인젝션 표면이 없다.
/// - `stdin_payload`: child stdin으로 보낼 JSON payload. 호출부에서 cap이 적용돼
///   OS 파이프 버퍼보다 작으므로 자식이 stdin을 안 읽어도 write_all이 블로킹되지 않는다.
/// - `timeout`: 1회 실행 상한. 초과 시 child를 kill 후 좀비를 회수하고 `None`.
/// - `max_bytes`: stdout 수용 상한. 초과분은 절단한다.
///
/// 반환값: 정상 종료 + 비어있지 않은 stdout이면 `Some(text)`. spawn 실패, 타임아웃,
/// 비정상 종료, 빈 출력, reader join 실패는 모두 `None`(호출부에서 "직전 라인 유지").
///
/// 설계(견고화): macOS에는 `gtimeout`이 없으므로 자체 폴링 타임아웃을 쓴다.
/// - 타임아웃 시계는 어떤 블로킹 호출보다도 **먼저**(spawn 직후) 시작해 방어한다.
/// - stdout 읽기는 **별도 reader 스레드**가 `read_to_end`로 max_bytes까지 수행한다.
///   메인 흐름은 `try_wait` deadline으로 자식 종료만 기다리고 **절대 read로 블로킹되지
///   않는다**(H2 방어). 자식 종료/타임아웃 후 reader 스레드를 deadline 내 짧은 grace로만
///   join 시도하고, 제때 안 끝나면(자손이 파이프 점유) 결과를 포기하고 `None`을 반환해
///   메인 status 스레드를 진행시킨다.
/// - `process_group(0)`로 자식을 자기 프로세스 그룹에 둔 뒤 timeout/read-grace 실패 시
///   그룹 전체에 SIGKILL을 보낸다. direct child만 죽이면 pipe-holding descendant가
///   stdout을 잡고 남아 reader 스레드와 외부 프로세스를 누적시킬 수 있다.
fn run_status_command(
    argv: &[String],
    stdin_payload: &str,
    timeout: Duration,
    max_bytes: usize,
) -> Option<String> {
    let executable = argv.first()?;
    let mut child = Command::new(executable)
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        // 자식을 자기 프로세스 그룹에 둔다(향후 group-kill 기반, 현재는 무해한 격리).
        .process_group(0)
        .spawn()
        .ok()?;
    let child_process_group = child.id();

    // 방어: 타임아웃 시계를 어떤 블로킹 호출(stdin write 등)보다 먼저 시작한다.
    let started = Instant::now();

    // stdout을 별도 스레드에서 드레인한다. 메인 스레드는 read로 블로킹되지 않는다.
    // max_bytes + 1까지만 읽어 상한 초과 여부를 알 수 있게 한 뒤 호출부에서 절단한다.
    let reader_handle = child.stdout.take().map(|mut stdout| {
        let read_limit = (max_bytes as u64).saturating_add(1);
        thread::spawn(move || -> Option<Vec<u8>> {
            let mut buffer = Vec::new();
            match stdout.by_ref().take(read_limit).read_to_end(&mut buffer) {
                Ok(_) => Some(buffer),
                Err(_) => None,
            }
        })
    });

    // stdin write 실패는 치명적이지 않다(명령이 stdin을 안 읽을 수 있음). drop으로 EOF 전달.
    // payload는 호출부 cap으로 파이프 버퍼보다 작아 자식이 안 읽어도 write_all이 안 막힌다.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(stdin_payload.as_bytes());
        // stdin scope를 명시적으로 닫아 EOF를 보낸다.
    }

    // 메인 흐름: child 종료만 기다린다(read로 블로킹하지 않음). 종료 감지는 WNOWAIT으로
    // 수행해 reader cleanup 전에는 child PID/PGID를 회수하지 않는다. 자식이 빨리 끝났지만
    // 자손이 stdout 파이프를 물고 있는 케이스에서, reader grace 실패 후 process-group kill이
    // 필요한 동안 PID/PGID 재사용 race를 막기 위한 보수적 순서다.
    let child_exited = loop {
        match child_exited_without_reaping(child_process_group) {
            Ok(true) => break true,
            Ok(false) => {
                if started.elapsed() >= timeout {
                    // 타임아웃: process group 전체를 죽인 뒤 direct child를 wait로 회수한다.
                    let _ = kill_process_group(child_process_group, libc::SIGKILL);
                    let _ = child.kill();
                    let _ = child.wait();
                    break false;
                }
                thread::sleep(STATUS_COMMAND_POLL_INTERVAL);
            }
            Err(err) => {
                if child_already_reaped_error(&err) {
                    break false;
                }
                let _ = kill_process_group(child_process_group, libc::SIGKILL);
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
        }
    };

    // reader 스레드를 grace 안에서만 join 시도한다. 정상 경로는 fd가 닫혀 즉시 끝나지만,
    // 자손이 stdout 파이프를 점유하면 read_to_end가 안 끝날 수 있다. 그 경우 핸들을
    // detach(버림)하고 None을 반환해 메인 status 스레드를 절대 막지 않는다.
    let output = match reader_handle {
        Some(handle) => {
            let output = join_reader_within_grace(handle, STATUS_COMMAND_READER_JOIN_GRACE);
            if output.is_none() && child_exited {
                let _ = kill_process_group(child_process_group, libc::SIGKILL);
                let _ = child.wait();
            }
            output
        }
        None => None,
    };

    output.as_ref()?;

    // 비정상 종료/타임아웃이면 출력을 신뢰하지 않는다. child가 정상 deadline 내 종료한
    // 경우에만, reader cleanup 이후에 wait로 실제 종료 상태를 회수한다.
    let status = if child_exited {
        child.wait().ok()?
    } else {
        return None;
    };
    if !status.success() {
        return None;
    }
    let mut buffer = output?;
    buffer.truncate(max_bytes);
    if buffer.is_empty() {
        return None;
    }
    // 비-UTF8 stdout은 lossy 변환으로 안전하게 수용한다.
    Some(String::from_utf8_lossy(&buffer).into_owned())
}

fn child_exited_without_reaping(process_id: u32) -> std::io::Result<bool> {
    let pid = i32::try_from(process_id)
        .map_err(|_| std::io::Error::other("child pid exceeds pid_t range"))?;
    loop {
        let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                info.as_mut_ptr(),
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if rc == 0 {
            let info = unsafe { info.assume_init() };
            return Ok(unsafe { info.si_pid() } != 0);
        }
        let err = std::io::Error::last_os_error();
        if err.kind() != std::io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

fn child_already_reaped_error(err: &std::io::Error) -> bool {
    err.raw_os_error() == Some(libc::ECHILD)
}

fn kill_process_group(process_group_leader: u32, signal: libc::c_int) -> std::io::Result<()> {
    let pgid = i32::try_from(process_group_leader)
        .map_err(|_| std::io::Error::other("process group id exceeds pid_t range"))?;
    if pgid <= 1 {
        return Ok(());
    }
    let rc = unsafe { libc::kill(-(pgid as libc::pid_t), signal) };
    if rc == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(err)
    }
}

/// reader 스레드를 `grace` 안에서만 join 시도한다. 끝났으면 결과를, 아직 실행 중이면
/// 핸들을 detach(버림)하고 `None`을 반환해 호출부가 절대 블로킹되지 않게 한다.
///
/// `JoinHandle::join`은 블로킹이라 타임아웃 join이 std에 없다. 대신 `is_finished()`를
/// 짧게 폴링해 grace 안에 끝났을 때만 join한다. 끝나지 않으면 핸들을 그대로 버리는데,
/// 데몬형 자손이 stdout을 물고 있어도 leak되는 건 reader 스레드 1개뿐이며 메인
/// status 스레드의 무블로킹 진행이 보장된다(완전한 group-kill은 Phase 2).
fn join_reader_within_grace(
    handle: thread::JoinHandle<Option<Vec<u8>>>,
    grace: Duration,
) -> Option<Vec<u8>> {
    let deadline = Instant::now() + grace;
    loop {
        if handle.is_finished() {
            return handle.join().ok().flatten();
        }
        if Instant::now() >= deadline {
            // 자손-점유 케이스: 핸들을 버리고 결과 포기. 메인 스레드는 진행.
            return None;
        }
        thread::sleep(STATUS_COMMAND_POLL_INTERVAL);
    }
}

/// command-backed status 폴링 스레드를 띄운다. `spawn_status_metadata_thread`와
/// 동형 패턴(sync_channel + try_recv 최신)이지만, RPC 대신 외부 명령을 실행한다.
/// spec §5.3.
///
/// 매 tick(interval)마다: `Request::Info`로 `SessionInfo`를 얻고, `terminal_size()`로
/// cols/rows를 구해 [`build_status_payload`]를 만든 뒤 [`run_status_command`]를 호출한다.
/// 성공 출력은 [`sanitize::sanitize_status_command_line`]으로 살균해 채널로 보낸다.
/// 실패면 보내지 않아 호출부가 직전 라인을 유지하게 한다.
///
/// 파라미터:
/// - `target`: `Request::Info` 대상(보통 pane id).
/// - `argv`: 실행할 status 명령 argv.
/// - `interval`: 폴링 주기.
/// - `allow_color`: stdout SGR 통과 여부.
/// - `debug`: 실패를 stderr 1줄로 보고할지 여부.
/// - `running`/`enabled`: metadata 스레드와 동일한 생명주기/일시정지 게이트.
/// - `alt_screen`: alt-screen(vim 등) 활성 상태.
/// - `run_during_alt_screen`: ForceRow(`--status`)처럼 draw 가드가 alt-screen 중에도
///   열리는 명시적 상태줄 요청. false면 그리지도 않을 출력을 위해 매 interval마다
///   외부 프로세스를 spawn하는 낭비를 막는다.
/// - `surface_format`: 매 tick의 payload에 직렬화할 출력 포맷(`"cmux-status"`/`"oneline"`).
///   backend에서 파생된 내부 상수라 `&'static str`. 설계 §3.3.
///
/// 반환값: 살균된 status 라인 수신 채널과 스레드 핸들.
#[allow(clippy::too_many_arguments)]
fn spawn_status_command_thread(
    target: String,
    argv: Vec<String>,
    interval: Duration,
    allow_color: bool,
    debug: bool,
    running: Arc<AtomicBool>,
    enabled: Arc<AtomicBool>,
    alt_screen: Arc<AltScreenState>,
    run_during_alt_screen: bool,
    surface_format: &'static str,
) -> (mpsc::Receiver<String>, thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::sync_channel(STATUS_COMMAND_CHANNEL_LIMIT);
    // 1회 실행 타임아웃은 interval과 상한(1500ms) 중 작은 값으로 둬 폴링이 밀리지 않게 한다.
    let timeout = interval.min(STATUS_COMMAND_MAX_TIMEOUT);
    let handle = thread::spawn(move || {
        while running.load(Ordering::SeqCst) {
            // interval을 짧은 청크로 쪼개 대기하며 매 청크마다 running을 확인한다.
            // detach 시 긴 interval(최대 1시간) 내내 teardown join()이 블로킹되지 않게 한다.
            if !interruptible_sleep(interval, &running) {
                break;
            }
            // enabled(nested-agent suspend 게이트)이 아니거나, alt-screen 활성 중인데
            // draw 가드가 닫혀 있으면 명령을 실행하지 않는다.
            if !enabled.load(Ordering::SeqCst)
                || (alt_screen.active.load(Ordering::Relaxed) && !run_during_alt_screen)
            {
                continue;
            }
            let info: Result<SessionInfo> = rpc_with_read_timeout(
                &Request::Info {
                    target: target.clone(),
                },
                Some(STATUS_METADATA_RPC_TIMEOUT),
            );
            let Ok(info) = info else {
                if debug {
                    eprintln!("lterm: status 명령 tick에서 세션 메타데이터 조회 실패");
                }
                continue;
            };
            let (cols, rows) = terminal_size();
            let payload = build_status_payload(&info, cols, rows, surface_format);
            match run_status_command(&argv, &payload, timeout, STATUS_COMMAND_MAX_OUTPUT_BYTES) {
                Some(output) => {
                    let line =
                        sanitize::sanitize_status_command_line(output.as_bytes(), allow_color);
                    let _ = tx.try_send(line);
                }
                None => {
                    if debug {
                        eprintln!("lterm: status 명령 실행 실패/타임아웃 — 직전 라인 유지");
                    }
                }
            }
        }
    });
    (rx, handle)
}

/// `spawn_status_command_thread` 채널에서 가장 최신 status 라인을 꺼낸다.
/// `apply_pending_status_metadata`와 동형으로, 큐에 쌓인 것을 모두 비우고 마지막 것만 남긴다.
///
/// 반환값: 새 라인이 도착했으면 `Some(latest)`, 없으면 `None`(직전 라인 유지).
fn apply_pending_status_command(rx: Option<&mpsc::Receiver<String>>) -> Option<String> {
    let rx = rx?;
    let mut latest = None;
    while let Ok(line) = rx.try_recv() {
        latest = Some(line);
    }
    latest
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
    // content-dedup으로 본문이 직전과 동일하면 refresh가 아무것도 쓰지 않고 drew=false를
    // 반환한다 — 이때 flush는 생략한다(불필요한 syscall 회피). status_dirty는 어느 경우든
    // 클리어해 "처리됨"으로 표시하고, 타이머는 매 attempt 리셋해 idle 재검 250ms 스로틀을
    // 유지한다(본문 변화가 없으면 drew=false라 실제 redraw 빈도는 4Hz→내용변경 시에만으로 준다).
    let drew = status_bar.refresh(stdout)?;
    if drew {
        stdout.flush().context("flush stdout")?;
    }
    *status_dirty = false;
    *last_status_refresh = Instant::now();
    Ok(())
}

fn refresh_status_or_detached(
    status_bar: &mut StatusBar,
    stdout: &mut std::io::Stdout,
    status_dirty: &mut bool,
    last_status_refresh: &mut Instant,
) -> Result<bool> {
    match refresh_status(status_bar, stdout, status_dirty, last_status_refresh) {
        Ok(()) => Ok(false),
        Err(err) if anyhow_error_is_broken_pipe(&err) => Ok(true),
        Err(err) => Err(err),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusTransitionResult {
    Applied,
    Ignored,
    Detached,
}

#[allow(clippy::too_many_arguments)]
fn suspend_status_row(
    target: &str,
    subscriber_id: u64,
    row_runtime: &StatusPresenceRuntimeHandle,
    status_bar: &mut StatusBar,
    stdout: &mut std::io::Stdout,
    terminal_output_tracker: &mut TerminalOutputTracker,
    metadata_enabled: &AtomicBool,
    status_dirty: &mut bool,
    last_status_refresh: &mut Instant,
) -> Result<StatusTransitionResult> {
    row_runtime.with_locked(|runtime| -> Result<StatusTransitionResult> {
        if runtime.state != StatusPresenceState::Active {
            return Ok(StatusTransitionResult::Ignored);
        }
        runtime.state = StatusPresenceState::Transitioning;
        metadata_enabled.store(false, Ordering::SeqCst);

        if let Err(err) = status_bar.restore(stdout) {
            runtime.state = StatusPresenceState::Active;
            metadata_enabled.store(true, Ordering::SeqCst);
            if anyhow_error_is_broken_pipe(&err) {
                return Ok(StatusTransitionResult::Detached);
            }
            return Err(err);
        }

        let (cols, rows) = terminal_size();
        match handle_resize_tick(resize(target, rows.max(1), cols, Some(subscriber_id))) {
            ResizeTickOutcome::Advance => {
                status_bar.style = None;
                terminal_output_tracker.set_status_scroll_bottom(None);
                runtime.state = StatusPresenceState::Suspended;
                *status_dirty = false;
                *last_status_refresh = Instant::now();
                write_lterm_title_cue(
                    stdout,
                    &status_bar.session_name,
                    &status_bar.pane_id,
                    "nested agent",
                )?;
                stdout.flush().context("flush status suspend")?;
                Ok(StatusTransitionResult::Applied)
            }
            ResizeTickOutcome::Retry => {
                runtime.state = StatusPresenceState::Active;
                metadata_enabled.store(true, Ordering::SeqCst);
                *status_dirty = true;
                Ok(StatusTransitionResult::Ignored)
            }
            ResizeTickOutcome::StaleSubscriberId => {
                runtime.state = StatusPresenceState::Disabled;
                Ok(StatusTransitionResult::Detached)
            }
        }
    })
}

#[allow(clippy::too_many_arguments)]
fn resume_status_row(
    target: &str,
    subscriber_id: u64,
    row_runtime: &StatusPresenceRuntimeHandle,
    status_bar: &mut StatusBar,
    stdout: &mut std::io::Stdout,
    terminal_output_tracker: &mut TerminalOutputTracker,
    metadata_enabled: &AtomicBool,
    status_dirty: &mut bool,
    last_status_refresh: &mut Instant,
) -> Result<StatusTransitionResult> {
    row_runtime.with_locked(|runtime| -> Result<StatusTransitionResult> {
        if runtime.state != StatusPresenceState::Suspended {
            return Ok(StatusTransitionResult::Ignored);
        }
        runtime.state = StatusPresenceState::Transitioning;
        let info = match info(target) {
            Ok(info) => info,
            Err(_) => {
                runtime.state = StatusPresenceState::Suspended;
                metadata_enabled.store(false, Ordering::SeqCst);
                return Ok(StatusTransitionResult::Ignored);
            }
        };
        let (cols, rows) = terminal_size();
        let body_rows = attach_pty_rows(rows, true);
        match handle_resize_tick(resize(target, body_rows, cols, Some(subscriber_id))) {
            ResizeTickOutcome::Advance => {
                terminal_output_tracker
                    .set_status_scroll_bottom(status_scroll_bottom_for_terminal_rows(rows, true));
                status_bar.update_info(&info);
                status_bar.style = Some(resolve_status_style(info.status_theme));
                // suspend 동안 status 행이 화면에서 사라졌으므로, 복귀 시 본문이 직전과
                // 동일해도 content-dedup을 우회해 반드시 한 번 그려야 한다.
                status_bar.force_redraw = true;
                match refresh_status_or_detached(
                    status_bar,
                    stdout,
                    status_dirty,
                    last_status_refresh,
                ) {
                    Ok(true) => {
                        runtime.state = StatusPresenceState::Disabled;
                        return Ok(StatusTransitionResult::Detached);
                    }
                    Ok(false) => {}
                    Err(err) => {
                        runtime.state = StatusPresenceState::Suspended;
                        metadata_enabled.store(false, Ordering::SeqCst);
                        return Err(err);
                    }
                }
                metadata_enabled.store(true, Ordering::SeqCst);
                runtime.state = StatusPresenceState::Active;
                Ok(StatusTransitionResult::Applied)
            }
            ResizeTickOutcome::Retry => {
                runtime.state = StatusPresenceState::Suspended;
                metadata_enabled.store(false, Ordering::SeqCst);
                Ok(StatusTransitionResult::Ignored)
            }
            ResizeTickOutcome::StaleSubscriberId => {
                runtime.state = StatusPresenceState::Disabled;
                Ok(StatusTransitionResult::Detached)
            }
        }
    })
}

fn forward_pty_output_frame_or_detached(
    stdout: &mut impl Write,
    bytes: &[u8],
    status_enabled: bool,
    status_dirty: &mut bool,
) -> Result<bool> {
    if let Err(err) = stdout.write_all(bytes) {
        if err.kind() == ErrorKind::BrokenPipe {
            return Ok(true);
        }
        return Err(err).context("write stdout");
    }

    if status_enabled {
        // Mark the status dirty, but do not repaint in the same PTY-output
        // window. This is especially important when agent TUIs emit ED (`CSI J`)
        // or reset DECSTBM (`CSI r`): those sequences can erase the host-side
        // status row or temporarily return the scroll region to the full terminal
        // surface, and interleaving a host-side cursor/SGR repair between an
        // agent TUI's erase and subsequent redraw has repeatedly caused submitted
        // prompt lines and color state to disappear. The read timeout/heartbeat
        // paths repaint after output goes quiet, while the forced heartbeat still
        // bounds recovery during continuous output. 확정 손상(ED/DECSTBM reset/RIS)은
        // heartbeat fast lane(STATUS_DAMAGE_HEARTBEAT)으로 ~50ms 내 빠르게 복구된다.
        *status_dirty = true;
    }

    if let Err(err) = stdout.flush() {
        if err.kind() == ErrorKind::BrokenPipe {
            return Ok(true);
        }
        return Err(err).context("flush stdout");
    }
    Ok(false)
}

fn reset_raw_attach_initial_sgr_if_needed(
    status_enabled: bool,
    stdout_is_terminal: bool,
    stdout: &mut impl Write,
) -> Result<()> {
    if status_enabled || !stdout_is_terminal {
        return Ok(());
    }
    reset_host_terminal_sgr(stdout)?;
    stdout
        .flush()
        .context("flush raw attach initial SGR reset")?;
    Ok(())
}

fn anyhow_error_is_broken_pipe(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io_err| io_err.kind() == ErrorKind::BrokenPipe)
    })
}

/// heartbeat **timing/dirty 서브 게이트**만 평가한다. **호출자는 반드시 `status_enabled`
/// 와 `!alt_screen_active` 가드를 별도로 평가해야 한다** — alt-screen 중에 forced redraw가
/// alt buffer로 새는 회귀를 방지하기 위한 분리. 함수명이 "heartbeat 전체 게이트"로 오인되지
/// 않도록 `heartbeat_due`로 둔다.
///
/// - **idle 경로**: `!status_dirty` 이고 `STATUS_HEARTBEAT` 경과 시 발화 — PTY가 잠잠한
///   동안 외부 DECSTBM 리셋(다른 앱 백그라운드 등)을 self-heal.
/// - **fast lane(손상 경로)**: `damage_pending`(=`observe()`가 ED/DECSTBM reset/RIS로 status
///   row 손상을 확정 감지)이고 `STATUS_DAMAGE_HEARTBEAT` 경과 시 발화 — 풀스크린 에이전트
///   TUI가 status 영역을 손상시킨 경우 forced 2초 백스톱 대신 ~50ms 내 self-heal한다.
///   `STATUS_DAMAGE_HEARTBEAT` 간격 자체가 rate-limit이라 연속 출력 중에도 매 프레임 repaint
///   폭주가 일어나지 않는다.
/// - **forced 경로**: `STATUS_HEARTBEAT_FORCED` 경과 시 dirty 여부와 무관하게 발화 —
///   PTY가 연속 출력 중이면 read()가 매번 Ok(n)을 반환해 WouldBlock 분기가 fire하지
///   않으므로 status_dirty가 영원히 클리어되지 않는다. 이 경로가 없으면 cmux pane swap /
///   Termius 백그라운드 복귀 후 status 영역 자가복구가 무한히 차단된다. fast lane이 잡지 못하는
///   비손상 dirty(예: metadata 변경)의 백스톱이기도 하다.
fn heartbeat_due(elapsed: Duration, status_dirty: bool, damage_pending: bool) -> bool {
    if !status_dirty && elapsed >= STATUS_HEARTBEAT {
        return true;
    }
    if damage_pending && elapsed >= STATUS_DAMAGE_HEARTBEAT {
        return true;
    }
    elapsed >= STATUS_HEARTBEAT_FORCED
}

fn status_bar_disabled_by_env() -> bool {
    env_flag_enabled("LTERM_NO_STATUS") || env_flag_disabled("LTERM_STATUS")
}

fn status_sgr_stack_supported() -> bool {
    if env_flag_disabled("LTERM_STATUS_SGR_STACK") {
        return false;
    }
    if env_flag_enabled("LTERM_STATUS_SGR_STACK") {
        return true;
    }

    let term = std::env::var("TERM")
        .unwrap_or_default()
        .to_ascii_lowercase();
    if term.is_empty() || term == "dumb" {
        return false;
    }

    let terminal_identity = [
        std::env::var("TERM_PROGRAM").unwrap_or_default(),
        std::env::var("LC_TERMINAL").unwrap_or_default(),
        std::env::var("TERMINAL_EMULATOR").unwrap_or_default(),
    ]
    .join(" ")
    .to_ascii_lowercase();

    // `CSI # {` / `CSI # }` are xterm-private controls, not a generic
    // xterm-compatible TERM capability. Auto-enable only for terminal identities
    // we intentionally allowlist; leave Kitty/Alacritty/Ghostty/Termius and
    // generic TERM=xterm-* on the explicit opt-in path until verified on device.
    terminal_identity.contains("xterm")
        || terminal_identity.contains("iterm")
        || terminal_identity.contains("wezterm")
        || matches!(term.as_str(), "xterm" | "wezterm")
}

// ── cross-env status 백엔드 라우팅 (PoC1: 순수 결정 함수 + 타입만, 아직 미배선) ──
//
// 연구 결론(memory: lterm-status-default-on §B): 일반 터미널 + 메인버퍼 에이전트에서
// 분리형 status 한 줄의 무손상 공존은 원리적으로 불가능하다. 안전은 (1) host가 곧
// 터미널이거나(tmux/cmux) (2) 셀 그리드 밖 네이티브 렌더(iTerm)일 때만 성립한다.
// 따라서 단일 boolean(status_enabled) 대신 환경/정책으로 "가장 안전한 메커니즘"을
// 고르는 라우팅이 필요하다. 이 PoC는 그 결정 로직만 순수 함수로 확정하고, 실제 렌더
// 배선(StatusBackend trait, cmux surface 위임 등)은 후속 단계로 분리한다.

/// status row를 어떤 메커니즘으로 표시할지 결정한 결과.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusBackend {
    /// 미표시(env 강제 off / 비-TTY / 기하 부족).
    Disabled,
    /// 현행 DECSTBM 단일행 오버레이(plain 터미널 best-effort, 에이전트와 ~50ms 수렴).
    DecstbmOverlay,
    /// 터미널 네이티브 chrome(iTerm OSC1337 user var/타이틀). 셀 그리드 밖, plain text·단일라인.
    NativeChrome,
    /// 별도 surface 위임(cmux split=ghostty PTY / 진짜 tmux status-line). 멀티라인·truecolor 안전.
    DelegatedSurface(SurfaceKind),
    /// 에이전트엔 분리형 row를 양보하고 타이틀 cue + LTERM_SESSION/LTERM_PANE 위임(유일 무손상).
    TitleCueDelegation,
}

impl StatusBackend {
    /// 콘텐츠 명령이 내야 할 출력 포맷 식별자(payload `surface_format` 필드, 설계 §3.3).
    ///
    /// `DelegatedSurface(Cmux)`는 cmux 네이티브 pill JSON(`"cmux-status"`)을, 그 외 백엔드는
    /// 기존 SGR 한 줄(`"oneline"`)을 요청한다. understatus가 이 값으로 출력 모드를 분기한다.
    fn surface_format(self) -> &'static str {
        match self {
            StatusBackend::DelegatedSurface(SurfaceKind::Cmux) => "cmux-status",
            _ => "oneline",
        }
    }
}

/// [`StatusBackend::DelegatedSurface`]가 위임할 외부 surface 종류.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SurfaceKind {
    /// cmux 별도 surface(open_cmux_split/send_cmux_attach 경로).
    Cmux,
    /// 진짜 tmux status-line(set-option status-left/right + #(cmd)).
    Tmux,
}

/// [`select_status_backend`]의 순수 입력. 환경 신호 스냅샷(side-effect 없이 테스트가 구성).
#[derive(Debug, Clone, Copy)]
struct StatusEnvSnapshot {
    /// TTY + rows>1 + cols>0. false면 무조건 Disabled.
    terminal_capable: bool,
    /// LTERM_NO_STATUS=1 또는 LTERM_STATUS=0로 강제 off([`status_bar_disabled_by_env`]).
    forced_off: bool,
    /// cmux 환경. 별도 surface가 에이전트와도 안전.
    /// 배선 TODO: 신호원 `tmux_compat::inside_cmux`는 현재 **private** — 배선 시 `pub(crate)`
    /// 승격(또는 래퍼)이 필요하다.
    inside_cmux: bool,
    /// 진짜 tmux 내부($TMUX).
    /// 배선 TODO: lterm은 tmux-compat 호스트로서 자식에 `TMUX`를 **스스로 export**하므로
    /// (`tmux_compat.rs`), 순진하게 `$TMUX` 존재로 채우면 lterm-as-tmux 세션이
    /// DelegatedSurface(Tmux)로 **오분류**된다. 배선 시 lterm 고유 마커로 self-provided TMUX를
    /// 식별해 `real_tmux=false`로 둬야 한다(순수 함수 `select_status_backend`는 이 신호를 신뢰만 함).
    real_tmux: bool,
    /// iTerm2 식별(TERM_PROGRAM/LC_TERMINAL에 iterm).
    is_iterm: bool,
    /// iTerm 네이티브 chrome 위임 opt-in(예: LTERM_STATUS_ITERM=1). 명시 설정시에만 NativeChrome.
    iterm_native_optin: bool,
}

/// 환경/정책으로 가장 안전한 status 백엔드를 고른다(순수 함수, side-effect 없음).
///
/// 우선순위는 "자원 경쟁 없는 분리 렌더"부터 "scroll-region 공유 best-effort"까지 내림차순:
/// 1) env 강제 off / 터미널·기하 미충족 → `Disabled`.
/// 2) `ForceRow`(사용자가 in-terminal row를 명시 강제) → `DecstbmOverlay`(위임보다 우선).
/// 3) cmux → `DelegatedSurface(Cmux)`. 별 surface라 에이전트와도 무손상 → RowOff 검사보다 먼저.
/// 4) 진짜 tmux → `DelegatedSurface(Tmux)`. status-line이 행을 전담.
/// 5) iTerm2 + opt-in → `NativeChrome`. 셀 그리드 밖 안전(명시 opt-in은 사용자 의도이므로
///    에이전트(RowOff)보다 먼저 — opt-in 했다면 에이전트 status도 네이티브로 보고 싶다는 뜻).
/// 6) `RowOff`(에이전트) → `TitleCueDelegation`. 분리형 row 미표시 + 타이틀/자체 statusline 위임.
/// 7) 그 외(`RowAuto`, plain/iTerm-no-optin) → `DecstbmOverlay` best-effort.
///
/// 주: 3)에서 cmux는 RowAuto(셸) 세션도 별 surface로 위임한다(배치 일관성). 셸은 에이전트
/// 충돌이 없어 in-pane DECSTBM도 안전하므로, 셸에 한해 in-pane row를 원하면 3)을 6) 뒤로
/// 옮기면 된다 — 이 PoC는 그 결정을 테스트로 못박아 리뷰 가능하게 한다.
fn select_status_backend(policy: StatusPresencePolicy, env: &StatusEnvSnapshot) -> StatusBackend {
    if env.forced_off || !env.terminal_capable {
        return StatusBackend::Disabled;
    }
    if policy == StatusPresencePolicy::ForceRow {
        return StatusBackend::DecstbmOverlay;
    }
    if env.inside_cmux {
        return StatusBackend::DelegatedSurface(SurfaceKind::Cmux);
    }
    if env.real_tmux {
        return StatusBackend::DelegatedSurface(SurfaceKind::Tmux);
    }
    if env.is_iterm && env.iterm_native_optin {
        return StatusBackend::NativeChrome;
    }
    if policy == StatusPresencePolicy::RowOff {
        return StatusBackend::TitleCueDelegation;
    }
    StatusBackend::DecstbmOverlay
}

/// in-grid DECSTBM row를 예약할지 결정한다(순수 함수, 설계 §4.1).
///
/// `sink_enabled`(off-grid cmux pill)가 켜져 있지 않고, backend가 Disabled가 아니며,
/// 정책이 행을 원할 때(`requests_row()`) true.
///
/// 기존 `reserves_in_grid_row() && requests_row()` 로직과의 차이: cmux 셸 세션처럼
/// `sink_enabled=false` 이면서 backend==`DelegatedSurface(Cmux)`인 경우(LTERM_STATUS_COMMAND
/// 미설정)도 in-grid DECSTBM 행을 보존한다. pill이 실제 활성일 때(`sink_enabled=true`)만
/// off-grid로 전환되므로 R8 상호배타(sink_enabled ⟹ in_grid==false)는 `!sink_enabled` 항으로
/// 구조적으로 유지된다.
fn compute_in_grid(
    backend: StatusBackend,
    policy: StatusPresencePolicy,
    sink_enabled: bool,
) -> bool {
    !sink_enabled && backend != StatusBackend::Disabled && policy.requests_row()
}

/// off-grid cmux pill sink를 켤지 결정한다(순수 함수, 설계 §4.1).
///
/// 조건: backend==`DelegatedSurface(Cmux)` + 콘텐츠 명령 구성됨(`command_configured` =
/// LTERM_STATUS_COMMAND 설정) + 명시적 비활성 아님(`!explicit_no_status`, 즉 `--no-status` 아님).
///
/// `requests_row()`에 종속되지 않으므로 codex 같은 agent 세션(RowOff)에서도 켜진다 — 이것이
/// R3 BLOCKER 교정의 핵심이다. sink_enabled=true면 `compute_in_grid`가 `!sink_enabled` 항으로
/// in_grid=false를 보장하므로 DECSTBM+pill 이중 렌더(R8)는 구조적으로 차단된다.
fn compute_sink_enabled(
    backend: StatusBackend,
    command_configured: bool,
    explicit_no_status: bool,
) -> bool {
    matches!(backend, StatusBackend::DelegatedSurface(SurfaceKind::Cmux))
        && command_configured
        && !explicit_no_status
}

/// `$TMUX`의 socket 필드가 lterm self-provided TMUX인지 판정한다(순수, env 비의존).
///
/// lterm은 tmux-compat 호스트로서 자식에 `TMUX={compat_socket},{pid},0`
/// (`server::fake_tmux_value`)과 `LTERM_SOCKET={lterm_socket}`(tmux_compat.rs)을 함께 export한다.
/// 최신 lterm의 compat socket은 의도적으로 listen하지 않는 fast-fail 경로지만, 과거
/// lterm은 live daemon socket을 `$TMUX`에 넣었으므로 둘 다 self-provided로 인정한다.
///
/// # 인자
/// - `tmux_socket_field`: `$TMUX`를 `,`로 가른 첫 필드(socket 경로).
/// - `lterm_socket_env`: legacy self-detection용 `LTERM_SOCKET` 값(있으면).
/// - `lterm_socket_path`: legacy self-detection용 `paths::socket_path()` 결과 문자열(있으면).
/// - `tmux_compat_socket_path`: 최신 self-detection용 compat-only socket 문자열(있으면).
///
/// # 반환
/// socket 필드가 lterm live socket(legacy) 또는 compat socket(현재)과 일치하면 `true`.
/// 빈 필드는 판정 불가로 `false`.
fn is_self_provided_tmux(
    tmux_socket_field: &str,
    lterm_socket_env: Option<&str>,
    lterm_socket_path: Option<&str>,
    tmux_compat_socket_path: Option<&str>,
) -> bool {
    if tmux_socket_field.is_empty() {
        return false;
    }
    lterm_socket_env == Some(tmux_socket_field)
        || lterm_socket_path == Some(tmux_socket_field)
        || tmux_compat_socket_path == Some(tmux_socket_field)
}

/// 진짜 외부 tmux 안에서 실행 중인지 판정한다(lterm self-provided TMUX는 제외).
///
/// 순진한 `$TMUX` 존재 검사는 lterm-as-tmux 세션을 진짜 tmux로 **오분류**한다(lterm이 자식에 TMUX를
/// 스스로 export하므로). [`is_self_provided_tmux`]로 lterm 소켓 일치를 걸러 오분류를 막는다.
fn detect_real_tmux() -> bool {
    let Some(tmux) = std::env::var_os("TMUX") else {
        return false;
    };
    let tmux = tmux.to_string_lossy();
    let socket_field = tmux.split(',').next().unwrap_or("");
    if socket_field.is_empty() {
        return false;
    }
    let lterm_socket_env = std::env::var("LTERM_SOCKET").ok();
    let lterm_socket_path = paths::socket_path().ok().map(|p| p.display().to_string());
    let tmux_compat_socket_path = paths::tmux_compat_socket_path()
        .ok()
        .map(|p| p.display().to_string());
    !is_self_provided_tmux(
        socket_field,
        lterm_socket_env.as_deref(),
        lterm_socket_path.as_deref(),
        tmux_compat_socket_path.as_deref(),
    )
}

/// iTerm2 터미널인지 판정한다(`TERM_PROGRAM`/`LC_TERMINAL`에 "iterm" 포함).
fn detect_is_iterm() -> bool {
    let identity = [
        std::env::var("TERM_PROGRAM").unwrap_or_default(),
        std::env::var("LC_TERMINAL").unwrap_or_default(),
    ]
    .join(" ")
    .to_ascii_lowercase();
    identity.contains("iterm")
}

/// 실제 환경에서 [`StatusEnvSnapshot`]을 수집한다([`select_status_backend`] 입력).
///
/// `terminal_capable`는 이전 `status_bar_supported`가 쓰던 geometry/TTY 검사와 동일 신호이되 `show_status`
/// (정책)는 제외한다 — 정책은 [`select_status_backend`]에 별도 전달되기 때문이다.
fn gather_status_env_snapshot() -> StatusEnvSnapshot {
    let (cols, rows) = terminal_size();
    let terminal_capable =
        rows > 1 && cols > 0 && std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    StatusEnvSnapshot {
        terminal_capable,
        forced_off: status_bar_disabled_by_env(),
        inside_cmux: crate::tmux_compat::inside_cmux(),
        real_tmux: detect_real_tmux(),
        is_iterm: detect_is_iterm(),
        iterm_native_optin: env_flag_enabled("LTERM_STATUS_ITERM"),
    }
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
    if no_color_requested() {
        return StatusStyle::Minimal;
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

fn no_color_requested() -> bool {
    std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty())
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
    /// Whether status redraws may use xterm's SGR stack controls (`CSI # {` /
    /// `CSI # }`) to preserve the PTY application's current rendition around
    /// lterm's own `SGR 0` and theme writes. Detected conservatively once at
    /// attach start and overridable with `LTERM_STATUS_SGR_STACK`.
    preserve_sgr_stack: bool,
    /// command-backed status의 최신 출력(이미 `sanitize_status_command_line`으로
    /// 살균된 단일행, **폭 미절단**). `None`이면 기존 `format_status_line` fallback을
    /// 사용한다. `apply_pending_status_command`로 갱신된다.
    command_line: Option<String>,
    /// command-backed status가 자체 ANSI 색을 쓰도록 허용할지 여부(config의 allow_color).
    /// true면 draw에서 테마 bg를 입히지 않고 reset으로 시작해 understatus 색이 살게 한다.
    /// false면 plain fallback과 동일하게 테마 bg를 적용한다.
    command_allow_color: bool,
    /// PTY 출력 스트림에서 추적한 터미널 상태(커서 visibility 등). status 행 repaint 시
    /// 커서를 잠깐 숨겼다가 PTY 앱이 마지막으로 설정한 visibility로 복원하기 위해 참조한다.
    /// `None`이면(테스트/미배선) 커서가 보이는 상태(true)로 가정한다.
    terminal_state: Option<Arc<AltScreenState>>,
    /// 직전 `refresh`에서 실제로 화면에 쓴 "본문"(reserve seq + 행 draw 페이로드, 단 커서
    /// visibility 토글은 제외). content-dedup 키로 쓴다. codex 같은 메인버퍼 TUI는 idle에서도
    /// 스피너를 ~4회/초 출력해 status_dirty를 계속 set하지만, 그릴 본문이 직전과 동일하면
    /// reserve/draw/커서 envelope를 통째로 생략해 4Hz 커서 건드림(=깜빡임)을 없앤다.
    /// `None`이면 아직 한 번도 그리지 않은 상태라 무조건 그린다.
    last_body: Option<String>,
    /// 본문이 직전과 동일해도 반드시 redraw해야 하는 경우(화면 손상, alt-screen 복귀,
    /// resume 등)에 한 번 true로 세팅한다. dedup 키와 무관하게 한 번 강제로 그린 뒤
    /// `refresh` 내부에서 false로 리셋한다. enter 시점 첫 draw도 기본 true로 커버한다.
    /// 이 플래그는 reserve(DECSTBM scroll-region 재설정)를 포함한 전체 redraw를 강제한다.
    force_redraw: bool,
    /// 주기적 백스톱(STATUS_HEARTBEAT_FORCED)이 status 내용만 다시 그리도록 한 번 true로
    /// 세팅한다. `force_redraw`와 달리 reserve(DECSTBM scroll-region 재설정)를 포함하지 않아
    /// codex 등이 쓰는 자체 scroll-region을 덮어쓰지 않는다. 화면 손상 없이 idle에서 status
    /// 행만 추적-청소+커서 숨김 envelope로 다시 그릴 때 쓴다. `refresh` 내부에서 false로
    /// 리셋한다. `force_redraw`가 함께 참이면 reserve 포함 redraw가 우선한다.
    force_content_redraw: bool,
}

impl StatusBar {
    fn enter(
        info: Option<&SessionInfo>,
        style: Option<StatusStyle>,
        terminal_state: Option<Arc<AltScreenState>>,
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
            preserve_sgr_stack: status_sgr_stack_supported(),
            // command-backed status는 attach에서 config가 있을 때만 활성화된다.
            // 기본은 비활성(None)이라 LTERM_STATUS_COMMAND 미설정 시 기존 동작과 동일하다.
            command_line: None,
            command_allow_color: false,
            terminal_state,
            // 첫 draw는 last_body가 없어 어차피 dedup 미스로 그려지지만, force_redraw=true로도
            // 명시해 enter 시점 reserve+draw가 확실히 화면에 나가게 한다.
            last_body: None,
            force_redraw: true,
            // 백스톱 전용 플래그. enter 시점에는 force_redraw가 reserve 포함 첫 draw를 보장한다.
            force_content_redraw: false,
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

    /// status 행을 다시 그린다. content-dedup 키는 행 draw 페이로드(`build_draw_body`)만으로
    /// 구성한다 — reserve(DECSTBM scroll-region 명령)는 매번 동일해 내용 변화 판정에 무관하므로
    /// 키에서 제외한다. draw 본문이 직전과 같고 `force_redraw`/`force_content_redraw` 둘 다
    /// 아니면 reserve/draw/커서를 일절 건드리지 않고 `Ok(false)`를 반환한다. codex 같은 메인버퍼
    /// TUI는 idle에서도 스피너를 ~4회/초 출력해 status_dirty를 계속 set하지만, 본문이 같으면 이
    /// dedup이 4Hz 커서 건드림(=깜빡임)을 없앤다.
    ///
    /// 실제로 그릴 때는 두 경로가 있다:
    /// - `write_with_reserve`(force_redraw 또는 내용 변경): reserve(scroll-region 재설정) +
    ///   draw + 커서 envelope. damage/alt-exit/resume/enter/내용변경에서 쓴다.
    /// - `write_content_only`(force_content_redraw, 백스톱): reserve 없이 draw + 커서 envelope.
    ///   codex 자체 scroll-region을 덮어쓰지 않도록 DECSTBM을 내보내지 않는다.
    ///
    /// 둘 다 참이면 reserve 포함 경로가 우선한다. 모든 경로는 reserve(또는 "")+draw+커서를 단일
    /// write_all로 묶어 `\x1b[?25l`(숨김) → 본문 → 추적 visibility 복원 1회로 내보내 중간 깜빡임을
    /// 방지한다. 반환값은 실제로 화면에 썼는지(drew) 여부 — 호출자는 true일 때만 flush한다.
    fn refresh(&mut self, stdout: &mut impl Write) -> Result<bool> {
        let (cols, rows) = terminal_size();
        // dedup 키는 draw 본문만으로 구성한다. reserve는 매번 동일한 scroll-region 명령이라
        // 내용 변화 판정에 불필요하므로 키에서 제외한다. 커서 visibility 토글도 제외된 순수
        // 본문이라 커서가 보임↔숨김으로만 바뀐 경우는 키가 동일해 redraw하지 않는다(의도).
        let draw_body = self.build_draw_body(cols, rows);
        if draw_body.is_empty() {
            // rows<=1/cols<=1 등으로 그릴 본문이 없으면 아무것도 쓰지 않는다. force 플래그와
            // last_body는 유지해, 터미널이 다시 커져 그릴 수 있게 되면 그때 반영되도록 한다.
            return Ok(false);
        }
        let changed = self.last_body.as_deref() != Some(draw_body.as_str());
        // damage/alt-exit/resume/enter/내용변경 → reserve(scroll-region 재설정) 포함.
        let write_with_reserve = self.force_redraw || changed;
        // 백스톱 → reserve 없이 내용만(codex scroll-region 보존).
        let write_content_only = self.force_content_redraw;
        if !write_with_reserve && !write_content_only {
            // idle 동일 내용 + 두 플래그 모두 false면 reserve/draw/커서 전부 생략한다.
            return Ok(false);
        }
        // 둘 다 참이면 reserve 포함이 우선한다. content-only 경로는 reserve를 빈 문자열로 둔다.
        let reserve = if write_with_reserve {
            self.build_reserve_body(rows)
        } else {
            String::new()
        };
        // 페이로드 맨 앞에서 커서를 숨기고(`\x1b[?25l`), 맨 끝에서 PTY 앱이 마지막에 설정한
        // visibility로 복원한다. reserve+draw+커서 envelope를 합쳐 단일 write_all로 내보낸다.
        let cursor_restore = self.cursor_restore_suffix();
        let payload = format!("\x1b[?25l{reserve}{draw_body}{cursor_restore}");
        stdout
            .write_all(payload.as_bytes())
            .context("refresh lterm status bar")?;
        // draw 본문이 실제로 나갔으므로 drawn_status_rows bookkeeping을 갱신한다.
        // build_draw_body는 non-mutating이라 그 사이 drawn_status_rows가 바뀌지 않았고,
        // 따라서 여기서 다시 계산한 rows_to_clear는 본문 빌드 때와 동일하다.
        let rows_to_clear = self.visible_previous_status_rows(rows);
        self.remember_status_row(rows, &rows_to_clear);
        self.last_body = Some(draw_body);
        self.force_redraw = false;
        self.force_content_redraw = false;
        Ok(true)
    }

    fn update_info(&mut self, info: &SessionInfo) -> bool {
        let session_name = sanitize::terminal_text(&info.name);
        let pane_id = sanitize::terminal_text(&info.pane_id);
        if self.session_name == session_name && self.pane_id == pane_id {
            return false;
        }
        self.session_name = session_name;
        self.pane_id = pane_id;
        true
    }

    /// PTY 앱이 노출 중인 커서 visibility. `terminal_state`가 미배선(None)이면 커서가
    /// 보이는 상태(true)로 가정한다 — 즉 repaint 후 `\x1b[?25h`로 복원해 기존 동작
    /// (커서를 끄지 않던 시절)을 회귀 없이 유지한다.
    fn cursor_visible(&self) -> bool {
        self.terminal_state
            .as_ref()
            .is_none_or(|state| state.cursor_visible.load(Ordering::Relaxed))
    }

    /// repaint 시 커서가 status 행으로 튀어 깜빡이지 않도록, 페이로드 끝에서 PTY 앱의
    /// 추적된 visibility로 커서를 복원하는 시퀀스. 보임이면 `\x1b[?25h`, 숨김이면
    /// `\x1b[?25l`. 페이로드 앞에는 항상 `\x1b[?25l`(숨김)을 붙여 cursor save/move/draw/
    /// restore 동안 커서가 보이지 않게 한다.
    fn cursor_restore_suffix(&self) -> &'static str {
        if self.cursor_visible() {
            "\x1b[?25h"
        } else {
            "\x1b[?25l"
        }
    }

    /// reserve의 "본문"(커서 visibility 토글 제외): `\x1b7\x1b[1;{n}r\x1b8`. style이 없거나
    /// rows<=1이면 빈 문자열을 반환한다. content-dedup 키 구성과 envelope 1회 wrap에 쓰인다.
    /// DECSTBM(`\x1b[1;{n}r`)은 커서를 home으로 이동시키지만 save/restore(`\x1b7`/`\x1b8`)로
    /// 위치는 복원된다. 커서 숨김/복원은 호출자(`reserve_terminal_area`/`refresh`)가 감싼다.
    fn build_reserve_body(&self, rows: u16) -> String {
        if self.style.is_none() || rows <= 1 {
            return String::new();
        }
        let scroll_bottom = rows - 1;
        format!("\x1b7\x1b[1;{scroll_bottom}r\x1b8")
    }

    fn reserve_terminal_area(&self, stdout: &mut impl Write, rows: u16) -> Result<()> {
        let body = self.build_reserve_body(rows);
        if body.is_empty() {
            return Ok(());
        }
        // 그 사이 커서가 보이면 codex처럼 메인 버퍼 입력 커서를 노출하는 TUI에서 커서가
        // 잠깐 튄다. 숨김→reserve→추적 상태로 복원으로 감싼다.
        let cursor_restore = self.cursor_restore_suffix();
        write!(stdout, "\x1b[?25l{body}{cursor_restore}")
            .context("reserve lterm status bar row")?;
        Ok(())
    }

    fn draw(&mut self, stdout: &mut impl Write) -> Result<()> {
        let (cols, rows) = terminal_size();
        self.draw_at_size(stdout, cols, rows)
    }

    /// status 행 draw의 "본문"(커서 visibility 토글 제외)을 만든다. 페이로드 형태는
    /// `\x1b7{sgr_push}{이전행 clear}*\x1b[{rows};1H{current_row_clear}{sgr}{line}\x1b[0m\x1b[K{sgr_pop}\x1b8`.
    /// 가드(rows<=1/cols<=1/style None)에 걸리면 빈 문자열을 반환한다. 순수 함수라
    /// `remember_status_row` 같은 side-effect는 호출하지 않는다 — content-dedup 키 구성과
    /// envelope 1회 wrap에 안전하게 재사용하기 위함이다.
    fn build_draw_body(&self, cols: u16, rows: u16) -> String {
        // cols<=1이면 마지막 칸을 비우고도 그릴 공간이 없어 autowrap 회피 의미가 사라진다.
        if rows <= 1 || cols <= 1 {
            return String::new();
        }
        // 마지막 칸까지 채우면 일부 모바일 터미널(예: Termius)에서 deferred-wrap 미구현으로
        // 즉시 스크롤이 발생해 status line이 본문으로 밀려 올라간다. cols-1만 그린다.
        let safe_width = cols.saturating_sub(1).max(1);
        // status row의 콘텐츠 소스와 테마 bg 적용 여부를 결정한다.
        // command_line이 Some(비어있지 않음)이면 command-backed 모드로, 살균된 출력을
        // safe_width로 ANSI-aware 절단해 쓴다. allow_color면 understatus 자체 색이 살도록
        // 테마 bg를 입히지 않는다. None이거나 빈 문자열이면 기존 format_status_line fallback.
        let (line, use_theme_bg) = match self.command_line.as_deref() {
            Some(cmd) if !cmd.is_empty() => (
                sanitize::truncate_status_line_ansi(cmd, safe_width),
                !self.command_allow_color,
            ),
            _ => (
                format_status_line(&self.session_name, &self.pane_id, safe_width),
                true,
            ),
        };
        // \x1b[2K로 행을 먼저 비워야 옛 상태(긴 세션명 잔재)가 남지 않는다.
        // fallback/plain 모드는 \x1b[0m로 시작해 이전 PTY rendition(bold/italic/inverse 등)이
        // status line으로 새는 것을 차단한다. Full은 theme enum에서 고른 고정 SGR만
        // 적용하므로 사용자 입력 escape sequence가 status row에 주입되지 않는다.
        // (bold(1)은 두 모드 모두에서 사용하지 않는다: bold+black을 흰색으로 렌더하는 터미널이 있다.)
        //
        // command-backed + allow_color 모드에서는 테마 bg를 입히지 않고 reset(\x1b[0m)으로
        // 시작해 understatus가 emit한 SGR 색이 그대로 보이게 한다. 살균 단계에서 위험한
        // escape는 이미 제거되고 SGR 끝에 reset이 붙으므로 색 누수는 차단된다.
        let style = match self.style {
            Some(style) => style,
            None => return String::new(),
        };
        let sgr = if use_theme_bg { style.sgr() } else { "\x1b[0m" };
        // SGR + cursor save/restore + 본문을 단일 String 으로 buffer 후 write_all 1회 호출.
        // 이는 strict atomicity 보장은 아니다 (write_all은 내부적으로 여러 syscall 가능).
        // TTY/PTY는 POSIX PIPE_BUF atomicity 적용 대상이 아니므로 partial-write 가능성 잔존.
        // 그러나 write! 매크로는 placeholder 마다 write_fmt 가 분할 syscall을 일으켜 SGR sequence
        // 중간이 다른 출력과 interleave 될 위험이 컸다 — buffered write 로 그 위험을 줄인다.
        // CSI # { / CSI # } is xterm's SGR stack. DECSC/DECRC already saves
        // cursor position, but not every terminal restores rendition there; wrap
        // every host-side reset/theme write so an agent TUI keeps its foreground,
        // background, and truecolor state after lterm repaints the reserved row.
        // Terminals that cannot safely handle this private CSI can disable it
        // with LTERM_STATUS_SGR_STACK=0; unknown/dumb terminals skip it by
        // default.
        let rows_to_clear = self.visible_previous_status_rows(rows);
        let sgr_push = if self.preserve_sgr_stack {
            "\x1b[#{"
        } else {
            ""
        };
        let sgr_pop = if self.preserve_sgr_stack {
            "\x1b[#}"
        } else {
            ""
        };
        let mut body = format!("\x1b7{sgr_push}");
        for previous_row in &rows_to_clear {
            // 새 terminal height 에서 예전 status row 가 보이는 경우 먼저 default 배경으로
            // 지운다. 그렇지 않으면 pane grow / mobile rotate 뒤 예전 status row 가 본문
            // 중간에 남아 "statusline 여러 개"처럼 보인다.
            body.push_str(&format!("\x1b[{previous_row};1H\x1b[0m\x1b[2K"));
        }
        let current_row_clear = if self.drawn_status_rows.contains(&rows) {
            ""
        } else {
            "\x1b[2K"
        };
        body.push_str(&format!(
            "\x1b[{rows};1H{current_row_clear}{sgr}{line}\x1b[0m\x1b[K{sgr_pop}\x1b8"
        ));
        body
    }

    fn draw_at_size(&mut self, stdout: &mut impl Write, cols: u16, rows: u16) -> Result<()> {
        let body = self.build_draw_body(cols, rows);
        if body.is_empty() {
            return Ok(());
        }
        // idle 상태에서 status 행은 ~250ms마다(STATUS_HEARTBEAT) repaint된다. cursor
        // save(`\x1b7`)→status 행으로 이동→draw→restore(`\x1b8`)를 하는 동안 커서가 보이면
        // codex처럼 메인 버퍼 입력 커서를 노출하는 TUI에서 커서가 status 행으로 튀며
        // 깜빡인다. 페이로드 맨 앞에서 커서를 숨기고(`\x1b[?25l`), 맨 끝에서 PTY 앱이 마지막에
        // 설정한 visibility로 복원한다. 단일 write_all로 나가므로 터미널이 한 번에 처리해
        // 중간 상태(커서가 status 행에 보이는 순간)가 사용자에게 노출되지 않는다.
        let cursor_restore = self.cursor_restore_suffix();
        let payload = format!("\x1b[?25l{body}{cursor_restore}");
        stdout
            .write_all(payload.as_bytes())
            .context("draw lterm status bar")?;
        let rows_to_clear = self.visible_previous_status_rows(rows);
        self.remember_status_row(rows, &rows_to_clear);
        Ok(())
    }

    fn restore(&self, stdout: &mut impl Write) -> Result<()> {
        if self.style.is_none() {
            return Ok(());
        }
        let (_, rows) = terminal_size();
        let rows_to_clear = self.visible_previous_status_rows(rows);
        let sgr_push = if self.preserve_sgr_stack {
            "\x1b[#{"
        } else {
            ""
        };
        let sgr_pop = if self.preserve_sgr_stack {
            "\x1b[#}"
        } else {
            ""
        };
        let mut payload = format!("\x1b7{sgr_push}\x1b[r");
        for previous_row in &rows_to_clear {
            payload.push_str(&format!("\x1b[{previous_row};1H\x1b[0m\x1b[2K"));
        }
        payload.push_str(&format!("\x1b[{rows};1H\x1b[0m\x1b[2K{sgr_pop}\x1b8"));
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

/// PTY가 alternate screen buffer에 진입했는지, 그리고 PTY 앱이 커서를 보이도록
/// 두고 있는지를 추적한다.
///
/// `active`: true 동안에는 host-side status bar 그리기를 일시 중단해 vim/htop 같은
/// alt-screen 앱과 화면 충돌을 피한다. PTY 출력 스트림에서 `\x1b[?1049h/47h/1047h`
/// (enter) 와 대응하는 `l` (exit)을 관찰한다.
///
/// `cursor_visible`: PTY 앱이 마지막으로 설정한 DECTCEM(`\x1b[?25h` 보이기 /
/// `\x1b[?25l` 숨기기) 상태. status 행 repaint 시 커서를 잠깐 숨겼다가 이 추적값으로
/// 복원하기 위해 사용한다. codex처럼 메인 버퍼에서 입력 커서를 노출하는 TUI는
/// 커서가 보이는 상태인데, status repaint가 커서 저장/이동/복원을 하면서 커서를
/// 숨기지 않으면 4Hz로 커서가 status 행으로 튀며 깜빡인다. 커서는 기본 표시
/// 상태이므로 초기값은 **true** 다.
///
/// `AtomicBool` + `Ordering::Relaxed` 사용 근거: 현재 단일 attach 스레드에서
/// observe(write)와 attach 루프(read)가 모두 일어나므로 ordering 요구는 없다.
/// `Arc`는 향후 PTY reader/observer 분리를 대비한 형태이며, 그 때에는 publishing
/// data가 동반되지 않으면 Relaxed로 충분하다.
struct AltScreenState {
    active: AtomicBool,
    /// PTY 앱이 노출 중인 커서 visibility(DECTCEM). 기본 true(보임).
    cursor_visible: AtomicBool,
}

impl Default for AltScreenState {
    fn default() -> Self {
        Self {
            active: AtomicBool::new(false),
            // 커서는 기본적으로 보이는 상태이므로 true 로 시작한다.
            cursor_visible: AtomicBool::new(true),
        }
    }
}

struct TerminalOutputTracker {
    restore_state: Arc<KeyboardProtocolRestoreState>,
    alt_screen: Arc<AltScreenState>,
    status_scroll_bottom: Option<u16>,
    tail: Vec<u8>,
}

#[derive(Default)]
struct TerminalOutputEffects {
    status_area_dirty: bool,
}

impl TerminalOutputTracker {
    fn new(
        restore_state: Arc<KeyboardProtocolRestoreState>,
        alt_screen: Arc<AltScreenState>,
        status_scroll_bottom: Option<u16>,
    ) -> Self {
        Self {
            restore_state,
            alt_screen,
            status_scroll_bottom,
            tail: Vec::new(),
        }
    }

    fn set_status_scroll_bottom(&mut self, status_scroll_bottom: Option<u16>) {
        self.status_scroll_bottom = status_scroll_bottom;
    }

    fn observe(&mut self, bytes: &[u8]) -> TerminalOutputEffects {
        const TAIL_LIMIT: usize = 64;
        let mut effects = TerminalOutputEffects::default();
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
            if self.status_scroll_bottom.is_some() {
                effects.status_area_dirty |= observe_status_area_damage_sequences_after(
                    &boundary,
                    old_tail.len(),
                    self.status_scroll_bottom,
                );
            }
        }

        observe_keyboard_protocol_sequences(bytes, &self.restore_state);
        observe_alt_screen_sequences(bytes, &self.alt_screen);
        if self.status_scroll_bottom.is_some() {
            effects.status_area_dirty |=
                observe_status_area_damage_sequences(bytes, self.status_scroll_bottom);
        }

        if bytes.len() >= TAIL_LIMIT {
            self.tail
                .extend_from_slice(&bytes[bytes.len() - TAIL_LIMIT..]);
        } else {
            let old_keep = old_tail.len().min(TAIL_LIMIT - bytes.len());
            self.tail
                .extend_from_slice(&old_tail[old_tail.len() - old_keep..]);
            self.tail.extend_from_slice(bytes);
        }
        effects
    }
}

fn observe_keyboard_protocol_sequences(bytes: &[u8], state: &KeyboardProtocolRestoreState) {
    observe_keyboard_protocol_sequences_after(bytes, 0, state);
}

fn observe_status_area_damage_sequences(bytes: &[u8], status_scroll_bottom: Option<u16>) -> bool {
    observe_status_area_damage_sequences_after(bytes, 0, status_scroll_bottom)
}

fn observe_status_area_damage_sequences_after(
    bytes: &[u8],
    min_final_index: usize,
    status_scroll_bottom: Option<u16>,
) -> bool {
    let mut i = 0;
    while i < bytes.len() {
        if min_final_index > 0 && i >= min_final_index {
            break;
        }
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'c' {
            if i + 1 >= min_final_index {
                return true;
            }
            i += 2;
            continue;
        }

        let params_start = if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            i + 2
        } else {
            i += 1;
            continue;
        };

        let scan_end = bytes.len().min(i + 64);
        let mut j = params_start;
        while j < scan_end {
            let byte = bytes[j];
            if (0x40..=0x7e).contains(&byte) {
                if j >= min_final_index
                    && csi_sequence_can_damage_status_area(
                        byte,
                        &bytes[params_start..j],
                        status_scroll_bottom,
                    )
                {
                    return true;
                }
                break;
            }
            j += 1;
        }
        i += 1;
    }
    false
}

fn csi_sequence_can_damage_status_area(
    final_byte: u8,
    params: &[u8],
    status_scroll_bottom: Option<u16>,
) -> bool {
    match final_byte {
        // ED variants are conservative damage signals. Even `CSI 1J` can
        // clear the host status row if the PTY first moves the cursor onto
        // the bottom row; without full cursor tracking, repaint immediately.
        b'J' => csi_first_numeric_param(params).is_none_or(|param| param <= 3),
        // DECSTBM reset restores the scroll region to the full surface; that
        // can let subsequent PTY output overwrite the host status row. Parameterized
        // regions are damaging only when their bottom includes the reserved status
        // row; the common PTY-owned `CSI 1;<body_rows>r` prompt redraw is benign.
        b'r' => csi_decstbm_touches_status_area(params, status_scroll_bottom),
        _ => false,
    }
}

fn csi_first_numeric_param(params: &[u8]) -> Option<u16> {
    if params.is_empty() {
        return Some(0);
    }
    let mut value: u16 = 0;
    let mut seen_digit = false;
    for &byte in params {
        match byte {
            b'0'..=b'9' => {
                seen_digit = true;
                value = value
                    .saturating_mul(10)
                    .saturating_add(u16::from(byte - b'0'));
            }
            b';' => return Some(if seen_digit { value } else { 0 }),
            _ => return None,
        }
    }
    Some(if seen_digit { value } else { 0 })
}

fn csi_decstbm_touches_status_area(params: &[u8], status_scroll_bottom: Option<u16>) -> bool {
    if params.is_empty() {
        return true;
    }
    let Some(status_scroll_bottom) = status_scroll_bottom else {
        return true;
    };
    let Some(params) = csi_numeric_params(params) else {
        return true;
    };
    if params.is_empty() || params.iter().all(|param| param.unwrap_or(0) == 0) {
        return true;
    }
    let Some(Some(bottom)) = params.get(1) else {
        return true;
    };
    if *bottom == 0 {
        return true;
    }
    *bottom > status_scroll_bottom
}

fn csi_numeric_params(params: &[u8]) -> Option<Vec<Option<u16>>> {
    let mut out = Vec::new();
    let mut seen_digit = false;
    let mut current: u16 = 0;
    for &byte in params {
        match byte {
            b'0'..=b'9' => {
                seen_digit = true;
                current = current
                    .saturating_mul(10)
                    .saturating_add(u16::from(byte - b'0'));
            }
            b';' => {
                out.push(seen_digit.then_some(current));
                seen_digit = false;
                current = 0;
            }
            _ => return None,
        }
    }
    out.push(seen_digit.then_some(current));
    Some(out)
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
/// PTY 출력에서 alternate screen buffer 진입/종료 시퀀스(`CSI ? 47 / 1047 / 1049 h|l`)와
/// 커서 visibility(DECTCEM, `CSI ? 25 h|l`)를 함께 관찰해 각각 `alt_screen.active`와
/// `alt_screen.cursor_visible`를 갱신한다. 둘 다 같은 `CSI ? <params> h|l` 형태라 한 번의
/// 스캔으로 처리하며, `?1049;25l`처럼 한 시퀀스에 두 mode가 묶여 오면 둘 다 갱신한다.
/// 청크 경계로 잘린 시퀀스는 호출자가 tail 버퍼를 합쳐서 다시 부르며, 그 경우
/// `min_final_index`로 이전 청크에 이미 본 종결자(`h`/`l`)를 다시 처리하지 않게 막는다.
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
                    let set = byte == b'h';
                    if alt_screen_param_matches(params) {
                        alt_screen.active.store(set, Ordering::Relaxed);
                    }
                    // DECTCEM(`?25h` 보이기 / `?25l` 숨기기) 추적. alt-screen 토글과
                    // 독립적으로 갱신하므로 `?1049;25l` 같은 그룹에서 둘 다 반영된다.
                    if dectcem_param_matches(params) {
                        alt_screen.cursor_visible.store(set, Ordering::Relaxed);
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

/// `CSI ? <params> h|l`의 params에 DECTCEM(커서 visibility) mode 25가 포함됐는지 검사한다.
/// `alt_screen_param_matches`와 동일하게 `;`(ECMA-48 parameter separator)로만 split한다.
/// `:`(subparameter separator)는 split하지 않으므로 `?25:5h`처럼 25의 subparameter가 붙은
/// 경우를 "mode 25"로 오인하지 않는다. `?1049;25l`처럼 다른 mode와 그룹으로 묶인 경우도
/// split 후 `25` 토큰을 정확히 잡아낸다.
fn dectcem_param_matches(params: &[u8]) -> bool {
    params
        .split(|byte| *byte == b';')
        .any(|param| param == b"25")
}

struct RawModeGuard {
    active: bool,
    keyboard_protocol_restore_state: Arc<KeyboardProtocolRestoreState>,
}

struct RawAttachTerminalGuards {
    raw: Option<RawModeGuard>,
    _cleanup: HostTerminalCleanupGuard,
}

impl RawAttachTerminalGuards {
    fn enter(alt_screen_state: Arc<AltScreenState>) -> Result<Self> {
        let mut cleanup = HostTerminalCleanupGuard::new(alt_screen_state);
        let raw = RawModeGuard::enter()?;
        cleanup.arm(raw.active());
        Ok(Self {
            raw: Some(raw),
            _cleanup: cleanup,
        })
    }

    fn keyboard_protocol_restore_state(&self) -> Arc<KeyboardProtocolRestoreState> {
        self.raw
            .as_ref()
            .expect("raw attach terminal guards must hold raw mode until drop")
            .keyboard_protocol_restore_state()
    }
}

impl Drop for RawAttachTerminalGuards {
    fn drop(&mut self) {
        // Explicitly drop RawModeGuard first so raw mode and tracked keyboard
        // protocol state are restored before HostTerminalCleanupGuard emits
        // scroll/cursor/SGR cleanup on the host stdout.
        drop(self.raw.take());
    }
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

    fn active(&self) -> bool {
        self.active
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

struct HostTerminalCleanupGuard {
    active: bool,
    alt_screen_state: Arc<AltScreenState>,
}

impl HostTerminalCleanupGuard {
    fn new(alt_screen_state: Arc<AltScreenState>) -> Self {
        Self {
            active: false,
            alt_screen_state,
        }
    }

    fn arm(&mut self, raw_mode_active: bool) {
        self.active = raw_mode_active && std::io::stdout().is_terminal();
    }
}

impl Drop for HostTerminalCleanupGuard {
    fn drop(&mut self) {
        if self.active {
            emit_normal_attach_terminal_cleanup(
                self.alt_screen_state.active.load(Ordering::Relaxed),
            );
        }
    }
}

fn emit_normal_attach_terminal_cleanup(alt_screen_active: bool) {
    let bytes = normal_attach_terminal_cleanup_bytes(alt_screen_active);
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(&bytes);
    let _ = stdout.flush();
}

fn normal_attach_terminal_cleanup_bytes(alt_screen_active: bool) -> Vec<u8> {
    let mut bytes = Vec::new();
    if alt_screen_active {
        // Leave alt-screen only when the raw stream actually entered it during
        // this attach. Normal exits should not blindly force an alt-buffer
        // switch for ordinary shell sessions.
        bytes.extend_from_slice(b"\x1b[?1049l\x1b[?47l\x1b[?1047l");
    }
    bytes.extend_from_slice(b"\x1b[r\x1b[?25h\x1b[?2004l\x1b[0m\r\n");
    bytes
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StdinInputState {
    Ready,
    Pending,
    InvalidFd { revents: i16 },
}

fn stdin_input_state(fd: RawFd, timeout: Duration) -> Result<StdinInputState> {
    let timeout_ms = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
    let mut pollfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        let rc = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };
        if rc > 0 {
            if pollfd.revents & libc::POLLNVAL != 0 {
                return Ok(StdinInputState::InvalidFd {
                    revents: pollfd.revents,
                });
            }
            if pollfd.revents & libc::POLLERR != 0 {
                bail!("stdin poll reported error events: {:#x}", pollfd.revents);
            }
            return Ok(if pollfd.revents & (libc::POLLIN | libc::POLLHUP) != 0 {
                StdinInputState::Ready
            } else {
                StdinInputState::Pending
            });
        }
        if rc == 0 {
            return Ok(StdinInputState::Pending);
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
        AGENT_TITLE_REFRESH, ATTACH_ACTIVE, ATTACH_OUTPUT_IDLE_TIMEOUT,
        ATTACH_RESPONSE_HEADER_LIMIT, AgentPresenceCue, AgentTitleCueRuntime, AltScreenState,
        AttachActiveGuard, AttachMode, CapabilityFileIdentity, ComposeRenderAction, DaemonStatus,
        HOST_TERMINAL_SGR_RESET, KeyboardProtocolRestoreState, MAX_EXTRACTED_URL_BYTES,
        MAX_EXTRACTED_URLS, MAX_KEYBOARD_PROTOCOL_RESTORE_POPS, MAX_TRACE_JSONL_LINE_BYTES,
        MOBILE_TRANSCRIPT_SGR_RESET, MobileTranscriptInputContext, MobileTranscriptOptions,
        NESTED_AGENT_POLL, NestedAgentDetector, NestedAgentTransition, ProcessInfo, ProcessRow,
        RECONNECT_STATE_SCHEMA_VERSION, RPC_PARSE_ERROR_PREVIEW_BYTES, ResizeTickOutcome,
        STATUS_DAMAGE_HEARTBEAT, STATUS_HEARTBEAT, STATUS_HEARTBEAT_FORCED, STATUS_PAYLOAD_CWD_CAP,
        StatusBackend, StatusBar, StatusCommandConfig, StatusEnvSnapshot, StatusPresencePolicy,
        StatusPresenceRuntimeHandle, StatusPresenceState, StatusStyle, StatusTheme, SurfaceKind,
        TerminalOutputTracker, agent_name_from_command, agent_presence_banner_enabled,
        agent_presence_cue_enabled, alt_screen_param_matches, anyhow_error_is_broken_pipe,
        apply_pending_status_command, attach_pty_rows, automatic_reconnect_candidate,
        build_process_tree_from_rows, build_status_payload, compose_commit_bytes,
        compose_display_line, compose_is_local_exit_key, compose_pop_grapheme, compose_prompt_line,
        compose_push_paste, compose_refresh_interval, compose_render_action,
        compose_sanitized_display_line, compose_should_commit, compose_tail_start,
        compose_terminal_enter_sequence, compose_terminal_leave_sequence, compute_in_grid,
        compute_sink_enabled, create_private_capability_file, current_unix_ms,
        cursor_clamp_into_scroll_region, dectcem_param_matches,
        ensure_automatic_reconnect_candidate, ensure_panic_terminal_cleanup_hook,
        ensure_trace_force_target_private, extract_search_matches, extract_urls,
        finish_attach_results, format_attach_failure_diagnosis, format_status_line,
        forward_pty_output_frame_or_detached, handle_mobile_transcript_input, handle_resize_tick,
        heartbeat_due, hex_decode, hex_encode, hex_encoded_len, instrument_protocol_error,
        interruptible_sleep, is_self_provided_tmux, join_attach_input_thread,
        keyboard_protocol_restore_bytes, likely_agent_session, matches_env_bool,
        mobile_client_detected, mobile_transcript_capture_changed, mobile_transcript_grep_query,
        nested_known_agent_present_in_processes, normal_attach_terminal_cleanup_bytes,
        observe_keyboard_protocol_sequences, panic_terminal_cleanup_bytes,
        parse_status_command_bool, parse_status_command_interval, parse_status_style,
        raw_attach_command_hint, read_attach_response_header, read_private_capability_file,
        read_reconnect_state_best_effort_from_path, read_reconnect_state_from_path,
        read_trace_jsonl_line, recent_exits_protocol_error,
        remember_reconnect_target_best_effort_at_path, reset_raw_attach_initial_sgr_if_needed,
        resolve_attach_mode, resolve_status_style, rpc_parse_error_preview,
        run_nested_agent_detection_loop, run_status_command, select_status_backend,
        should_mobile_transcript_auto, status_sgr_stack_supported, status_theme_protocol_error,
        tmux_parent_pane_protocol_error, trace_file_summary, trace_output_open_context,
        trace_summary_text, unlink_capability_path_if_identity_matches, validate_trace_replay,
        write_lterm_agent_presence_banner, write_lterm_title_cue, write_mobile_transcript_update,
        write_mobile_transcript_urls, write_numbered_search_matches,
    };
    use crate::protocol::{
        ExitEvidenceState, ExitOutcomeState, RecentSessionExit, SessionExitTrigger,
        SessionLifecycleState,
    };
    use std::io::{BufReader, Cursor, ErrorKind, Read, Write};
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::thread;
    use std::time::{Duration, Instant};

    #[test]
    fn capability_file_is_exclusive_private_and_exact_format_only() {
        let dir = tempfile::tempdir().expect("capability tempdir");
        let path = dir.path().join("input.cap");
        let mut file = create_private_capability_file(&path).expect("create capability file");
        let token = crate::protocol::CapabilityToken::new_random();
        write!(file, "lterm-input-capability-v1\n{}\n", token.as_str())
            .expect("write capability file");
        file.sync_all().expect("sync capability file");
        drop(file);
        assert_eq!(
            std::fs::metadata(&path)
                .expect("stat capability")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            read_private_capability_file(&path).expect("read capability"),
            token
        );
        assert!(create_private_capability_file(&path).is_err());

        std::fs::write(
            &path,
            format!("lterm-input-capability-v1\n{}\ntrailing", token.as_str()),
        )
        .expect("replace malformed capability contents");
        assert!(read_private_capability_file(&path).is_err());

        std::fs::write(&path, b"lterm-input-capability-v1\n").expect("truncate capability token");
        assert!(read_private_capability_file(&path).is_err());
        std::fs::write(&path, vec![b'x'; 129]).expect("write oversized capability");
        assert!(read_private_capability_file(&path).is_err());
    }

    #[test]
    fn capability_file_rejects_symlink_and_wrong_mode() {
        let dir = tempfile::tempdir().expect("capability tempdir");
        let target = dir.path().join("target");
        std::fs::write(&target, b"not a capability").expect("seed target");
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600))
            .expect("chmod target");
        let link = dir.path().join("link");
        symlink(&target, &link).expect("create symlink");
        assert!(read_private_capability_file(&link).is_err());
        assert!(create_private_capability_file(&link).is_err());

        let token = crate::protocol::CapabilityToken::new_random();
        std::fs::write(
            &target,
            format!("lterm-input-capability-v1\n{}\n", token.as_str()),
        )
        .expect("write capability");
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644))
            .expect("chmod unsafe capability");
        assert!(read_private_capability_file(&target).is_err());

        for mode in [0o4600, 0o2600, 0o1600] {
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode))
                .expect("set special capability mode");
            assert!(
                read_private_capability_file(&target).is_err(),
                "special permission mode {mode:o} must be rejected"
            );
        }

        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600))
            .expect("restore private mode");
        let hardlink = dir.path().join("hardlink");
        std::fs::hard_link(&target, &hardlink).expect("create hard link");
        assert!(read_private_capability_file(&target).is_err());
        assert!(read_private_capability_file(&hardlink).is_err());
        assert!(read_private_capability_file(dir.path()).is_err());
    }

    #[test]
    fn capability_persistence_failure_cleanup_refuses_replaced_path_leaf() {
        let dir = tempfile::tempdir().expect("capability tempdir");
        let path = dir.path().join("input.cap");
        let file = create_private_capability_file(&path).expect("create capability file");
        let metadata = file.metadata().expect("stat capability file");
        let identity = CapabilityFileIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        };
        drop(file);
        let original = dir.path().join("original.cap");
        std::fs::rename(&path, &original).expect("move original capability leaf");
        std::fs::write(&path, b"replacement must survive").expect("create replacement leaf");
        let error = unlink_capability_path_if_identity_matches(&path, identity)
            .expect_err("replacement path must not be unlinked");
        assert!(error.to_string().contains("changed"));
        assert_eq!(
            std::fs::read(&path).expect("replacement remains"),
            b"replacement must survive"
        );
    }

    #[test]
    fn capability_issue_error_cleanup_unlinks_matching_original_leaf() {
        let dir = tempfile::tempdir().expect("capability tempdir");
        let path = dir.path().join("input.cap");
        let file = create_private_capability_file(&path).expect("create capability file");
        let metadata = file.metadata().expect("stat capability file");
        let identity = CapabilityFileIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        };
        drop(file);
        unlink_capability_path_if_identity_matches(&path, identity)
            .expect("matching original leaf should be removed");
        assert!(!path.exists());
    }

    #[test]
    fn compose_commit_bytes_match_input_enter_semantics() {
        assert_eq!(compose_commit_bytes("", true), b"\r");
        assert_eq!(compose_commit_bytes("hello", true), b"hello\r");
        assert_eq!(compose_commit_bytes("hello", false), b"hello");
    }

    #[test]
    fn extract_urls_finds_trims_and_deduplicates_recent_links() {
        let extraction = extract_urls(
            "open https://example.com/path?q=1#frag.\n\
             again https://example.com/path?q=1#frag and http://host.test/a(b)!\n\
             quoted \"https://quoted.example/path\" <https://angle.example/x> `http://tick.test`",
        );

        assert_eq!(
            extraction.urls,
            vec![
                "https://example.com/path?q=1#frag",
                "http://host.test/a(b)",
                "https://quoted.example/path",
                "https://angle.example/x",
                "http://tick.test",
            ]
        );
        assert_eq!(extraction.last.as_deref(), Some("http://tick.test"));
    }

    #[test]
    fn extract_urls_last_uses_most_recent_occurrence_before_deduplication() {
        let extraction = extract_urls("https://a.test https://b.test https://a.test");
        assert_eq!(extraction.urls, vec!["https://a.test", "https://b.test"]);
        assert_eq!(extraction.last.as_deref(), Some("https://a.test"));
    }

    #[test]
    fn extract_urls_handles_empty_and_non_url_scheme_bodies() {
        let extraction = extract_urls("no links http:// https://");
        assert!(extraction.urls.is_empty());
        assert!(extraction.last.is_none());
    }

    #[test]
    fn extract_urls_matches_schemes_case_insensitively_and_preserves_text() {
        let extraction = extract_urls("go HTTP://Upper.Example/path then HtTpS://Mixed.Example/ok");
        assert_eq!(
            extraction.urls,
            vec!["HTTP://Upper.Example/path", "HtTpS://Mixed.Example/ok"]
        );
        assert_eq!(extraction.last.as_deref(), Some("HtTpS://Mixed.Example/ok"));
    }

    #[test]
    fn extract_urls_trims_trailing_delimiters_without_recounting_each_suffix() {
        let noisy_suffix = ")".repeat(512);
        let input = format!("https://example.test/a(b){noisy_suffix} https://done.test/ok");
        let extraction = extract_urls(&input);
        assert_eq!(
            extraction.urls,
            vec!["https://example.test/a(b)", "https://done.test/ok"]
        );
        assert_eq!(extraction.last.as_deref(), Some("https://done.test/ok"));
    }

    #[test]
    fn extract_urls_skips_over_length_tokens_without_truncating() {
        let long_url = format!(
            "https://example.test/{}",
            "a".repeat(MAX_EXTRACTED_URL_BYTES)
        );
        let extraction = extract_urls(&format!("{long_url} https://ok.test/done"));
        assert_eq!(extraction.urls, vec!["https://ok.test/done"]);
        assert_eq!(extraction.last.as_deref(), Some("https://ok.test/done"));
        assert!(
            !extraction
                .urls
                .iter()
                .any(|url| url.starts_with(&long_url[..128]))
        );
    }

    #[test]
    fn extract_urls_skips_raw_candidates_over_length_before_trimming() {
        let candidate = format!(
            "https://example.test/ok{}",
            ")".repeat(MAX_EXTRACTED_URL_BYTES)
        );
        let extraction = extract_urls(&format!("{candidate} https://ok.test/done"));
        assert_eq!(extraction.urls, vec!["https://ok.test/done"]);
        assert_eq!(extraction.last.as_deref(), Some("https://ok.test/done"));
    }

    #[test]
    fn extract_urls_skips_non_ascii_tokens_to_keep_schema_byte_caps_standard() {
        let extraction = extract_urls("https://example.test/é https://ok.test/ascii");
        assert_eq!(extraction.urls, vec!["https://ok.test/ascii"]);
        assert_eq!(extraction.last.as_deref(), Some("https://ok.test/ascii"));
    }

    #[test]
    fn extract_urls_caps_unique_rows_but_keeps_newest_valid_last() {
        let mut input = String::new();
        for index in 0..(MAX_EXTRACTED_URLS + 5) {
            input.push_str(&format!("https://u{index}.example/path "));
        }

        let extraction = extract_urls(&input);
        assert_eq!(extraction.urls.len(), MAX_EXTRACTED_URLS);
        assert_eq!(
            extraction.urls.first().map(String::as_str),
            Some("https://u0.example/path")
        );
        assert_eq!(
            extraction.urls.last().map(String::as_str),
            Some("https://u255.example/path")
        );
        assert_eq!(
            extraction.last.as_deref(),
            Some("https://u260.example/path")
        );
        assert!(
            !extraction
                .urls
                .iter()
                .any(|url| url == "https://u256.example/path")
        );
    }

    #[test]
    fn extract_search_matches_finds_case_sensitive_sanitized_lines_in_order() {
        let matches = extract_search_matches(
            "alpha needle\nNEEDLE uppercase\n\x1b[31mred needle\x1b[0m\nneedle again\n",
            "needle",
        );
        assert_eq!(
            matches,
            vec![
                "alpha needle".to_string(),
                "red needle".to_string(),
                "needle again".to_string(),
            ]
        );
    }

    #[test]
    fn extract_search_matches_strips_terminal_control_payloads() {
        let matches = extract_search_matches(
            "safe needle \x1b]52;c;secret\x07after\nansi \x1b[31mneedle\x1b[0m done\n",
            "needle",
        );
        assert_eq!(
            matches,
            vec![
                "safe needle after".to_string(),
                "ansi needle done".to_string(),
            ]
        );
        let rendered = matches.join("\n");
        assert!(!rendered.contains('\x1b'), "{rendered:?}");
        assert!(!rendered.contains("secret"), "{rendered:?}");
    }

    #[test]
    fn write_numbered_search_matches_uses_one_based_rows() {
        let mut out = Vec::new();
        write_numbered_search_matches(&["first".to_string(), "second".to_string()], &mut out)
            .unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "1\tfirst\n2\tsecond\n");
    }

    #[test]
    fn mobile_transcript_urls_reuses_numbered_url_output_without_remote_send() {
        let mut out = Vec::new();
        write_mobile_transcript_urls(
            "login at https://claude.ai/login then visit https://example.test/done.",
            &mut out,
        )
        .unwrap();

        assert_eq!(
            String::from_utf8(out).unwrap(),
            "1\thttps://claude.ai/login\n2\thttps://example.test/done\n"
        );
    }

    #[test]
    fn mobile_transcript_urls_reports_empty_results_locally() {
        let mut out = Vec::new();
        write_mobile_transcript_urls("no links here", &mut out).unwrap();

        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!("{MOBILE_TRANSCRIPT_SGR_RESET}No URLs found in current transcript.\n")
        );
    }

    #[test]
    fn mobile_transcript_url_commands_are_local_only() {
        for command in ["/links", "/urls"] {
            let options = MobileTranscriptOptions {
                tail: 80,
                refresh: Duration::from_millis(500),
                read_only: false,
                append_enter: true,
                banner: false,
            };
            let mut last_capture = "prior capture".to_string();
            let mut out = Vec::new();
            let mut sent_payloads = Vec::new();
            let mut capture_calls = 0;

            let keep_running = handle_mobile_transcript_input(
                command,
                MobileTranscriptInputContext {
                    target: "api",
                    tail_start: -80,
                    append_enter: options.append_enter,
                },
                &mut last_capture,
                &mut out,
                |target, start, end| {
                    capture_calls += 1;
                    assert_eq!(target, "api");
                    assert_eq!(start, Some(-80));
                    assert_eq!(end, None);
                    Ok("copy https://login.example/device".to_string())
                },
                |target, data| {
                    sent_payloads.push((target.to_string(), data));
                    Ok(())
                },
                |target| Ok(format!("lterm attach --raw -- {target}")),
            )
            .unwrap();

            assert!(keep_running);
            assert_eq!(capture_calls, 1, "{command} must capture locally once");
            assert!(
                sent_payloads.is_empty(),
                "{command} must not be forwarded to the PTY: {sent_payloads:?}"
            );
            assert_eq!(last_capture, "prior capture");
            assert_eq!(
                String::from_utf8(out).unwrap(),
                "1\thttps://login.example/device\n"
            );
        }
    }

    #[test]
    fn mobile_transcript_grep_command_is_local_only() {
        let options = MobileTranscriptOptions {
            tail: 80,
            refresh: Duration::from_millis(500),
            read_only: false,
            append_enter: true,
            banner: false,
        };
        let mut last_capture = "prior capture".to_string();
        let mut out = Vec::new();
        let mut sent_payloads = Vec::new();
        let mut capture_calls = 0;

        let keep_running = handle_mobile_transcript_input(
            "/grep needle",
            MobileTranscriptInputContext {
                target: "api",
                tail_start: -80,
                append_enter: options.append_enter,
            },
            &mut last_capture,
            &mut out,
            |target, start, end| {
                capture_calls += 1;
                assert_eq!(target, "api");
                assert_eq!(start, Some(-80));
                assert_eq!(end, None);
                Ok("one needle\nno match\ntwo needle\n".to_string())
            },
            |target, data| {
                sent_payloads.push((target.to_string(), data));
                Ok(())
            },
            |target| Ok(format!("lterm attach --raw -- {target}")),
        )
        .unwrap();

        assert!(keep_running);
        assert_eq!(capture_calls, 1);
        assert!(
            sent_payloads.is_empty(),
            "/grep must not be forwarded to the PTY: {sent_payloads:?}"
        );
        assert_eq!(last_capture, "prior capture");
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "1\tone needle\n2\ttwo needle\n"
        );
    }

    #[test]
    fn mobile_transcript_grep_query_keeps_literal_query_after_separator() {
        assert_eq!(mobile_transcript_grep_query("/grep"), Some(""));
        assert_eq!(mobile_transcript_grep_query("/grep needle"), Some("needle"));
        assert_eq!(
            mobile_transcript_grep_query("/grep   needle  "),
            Some("needle  ")
        );
        assert_eq!(mobile_transcript_grep_query("/grepneedle"), None);
    }

    #[test]
    fn mobile_transcript_grep_preserves_trailing_query_space() {
        let mut last_capture = "prior capture".to_string();
        let mut out = Vec::new();
        let mut sent_payloads = Vec::new();

        let keep_running = handle_mobile_transcript_input(
            "/grep   needle  ",
            MobileTranscriptInputContext {
                target: "api",
                tail_start: -80,
                append_enter: true,
            },
            &mut last_capture,
            &mut out,
            |target, start, end| {
                assert_eq!(target, "api");
                assert_eq!(start, Some(-80));
                assert_eq!(end, None);
                Ok("one needle  \ntwo needle\n".to_string())
            },
            |target, data| {
                sent_payloads.push((target.to_string(), data));
                Ok(())
            },
            |target| Ok(format!("lterm attach --raw -- {target}")),
        )
        .unwrap();

        assert!(keep_running);
        assert!(sent_payloads.is_empty());
        assert_eq!(last_capture, "prior capture");
        assert_eq!(String::from_utf8(out).unwrap(), "1\tone needle  \n");
    }

    #[test]
    fn mobile_transcript_grep_reports_usage_and_empty_results_locally() {
        for (command, expected, expected_capture_calls) in [
            (
                "/grep",
                format!("{MOBILE_TRANSCRIPT_SGR_RESET}Usage: /grep QUERY\n"),
                0,
            ),
            (
                "/grep missing",
                format!("{MOBILE_TRANSCRIPT_SGR_RESET}No matches found in current transcript.\n"),
                1,
            ),
        ] {
            let mut last_capture = "prior capture".to_string();
            let mut out = Vec::new();
            let mut sent_payloads = Vec::new();
            let mut capture_calls = 0;

            let keep_running = handle_mobile_transcript_input(
                command,
                MobileTranscriptInputContext {
                    target: "api",
                    tail_start: -80,
                    append_enter: true,
                },
                &mut last_capture,
                &mut out,
                |target, start, end| {
                    capture_calls += 1;
                    assert_eq!(target, "api");
                    assert_eq!(start, Some(-80));
                    assert_eq!(end, None);
                    Ok("one needle\n".to_string())
                },
                |target, data| {
                    sent_payloads.push((target.to_string(), data));
                    Ok(())
                },
                |target| Ok(format!("lterm attach --raw -- {target}")),
            )
            .unwrap();

            assert!(keep_running);
            assert_eq!(capture_calls, expected_capture_calls, "{command}");
            assert!(
                sent_payloads.is_empty(),
                "{command} must not be forwarded to the PTY: {sent_payloads:?}"
            );
            assert_eq!(last_capture, "prior capture");
            assert_eq!(String::from_utf8(out).unwrap(), expected);
        }
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
            lifecycle_state: None,
        }
    }

    #[test]
    fn automatic_reconnect_accepts_only_healthy_live_sessions() {
        let mut info = sample_session_info("agent", "sh", None);
        assert!(automatic_reconnect_candidate(&info));
        assert!(ensure_automatic_reconnect_candidate(&info).is_ok());

        info.lifecycle_state = Some(SessionLifecycleState::MonitorFailed);
        assert!(!automatic_reconnect_candidate(&info));
        let error = ensure_automatic_reconnect_candidate(&info)
            .expect_err("monitor-failed reconnect must require explicit selection")
            .to_string();
        assert!(error.contains("leader state is unknown"), "{error}");
        assert!(error.contains("lterm resume"), "{error}");

        info.alive = false;
        info.lifecycle_state = Some(SessionLifecycleState::Ending {
            trigger: SessionExitTrigger::DaemonShutdown,
        });
        assert!(!automatic_reconnect_candidate(&info));
        let error = ensure_automatic_reconnect_candidate(&info)
            .expect_err("ending reconnect must be rejected")
            .to_string();
        assert!(error.contains("ending session"), "{error}");
        assert!(!error.contains("lterm resume"), "{error}");
    }

    #[test]
    fn recent_exits_protocol_guard_is_non_destructive_and_explicit() {
        let old = DaemonStatus {
            version: "1.0.31".to_string(),
            protocol_version: 7,
            session_count: 1,
            active_connections: 0,
            shutting_down: false,
            daemon_uid: None,
            started_at_unix_secs: None,
        };
        let message = recent_exits_protocol_error(&old).expect("protocol 7 must be rejected");
        assert!(message.contains("upgrade/restart is required"), "{message}");
        assert!(
            message.contains("no live session was modified"),
            "{message}"
        );
        assert!(!message.contains("lterm shutdown"), "{message}");

        let current = DaemonStatus {
            protocol_version: super::RECENT_EXITS_PROTOCOL_VERSION,
            ..old
        };
        assert_eq!(recent_exits_protocol_error(&current), None);
    }

    #[test]
    fn reconnect_state_minimal_round_trip_uses_exact_private_keys() {
        let dir = tempfile::tempdir().expect("reconnect state tempdir");
        let path = dir.path().join("reconnect-state.json");
        let info = sample_session_info(
            "mobile-main",
            "TOKEN=secret sh -lc 'echo hidden'",
            Some("codex"),
        );

        remember_reconnect_target_best_effort_at_path(&info, &path);

        let bytes = std::fs::read(&path).expect("reconnect state file");
        #[cfg(unix)]
        assert_eq!(
            std::fs::metadata(&path)
                .expect("reconnect state metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600,
            "reconnect state should be private"
        );
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).expect("reconnect state JSON");
        let keys: std::collections::BTreeSet<_> = value
            .as_object()
            .expect("reconnect state object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            std::collections::BTreeSet::from([
                "pane_id",
                "recorded_at_unix_ms",
                "schema_version",
                "session_id",
                "session_name",
            ])
        );
        assert!(
            !String::from_utf8_lossy(&bytes).contains("TOKEN=secret")
                && !String::from_utf8_lossy(&bytes).contains("echo hidden")
                && !String::from_utf8_lossy(&bytes).contains("/tmp")
                && !String::from_utf8_lossy(&bytes).contains("codex"),
            "reconnect state must not store command/cwd/agent metadata: {}",
            String::from_utf8_lossy(&bytes)
        );

        let loaded = read_reconnect_state_from_path(&path)
            .expect("read reconnect state")
            .expect("reconnect state present");
        assert_eq!(loaded.schema_version, RECONNECT_STATE_SCHEMA_VERSION);
        assert_eq!(loaded.session_id, info.id);
        assert_eq!(loaded.pane_id, info.pane_id);
        assert_eq!(loaded.session_name, info.name);
    }

    #[test]
    fn reconnect_state_best_effort_read_ignores_missing_corrupt_and_unknown_schema() {
        let dir = tempfile::tempdir().expect("reconnect state tempdir");
        let path = dir.path().join("reconnect-state.json");

        assert!(read_reconnect_state_best_effort_from_path(&path).is_none());

        std::fs::write(&path, b"not json").expect("write corrupt reconnect state");
        assert!(read_reconnect_state_best_effort_from_path(&path).is_none());

        std::fs::write(
            &path,
            br#"{"schema_version":999,"session_id":"id","pane_id":"%1","session_name":"main","recorded_at_unix_ms":1}"#,
        )
        .expect("write unsupported reconnect state");
        assert!(read_reconnect_state_best_effort_from_path(&path).is_none());

        std::fs::write(
            &path,
            br#"{"schema_version":1,"session_id":"id","pane_id":"%1","session_name":"main","recorded_at_unix_ms":1,"command":"secret"}"#,
        )
        .expect("write unknown-field reconnect state");
        assert!(read_reconnect_state_best_effort_from_path(&path).is_none());
    }

    #[test]
    fn reconnect_state_write_failure_is_best_effort() {
        let dir = tempfile::tempdir().expect("reconnect state tempdir");
        let missing_parent = dir.path().join("missing").join("reconnect-state.json");
        let info = sample_session_info("mobile-main", "/bin/sh", None);

        remember_reconnect_target_best_effort_at_path(&info, &missing_parent);

        assert!(
            !missing_parent.exists(),
            "best-effort reconnect state write should not create missing parents in this failure fixture"
        );
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
        assert_eq!(
            String::from_utf8(out.clone()).unwrap(),
            format!("{MOBILE_TRANSCRIPT_SGR_RESET}one\n")
        );
        assert_eq!(previous, "one\n");

        out.clear();
        assert!(
            write_mobile_transcript_update(&mut previous, "one\ntwo\n", &mut out).unwrap(),
            "longer capture should write only the suffix"
        );
        assert_eq!(
            String::from_utf8(out.clone()).unwrap(),
            format!("{MOBILE_TRANSCRIPT_SGR_RESET}two\n")
        );

        out.clear();
        assert!(!write_mobile_transcript_update(&mut previous, "one\ntwo\n", &mut out).unwrap());
        assert!(out.is_empty());

        out.clear();
        assert!(write_mobile_transcript_update(&mut previous, "fresh\n", &mut out).unwrap());
        let rendered = String::from_utf8(out).unwrap();
        assert!(
            rendered.starts_with(MOBILE_TRANSCRIPT_SGR_RESET),
            "transcript refresh must reset stale terminal colors before local UI text: {rendered:?}"
        );
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
        assert_eq!(
            rendered,
            format!("{MOBILE_TRANSCRIPT_SGR_RESET}safe red done\n")
        );
        let sanitized_payload = rendered
            .strip_prefix(MOBILE_TRANSCRIPT_SGR_RESET)
            .expect("transcript update should start with exactly one local SGR reset");
        assert!(
            !sanitized_payload.contains('\x1b'),
            "sanitized transcript payload must not leak reset-only or other escapes: {rendered:?}"
        );
        assert!(!rendered.contains("secret"));
        assert_eq!(previous, "safe red done\n");

        previous = "one\ntwo\nthree\n".to_string();
        out.clear();
        assert!(
            write_mobile_transcript_update(&mut previous, "two\nthree\nfour\n", &mut out).unwrap(),
            "tail-window rollover should append only unseen complete-line suffix"
        );
        assert_eq!(
            String::from_utf8(out.clone()).unwrap(),
            format!("{MOBILE_TRANSCRIPT_SGR_RESET}four\n")
        );
        assert_eq!(previous, "two\nthree\nfour\n");

        previous = "alpha\nrepeat\nrepeat\n".to_string();
        out.clear();
        assert!(
            write_mobile_transcript_update(&mut previous, "repeat\nrepeat\nomega\n", &mut out)
                .unwrap(),
            "tail-window rollover should prefer the longest repeated-line overlap"
        );
        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!("{MOBILE_TRANSCRIPT_SGR_RESET}omega\n")
        );
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
    fn raw_attach_hint_quotes_target_and_uses_option_terminator() {
        assert_eq!(
            raw_attach_command_hint("codex-lterm").unwrap(),
            "lterm attach --raw -- codex-lterm"
        );
        assert_eq!(
            raw_attach_command_hint("-leading").unwrap(),
            "lterm attach --raw -- -leading"
        );
        assert_eq!(
            raw_attach_command_hint("bad name;rm -rf /").unwrap(),
            "lterm attach --raw -- 'bad name;rm -rf /'"
        );
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

    #[test]
    fn status_command_config_parses_command_interval_ansi_and_debug() {
        // command 없음(빈 문자열) → None.
        assert!(StatusCommandConfig::from_raw_parts("", None, None, None).is_none());
        // shlex 실패(따옴표 미닫힘) → None.
        assert!(StatusCommandConfig::from_raw_parts("foo \"bar", None, None, None).is_none());

        let config = StatusCommandConfig::from_raw_parts(
            "understatus --json",
            Some("5"),
            Some("0"),
            Some("1"),
        )
        .expect("valid command should parse");
        assert_eq!(config.argv, vec!["understatus", "--json"]);
        assert_eq!(config.interval, Duration::from_secs(5));
        assert!(!config.allow_color, "ANSI=0 should disable color");
        assert!(config.debug, "DEBUG=1 should enable debug");

        // ANSI 기본 true.
        let default_ansi = StatusCommandConfig::from_raw_parts("cmd", None, None, None)
            .expect("valid command should parse");
        assert!(default_ansi.allow_color, "ANSI default must be true");
        assert!(!default_ansi.debug, "DEBUG default must be false");
        assert_eq!(
            default_ansi.interval,
            Duration::from_secs(2),
            "interval default must be 2s"
        );
    }

    #[test]
    fn status_command_interval_clamps_and_falls_back() {
        // 0 → 하한 1초.
        assert_eq!(
            parse_status_command_interval(Some("0")),
            Duration::from_secs(1)
        );
        // 99999 → 상한 3600초.
        assert_eq!(
            parse_status_command_interval(Some("99999")),
            Duration::from_secs(3600)
        );
        // 비숫자 → 기본 2초.
        assert_eq!(
            parse_status_command_interval(Some("abc")),
            Duration::from_secs(2)
        );
        // None → 기본 2초.
        assert_eq!(parse_status_command_interval(None), Duration::from_secs(2));
        // 정상 범위 통과.
        assert_eq!(
            parse_status_command_interval(Some("10")),
            Duration::from_secs(10)
        );
    }

    #[test]
    fn status_command_bool_honors_default_for_unknown_values() {
        // None → default 그대로.
        assert!(parse_status_command_bool(None, true));
        assert!(!parse_status_command_bool(None, false));
        // 명시 false.
        assert!(!parse_status_command_bool(Some("0"), true));
        assert!(!parse_status_command_bool(Some("false"), true));
        // 명시 true.
        assert!(parse_status_command_bool(Some("1"), false));
        assert!(parse_status_command_bool(Some("on"), false));
        // 알 수 없는 값 → default 유지.
        assert!(parse_status_command_bool(Some("maybe"), true));
        assert!(!parse_status_command_bool(Some("maybe"), false));
    }

    #[test]
    fn agent_name_from_command_extracts_known_agents() {
        assert_eq!(agent_name_from_command("codex"), Some("codex".to_string()));
        assert_eq!(
            agent_name_from_command("claude --model opus"),
            Some("claude".to_string())
        );
        assert_eq!(
            agent_name_from_command("env FOO=1 codex"),
            Some("codex".to_string())
        );
        assert_eq!(
            agent_name_from_command("/usr/local/bin/gemini chat"),
            Some("gemini".to_string())
        );
        assert_eq!(
            agent_name_from_command("npx @openai/codex"),
            Some("codex".to_string())
        );
        // 미상 명령 → None.
        assert_eq!(agent_name_from_command("/bin/zsh -l"), None);
        assert_eq!(agent_name_from_command(""), None);
    }

    #[test]
    fn build_status_payload_emits_schema_keys_and_strips_controls() {
        let info = sample_session_info("my\u{7}session", "/usr/local/bin/codex --model gpt", None);
        let json: serde_json::Value =
            serde_json::from_str(&build_status_payload(&info, 120, 40, "oneline"))
                .expect("payload should be valid JSON");
        assert_eq!(json["source"], "lterm");
        assert_eq!(json["version"], 1);
        assert_eq!(json["surface_format"], "oneline");
        // 제어문자(BEL)가 제거된 세션 이름.
        assert_eq!(json["session"], "mysession");
        assert_eq!(json["pane"], "%test");
        assert_eq!(json["session_key"], "mysession/%test");
        assert_eq!(json["agent"], "codex");
        assert_eq!(json["cwd"], "/tmp");
        assert_eq!(json["cols"], 120);
        assert_eq!(json["rows"], 40);
    }

    /// C3: backend==Cmux면 payload의 surface_format이 "cmux-status"여야 한다(설계 §3.3 AC).
    /// surface_format은 backend에서 파생되므로 StatusBackend::surface_format()도 함께 단언한다.
    #[test]
    fn build_status_payload_surface_format_reflects_backend() {
        let info = sample_session_info("codex", "/usr/local/bin/codex", None);
        // backend==DelegatedSurface(Cmux) → "cmux-status".
        let cmux_format = StatusBackend::DelegatedSurface(SurfaceKind::Cmux).surface_format();
        assert_eq!(cmux_format, "cmux-status");
        let cmux_json: serde_json::Value =
            serde_json::from_str(&build_status_payload(&info, 120, 40, cmux_format))
                .expect("payload should be valid JSON");
        assert_eq!(cmux_json["surface_format"], "cmux-status");

        // 그 외 backend(예: DecstbmOverlay/Tmux) → "oneline".
        assert_eq!(StatusBackend::DecstbmOverlay.surface_format(), "oneline");
        assert_eq!(
            StatusBackend::DelegatedSurface(SurfaceKind::Tmux).surface_format(),
            "oneline"
        );
        assert_eq!(StatusBackend::Disabled.surface_format(), "oneline");
        let oneline_json: serde_json::Value = serde_json::from_str(&build_status_payload(
            &info,
            120,
            40,
            StatusBackend::DecstbmOverlay.surface_format(),
        ))
        .expect("payload should be valid JSON");
        assert_eq!(oneline_json["surface_format"], "oneline");
    }

    #[test]
    fn build_status_payload_handles_null_agent_and_cwd() {
        let mut info = sample_session_info("shell", "/bin/zsh", None);
        info.cwd = String::new();
        let json: serde_json::Value =
            serde_json::from_str(&build_status_payload(&info, 80, 24, "oneline"))
                .expect("payload should be valid JSON");
        assert!(json["agent"].is_null(), "unknown agent must serialize null");
        assert!(json["cwd"].is_null(), "empty cwd must serialize null");
        assert_eq!(json["session_key"], "shell/%test");
    }

    #[test]
    fn build_status_payload_caps_long_cwd_and_stays_valid_json() {
        // H1 견고화: 매우 긴 cwd가 cap으로 잘려도 payload는 유효 JSON이어야 하고,
        // cwd 길이는 STATUS_PAYLOAD_CWD_CAP 이하여야 한다(char 경계 안전 절단).
        let mut info = sample_session_info("shell", "/bin/zsh", None);
        info.cwd = "/".to_string() + &"가".repeat(2000); // 멀티바이트로 경계 절단도 검증.
        let payload = build_status_payload(&info, 80, 24, "oneline");
        let json: serde_json::Value =
            serde_json::from_str(&payload).expect("payload must remain valid JSON after capping");
        let cwd = json["cwd"].as_str().expect("cwd should be present");
        assert!(
            cwd.len() <= STATUS_PAYLOAD_CWD_CAP,
            "cwd must be capped to {} bytes, got {}",
            STATUS_PAYLOAD_CWD_CAP,
            cwd.len()
        );
    }

    fn wait_for_pid_file(path: &std::path::Path, timeout: Duration) -> i32 {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(contents) = std::fs::read_to_string(path) {
                if let Ok(pid) = contents.trim().parse::<i32>() {
                    return pid;
                }
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for pid file {}",
                path.display()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_for_process_exit(pid: i32, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
            if rc != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn run_status_command_returns_stdout_for_successful_command() {
        let argv = vec!["printf".to_string(), "HELLO".to_string()];
        let out = run_status_command(&argv, "", Duration::from_secs(2), 65536);
        assert_eq!(out, Some("HELLO".to_string()));
    }

    #[test]
    fn run_status_command_truncates_to_max_bytes() {
        let argv = vec!["printf".to_string(), "ABCDEFGHIJ".to_string()];
        let out = run_status_command(&argv, "", Duration::from_secs(2), 4);
        assert_eq!(out, Some("ABCD".to_string()), "stdout must be capped");
    }

    #[test]
    fn run_status_command_times_out_without_zombie() {
        let argv = vec!["sleep".to_string(), "5".to_string()];
        let started = Instant::now();
        let out = run_status_command(&argv, "", Duration::from_millis(200), 65536);
        assert_eq!(out, None, "timed-out command must yield None");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "timeout must trigger well before the command would finish"
        );
    }

    #[test]
    fn run_status_command_kills_pipe_holding_descendant_group() {
        let temp = tempfile::tempdir().expect("tempdir");
        let pid_file = temp.path().join("status-grandchild.pid");
        let quoted_pid_file =
            shlex::try_quote(pid_file.to_str().expect("utf8 pid path")).expect("quote pid path");
        let script = format!("sleep 30 & echo $! > {quoted_pid_file}; wait");
        let argv = vec!["sh".to_string(), "-c".to_string(), script];

        let started = Instant::now();
        let out = run_status_command(&argv, "", Duration::from_millis(200), 65536);

        assert_eq!(out, None, "timed-out status command must yield None");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "pipe-holding descendant must not keep status command blocked"
        );
        let pid = wait_for_pid_file(&pid_file, Duration::from_secs(2));
        assert!(
            wait_for_process_exit(pid, Duration::from_secs(2)),
            "status command timeout must kill process-group descendants; pid={pid}"
        );
    }

    #[test]
    fn run_status_command_returns_none_for_missing_binary() {
        let argv = vec!["lterm-nonexistent-binary-xyz".to_string()];
        assert_eq!(
            run_status_command(&argv, "", Duration::from_secs(1), 65536),
            None
        );
    }

    #[test]
    fn run_status_command_does_not_block_when_child_ignores_stdin() {
        // H1 회귀: 자식이 stdin을 전혀 읽지 않아도(`true`), 큰 payload write_all이
        // 블로킹되지 않고 명령이 정상적으로 deadline 안에 반환되어야 한다.
        // payload는 호출부 cap으로 작지만, 방어적으로 넉넉한 크기를 넣어도 안전함을 본다.
        let argv = vec!["true".to_string()];
        let big_payload = "x".repeat(8 * 1024);
        let started = Instant::now();
        let out = run_status_command(&argv, &big_payload, Duration::from_secs(2), 65536);
        // `true`는 빈 stdout이라 None(직전 라인 유지)이지만, 핵심은 블로킹 없이 반환됨.
        assert_eq!(out, None, "empty stdout must yield None");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "must return quickly without blocking on stdin write"
        );
    }

    #[test]
    fn run_status_command_returns_within_deadline_for_large_stdout_then_exit() {
        // H2 회귀: 자식이 max_bytes를 크게 초과하는 stdout(파이프 버퍼엔 들어가는 크기)을
        // 내고 정상 종료하면, reader 스레드가 드레인하고 메인 흐름은 deadline 안에 반환하며
        // 출력은 max_bytes로 절단된다. (yes처럼 무한 출력은 reader가 멈춘 뒤 자식이 full
        // 파이프에 막혀 timeout None이 되는 별개 경로이므로, 여기선 유한 출력으로 검증.)
        let payload = "A".repeat(4096); // max_bytes=16보다 훨씬 크지만 파이프 버퍼(64KB) 이하.
        let argv = vec!["printf".to_string(), "%s".to_string(), payload];
        let started = Instant::now();
        let out = run_status_command(&argv, "", Duration::from_secs(2), 16);
        assert_eq!(
            out.as_deref(),
            Some("AAAAAAAAAAAAAAAA"),
            "large stdout must be drained and capped to max_bytes"
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "large stdout + early exit must return within deadline, got {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn rpc_parse_error_preview_is_sanitized_and_capped() {
        let mut bytes = b"SAFE\x1b]52;c;SECRET\x07_AFTER".to_vec();
        bytes.extend(std::iter::repeat_n(
            b'A',
            RPC_PARSE_ERROR_PREVIEW_BYTES + 256,
        ));

        let preview = rpc_parse_error_preview(&bytes);

        assert!(preview.contains("SAFE"), "{preview:?}");
        assert!(
            !preview.contains("SECRET"),
            "OSC payload must not survive preview sanitization: {preview:?}"
        );
        assert!(
            preview.contains("bytes omitted"),
            "oversized preview should report omitted bytes: {preview:?}"
        );
        assert!(
            preview.len() < RPC_PARSE_ERROR_PREVIEW_BYTES + 256,
            "preview must be capped, got {} bytes",
            preview.len()
        );
    }

    #[test]
    fn terminal_output_tracker_skips_status_damage_scan_without_status_row() {
        let restore = Arc::new(KeyboardProtocolRestoreState::default());
        let alt = Arc::new(AltScreenState::default());
        let mut no_status =
            TerminalOutputTracker::new(Arc::clone(&restore), Arc::clone(&alt), None);
        let mut with_status =
            TerminalOutputTracker::new(Arc::clone(&restore), Arc::clone(&alt), Some(23));

        assert!(
            !no_status.observe(b"\x1b[2J").status_area_dirty,
            "status-disabled raw attach must not mark an impossible status-row redraw"
        );
        assert!(
            with_status.observe(b"\x1b[2J").status_area_dirty,
            "status-enabled attach still treats screen clear as status damage"
        );
    }

    #[test]
    fn apply_pending_status_command_keeps_only_latest() {
        let (tx, rx) = std::sync::mpsc::sync_channel::<String>(4);
        assert_eq!(apply_pending_status_command(Some(&rx)), None);
        tx.try_send("first".to_string()).unwrap();
        tx.try_send("second".to_string()).unwrap();
        assert_eq!(
            apply_pending_status_command(Some(&rx)),
            Some("second".to_string()),
            "only the most recent line should survive"
        );
        // 채널 비었으면 None(직전 라인 유지).
        assert_eq!(apply_pending_status_command(Some(&rx)), None);
        // None receiver → None.
        assert_eq!(apply_pending_status_command(None), None);
    }

    #[test]
    fn interruptible_sleep_completes_when_running_stays_true() {
        use std::sync::atomic::AtomicBool;
        let running = AtomicBool::new(true);
        // 짧은 total은 정상적으로 끝까지 대기하고 true를 반환한다.
        assert!(interruptible_sleep(Duration::from_millis(50), &running));
    }

    #[test]
    fn interruptible_sleep_wakes_promptly_when_running_cleared() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;
        // 1시간 interval을 흉내내 long sleep 중 detach를 시뮬레이션한다.
        let running = Arc::new(AtomicBool::new(true));
        let waker = Arc::clone(&running);
        let stopper = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            waker.store(false, Ordering::SeqCst);
        });
        let started = Instant::now();
        let completed = interruptible_sleep(Duration::from_secs(3600), &running);
        let elapsed = started.elapsed();
        stopper.join().unwrap();
        // running=false면 조기 중단 신호로 false를 반환한다.
        assert!(
            !completed,
            "running이 꺼지면 false(루프 종료)를 반환해야 한다"
        );
        // 청크(100ms) + store 지연(50ms)을 고려해도 긴 interval 내내 블로킹되지 않는다.
        assert!(
            elapsed < Duration::from_secs(1),
            "조기 중단이 1초 내에 일어나야 하는데 {elapsed:?} 걸렸다"
        );
    }

    #[test]
    fn interruptible_sleep_returns_false_when_already_stopped() {
        use std::sync::atomic::AtomicBool;
        let running = AtomicBool::new(false);
        // 시작부터 running=false면 즉시 false를 반환한다(대기 없음).
        let started = Instant::now();
        assert!(!interruptible_sleep(Duration::from_secs(3600), &running));
        assert!(started.elapsed() < Duration::from_millis(50));
    }

    #[test]
    fn nested_agent_detection_loop_interrupts_poll_sleep_on_shutdown() {
        use std::sync::atomic::AtomicBool;
        use std::sync::mpsc;

        let running = Arc::new(AtomicBool::new(true));
        let (tx, rx) = mpsc::sync_channel(4);
        let thread_running = Arc::clone(&running);
        let handle = std::thread::spawn(move || {
            run_nested_agent_detection_loop(&thread_running, &tx, || Ok(false));
        });

        let first_poll = rx
            .recv_timeout(Duration::from_millis(200))
            .expect("first nested-agent poll should be sent before sleeping");
        assert_eq!(first_poll, Ok(false));
        let started = Instant::now();
        running.store(false, Ordering::SeqCst);
        handle.join().expect("nested detection thread joins");

        assert!(
            started.elapsed() < Duration::from_millis(300),
            "nested detection teardown should not wait for the full {:?} poll sleep; elapsed {:?}",
            NESTED_AGENT_POLL,
            started.elapsed()
        );
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
    fn status_presence_policy_keeps_transport_separate_from_row_intent() {
        assert!(StatusPresencePolicy::RowAuto.requests_row());
        assert!(StatusPresencePolicy::RowAuto.allows_nested_suspend());
        assert!(!StatusPresencePolicy::RowOff.requests_row());
        assert!(!StatusPresencePolicy::RowOff.allows_nested_suspend());
        assert!(StatusPresencePolicy::ForceRow.requests_row());
        assert!(!StatusPresencePolicy::ForceRow.allows_nested_suspend());
        assert_eq!(
            StatusPresencePolicy::from_legacy_show_status(true),
            StatusPresencePolicy::RowAuto
        );
        assert_eq!(
            StatusPresencePolicy::from_legacy_show_status(false),
            StatusPresencePolicy::RowOff
        );
    }

    #[test]
    fn agent_presence_cue_is_sanitized_and_explains_hidden_row() {
        let mut title = Vec::new();
        write_lterm_title_cue(&mut title, "repo\x1b]0;bad\x07", "%0\nnext", "codex\tagent")
            .expect("title cue");
        let title = String::from_utf8(title).expect("title cue is utf8");

        assert!(title.starts_with("\x1b]0;"), "{title:?}");
        assert!(title.ends_with('\x07'), "{title:?}");
        assert_eq!(
            title.matches('\x1b').count(),
            1,
            "title may only contain its OSC introducer, not user-controlled ESC: {title:?}"
        );
        assert_eq!(
            title.matches('\x07').count(),
            1,
            "title may only contain its OSC terminator, not user-controlled BEL: {title:?}"
        );
        let title_inner = title
            .strip_prefix("\x1b]0;")
            .and_then(|value| value.strip_suffix('\x07'))
            .expect("title wrapper");
        assert!(
            title_inner.contains("lt:repo:%0next · codexagent"),
            "title cue should retain readable sanitized text: {title:?}"
        );
        assert!(
            !title_inner.contains('\x1b') && !title_inner.contains('\x07'),
            "title payload must not include user-controlled terminal controls: {title:?}"
        );

        let mut banner = Vec::new();
        write_lterm_agent_presence_banner(
            &mut banner,
            "repo\x1b]0;bad\x07",
            "%0\nnext",
            "codex\tagent",
        )
        .expect("banner cue");
        let banner = String::from_utf8(banner).expect("banner cue is utf8");

        assert!(
            banner.contains("[lterm] repo %0next · codexagent"),
            "banner should show lterm/session/pane/agent identity: {banner:?}"
        );
        assert!(
            banner.contains("status row hidden for agent TUI; use --status to show it"),
            "banner should explain why no bottom row is visible: {banner:?}"
        );
        assert!(
            banner.ends_with("\r\n"),
            "banner should return the cursor to the left margin before raw attach: {banner:?}"
        );
        assert!(
            !banner.contains('\x1b') && !banner.contains('\x07'),
            "banner must not include terminal controls: {banner:?}"
        );
    }

    #[test]
    fn initial_agent_presence_cue_resets_stale_host_sgr() {
        let _guard = crate::TEST_ENV_LOCK.lock().expect("env lock");
        let _env_guard = EnvGuard::capture(&["LTERM_AGENT_BANNER"]);
        // SAFETY: TEST_ENV_LOCK serializes process-wide environment mutation in tests.
        unsafe {
            std::env::remove_var("LTERM_AGENT_BANNER");
        }

        let cue = AgentPresenceCue {
            session: "omx-lterm".to_string(),
            pane: "%0".to_string(),
            agent: "omx".to_string(),
        };
        let mut output = Vec::new();
        cue.emit_initial(&mut output).expect("initial cue");

        assert!(
            output.starts_with(HOST_TERMINAL_SGR_RESET),
            "agent row-off cue must clear stale host SGR before title/banner: {output:?}"
        );
        let output = String::from_utf8(output).expect("cue output is utf8");
        assert!(
            output[HOST_TERMINAL_SGR_RESET.len()..].starts_with("\x1b]0;"),
            "SGR reset should precede the terminal-title cue: {output:?}"
        );
        assert!(
            output.contains("[lterm] omx-lterm %0 · omx"),
            "initial cue should still include the explanatory banner: {output:?}"
        );
    }

    #[test]
    fn row_off_raw_attach_resets_stale_host_sgr_only_on_terminals() {
        let mut terminal_output = Vec::new();
        reset_raw_attach_initial_sgr_if_needed(false, true, &mut terminal_output)
            .expect("row-off terminal reset");
        assert_eq!(terminal_output, HOST_TERMINAL_SGR_RESET);

        let mut status_output = Vec::new();
        reset_raw_attach_initial_sgr_if_needed(true, true, &mut status_output)
            .expect("status row owns its own SGR");
        assert!(
            status_output.is_empty(),
            "status-enabled attach already resets through StatusBar"
        );

        let mut piped_output = Vec::new();
        reset_raw_attach_initial_sgr_if_needed(false, false, &mut piped_output)
            .expect("non-terminal attach reset check");
        assert!(
            piped_output.is_empty(),
            "non-terminal raw attach output must not be prefixed with host UI escapes"
        );

        let mut status_piped_output = Vec::new();
        reset_raw_attach_initial_sgr_if_needed(true, false, &mut status_piped_output)
            .expect("status-enabled non-terminal reset check");
        assert!(
            status_piped_output.is_empty(),
            "status-enabled non-terminal attach must not be prefixed with host UI escapes"
        );
    }

    #[test]
    fn agent_presence_cue_env_flags_split_title_and_banner_controls() {
        let _guard = crate::TEST_ENV_LOCK.lock().expect("env lock");
        let _env_guard = EnvGuard::capture(&["LTERM_AGENT_CUE", "LTERM_AGENT_BANNER"]);
        // SAFETY: TEST_ENV_LOCK serializes process-wide environment mutation in tests.
        unsafe {
            std::env::remove_var("LTERM_AGENT_CUE");
            std::env::remove_var("LTERM_AGENT_BANNER");
        }
        assert!(agent_presence_cue_enabled());
        assert!(agent_presence_banner_enabled());

        // SAFETY: TEST_ENV_LOCK serializes process-wide environment mutation in tests.
        unsafe {
            std::env::set_var("LTERM_AGENT_BANNER", "0");
        }
        assert!(
            agent_presence_cue_enabled(),
            "banner opt-out must keep the terminal-title cue enabled"
        );
        assert!(!agent_presence_banner_enabled());

        // SAFETY: TEST_ENV_LOCK serializes process-wide environment mutation in tests.
        unsafe {
            std::env::set_var("LTERM_AGENT_CUE", "0");
            std::env::remove_var("LTERM_AGENT_BANNER");
        }
        assert!(
            !agent_presence_cue_enabled(),
            "outer cue gate suppresses the full cue path"
        );
        assert!(
            agent_presence_banner_enabled(),
            "banner helper remains independently controlled by LTERM_AGENT_BANNER"
        );
    }

    #[test]
    fn agent_title_cue_runtime_refreshes_title_without_inline_banner() {
        let mut runtime = AgentTitleCueRuntime::new(AgentPresenceCue {
            session: "repo".to_string(),
            pane: "%0".to_string(),
            agent: "codex".to_string(),
        });
        assert!(
            !runtime.refresh_due(),
            "new runtime should not immediately re-emit the title cue"
        );

        runtime.last_refresh = Instant::now() - AGENT_TITLE_REFRESH - Duration::from_millis(1);
        assert!(
            !runtime.refresh_due(),
            "title refresh should wait until the attached PTY has produced output"
        );
        runtime.observe_pty_output();
        assert!(
            !runtime.refresh_due(),
            "fresh PTY output should reset the idle interval"
        );
        runtime.last_refresh = Instant::now() - AGENT_TITLE_REFRESH - Duration::from_millis(1);
        runtime.last_pty_output =
            Some(Instant::now() - AGENT_TITLE_REFRESH + Duration::from_millis(1));
        assert!(
            !runtime.refresh_due(),
            "title refresh should wait for a full idle interval after the latest PTY output"
        );
        runtime.last_pty_output =
            Some(Instant::now() - AGENT_TITLE_REFRESH - Duration::from_millis(1));
        assert!(runtime.refresh_due());

        let mut output = Vec::new();
        runtime
            .refresh_title(&mut output)
            .expect("refresh title cue");
        let output = String::from_utf8(output).expect("title cue is utf8");
        assert_eq!(output, "\x1b]0;lt:repo:%0 · codex\x07");
        assert!(
            !output.contains("[lterm]"),
            "periodic refresh must not inject an inline row/banner into the raw TUI surface"
        );
        assert!(
            runtime.last_pty_output.is_none(),
            "successful refresh should disarm until the TUI writes again"
        );
        assert!(
            !runtime.refresh_due(),
            "successful refresh should reset the interval"
        );
    }

    #[test]
    fn row_presence_runtime_controls_resize_geometry() {
        let handle = StatusPresenceRuntimeHandle::new(true);
        assert_eq!(handle.pty_rows_for(24), 23);
        assert_eq!(handle.status_scroll_bottom_for(24), Some(23));

        handle.with_locked(|runtime| runtime.state = StatusPresenceState::Suspended);
        assert_eq!(handle.pty_rows_for(24), 24);
        assert_eq!(handle.status_scroll_bottom_for(24), None);

        handle.with_locked(|runtime| runtime.state = StatusPresenceState::Transitioning);
        assert_eq!(
            handle.pty_rows_for(24),
            24,
            "transitioning state must not reuse stale row-active geometry"
        );

        let disabled = StatusPresenceRuntimeHandle::new(false);
        assert_eq!(disabled.pty_rows_for(24), 24);
        assert_eq!(disabled.status_scroll_bottom_for(24), None);
    }

    fn sample_process(command: &str, depth: usize) -> ProcessInfo {
        ProcessInfo {
            session: "main".to_string(),
            pane_id: "%0".to_string(),
            depth,
            pid: (100 + depth) as u32,
            ppid: 1,
            process_group_id: Some(100),
            orphan: false,
            stat: "S".to_string(),
            cpu_percent: 0.0,
            mem_percent: 0.0,
            rss_kib: 0,
            elapsed: "00:00".to_string(),
            command: command.to_string(),
        }
    }

    fn sample_process_session(
        name: &str,
        pane_id: &str,
        process_id: u32,
        process_group_id: i32,
    ) -> crate::protocol::SessionInfo {
        let mut session = sample_session_info(name, "sh -lc 'sleep 60'", None);
        session.pane_id = pane_id.to_string();
        session.process_id = Some(process_id);
        session.process_group_id = Some(process_group_id);
        session
    }

    fn sample_process_row(pid: u32, ppid: u32, pgid: i32, command: &str) -> ProcessRow {
        ProcessRow {
            pid,
            ppid,
            pgid,
            stat: "S".to_string(),
            cpu_percent: 0.0,
            mem_percent: 0.0,
            rss_kib: 0,
            elapsed: "00:00".to_string(),
            command: command.to_string(),
        }
    }

    #[test]
    fn process_tree_from_rows_marks_same_group_non_descendants_as_orphans() {
        let session = sample_process_session("main", "%7", 100, 700);
        let processes = build_process_tree_from_rows(
            vec![session],
            vec![
                sample_process_row(130, 999, 700, "escaped-b"),
                sample_process_row(120, 100, 700, "child-b"),
                sample_process_row(140, 999, 701, "other-group"),
                sample_process_row(100, 1, 700, "root-shell"),
                sample_process_row(150, 110, 700, "grandchild"),
                sample_process_row(115, 999, 700, "escaped-a"),
                sample_process_row(110, 100, 700, "child-a"),
            ],
            true,
        );

        let summary: Vec<_> = processes
            .iter()
            .map(|process| {
                (
                    process.pid,
                    process.depth,
                    process.orphan,
                    process.session.as_str(),
                    process.pane_id.as_str(),
                    process.command.as_str(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                (100, 0, false, "main", "%7", "root-shell"),
                (110, 1, false, "main", "%7", "child-a"),
                (150, 2, false, "main", "%7", "grandchild"),
                (120, 1, false, "main", "%7", "child-b"),
                (115, 1, true, "main", "%7", "escaped-a"),
                (130, 1, true, "main", "%7", "escaped-b"),
            ],
            "synthetic process rows should deterministically separate descendants from same-pgid escaped rows"
        );

        let report = serde_json::to_value(&processes).expect("process report serializes");
        let rows = report.as_array().expect("process report is a JSON array");
        assert!(
            rows.iter().any(|row| {
                row.get("orphan").and_then(serde_json::Value::as_bool) == Some(true)
                    && row
                        .get("process_group_id")
                        .and_then(serde_json::Value::as_i64)
                        == Some(700)
            }),
            "JSON reporting must expose orphan classification and process group: {report:?}"
        );
        assert!(
            rows.iter().all(|row| {
                row.get("command")
                    .and_then(serde_json::Value::as_str)
                    .is_none_or(|command| command != "other-group")
            }),
            "unrelated process groups must stay out of the report: {report:?}"
        );
    }

    #[test]
    fn process_tree_from_rows_omits_orphans_when_disabled_or_session_pgid_missing() {
        let session = sample_process_session("main", "%7", 100, 700);
        let process_rows = vec![
            sample_process_row(100, 1, 700, "root-shell"),
            sample_process_row(115, 999, 700, "escaped-a"),
        ];

        let without_orphans =
            build_process_tree_from_rows(vec![session.clone()], process_rows.clone(), false);
        assert_eq!(
            without_orphans
                .iter()
                .map(|process| (process.pid, process.orphan))
                .collect::<Vec<_>>(),
            vec![(100, false)],
            "--orphans must be an explicit opt-in for same-pgid escaped rows"
        );

        let mut missing_group = session;
        missing_group.process_group_id = None;
        let missing_group_report =
            build_process_tree_from_rows(vec![missing_group], process_rows, true);
        assert_eq!(
            missing_group_report
                .iter()
                .map(|process| (process.pid, process.orphan))
                .collect::<Vec<_>>(),
            vec![(100, false)],
            "sessions without a stored process group cannot classify escaped same-pgid rows"
        );
    }

    #[test]
    fn nested_agent_classifier_matches_known_agent_descendants() {
        assert!(nested_known_agent_present_in_processes(&[
            sample_process("/bin/zsh -l", 0),
            sample_process("/usr/local/bin/codex --model gpt", 1),
        ]));
        assert!(nested_known_agent_present_in_processes(&[
            sample_process("/bin/zsh -l", 0),
            sample_process("omx --madmax --xhigh", 1),
        ]));
        assert!(
            !nested_known_agent_present_in_processes(&[
                sample_process("/bin/zsh -l", 0),
                sample_process("/usr/bin/vim README.md", 1),
            ]),
            "non-agent full-screen tools are not part of this best-effort classifier"
        );
        assert!(
            !nested_known_agent_present_in_processes(&[
                sample_process("/bin/zsh -l", 0),
                sample_process("/usr/bin/vim codex", 1),
            ]),
            "ordinary command arguments must not look like agent launchers"
        );
        assert!(
            !nested_known_agent_present_in_processes(&[
                sample_process("/bin/zsh -l", 0),
                sample_process("tail -f /tmp/@openai/codex.log", 1),
            ]),
            "scoped package names in ordinary filenames must not suspend the row"
        );
        assert!(
            !nested_known_agent_present_in_processes(&[
                sample_process("/bin/zsh -l", 0),
                sample_process("less claude", 1),
            ]),
            "known agent words are only meaningful as executables or wrapper payloads"
        );
        assert!(nested_known_agent_present_in_processes(&[
            sample_process("/bin/zsh -l", 0),
            sample_process(
                "node /Users/me/.nvm/versions/node/v22/lib/node_modules/oh-my-codex/dist/cli/omx.js",
                1
            ),
        ]));
        assert!(nested_known_agent_present_in_processes(&[
            sample_process("/bin/zsh -l", 0),
            sample_process("/usr/bin/env node /opt/homebrew/bin/codex.js", 1),
        ]));
        assert!(nested_known_agent_present_in_processes(&[
            sample_process("/bin/zsh -l", 0),
            sample_process("npx -y @openai/codex", 1),
        ]));
        assert!(nested_known_agent_present_in_processes(&[
            sample_process("/bin/zsh -l", 0),
            sample_process("npm exec -- @openai/codex", 1),
        ]));
        assert!(nested_known_agent_present_in_processes(&[
            sample_process("/bin/zsh -l", 0),
            sample_process(
                "node /Users/me/.npm/_npx/x/node_modules/@anthropic-ai/claude-code/cli.js",
                1
            ),
        ]));
        assert!(
            !nested_known_agent_present_in_processes(&[
                sample_process("/bin/zsh -l", 0),
                sample_process("node /tmp/not-an-agent.js", 1),
            ]),
            "generic node wrappers should not suspend the row"
        );
        assert!(
            !nested_known_agent_present_in_processes(&[
                sample_process("/bin/zsh -l", 0),
                sample_process("npm install @openai/codex", 1),
            ]),
            "package-manager install operations are not agent execution"
        );
        assert!(
            !nested_known_agent_present_in_processes(&[
                sample_process("/bin/zsh -l", 0),
                sample_process("npm view @anthropic-ai/claude-code", 1),
            ]),
            "package-manager metadata queries are not agent execution"
        );
    }

    #[test]
    fn nested_agent_detector_restores_after_detection_errors_when_suppressed() {
        let mut detector = NestedAgentDetector::new();
        assert_eq!(detector.apply_presence_poll(Ok(true)), None);
        assert_eq!(
            detector.apply_presence_poll(Ok(true)),
            Some(NestedAgentTransition::Suspend)
        );
        assert!(detector.suppressed);

        assert_eq!(
            detector.apply_presence_poll(Err("process tree unavailable".to_string())),
            None
        );
        assert_eq!(
            detector.apply_presence_poll(Err("process tree unavailable".to_string())),
            Some(NestedAgentTransition::Resume)
        );
        assert!(!detector.suppressed);
    }

    #[test]
    fn nested_agent_detector_errors_do_not_create_positive_debounce() {
        let mut detector = NestedAgentDetector::new();
        assert_eq!(detector.apply_presence_poll(Ok(true)), None);
        assert_eq!(
            detector.apply_presence_poll(Err("process tree unavailable".to_string())),
            None
        );
        assert_eq!(detector.apply_presence_poll(Ok(true)), None);
        assert_eq!(
            detector.apply_presence_poll(Ok(true)),
            Some(NestedAgentTransition::Suspend)
        );
    }

    #[test]
    fn nested_agent_detector_retries_ignored_transitions() {
        let mut detector = NestedAgentDetector::new();
        assert_eq!(detector.apply_presence_poll(Ok(true)), None);
        let transition = detector
            .apply_presence_poll(Ok(true))
            .expect("stable positive poll suspends");
        assert_eq!(transition, NestedAgentTransition::Suspend);
        detector.retry_transition(transition);
        assert!(!detector.suppressed);
        assert_eq!(
            detector.apply_presence_poll(Ok(true)),
            Some(NestedAgentTransition::Suspend),
            "ignored suspend transition remains retryable"
        );

        assert_eq!(detector.apply_presence_poll(Ok(false)), None);
        let transition = detector
            .apply_presence_poll(Ok(false))
            .expect("stable negative poll resumes");
        assert_eq!(transition, NestedAgentTransition::Resume);
        detector.retry_transition(transition);
        assert!(detector.suppressed);
        assert_eq!(
            detector.apply_presence_poll(Ok(false)),
            Some(NestedAgentTransition::Resume),
            "ignored resume transition remains retryable"
        );
    }

    fn trace_start_event() -> serde_json::Value {
        serde_json::json!({
            "type": "start",
            "schema_version": "1.0",
            "format": "lterm-trace-jsonl",
            "duration_ms": 100_u64
        })
    }

    fn trace_output_event(
        chunk_index: serde_json::Value,
        elapsed_ms: serde_json::Value,
        bytes_hex: serde_json::Value,
        len: serde_json::Value,
    ) -> serde_json::Value {
        serde_json::json!({
            "type": "output",
            "direction": "stdout",
            "chunk_index": chunk_index,
            "elapsed_ms": elapsed_ms,
            "bytes_hex": bytes_hex,
            "len": len
        })
    }

    fn trace_end_event(
        chunks_recorded: serde_json::Value,
        bytes_recorded: serde_json::Value,
    ) -> serde_json::Value {
        serde_json::json!({
            "type": "end",
            "chunks_recorded": chunks_recorded,
            "bytes_recorded": bytes_recorded
        })
    }

    fn trace_file(events: &[serde_json::Value]) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new_in("/tmp").expect("trace tempfile");
        for event in events {
            writeln!(file, "{event}").expect("write trace event");
        }
        file.flush().expect("flush trace tempfile");
        file
    }

    fn trace_replay_error_contains(events: &[serde_json::Value], timing: bool, needle: &str) {
        let file = trace_file(events);
        let err = validate_trace_replay(file.path(), timing).expect_err("trace should be rejected");
        let rendered = format!("{err:#}");
        assert!(
            rendered.contains(needle),
            "expected {needle:?} in error, got {rendered:?}"
        );
    }

    #[test]
    fn validate_trace_replay_accepts_valid_minimal_trace() {
        let file = trace_file(&[
            trace_start_event(),
            trace_output_event(0_u64.into(), 7_u64.into(), "6869".into(), 2_u64.into()),
            trace_end_event(1_u64.into(), 2_u64.into()),
        ]);

        let plan = validate_trace_replay(file.path(), false).expect("valid trace");
        assert_eq!(plan.total_bytes, 2);
        assert_eq!(plan.chunks.len(), 1);
        assert_eq!(plan.chunks[0].line_number, 2);
        assert_eq!(plan.chunks[0].elapsed_ms, 7);
        assert_eq!(plan.chunks[0].bytes, b"hi");
    }

    #[test]
    fn validate_trace_replay_rejects_missing_start() {
        trace_replay_error_contains(
            &[trace_end_event(0_u64.into(), 0_u64.into())],
            false,
            "end before start",
        );
        trace_replay_error_contains(&[], false, "missing a start event");
    }

    #[test]
    fn validate_trace_replay_rejects_output_before_start() {
        trace_replay_error_contains(
            &[
                trace_output_event(0_u64.into(), 0_u64.into(), "00".into(), 1_u64.into()),
                trace_end_event(1_u64.into(), 1_u64.into()),
            ],
            false,
            "output before start",
        );
    }

    #[test]
    fn validate_trace_replay_rejects_duplicate_start() {
        trace_replay_error_contains(
            &[
                trace_start_event(),
                trace_start_event(),
                trace_end_event(0_u64.into(), 0_u64.into()),
            ],
            false,
            "duplicate start event",
        );
    }

    #[test]
    fn validate_trace_replay_rejects_output_after_end() {
        trace_replay_error_contains(
            &[
                trace_start_event(),
                trace_end_event(0_u64.into(), 0_u64.into()),
                trace_output_event(0_u64.into(), 0_u64.into(), "00".into(), 1_u64.into()),
            ],
            false,
            "output after end",
        );
    }

    #[test]
    fn validate_trace_replay_rejects_chunk_index_gap() {
        trace_replay_error_contains(
            &[
                trace_start_event(),
                trace_output_event(1_u64.into(), 0_u64.into(), "00".into(), 1_u64.into()),
                trace_end_event(1_u64.into(), 1_u64.into()),
            ],
            false,
            "has chunk_index 1 but expected 0",
        );
    }

    #[test]
    fn validate_trace_replay_rejects_non_monotonic_elapsed_ms() {
        trace_replay_error_contains(
            &[
                trace_start_event(),
                trace_output_event(0_u64.into(), 20_u64.into(), "00".into(), 1_u64.into()),
                trace_output_event(1_u64.into(), 10_u64.into(), "01".into(), 1_u64.into()),
                trace_end_event(2_u64.into(), 2_u64.into()),
            ],
            false,
            "non-monotonic elapsed_ms",
        );
    }

    #[test]
    fn validate_trace_replay_rejects_len_hex_mismatch() {
        trace_replay_error_contains(
            &[
                trace_start_event(),
                trace_output_event(0_u64.into(), 0_u64.into(), "6869".into(), 1_u64.into()),
                trace_end_event(1_u64.into(), 1_u64.into()),
            ],
            false,
            "has len 1 but bytes_hex decodes to 2 bytes",
        );
    }

    #[test]
    fn validate_trace_replay_rejects_non_hex_bytes() {
        trace_replay_error_contains(
            &[
                trace_start_event(),
                trace_output_event(0_u64.into(), 0_u64.into(), "zz".into(), 1_u64.into()),
                trace_end_event(1_u64.into(), 1_u64.into()),
            ],
            false,
            "non-hex digit",
        );
    }

    #[test]
    fn validate_trace_replay_rejects_timing_delay_cap() {
        trace_replay_error_contains(
            &[
                trace_start_event(),
                trace_output_event(0_u64.into(), 60_001_u64.into(), "00".into(), 1_u64.into()),
                trace_end_event(1_u64.into(), 1_u64.into()),
            ],
            true,
            "exceeds safety cap",
        );
    }

    #[test]
    fn validate_trace_replay_rejects_end_count_mismatch() {
        trace_replay_error_contains(
            &[
                trace_start_event(),
                trace_output_event(0_u64.into(), 0_u64.into(), "00".into(), 1_u64.into()),
                trace_end_event(2_u64.into(), 1_u64.into()),
            ],
            false,
            "records 2 chunks but replay saw 1",
        );

        trace_replay_error_contains(
            &[
                trace_start_event(),
                trace_output_event(0_u64.into(), 0_u64.into(), "00".into(), 1_u64.into()),
                trace_end_event(1_u64.into(), 2_u64.into()),
            ],
            false,
            "records 2 bytes but replay saw 1",
        );
    }

    #[test]
    fn validate_trace_replay_rejects_schema_and_type_edges() {
        trace_replay_error_contains(
            &[
                serde_json::json!({
                    "type": "start",
                    "schema_version": "2.0",
                    "format": "lterm-trace-jsonl",
                    "duration_ms": 100_u64
                }),
                trace_end_event(0_u64.into(), 0_u64.into()),
            ],
            false,
            "unsupported trace schema_version",
        );
        trace_replay_error_contains(
            &[
                serde_json::json!({
                    "type": "start",
                    "schema_version": "1.0",
                    "format": 7,
                    "duration_ms": 100_u64
                }),
                trace_end_event(0_u64.into(), 0_u64.into()),
            ],
            false,
            "non-string field format",
        );
        trace_replay_error_contains(
            &[
                trace_start_event(),
                serde_json::json!({
                    "type": "output",
                    "direction": "stderr",
                    "chunk_index": 0_u64,
                    "elapsed_ms": 0_u64,
                    "bytes_hex": "00",
                    "len": 1_u64
                }),
                trace_end_event(1_u64.into(), 1_u64.into()),
            ],
            false,
            "unsupported output direction",
        );
        trace_replay_error_contains(
            &[
                trace_start_event(),
                serde_json::json!({
                    "type": "metadata",
                    "value": "ignored"
                }),
                trace_end_event(0_u64.into(), 0_u64.into()),
            ],
            false,
            "unsupported event type",
        );
        trace_replay_error_contains(&[trace_start_event()], false, "missing an end event");
    }

    #[test]
    fn validate_trace_replay_rejects_non_u64_optional_counts() {
        trace_replay_error_contains(
            &[
                trace_start_event(),
                serde_json::json!({
                    "type": "output",
                    "direction": "stdout",
                    "chunk_index": "zero",
                    "elapsed_ms": 0_u64,
                    "bytes_hex": "00",
                    "len": 1_u64
                }),
                trace_end_event(1_u64.into(), 1_u64.into()),
            ],
            false,
            "non-u64 field chunk_index",
        );
        trace_replay_error_contains(
            &[
                trace_start_event(),
                trace_output_event(0_u64.into(), 0_u64.into(), "00".into(), 1_u64.into()),
                serde_json::json!({
                    "type": "end",
                    "chunks_recorded": "one",
                    "bytes_recorded": 1_u64
                }),
            ],
            false,
            "non-u64 field chunks_recorded",
        );
        trace_replay_error_contains(
            &[
                trace_start_event(),
                trace_output_event(0_u64.into(), 0_u64.into(), "00".into(), 1_u64.into()),
                serde_json::json!({
                    "type": "end",
                    "chunks_recorded": 1_u64,
                    "bytes_recorded": "one"
                }),
            ],
            false,
            "non-u64 field bytes_recorded",
        );
    }

    #[test]
    fn trace_file_summary_collects_metadata_and_counts_unknowns() {
        let file = trace_file(&[
            serde_json::json!({
                "type": "start",
                "schema_version": "1.0",
                "format": "lterm-trace-jsonl",
                "trace_id": "trace-1",
                "producer": "lterm",
                "client_version": "1.2.3",
                "client_protocol_version": 4_u64,
                "target": "main",
                "created_at_unix_ms": 123_u64,
                "duration_ms": 250_u64,
                "max_bytes": 4096_u64,
                "rows": 24_u64,
                "cols": 80_u64,
                "raw_stream_policy": "raw-transparent"
            }),
            trace_output_event(0_u64.into(), 5_u64.into(), "6869".into(), 2_u64.into()),
            serde_json::json!({
                "type": "output",
                "direction": "stdout",
                "elapsed_ms": 7_u64,
                "bytes_hex": "21"
            }),
            trace_end_event(2_u64.into(), 3_u64.into()),
            trace_start_event(),
            serde_json::json!({"type": "end"}),
            serde_json::json!({"type": "metadata"}),
            serde_json::json!({"type": "output", "bytes_hex": "zz", "len": 1_u64}),
        ]);
        let mut raw = std::fs::OpenOptions::new()
            .append(true)
            .open(file.path())
            .expect("append malformed trace line");
        writeln!(raw, "not-json").expect("malformed line");
        writeln!(raw).expect("blank line");
        raw.flush().expect("flush trace file");

        let summary = trace_file_summary(file.path()).expect("trace summary");
        assert_eq!(summary.format.as_deref(), Some("lterm-trace-jsonl"));
        assert_eq!(summary.schema_version.as_deref(), Some("1.0"));
        assert_eq!(summary.trace_id.as_deref(), Some("trace-1"));
        assert_eq!(summary.producer.as_deref(), Some("lterm"));
        assert_eq!(summary.client_version.as_deref(), Some("1.2.3"));
        assert_eq!(summary.client_protocol_version, Some(4));
        assert_eq!(summary.target.as_deref(), Some("main"));
        assert_eq!(summary.created_at_unix_ms, Some(123));
        assert_eq!(summary.duration_ms, Some(250));
        assert_eq!(summary.max_bytes, Some(4096));
        assert_eq!(summary.rows, Some(24));
        assert_eq!(summary.cols, Some(80));
        assert_eq!(
            summary.raw_stream_policy.as_deref(),
            Some("raw-transparent")
        );
        assert_eq!(summary.event_count, 9);
        assert_eq!(summary.output_chunks, 2);
        assert_eq!(summary.output_bytes, 3);
        assert_eq!(summary.first_output_elapsed_ms, Some(5));
        assert_eq!(summary.last_output_elapsed_ms, Some(7));
        assert_eq!(summary.end_chunks_recorded, Some(2));
        assert_eq!(summary.end_bytes_recorded, Some(3));
        assert_eq!(summary.unknown_events, 5);
    }

    #[test]
    fn read_trace_jsonl_line_handles_crlf_final_line_and_rejects_bad_input() {
        let path = std::path::Path::new("trace.fixture");
        let mut reader = BufReader::new(Cursor::new(b"one\r\ntwo".to_vec()));
        let mut line_number = 0_usize;
        assert_eq!(
            read_trace_jsonl_line(&mut reader, &mut line_number, path)
                .expect("first line")
                .as_deref(),
            Some("one")
        );
        assert_eq!(line_number, 1);
        assert_eq!(
            read_trace_jsonl_line(&mut reader, &mut line_number, path)
                .expect("final line")
                .as_deref(),
            Some("two")
        );
        assert_eq!(line_number, 2);
        assert!(
            read_trace_jsonl_line(&mut reader, &mut line_number, path)
                .expect("eof")
                .is_none()
        );

        let mut invalid_utf8 = BufReader::new(Cursor::new(vec![0xff, b'\n']));
        let err = read_trace_jsonl_line(&mut invalid_utf8, &mut 0, path)
            .expect_err("invalid UTF-8 should fail");
        assert!(
            format!("{err:#}").contains("not valid UTF-8"),
            "unexpected UTF-8 error: {err:#}"
        );

        let mut oversized = BufReader::new(Cursor::new(vec![b'x'; MAX_TRACE_JSONL_LINE_BYTES + 1]));
        let err = read_trace_jsonl_line(&mut oversized, &mut 0, path)
            .expect_err("oversized line should fail");
        assert!(
            format!("{err:#}").contains("maximum JSONL line length"),
            "unexpected line cap error: {err:#}"
        );
    }

    #[test]
    fn trace_summary_text_and_open_context_cover_human_output_paths() {
        let summary = super::TraceFileSummary {
            path: "/tmp/trace\u{1b}[31m.jsonl".to_string(),
            format: Some("lterm-trace-jsonl".to_string()),
            schema_version: Some("1.0".to_string()),
            trace_id: Some("trace-id".to_string()),
            producer: Some("lterm".to_string()),
            client_version: Some("1.0.0".to_string()),
            client_protocol_version: Some(4),
            target: Some("main".to_string()),
            created_at_unix_ms: Some(123),
            duration_ms: Some(250),
            max_bytes: Some(4096),
            rows: Some(24),
            cols: Some(80),
            raw_stream_policy: Some("raw-transparent".to_string()),
            event_count: 3,
            output_chunks: 1,
            output_bytes: 2,
            first_output_elapsed_ms: Some(5),
            last_output_elapsed_ms: Some(5),
            end_elapsed_ms: Some(7),
            end_reason: Some("duration".to_string()),
            end_bytes_recorded: Some(2),
            end_chunks_recorded: Some(1),
            unknown_events: 0,
        };
        let rendered = trace_summary_text(&summary);
        assert!(
            !rendered.contains('\u{1b}'),
            "summary text must sanitize terminal controls: {rendered:?}"
        );
        assert!(rendered.contains("path\t/tmp/trace.jsonl\n"));
        assert!(rendered.contains("format\tlterm-trace-jsonl\n"));
        assert!(rendered.contains("client_protocol_version\t4\n"));
        assert!(rendered.contains("event_count\t3\n"));
        assert!(rendered.contains("end_reason\tduration\n"));
        assert!(rendered.contains("unknown_events\t0\n"));

        let unknowns = super::TraceFileSummary {
            path: "unknowns".to_string(),
            ..super::TraceFileSummary::default()
        };
        let unknown_rendered = trace_summary_text(&unknowns);
        assert!(unknown_rendered.contains("path\tunknowns\n"));
        assert!(unknown_rendered.contains("format\tunknown\n"));
        assert!(unknown_rendered.contains("client_protocol_version\tunknown\n"));
        assert!(unknown_rendered.contains("event_count\t0\n"));

        assert!(
            trace_output_open_context(std::path::Path::new("trace.jsonl"), false)
                .contains("pass --force")
        );
        assert!(
            trace_output_open_context(std::path::Path::new("trace.jsonl"), true)
                .contains("truncate")
        );
    }

    #[test]
    fn trace_hex_helpers_cover_encoding_decoding_and_validation_edges() {
        assert!(current_unix_ms().is_some());
        assert_eq!(hex_encode(b"\x00\x0f\x10\xff"), "000f10ff");
        assert_eq!(hex_encoded_len("6869").expect("hex len"), 2);
        assert_eq!(hex_decode("4869").expect("uppercase hex"), b"Hi");
        assert!(
            hex_encoded_len("abc")
                .unwrap_err()
                .to_string()
                .contains("odd length")
        );
        assert!(
            hex_encoded_len("zz")
                .unwrap_err()
                .to_string()
                .contains("non-hex")
        );
        assert!(
            hex_decode("0")
                .unwrap_err()
                .to_string()
                .contains("odd length")
        );
        assert!(
            hex_decode("0g")
                .unwrap_err()
                .to_string()
                .contains("non-hex")
        );
    }

    #[test]
    fn ensure_trace_force_target_private_rejects_unsafe_targets() {
        let dir = tempfile::tempdir().expect("trace tempdir");

        let target = dir.path().join("target.trace");
        std::fs::write(&target, b"trace").expect("target file");
        let link = dir.path().join("link.trace");
        symlink(&target, &link).expect("trace symlink");
        let err = ensure_trace_force_target_private(&link).expect_err("symlink reject");
        assert!(
            err.to_string().contains("refusing to overwrite symlink"),
            "unexpected symlink error: {err:#}"
        );

        let err = ensure_trace_force_target_private(dir.path()).expect_err("directory reject");
        assert!(
            err.to_string().contains("refusing to overwrite non-file"),
            "unexpected directory error: {err:#}"
        );

        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644))
            .expect("world-readable trace file");
        let err = ensure_trace_force_target_private(&target).expect_err("public file reject");
        assert!(
            err.to_string().contains("permissions 644"),
            "unexpected public-file error: {err:#}"
        );
    }

    #[test]
    fn ensure_trace_force_target_private_allows_missing_or_private_file() {
        let dir = tempfile::tempdir().expect("trace tempdir");
        let missing = dir.path().join("missing.trace");
        ensure_trace_force_target_private(&missing).expect("missing target is safe for create");

        let private = dir.path().join("private.trace");
        std::fs::write(&private, b"trace").expect("private trace file");
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o600))
            .expect("private trace permissions");
        ensure_trace_force_target_private(&private).expect("private regular file is safe");
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
    fn attach_header_reader_rejects_eof_before_header() {
        let mut reader = BufReader::new(Cursor::new(Vec::<u8>::new()));

        let err = read_attach_response_header(&mut reader).expect_err("empty attach header");
        assert!(
            err.to_string()
                .contains("daemon closed attach before header"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn status_bar_redraw_clears_previous_row_after_resize() {
        let mut status_bar = StatusBar {
            session_name: "omx-lterm".to_string(),
            pane_id: "%0".to_string(),
            style: Some(StatusStyle::Full(StatusTheme::Blue)),
            drawn_status_rows: Vec::new(),
            preserve_sgr_stack: true,
            command_line: None,
            command_allow_color: false,
            terminal_state: None,
            last_body: None,
            force_redraw: true,
            force_content_redraw: false,
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
    fn draw_at_size_hides_cursor_and_restores_visible_by_default() {
        // terminal_state=None이면 커서가 보이는 상태로 가정 → 페이로드는 `\x1b[?25l`로
        // 시작하고 `\x1b[?25h`(보임 복원)로 끝나야 한다. repaint 중 커서가 status 행으로
        // 튀어 깜빡이는 것을 막는다.
        let mut status_bar = StatusBar {
            session_name: "omx-lterm".to_string(),
            pane_id: "%0".to_string(),
            style: Some(StatusStyle::Full(StatusTheme::Blue)),
            drawn_status_rows: Vec::new(),
            preserve_sgr_stack: true,
            command_line: None,
            command_allow_color: false,
            terminal_state: None,
            last_body: None,
            force_redraw: true,
            force_content_redraw: false,
        };
        let mut output = Vec::new();
        status_bar
            .draw_at_size(&mut output, 80, 24)
            .expect("draw with default cursor visibility");
        let payload = String::from_utf8(output).expect("status payload should be utf8");
        assert!(
            payload.starts_with("\x1b[?25l"),
            "draw payload must start by hiding the cursor: {payload:?}"
        );
        assert!(
            payload.ends_with("\x1b8\x1b[?25h"),
            "draw payload must restore cursor visible after \\x1b8: {payload:?}"
        );
        status_bar.style = None;
    }

    #[test]
    fn draw_at_size_restores_hidden_cursor_when_pty_hid_it() {
        // PTY 앱이 커서를 숨겨둔 상태(cursor_visible=false)면 repaint 후에도 숨김으로
        // 복원해야 한다 — 즉 페이로드는 `\x1b[?25l`로 시작하고 `\x1b[?25l`로 끝난다.
        let state = Arc::new(AltScreenState::default());
        state.cursor_visible.store(false, Ordering::Relaxed);
        let mut status_bar = StatusBar {
            session_name: "omx-lterm".to_string(),
            pane_id: "%0".to_string(),
            style: Some(StatusStyle::Full(StatusTheme::Blue)),
            drawn_status_rows: Vec::new(),
            preserve_sgr_stack: true,
            command_line: None,
            command_allow_color: false,
            terminal_state: Some(Arc::clone(&state)),
            last_body: None,
            force_redraw: true,
            force_content_redraw: false,
        };
        let mut output = Vec::new();
        status_bar
            .draw_at_size(&mut output, 80, 24)
            .expect("draw with hidden cursor");
        let payload = String::from_utf8(output).expect("status payload should be utf8");
        assert!(
            payload.starts_with("\x1b[?25l"),
            "draw payload must start by hiding the cursor: {payload:?}"
        );
        assert!(
            payload.ends_with("\x1b8\x1b[?25l"),
            "draw payload must keep cursor hidden when PTY had it hidden: {payload:?}"
        );
        status_bar.style = None;
    }

    #[test]
    fn reserve_terminal_area_wraps_cursor_visibility() {
        // reserve도 DECSTBM이 커서를 home으로 옮기므로 같은 패턴으로 감싼다.
        // 기본(보임)이면 `\x1b[?25l`로 시작, `\x1b[?25h`로 끝.
        let status_bar = StatusBar {
            session_name: "omx-lterm".to_string(),
            pane_id: "%0".to_string(),
            style: Some(StatusStyle::Full(StatusTheme::Blue)),
            drawn_status_rows: Vec::new(),
            preserve_sgr_stack: true,
            command_line: None,
            command_allow_color: false,
            terminal_state: None,
            last_body: None,
            force_redraw: true,
            force_content_redraw: false,
        };
        let mut output = Vec::new();
        status_bar
            .reserve_terminal_area(&mut output, 24)
            .expect("reserve with default cursor visibility");
        let payload = String::from_utf8(output).expect("reserve payload should be utf8");
        assert_eq!(
            payload, "\x1b[?25l\x1b7\x1b[1;23r\x1b8\x1b[?25h",
            "reserve must hide cursor, set scroll region, then restore visible"
        );

        // PTY가 커서를 숨긴 상태면 `\x1b[?25l`로 닫는다.
        let state = Arc::new(AltScreenState::default());
        state.cursor_visible.store(false, Ordering::Relaxed);
        let hidden_bar = StatusBar {
            session_name: "omx-lterm".to_string(),
            pane_id: "%0".to_string(),
            style: Some(StatusStyle::Full(StatusTheme::Blue)),
            drawn_status_rows: Vec::new(),
            preserve_sgr_stack: true,
            command_line: None,
            command_allow_color: false,
            terminal_state: Some(state),
            last_body: None,
            force_redraw: true,
            force_content_redraw: false,
        };
        let mut hidden_output = Vec::new();
        hidden_bar
            .reserve_terminal_area(&mut hidden_output, 24)
            .expect("reserve with hidden cursor");
        let hidden_payload =
            String::from_utf8(hidden_output).expect("reserve payload should be utf8");
        assert_eq!(
            hidden_payload, "\x1b[?25l\x1b7\x1b[1;23r\x1b8\x1b[?25l",
            "reserve must keep cursor hidden when PTY had it hidden"
        );
    }

    /// content-dedup 테스트용 StatusBar 생성. command_line으로 본문을 결정적으로 바꿔
    /// terminal_size()(테스트 환경 fallback 80x24)와 무관하게 dedup 키 차이를 만든다.
    fn dedup_test_status_bar(command_line: Option<&str>) -> StatusBar {
        StatusBar {
            session_name: "omx-lterm".to_string(),
            pane_id: "%0".to_string(),
            style: Some(StatusStyle::Full(StatusTheme::Blue)),
            drawn_status_rows: Vec::new(),
            preserve_sgr_stack: true,
            command_line: command_line.map(str::to_string),
            command_allow_color: false,
            terminal_state: None,
            last_body: None,
            force_redraw: true,
            force_content_redraw: false,
        }
    }

    /// dedup이 안정화될 때까지 refresh를 반복한다(=Ok(false)로 skip할 때까지). 첫 draw는
    /// 현재 행을 `\x1b[2K`로 비우지만, drawn_status_rows에 등록된 뒤(remember_status_row)에는
    /// 같은 행 redraw가 그 clear를 생략한다(same-row flicker 회피). 따라서 본문은 첫 draw
    /// 후 한 번 바뀐 뒤 안정화되므로, dedup 테스트는 그 안정 지점부터 검증한다.
    fn refresh_until_stable(status_bar: &mut StatusBar) {
        for _ in 0..4 {
            let mut sink = Vec::new();
            if !status_bar
                .refresh(&mut sink)
                .expect("refresh while stabilizing")
            {
                return;
            }
        }
        panic!("refresh did not stabilize (dedup never engaged)");
    }

    #[test]
    fn refresh_skips_redraw_when_body_unchanged() {
        // (a) 동일 본문 + force_redraw=false면 refresh가 Ok(false)로 skip하고 아무것도 쓰지 않는다.
        // codex 스피너가 status_dirty를 4Hz로 set해도 본문이 같으면 커서를 일절 건드리지 않는다.
        let mut status_bar = dedup_test_status_bar(None);
        let mut first = Vec::new();
        let drew_first = status_bar.refresh(&mut first).expect("first refresh draws");
        assert!(
            drew_first,
            "first refresh must draw (force_redraw 기본 true)"
        );
        assert!(!first.is_empty(), "first refresh must write a payload");

        refresh_until_stable(&mut status_bar);

        // 안정화 이후 동일 본문 refresh는 아무것도 쓰지 않아야 한다(커서 미접촉).
        let mut deduped = Vec::new();
        let drew = status_bar
            .refresh(&mut deduped)
            .expect("stable refresh dedups");
        assert!(
            !drew,
            "refresh with identical body must return Ok(false) once stable"
        );
        assert!(
            deduped.is_empty(),
            "deduped refresh must not write anything (cursor untouched): {deduped:?}"
        );
        status_bar.style = None;
    }

    #[test]
    fn refresh_redraws_when_body_changes() {
        // (b) 본문이 바뀌면(command_line 변경) Ok(true)로 다시 쓴다.
        let mut status_bar = dedup_test_status_bar(None);
        refresh_until_stable(&mut status_bar);

        // 같은 본문 — skip.
        let mut unchanged = Vec::new();
        assert!(
            !status_bar
                .refresh(&mut unchanged)
                .expect("unchanged refresh"),
            "identical body should dedup once stable"
        );

        // command_line을 바꿔 본문을 변경 → 다시 그려야 한다.
        status_bar.command_line = Some("\x1b[32mbusy\x1b[0m".to_string());
        let mut changed = Vec::new();
        let drew = status_bar.refresh(&mut changed).expect("changed refresh");
        assert!(drew, "changed body must redraw");
        assert!(!changed.is_empty(), "changed refresh must write a payload");
        status_bar.style = None;
    }

    #[test]
    fn refresh_force_redraw_overrides_dedup() {
        // (c) force_redraw=true면 본문이 동일해도 다시 쓰고, 그 후 force_redraw는 false로 리셋된다.
        let mut status_bar = dedup_test_status_bar(None);
        refresh_until_stable(&mut status_bar);

        status_bar.force_redraw = true;
        let mut forced = Vec::new();
        let drew = status_bar.refresh(&mut forced).expect("forced refresh");
        assert!(drew, "force_redraw must draw even with identical body");
        assert!(!forced.is_empty(), "forced refresh must write a payload");
        assert!(
            !status_bar.force_redraw,
            "force_redraw must reset to false after a real draw"
        );

        // 리셋 후 동일 본문은 다시 dedup된다.
        let mut after = Vec::new();
        assert!(
            !status_bar.refresh(&mut after).expect("post-force refresh"),
            "after force_redraw reset, identical body dedups again"
        );
        status_bar.style = None;
    }

    #[test]
    fn refresh_content_only_backstop_omits_reserve() {
        // 백스톱(force_content_redraw)은 내용만 redraw하고 reserve(DECSTBM scroll-region 재설정)를
        // 내보내지 않아 codex 등이 쓰는 자체 scroll-region을 보존한다(주기적 reserve 재확인이 codex
        // idle 레이아웃을 침범해 입력칸이 늘어나던 회귀 차단). 대조로 force_redraw는 reserve를 포함한다.
        // reserve = `\x1b7\x1b[1;{N}r\x1b8`이므로 DECSTBM 마커 `\x1b[1;`의 유무로 판정한다
        // (draw 본문은 `\x1b[{rows};1H`만 써 `\x1b[1;`을 만들지 않는다).
        let mut bar = dedup_test_status_bar(None);
        refresh_until_stable(&mut bar);

        // (1) content-only: reserve(DECSTBM) 없이 그린다.
        bar.force_content_redraw = true;
        let mut content_only = Vec::new();
        assert!(
            bar.refresh(&mut content_only)
                .expect("content-only refresh"),
            "force_content_redraw must draw even with identical body"
        );
        let payload = String::from_utf8(content_only).expect("payload utf8");
        assert!(!payload.is_empty(), "content-only must write a payload");
        assert!(
            !payload.contains("\x1b[1;"),
            "content-only must NOT re-issue DECSTBM scroll-region: {payload:?}"
        );
        assert!(
            payload.starts_with("\x1b[?25l"),
            "content-only도 force_redraw와 동일하게 커서 숨김 envelope로 시작해야 함: {payload:?}"
        );
        assert!(
            !bar.force_content_redraw,
            "force_content_redraw must reset to false after a draw"
        );

        // (2) 대조: force_redraw는 reserve(DECSTBM)를 포함한다.
        refresh_until_stable(&mut bar);
        bar.force_redraw = true;
        let mut with_reserve = Vec::new();
        assert!(
            bar.refresh(&mut with_reserve).expect("forced refresh"),
            "force_redraw draws"
        );
        let reserve_payload = String::from_utf8(with_reserve).expect("payload utf8");
        assert!(
            reserve_payload.contains("\x1b[1;"),
            "force_redraw must include DECSTBM reserve: {reserve_payload:?}"
        );

        // (3) 두 플래그 동시 참: reserve 포함 경로가 우선(write_with_reserve = force_redraw || changed)
        //     해야 손상 복구가 content-only로 떨어지지 않는다. 그린 뒤 두 플래그 모두 리셋.
        refresh_until_stable(&mut bar);
        bar.force_redraw = true;
        bar.force_content_redraw = true;
        let mut both = Vec::new();
        assert!(
            bar.refresh(&mut both).expect("both-flags refresh"),
            "both flags must draw"
        );
        let both_payload = String::from_utf8(both).expect("payload utf8");
        assert!(
            both_payload.contains("\x1b[1;"),
            "force_redraw+force_content_redraw 동시엔 reserve 포함 경로가 우선해야 함: {both_payload:?}"
        );
        assert!(
            !bar.force_redraw && !bar.force_content_redraw,
            "두 플래그 모두 draw 후 리셋되어야 함"
        );
        bar.style = None;
    }

    // ── 지뢰 처리: self-provided TMUX 식별 (real_tmux 오분류 방지) ──

    /// lterm self-provided TMUX(`$TMUX` socket 필드 == LTERM_SOCKET)는 self로 식별된다.
    #[test]
    fn self_provided_tmux_matches_lterm_socket_env() {
        let sock = "/run/user/501/lterm.sock";
        assert!(is_self_provided_tmux(sock, Some(sock), None, None));
    }

    /// `$TMUX` socket 필드가 paths::socket_path()와 일치해도 self로 식별된다(LTERM_SOCKET 미설정 폴백).
    #[test]
    fn self_provided_tmux_matches_socket_path_fallback() {
        let sock = "/tmp/lterm-runtime/lterm.sock";
        assert!(is_self_provided_tmux(sock, None, Some(sock), None));
    }

    /// 최신 lterm의 `$TMUX` socket 필드는 live daemon이 아닌 compat-only fast-fail 경로다.
    #[test]
    fn self_provided_tmux_matches_compat_socket_path() {
        let live_sock = "/run/user/501/lterm.sock";
        let compat_sock = "/run/user/501/.lterm.sock.tmux-compat";
        assert!(is_self_provided_tmux(
            compat_sock,
            Some(live_sock),
            Some(live_sock),
            Some(compat_sock)
        ));
    }

    /// 진짜 외부 tmux(소켓 경로가 lterm과 다름)는 self가 아니다 → real_tmux로 분류되어야 한다.
    #[test]
    fn external_tmux_is_not_self_provided() {
        let real_tmux_sock = "/private/tmp/tmux-501/default";
        let lterm_sock = "/run/user/501/lterm.sock";
        assert!(!is_self_provided_tmux(
            real_tmux_sock,
            Some(lterm_sock),
            Some(lterm_sock),
            Some("/run/user/501/.lterm.sock.tmux-compat")
        ));
        // lterm 마커가 전혀 없어도(둘 다 None) 외부 tmux는 self가 아니다.
        assert!(!is_self_provided_tmux(real_tmux_sock, None, None, None));
    }

    /// 빈 socket 필드는 판정 불가로 self가 아니다(real_tmux=false로 떨어져 오분류 방지).
    #[test]
    fn empty_tmux_socket_field_is_not_self() {
        assert!(!is_self_provided_tmux(
            "",
            Some("/x"),
            Some("/y"),
            Some("/z")
        ));
        assert!(!is_self_provided_tmux("", None, None, None));
    }

    // ── select_status_backend 라우팅 매트릭스 (PoC1) ──

    /// 모든 신호 false + terminal_capable=true인 기준 스냅샷. 테스트가 필드만 덮어쓴다.
    fn capable_env() -> StatusEnvSnapshot {
        StatusEnvSnapshot {
            terminal_capable: true,
            forced_off: false,
            inside_cmux: false,
            real_tmux: false,
            is_iterm: false,
            iterm_native_optin: false,
        }
    }

    /// env 강제 off는 다른 모든 신호(cmux/tmux/iTerm)와 정책을 무력화하고 Disabled.
    #[test]
    fn backend_forced_off_overrides_all_signals() {
        let env = StatusEnvSnapshot {
            forced_off: true,
            inside_cmux: true,
            real_tmux: true,
            is_iterm: true,
            iterm_native_optin: true,
            ..capable_env()
        };
        for policy in [
            StatusPresencePolicy::RowAuto,
            StatusPresencePolicy::RowOff,
            StatusPresencePolicy::ForceRow,
        ] {
            assert_eq!(select_status_backend(policy, &env), StatusBackend::Disabled);
        }
    }

    /// 터미널/기하 미충족(비-TTY 등)은 Disabled.
    #[test]
    fn backend_not_capable_disables() {
        let env = StatusEnvSnapshot {
            terminal_capable: false,
            ..capable_env()
        };
        assert_eq!(
            select_status_backend(StatusPresencePolicy::ForceRow, &env),
            StatusBackend::Disabled
        );
        assert_eq!(
            select_status_backend(StatusPresencePolicy::RowAuto, &env),
            StatusBackend::Disabled
        );
    }

    /// ForceRow는 모든 위임 신호(cmux/tmux/iTerm)보다 우선해 in-terminal DECSTBM row를 강제한다.
    #[test]
    fn backend_force_row_overrides_all_delegations() {
        let env = StatusEnvSnapshot {
            inside_cmux: true,
            real_tmux: true,
            is_iterm: true,
            iterm_native_optin: true,
            ..capable_env()
        };
        assert_eq!(
            select_status_backend(StatusPresencePolicy::ForceRow, &env),
            StatusBackend::DecstbmOverlay
        );
    }

    /// cmux는 셸(RowAuto)·에이전트(RowOff) 모두 별 surface로 위임(에이전트 검사보다 먼저).
    #[test]
    fn backend_cmux_delegates_for_shell_and_agent() {
        let env = StatusEnvSnapshot {
            inside_cmux: true,
            ..capable_env()
        };
        assert_eq!(
            select_status_backend(StatusPresencePolicy::RowAuto, &env),
            StatusBackend::DelegatedSurface(SurfaceKind::Cmux)
        );
        assert_eq!(
            select_status_backend(StatusPresencePolicy::RowOff, &env),
            StatusBackend::DelegatedSurface(SurfaceKind::Cmux)
        );
    }

    /// 진짜 tmux는 status-line으로 위임(에이전트 포함). cmux가 동시 참이면 cmux 우선.
    #[test]
    fn backend_real_tmux_delegates_and_cmux_wins() {
        let tmux_only = StatusEnvSnapshot {
            real_tmux: true,
            ..capable_env()
        };
        // 에이전트(RowOff)·셸(RowAuto) 모두 tmux status-line으로 위임(정책보다 tmux가 먼저).
        assert_eq!(
            select_status_backend(StatusPresencePolicy::RowOff, &tmux_only),
            StatusBackend::DelegatedSurface(SurfaceKind::Tmux)
        );
        assert_eq!(
            select_status_backend(StatusPresencePolicy::RowAuto, &tmux_only),
            StatusBackend::DelegatedSurface(SurfaceKind::Tmux)
        );
        let both = StatusEnvSnapshot {
            real_tmux: true,
            inside_cmux: true,
            ..capable_env()
        };
        assert_eq!(
            select_status_backend(StatusPresencePolicy::RowAuto, &both),
            StatusBackend::DelegatedSurface(SurfaceKind::Cmux)
        );
    }

    /// iTerm+opt-in은 셀 그리드 밖 NativeChrome(명시 opt-in은 에이전트보다 우선).
    #[test]
    fn backend_iterm_optin_uses_native_chrome_before_agent() {
        let env = StatusEnvSnapshot {
            is_iterm: true,
            iterm_native_optin: true,
            ..capable_env()
        };
        assert_eq!(
            select_status_backend(StatusPresencePolicy::RowAuto, &env),
            StatusBackend::NativeChrome
        );
        assert_eq!(
            select_status_backend(StatusPresencePolicy::RowOff, &env),
            StatusBackend::NativeChrome
        );
    }

    /// iTerm이지만 opt-in 없으면 NativeChrome로 가지 않는다(에이전트→타이틀, 셸→DECSTBM).
    #[test]
    fn backend_iterm_without_optin_falls_through() {
        let env = StatusEnvSnapshot {
            is_iterm: true,
            ..capable_env()
        };
        assert_eq!(
            select_status_backend(StatusPresencePolicy::RowOff, &env),
            StatusBackend::TitleCueDelegation
        );
        assert_eq!(
            select_status_backend(StatusPresencePolicy::RowAuto, &env),
            StatusBackend::DecstbmOverlay
        );
    }

    /// plain 터미널: 에이전트(RowOff)는 타이틀 위임, 셸(RowAuto)은 DECSTBM best-effort.
    #[test]
    fn backend_plain_agent_delegates_shell_overlays() {
        let env = capable_env();
        assert_eq!(
            select_status_backend(StatusPresencePolicy::RowOff, &env),
            StatusBackend::TitleCueDelegation
        );
        assert_eq!(
            select_status_backend(StatusPresencePolicy::RowAuto, &env),
            StatusBackend::DecstbmOverlay
        );
    }

    // ── C4: in_grid / sink_enabled 라우팅 게이트 (R8 BLOCKER 회귀 봉인) ──

    /// R8 핵심: pill 활성(sink_enabled=true) Cmux 세션은 in_grid=false → 풀 rows.
    /// pill 비활성(sink_enabled=false) Cmux 셸 세션은 in_grid=true → DECSTBM 보존.
    /// DecstbmOverlay는 항상 rows-1 예약. DECSTBM+pill 이중 렌더 방지.
    #[test]
    fn cmux_routes_full_rows_decstbm_reserves_one() {
        let cmux_backend = StatusBackend::DelegatedSurface(SurfaceKind::Cmux);

        // Cmux + sink_enabled=true(pill 활성): off-grid → in_grid=false → 풀 rows.
        let cmux_sink_on = compute_in_grid(cmux_backend, StatusPresencePolicy::RowAuto, true);
        assert!(
            !cmux_sink_on,
            "cmux with active pill must not reserve an in-grid row"
        );
        assert_eq!(
            attach_pty_rows(40, cmux_sink_on),
            40,
            "cmux pill-on gets full rows"
        );

        // Cmux + sink_enabled=false + requests_row: LTERM_STATUS_COMMAND 미설정 셸 세션 →
        // in_grid=true → DECSTBM 행 보존(회귀 수정 핵심).
        let cmux_sink_off = compute_in_grid(cmux_backend, StatusPresencePolicy::RowAuto, false);
        assert!(
            cmux_sink_off,
            "cmux shell session (sink off) must preserve DECSTBM row"
        );
        assert_eq!(
            attach_pty_rows(40, cmux_sink_off),
            39,
            "cmux sink-off gets rows-1"
        );

        // DecstbmOverlay + RowAuto + sink_enabled=false: in_grid=true → rows-1 예약.
        let decstbm_in_grid = compute_in_grid(
            StatusBackend::DecstbmOverlay,
            StatusPresencePolicy::RowAuto,
            false,
        );
        assert!(decstbm_in_grid, "decstbm overlay reserves an in-grid row");
        assert_eq!(
            attach_pty_rows(40, decstbm_in_grid),
            39,
            "decstbm gets rows-1"
        );
    }

    /// R8: sink_enabled=true면 in_grid는 반드시 false(동시 true 불가).
    /// `!sink_enabled` 항으로 구조적 보장. DecstbmOverlay면 sink는 항상 false다.
    #[test]
    fn in_grid_and_sink_enabled_are_mutually_exclusive() {
        // Cmux + sink_enabled=true: 어떤 정책이든 in_grid=false.
        for policy in [
            StatusPresencePolicy::RowAuto,
            StatusPresencePolicy::RowOff,
            StatusPresencePolicy::ForceRow,
        ] {
            let backend = StatusBackend::DelegatedSurface(SurfaceKind::Cmux);
            let sink = compute_sink_enabled(backend, true, false);
            let in_grid = compute_in_grid(backend, policy, sink);
            assert!(sink, "sink must be enabled when command configured");
            assert!(
                !in_grid,
                "in_grid must be false when sink_enabled for policy {policy:?}"
            );
            assert!(!(in_grid && sink), "in_grid and sink must not both be true");
        }
        // DecstbmOverlay: sink는 항상 false(non-cmux backend).
        assert!(!compute_sink_enabled(
            StatusBackend::DecstbmOverlay,
            true,
            false
        ));
    }

    /// sink 활성(BLOCKER 회귀 봉인): codex 시나리오 — backend=Cmux, LTERM_STATUS_COMMAND 설정,
    /// no_status=false → sink_enabled==true. presence=RowOff(agent)여도 켜지는 게 핵심이다.
    #[test]
    fn sink_enabled_true_for_codex_cmux_with_command() {
        let backend = StatusBackend::DelegatedSurface(SurfaceKind::Cmux);
        // command 설정됨(true) + 명시 비활성 아님(false) → 켜짐. (정책 비종속이므로 policy 입력 없음.)
        assert!(compute_sink_enabled(backend, true, false));
    }

    /// 명시 비활성/명령 미설정: --no-status(explicit_no_status=true) → sink off.
    /// LTERM_STATUS_COMMAND 미설정(command_configured=false) → sink off.
    #[test]
    fn sink_enabled_false_when_disabled_or_no_command() {
        let backend = StatusBackend::DelegatedSurface(SurfaceKind::Cmux);
        // --no-status 명시.
        assert!(!compute_sink_enabled(backend, true, true));
        // LTERM_STATUS_COMMAND 미설정.
        assert!(!compute_sink_enabled(backend, false, false));
        // 둘 다.
        assert!(!compute_sink_enabled(backend, false, true));
    }

    /// non-cmux backend는 command가 설정돼 있어도 sink를 켜지 않는다(sink는 Cmux 전용).
    #[test]
    fn sink_enabled_false_for_non_cmux_backends() {
        for backend in [
            StatusBackend::Disabled,
            StatusBackend::DecstbmOverlay,
            StatusBackend::NativeChrome,
            StatusBackend::TitleCueDelegation,
            StatusBackend::DelegatedSurface(SurfaceKind::Tmux),
        ] {
            assert!(
                !compute_sink_enabled(backend, true, false),
                "{backend:?} must not enable cmux pill sink"
            );
        }
    }

    /// in_grid은 정책이 행을 원할 때만(`requests_row()`) true. DecstbmOverlay+RowOff는 false.
    /// sink_enabled=false(pill 비활성) 기준.
    #[test]
    fn in_grid_requires_requests_row() {
        assert!(compute_in_grid(
            StatusBackend::DecstbmOverlay,
            StatusPresencePolicy::RowAuto,
            false
        ));
        assert!(compute_in_grid(
            StatusBackend::DecstbmOverlay,
            StatusPresencePolicy::ForceRow,
            false
        ));
        assert!(!compute_in_grid(
            StatusBackend::DecstbmOverlay,
            StatusPresencePolicy::RowOff,
            false
        ));
    }

    #[test]
    fn refresh_payload_hides_cursor_and_restores_tracked_visibility() {
        // (d) 실제로 쓸 때 페이로드는 `\x1b[?25l`(숨김)로 시작하고 추적된 visibility로 복원한다.
        // terminal_state=None(보임 가정) → `\x1b[?25h`로 끝난다.
        let mut visible_bar = dedup_test_status_bar(None);
        let mut visible_payload = Vec::new();
        assert!(
            visible_bar
                .refresh(&mut visible_payload)
                .expect("visible refresh"),
            "first refresh draws"
        );
        let visible = String::from_utf8(visible_payload).expect("payload utf8");
        assert!(
            visible.starts_with("\x1b[?25l"),
            "refresh payload must start by hiding the cursor: {visible:?}"
        );
        assert!(
            visible.ends_with("\x1b[?25h"),
            "refresh payload must restore cursor visible by default: {visible:?}"
        );
        visible_bar.style = None;

        // PTY가 커서를 숨긴 상태면 `\x1b[?25l`로 닫는다.
        let state = Arc::new(AltScreenState::default());
        state.cursor_visible.store(false, Ordering::Relaxed);
        let mut hidden_bar = StatusBar {
            session_name: "omx-lterm".to_string(),
            pane_id: "%0".to_string(),
            style: Some(StatusStyle::Full(StatusTheme::Blue)),
            drawn_status_rows: Vec::new(),
            preserve_sgr_stack: true,
            command_line: None,
            command_allow_color: false,
            terminal_state: Some(state),
            last_body: None,
            force_redraw: true,
            force_content_redraw: false,
        };
        let mut hidden_payload = Vec::new();
        assert!(
            hidden_bar
                .refresh(&mut hidden_payload)
                .expect("hidden refresh"),
            "first refresh draws"
        );
        let hidden = String::from_utf8(hidden_payload).expect("payload utf8");
        assert!(
            hidden.starts_with("\x1b[?25l"),
            "refresh payload must start by hiding the cursor: {hidden:?}"
        );
        assert!(
            hidden.ends_with("\x1b[?25l"),
            "refresh payload must keep cursor hidden when PTY had it hidden: {hidden:?}"
        );
        hidden_bar.style = None;
    }

    #[test]
    fn status_bar_redraw_clears_rows_hidden_by_shrink_then_growth() {
        let mut status_bar = StatusBar {
            session_name: "omx-lterm".to_string(),
            pane_id: "%0".to_string(),
            style: Some(StatusStyle::Full(StatusTheme::Blue)),
            drawn_status_rows: Vec::new(),
            preserve_sgr_stack: true,
            command_line: None,
            command_allow_color: false,
            terminal_state: None,
            last_body: None,
            force_redraw: true,
            force_content_redraw: false,
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
            preserve_sgr_stack: true,
            command_line: None,
            command_allow_color: false,
            terminal_state: None,
            last_body: None,
            force_redraw: true,
            force_content_redraw: false,
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
            payload.contains("\x1b[0m\x1b[K\x1b[#}\x1b8"),
            "same-row redraws should still clear from the padded status text to line end, covering the intentionally unwritten final column without a full-row clear: {payload:?}"
        );
        status_bar.style = None;
    }

    #[test]
    fn status_bar_redraw_preserves_application_sgr_state() {
        let mut status_bar = StatusBar {
            session_name: "omx-lterm".to_string(),
            pane_id: "%0".to_string(),
            style: Some(StatusStyle::Full(StatusTheme::Blue)),
            drawn_status_rows: Vec::new(),
            preserve_sgr_stack: true,
            command_line: None,
            command_allow_color: false,
            terminal_state: None,
            last_body: None,
            force_redraw: true,
            force_content_redraw: false,
        };
        let mut output = Vec::new();

        status_bar
            .draw_at_size(&mut output, 80, 24)
            .expect("draw status");

        let payload = String::from_utf8(output).expect("status payload should be utf8");
        assert!(
            payload.starts_with("\x1b[?25l\x1b7\x1b[#{"),
            "status redraw should hide cursor, then push SGR before any host-side resets: {payload:?}"
        );
        assert!(
            payload.ends_with("\x1b[#}\x1b8\x1b[?25h"),
            "status redraw should pop SGR, restore cursor position, then restore cursor visibility: {payload:?}"
        );
        status_bar.style = None;
    }

    #[test]
    fn status_bar_restore_preserves_application_sgr_state() {
        let mut status_bar = StatusBar {
            session_name: "omx-lterm".to_string(),
            pane_id: "%0".to_string(),
            style: Some(StatusStyle::Full(StatusTheme::Blue)),
            drawn_status_rows: vec![20],
            preserve_sgr_stack: true,
            command_line: None,
            command_allow_color: false,
            terminal_state: None,
            last_body: None,
            force_redraw: true,
            force_content_redraw: false,
        };
        let mut output = Vec::new();

        status_bar.restore(&mut output).expect("restore status");

        let payload = String::from_utf8(output).expect("restore payload should be utf8");
        assert!(
            payload.starts_with("\x1b7\x1b[#{\x1b[r"),
            "status restore should push SGR before host-side resets: {payload:?}"
        );
        assert!(
            payload.ends_with("\x1b[#}\x1b8"),
            "status restore should pop SGR before restoring the application cursor: {payload:?}"
        );
        status_bar.style = None;
    }

    #[test]
    fn status_bar_redraw_and_restore_skip_sgr_stack_when_disabled() {
        let mut status_bar = StatusBar {
            session_name: "omx-lterm".to_string(),
            pane_id: "%0".to_string(),
            style: Some(StatusStyle::Full(StatusTheme::Blue)),
            drawn_status_rows: vec![20],
            preserve_sgr_stack: false,
            command_line: None,
            command_allow_color: false,
            terminal_state: None,
            last_body: None,
            force_redraw: true,
            force_content_redraw: false,
        };
        let mut output = Vec::new();

        status_bar
            .draw_at_size(&mut output, 80, 24)
            .expect("draw status without sgr stack");
        let draw_payload = String::from_utf8(output.clone()).expect("draw payload should be utf8");
        assert!(
            !draw_payload.contains("\x1b[#{") && !draw_payload.contains("\x1b[#}"),
            "status redraw must not emit private SGR stack controls when disabled: {draw_payload:?}"
        );
        assert!(
            draw_payload.starts_with("\x1b[?25l\x1b7"),
            "status redraw still hides cursor then saves it without SGR stack: {draw_payload:?}"
        );
        assert!(
            draw_payload.ends_with("\x1b8\x1b[?25h"),
            "status redraw still restores cursor position then visibility without SGR stack: {draw_payload:?}"
        );
        assert!(
            draw_payload.contains("\x1b[24;1H\x1b[2K\x1b[0;30;104m"),
            "status redraw still paints the status row when SGR stack is disabled: {draw_payload:?}"
        );
        assert!(
            draw_payload.contains("\x1b[0m\x1b[K\x1b8"),
            "status redraw still resets/clears and restores cursor when SGR stack is disabled: {draw_payload:?}"
        );

        output.clear();
        status_bar
            .restore(&mut output)
            .expect("restore status without sgr stack");
        let restore_payload = String::from_utf8(output).expect("restore payload should be utf8");
        assert!(
            !restore_payload.contains("\x1b[#{") && !restore_payload.contains("\x1b[#}"),
            "status restore must not emit private SGR stack controls when disabled: {restore_payload:?}"
        );
        assert!(
            restore_payload.starts_with("\x1b7\x1b[r"),
            "status restore still resets scroll region after saving cursor: {restore_payload:?}"
        );
        assert!(
            restore_payload.ends_with("\x1b[0m\x1b[2K\x1b8"),
            "status restore still clears the active status row and restores cursor: {restore_payload:?}"
        );
        status_bar.style = None;
    }

    // ===== command-backed status: draw_at_size 분기 =====

    #[test]
    fn status_bar_command_line_with_allow_color_omits_theme_bg() {
        // command_allow_color=true면 테마 bg SGR(`\x1b[0;30;104m`)을 입히지 않고
        // reset(`\x1b[0m`)으로 시작해 understatus 자체 색이 살아야 한다.
        let mut status_bar = StatusBar {
            session_name: "omx-lterm".to_string(),
            pane_id: "%0".to_string(),
            style: Some(StatusStyle::Full(StatusTheme::Blue)),
            drawn_status_rows: Vec::new(),
            preserve_sgr_stack: true,
            // 살균을 거친(완결 SGR) 형태를 직접 주입한다.
            command_line: Some("\x1b[31mstatus\x1b[0m".to_string()),
            command_allow_color: true,
            terminal_state: None,
            last_body: None,
            force_redraw: true,
            force_content_redraw: false,
        };
        let mut output = Vec::new();
        status_bar
            .draw_at_size(&mut output, 80, 24)
            .expect("draw command-backed status with color");
        let payload = String::from_utf8(output).expect("status payload should be utf8");

        // 테마 bg는 들어가지 않는다.
        assert!(
            !payload.contains("\x1b[0;30;104m"),
            "allow_color command status must not paint theme bg: {payload:?}"
        );
        // status row는 위치 지정 + reset으로 시작한다(테마 bg 대신 \x1b[0m).
        assert!(
            payload.contains("\x1b[24;1H\x1b[2K\x1b[0m\x1b[31mstatus"),
            "allow_color command status should start with reset then command content: {payload:?}"
        );
        // 명령 콘텐츠가 포함된다.
        assert!(
            payload.contains("status"),
            "command content must be present: {payload:?}"
        );
        // 끝 보호장치: \x1b[0m\x1b[K 유지.
        assert!(
            payload.contains("\x1b[0m\x1b[K"),
            "trailing reset + clear-to-eol must be preserved: {payload:?}"
        );
        // cursor hide + scroll-region/cursor save·restore + SGR stack 보호장치 + cursor 복원 유지.
        assert!(
            payload.starts_with("\x1b[?25l\x1b7\x1b[#{"),
            "cursor hide + cursor save + SGR push preserved: {payload:?}"
        );
        assert!(
            payload.ends_with("\x1b[#}\x1b8\x1b[?25h"),
            "SGR pop + cursor position restore + cursor visibility restore preserved: {payload:?}"
        );
        status_bar.style = None;
    }

    #[test]
    fn status_bar_command_line_plain_without_color_keeps_theme_bg() {
        // command_allow_color=false면 plain 콘텐츠라도 테마 bg를 유지해 fallback과
        // 동일한 스타일로 그린다.
        let mut status_bar = StatusBar {
            session_name: "omx-lterm".to_string(),
            pane_id: "%0".to_string(),
            style: Some(StatusStyle::Full(StatusTheme::Blue)),
            drawn_status_rows: Vec::new(),
            preserve_sgr_stack: true,
            command_line: Some("plain-status".to_string()),
            command_allow_color: false,
            terminal_state: None,
            last_body: None,
            force_redraw: true,
            force_content_redraw: false,
        };
        let mut output = Vec::new();
        status_bar
            .draw_at_size(&mut output, 80, 24)
            .expect("draw command-backed plain status");
        let payload = String::from_utf8(output).expect("status payload should be utf8");

        assert!(
            payload.contains("\x1b[24;1H\x1b[2K\x1b[0;30;104mplain-status"),
            "plain command status with color disabled keeps theme bg: {payload:?}"
        );
        assert!(
            payload.contains("\x1b[0m\x1b[K"),
            "trailing reset + clear-to-eol must be preserved: {payload:?}"
        );
        status_bar.style = None;
    }

    #[test]
    fn status_bar_command_line_none_matches_format_status_line_bytes() {
        // command_line=None이면 기존 format_status_line 경로와 바이트 동일이어야 한다(회귀 0).
        let mut command_bar = StatusBar {
            session_name: "api".to_string(),
            pane_id: "%1".to_string(),
            style: Some(StatusStyle::Full(StatusTheme::Blue)),
            drawn_status_rows: Vec::new(),
            preserve_sgr_stack: true,
            command_line: None,
            // allow_color가 true라도 command_line이 None이면 fallback이라 영향 없어야 한다.
            command_allow_color: true,
            terminal_state: None,
            last_body: None,
            force_redraw: true,
            force_content_redraw: false,
        };
        let mut command_output = Vec::new();
        command_bar
            .draw_at_size(&mut command_output, 80, 24)
            .expect("draw fallback status");

        // command-backed 필드를 전혀 안 쓰는 기준 인스턴스.
        let mut baseline_bar = StatusBar {
            session_name: "api".to_string(),
            pane_id: "%1".to_string(),
            style: Some(StatusStyle::Full(StatusTheme::Blue)),
            drawn_status_rows: Vec::new(),
            preserve_sgr_stack: true,
            command_line: None,
            command_allow_color: false,
            terminal_state: None,
            last_body: None,
            force_redraw: true,
            force_content_redraw: false,
        };
        let mut baseline_output = Vec::new();
        baseline_bar
            .draw_at_size(&mut baseline_output, 80, 24)
            .expect("draw baseline status");

        assert_eq!(
            command_output, baseline_output,
            "None command_line must produce byte-identical output to format_status_line path"
        );
        // 실제로 format_status_line 콘텐츠가 들어갔는지도 확인.
        let payload = String::from_utf8(command_output).expect("status payload should be utf8");
        assert!(
            payload.contains(&format_status_line("api", "%1", 79)),
            "fallback must render format_status_line content: {payload:?}"
        );
        command_bar.style = None;
        baseline_bar.style = None;
    }

    #[test]
    fn status_bar_command_line_long_is_ansi_truncated_without_dangling_esc() {
        // 긴 command_line이 safe_width(=cols-1)로 ANSI-aware 절단되고 미완 ESC가 없어야 한다.
        let long = format!("\x1b[31m{}\x1b[0m", "x".repeat(200));
        let mut status_bar = StatusBar {
            session_name: "omx-lterm".to_string(),
            pane_id: "%0".to_string(),
            style: Some(StatusStyle::Full(StatusTheme::Blue)),
            drawn_status_rows: Vec::new(),
            preserve_sgr_stack: true,
            command_line: Some(long),
            command_allow_color: true,
            terminal_state: None,
            last_body: None,
            force_redraw: true,
            force_content_redraw: false,
        };
        let mut output = Vec::new();
        // cols=20 → safe_width=19.
        status_bar
            .draw_at_size(&mut output, 20, 24)
            .expect("draw long command-backed status");
        let payload = String::from_utf8(output).expect("status payload should be utf8");

        // 절단된 콘텐츠는 truncate_status_line_ansi 결과와 일치해야 한다.
        let expected_content = crate::sanitize::truncate_status_line_ansi(
            &format!("\x1b[31m{}\x1b[0m", "x".repeat(200)),
            19,
        );
        assert!(
            payload.contains(&expected_content),
            "long command line must be ANSI-truncated to safe_width: {payload:?}"
        );
        // status row 위치 지정 직후 마지막 escape는 완결된 SGR(`m`)이어야 한다 — 미완 ESC 금지.
        // 페이로드 전체에서 마지막 ESC가 `m`으로 끝나는지(보호장치 \x1b8 제외) 확인.
        if let Some(body_start) = payload.find("\x1b[24;1H") {
            let body = &payload[body_start..];
            // 본문 영역의 SGR은 모두 `m`으로 종결됨을 보장하기 위해 미완 `\x1b[<숫자>` 잔존이 없어야 한다.
            assert!(
                !body.contains("\x1b[31\x1b8") && !body.ends_with("\x1b[31"),
                "no dangling ESC may remain in truncated body: {body:?}"
            );
        }
        status_bar.style = None;
    }

    #[test]
    fn status_bar_sgr_stack_support_is_gated_and_overridable() {
        let _lock = crate::TEST_ENV_LOCK.lock().unwrap();
        let _env_guard = EnvGuard::capture(&[
            "TERM",
            "TERM_PROGRAM",
            "LC_TERMINAL",
            "TERMINAL_EMULATOR",
            "LTERM_STATUS_SGR_STACK",
        ]);

        // SAFETY: crate::TEST_ENV_LOCK is held; EnvGuard restores on drop.
        unsafe {
            std::env::set_var("TERM", "dumb");
            std::env::remove_var("TERM_PROGRAM");
            std::env::remove_var("LC_TERMINAL");
            std::env::remove_var("TERMINAL_EMULATOR");
            std::env::remove_var("LTERM_STATUS_SGR_STACK");
        }
        assert!(
            !status_sgr_stack_supported(),
            "unknown/dumb terminals should not get xterm-private SGR stack by default"
        );

        for generic_term in [
            "xterm-256color",
            "tmux-256color",
            "screen-256color",
            "ansi",
            "vt100",
        ] {
            // SAFETY: crate::TEST_ENV_LOCK is held; EnvGuard restores on drop.
            unsafe {
                std::env::set_var("TERM", generic_term);
                std::env::remove_var("TERM_PROGRAM");
                std::env::remove_var("LC_TERMINAL");
                std::env::remove_var("TERMINAL_EMULATOR");
            }
            assert!(
                !status_sgr_stack_supported(),
                "generic TERM={generic_term} alone is too broad for private SGR stack auto-enable"
            );
        }

        for identity in ["xterm", "iTerm.app", "WezTerm"] {
            // SAFETY: crate::TEST_ENV_LOCK is held; EnvGuard restores on drop.
            unsafe {
                std::env::set_var("TERM", "xterm-256color");
                std::env::set_var("TERM_PROGRAM", identity);
                std::env::remove_var("LC_TERMINAL");
                std::env::remove_var("TERMINAL_EMULATOR");
            }
            assert!(
                status_sgr_stack_supported(),
                "recognized terminal identity {identity} should preserve app SGR by default"
            );
        }

        for unverified_identity in ["kitty", "Alacritty", "Ghostty", "Termius"] {
            // SAFETY: crate::TEST_ENV_LOCK is held; EnvGuard restores on drop.
            unsafe {
                std::env::set_var("TERM", "xterm-256color");
                std::env::set_var("TERM_PROGRAM", unverified_identity);
                std::env::remove_var("LC_TERMINAL");
                std::env::remove_var("TERMINAL_EMULATOR");
            }
            assert!(
                !status_sgr_stack_supported(),
                "unverified terminal identity {unverified_identity} must stay opt-in for private SGR stack"
            );
        }

        for unverified_term in ["xterm-kitty", "xterm-ghostty"] {
            // SAFETY: crate::TEST_ENV_LOCK is held; EnvGuard restores on drop.
            unsafe {
                std::env::set_var("TERM", unverified_term);
                std::env::remove_var("TERM_PROGRAM");
                std::env::remove_var("LC_TERMINAL");
                std::env::remove_var("TERMINAL_EMULATOR");
            }
            assert!(
                !status_sgr_stack_supported(),
                "specific TERM={unverified_term} remains opt-in until SGR stack support is verified"
            );
        }

        for verified_term in ["xterm", "wezterm"] {
            // SAFETY: crate::TEST_ENV_LOCK is held; EnvGuard restores on drop.
            unsafe {
                std::env::set_var("TERM", verified_term);
                std::env::remove_var("TERM_PROGRAM");
                std::env::remove_var("LC_TERMINAL");
                std::env::remove_var("TERMINAL_EMULATOR");
            }
            assert!(
                status_sgr_stack_supported(),
                "specific TERM={verified_term} should preserve app SGR by default"
            );
        }

        // SAFETY: crate::TEST_ENV_LOCK is held; EnvGuard restores on drop.
        unsafe {
            std::env::set_var("LTERM_STATUS_SGR_STACK", "0");
        }
        assert!(
            !status_sgr_stack_supported(),
            "explicit opt-out must disable private SGR stack sequences"
        );

        // SAFETY: crate::TEST_ENV_LOCK is held; EnvGuard restores on drop.
        unsafe {
            std::env::set_var("TERM", "dumb");
            std::env::set_var("LTERM_STATUS_SGR_STACK", "1");
        }
        assert!(
            status_sgr_stack_supported(),
            "explicit opt-in should be available for verified terminals"
        );
    }

    #[test]
    fn status_damage_output_defers_status_repaint_until_later_refresh() {
        let mut output = Vec::new();
        let mut status_dirty = false;
        let agent_redraw = b"\x1b[2Jagent prompt redraw";

        let detached = forward_pty_output_frame_or_detached(
            &mut output,
            agent_redraw,
            true,
            &mut status_dirty,
        )
        .expect("forward pty output");

        assert!(!detached);
        assert_eq!(
            output, agent_redraw,
            "status-damaging PTY output must be flushed without interleaving a host status repaint"
        );
        assert!(
            status_dirty,
            "status damage must be remembered for the heartbeat path"
        );

        let mut status_bar = StatusBar {
            session_name: "omx-lterm".to_string(),
            pane_id: "%0".to_string(),
            style: Some(StatusStyle::Full(StatusTheme::Blue)),
            drawn_status_rows: Vec::new(),
            preserve_sgr_stack: true,
            command_line: None,
            command_allow_color: false,
            terminal_state: None,
            last_body: None,
            force_redraw: true,
            force_content_redraw: false,
        };
        status_bar
            .draw_at_size(&mut output, 80, 24)
            .expect("later explicit status refresh");
        let delayed_payload = String::from_utf8(output).expect("status payload should be utf8");
        assert!(
            delayed_payload.contains("\x1b[24;1H\x1b[2K\x1b[0;30;104m"),
            "a later refresh should still repaint the dirty status row: {delayed_payload:?}"
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
            "NO_COLOR",
            "FORCE_COLOR",
            "CLICOLOR",
            "CLICOLOR_FORCE",
            "LTERM_AGENT",
        ]);

        // SAFETY: crate::TEST_ENV_LOCK is held; EnvGuard restores on drop.
        unsafe {
            std::env::set_var("TERM", "xterm-256color");
            std::env::set_var("COLORTERM", "truecolor");
            std::env::set_var("TERM_PROGRAM", "Termius");
            std::env::set_var("LC_TERMINAL", "iTerm2");
            std::env::set_var("NO_COLOR", "1");
            std::env::set_var("FORCE_COLOR", "3");
            std::env::set_var("CLICOLOR", "0");
            std::env::set_var("CLICOLOR_FORCE", "1");
            std::env::set_var("LTERM_AGENT", "host-value");
        }

        let mut env = std::collections::HashMap::from([
            ("LTERM_AGENT".to_string(), "omx".to_string()),
            ("LC_TERMINAL".to_string(), "explicit-client".to_string()),
        ]);
        super::inherit_terminal_capability_env(&mut env);
        super::inherit_child_color_policy_env_unless_agent(&mut env);

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
            "caller-supplied session env should stay authoritative"
        );
        for key in ["NO_COLOR", "FORCE_COLOR", "CLICOLOR", "CLICOLOR_FORCE"] {
            assert!(
                !env.contains_key(key),
                "{key} is an application color policy, not a terminal capability, and must not leak into child agent sessions"
            );
        }
    }

    #[test]
    fn new_sessions_inherit_codex_home_without_overwriting_explicit_values() {
        let _lock = crate::TEST_ENV_LOCK.lock().unwrap();
        let _env_guard =
            EnvGuard::capture(&["CODEX_HOME", "LTERM_SHOULD_NOT_FORWARD_CODEX_HOME_TEST"]);

        // SAFETY: crate::TEST_ENV_LOCK is held; EnvGuard restores on drop.
        unsafe {
            std::env::set_var("CODEX_HOME", "/tmp/lterm-client-codex-home");
            std::env::set_var("LTERM_SHOULD_NOT_FORWARD_CODEX_HOME_TEST", "client-only");
        }

        let mut inherited = std::collections::HashMap::new();
        super::inherit_client_session_home_env(&mut inherited);
        assert_eq!(
            inherited.get("CODEX_HOME").map(String::as_str),
            Some("/tmp/lterm-client-codex-home")
        );
        assert!(
            !inherited.contains_key("LTERM_SHOULD_NOT_FORWARD_CODEX_HOME_TEST"),
            "client session home inheritance must remain narrowly allowlisted"
        );

        let mut explicit = std::collections::HashMap::from([(
            "CODEX_HOME".to_string(),
            "/tmp/explicit-codex-home".to_string(),
        )]);
        super::inherit_client_session_home_env(&mut explicit);
        assert_eq!(
            explicit.get("CODEX_HOME").map(String::as_str),
            Some("/tmp/explicit-codex-home"),
            "caller-supplied session env should stay authoritative"
        );

        // SAFETY: crate::TEST_ENV_LOCK is held; EnvGuard restores on drop.
        unsafe {
            std::env::set_var("CODEX_HOME", "");
        }
        let mut empty = std::collections::HashMap::new();
        super::inherit_client_session_home_env(&mut empty);
        assert!(
            !empty.contains_key("CODEX_HOME"),
            "empty client CODEX_HOME should not inject an empty session env"
        );

        // SAFETY: crate::TEST_ENV_LOCK is held; EnvGuard restores on drop.
        unsafe {
            std::env::remove_var("CODEX_HOME");
        }
        let mut absent = std::collections::HashMap::new();
        super::inherit_client_session_home_env(&mut absent);
        assert!(
            !absent.contains_key("CODEX_HOME"),
            "missing client CODEX_HOME should not inject an empty session env"
        );
    }

    #[test]
    fn plain_new_sessions_inherit_current_color_policy_env_without_overwriting_explicit_values() {
        let _lock = crate::TEST_ENV_LOCK.lock().unwrap();
        let _env_guard = EnvGuard::capture(&[
            "NO_COLOR",
            "FORCE_COLOR",
            "CLICOLOR",
            "CLICOLOR_FORCE",
            "LTERM_AGENT",
        ]);

        // SAFETY: crate::TEST_ENV_LOCK is held; EnvGuard restores on drop.
        unsafe {
            std::env::set_var("NO_COLOR", "1");
            std::env::set_var("FORCE_COLOR", "3");
            std::env::set_var("CLICOLOR", "0");
            std::env::set_var("CLICOLOR_FORCE", "1");
            std::env::remove_var("LTERM_AGENT");
        }

        let mut env = std::collections::HashMap::from([(
            "CLICOLOR_FORCE".to_string(),
            "explicit-client".to_string(),
        )]);
        super::inherit_child_color_policy_env_unless_agent(&mut env);

        assert_eq!(env.get("NO_COLOR").map(String::as_str), Some("1"));
        assert_eq!(env.get("FORCE_COLOR").map(String::as_str), Some("3"));
        assert_eq!(env.get("CLICOLOR").map(String::as_str), Some("0"));
        assert_eq!(
            env.get("CLICOLOR_FORCE").map(String::as_str),
            Some("explicit-client"),
            "caller-supplied session env should stay authoritative"
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
    fn normal_attach_cleanup_skips_alt_screen_exit_when_not_observed() {
        let bytes = normal_attach_terminal_cleanup_bytes(false);
        assert!(
            !bytes
                .windows(b"\x1b[?1049l".len())
                .any(|w| w == b"\x1b[?1049l"),
            "normal cleanup must not unconditionally leave alt-screen"
        );
        assert!(
            !bytes.windows(b"\x1b[?47l".len()).any(|w| w == b"\x1b[?47l"),
            "normal cleanup must not emit legacy alt-screen exit unless observed"
        );
        assert!(bytes.starts_with(b"\x1b[r"));
        assert!(bytes.windows(6).any(|w| w == b"\x1b[?25h"));
        assert!(bytes.windows(8).any(|w| w == b"\x1b[?2004l"));
        assert!(bytes.ends_with(b"\x1b[0m\r\n"));
    }

    #[test]
    fn normal_attach_cleanup_exits_alt_screen_only_when_observed() {
        let bytes = normal_attach_terminal_cleanup_bytes(true);
        let pos_alt = bytes
            .windows(b"\x1b[?1049l".len())
            .position(|w| w == b"\x1b[?1049l")
            .expect("conditional alt-screen exit");
        let pos_scroll = bytes
            .windows(b"\x1b[r".len())
            .position(|w| w == b"\x1b[r")
            .expect("scroll reset in normal cleanup");
        assert!(
            pos_alt < pos_scroll,
            "conditional alt-screen exit must happen before scroll reset"
        );
        assert!(bytes.windows(b"\x1b[?47l".len()).any(|w| w == b"\x1b[?47l"));
        assert!(
            bytes
                .windows(b"\x1b[?1047l".len())
                .any(|w| w == b"\x1b[?1047l")
        );
        assert!(bytes.ends_with(b"\x1b[0m\r\n"));
    }

    #[test]
    fn normal_attach_cleanup_keeps_raw_recovery_minimal_and_ordered() {
        const ALT_SCREEN_EXIT_1049: &[u8] = b"\x1b[?1049l";
        const SCROLL_REGION_RESET: &[u8] = b"\x1b[r";
        const CURSOR_SHOW: &[u8] = b"\x1b[?25h";
        const BRACKETED_PASTE_DISABLE: &[u8] = b"\x1b[?2004l";
        const SGR_RESET: &[u8] = b"\x1b[0m";

        let bytes = normal_attach_terminal_cleanup_bytes(false);
        assert!(
            !bytes.windows(b"\x1b]52;".len()).any(|w| w == b"\x1b]52;"),
            "normal raw attach cleanup must not introduce clipboard-capable OSC controls"
        );
        assert!(
            !bytes
                .windows(ALT_SCREEN_EXIT_1049.len())
                .any(|w| w == ALT_SCREEN_EXIT_1049),
            "normal cleanup without observed alt-screen must stay on the main screen"
        );

        let pos_scroll = bytes
            .windows(SCROLL_REGION_RESET.len())
            .position(|w| w == SCROLL_REGION_RESET)
            .expect("scroll-region reset in normal cleanup");
        let pos_cursor = bytes
            .windows(CURSOR_SHOW.len())
            .position(|w| w == CURSOR_SHOW)
            .expect("cursor show in normal cleanup");
        let pos_bracketed_paste = bytes
            .windows(BRACKETED_PASTE_DISABLE.len())
            .position(|w| w == BRACKETED_PASTE_DISABLE)
            .expect("bracketed paste disable in normal cleanup");
        let pos_sgr_reset = bytes
            .windows(SGR_RESET.len())
            .rposition(|w| w == SGR_RESET)
            .expect("final SGR reset in normal cleanup");

        assert!(
            pos_scroll < pos_cursor
                && pos_cursor < pos_bracketed_paste
                && pos_bracketed_paste < pos_sgr_reset,
            "normal cleanup order should be scroll reset -> cursor show -> bracketed paste off -> final local SGR reset: {bytes:?}"
        );
        assert_eq!(
            bytes
                .windows(SGR_RESET.len())
                .filter(|w| *w == SGR_RESET)
                .count(),
            1,
            "normal cleanup should emit exactly one host-local SGR reset"
        );
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
        let mut tracker =
            TerminalOutputTracker::new(Arc::clone(&state), Arc::clone(&alt), Some(23));
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
        let mut tracker =
            TerminalOutputTracker::new(Arc::clone(&state), Arc::clone(&alt), Some(23));
        tracker.observe(b"plain output");
        tracker.observe(b"\x1b[>1u");
        assert_eq!(state.kitty_push_depth.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn terminal_output_tracker_marks_status_damage_for_screen_erases_and_scroll_resets() {
        let state = Arc::new(KeyboardProtocolRestoreState::default());
        let alt = Arc::new(AltScreenState::default());
        let mut tracker =
            TerminalOutputTracker::new(Arc::clone(&state), Arc::clone(&alt), Some(23));

        assert!(
            !tracker
                .observe(b"plain \x1b[31mred\x1b[0m")
                .status_area_dirty,
            "plain text and SGR should not force status redraw"
        );
        assert!(
            tracker.observe(b"\x1b[J").status_area_dirty,
            "ED from cursor to end can erase the host status row"
        );
        assert!(
            tracker.observe(b"\x1b[2J").status_area_dirty,
            "full-screen erase must mark the status row dirty"
        );
        assert!(
            tracker.observe(b"\x1b[3J").status_area_dirty,
            "scrollback erase can still accompany a status-row clearing redraw"
        );
        assert!(
            tracker.observe(b"\x1b[1J").status_area_dirty,
            "erase start-to-cursor can touch the status row after cursor movement"
        );
        assert!(
            tracker.observe(b"\x1b[r").status_area_dirty,
            "DECSTBM reset must mark status reservation dirty"
        );
        assert!(
            !tracker.observe(b"\x1b[1;23r").status_area_dirty,
            "body-only DECSTBM should not repaint on every prompt redraw"
        );
        assert!(
            tracker.observe(b"\x1b[1;24r").status_area_dirty,
            "full-height DECSTBM includes the reserved status row"
        );
        assert!(
            tracker.observe(b"\x1bc").status_area_dirty,
            "RIS must mark the status row dirty"
        );
        assert!(
            tracker.observe(b"\x1b[?2J").status_area_dirty,
            "private/selective ED should conservatively mark the status row dirty"
        );
        assert!(
            !tracker.observe("śr".as_bytes()).status_area_dirty,
            "UTF-8 continuation bytes must not be misread as raw C1 CSI"
        );
    }

    #[test]
    fn terminal_output_tracker_marks_status_damage_across_chunk_boundaries() {
        let state = Arc::new(KeyboardProtocolRestoreState::default());
        let alt = Arc::new(AltScreenState::default());
        let mut tracker =
            TerminalOutputTracker::new(Arc::clone(&state), Arc::clone(&alt), Some(23));

        assert!(!tracker.observe(b"prompt\x1b[").status_area_dirty);
        assert!(
            tracker.observe(b"2Jafter").status_area_dirty,
            "split CSI ED should still mark the status row dirty"
        );
        assert!(!tracker.observe(b"more\x1b").status_area_dirty);
        assert!(
            tracker.observe(b"cafter").status_area_dirty,
            "split RIS should still mark the status row dirty"
        );
        assert!(
            !tracker.observe("moreś".as_bytes()).status_area_dirty,
            "UTF-8 continuation bytes split near ASCII must not look like CSI"
        );
        assert!(
            !tracker.observe(b"Jafter").status_area_dirty,
            "plain ASCII after UTF-8 text is not a split raw CSI sequence"
        );
    }

    #[test]
    fn broken_pipe_detection_sees_context_wrapped_io_errors() {
        let err =
            anyhow::Error::new(std::io::Error::from(ErrorKind::BrokenPipe)).context("flush stdout");
        assert!(anyhow_error_is_broken_pipe(&err));

        let other =
            anyhow::Error::new(std::io::Error::from(ErrorKind::ConnectionReset)).context("flush");
        assert!(!anyhow_error_is_broken_pipe(&other));
    }

    #[test]
    fn attach_input_thread_join_surfaces_worker_errors() {
        let handle = thread::spawn(|| -> anyhow::Result<()> { anyhow::bail!("stdin failed") });

        let err = join_attach_input_thread(handle).expect_err("input thread error");

        let err = format!("{err:#}");
        assert!(err.contains("attach input thread failed"), "{err}");
        assert!(err.contains("stdin failed"), "{err}");
    }

    #[test]
    fn attach_result_reports_input_error_when_output_loop_is_clean() {
        let err = finish_attach_results(Ok(()), Err(anyhow::anyhow!("write pty input")))
            .expect_err("input error should propagate");

        assert!(format!("{err:#}").contains("write pty input"));
    }

    #[test]
    fn attach_result_preserves_output_error_and_mentions_input_error() {
        let err = finish_attach_results(
            Err(anyhow::anyhow!("read pty output")),
            Err(anyhow::anyhow!("read stdin")),
        )
        .expect_err("output error should remain primary");

        let err = format!("{err:#}");
        assert!(
            err.starts_with("read pty output"),
            "output error must stay first: {err}"
        );
        assert!(err.contains("attach input thread also failed"), "{err}");
        assert!(err.contains("read stdin"), "{err}");
    }

    #[test]
    fn attach_failure_diagnosis_uses_immutable_uuid_and_distinguishes_lifecycle_state() {
        let original = sample_session_info("reused-name", "sh", None);

        let mut healthy = original.clone();
        healthy.lifecycle_state = Some(SessionLifecycleState::Healthy);
        let diagnosis = format_attach_failure_diagnosis(&original, Some(&healthy), &[])
            .expect("healthy diagnosis");
        assert!(diagnosis.contains("session remains alive"), "{diagnosis}");
        assert!(diagnosis.contains("lterm resume"), "{diagnosis}");

        let mut degraded = original.clone();
        degraded.lifecycle_state = Some(SessionLifecycleState::MonitorFailed);
        let diagnosis = format_attach_failure_diagnosis(&original, Some(&degraded), &[])
            .expect("monitor-failed diagnosis");
        assert!(diagnosis.contains("leader state is unknown"), "{diagnosis}");
        assert!(!diagnosis.contains("remains alive"), "{diagnosis}");
        assert!(!diagnosis.contains("lterm resume"), "{diagnosis}");

        let mut ending = original.clone();
        ending.alive = false;
        ending.lifecycle_state = Some(SessionLifecycleState::Ending {
            trigger: SessionExitTrigger::CloseRequested,
        });
        let diagnosis = format_attach_failure_diagnosis(&original, Some(&ending), &[])
            .expect("ending diagnosis");
        assert!(diagnosis.contains("session is ending"), "{diagnosis}");
        assert!(diagnosis.contains("close_requested"), "{diagnosis}");
        assert!(!diagnosis.contains("lterm resume"), "{diagnosis}");

        let mut reused = original.clone();
        reused.id = "different-uuid".to_string();
        let exit = RecentSessionExit {
            schema_version: "1.0".to_string(),
            session_id: original.id.clone(),
            name: original.name.clone(),
            pane_id: original.pane_id.clone(),
            parent_session_id: None,
            parent_pane_id: None,
            agent_name: None,
            created_unix_ms: 1,
            trigger_claimed_unix_ms: 2,
            reaped_unix_ms: Some(3),
            trigger: SessionExitTrigger::LeaderExited,
            outcome_state: ExitOutcomeState::Complete,
            exit_code: Some(37),
            signal: None,
            evidence_state: ExitEvidenceState::Complete,
        };
        let diagnosis = format_attach_failure_diagnosis(&original, Some(&reused), &[exit])
            .expect("recorded exit diagnosis");
        assert!(
            diagnosis.contains("session ended during attach"),
            "{diagnosis}"
        );
        assert!(diagnosis.contains("exit_code=37"), "{diagnosis}");
        assert!(!diagnosis.contains("session remains alive"), "{diagnosis}");
    }

    #[test]
    fn attach_diagnostic_suffix_keeps_the_original_output_error_first() {
        let original = sample_session_info("agent", "sh", None);
        let suffix = format_attach_failure_diagnosis(&original, Some(&original), &[])
            .expect("healthy diagnosis");
        let combined = anyhow::anyhow!("read pty output; {suffix}");
        assert!(
            format!("{combined:#}").starts_with("read pty output"),
            "output error must remain primary"
        );
    }

    #[test]
    fn alt_screen_tracker_observes_enter_and_exit() {
        let state = Arc::new(AltScreenState::default());
        let kbd = Arc::new(KeyboardProtocolRestoreState::default());
        let mut tracker = TerminalOutputTracker::new(Arc::clone(&kbd), Arc::clone(&state), None);

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
        let mut tracker = TerminalOutputTracker::new(Arc::clone(&kbd), Arc::clone(&state), None);

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
        let mut tracker = TerminalOutputTracker::new(Arc::clone(&kbd), Arc::clone(&state), None);

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
        let mut tracker = TerminalOutputTracker::new(Arc::clone(&kbd), Arc::clone(&state), None);

        // 기본 cursor visibility는 보임(true).
        assert!(state.cursor_visible.load(Ordering::Relaxed));

        // xterm 그룹 set: ?47;1049h → 1049 매치 → enter (커서 visibility는 미변경)
        tracker.observe(b"\x1b[?47;1049h");
        assert!(state.active.load(Ordering::Relaxed));
        assert!(state.cursor_visible.load(Ordering::Relaxed));

        // 그룹 reset: ?1049;25l → 1049 매치 → exit, 그리고 25 매치 → 커서 숨김.
        // 한 시퀀스에 alt-screen과 DECTCEM이 함께 와도 둘 다 갱신돼야 한다.
        tracker.observe(b"\x1b[?1049;25l");
        assert!(!state.active.load(Ordering::Relaxed));
        assert!(
            !state.cursor_visible.load(Ordering::Relaxed),
            "그룹 param의 25l은 커서 visibility도 false로 갱신해야 한다"
        );

        // 첫번째 파라미터가 무관해도 두번째가 alt-screen이면 매치.
        // ?25;1049h → 1049 매치 → enter, 그리고 25h → 커서 보임 복원.
        tracker.observe(b"\x1b[?25;1049h");
        assert!(state.active.load(Ordering::Relaxed));
        assert!(
            state.cursor_visible.load(Ordering::Relaxed),
            "그룹 param의 25h은 커서 visibility도 true로 갱신해야 한다"
        );
    }

    #[test]
    fn dectcem_param_matches_detects_mode_25_only() {
        // 단일 mode 25 → true
        assert!(dectcem_param_matches(b"25"));
        // 그룹의 끝/중간/시작에 25가 있으면 true (`;` split 기반)
        assert!(dectcem_param_matches(b"1049;25"));
        assert!(dectcem_param_matches(b"25;1049"));
        assert!(dectcem_param_matches(b"47;25;1006"));
        // 25를 포함하지 않는 mode들 → false
        assert!(!dectcem_param_matches(b"47"));
        assert!(!dectcem_param_matches(b"2004"));
        assert!(!dectcem_param_matches(b"1004"));
        // 25를 부분 문자열로 포함하지만 독립 토큰이 아니면 false (`;` split이라 1025/250 미오인)
        assert!(!dectcem_param_matches(b"1025"));
        assert!(!dectcem_param_matches(b"250"));
        // `:`는 subparameter separator라 split하지 않는다 → `25:5`는 mode 25가 아님
        assert!(!dectcem_param_matches(b"25:5"));
    }

    #[test]
    fn cursor_visibility_tracker_observes_hide_and_show() {
        let state = Arc::new(AltScreenState::default());
        let kbd = Arc::new(KeyboardProtocolRestoreState::default());
        let mut tracker = TerminalOutputTracker::new(Arc::clone(&kbd), Arc::clone(&state), None);

        // 기본은 보임(true).
        assert!(state.cursor_visible.load(Ordering::Relaxed));
        // ?25l → 숨김
        tracker.observe(b"prompt\x1b[?25lhidden");
        assert!(!state.cursor_visible.load(Ordering::Relaxed));
        // ?25h → 다시 보임
        tracker.observe(b"\x1b[?25hshown");
        assert!(state.cursor_visible.load(Ordering::Relaxed));
        // DECTCEM 토글은 alt-screen active를 건드리지 않는다.
        assert!(!state.active.load(Ordering::Relaxed));
    }

    #[test]
    fn cursor_visibility_sequence_split_across_chunks_observed() {
        // 청크 경계로 `\x1b[?25l`이 쪼개져도 tail 버퍼 합성 경로가 잡아낸다.
        let state = Arc::new(AltScreenState::default());
        let kbd = Arc::new(KeyboardProtocolRestoreState::default());
        let mut tracker = TerminalOutputTracker::new(Arc::clone(&kbd), Arc::clone(&state), None);

        tracker.observe(b"prefix\x1b[?2");
        // 아직 종결자 없음 → 미변경(기본 true 유지)
        assert!(state.cursor_visible.load(Ordering::Relaxed));
        tracker.observe(b"5l");
        assert!(
            !state.cursor_visible.load(Ordering::Relaxed),
            "청크 경계로 잘린 ?25l도 boundary 경로가 잡아 숨김으로 갱신해야 한다"
        );
    }

    #[test]
    fn alt_screen_sequence_split_across_chunks_observed_once() {
        let state = Arc::new(AltScreenState::default());
        let kbd = Arc::new(KeyboardProtocolRestoreState::default());
        let mut tracker = TerminalOutputTracker::new(Arc::clone(&kbd), Arc::clone(&state), None);

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
        let mut tracker = TerminalOutputTracker::new(Arc::clone(&kbd), Arc::clone(&state), None);

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
        let mut tracker = TerminalOutputTracker::new(Arc::clone(&kbd), Arc::clone(&state), None);

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
        // damage_pending=false 이므로 fast lane은 비활성. attach 루프 시작 직후
        // ZERO elapsed는 모든 경로가 false여야 한다.
        assert!(!heartbeat_due(Duration::ZERO, false, false));
        assert!(!heartbeat_due(Duration::ZERO, true, false));
        assert!(!heartbeat_due(
            STATUS_HEARTBEAT - Duration::from_millis(1),
            false,
            false
        ));
        assert!(heartbeat_due(STATUS_HEARTBEAT, false, false));
        assert!(heartbeat_due(
            STATUS_HEARTBEAT + Duration::from_millis(50),
            false,
            false
        ));
    }

    #[test]
    fn heartbeat_dirty_blocks_idle_path_until_forced_threshold() {
        // status_dirty=true 이고 damage_pending=false 면 STATUS_HEARTBEAT 경과만으로는
        // 발화하지 않고, STATUS_HEARTBEAT_FORCED 경과 후에 강제 발화한다. 이 경로가 없으면
        // PTY 연속 출력 중 외부 DECSTBM 리셋을 영원히 회복하지 못한다. fast lane 도입 후에도
        // 비손상 dirty(damage_pending=false)의 백스톱 타이밍은 불변이어야 한다 — spec (d).
        assert!(!heartbeat_due(STATUS_HEARTBEAT, true, false));
        assert!(!heartbeat_due(
            STATUS_HEARTBEAT_FORCED - Duration::from_millis(1),
            true,
            false
        ));
        assert!(heartbeat_due(STATUS_HEARTBEAT_FORCED, true, false));
        assert!(heartbeat_due(
            STATUS_HEARTBEAT_FORCED + Duration::from_millis(100),
            true,
            false
        ));
    }

    #[test]
    fn heartbeat_damage_fast_lane_fires_at_short_interval_and_rate_limits() {
        // 확정 손상 fast lane: damage_pending=true 이고 STATUS_DAMAGE_HEARTBEAT 경과 시
        // forced 2초를 기다리지 않고 발화한다 — spec (a).
        assert!(heartbeat_due(STATUS_DAMAGE_HEARTBEAT, true, true));
        assert!(heartbeat_due(
            STATUS_DAMAGE_HEARTBEAT + Duration::from_millis(10),
            true,
            true
        ));
        // status_dirty=false 여도(예외적) 손상이 확정되면 fast lane이 발화한다.
        assert!(heartbeat_due(STATUS_DAMAGE_HEARTBEAT, false, true));
        // rate-limit: STATUS_DAMAGE_HEARTBEAT 미경과면 손상 중이라도 발화하지 않아
        // 연속 출력 중 매 프레임 repaint 폭주를 막는다 — spec (b).
        assert!(!heartbeat_due(Duration::ZERO, true, true));
        assert!(!heartbeat_due(
            STATUS_DAMAGE_HEARTBEAT - Duration::from_millis(1),
            true,
            true
        ));
    }

    #[test]
    fn heartbeat_damage_fast_lane_is_faster_than_forced_backstop() {
        // fast lane이 forced 백스톱보다 훨씬 빨라야 손상 복구가 체감상 즉시여야 한다.
        assert!(STATUS_DAMAGE_HEARTBEAT < STATUS_HEARTBEAT_FORCED);
        assert!(STATUS_DAMAGE_HEARTBEAT < STATUS_HEARTBEAT);
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
    fn instrument_protocol_guard_rejects_old_daemon() {
        let old = DaemonStatus {
            version: "0.1.2".to_string(),
            protocol_version: 3,
            session_count: 0,
            active_connections: 0,
            shutting_down: false,
            daemon_uid: None,
            started_at_unix_secs: None,
        };
        let current = DaemonStatus {
            protocol_version: super::INSTRUMENT_PROTOCOL_VERSION,
            ..old.clone()
        };

        let message = instrument_protocol_error(&old).expect("old daemon should be rejected");
        assert!(message.contains("does not support instrument snapshots"));
        assert!(message.contains("lterm shutdown"));
        assert_eq!(instrument_protocol_error(&current), None);
    }

    #[test]
    fn explicit_tmux_parent_protocol_guard_rejects_protocol_six() {
        let old = DaemonStatus {
            version: "1.0.31".to_string(),
            protocol_version: 6,
            session_count: 0,
            active_connections: 0,
            shutting_down: false,
            daemon_uid: None,
            started_at_unix_secs: None,
        };
        let current = DaemonStatus {
            protocol_version: super::TMUX_PARENT_PANE_PROTOCOL_VERSION,
            ..old.clone()
        };

        let message = tmux_parent_pane_protocol_error(&old).expect("protocol 6 should be rejected");
        assert!(message.contains("explicit tmux parent panes"));
        assert!(message.contains("requires protocol 7"));
        assert!(message.contains("lterm shutdown"));
        assert_eq!(tmux_parent_pane_protocol_error(&current), None);
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
            "NO_COLOR",
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
            std::env::remove_var("NO_COLOR");

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
    fn no_color_prefers_minimal_status_style_unless_lterm_style_overrides() {
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
            "NO_COLOR",
        ]);

        // SAFETY: crate::TEST_ENV_LOCK is held; EnvGuard restores on drop.
        unsafe {
            std::env::remove_var("LTERM_STATUS_STYLE");
            std::env::set_var("LTERM_STATUS_THEME", "green");
            std::env::remove_var("SSH_CONNECTION");
            std::env::remove_var("SSH_CLIENT");
            std::env::remove_var("SSH_TTY");
            std::env::remove_var("TERM_PROGRAM");
            std::env::remove_var("LC_TERMINAL");
            std::env::remove_var("TERMINAL_EMULATOR");
            std::env::set_var("NO_COLOR", "1");
        }
        assert_eq!(
            resolve_status_style(None),
            StatusStyle::Minimal,
            "NO_COLOR should prevent colored status themes from leaking terminal-wide colors"
        );

        // SAFETY: crate::TEST_ENV_LOCK is held; EnvGuard restores on drop.
        unsafe {
            std::env::set_var("LTERM_STATUS_STYLE", "full");
        }
        assert_eq!(
            resolve_status_style(None),
            StatusStyle::Full(StatusTheme::Green),
            "lterm-specific full style override remains available when color is explicitly wanted"
        );
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
            "NO_COLOR",
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
                std::env::remove_var("NO_COLOR");
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
            "NO_COLOR",
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
            std::env::remove_var("NO_COLOR");
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
