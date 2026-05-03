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
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const MAX_RPC_RESPONSE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_DAEMON_LOG_BYTES: u64 = 10 * 1024 * 1024;
const RPC_TIMEOUT: Duration = Duration::from_secs(5);
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
    let mut terminal_output_tracker =
        TerminalOutputTracker::new(_raw.keyboard_protocol_restore_state());
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
    let mut status_bar = StatusBar::enter(status_info.as_ref(), status_enabled, &mut stdout)?;
    if status_enabled {
        stream
            .set_read_timeout(Some(Duration::from_millis(30)))
            .context("set attach output read timeout")?;
    }
    let mut buf = [0_u8; 8192];
    let mut status_dirty = false;
    let output_result = (|| -> Result<()> {
        loop {
            while resize_rx.try_recv().is_ok() {
                status_bar.refresh(&mut stdout)?;
                stdout.flush().context("flush stdout")?;
                status_dirty = false;
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
                    if status_dirty {
                        status_bar.refresh(&mut stdout)?;
                        stdout.flush().context("flush stdout")?;
                        status_dirty = false;
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
        if status_dirty {
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

struct StatusBar {
    active: bool,
    session_name: String,
    pane_id: String,
}

impl StatusBar {
    fn enter(
        info: Option<&SessionInfo>,
        show_status: bool,
        stdout: &mut impl Write,
    ) -> Result<Self> {
        let active = show_status;
        let (session_name, pane_id) = info
            .map(|info| {
                (
                    sanitize::terminal_text(&info.name),
                    sanitize::terminal_text(&info.pane_id),
                )
            })
            .unwrap_or_else(|| ("unknown".to_string(), "?".to_string()));
        let mut status = Self {
            active,
            session_name,
            pane_id,
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
        if !self.active {
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
        if !self.active {
            return Ok(());
        }
        let (cols, rows) = terminal_size();
        if rows <= 1 || cols == 0 {
            return Ok(());
        }
        let line = format_status_line(&self.session_name, &self.pane_id, cols);
        write!(
            stdout,
            "\x1b7\x1b[{rows};1H\x1b[1;30;104m{line}\x1b[0m\x1b8"
        )
        .context("draw lterm status bar")?;
        Ok(())
    }

    fn restore(&self, stdout: &mut impl Write) -> Result<()> {
        if !self.active {
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
        let mut stdout = std::io::stdout();
        let _ = self.restore(&mut stdout);
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
    kitty_direct: AtomicBool,
    kitty_direct_inside_push: AtomicBool,
}

struct TerminalOutputTracker {
    restore_state: Arc<KeyboardProtocolRestoreState>,
    tail: Vec<u8>,
}

impl TerminalOutputTracker {
    fn new(restore_state: Arc<KeyboardProtocolRestoreState>) -> Self {
        Self {
            restore_state,
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
        }

        observe_keyboard_protocol_sequences(bytes, &self.restore_state);

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
                        b'<' => observe_kitty_pop(state),
                        b'=' => {
                            if let Some(enabled) = keyboard_protocol_direct_enabled(params) {
                                observe_kitty_direct(state, enabled);
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

fn observe_kitty_pop(state: &KeyboardProtocolRestoreState) {
    let _ = state
        .kitty_push_depth
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |depth| {
            Some(depth.saturating_sub(1))
        });
    if state.kitty_push_depth.load(Ordering::Relaxed) == 0 {
        state
            .kitty_direct_inside_push
            .store(false, Ordering::Relaxed);
    }
}

fn observe_kitty_direct(state: &KeyboardProtocolRestoreState, enabled: bool) {
    if !enabled {
        state.kitty_direct.store(false, Ordering::Relaxed);
        state
            .kitty_direct_inside_push
            .store(false, Ordering::Relaxed);
        return;
    }
    if state.kitty_push_depth.load(Ordering::Relaxed) > 0 {
        state
            .kitty_direct_inside_push
            .store(true, Ordering::Relaxed);
    } else {
        state.kitty_direct.store(true, Ordering::Relaxed);
    }
}

fn keyboard_protocol_direct_enabled(params: &[u8]) -> Option<bool> {
    let first = params.split(|byte| *byte == b';').next().unwrap_or(params);
    if first.is_empty() || !first.iter().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let value = std::str::from_utf8(first).ok()?.parse::<u16>().ok()?;
    Some(value > 0)
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
    let direct = state.kitty_direct.load(Ordering::Relaxed);
    let mut restore = Vec::new();
    for _ in 0..push_depth {
        restore.extend_from_slice(b"\x1b[<u");
    }
    if push_depth == 0 && direct {
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
        KeyboardProtocolRestoreState, TerminalOutputTracker, attach_pty_rows, format_status_line,
        keyboard_protocol_restore_bytes, matches_env_bool, observe_keyboard_protocol_sequences,
    };
    use std::sync::atomic::Ordering;

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
        assert!(!state.kitty_direct.load(Ordering::Relaxed));
        assert_eq!(keyboard_protocol_restore_bytes(&state), b"\x1b[<u");

        let state = KeyboardProtocolRestoreState::default();
        observe_keyboard_protocol_sequences(b"\x1b[=3;1u", &state);
        assert_eq!(state.kitty_push_depth.load(Ordering::Relaxed), 0);
        assert!(state.kitty_direct.load(Ordering::Relaxed));
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
        assert!(!state.kitty_direct.load(Ordering::Relaxed));
        assert!(keyboard_protocol_restore_bytes(&state).is_empty());
    }

    #[test]
    fn restores_push_without_clobbering_direct_mode() {
        let state = KeyboardProtocolRestoreState::default();
        observe_keyboard_protocol_sequences(b"\x1b[>1u\x1b[=3u", &state);
        assert_eq!(keyboard_protocol_restore_bytes(&state), b"\x1b[<u");
    }

    #[test]
    fn observes_keyboard_protocol_sequences_split_across_chunks_once() {
        let state = std::sync::Arc::new(KeyboardProtocolRestoreState::default());
        let mut tracker = TerminalOutputTracker::new(std::sync::Arc::clone(&state));
        tracker.observe(b"\x1b[>");
        tracker.observe(b"1u");
        tracker.observe(b"plain output");
        assert_eq!(state.kitty_push_depth.load(Ordering::Relaxed), 1);
        assert_eq!(keyboard_protocol_restore_bytes(&state), b"\x1b[<u");
    }

    #[test]
    fn observes_whole_keyboard_protocol_sequences_once_after_tail() {
        let state = std::sync::Arc::new(KeyboardProtocolRestoreState::default());
        let mut tracker = TerminalOutputTracker::new(std::sync::Arc::clone(&state));
        tracker.observe(b"plain output");
        tracker.observe(b"\x1b[>1u");
        assert_eq!(state.kitty_push_depth.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn ignores_non_keyboard_csi_sequences() {
        let state = KeyboardProtocolRestoreState::default();
        observe_keyboard_protocol_sequences(b"\x1b[?25l\x1b[>4;1m\x1b[31m", &state);
        assert_eq!(state.kitty_push_depth.load(Ordering::Relaxed), 0);
        assert!(!state.kitty_direct.load(Ordering::Relaxed));
    }
}
