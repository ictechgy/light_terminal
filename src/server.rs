use crate::paths;
use crate::protocol::{Request, Response, SessionInfo};
use anyhow::{Context, Result, bail};
use portable_pty::{Child, ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::os::raw::c_int;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const RING_LIMIT: usize = 2 * 1024 * 1024;
const SUBSCRIBER_QUEUE_LIMIT: usize = 128;

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
                let state = Arc::clone(&state);
                thread::spawn(move || {
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
    pane_index: AtomicU64,
    shutting_down: AtomicBool,
}

#[derive(Default)]
struct SessionMaps {
    by_name: HashMap<String, Arc<Session>>,
    by_pane: HashMap<String, Arc<Session>>,
    by_id: HashMap<String, Arc<Session>>,
}

struct Session {
    id: String,
    name: String,
    pane_id: String,
    command: String,
    cwd: String,
    created_unix_ms: u128,
    child: Mutex<Box<dyn Child + Send + Sync>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    ring: Mutex<VecDeque<u8>>,
    subscribers: Mutex<Vec<(u64, SyncSender<Vec<u8>>)>>,
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
            command: self.command.clone(),
            cwd: self.cwd.clone(),
            created_unix_ms: self.created_unix_ms,
            alive: self.alive.load(Ordering::SeqCst),
            exit_code: if exit == i32::MIN { None } else { Some(exit) },
            rows: *lock(&self.rows),
            cols: *lock(&self.cols),
        }
    }

    fn append_output(&self, bytes: &[u8]) {
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
        let mut disconnected = Vec::new();
        for (id, tx) in subscribers {
            match tx.try_send(bytes.to_vec()) {
                Ok(()) | Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Disconnected(_)) => disconnected.push(id),
            }
        }
        if !disconnected.is_empty() {
            let mut subscribers = lock(&self.subscribers);
            subscribers.retain(|(id, _)| !disconnected.contains(id));
        }
    }

    fn capture(&self, start: Option<i32>) -> String {
        String::from_utf8_lossy(&self.capture_bytes(start)).to_string()
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

    fn subscribe(&self) -> (u64, Receiver<Vec<u8>>) {
        let id = self.next_subscriber_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::sync_channel(SUBSCRIBER_QUEUE_LIMIT);
        lock(&self.subscribers).push((id, tx));
        (id, rx)
    }

    fn unsubscribe(&self, subscriber_id: u64) {
        lock(&self.subscribers).retain(|(id, _)| *id != subscriber_id);
    }

    fn close_subscribers(&self) {
        lock(&self.subscribers).clear();
    }
}

fn handle_connection(state: Arc<State>, mut stream: UnixStream) -> Result<()> {
    verify_peer_owner(&stream)?;
    let mut reader = BufReader::new(stream.try_clone().context("clone request stream")?);
    let mut line = String::new();
    reader.read_line(&mut line).context("read request line")?;
    if line.trim().is_empty() {
        return Ok(());
    }
    let request: Request =
        serde_json::from_str(&line).with_context(|| format!("parse request: {line}"))?;

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

fn handle_request(state: &Arc<State>, request: Request) -> Result<Response> {
    match request {
        Request::Ping => Ok(Response::ok(serde_json::json!({ "pong": true }))),
        Request::New {
            name,
            command,
            cwd,
            rows,
            cols,
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
                    env,
                    tmux,
                },
            )?;
            Ok(Response::ok(session.info()))
        }
        Request::AttachOrNew { target } => {
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
                    cwd: None,
                    rows: None,
                    cols: None,
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
            let _ = lock(&session.killer).kill();
            session.alive.store(false, Ordering::SeqCst);
            remove_session(state, &session);
            session.close_subscribers();
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
                let _ = lock(&session.killer).kill();
                session.alive.store(false, Ordering::SeqCst);
                session.close_subscribers();
            }
            Ok(Response::empty())
        }
        Request::Attach { .. } => unreachable!(),
    }
}

struct NewSessionParams {
    name: Option<String>,
    command: Option<String>,
    cwd: Option<String>,
    rows: Option<u16>,
    cols: Option<u16>,
    env: HashMap<String, String>,
    tmux: bool,
}

fn create_session(state: &Arc<State>, params: NewSessionParams) -> Result<Arc<Session>> {
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
    let pane_num = state.pane_index.fetch_add(1, Ordering::SeqCst);
    let pane_id = format!("%{pane_num}");
    let name = params.name.unwrap_or_else(|| format!("lterm-{pane_num}"));
    validate_session_name(state, &name)?;
    let cwd = params
        .cwd
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|p| p.display().to_string())
        })
        .unwrap_or_else(|| ".".to_string());
    let command = params.command.unwrap_or_else(default_shell_command);

    let mut cmd = CommandBuilder::new(default_shell());
    cmd.arg("-lc");
    cmd.arg(&command);
    cmd.cwd(PathBuf::from(&cwd));
    for (key, value) in params.env {
        cmd.env(key, value);
    }
    cmd.env("LTERM_SESSION", &name);
    cmd.env("LTERM_PANE", &pane_id);
    cmd.env("LTERM_SOCKET", paths::socket_path()?.display().to_string());
    cmd.env("LTERM_BIN", std::env::current_exe()?.display().to_string());
    if params.tmux {
        cmd.env("TMUX", fake_tmux_value()?);
        cmd.env("TMUX_PANE", &pane_id);
        cmd.env("TERM_PROGRAM", "lterm");
        let shim = paths::shim_dir()?;
        let old_path = std::env::var("PATH").unwrap_or_default();
        cmd.env("PATH", format!("{}:{old_path}", shim.display()));
    }

    let child = pair
        .slave
        .spawn_command(cmd)
        .context("spawn command in pty")?;
    let killer = child.clone_killer();
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().context("clone pty reader")?;
    let writer = pair.master.take_writer().context("take pty writer")?;

    let session = Arc::new(Session {
        id,
        name: name.clone(),
        pane_id,
        command,
        cwd,
        created_unix_ms: now_unix_ms(),
        child: Mutex::new(child),
        killer: Mutex::new(killer),
        master: Mutex::new(pair.master),
        writer: Mutex::new(writer),
        ring: Mutex::new(VecDeque::new()),
        subscribers: Mutex::new(Vec::new()),
        next_subscriber_id: AtomicU64::new(1),
        alive: AtomicBool::new(true),
        exit_code: AtomicI32::new(i32::MIN),
        rows: Mutex::new(rows),
        cols: Mutex::new(cols),
    });

    insert_session(state, Arc::clone(&session))?;

    let state_for_reader = Arc::clone(state);
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

        let exit_code = match lock(&session_for_reader.child).wait() {
            Ok(status) => status.exit_code().min(i32::MAX as u32) as i32,
            Err(err) => {
                eprintln!("wait error for {}: {err}", session_for_reader.name);
                1
            }
        };
        session_for_reader
            .exit_code
            .store(exit_code, Ordering::SeqCst);
        session_for_reader.alive.store(false, Ordering::SeqCst);
        session_for_reader.close_subscribers();
        remove_session(&state_for_reader, &session_for_reader);
    });

    Ok(session)
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

    serde_json::to_writer(&mut stream, &Response::empty()).context("write attach ok")?;
    stream.write_all(b"\n").context("write attach ok newline")?;

    let initial = session.capture_bytes(None);
    if !initial.is_empty() {
        stream.write_all(&initial).ok();
    }

    let (subscriber_id, rx) = session.subscribe();
    let mut output = stream.try_clone().context("clone output stream")?;
    let output_thread = thread::spawn(move || {
        for bytes in rx {
            if output.write_all(&bytes).is_err() {
                break;
            }
            let _ = output.flush();
        }
    });

    let mut input = stream;
    let mut buf = [0_u8; 8192];
    while session.alive.load(Ordering::SeqCst) {
        let n = match input.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(err) if err.kind() == ErrorKind::Interrupted => continue,
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

fn insert_session(state: &Arc<State>, session: Arc<Session>) -> Result<()> {
    let mut sessions = lock(&state.sessions);
    if sessions.by_name.contains_key(&session.name) {
        bail!("session name already exists: {}", session.name);
    }
    if sessions.by_pane.contains_key(&session.pane_id) || sessions.by_id.contains_key(&session.id) {
        bail!("internal session id collision");
    }
    sessions
        .by_name
        .insert(session.name.clone(), Arc::clone(&session));
    sessions
        .by_pane
        .insert(session.pane_id.clone(), Arc::clone(&session));
    sessions.by_id.insert(session.id.clone(), session);
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

fn validate_session_name(state: &Arc<State>, name: &str) -> Result<()> {
    if name.trim().is_empty() {
        bail!("session name cannot be empty");
    }
    if name.starts_with('%') {
        bail!("session name cannot look like a pane id: {name}");
    }
    if Uuid::parse_str(name).is_ok() {
        bail!("session name cannot look like a UUID: {name}");
    }
    let sessions = lock(&state.sessions);
    if sessions.by_name.contains_key(name) {
        bail!("session name already exists: {name}");
    }
    Ok(())
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
        shlex::try_quote(&default_shell()).unwrap_or_default()
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
    if socket.exists() {
        if ping_socket(socket).unwrap_or(false) {
            bail!("lterm daemon already running at {}", socket.display());
        }
        fs::remove_file(socket)
            .with_context(|| format!("remove stale socket {}", socket.display()))?;
    }
    Ok(())
}

fn ping_socket(socket: &Path) -> Result<bool> {
    let mut stream = UnixStream::connect(socket)?;
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
    Ok(())
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
