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
/// 재그린다. PTY가 활발히 출력 중일 때 idle heartbeat는 status_dirty가 클리어되어야 발화하므로,
/// busy 출력 시에는 [`STATUS_HEARTBEAT_FORCED`] 가 dirty 여부와 무관하게 강제 redraw한다.
const STATUS_HEARTBEAT: Duration = Duration::from_millis(250);
/// busy PTY 출력으로 [`STATUS_HEARTBEAT`] idle 경로가 차단된 경우(WouldBlock이 fire하지
/// 않아 status_dirty가 클리어되지 않음) self-heal이 영원히 막히지 않게 강제 발화하는 상한.
/// 사용자 보고: cmux pane swap 후 status가 회복 안 되는 증상 회복용. 500ms = 2× heartbeat.
const STATUS_HEARTBEAT_FORCED: Duration = Duration::from_millis(500);
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
    let mut stream = UnixStream::connect(&path)
        .with_context(|| format!("connect to lterm daemon at {}", path.display()))?;
    // PR #15: attach 시점의 클라이언트 geometry 를 함께 보낸다. server 는 이 값을
    // 바로 Subscriber 에 박아 clamp-to-smallest 정책의 인풋으로 쓴다.
    let request = Request::Attach {
        target: target.to_string(),
        rows: pty_rows,
        cols,
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
        // SGR + cursor save/restore + 본문을 단일 String 으로 buffer 후 write_all 1회 호출.
        // 이는 strict atomicity 보장은 아니다 (write_all은 내부적으로 여러 syscall 가능).
        // TTY/PTY는 POSIX PIPE_BUF atomicity 적용 대상이 아니므로 partial-write 가능성 잔존.
        // 그러나 write! 매크로는 placeholder 마다 write_fmt 가 분할 syscall을 일으켜 SGR sequence
        // 중간이 다른 출력과 interleave 될 위험이 컸다 — buffered write 로 그 위험을 줄인다.
        let payload = format!("\x1b7\x1b[{rows};1H\x1b[2K{sgr}{line}\x1b[0m\x1b8");
        stdout
            .write_all(payload.as_bytes())
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
    b"\x1b[?1049l\x1b[?47l\x1b[?1047l\x1b[r\x1b[?25h\
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
        let prev = ATTACH_ACTIVE.fetch_sub(1, std::sync::atomic::Ordering::Release);
        // refcount underflow는 unique-owner 계약 위반(생성 경로 외에서 Drop이 발생).
        // wrapping으로 usize::MAX가 되면 panic hook이 영구히 cleanup을 emit해 디버깅이
        // 어려워지므로 dev/test 단계에서 즉시 잡는다. release 빌드는 no-op.
        debug_assert!(
            prev > 0,
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
        ATTACH_ACTIVE, AltScreenState, AttachActiveGuard, KeyboardProtocolRestoreState,
        ResizeTickOutcome, STATUS_HEARTBEAT, STATUS_HEARTBEAT_FORCED, StatusStyle,
        TerminalOutputTracker, alt_screen_param_matches, attach_pty_rows,
        cursor_clamp_into_scroll_region, ensure_panic_terminal_cleanup_hook, format_status_line,
        handle_resize_tick, heartbeat_due, keyboard_protocol_restore_bytes, matches_env_bool,
        observe_keyboard_protocol_sequences, panic_terminal_cleanup_bytes, parse_status_style,
        resolve_status_style,
    };
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    /// 환경 변수를 변경하는 모든 테스트가 공유하는 직렬화 잠금. process-global env에 대한
    /// race를 막기 위해 env-touching 테스트는 반드시 이 lock을 잡고, 종료 시 EnvGuard로
    /// 원본 값을 복원해 다른 테스트로 누설되지 않게 한다.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// ATTACH_ACTIVE 플래그를 만지는 테스트가 공유하는 직렬화 잠금.
    /// process-global static AtomicBool 이므로 병렬 테스트 race를 막아야 한다.
    static ATTACH_FLAG_LOCK: Mutex<()> = Mutex::new(());

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
        // 커서 visible → kitty pop ×16 → kitty direct disable → SGR 리셋 → CR+LF.
        // alt-screen 종료가 \x1b[r 보다 먼저 와서 reset이 메인 버퍼에 적용되어야 한다.
        // pop 16회 = MAX_KEYBOARD_PROTOCOL_RESTORE_POPS 와 정렬 (스택 바닥 이상은 no-op).
        let expected = b"\x1b[?1049l\x1b[?47l\x1b[?1047l\x1b[r\x1b[?25h\
                         \x1b[<u\x1b[<u\x1b[<u\x1b[<u\x1b[<u\x1b[<u\x1b[<u\x1b[<u\
                         \x1b[<u\x1b[<u\x1b[<u\x1b[<u\x1b[<u\x1b[<u\x1b[<u\x1b[<u\
                         \x1b[=0u\x1b[0m\r\n";
        assert_eq!(bytes, expected);
        // pop 시퀀스가 정확히 16번 등장하는지 검증 (회귀 시 즉시 catch)
        let pop_count = bytes.windows(4).filter(|w| *w == b"\x1b[<u").count();
        assert_eq!(
            pop_count, 16,
            "kitty pop은 MAX_KEYBOARD_PROTOCOL_RESTORE_POPS=16과 일치"
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
        let _guard = ATTACH_FLAG_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());

        // 사전 capture (다른 코드 경로에서 set 했을 수 있음)
        let prior = ATTACH_ACTIVE.load(Ordering::Acquire);
        ATTACH_ACTIVE.store(0, Ordering::Release);

        assert_eq!(ATTACH_ACTIVE.load(Ordering::Acquire), 0);
        {
            let _g = AttachActiveGuard::enter();
            assert_eq!(ATTACH_ACTIVE.load(Ordering::Acquire), 1);
        }
        assert_eq!(ATTACH_ACTIVE.load(Ordering::Acquire), 0);

        // 원래 상태 복원
        ATTACH_ACTIVE.store(prior, Ordering::Release);
    }

    #[test]
    fn attach_active_guard_supports_nested_attach() {
        // nested attach (예: cmux 안에서 lterm omx로 attach 후 그 안에서 다시 lterm attach)
        // 시 inner Drop이 outer의 활성 상태를 무효화하지 않아야 한다. AtomicBool이었을 때의
        // 회귀를 회귀 테스트로 잡는다 (quad-review MEDIUM 합의).
        let _guard = ATTACH_FLAG_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let prior = ATTACH_ACTIVE.load(Ordering::Acquire);
        ATTACH_ACTIVE.store(0, Ordering::Release);

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

        ATTACH_ACTIVE.store(prior, Ordering::Release);
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
