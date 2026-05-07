use crate::paths;
use crate::protocol::{Request, Response, SessionInfo};
use crate::sanitize;
use anyhow::{Context, Result, bail};
use libc::c_int;
use portable_pty::{Child, ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const RING_LIMIT: usize = 2 * 1024 * 1024;
const MAX_REQUEST_BYTES: usize = 1024 * 1024;
const MAX_CONNECTIONS: usize = 64;
const MAX_SESSIONS: usize = 256;
const MAX_SUBSCRIBERS_PER_SESSION: usize = 32;
const SUBSCRIBER_QUEUE_LIMIT: usize = 128;
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(5);

type OutputChunk = Arc<[u8]>;

/// attach 클라이언트 한 명의 lifecycle 단위.
///
/// `tx`는 PTY 출력 broadcast 채널, `on_evict`는 채널이 backpressure로
/// `Full`/`Disconnected` 가 되어 서브스크라이버를 강제 제거할 때 호출되는
/// 콜백이다. `handle_attach`은 `on_evict`에 attach `UnixStream` 의
/// `shutdown(Both)` 을 등록해, eviction 발생 시 그 클라이언트의 input loop도
/// EOF로 깨워 종료시킨다. 이 콜백이 없으면 output 만 끊긴 채 input이 살아있는
/// "zombie attach"가 만들어져 사용자가 frozen으로 인지하면서도 keystroke 가
/// PTY로 흘러들어가 위험한 명령이 실행될 수 있다 (quad-review RC-2b).
///
/// 필드명을 `shutdown`이 아닌 `on_evict`로 둔 것은 Gemini quad-review LOW
/// 피드백 반영: 구조체 필드의 동사형 이름(`shutdown`)은 boolean 상태로 오인되기
/// 쉬워 callback 임을 명시적으로 드러내기 위함.
#[derive(Clone)]
struct Subscriber {
    id: u64,
    tx: SyncSender<OutputChunk>,
    on_evict: Arc<dyn Fn() + Send + Sync>,
}

pub fn serve_forever() -> Result<()> {
    let socket = paths::socket_path()?;
    prepare_socket_path(&socket)?;
    let listener =
        UnixListener::bind(&socket).with_context(|| format!("bind {}", socket.display()))?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("chmod 0600 {}", socket.display()))?;
    eprintln!("lterm daemon listening on {}", socket.display());

    let state = Arc::new(State::default());
    for stream in listener.incoming() {
        if state.shutting_down.load(Ordering::SeqCst) {
            break;
        }
        match stream {
            Ok(stream) => {
                let Some(connection_guard) = state.try_acquire_connection() else {
                    drop(stream);
                    continue;
                };
                let state = Arc::clone(&state);
                thread::spawn(move || {
                    let _connection_guard = connection_guard;
                    if let Err(err) = handle_connection(state, stream) {
                        eprintln!("connection error: {err:#}");
                    }
                });
            }
            Err(err) => eprintln!("accept error: {err}"),
        }
    }
    Ok(())
}

#[derive(Default)]
struct State {
    sessions: Mutex<SessionMaps>,
    shutting_down: AtomicBool,
    active_connections: AtomicUsize,
}

impl State {
    fn try_acquire_connection(self: &Arc<Self>) -> Option<ConnectionGuard> {
        let mut current = self.active_connections.load(Ordering::SeqCst);
        loop {
            if current >= MAX_CONNECTIONS {
                return None;
            }
            match self.active_connections.compare_exchange(
                current,
                current + 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => {
                    return Some(ConnectionGuard {
                        state: Arc::clone(self),
                    });
                }
                Err(next) => current = next,
            }
        }
    }
}

struct ConnectionGuard {
    state: Arc<State>,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.state.active_connections.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Default)]
struct SessionMaps {
    by_name: HashMap<String, Arc<Session>>,
    by_pane: HashMap<String, Arc<Session>>,
    by_id: HashMap<String, Arc<Session>>,
    reserved_names: HashSet<String>,
    reserved_panes: HashSet<String>,
}

struct Session {
    id: String,
    name: String,
    pane_id: String,
    parent_pane_id: Mutex<Option<String>>,
    parent_session_id: Mutex<Option<String>>,
    parent_token: String,
    command: String,
    cwd: String,
    created_unix_ms: u128,
    process_id: Option<u32>,
    process_group_id: Option<i32>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    ring: Mutex<VecDeque<u8>>,
    subscribers: Mutex<Vec<Subscriber>>,
    output_state: Mutex<()>,
    next_subscriber_id: AtomicU64,
    alive: AtomicBool,
    exit_code: AtomicI32,
    rows: Mutex<u16>,
    cols: Mutex<u16>,
}

impl Session {
    fn info(&self) -> SessionInfo {
        let exit = self.exit_code.load(Ordering::SeqCst);
        SessionInfo {
            id: self.id.clone(),
            name: self.name.clone(),
            pane_id: self.pane_id.clone(),
            parent_pane_id: lock(&self.parent_pane_id).clone(),
            parent_session_id: lock(&self.parent_session_id).clone(),
            command: self.command.clone(),
            cwd: self.cwd.clone(),
            created_unix_ms: self.created_unix_ms,
            alive: self.alive.load(Ordering::SeqCst),
            exit_code: if exit == i32::MIN { None } else { Some(exit) },
            rows: *lock(&self.rows),
            cols: *lock(&self.cols),
            attached_clients: lock(&self.subscribers).len(),
            process_id: self.process_id,
            process_group_id: self.process_group_id,
        }
    }

    fn append_output(&self, bytes: &[u8]) {
        let _output_guard = lock(&self.output_state);
        {
            let mut ring = lock(&self.ring);
            for byte in bytes {
                if ring.len() >= RING_LIMIT {
                    ring.pop_front();
                }
                ring.push_back(*byte);
            }
        }

        let subscribers = lock(&self.subscribers).clone();
        let chunk: Option<OutputChunk> = if subscribers.is_empty() {
            None
        } else {
            Some(Arc::from(bytes))
        };
        let mut disconnected = Vec::new();
        for sub in &subscribers {
            let Some(chunk) = &chunk else {
                break;
            };
            match sub.tx.try_send(Arc::clone(chunk)) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                    disconnected.push(sub.id);
                }
            }
        }
        if !disconnected.is_empty() {
            // RC-2b: backpressure로 sub를 evict할 때 attach UnixStream 도 같이 닫아
            // input loop를 EOF로 깨운다. 그렇지 않으면 output 만 끊기고 input은
            // 살아있는 zombie attach 가 만들어져 사용자가 frozen으로 인지하는 동안
            // keystroke 가 PTY로 흘러들어가 위험한 명령이 실행될 수 있다.
            //
            // shutdown 훅은 subscribers lock 을 해제한 뒤 호출한다. handle_attach 측
            // input loop 가 EOF로 깨어나면서 unsubscribe 경로로 들어오면 같은 lock을
            // 다시 잡으려 시도하므로, lock 안에서 호출하면 deadlock 위험이 있다.
            let shutdowns = {
                let mut subscribers = lock(&self.subscribers);
                evict_disconnected_subscribers(&mut subscribers, &disconnected)
            };
            for shutdown in shutdowns {
                shutdown();
            }
        }
    }

    fn capture(&self, start: Option<i32>) -> String {
        sanitize::terminal_capture(&self.capture_bytes(start))
    }

    fn capture_bytes(&self, start: Option<i32>) -> Vec<u8> {
        let ring = lock(&self.ring);
        let bytes: Vec<u8> = ring.iter().copied().collect();
        let Some(start) = start else {
            return bytes;
        };
        let spans = line_spans(&bytes);
        if spans.is_empty() {
            return bytes;
        }
        if start < 0 {
            let keep = (-start) as usize;
            let first = spans.len().saturating_sub(keep);
            let begin = spans[first].0;
            return bytes[begin..].to_vec();
        }
        let first = start as usize;
        if first >= spans.len() {
            return Vec::new();
        }
        bytes[spans[first].0..].to_vec()
    }

    fn subscribe_with_snapshot(
        &self,
        on_evict: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<(u64, Receiver<OutputChunk>, Vec<u8>)> {
        let _output_guard = lock(&self.output_state);
        let initial = {
            let ring = lock(&self.ring);
            ring.iter().copied().collect()
        };
        let (id, rx) = self.subscribe_locked(on_evict)?;
        Ok((id, rx, initial))
    }

    fn subscribe_locked(
        &self,
        on_evict: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<(u64, Receiver<OutputChunk>)> {
        let id = self.next_subscriber_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::sync_channel(SUBSCRIBER_QUEUE_LIMIT);
        let mut subscribers = lock(&self.subscribers);
        if subscribers.len() >= MAX_SUBSCRIBERS_PER_SESSION {
            bail!("too many attached subscribers for session {}", self.name);
        }
        subscribers.push(Subscriber { id, tx, on_evict });
        Ok((id, rx))
    }

    fn unsubscribe(&self, subscriber_id: u64) {
        let _output_guard = lock(&self.output_state);
        lock(&self.subscribers).retain(|sub| sub.id != subscriber_id);
    }

    fn close_subscribers(&self) {
        let _output_guard = lock(&self.output_state);
        lock(&self.subscribers).clear();
    }
}

/// `disconnected` 에 해당하는 sub들을 `subscribers` 에서 제거하고 그들의 shutdown 훅을
/// 반환한다. 호출자는 반환된 훅들을 **subscribers lock 을 해제한 뒤** 호출해야 한다 —
/// shutdown 훅이 내부적으로 attach input loop 를 깨우고, 그 input loop 가 unsubscribe
/// 경로로 들어오면 같은 lock을 다시 잡으려 하므로 lock 안에서 호출 시 deadlock 위험이 있다.
fn evict_disconnected_subscribers(
    subscribers: &mut Vec<Subscriber>,
    disconnected: &[u64],
) -> Vec<Arc<dyn Fn() + Send + Sync>> {
    let mut shutdowns = Vec::new();
    subscribers.retain(|sub| {
        if disconnected.contains(&sub.id) {
            shutdowns.push(Arc::clone(&sub.on_evict));
            false
        } else {
            true
        }
    });
    shutdowns
}

fn handle_connection(state: Arc<State>, mut stream: UnixStream) -> Result<()> {
    verify_peer_owner(&stream)?;
    stream
        .set_read_timeout(Some(REQUEST_READ_TIMEOUT))
        .context("set request read timeout")?;
    let line = read_request_line(&mut stream)?;
    stream.set_read_timeout(None).ok();
    if line.trim().is_empty() {
        return Ok(());
    }
    let request: Request = serde_json::from_str(&line)
        .with_context(|| format!("parse request: {}", sanitized_preview(&line)))?;

    if let Request::Attach { target } = request {
        return handle_attach(state, stream, &target);
    }

    let shutdown = matches!(request, Request::Shutdown);
    let response = match handle_request(&state, request) {
        Ok(response) => response,
        Err(err) => Response::err(format!("{err:#}")),
    };
    serde_json::to_writer(&mut stream, &response).context("write response")?;
    stream.write_all(b"\n").context("write response newline")?;
    stream.flush().ok();

    if shutdown && response.ok {
        thread::sleep(Duration::from_millis(25));
        std::process::exit(0);
    }
    Ok(())
}

fn read_request_line(stream: &mut UnixStream) -> Result<String> {
    let mut bytes = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        let n = stream.read(&mut byte).context("read request line")?;
        if n == 0 {
            break;
        }
        bytes.push(byte[0]);
        if bytes.len() > MAX_REQUEST_BYTES {
            bail!("request exceeded {MAX_REQUEST_BYTES} bytes");
        }
        if byte[0] == b'\n' {
            break;
        }
    }
    if !bytes.is_empty() && !bytes.ends_with(b"\n") {
        bail!("request missing newline before EOF");
    }
    String::from_utf8(bytes).context("request is not valid UTF-8")
}

fn sanitized_preview(value: &str) -> String {
    const LIMIT: usize = 256;
    let mut preview = String::new();
    for ch in value.chars().take(LIMIT) {
        match ch {
            '\t' | '\n' | '\r' => preview.push(' '),
            ch if ch.is_control() => preview.push('�'),
            ch => preview.push(ch),
        }
    }
    if value.chars().count() > LIMIT {
        preview.push('…');
    }
    preview
}

fn handle_request(state: &Arc<State>, request: Request) -> Result<Response> {
    match request {
        Request::Ping => Ok(Response::ok(serde_json::json!({ "pong": true }))),
        Request::New {
            name,
            command,
            cwd,
            rows,
            cols,
            parent_pane_id,
            parent_token,
            env,
            tmux,
        } => {
            let session = create_session(
                state,
                NewSessionParams {
                    name,
                    command,
                    cwd,
                    rows,
                    cols,
                    parent_pane_id,
                    parent_token,
                    env,
                    tmux,
                },
            )?;
            Ok(Response::ok(session.info()))
        }
        Request::AttachOrNew {
            target,
            cwd,
            parent_pane_id,
            parent_token,
        } => {
            let target = normalize_target(&target);
            if let Ok(session) = resolve_session(state, &target) {
                return Ok(Response::ok(session.info()));
            }
            if target.starts_with('%') {
                bail!("cannot auto-create a missing pane target: {target}");
            }
            let session = create_session(
                state,
                NewSessionParams {
                    name: Some(target),
                    command: None,
                    cwd,
                    rows: None,
                    cols: None,
                    parent_pane_id,
                    parent_token,
                    env: HashMap::new(),
                    tmux: false,
                },
            )?;
            Ok(Response::ok(session.info()))
        }
        Request::List => {
            let sessions = lock(&state.sessions);
            let mut infos: Vec<_> = sessions.by_pane.values().map(|s| s.info()).collect();
            infos.sort_by(|a, b| a.created_unix_ms.cmp(&b.created_unix_ms));
            Ok(Response::ok(infos))
        }
        Request::Info { target } => Ok(Response::ok(resolve_session(state, &target)?.info())),
        Request::Kill { target } => {
            let session = resolve_session(state, &target)?;
            terminate_session(state, &session);
            Ok(Response::empty())
        }
        Request::Send { target, data } => {
            let session = resolve_session(state, &target)?;
            if !session.alive.load(Ordering::SeqCst) {
                bail!("session is not alive: {target}");
            }
            lock(&session.writer)
                .write_all(&data)
                .context("write to pty")?;
            Ok(Response::empty())
        }
        Request::Capture { target, start } => {
            let session = resolve_session(state, &target)?;
            Ok(Response::ok(session.capture(start)))
        }
        Request::Resize { target, rows, cols } => {
            if rows == 0 || cols == 0 {
                bail!("resize dimensions must be at least 1 row and 1 column");
            }
            let session = resolve_session(state, &target)?;
            lock(&session.master)
                .resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .context("resize pty")?;
            *lock(&session.rows) = rows;
            *lock(&session.cols) = cols;
            Ok(Response::empty())
        }
        Request::Shutdown => {
            state.shutting_down.store(true, Ordering::SeqCst);
            let sessions: Vec<_> = lock(&state.sessions).by_pane.values().cloned().collect();
            for session in sessions {
                terminate_session(state, &session);
            }
            Ok(Response::empty())
        }
        Request::Attach { .. } => unreachable!(),
    }
}

struct ParentSession {
    id: String,
    pane_id: String,
}

struct ParentRequest {
    pane_id: String,
    token: String,
}

fn parent_request(
    parent_pane_id: Option<String>,
    parent_token: Option<String>,
) -> Option<ParentRequest> {
    let parent_pane_id = parent_pane_id?;
    let parent_pane_id = normalize_target(&parent_pane_id);
    if !parent_pane_id.starts_with('%') {
        return None;
    }
    let token = parent_token.filter(|token| !token.is_empty())?;
    Some(ParentRequest {
        pane_id: parent_pane_id,
        token,
    })
}

fn resolve_parent_session_locked(
    sessions: &SessionMaps,
    request: &ParentRequest,
) -> Option<ParentSession> {
    let session = sessions.by_pane.get(&request.pane_id)?;
    if !session.alive.load(Ordering::SeqCst) || session.parent_token != request.token {
        return None;
    }
    Some(ParentSession {
        id: session.id.clone(),
        pane_id: session.pane_id.clone(),
    })
}

fn validate_parent_request(state: &Arc<State>, request: &ParentRequest) -> Result<()> {
    let sessions = lock(&state.sessions);
    resolve_parent_session_locked(&sessions, request)
        .map(|_| ())
        .with_context(|| format!("parent session no longer available: {}", request.pane_id))
}

struct NewSessionParams {
    name: Option<String>,
    command: Option<String>,
    cwd: Option<String>,
    rows: Option<u16>,
    cols: Option<u16>,
    parent_pane_id: Option<String>,
    parent_token: Option<String>,
    env: HashMap<String, String>,
    tmux: bool,
}

fn create_session(state: &Arc<State>, params: NewSessionParams) -> Result<Arc<Session>> {
    let parent_request = parent_request(params.parent_pane_id, params.parent_token);
    if let Some(parent_request) = parent_request.as_ref() {
        validate_parent_request(state, parent_request)?;
    }
    let reservation = reserve_session_identity(state, params.name)?;
    let pty_system = native_pty_system();
    let rows = params.rows.unwrap_or(24).max(1);
    let cols = params.cols.unwrap_or(80).max(1);
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("open pty")?;

    let id = Uuid::new_v4().to_string();
    let parent_token = Uuid::new_v4().to_string();
    let pane_id = reservation.pane_id().to_string();
    let name = reservation.name().to_string();
    let cwd = params
        .cwd
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|p| p.display().to_string())
        })
        .unwrap_or_else(|| ".".to_string());
    let command = params.command.unwrap_or_else(default_shell_command);
    let mut spawn_command = command.clone();
    let tmux_shim = if params.tmux {
        let shim = paths::shim_dir()?;
        let shim_path = shim.display().to_string();
        let quoted_shim = shlex::try_quote(&shim_path).context("quote tmux shim path")?;
        spawn_command = format!("PATH={quoted_shim}${{PATH:+:$PATH}}; export PATH; {command}");
        Some(shim)
    } else {
        None
    };

    let mut cmd = CommandBuilder::new(default_shell());
    cmd.arg("-lc");
    cmd.arg(&spawn_command);
    cmd.cwd(PathBuf::from(&cwd));
    for (key, value) in sanitize_child_env(params.env)? {
        cmd.env(key, value);
    }
    cmd.env("LTERM_SESSION", &name);
    cmd.env("LTERM_PANE", &pane_id);
    cmd.env("LTERM_PARENT_TOKEN", &parent_token);
    cmd.env("LTERM_SOCKET", paths::socket_path()?.display().to_string());
    cmd.env("LTERM_BIN", std::env::current_exe()?.display().to_string());
    if params.tmux {
        cmd.env("TMUX", fake_tmux_value()?);
        cmd.env("TMUX_PANE", &pane_id);
        cmd.env("TERM_PROGRAM", "lterm");
        let shim = tmux_shim.context("missing tmux shim path")?;
        let old_path = std::env::var("PATH").unwrap_or_default();
        cmd.env("PATH", format!("{}:{old_path}", shim.display()));
    }

    let child = pair
        .slave
        .spawn_command(cmd)
        .context("spawn command in pty")?;
    let child_guard = SpawnedChildGuard::new(child);
    let process_id = child_guard
        .child_ref()
        .process_id()
        .context("spawned child did not report a process id")?;
    let process_group_id =
        verified_process_group_id(pair.master.process_group_leader(), Some(process_id), &name);
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().context("clone pty reader")?;
    let writer = pair.master.take_writer().context("take pty writer")?;
    let (child, killer) = child_guard.into_parts();

    let session = Arc::new(Session {
        id,
        name: name.clone(),
        pane_id,
        parent_pane_id: Mutex::new(None),
        parent_session_id: Mutex::new(None),
        parent_token,
        command,
        cwd,
        created_unix_ms: now_unix_ms(),
        process_id: Some(process_id),
        process_group_id,
        child: Mutex::new(child),
        killer: Mutex::new(killer),
        master: Mutex::new(pair.master),
        writer: Mutex::new(writer),
        ring: Mutex::new(VecDeque::new()),
        subscribers: Mutex::new(Vec::new()),
        output_state: Mutex::new(()),
        next_subscriber_id: AtomicU64::new(1),
        alive: AtomicBool::new(true),
        exit_code: AtomicI32::new(i32::MIN),
        rows: Mutex::new(rows),
        cols: Mutex::new(cols),
    });

    if let Err(err) = reservation.commit(Arc::clone(&session), parent_request.as_ref()) {
        cleanup_uncommitted_session(&session);
        return Err(err);
    }

    let session_for_reader = Arc::clone(&session);
    thread::spawn(move || {
        let mut buf = [0_u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => session_for_reader.append_output(&buf[..n]),
                Err(err) if err.kind() == ErrorKind::Interrupted => continue,
                Err(err) if err.kind() == ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(err) => {
                    eprintln!("pty read error for {}: {err}", session_for_reader.name);
                    break;
                }
            }
        }
        session_for_reader.close_subscribers();
    });

    let state_for_waiter = Arc::clone(state);
    let session_for_waiter = Arc::clone(&session);
    thread::spawn(move || {
        let exit_code = match lock(&session_for_waiter.child).wait() {
            Ok(status) => status.exit_code().min(i32::MAX as u32) as i32,
            Err(err) => {
                eprintln!("wait error for {}: {err}", session_for_waiter.name);
                1
            }
        };
        session_for_waiter
            .exit_code
            .store(exit_code, Ordering::SeqCst);
        session_for_waiter.alive.store(false, Ordering::SeqCst);
        session_for_waiter.close_subscribers();
        terminate_child_sessions(&state_for_waiter, &session_for_waiter.id);
        remove_session(&state_for_waiter, &session_for_waiter);
    });

    Ok(session)
}

struct SpawnedChildGuard {
    child: Option<Box<dyn Child + Send + Sync>>,
    killer: Option<Box<dyn ChildKiller + Send + Sync>>,
}

impl SpawnedChildGuard {
    fn new(child: Box<dyn Child + Send + Sync>) -> Self {
        let killer = child.clone_killer();
        Self {
            child: Some(child),
            killer: Some(killer),
        }
    }

    fn child_ref(&self) -> &(dyn Child + Send + Sync) {
        self.child
            .as_deref()
            .expect("spawned child guard owns child")
    }

    fn into_parts(
        mut self,
    ) -> (
        Box<dyn Child + Send + Sync>,
        Box<dyn ChildKiller + Send + Sync>,
    ) {
        let child = self.child.take().expect("spawned child guard owns child");
        let killer = self.killer.take().expect("spawned child guard owns killer");
        (child, killer)
    }
}

impl Drop for SpawnedChildGuard {
    fn drop(&mut self) {
        if let Some(killer) = self.killer.as_mut() {
            let _ = killer.kill();
        }
        if let Some(child) = self.child.as_mut() {
            let _ = child.wait();
        }
    }
}

fn cleanup_uncommitted_session(session: &Session) {
    let _ = lock(&session.killer).kill();
    let _ = lock(&session.child).wait();
    session.close_subscribers();
}

struct SessionReservation {
    state: Arc<State>,
    name: String,
    pane_id: String,
    active: bool,
}

impl SessionReservation {
    fn name(&self) -> &str {
        &self.name
    }

    fn pane_id(&self) -> &str {
        &self.pane_id
    }

    fn commit(
        mut self,
        session: Arc<Session>,
        parent_request: Option<&ParentRequest>,
    ) -> Result<()> {
        let mut sessions = lock(&self.state.sessions);
        if !sessions.reserved_names.contains(&self.name)
            || !sessions.reserved_panes.contains(&self.pane_id)
        {
            bail!("internal session reservation missing");
        }
        if sessions.by_name.contains_key(&session.name)
            || sessions.by_pane.contains_key(&session.pane_id)
            || sessions.by_id.contains_key(&session.id)
        {
            bail!("internal session id collision");
        }
        let parent_session = match parent_request {
            Some(request) => Some(
                resolve_parent_session_locked(&sessions, request).with_context(|| {
                    format!("parent session no longer available: {}", request.pane_id)
                })?,
            ),
            None => None,
        };
        *lock(&session.parent_pane_id) =
            parent_session.as_ref().map(|parent| parent.pane_id.clone());
        *lock(&session.parent_session_id) = parent_session.map(|parent| parent.id);
        sessions.reserved_names.remove(&self.name);
        sessions.reserved_panes.remove(&self.pane_id);
        sessions
            .by_name
            .insert(session.name.clone(), Arc::clone(&session));
        sessions
            .by_pane
            .insert(session.pane_id.clone(), Arc::clone(&session));
        sessions.by_id.insert(session.id.clone(), session);
        self.active = false;
        Ok(())
    }
}

impl Drop for SessionReservation {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let mut sessions = lock(&self.state.sessions);
        sessions.reserved_names.remove(&self.name);
        sessions.reserved_panes.remove(&self.pane_id);
    }
}

fn reserve_session_identity(
    state: &Arc<State>,
    requested_name: Option<String>,
) -> Result<SessionReservation> {
    if let Some(name) = &requested_name {
        validate_session_name_syntax(name)?;
    }

    let mut sessions = lock(&state.sessions);
    if sessions.by_pane.len() + sessions.reserved_panes.len() >= MAX_SESSIONS {
        bail!("too many lterm sessions; limit is {MAX_SESSIONS}");
    }
    if let Some(name) = &requested_name
        && (sessions.by_name.contains_key(name) || sessions.reserved_names.contains(name))
    {
        bail!("session name already exists: {name}");
    }

    let mut selected = None;
    for pane_num in 0..MAX_SESSIONS {
        let pane_id = format!("%{pane_num}");
        if sessions.by_pane.contains_key(&pane_id) || sessions.reserved_panes.contains(&pane_id) {
            continue;
        }
        let name = requested_name
            .clone()
            .unwrap_or_else(|| format!("lterm-{pane_num}"));
        if requested_name.is_none()
            && (sessions.by_name.contains_key(&name) || sessions.reserved_names.contains(&name))
        {
            continue;
        }
        selected = Some((pane_id, name));
        break;
    }

    let (pane_id, name) = selected.context("no available lterm pane id")?;
    sessions.reserved_panes.insert(pane_id.clone());
    sessions.reserved_names.insert(name.clone());
    Ok(SessionReservation {
        state: Arc::clone(state),
        name,
        pane_id,
        active: true,
    })
}

fn handle_attach(state: Arc<State>, mut stream: UnixStream, target: &str) -> Result<()> {
    let session = match resolve_session(&state, target) {
        Ok(session) => session,
        Err(err) => {
            let response = Response::err(format!("{err:#}"));
            serde_json::to_writer(&mut stream, &response).ok();
            stream.write_all(b"\n").ok();
            return Ok(());
        }
    };

    // RC-2b: subscriber 가 backpressure 로 evict 될 때 호출되는 shutdown 훅 구성.
    // 같은 socket fd 의 SHUT_RDWR 은 input loop의 stream.read()를 EOF 로 깨워 attach 전체를
    // 종료시키므로, output 만 끊긴 채 input은 살아있는 zombie attach 가 만들어지지 않는다.
    let shutdown_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(err) => {
            let response = Response::err(format!("{err:#}"));
            serde_json::to_writer(&mut stream, &response).ok();
            stream.write_all(b"\n").ok();
            return Ok(());
        }
    };
    let on_evict: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        let _ = shutdown_stream.shutdown(std::net::Shutdown::Both);
    });

    let (subscriber_id, rx, initial) = match session.subscribe_with_snapshot(on_evict) {
        Ok(subscription) => subscription,
        Err(err) => {
            let response = Response::err(format!("{err:#}"));
            serde_json::to_writer(&mut stream, &response).ok();
            stream.write_all(b"\n").ok();
            return Ok(());
        }
    };
    serde_json::to_writer(&mut stream, &Response::empty()).context("write attach ok")?;
    stream.write_all(b"\n").context("write attach ok newline")?;
    if !initial.is_empty() {
        stream.write_all(&initial).ok();
    }
    let mut output = stream.try_clone().context("clone output stream")?;
    let output_thread = thread::spawn(move || {
        for bytes in rx {
            if output.write_all(bytes.as_ref()).is_err() {
                break;
            }
            let _ = output.flush();
        }
    });

    let mut input = stream;
    input
        .set_read_timeout(Some(Duration::from_millis(100)))
        .context("set attach input read timeout")?;
    let mut buf = [0_u8; 8192];
    while session.alive.load(Ordering::SeqCst) {
        let n = match input.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(err) if err.kind() == ErrorKind::Interrupted => continue,
            Err(err)
                if err.kind() == ErrorKind::WouldBlock || err.kind() == ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(_) => break,
        };
        if lock(&session.writer).write_all(&buf[..n]).is_err() {
            break;
        }
    }
    session.unsubscribe(subscriber_id);
    let _ = output_thread.join();
    Ok(())
}

fn remove_session(state: &Arc<State>, session: &Session) {
    let mut sessions = lock(&state.sessions);
    if sessions
        .by_name
        .get(&session.name)
        .is_some_and(|s| s.id == session.id)
    {
        sessions.by_name.remove(&session.name);
    }
    if sessions
        .by_pane
        .get(&session.pane_id)
        .is_some_and(|s| s.id == session.id)
    {
        sessions.by_pane.remove(&session.pane_id);
    }
    if sessions
        .by_id
        .get(&session.id)
        .is_some_and(|s| s.id == session.id)
    {
        sessions.by_id.remove(&session.id);
    }
}

fn terminate_child_sessions(state: &Arc<State>, parent_session_id: &str) {
    let children: Vec<_> = {
        let sessions = lock(&state.sessions);
        sessions
            .by_pane
            .values()
            .filter(|candidate| {
                lock(&candidate.parent_session_id)
                    .as_deref()
                    .is_some_and(|id| id == parent_session_id)
            })
            .cloned()
            .collect()
    };
    for child in children {
        terminate_session(state, &child);
    }
}

fn verified_process_group_id(
    candidate: Option<libc::pid_t>,
    process_id: Option<u32>,
    session_name: &str,
) -> Option<i32> {
    let pgid = candidate.filter(|pgid| *pgid > 1)?;
    let Some(pid) = process_id.and_then(|pid| libc::pid_t::try_from(pid).ok()) else {
        eprintln!("session {session_name} has no verifiable child pid for process group cleanup");
        return None;
    };
    let actual = unsafe { libc::getpgid(pid) };
    if actual == pgid {
        return Some(pgid);
    }
    let err = std::io::Error::last_os_error();
    if actual < 0 {
        eprintln!("failed to verify process group for session {session_name}: {err}");
    } else {
        eprintln!(
            "not using process group {pgid} for session {session_name}: child {pid} is in group {actual}"
        );
    }
    None
}

fn terminate_session(state: &Arc<State>, session: &Session) {
    if !session.alive.swap(false, Ordering::SeqCst) {
        session.close_subscribers();
        terminate_child_sessions(state, &session.id);
        remove_session(state, session);
        return;
    }
    session.close_subscribers();
    terminate_child_sessions(state, &session.id);
    signal_process_group(session, libc::SIGHUP);
    wait_for_process_group_exit(session, Duration::from_millis(150));
    signal_process_group(session, libc::SIGTERM);
    wait_for_process_group_exit(session, Duration::from_millis(350));
    signal_process_group(session, libc::SIGKILL);
    wait_for_process_group_exit(session, Duration::from_millis(150));
    session.close_subscribers();
    remove_session(state, session);
}

fn signal_process_group(session: &Session, signal: libc::c_int) {
    if let Some(pgid) = verified_session_process_group_id(session) {
        let rc = unsafe { libc::kill(-pgid, signal) };
        if rc == 0 {
            return;
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::ESRCH) {
            eprintln!(
                "failed to signal process group {} for {}: {}",
                pgid, session.name, err
            );
        }
    }
    if signal == libc::SIGKILL {
        let _ = lock(&session.killer).kill();
    } else if let Some(pid) = session
        .process_id
        .and_then(|pid| libc::pid_t::try_from(pid).ok())
    {
        let rc = unsafe { libc::kill(pid, signal) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::ESRCH) {
                eprintln!(
                    "failed to signal child process {} for {}: {}",
                    pid, session.name, err
                );
            }
        }
    }
}

fn verified_session_process_group_id(session: &Session) -> Option<i32> {
    let pgid = session.process_group_id.filter(|pgid| *pgid > 1)?;
    if process_group_still_owns_child(session.process_id, pgid) {
        return Some(pgid);
    }
    eprintln!(
        "not signaling process group {} for {}: child pid {:?} no longer verifies that group",
        pgid, session.name, session.process_id
    );
    None
}

fn process_group_still_owns_child(process_id: Option<u32>, pgid: i32) -> bool {
    let Some(pid) = process_id.and_then(|pid| libc::pid_t::try_from(pid).ok()) else {
        return false;
    };
    unsafe { libc::getpgid(pid) == pgid }
}

fn wait_for_process_group_exit(session: &Session, timeout: Duration) {
    let Some(pgid) = verified_session_process_group_id(session) else {
        thread::sleep(timeout);
        return;
    };
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        let rc = unsafe { libc::kill(-pgid, 0) };
        if rc != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn resolve_session(state: &Arc<State>, target: &str) -> Result<Arc<Session>> {
    let target = normalize_target(target);
    let sessions = lock(&state.sessions);
    if target.starts_with('%') {
        if let Some(session) = sessions.by_pane.get(&target) {
            return Ok(Arc::clone(session));
        }
    }
    if let Some(session) = sessions.by_name.get(&target) {
        return Ok(Arc::clone(session));
    }
    if let Some(session) = sessions.by_id.get(&target) {
        return Ok(Arc::clone(session));
    }
    if !target.starts_with('%') {
        let pane = format!("%{target}");
        if let Some(session) = sessions.by_pane.get(&pane) {
            return Ok(Arc::clone(session));
        }
    }
    bail!("no such lterm session or pane: {target}")
}

fn validate_session_name_syntax(name: &str) -> Result<()> {
    if name.trim().is_empty() {
        bail!("session name cannot be empty");
    }
    if name.len() > 128 {
        bail!("session name cannot exceed 128 bytes");
    }
    if name.starts_with('%') {
        bail!("session name cannot look like a pane id: {name}");
    }
    if Uuid::parse_str(name).is_ok() {
        bail!("session name cannot look like a UUID: {name}");
    }
    if !name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
    {
        bail!("session name may only contain ASCII letters, numbers, '.', '_' and '-'");
    }
    Ok(())
}

fn sanitize_child_env(env: HashMap<String, String>) -> Result<HashMap<String, String>> {
    let mut safe = HashMap::with_capacity(env.len());
    for (key, value) in env {
        validate_env_key(&key)?;
        validate_env_value(&key, &value)?;
        if is_dangerous_env_key(&key) {
            bail!("refusing dangerous child environment variable: {key}");
        }
        safe.insert(key, value);
    }
    Ok(safe)
}

fn validate_env_key(key: &str) -> Result<()> {
    if key.is_empty() || key.len() > 128 {
        bail!("invalid child environment variable name length");
    }
    if key.contains('=') || key.contains('\0') {
        bail!("invalid child environment variable name: {key:?}");
    }
    let mut chars = key.chars();
    let first = chars.next().expect("checked non-empty");
    if !(first == '_' || first.is_ascii_alphabetic()) {
        bail!("invalid child environment variable name: {key}");
    }
    if !chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric()) {
        bail!("invalid child environment variable name: {key}");
    }
    Ok(())
}

fn validate_env_value(key: &str, value: &str) -> Result<()> {
    if value.contains('\0') {
        bail!("child environment variable {key} contains NUL");
    }
    if value.len() > 32 * 1024 {
        bail!("child environment variable {key} is too large");
    }
    Ok(())
}

fn is_dangerous_env_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    upper == "LD_PRELOAD"
        || upper == "LD_LIBRARY_PATH"
        || upper == "LD_AUDIT"
        || upper == "PATH"
        || upper == "BASH_ENV"
        || upper == "ENV"
        || upper == "PROMPT_COMMAND"
        || upper == "IFS"
        || upper == "BASHOPTS"
        || upper == "SHELLOPTS"
        || upper == "GLOBIGNORE"
        || upper == "TERMINFO"
        || upper == "TERMINFO_DIRS"
        || upper == "TMPDIR"
        || matches!(upper.as_str(), "PS0" | "PS1" | "PS2" | "PS3" | "PS4")
        || upper.starts_with("DYLD_")
        || upper.starts_with("BASH_FUNC_")
}

fn normalize_target(target: &str) -> String {
    let target = target.trim();
    if target.is_empty() {
        "main".to_string()
    } else {
        target.to_string()
    }
}

fn default_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
}

fn default_shell_command() -> String {
    format!(
        "exec {} -l",
        shlex::try_quote(&default_shell()).expect("default shell path should be shell-quotable")
    )
}

pub fn fake_tmux_value() -> Result<String> {
    Ok(format!(
        "{},{},0",
        paths::socket_path()?.display(),
        std::process::id()
    ))
}

fn now_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_else(|err| {
            eprintln!("system clock before UNIX_EPOCH: {err}");
            0
        })
}

fn line_spans(bytes: &[u8]) -> Vec<(usize, usize)> {
    if bytes.is_empty() {
        return Vec::new();
    }
    let mut spans = Vec::new();
    let mut start = 0;
    for (idx, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' {
            spans.push((start, idx + 1));
            start = idx + 1;
        }
    }
    if start < bytes.len() {
        spans.push((start, bytes.len()));
    }
    spans
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            eprintln!("recovering poisoned mutex");
            poisoned.into_inner()
        }
    }
}

fn prepare_socket_path(socket: &Path) -> Result<()> {
    let parent = socket
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .context("socket path must include a parent directory")?;
    paths::ensure_private_dir(parent)?;

    match fs::symlink_metadata(socket) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                bail!("refusing symlink socket path {}", socket.display());
            }
            if !meta.file_type().is_socket() {
                bail!(
                    "refusing to remove non-socket path {} while preparing lterm socket",
                    socket.display()
                );
            }
            if ping_socket(socket).unwrap_or(false) {
                bail!("lterm daemon already running at {}", socket.display());
            }
            fs::remove_file(socket)
                .with_context(|| format!("remove stale socket {}", socket.display()))?;
        }
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => return Err(err).with_context(|| format!("lstat {}", socket.display())),
    }
    Ok(())
}

fn ping_socket(socket: &Path) -> Result<bool> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_millis(500)))?;
    stream.set_write_timeout(Some(Duration::from_millis(500)))?;
    serde_json::to_writer(&mut stream, &Request::Ping)?;
    stream.write_all(b"\n")?;
    stream.shutdown(std::net::Shutdown::Write).ok();
    let mut bytes = Vec::new();
    stream.take(64 * 1024).read_to_end(&mut bytes)?;
    let response: Response = serde_json::from_slice(&bytes)?;
    Ok(response.ok)
}

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd"
))]
fn verify_peer_owner(stream: &UnixStream) -> Result<()> {
    let mut uid = 0_u32;
    let mut gid = 0_u32;
    let rc = unsafe { getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    if rc != 0 {
        bail!("getpeereid failed: {}", std::io::Error::last_os_error());
    }
    let expected = unsafe { geteuid() };
    if uid != expected {
        bail!("peer uid {uid} does not match daemon uid {expected}");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn verify_peer_owner(stream: &UnixStream) -> Result<()> {
    #[repr(C)]
    struct UCred {
        pid: i32,
        uid: u32,
        gid: u32,
    }
    const SOL_SOCKET: c_int = 1;
    const SO_PEERCRED: c_int = 17;
    let mut cred = UCred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<UCred>() as u32;
    let rc = unsafe {
        getsockopt(
            stream.as_raw_fd(),
            SOL_SOCKET,
            SO_PEERCRED,
            (&mut cred as *mut UCred).cast(),
            &mut len,
        )
    };
    if rc != 0 {
        bail!(
            "getsockopt(SO_PEERCRED) failed: {}",
            std::io::Error::last_os_error()
        );
    }
    let expected = unsafe { geteuid() };
    if cred.uid != expected {
        bail!("peer uid {} does not match daemon uid {expected}", cred.uid);
    }
    Ok(())
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "linux"
)))]
fn verify_peer_owner(_stream: &UnixStream) -> Result<()> {
    bail!("peer credential verification is not implemented for this platform")
}

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd"
))]
unsafe extern "C" {
    fn getpeereid(fd: c_int, euid: *mut u32, egid: *mut u32) -> c_int;
    fn geteuid() -> u32;
}

#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn getsockopt(
        socket: c_int,
        level: c_int,
        option_name: c_int,
        option_value: *mut std::ffi::c_void,
        option_len: *mut u32,
    ) -> c_int;
    fn geteuid() -> u32;
}

#[cfg(test)]
mod tests {
    use super::{Subscriber, evict_disconnected_subscribers, process_group_still_owns_child};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::mpsc;

    #[test]
    fn process_group_check_requires_current_child_group_match() {
        let pid = std::process::id();
        let pgid = unsafe { libc::getpgid(pid as libc::pid_t) };
        assert!(pgid > 1, "current process should have a real process group");

        assert!(process_group_still_owns_child(Some(pid), pgid));
        assert!(!process_group_still_owns_child(None, pgid));
        let mismatched_pgid = if pgid == i32::MAX { pgid - 1 } else { pgid + 1 };
        assert!(!process_group_still_owns_child(Some(pid), mismatched_pgid));
    }

    /// shutdown 호출 횟수를 세는 카운터를 가진 테스트용 Subscriber.
    /// `tx` 의 receiver는 보관하지 않으므로 broadcast 가 실제로 일어나는 시나리오는
    /// 검증하지 않는다 — 이 helper 의 책임은 evict 시 shutdown 훅이 정확히 호출되는지.
    fn test_subscriber(id: u64, calls: Arc<AtomicU32>) -> Subscriber {
        let (tx, _rx) = mpsc::sync_channel(1);
        let calls_for_closure = Arc::clone(&calls);
        Subscriber {
            id,
            tx,
            on_evict: Arc::new(move || {
                calls_for_closure.fetch_add(1, Ordering::SeqCst);
            }),
        }
    }

    #[test]
    fn evict_disconnected_returns_no_shutdowns_when_disconnected_empty() {
        let calls = Arc::new(AtomicU32::new(0));
        let mut subs = vec![test_subscriber(1, Arc::clone(&calls))];

        let shutdowns = evict_disconnected_subscribers(&mut subs, &[]);

        assert!(
            shutdowns.is_empty(),
            "no disconnected → no shutdown handles"
        );
        assert_eq!(subs.len(), 1, "no eviction → subscriber remains");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "shutdown must not be called when nothing is evicted",
        );
    }

    #[test]
    fn evict_disconnected_removes_only_listed_ids_and_returns_their_shutdowns() {
        let c1 = Arc::new(AtomicU32::new(0));
        let c2 = Arc::new(AtomicU32::new(0));
        let c3 = Arc::new(AtomicU32::new(0));
        let mut subs = vec![
            test_subscriber(1, Arc::clone(&c1)),
            test_subscriber(2, Arc::clone(&c2)),
            test_subscriber(3, Arc::clone(&c3)),
        ];

        let shutdowns = evict_disconnected_subscribers(&mut subs, &[1, 3]);

        // shutdown 훅은 호출 전이라 카운터는 아직 0.
        assert_eq!(shutdowns.len(), 2, "two shutdown handles for two evictions");
        assert_eq!(c1.load(Ordering::SeqCst), 0);
        assert_eq!(c3.load(Ordering::SeqCst), 0);

        for shutdown in &shutdowns {
            shutdown();
        }

        assert_eq!(subs.len(), 1, "only id=2 should remain");
        assert_eq!(subs[0].id, 2);
        assert_eq!(
            c1.load(Ordering::SeqCst),
            1,
            "evicted id=1 shutdown called exactly once",
        );
        assert_eq!(
            c2.load(Ordering::SeqCst),
            0,
            "retained id=2 shutdown must not be called",
        );
        assert_eq!(
            c3.load(Ordering::SeqCst),
            1,
            "evicted id=3 shutdown called exactly once",
        );
    }

    #[test]
    fn evict_disconnected_dedups_when_same_id_listed_twice() {
        // disconnected 목록에 id가 중복으로 들어와도 sub 은 단 한 번만 retain-out
        // 되며 shutdown 도 한 번만 발생해야 한다 (idempotent eviction invariant).
        let calls = Arc::new(AtomicU32::new(0));
        let mut subs = vec![test_subscriber(7, Arc::clone(&calls))];

        let shutdowns = evict_disconnected_subscribers(&mut subs, &[7, 7]);
        for shutdown in &shutdowns {
            shutdown();
        }

        assert!(subs.is_empty(), "id=7 evicted");
        assert_eq!(
            shutdowns.len(),
            1,
            "single subscriber yields single shutdown handle even if id repeats in list",
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
