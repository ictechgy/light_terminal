use crate::paths;
use crate::protocol::{Request, Response, SessionInfo};
use crate::sanitize;
use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::json;
use std::fs::OpenOptions;
use std::io::{ErrorKind, IsTerminal, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::PermissionsExt;
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
const MAX_DAEMON_LOG_BYTES: u64 = 10 * 1024 * 1024;
const RPC_TIMEOUT: Duration = Duration::from_secs(5);
/// Status bar self-heal 주기. cmux/Termius 등에서 다른 앱→복귀 시 외부에서 DECSTBM이
/// 리셋되어도 사용자 인지 한계(약 100~300ms) 안에 scroll region을 재확립하고 status를
/// 재그린다. PTY가 활발히 출력 중이면 status_dirty 경로가 즉시 처리하므로 heartbeat는
/// idle 상태에만 발화한다.
const STATUS_HEARTBEAT: Duration = Duration::from_millis(250);
const PS_CANDIDATES: &[&str] = &["/bin/ps", "/usr/bin/ps"];

pub fn ensure_server() -> Result<()> {
    if rpc::<serde_json::Value>(&Request::Ping).is_ok() {
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
            Ok(_) => return Ok(()),
            Err(err) => {
                last_err = Some(err);
                thread::sleep(Duration::from_millis(80));
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("daemon did not become ready")))
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

pub fn rpc<T: DeserializeOwned>(request: &Request) -> Result<T> {
    let path = paths::socket_path()?;
    let mut stream = UnixStream::connect(&path)
        .with_context(|| format!("connect to lterm daemon at {}", path.display()))?;
    stream
        .set_read_timeout(Some(RPC_TIMEOUT))
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
    env: std::collections::HashMap<String, String>,
    tmux: bool,
) -> Result<SessionInfo> {
    ensure_server()?;
    let cwd = Some(resolve_client_cwd(cwd)?);
    let parent = current_parent_request();
    rpc(&Request::New {
        name,
        command,
        cwd,
        rows: terminal_rows(),
        cols: terminal_cols(),
        parent_pane_id: parent.as_ref().map(|parent| parent.pane_id.clone()),
        parent_token: parent.map(|parent| parent.token),
        env,
        tmux,
    })
}

pub fn attach_or_new(target: &str) -> Result<SessionInfo> {
    ensure_server()?;
    let parent = current_parent_request();
    rpc(&Request::AttachOrNew {
        target: target.to_string(),
        cwd: Some(resolve_client_cwd(None)?),
        parent_pane_id: parent.as_ref().map(|parent| parent.pane_id.clone()),
        parent_token: parent.map(|parent| parent.token),
    })
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

pub fn kill(target: &str) -> Result<()> {
    ensure_server()?;
    rpc::<serde_json::Value>(&Request::Kill {
        target: target.to_string(),
    })?;
    Ok(())
}

pub fn send(target: &str, data: Vec<u8>) -> Result<()> {
    ensure_server()?;
    rpc::<serde_json::Value>(&Request::Send {
        target: target.to_string(),
        data,
    })?;
    Ok(())
}

pub fn capture(target: &str, start: Option<i32>) -> Result<String> {
    ensure_server()?;
    rpc(&Request::Capture {
        target: target.to_string(),
        start,
    })
}

pub fn resize(target: &str, rows: u16, cols: u16) -> Result<()> {
    ensure_server()?;
    rpc::<serde_json::Value>(&Request::Resize {
        target: target.to_string(),
        rows,
        cols,
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
    pub stat: String,
    pub cpu_percent: f32,
    pub mem_percent: f32,
    pub rss_kib: u64,
    pub elapsed: String,
    pub command: String,
}

pub fn process_tree(target: Option<&str>) -> Result<Vec<ProcessInfo>> {
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
        let Some(root) = session.process_id else {
            continue;
        };
        builder.append(&session.name, &session.pane_id, root, 0);
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

    fn into_processes(self) -> Vec<ProcessInfo> {
        self.processes
    }
}

#[derive(Debug, Clone)]
struct ProcessRow {
    pid: u32,
    ppid: u32,
    stat: String,
    cpu_percent: f32,
    mem_percent: f32,
    rss_kib: u64,
    elapsed: String,
    command: String,
}

fn read_process_table() -> Result<Vec<ProcessRow>> {
    let output = Command::new(ps_path()?)
        .args(["-axo", "pid=,ppid=,stat=,%cpu=,%mem=,rss=,etime=,command="])
        .output()
        .context("run ps")?;
    if !output.status.success() {
        bail!("ps exited with {}", output.status);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut rows = Vec::new();
    for line in text.lines() {
        let fields: Vec<_> = line.split_whitespace().take(7).collect();
        if fields.len() < 7 {
            continue;
        }
        let Some(command_start) = nth_field_start(line, 7) else {
            continue;
        };
        let Some(pid) = parse_nonzero_u32(fields[0]) else {
            continue;
        };
        let Some(ppid) = parse_u32(fields[1]) else {
            continue;
        };
        let Some(cpu_percent) = parse_f32(fields[3]) else {
            continue;
        };
        let Some(mem_percent) = parse_f32(fields[4]) else {
            continue;
        };
        let Some(rss_kib) = parse_u64(fields[5]) else {
            continue;
        };
        rows.push(ProcessRow {
            pid,
            ppid,
            stat: fields[2].to_string(),
            cpu_percent,
            mem_percent,
            rss_kib,
            elapsed: fields[6].to_string(),
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

pub fn attach(target: &str, show_status: bool, stdin_eof: AttachStdinEof) -> Result<()> {
    ensure_server()?;
    let status_enabled = status_bar_supported(show_status);
    let status_info = if status_enabled {
        Some(info(target)?)
    } else {
        None
    };
    let (cols, rows) = terminal_size();
    let _ = resize(target, attach_pty_rows(rows, status_enabled), cols);

    let path = paths::socket_path()?;
    let mut stream = UnixStream::connect(&path)
        .with_context(|| format!("connect to lterm daemon at {}", path.display()))?;
    let request = Request::Attach {
        target: target.to_string(),
    };
    stream.write_all(&serde_json::to_vec(&request)?)?;
    stream.write_all(b"\n")?;

    let mut header = Vec::new();
    let mut one = [0_u8; 1];
    loop {
        let n = stream.read(&mut one).context("read attach header")?;
        if n == 0 {
            bail!("daemon closed attach before header");
        }
        header.push(one[0]);
        if one[0] == b'\n' {
            break;
        }
        if header.len() > 64 * 1024 {
            bail!("attach header too large");
        }
    }
    let response: Response = serde_json::from_slice(&header).context("parse attach header")?;
    if !response.ok {
        bail!(
            response
                .error
                .unwrap_or_else(|| "attach failed".to_string())
        );
    }

    let _raw = RawModeGuard::enter()?;
    let alt_screen_state = Arc::new(AltScreenState::default());
    let mut terminal_output_tracker = TerminalOutputTracker::new(
        _raw.keyboard_protocol_restore_state(),
        Arc::clone(&alt_screen_state),
    );
    let running = Arc::new(AtomicBool::new(true));

    let mut writer = stream.try_clone().context("clone attach stream writer")?;
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
                    Ok(n) => writer.write_all(&buf[..n]).context("write pty input")?,
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
            if current != last {
                let _ = resize(
                    &resize_target,
                    attach_pty_rows(current.1, status_enabled),
                    current.0,
                );
                let _ = resize_tx.try_send(());
                last = current;
            }
        }
    });

    let mut stdout = std::io::stdout();
    let status_style = status_enabled.then(resolve_status_style);
    let mut status_bar = StatusBar::enter(status_info.as_ref(), status_style, &mut stdout)?;
    if status_enabled {
        stream
            .set_read_timeout(Some(Duration::from_millis(30)))
            .context("set attach output read timeout")?;
    }
    let mut buf = [0_u8; 8192];
    let mut status_dirty = false;
    let mut last_status_refresh = Instant::now();
    let mut prev_alt_screen_active = false;
    let output_result = (|| -> Result<()> {
        loop {
            let alt_screen_active = alt_screen_state.active.load(Ordering::Relaxed);

            // alt-screen 종료 즉시 refresh: alt buffer로 흘러갔던 status는 폐기되었으므로
            // 다음 heartbeat까지 빈 상태가 되지 않게 한 번 redraw한다. 이 redraw가 PTY의
            // main-buffer redraw와 시점이 겹치면 미세한 깜빡임이 가능하나, scroll region
            // (rows-1)이 PTY 본문을 status row와 분리하므로 실용적 문제는 없다.
            if status_enabled && prev_alt_screen_active && !alt_screen_active {
                status_bar.refresh(&mut stdout)?;
                stdout.flush().context("flush stdout")?;
                status_dirty = false;
                last_status_refresh = Instant::now();
            }
            prev_alt_screen_active = alt_screen_active;

            while resize_rx.try_recv().is_ok() {
                // alt-screen 동안 refresh하면 alt buffer로 출력되어 vim 등과 충돌한다.
                // 리사이즈 자체는 daemon-side resize 호출이 이미 처리했으므로, alt-screen
                // 종료 후 edge refresh가 새 크기로 다시 그린다.
                if !alt_screen_active {
                    status_bar.refresh(&mut stdout)?;
                    stdout.flush().context("flush stdout")?;
                    status_dirty = false;
                    last_status_refresh = Instant::now();
                }
            }
            // status_dirty == true 상황은 WouldBlock 경로가 곧바로 처리한다.
            // heartbeat는 idle 상태(외부 앱 백그라운드 등)에서만 self-heal 한다.
            if status_enabled
                && !status_dirty
                && !alt_screen_active
                && last_status_refresh.elapsed() >= STATUS_HEARTBEAT
            {
                status_bar.refresh(&mut stdout)?;
                stdout.flush().context("flush stdout")?;
                last_status_refresh = Instant::now();
            }
            let n = match stream.read(&mut buf) {
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
                        status_bar.refresh(&mut stdout)?;
                        stdout.flush().context("flush stdout")?;
                        status_dirty = false;
                        last_status_refresh = Instant::now();
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
            status_bar.refresh(&mut stdout)?;
            stdout.flush().context("flush stdout")?;
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
/// Full(검정 글자 + bright-blue 배경)로 강조하고, Termius 같은 모바일 SSH에서는
/// Minimal(plain text)로 폴백해 색 매핑 충돌과 시각 노이즈를 줄인다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StatusStyle {
    Full,
    Minimal,
}

fn resolve_status_style() -> StatusStyle {
    if let Ok(value) = std::env::var("LTERM_STATUS_STYLE") {
        if let Some(style) = parse_status_style(&value) {
            return style;
        }
    }
    if is_ssh_session() {
        return StatusStyle::Minimal;
    }
    StatusStyle::Full
}

fn parse_status_style(value: &str) -> Option<StatusStyle> {
    match value.trim().to_ascii_lowercase().as_str() {
        "full" => Some(StatusStyle::Full),
        "minimal" => Some(StatusStyle::Minimal),
        _ => None,
    }
}

fn is_ssh_session() -> bool {
    ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"]
        .iter()
        .any(|name| std::env::var(name).is_ok_and(|v| !v.is_empty()))
}

struct StatusBar {
    session_name: String,
    pane_id: String,
    /// None 이면 status bar를 그리지 않는다 (--no-status / LTERM_NO_STATUS=1 등).
    style: Option<StatusStyle>,
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
        };
        status.reserve_terminal_area(stdout)?;
        status.draw(stdout)?;
        stdout.flush().context("flush stdout")?;
        Ok(status)
    }

    fn refresh(&mut self, stdout: &mut impl Write) -> Result<()> {
        self.reserve_terminal_area(stdout)?;
        self.draw(stdout)
    }

    fn reserve_terminal_area(&self, stdout: &mut impl Write) -> Result<()> {
        if self.style.is_none() {
            return Ok(());
        }
        let (_, rows) = terminal_size();
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
        // status line으로 새는 것을 차단한다. Full은 reset 뒤 검정 글자 + bright-blue
        // 배경을 단일 CSI(\x1b[0;30;104m)로 적용해 바이트를 줄인다.
        // (bold(1)은 두 모드 모두에서 사용하지 않는다: bold+black을 흰색으로 렌더하는 터미널이 있다.)
        let sgr = match self.style {
            Some(StatusStyle::Full) => "\x1b[0;30;104m",
            Some(StatusStyle::Minimal) => "\x1b[0m",
            None => return Ok(()),
        };
        write!(stdout, "\x1b7\x1b[{rows};1H\x1b[2K{sgr}{line}\x1b[0m\x1b8")
            .context("draw lterm status bar")?;
        Ok(())
    }

    fn restore(&self, stdout: &mut impl Write) -> Result<()> {
        if self.style.is_none() {
            return Ok(());
        }
        let (_, rows) = terminal_size();
        write!(stdout, "\x1b7\x1b[r\x1b[{rows};1H\x1b[0m\x1b[2K\x1b8")
            .context("restore terminal after lterm status bar")?;
        stdout.flush().ok();
        Ok(())
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
    let width = cols as usize;
    let mut line = format!(" lterm  {session_name}  {pane_id} ");
    if line.chars().count() > width {
        line = line.chars().take(width).collect();
    }
    let len = line.chars().count();
    if len < width {
        line.push_str(&" ".repeat(width - len));
    }
    line
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
        let scan_end = bytes.len().min(i + 32);
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
    // `;` (그리고 colon 변형 `:`)로 split 후 한 파라미터라도 alt-screen 모드면 매치한다.
    params
        .split(|byte| *byte == b';' || *byte == b':')
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
        AltScreenState, KeyboardProtocolRestoreState, StatusStyle, TerminalOutputTracker,
        alt_screen_param_matches, attach_pty_rows, format_status_line,
        keyboard_protocol_restore_bytes, matches_env_bool, observe_keyboard_protocol_sequences,
        parse_status_style, resolve_status_style,
    };
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::Ordering;

    /// 환경 변수를 변경하는 모든 테스트가 공유하는 직렬화 잠금. process-global env에 대한
    /// race를 막기 위해 env-touching 테스트는 반드시 이 lock을 잡고, 종료 시 EnvGuard로
    /// 원본 값을 복원해 다른 테스트로 누설되지 않게 한다.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// 지정된 환경 변수의 현재 값을 저장하고, Drop 시 원래 값(또는 unset 상태)으로 복원한다.
    /// ENV_LOCK을 잡은 상태에서만 사용해야 한다.
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
            // SAFETY: 호출자는 ENV_LOCK을 잡고 있어야 한다 (테스트 컨벤션).
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

    #[test]
    fn status_bar_reserves_one_terminal_row_when_possible() {
        assert_eq!(attach_pty_rows(24, true), 23);
        assert_eq!(attach_pty_rows(1, true), 1);
        assert_eq!(attach_pty_rows(24, false), 24);
    }

    #[test]
    fn status_line_is_exact_terminal_width() {
        assert_eq!(format_status_line("recovery", "%0", 12), " lterm  reco");
        assert_eq!(format_status_line("api", "%1", 16), " lterm  api  %1 ");
        assert_eq!(format_status_line("api", "%1", 18), " lterm  api  %1   ");
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
    fn ignores_non_keyboard_csi_sequences() {
        let state = KeyboardProtocolRestoreState::default();
        observe_keyboard_protocol_sequences(b"\x1b[?25l\x1b[>4;1m\x1b[31m", &state);
        assert_eq!(state.kitty_push_depth.load(Ordering::Relaxed), 0);
        assert_eq!(state.kitty_direct_flags.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn status_style_env_takes_precedence_over_ssh() {
        // 다른 env-touching 테스트와 충돌하지 않도록 모듈 공유 ENV_LOCK으로 직렬화한 뒤,
        // EnvGuard로 테스트가 끝나면 원래 환경 변수를 복원한다.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let _env_guard = EnvGuard::capture(&[
            "LTERM_STATUS_STYLE",
            "SSH_CONNECTION",
            "SSH_CLIENT",
            "SSH_TTY",
        ]);

        // SAFETY: ENV_LOCK is held; EnvGuard restores on drop.
        unsafe {
            std::env::remove_var("LTERM_STATUS_STYLE");
            std::env::remove_var("SSH_CONNECTION");
            std::env::remove_var("SSH_CLIENT");
            std::env::remove_var("SSH_TTY");

            std::env::set_var("SSH_CONNECTION", "1.2.3.4 22 5.6.7.8 22");
            std::env::set_var("LTERM_STATUS_STYLE", "full");
        }
        assert_eq!(resolve_status_style(), StatusStyle::Full);

        unsafe {
            std::env::set_var("LTERM_STATUS_STYLE", "minimal");
        }
        assert_eq!(resolve_status_style(), StatusStyle::Minimal);

        unsafe {
            std::env::remove_var("LTERM_STATUS_STYLE");
        }
        // SSH only → Minimal
        assert_eq!(resolve_status_style(), StatusStyle::Minimal);

        unsafe {
            std::env::remove_var("SSH_CONNECTION");
        }
        // No SSH, no style → Full
        assert_eq!(resolve_status_style(), StatusStyle::Full);

        // EnvGuard 가 drop 되면서 원래 환경 변수 값을 복원한다.
    }

    #[test]
    fn parse_status_style_accepts_known_values() {
        assert_eq!(parse_status_style("full"), Some(StatusStyle::Full));
        assert_eq!(parse_status_style("Minimal"), Some(StatusStyle::Minimal));
        assert_eq!(parse_status_style(" full "), Some(StatusStyle::Full));
        assert_eq!(parse_status_style("off"), None);
        assert_eq!(parse_status_style(""), None);
    }
}
