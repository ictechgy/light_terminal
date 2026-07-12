use crate::paths;
use crate::protocol::{
    CAPABILITY_PROTOCOL_VERSION, CHILD_COLOR_POLICY_ENV, CMUX_CONTEXT_ENV, CapabilityAction,
    CapabilityToken, DaemonStatus, InstrumentSnapshot, IssueInputCapabilityResult,
    MAX_CAPABILITY_INPUT_BYTES, MAX_INPUT_CAPABILITY_BUDGET, MAX_METADATA_JOURNAL_ENTRIES,
    MetadataHistoryResult, MetadataJournalEntry, MetadataOperation, MetadataPurgeAggregate,
    MetadataPurgeResult, MetadataStepDirection, MetadataStepResult, MetadataValue,
    PROTOCOL_VERSION, Request, Response, SensitiveCapabilityRequest, SessionInfo, StatusTheme,
    WaitContainsResult, WaitExitResult,
};
use crate::sanitize;
use anyhow::{Context, Result, anyhow, bail};
use libc::{c_int, mode_t};
use portable_pty::{Child, ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const RING_LIMIT: usize = 2 * 1024 * 1024;
const MAX_REQUEST_BYTES: usize = 1024 * 1024;
const MAX_CONNECTIONS: usize = 64;
const MAX_BLOCKING_WAITS: usize = 16;
const MAX_WAIT_CONTAINS_NEEDLE_BYTES: usize = 4096;
const MAX_SESSIONS: usize = 256;
const MAX_SUBSCRIBERS_PER_SESSION: usize = 32;
const MAX_TERMINAL_ROWS: u16 = 1000;
const MAX_TERMINAL_COLS: u16 = 1000;
const MAX_TERMINAL_CELLS: u32 = 200_000;
const MAX_PENDING_ESCAPE_BYTES: usize = 8192;
const ALT_SCREEN_ENTER: &[u8] = b"\x1b[?1049h";
#[cfg(debug_assertions)]
const INTERNAL_TEST_MODE_ENV: &str = "LTERM_INTERNAL_TEST_MODE";
#[cfg(debug_assertions)]
const INTERNAL_TEST_DEGRADE_TERMINAL_PARSER_ENV: &str =
    "LTERM_INTERNAL_TEST_DEGRADE_TERMINAL_PARSER";
/// PR #16: subscriber 별 broadcast 채널 슬롯 한도. 슬롯 하나가 들고 있는 것은
/// `Arc<[u8]>` 의 fat pointer 라 메모리 비용은 슬롯 수에 비례하지만 메시지 바이트
/// 수에 비례하지 않는다. 128 → 256 으로 키운 것은 모바일 reattach 직후 PTY burst
/// 가 한 번에 밀려 들어오는 동안 consumer 가 따라잡을 시간 버퍼를 확보하기 위함
/// (PR #13 의 zombie-attach guard 가 트랜지언트 jitter 를 false-positive 로 잡는
/// 마진을 줄이는 두 번째 변).
const SUBSCRIBER_QUEUE_LIMIT: usize = 256;
/// PR #16: `append_output` 의 backpressure 2-pass 회복 윈도우. 1-pass try_send 가
/// `Full` 을 반환한 sub 에 대해 본 시간 동안 `send_timeout` 으로 한 번 더 시도하고,
/// 그래도 자리가 나지 않으면 그제서야 evict 한다 — PR #13 의 zombie-attach 차단
/// 보장은 유지하면서, 모바일 SSH 의 50–200ms 딸꾹질이 attach 를 끊지 않도록 한다.
/// PR #16 fold-in 이후 최악 시 본 함수의 대기 시간은 pending 수 K 와 무관하게
/// `BACKPRESSURE_SEND_TIMEOUT` 하나로 묶인다.
const BACKPRESSURE_SEND_TIMEOUT: Duration = Duration::from_millis(100);
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_SENSITIVE_CAPABILITY_FRAME_BYTES: usize = 128 * 1024;
const MAX_INPUT_CAPABILITIES: usize = 1024;
const MAX_INPUT_CAPABILITIES_PER_SESSION: usize = 64;

type OutputChunk = Arc<[u8]>;
type BackpressureHook = Arc<dyn Fn() + Send + Sync>;

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
///
/// PR #15: per-client geometry 추적을 위해 `rows`/`cols` 를 들고 있는다. 이
/// 두 필드는 Attach 시점의 초기값이 박히고, 이후 `Request::Resize` 에
/// `subscriber_id == Some(id)` 가 들어올 때만 갱신된다. 모든 attach 의
/// `min(rows)`, `min(cols)` 가 PTY winsize 가 되어, 가장 좁은 클라이언트 기준
/// 으로 clamp 된다 (PR #14 의 client-side first-attach guard 를 대체하는
/// canonical 정책 — 모바일 detach 시 desktop 사이즈로 자동 회복).
#[derive(Clone)]
struct Subscriber {
    id: u64,
    tx: SyncSender<OutputChunk>,
    on_evict: Arc<dyn Fn() + Send + Sync>,
    /// 이 attach client 가 본다고 보고한 PTY rows (status row 차감 후).
    rows: u16,
    /// 이 attach client 가 본다고 보고한 PTY cols.
    cols: u16,
}

pub fn serve_forever() -> Result<()> {
    let socket = paths::socket_path()?;
    prepare_socket_path(&socket)?;
    let listener = bind_private_socket(&socket)?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("chmod 0600 {}", socket.display()))?;
    if let Err(err) = paths::record_default_socket_path(&socket) {
        eprintln!(
            "failed to record active lterm socket {}: {err:#}",
            socket.display()
        );
    }
    eprintln!("lterm daemon listening on {}", socket.display());

    // 데몬 시작 시각. SystemTime이 UNIX_EPOCH 이전이면 None을 그대로 들고 가서
    // doctor 측 uptime 계산이 sentinel 0으로 50+년을 보고하지 않게 한다 (quad-review
    // 합의 이슈: started_at_unix_secs=0 wire 전송 회피).
    let started_at_unix_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs());
    let state = Arc::new(State::new(started_at_unix_secs));
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
    /// Canonical multi-lock order is `sessions -> input_capabilities`.
    input_capabilities: Mutex<InputCapabilityRegistry>,
    shutting_down: AtomicBool,
    active_connections: AtomicUsize,
    active_blocking_waits: AtomicUsize,
    // 데몬 시작 시각(UNIX epoch seconds). doctor의 uptime 계산용. None이면
    // SystemTime::now()가 시스템 clock 이슈로 실패한 경우. wire에서 그대로 None을
    // 보내 client가 uptime을 omit하게 한다.
    started_at_unix_secs: Option<u64>,
}

#[derive(Default)]
struct InputCapabilityRegistry {
    grants: HashMap<CapabilityToken, InputCapabilityGrant>,
}

struct InputCapabilityGrant {
    session_id: String,
    session: Weak<Session>,
    remaining_attempt_bytes: u64,
}

impl State {
    // Production 데몬 진입 경로용 명시 생성자. session_count/connection_count 같은
    // 카운터 계열은 Default::default()로 0에서 시작해도 항상 안전하므로 wire에
    // 의존하는 시작 시각만 인자로 받는다. 테스트는 State::default()를 그대로 사용.
    fn new(started_at_unix_secs: Option<u64>) -> Self {
        Self {
            started_at_unix_secs,
            ..Self::default()
        }
    }

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

    fn try_acquire_blocking_wait(self: &Arc<Self>) -> Option<BlockingWaitGuard> {
        let mut current = self.active_blocking_waits.load(Ordering::SeqCst);
        loop {
            if current >= MAX_BLOCKING_WAITS {
                return None;
            }
            match self.active_blocking_waits.compare_exchange(
                current,
                current + 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => {
                    return Some(BlockingWaitGuard {
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

struct BlockingWaitGuard {
    state: Arc<State>,
}

impl Drop for BlockingWaitGuard {
    fn drop(&mut self) {
        self.state
            .active_blocking_waits
            .fetch_sub(1, Ordering::SeqCst);
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

#[derive(Clone, Copy, Debug, Default)]
struct OutputProgress {
    revision: u64,
    closed: bool,
    total_bytes: u64,
}

struct OutputClosedGuard {
    session: Arc<Session>,
}

impl OutputClosedGuard {
    fn new(session: Arc<Session>) -> Self {
        Self { session }
    }
}

impl Drop for OutputClosedGuard {
    fn drop(&mut self) {
        self.session.close_subscribers();
        self.session.mark_output_closed();
    }
}

struct Session {
    id: String,
    // Canonical lock order is `State.sessions -> Session.metadata`. The
    // unified lock keeps current metadata, journal cursor, entries, and purge
    // evidence coherent. Never call `Session::info()` while holding it.
    metadata: Mutex<SessionMetadata>,
    pane_id: String,
    parent_pane_id: Mutex<Option<String>>,
    parent_session_id: Mutex<Option<String>>,
    parent_token: String,
    command: String,
    cwd: String,
    created_unix_ms: u128,
    process_id: Option<u32>,
    process_group_id: Option<i32>,
    agent_name: Option<String>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    ring: Mutex<VecDeque<u8>>,
    /// PR #17: attach snapshot 은 raw 2 MiB ring replay 가 아니라 이 in-memory
    /// terminal state 에서 합성한 현재 화면 frame 을 보낸다. raw ring 은 capture 와
    /// 라인 기반 조회를 위해 그대로 유지하지만, attach 초기 출력에는 오래된 scrollback
    /// / 이미 지워진 escape history 를 다시 먹이지 않는다.
    terminal_screen: Mutex<vt100::Parser>,
    /// `terminal_screen` 이 아직 완성되지 않은 control sequence 를 들고 있는 경우,
    /// 새 attach snapshot 끝에 같은 prefix 를 붙여 이후 live raw suffix 가 기존 PTY
    /// parser 상태와 같은 방식으로 해석되게 한다. 이 값은 snapshot 정확도 계약의
    /// 일부이므로 `terminal_screen` / `terminal_normal_screen` 과 함께
    /// `output_state` 아래에서 갱신·조회한다.
    terminal_pending: Mutex<TerminalPrefixTracker>,
    /// active alt-screen snapshot 이 나중에 `1049l` 을 받았을 때 session 의 normal
    /// buffer 로 돌아갈 수 있도록, alt-screen 진입/종료 경계에서 관찰한 normal-screen
    /// frame 을 보관한다. 일반 normal-screen attach snapshot 은 이 cache 가 아니라
    /// `terminal_screen` 의 현재 parser state 에서 직접 합성하므로, hot path 는 평상시
    /// normal output 마다 screen clone 을 만들지 않는다.
    terminal_normal_screen: Mutex<vt100::Screen>,
    /// `vt100` terminal-screen state is an attach-snapshot convenience, not the
    /// availability-critical raw PTY path. If parser or snapshot code panics,
    /// this flag quarantines the session-local emulator state so the PTY reader
    /// keeps appending raw bytes, broadcasting live output, and accepting input.
    terminal_parser_degraded: AtomicBool,
    #[cfg(test)]
    terminal_parser_panic_on_next_update: AtomicBool,
    #[cfg(test)]
    terminal_parser_panic_on_next_snapshot: AtomicBool,
    #[cfg(test)]
    terminal_parser_panic_on_next_resize: AtomicBool,
    subscribers: Mutex<Vec<Subscriber>>,
    /// Coarse state mutex for operations that must observe a coherent output
    /// image. `append_output` holds it while appending to the raw ring,
    /// updating `terminal_screen`, `terminal_pending`, and
    /// `terminal_normal_screen`, then cloning the subscriber list for the live
    /// chunk. Attach snapshot paths hold the same mutex while reading those
    /// terminal fields and queueing the initial snapshot. Resize paths hold it
    /// while resizing the PTY and parser state.
    ///
    /// Individual fields still keep their own mutexes for narrow readers such
    /// as capture. Per-subscriber rows/cols edits are also allowed under
    /// `geometry_apply > subscribers` alone because they do not combine with
    /// terminal parser state. Paths that combine terminal parser state,
    /// pending escape bytes, subscriber list snapshots or add/remove
    /// membership, or PTY resize must go through this guard to preserve the
    /// snapshot/live-output order contract.
    output_state: Mutex<()>,
    output_progress: (Mutex<OutputProgress>, Condvar),
    #[cfg(test)]
    backpressure_hook: Mutex<Option<BackpressureHook>>,
    /// Serializes live chunk enqueue order. `append_output` takes this before
    /// `output_state`, so later appenders wait outside the snapshot/resize
    /// state mutex while an earlier chunk is in the subscriber backpressure
    /// retry window.
    broadcast_order: Mutex<()>,
    /// PR #15 quad-review HIGH 후속: per-client geometry 갱신 → clamp 결정 →
    /// `master.resize` → 세션 cached `rows`/`cols` 갱신 의 4단계가 다른 attach 의
    /// subscribe/unsubscribe/Resize 와 인터리빙되지 않도록 보호하는 단일 직렬화 락.
    ///
    /// 보호 대상 시나리오 (Codex/Forge/Claude 합의의 race):
    /// - narrow client A 가 Resize(24, 80) 을 보내 per-client geometry 를 갱신함.
    /// - 같은 순간 다른 스레드가 narrow A 를 unsubscribe → re-clamp 가 wide 한
    ///   잔여 client 사이즈로 PTY 를 키움.
    /// - 그 직후 처음 Resize 의 apply 가 깨어나 PTY 를 다시 narrow 사이즈로 줄임.
    ///
    /// 결과적으로 살아있는 attach 가 wide 한데 PTY 가 narrow 인 stale 상태가 된다.
    ///
    /// Canonical lock order:
    /// - attach/resize/detach geometry paths enter through `geometry_apply`
    ///   first;
    /// - per-subscriber geometry edits may then take `subscribers` directly
    ///   only to update that subscriber's rows/cols;
    /// - clamp/apply-resize work takes `output_state`, reads `subscribers` to
    ///   compute the clamp target, releases `subscribers`, then touches
    ///   `master`, `rows`/`cols`, and `terminal_screen`;
    /// - `append_output` takes `broadcast_order > output_state`, updates
    ///   ring/parser/`terminal_pending`/normal-screen state, snapshots
    ///   `subscribers`, then drops `output_state` before slow backpressure
    ///   waits.
    /// - `broadcast_order` and `geometry_apply` are disjoint entry locks; no
    ///   path should hold both without defining a new order here first.
    ///
    /// 즉 geometry 변경/clamp/resize 경로의 임의 함수는 본 락을 가장 먼저 잡아야
    /// 하며, PTY resize 와 parser resize 는 output broadcast/snapshot 과 직렬화한다.
    geometry_apply: Mutex<()>,
    next_subscriber_id: AtomicU64,
    /// Whether the leader process is still considered live for user-visible
    /// session state and attach input loops.
    alive: AtomicBool,
    /// One-shot finalizer gate shared by the leader waiter and explicit
    /// kill/shutdown paths. `alive` can become false before teardown finishes,
    /// so cleanup needs its own idempotence and completion state.
    cleanup_started: AtomicBool,
    cleanup_completion: (Mutex<bool>, Condvar),
    cleanup_complete: AtomicBool,
    /// Set after `waitid(..., WNOWAIT)` observes leader exit but before the
    /// waiter reaps it. During this window the stored pgid is still anchored by
    /// the unreaped leader pid and cannot be reused by an unrelated process.
    leader_exit_observed: AtomicBool,
    /// Set immediately after the leader wait returns. A concurrent explicit
    /// kill must not rely on the reaped leader pid to verify the process group.
    leader_reaped: AtomicBool,
    /// One-shot gate for the unreaped-leader residual process-group cleanup.
    /// Both explicit terminate and the waiter can discover the same unreaped
    /// leader; only one should send the short residual signal ladder.
    unreaped_cleanup_started: AtomicBool,
    exit_code: AtomicI32,
    rows: Mutex<u16>,
    cols: Mutex<u16>,
}

#[derive(Clone)]
struct SessionMetadata {
    current: MetadataValue,
    entries: Vec<MetadataJournalEntry>,
    cursor: usize,
    purge: MetadataPurgeAggregate,
}

impl SessionMetadata {
    fn new(name: String, status_theme: Option<StatusTheme>) -> Self {
        Self {
            current: MetadataValue { name, status_theme },
            entries: Vec::new(),
            cursor: 0,
            purge: MetadataPurgeAggregate {
                generation: 0,
                purged_entries_total: 0,
                last_purged_unix_ms: None,
            },
        }
    }
}

impl Session {
    fn name(&self) -> String {
        lock(&self.metadata).current.name.clone()
    }

    fn info(&self) -> SessionInfo {
        let exit = self.exit_code.load(Ordering::SeqCst);
        let metadata = lock(&self.metadata).current.clone();
        SessionInfo {
            id: self.id.clone(),
            name: metadata.name,
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
            status_theme: metadata.status_theme,
            agent_name: self.agent_name.clone(),
        }
    }

    fn instrument_snapshot_relaxed(&self) -> InstrumentSnapshot {
        let progress = *lock(&self.output_progress.0);
        let alive = self.alive.load(Ordering::SeqCst);
        let attached_clients = lock(&self.subscribers).len();
        let rows = *lock(&self.rows);
        let cols = *lock(&self.cols);
        InstrumentSnapshot {
            schema_version: "1.0".to_string(),
            observed_unix_ms: u64::try_from(now_unix_ms()).unwrap_or(u64::MAX),
            session_id: self.id.clone(),
            pane_id: self.pane_id.clone(),
            alive,
            output_closed: progress.closed,
            output_revision: progress.revision,
            output_total_bytes: progress.total_bytes,
            attached_clients,
            rows,
            cols,
        }
    }

    fn append_output(&self, bytes: &[u8]) {
        let broadcast_guard = lock(&self.broadcast_order);
        let (subscribers, chunk) = {
            let _output_guard = lock(&self.output_state);
            {
                let mut ring = lock(&self.ring);
                append_ring_bytes(&mut ring, bytes);
            }
            // Keep the raw ring, vt100 parser, pending escape-prefix tracker,
            // normal-screen fallback, and subscriber snapshot under one
            // `output_state` critical section. Otherwise a new attach could
            // synthesize a screen snapshot from parser state that does not
            // match the pending bytes that will prefix the next live chunk.
            // The section ends before `broadcast_chunk`, so slow subscriber
            // sends/backpressure waits do not block attach snapshots or resize.
            self.update_terminal_snapshot_state(bytes);
            self.mark_output_changed(bytes.len());

            let subscribers = lock(&self.subscribers).clone();
            if subscribers.is_empty() {
                return;
            }
            (subscribers, Arc::from(bytes))
        };
        #[cfg(test)]
        let backpressure_hook = lock(&self.backpressure_hook).clone();
        #[cfg(not(test))]
        let backpressure_hook: Option<BackpressureHook> = None;
        let disconnected = broadcast_chunk(
            &subscribers,
            chunk,
            BACKPRESSURE_SEND_TIMEOUT,
            backpressure_hook.as_deref(),
        );
        let shutdowns = if disconnected.is_empty() {
            Vec::new()
        } else {
            // The slow retry window above intentionally runs without
            // `output_state`, but the actual subscriber mutation must still
            // serialize with attach/resize clamp paths that compute geometry
            // from the current subscriber set under the same state mutex.
            let _output_guard = lock(&self.output_state);
            // RC-2b: backpressure로 sub를 evict할 때 attach UnixStream 도 같이 닫아
            // input loop를 EOF로 깨운다. 그렇지 않으면 output 만 끊기고 input은
            // 살아있는 zombie attach 가 만들어져 사용자가 frozen으로 인지하는 동안
            // keystroke 가 PTY로 흘러들어가 위험한 명령이 실행될 수 있다.
            //
            // shutdown 훅은 subscribers lock 을 해제한 뒤 호출한다. handle_attach 측
            // input loop 가 EOF로 깨어나면서 unsubscribe 경로로 들어오면 같은 lock을
            // 다시 잡으려 시도하므로, lock 안에서 호출하면 deadlock 위험이 있다.
            {
                let mut subscribers = lock(&self.subscribers);
                evict_disconnected_subscribers(&mut subscribers, &disconnected)
            }
        };
        drop(broadcast_guard);
        for shutdown in shutdowns {
            shutdown();
        }
    }

    fn update_terminal_snapshot_state(&self, bytes: &[u8]) {
        if self.terminal_parser_degraded() {
            return;
        }
        if internal_test_degrade_terminal_parser() {
            self.mark_terminal_parser_degraded("internal test requested parser degradation");
            self.clear_terminal_pending_prefix();
            return;
        }
        let result = catch_unwind(AssertUnwindSafe(|| {
            let pending_before = lock(&self.terminal_pending).pending_bytes();
            let normal_screen = self.process_terminal_screen(bytes, &pending_before);
            if let Some(normal_screen) = normal_screen {
                *lock(&self.terminal_normal_screen) = normal_screen;
            }
            lock(&self.terminal_pending).process(bytes);
        }));
        if result.is_err() {
            self.mark_terminal_parser_degraded("terminal parser panicked while processing output");
            self.clear_terminal_pending_prefix();
        }
    }

    fn terminal_parser_degraded(&self) -> bool {
        self.terminal_parser_degraded.load(Ordering::SeqCst)
    }

    fn mark_terminal_parser_degraded(&self, reason: &'static str) {
        if !self.terminal_parser_degraded.swap(true, Ordering::SeqCst) {
            eprintln!(
                "terminal parser degraded for pane {}: {reason}; raw PTY output will continue without screen-state snapshots",
                self.pane_id
            );
        }
    }

    fn clear_terminal_pending_prefix(&self) {
        *lock(&self.terminal_pending) = TerminalPrefixTracker::default();
    }

    fn mark_output_changed(&self, appended_bytes: usize) {
        let (progress, changed) = &self.output_progress;
        let mut progress = lock(progress);
        progress.total_bytes = progress.total_bytes.saturating_add(appended_bytes as u64);
        progress.revision = progress.revision.wrapping_add(1);
        changed.notify_all();
    }

    fn mark_output_closed(&self) {
        let (progress, changed) = &self.output_progress;
        let mut progress = lock(progress);
        progress.closed = true;
        progress.revision = progress.revision.wrapping_add(1);
        changed.notify_all();
    }

    fn capture(&self, start: Option<i32>, end: Option<i32>) -> String {
        sanitize::terminal_capture(&self.capture_bytes(start, end))
    }

    fn capture_bytes(&self, start: Option<i32>, end: Option<i32>) -> Vec<u8> {
        let ring = lock(&self.ring);
        if start.is_none() && end.is_none() {
            return ring.iter().copied().collect();
        }
        capture_bytes_from_ring(&ring, start, end)
    }

    /// 새 attach client 를 등록한다. 초기 geometry (`rows`, `cols`) 는 attach 요청에
    /// 실려 들어와 Subscriber 에 박힌다 — clamp-to-smallest 정책에 즉시 반영하기
    /// 위함. 0×0 같은 degenerate 사이즈는 여기서 fail-fast 로 막아 invariant 를
    /// 경계에서 강제한다 (Codex PR #14 review MEDIUM 의 후속 조치).
    ///
    /// PTY 사이즈 재계산은 호출자가 `apply_clamped_pty_size` 를 별도로 호출해야
    /// 한다 — 본 함수는 `output_state` lock 을 잡고 있고, master.resize 와
    /// subscribers lock 을 같이 잡는 경로와 lock 순서가 꼬이는 것을 피하기 위함.
    ///
    /// PR #15 quad-review 후속(#4): 0×0 은 보통 `#[serde(default)]` 로 인한
    /// 구버전 클라이언트 페이로드(예: lterm 재빌드 누락) 가 원인이므로, 사용자가
    /// 다음 행동을 짐작할 수 있도록 친절한 메시지로 surface 한다.
    ///
    /// PR #16: attach snapshot 을 별도 동기 write 로 보내지 않고 broadcast 채널의 첫
    /// chunk 로 직접 푸시한다. caller 의 output 스레드는 이 chunk 를 그대로 받아
    /// 라이브 chunk 와 동일한 순서로 출력한다 — snapshot 동기 write 가 늦어지는 사이
    /// 라이브 채널이 차서 false-positive eviction 이 일어나던 race 를 닫는다.
    ///
    /// PR #17: 그 첫 chunk 의 내용은 더 이상 raw ring dump 가 아니다. raw ring 은
    /// capture 용으로 유지하되, attach replay 는 `terminal_screen` 이 합성한 현재
    /// visible state (`vt100::Screen::state_formatted`) 만 보낸다. 따라서 이미 지운
    /// 과거 output, scrollback, 잘린 escape sequence 를 새 attach terminal 에 다시
    /// 주입하지 않는다. snapshot push 는 `output_state` 가드를 들고 있는 동안 실행되므로
    /// 등록 직후의 `append_output` 이 같은 가드를 기다리며 직렬화되어 라이브 chunk 가
    /// snapshot 보다 먼저 큐에 들어가지 못한다.
    #[cfg(test)]
    fn subscribe_with_snapshot(
        &self,
        rows: u16,
        cols: u16,
        on_evict: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<(u64, Receiver<OutputChunk>)> {
        if rows == 0 || cols == 0 {
            bail!(
                "attach: client did not supply rows/cols (likely a version mismatch — please rebuild lterm)"
            );
        }
        validate_terminal_geometry("attach", rows, cols)?;
        let _output_guard = lock(&self.output_state);
        let has_output = {
            let ring = lock(&self.ring);
            !ring.is_empty()
        };
        // 빈 ring 일 때는 None 으로 두어 의미 없는 clear/reset snapshot 을 채널에
        // 넣지 않는다. 한 번이라도 PTY output 이 있었던 세션만 현재 screen state 를
        // 합성해 첫 chunk 로 보낸다. pending escape prefix 도 같은 `output_state`
        // 아래에서 읽어 parser state 와 live raw suffix 의 경계가 어긋나지 않게 한다.
        let initial_chunk: Option<OutputChunk> = if has_output {
            let (snapshot_rows, snapshot_cols) = {
                let subscribers = lock(&self.subscribers);
                clamp_to_smallest_with_candidate(&subscribers, rows, cols)
            };
            self.initial_screen_state_snapshot(snapshot_rows, snapshot_cols)
        } else {
            None
        };
        self.subscribe_locked(rows, cols, on_evict, initial_chunk)
    }

    /// 실제 attach 경로용 subscribe+clamp 단일 critical section.
    ///
    /// `subscribe_with_snapshot` 만 호출한 뒤 별도로 clamp resize 를 적용하면, 그 두
    /// 단계 사이에 `append_output` 이 `output_state` 를 잡아 새 subscriber 큐에 old PTY
    /// geometry 기준 라이브 bytes 를 넣을 수 있다. handle_attach 는 이 helper 를 통해
    /// `geometry_apply > output_state` 를 유지한 채 snapshot enqueue 와 PTY/parser resize
    /// 를 끝낸 뒤 라이브 output 을 다시 허용한다.
    fn subscribe_with_snapshot_and_apply_clamp(
        &self,
        rows: u16,
        cols: u16,
        on_evict: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<(u64, Receiver<OutputChunk>)> {
        if rows == 0 || cols == 0 {
            bail!(
                "attach: client did not supply rows/cols (likely a version mismatch — please rebuild lterm)"
            );
        }
        validate_terminal_geometry("attach", rows, cols)?;
        let geometry_guard = lock(&self.geometry_apply);
        let _output_guard = lock(&self.output_state);
        let has_output = {
            let ring = lock(&self.ring);
            !ring.is_empty()
        };
        let initial_chunk: Option<OutputChunk> = if has_output {
            let (snapshot_rows, snapshot_cols) = {
                let subscribers = lock(&self.subscribers);
                clamp_to_smallest_with_candidate(&subscribers, rows, cols)
            };
            self.initial_screen_state_snapshot(snapshot_rows, snapshot_cols)
        } else {
            None
        };
        let (subscriber_id, rx) = self.subscribe_locked(rows, cols, on_evict, initial_chunk)?;
        if let Err(err) = self.apply_clamped_pty_size_under_output_guard(&geometry_guard) {
            lock(&self.subscribers).retain(|sub| sub.id != subscriber_id);
            return Err(err);
        }
        Ok((subscriber_id, rx))
    }

    /// 새 subscriber 를 등록하고 `(id, rx)` 를 반환한다. `initial_chunk` 가 `Some` 이면
    /// 등록 직후 그 chunk 를 broadcast 채널에 푸시한다 — PR #16/17 의 snapshot 첫
    /// chunk 시나리오. 푸시는 본 함수가 `subscribers` lock 을 들고 있는 동안 실행되어
    /// 다른 broadcast 가 끼어들 여지를 차단한다.
    ///
    /// **순서 invariant 의 진짜 책임**: lock 자체보다 호출자의 `output_state` 가드가
    /// 핵심이다. `append_output` 은 `broadcast_order` 를 잡은 뒤 `output_state` 아래에서
    /// (a) ring/parser 갱신, (b) subscribers 스냅샷 복사를 끝내고 backpressure 대기
    /// 구간으로 넘어간다. 따라서 호출자는 `initial_chunk` 가 `Some` 일 때 본 함수 진입
    /// 전에 `output_state` 를 잡고 있어야 라이브 chunk 가 snapshot 앞에 끼어들지
    /// 못한다 (PR #16 의 race fix). 호출자가 그 가드를 들지 않으면 라이브 chunk 가
    /// snapshot 보다 먼저 큐에 들어갈 수 있다.
    ///
    /// **PR #16 quad-review HIGH 후속 (Codex/Claude/Gemini)**: 이전 시그니처는
    /// `(id, tx_clone, rx)` 를 반환하고 caller 가 자체적으로 push 를 했다. 그런데
    /// `subscribers` lock 은 함수 종료 시점에 이미 풀려 있으므로 도큐멘테이션이 약속
    /// 한 "push-before-unlock" invariant 가 사실은 성립하지 않았다 — 진짜로 보장하던
    /// 것은 호출자의 `output_state` 가드였다. 향후 `output_state` 없이 호출하는 caller
    /// 가 추가되면 침묵하며 순서가 깨지므로, 본 PR 에서는 push 자체를 본 함수 안으로
    /// 옮겨 호출자가 잘못 쓰는 경로를 줄였다.
    ///
    /// **Push 실패 시 rollback (Codex/Claude HIGH)**: 갓 등록한 sub 의 채널은
    /// `SUBSCRIBER_QUEUE_LIMIT` 만큼 비어 있어 단일 chunk 의 `try_send` 는 정상
    /// 코드패스에서 항상 성공한다. 그래도 invariant 가 깨질 가능성 (예: 채널 capacity
    /// 가 0 이라거나, mpsc 구현 버그로 즉시 disconnected 가 되는 등) 을 panic 이 아닌
    /// rollback + bail 로 처리한다 — 방금 push 한 Subscriber 를 같은 lock 안에서
    /// `retain` 으로 제거해 ghost 가 남지 않게 하고 caller 에 에러를 돌려준다.
    fn subscribe_locked(
        &self,
        rows: u16,
        cols: u16,
        on_evict: Arc<dyn Fn() + Send + Sync>,
        initial_chunk: Option<OutputChunk>,
    ) -> Result<(u64, Receiver<OutputChunk>)> {
        let id = self.next_subscriber_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::sync_channel(SUBSCRIBER_QUEUE_LIMIT);
        let mut subscribers = lock(&self.subscribers);
        if subscribers.len() >= MAX_SUBSCRIBERS_PER_SESSION {
            bail!("too many attached subscribers for session {}", self.name());
        }
        subscribers.push(Subscriber {
            id,
            tx: tx.clone(),
            on_evict,
            rows,
            cols,
        });
        if let Some(chunk) = initial_chunk {
            if let Err(err) = tx.try_send(chunk) {
                // 정상 코드패스에서는 도달 불가 — 그래도 panic 대신 push 한 sub 를
                // 즉시 같은 lock 안에서 빼낸다. caller 는 에러를 받고 attach 흐름을
                // 중단해 ghost subscriber 가 남지 않게 한다.
                subscribers.retain(|s| s.id != id);
                bail!("snapshot push failed for subscriber {}: {:?}", id, err);
            }
        }
        Ok((id, rx))
    }

    fn unsubscribe(&self, subscriber_id: u64) {
        // PR #15 quad-review HIGH 후속(#1): geometry_apply 락을 가장 먼저 잡아
        // "subscriber 제거 → clamp 재계산 → PTY resize" 4단계 동안 다른 attach 의
        // Resize/subscribe/unsubscribe 가 끼어들지 못하게 한다.
        let geometry_guard = lock(&self.geometry_apply);
        {
            let _output_guard = lock(&self.output_state);
            lock(&self.subscribers).retain(|sub| sub.id != subscriber_id);
        }
        // detach 후에는 살아있는 클라이언트 들의 min 으로 PTY 사이즈를 회복시킨다 —
        // 좁은 mobile 이 떠나면 wide desktop 사이즈로 다시 자라야 하는 핵심 시나리오.
        // 잔여 subscriber 가 0 이면 helper 가 no-op 으로 떨어져 PTY 사이즈는 마지막
        // 값에 그대로 남는다 (다음 attach 가 도착하면 그 시점에 다시 clamp).
        let _ = self.apply_clamped_pty_size(&geometry_guard);
    }

    fn close_subscribers(&self) {
        let _output_guard = lock(&self.output_state);
        lock(&self.subscribers).clear();
    }

    /// 살아있는 모든 attach client 의 geometry 로 PTY 사이즈를 재계산한다 (PR #15
    /// canonical clamp-to-smallest 정책). subscriber 가 한 명도 없으면 PTY 사이즈는
    /// 그대로 두어 다음 attach 가 새 정책을 결정하도록 한다.
    ///
    /// **`geometry_apply` 락 보유 요구**: 호출자는 본 함수에 진입하기 전에 반드시
    /// `session.geometry_apply` 의 `MutexGuard` 를 들고 있어야 한다. 이 가드는
    /// "per-client geometry 갱신 → clamp 결정 → master.resize → cached rows/cols 갱신"
    /// 전 구간을 단일 critical section 으로 묶어, 다른 attach 의 Resize/unsubscribe
    /// 가 중간에 끼어들어 PTY 사이즈가 살아있는 attach 와 어긋나는 결과를 막는다
    /// (PR #15 quad-review HIGH: Codex+Forge+Claude 의 race 시나리오).
    /// 가드를 컴파일타임에 강제하기 위해 인자로 `&MutexGuard<()>` 를 받는다.
    ///
    /// **하위 lock 순서**: public helper 는 먼저 `output_state` 를 잡고, 그 아래에서
    /// subscribers lock 을 짧게 잡아 min 만 계산한 뒤 즉시 해제한다. 이후 같은
    /// `output_state` 가드 아래에서 master resize, cached rows/cols 갱신, parser
    /// resize 를 적용한다. 즉 subscribers lock 과 master/parser lock 은 동시에 잡지
    /// 않지만, 전체 clamp/apply 구간은 `append_output` 과 같은 `output_state` 로
    /// 직렬화된다. lock-order 표기:
    /// `geometry_apply > output_state > subscribers`,
    /// `geometry_apply > output_state > master`,
    /// `geometry_apply > output_state > terminal_screen`.
    ///
    /// **`output_state` 와의 관계**: 이 public helper 는 `output_state` 가 잡혀
    /// 있지 **않은** 상태에서 호출되어야 한다. 이미 `output_state` 를 들고 있는
    /// attach path 는 `apply_clamped_pty_size_under_output_guard` 를 호출해야 하며,
    /// `subscribe_with_snapshot_and_apply_clamp` 가 그 예다.
    fn apply_clamped_pty_size(&self, _geometry_guard: &MutexGuard<'_, ()>) -> Result<()> {
        let _output_guard = lock(&self.output_state);
        self.apply_clamped_pty_size_under_output_guard(_geometry_guard)
    }

    fn apply_clamped_pty_size_under_output_guard(
        &self,
        _geometry_guard: &MutexGuard<'_, ()>,
    ) -> Result<()> {
        let target = {
            let subscribers = lock(&self.subscribers);
            clamp_to_smallest(&subscribers)
        };
        let Some((rows, cols)) = target else {
            return Ok(());
        };
        let current = (*lock(&self.rows), *lock(&self.cols));
        if current == (rows, cols) {
            return Ok(());
        }
        self.apply_pty_size_under_output_guard(
            rows,
            cols,
            "resize pty to clamped subscriber geometry",
        )?;
        Ok(())
    }

    fn apply_pty_size(&self, rows: u16, cols: u16, context: &'static str) -> Result<()> {
        let _output_guard = lock(&self.output_state);
        self.apply_pty_size_under_output_guard(rows, cols, context)
    }

    fn apply_pty_size_under_output_guard(
        &self,
        rows: u16,
        cols: u16,
        context: &'static str,
    ) -> Result<()> {
        lock(&self.master)
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context(context)?;
        *lock(&self.rows) = rows;
        *lock(&self.cols) = cols;
        self.resize_terminal_screen(rows, cols);
        Ok(())
    }

    fn resize_terminal_screen(&self, rows: u16, cols: u16) {
        if self.terminal_parser_degraded() {
            return;
        }
        let result = catch_unwind(AssertUnwindSafe(|| {
            let mut terminal_screen = lock(&self.terminal_screen);
            #[cfg(test)]
            if self
                .terminal_parser_panic_on_next_resize
                .swap(false, Ordering::SeqCst)
            {
                panic!("test-requested terminal parser resize panic after locking parser");
            }
            terminal_screen.screen_mut().set_size(rows, cols);
        }));
        if result.is_err() {
            self.mark_terminal_parser_degraded("terminal parser panicked while resizing");
            self.clear_terminal_pending_prefix();
        }
    }

    fn initial_screen_state_snapshot(&self, rows: u16, cols: u16) -> Option<OutputChunk> {
        if self.terminal_parser_degraded() {
            return None;
        }
        let result = catch_unwind(AssertUnwindSafe(|| {
            let terminal_screen = lock(&self.terminal_screen);
            #[cfg(test)]
            if self
                .terminal_parser_panic_on_next_snapshot
                .swap(false, Ordering::SeqCst)
            {
                panic!("test-requested terminal snapshot panic after locking parser");
            }
            let normal_screen = lock(&self.terminal_normal_screen);
            let pending = lock(&self.terminal_pending).pending_bytes();
            screen_state_snapshot(&terminal_screen, &normal_screen, rows, cols, &pending)
        }));
        match result {
            Ok(chunk) => chunk,
            Err(_) => {
                self.mark_terminal_parser_degraded(
                    "terminal parser panicked while formatting attach snapshot",
                );
                self.clear_terminal_pending_prefix();
                None
            }
        }
    }

    fn process_terminal_screen(
        &self,
        bytes: &[u8],
        pending_control_prefix: &[u8],
    ) -> Option<vt100::Screen> {
        let mut terminal_screen = lock(&self.terminal_screen);
        #[cfg(test)]
        if self
            .terminal_parser_panic_on_next_update
            .swap(false, Ordering::SeqCst)
        {
            panic!("test-requested terminal parser update panic after locking parser");
        }
        let started_alt = terminal_screen.screen().alternate_screen();

        if !started_alt && should_scan_for_alt_enter(bytes, pending_control_prefix) {
            let mut detector = AltEnterDetector::from_pending_prefix(pending_control_prefix);
            let mut normal_before_alt = None;
            for byte in bytes {
                let was_alt = terminal_screen.screen().alternate_screen();
                let before_alt_enter = if !was_alt && *byte == b'h' && detector.is_alt_enter_csi() {
                    Some(terminal_screen.screen().clone())
                } else {
                    None
                };
                terminal_screen.process(std::slice::from_ref(byte));
                let is_alt = terminal_screen.screen().alternate_screen();
                if !was_alt && is_alt {
                    normal_before_alt = before_alt_enter;
                }
                detector.process(*byte);
            }
            if terminal_screen.screen().alternate_screen() {
                normal_before_alt
            } else {
                // Still in the normal screen. New attach snapshots use
                // `terminal_screen` directly in this state, so avoid cloning the
                // full screen just to refresh the alt-screen fallback cache.
                None
            }
        } else {
            terminal_screen.process(bytes);
            if started_alt && !terminal_screen.screen().alternate_screen() {
                Some(terminal_screen.screen().clone())
            } else {
                None
            }
        }
    }
}

fn append_ring_bytes(ring: &mut VecDeque<u8>, bytes: &[u8]) {
    if bytes.len() >= RING_LIMIT {
        ring.clear();
        ring.extend(bytes[bytes.len() - RING_LIMIT..].iter().copied());
        return;
    }

    let overflow = ring
        .len()
        .saturating_add(bytes.len())
        .saturating_sub(RING_LIMIT);
    if overflow > 0 {
        ring.drain(..overflow);
    }
    ring.extend(bytes.iter().copied());
}

fn should_scan_for_alt_enter(bytes: &[u8], pending_control_prefix: &[u8]) -> bool {
    bytes.contains(&b'h')
        && (bytes.contains(&0x1b) || bytes.contains(&0x9b) || !pending_control_prefix.is_empty())
}

enum AltEnterDetector {
    Ground,
    Escape,
    Csi(Vec<u8>),
}

impl AltEnterDetector {
    fn from_pending_prefix(pending_control_prefix: &[u8]) -> Self {
        if pending_control_prefix.starts_with(&[0x9b]) {
            return Self::Csi(pending_control_prefix[1..].to_vec());
        }
        if pending_control_prefix.starts_with(b"\x1b[") {
            return Self::Csi(pending_control_prefix[2..].to_vec());
        }
        if pending_control_prefix == [0x1b] {
            return Self::Escape;
        }
        Self::Ground
    }

    fn is_alt_enter_csi(&self) -> bool {
        let Self::Csi(params) = self else {
            return false;
        };
        let Some(private_modes) = params.strip_prefix(b"?") else {
            return false;
        };
        private_modes
            .split(|byte| *byte == b';')
            .any(|param| matches!(param, b"47" | b"1047" | b"1049"))
    }

    fn process(&mut self, byte: u8) {
        match self {
            Self::Ground => match byte {
                0x1b => *self = Self::Escape,
                0x9b => *self = Self::Csi(Vec::new()),
                _ => {}
            },
            Self::Escape => match byte {
                b'[' => *self = Self::Csi(Vec::new()),
                0x1b => *self = Self::Escape,
                0x9b => *self = Self::Csi(Vec::new()),
                _ => *self = Self::Ground,
            },
            Self::Csi(params) => {
                if byte == 0x1b {
                    *self = Self::Escape;
                } else if (0x40..=0x7e).contains(&byte) {
                    *self = Self::Ground;
                } else {
                    params.push(byte);
                }
            }
        }
    }
}

/// 모든 attach client 의 geometry 중 컴포넌트별 최소값을 반환한다. 빈 리스트 면
/// `None` — 호출자가 "잔여 subscriber 가 없으니 PTY 를 그대로 두자" 같은 분기를
/// 명시적으로 표현할 수 있게 한다. 순수 함수로 떼낸 것은 단위 테스트에서 lock 이나
/// PTY 모킹 없이 정책을 검증하기 위함.
fn clamp_to_smallest(subscribers: &[Subscriber]) -> Option<(u16, u16)> {
    let mut iter = subscribers.iter();
    let first = iter.next()?;
    let mut rows = first.rows;
    let mut cols = first.cols;
    for sub in iter {
        rows = rows.min(sub.rows);
        cols = cols.min(sub.cols);
    }
    Some((rows, cols))
}

/// 기존 attach 들에 새 attach 후보 geometry 를 더했을 때의 clamp target 을 계산한다.
/// subscribe snapshot 은 실제 등록/resize 적용보다 먼저 합성되므로, 새 narrow client
/// 에게 wide frame 을 보내지 않도록 후보 geometry 를 포함한 target 크기로 만든다.
fn clamp_to_smallest_with_candidate(
    subscribers: &[Subscriber],
    candidate_rows: u16,
    candidate_cols: u16,
) -> (u16, u16) {
    let mut rows = candidate_rows;
    let mut cols = candidate_cols;
    for sub in subscribers {
        rows = rows.min(sub.rows);
        cols = cols.min(sub.cols);
    }
    (rows, cols)
}

fn validate_terminal_geometry(context: &str, rows: u16, cols: u16) -> Result<()> {
    if rows == 0 || cols == 0 {
        bail!("{context}: terminal dimensions must be at least 1 row and 1 column");
    }
    if rows > MAX_TERMINAL_ROWS || cols > MAX_TERMINAL_COLS {
        bail!(
            "{context}: terminal dimensions {rows}x{cols} exceed maximum {MAX_TERMINAL_ROWS}x{MAX_TERMINAL_COLS}"
        );
    }
    let cells = u32::from(rows) * u32::from(cols);
    if cells > MAX_TERMINAL_CELLS {
        bail!("{context}: terminal area {cells} cells exceeds maximum {MAX_TERMINAL_CELLS} cells");
    }
    Ok(())
}

fn initial_pty_size(rows: Option<u16>, cols: Option<u16>) -> Result<(u16, u16)> {
    let rows = rows.unwrap_or(24);
    let cols = cols.unwrap_or(80);
    validate_terminal_geometry("new", rows, cols)?;
    Ok((rows, cols))
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum TerminalPrefixState {
    #[default]
    Ground,
    Escape,
    Csi,
    String,
    StringEscape,
}

#[derive(Debug, Default)]
struct TerminalPrefixTracker {
    state: TerminalPrefixState,
    pending: Vec<u8>,
    utf8_remaining: u8,
}

impl TerminalPrefixTracker {
    fn process(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.process_byte(byte);
        }
    }

    fn pending_bytes(&self) -> Vec<u8> {
        self.pending.clone()
    }

    fn process_byte(&mut self, byte: u8) {
        let raw_c1 = !self.is_utf8_continuation(byte);
        match self.state {
            TerminalPrefixState::Ground => self.process_ground(byte, raw_c1),
            TerminalPrefixState::Escape => self.process_escape(byte),
            TerminalPrefixState::Csi => self.process_csi(byte, raw_c1),
            TerminalPrefixState::String => self.process_string(byte, raw_c1),
            TerminalPrefixState::StringEscape => self.process_string_escape(byte, raw_c1),
        }
        self.update_utf8_state(byte);
        if self.pending.len() > MAX_PENDING_ESCAPE_BYTES {
            self.pending.clear();
            self.state = TerminalPrefixState::Ground;
        }
    }

    fn is_utf8_continuation(&self, byte: u8) -> bool {
        self.utf8_remaining > 0 && (0x80..=0xbf).contains(&byte)
    }

    fn update_utf8_state(&mut self, byte: u8) {
        if self.utf8_remaining > 0 {
            if (0x80..=0xbf).contains(&byte) {
                self.utf8_remaining -= 1;
                return;
            }
            self.utf8_remaining = 0;
        }
        self.utf8_remaining = match byte {
            0xc2..=0xdf => 1,
            0xe0..=0xef => 2,
            0xf0..=0xf4 => 3,
            _ => 0,
        };
    }

    fn process_ground(&mut self, byte: u8, raw_c1: bool) {
        match byte {
            0x1b => {
                self.pending.clear();
                self.pending.push(byte);
                self.state = TerminalPrefixState::Escape;
            }
            0x90 | 0x98 | 0x9d..=0x9f if raw_c1 => {
                self.pending.clear();
                self.pending.push(byte);
                self.state = TerminalPrefixState::String;
            }
            0x9b if raw_c1 => {
                self.pending.clear();
                self.pending.push(byte);
                self.state = TerminalPrefixState::Csi;
            }
            _ => {
                self.pending.clear();
            }
        }
    }

    fn process_escape(&mut self, byte: u8) {
        self.pending.push(byte);
        match byte {
            b'[' => self.state = TerminalPrefixState::Csi,
            b']' | b'P' | b'X' | b'^' | b'_' => self.state = TerminalPrefixState::String,
            0x1b => {
                self.pending.clear();
                self.pending.push(byte);
                self.state = TerminalPrefixState::Escape;
            }
            _ => {
                self.pending.clear();
                self.state = TerminalPrefixState::Ground;
            }
        }
    }

    fn process_csi(&mut self, byte: u8, raw_c1: bool) {
        self.pending.push(byte);
        match byte {
            0x18 | 0x1a => {
                self.pending.clear();
                self.state = TerminalPrefixState::Ground;
            }
            0x9c if raw_c1 => {
                self.pending.clear();
                self.state = TerminalPrefixState::Ground;
            }
            0x1b => {
                self.pending.clear();
                self.pending.push(byte);
                self.state = TerminalPrefixState::Escape;
            }
            byte if (0x40..=0x7e).contains(&byte) => {
                self.pending.clear();
                self.state = TerminalPrefixState::Ground;
            }
            _ => {}
        }
    }

    fn process_string(&mut self, byte: u8, raw_c1: bool) {
        self.pending.push(byte);
        match byte {
            0x07 | 0x18 | 0x1a => {
                self.pending.clear();
                self.state = TerminalPrefixState::Ground;
            }
            0x9c if raw_c1 => {
                self.pending.clear();
                self.state = TerminalPrefixState::Ground;
            }
            0x1b => self.state = TerminalPrefixState::StringEscape,
            _ => {}
        }
    }

    fn process_string_escape(&mut self, byte: u8, raw_c1: bool) {
        self.pending.push(byte);
        match byte {
            b'\\' | 0x18 | 0x1a => {
                self.pending.clear();
                self.state = TerminalPrefixState::Ground;
            }
            0x9c if raw_c1 => {
                self.pending.clear();
                self.state = TerminalPrefixState::Ground;
            }
            0x1b => {}
            _ => self.state = TerminalPrefixState::String,
        }
    }
}

/// PR #17: raw ring replay 대신 현재 terminal state 를 새 attach 가 바로 해석할 수
/// 있는 escape stream 으로 합성한다. `vt100` 의 `state_formatted` 는 현재 visible
/// contents 와 input mode 를 재현하기에 충분한 bytes 를 만든다. 새 attach 의 geometry
/// 로 clone screen 을 resize 한 뒤 합성해, narrow mobile attach 에 wide raw history 를
/// 그대로 주입하지 않는다.
fn screen_state_snapshot(
    parser: &vt100::Parser,
    normal_screen: &vt100::Screen,
    rows: u16,
    cols: u16,
    pending_control_prefix: &[u8],
) -> Option<OutputChunk> {
    let mut snapshot = Vec::new();
    if parser.screen().alternate_screen() {
        let mut normal = normal_screen.clone();
        normal.set_scrollback(0);
        normal.set_size(rows, cols);
        snapshot.extend(normal.state_formatted());
        // `state_formatted` 는 visible contents/input modes 를 재현하지만 xterm
        // alternate-screen 진입 CSI 자체는 방출하지 않는다. lterm client 는 이 CSI 를
        // 관찰해야 status-bar refresh 를 alt buffer 위에 그리지 않으므로, active
        // alt-screen snapshot 앞에 session normal buffer 를 먼저 그린 뒤 명시적으로
        // 1049 enter 를 붙인다. 이후 live stream 이 1049 exit 를 보내도 attach 전 local
        // 화면이 아니라 session 의 normal buffer 로 복귀한다.
        snapshot.extend_from_slice(ALT_SCREEN_ENTER);
    }
    let mut screen = parser.screen().clone();
    screen.set_scrollback(0);
    screen.set_size(rows, cols);
    snapshot.extend(screen.state_formatted());
    snapshot.extend_from_slice(pending_control_prefix);
    if snapshot.is_empty() {
        None
    } else {
        Some(Arc::from(snapshot.into_boxed_slice()))
    }
}

/// PR #16: `subscribers` snapshot 에 `chunk` 를 broadcast 하면서 즉시 보낼 수 없는 sub
/// 에 대해 짧은 윈도우 동안 한 번 더 회복 기회를 준다. 두 단계로 동작한다:
///
/// 1. **Pass 1 (`try_send`, non-blocking)**: 모든 sub 에 즉시 시도. `Ok` 면 끝, `Full`
///    이면 대기 리스트에 보류, `Disconnected` 면 evict 대상으로 즉시 분류.
/// 2. **Pass 2 (round-robin `try_send`, 단일 공유 deadline)**: pass 1 에서 보류된 sub
///    들을 라운드로빈으로 돌며 `try_send` 를 반복한다. 라운드 한 번 돌아도 비워지지
///    않은 sub 가 남아 있으면 5ms sleep 후 다시 라운드. 한 sub 가 회복되면 `pending`
///    에서 빠지고, `Disconnected` 가 되면 evict 후보로 분류된다. 라운드를 도는 동안
///    공유 deadline (`Instant::now() + timeout`) 이 만료되면 잔여 sub 들은 일괄 evict.
///
/// **PR #16 quad-review HIGH 후속 (Codex security)**: 이전 구현은 pending sub 마다
/// 별도 `timeout` 윈도우를 순차로 소진했다 — 최악 시 wall time = `K * timeout`. 한
/// 세션에 32개 attach 가 모두 laggy 면 32 × 100ms = 3.2s 동안 PTY reader 스레드가
/// 멈춰 다른 모든 attach 의 출력이 정체되는 attacker-influenced DoS 경로였다. 본
/// 라운드로빈 + 공유 deadline 구조에서는 K 와 무관하게 worst-case wall time = `timeout`.
/// 라운드로빈이라 한 sub 의 `Full` 이 다른 sub 의 회복 기회를 차단하지도 않는다.
///
/// `SyncSender::send_timeout` 이 stable 이 아니므로 폴링으로 같은 의미를 구현한다 —
/// 정확한 wakeup 시점이 주어지지 않으니 polling 간격은 짧게(5ms) 두어 회복 latency 를
/// 최소화한다. 그래야 모바일 SSH 의 50–200ms 트랜지언트 jitter 가 attach 를 끊지
/// 않으면서도, 실제로 stuck 된 consumer 는 timeout 후에 evict 되어 PR #13 의
/// zombie-attach guard 가 유지된다.
///
/// 반환값은 evict 해야 할 sub 들의 id 목록. 호출자가 이 id 들로 `subscribers` lock 을
/// 다시 잡고 `evict_disconnected_subscribers` 를 호출해 lock-then-call 패턴을 완성한다.
///
/// 최악 시 본 함수의 wall time 은 K (pending sub 수) 와 무관하게 `timeout` 이다 —
/// 정상 sub 들은 pass 1 에서 이미 chunk 를 받았으므로 pass 2 가 길어져도 그들의
/// 전달은 영향이 없다.
fn broadcast_chunk(
    subscribers: &[Subscriber],
    chunk: OutputChunk,
    timeout: Duration,
    on_backpressure: Option<&(dyn Fn() + Send + Sync)>,
) -> Vec<u64> {
    let mut disconnected: Vec<u64> = Vec::new();
    let mut pending: Vec<usize> = Vec::new();
    // Pass 1: 모든 sub 에 즉시 try_send. Full 이면 pending 에 보류.
    for (idx, sub) in subscribers.iter().enumerate() {
        match sub.tx.try_send(Arc::clone(&chunk)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                if let Some(hook) = on_backpressure {
                    hook();
                }
                pending.push(idx);
            }
            Err(TrySendError::Disconnected(_)) => disconnected.push(sub.id),
        }
    }
    // Pass 2: 단일 공유 deadline 안에서 pending 을 라운드로빈으로 폴링.
    let deadline = std::time::Instant::now() + timeout;
    while !pending.is_empty() && std::time::Instant::now() < deadline {
        pending.retain(|&idx| {
            let sub = &subscribers[idx];
            match sub.tx.try_send(Arc::clone(&chunk)) {
                Ok(()) => false,
                Err(TrySendError::Disconnected(_)) => {
                    disconnected.push(sub.id);
                    false
                }
                Err(TrySendError::Full(_)) => {
                    if let Some(hook) = on_backpressure {
                        hook();
                    }
                    true
                }
            }
        });
        if !pending.is_empty() {
            thread::sleep(Duration::from_millis(5));
        }
    }
    // Deadline 만료 시점에 남은 sub 는 모두 evict.
    for idx in pending {
        disconnected.push(subscribers[idx].id);
    }
    disconnected
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

fn forward_attach_output(mut output: UnixStream, rx: Receiver<OutputChunk>) -> bool {
    let mut output_failed = false;
    for bytes in rx {
        if output.write_all(bytes.as_ref()).is_err() {
            output_failed = true;
            break;
        }
        let _ = output.flush();
    }
    // Wake the input loop when output is gone. On failure this removes stale
    // subscriber geometry; on clean session teardown this lets attached clients exit
    // promptly even if the PTY-side command ignores TERM/HUP.
    let _ = output.shutdown(std::net::Shutdown::Both);
    output_failed
}

fn handle_connection(state: Arc<State>, mut stream: UnixStream) -> Result<()> {
    verify_peer_owner(&stream)?;
    let frame = read_request_frame_with_timeout(&mut stream, REQUEST_READ_TIMEOUT)?;
    stream.set_read_timeout(None).ok();
    let line = frame.line;
    if line.trim().is_empty() {
        return Ok(());
    }
    let request: Request = serde_json::from_str(&line)
        .with_context(|| format!("parse request: {}", sanitized_preview(&line)))?;

    if let Request::Attach { target, rows, cols } = request {
        return handle_attach(state, stream, &target, rows, cols, frame.buffered);
    }

    if let Request::CapabilityChannel { action } = request {
        if !frame.buffered.is_empty() {
            bail!("capability channel sent sensitive bytes before ready");
        }
        return handle_capability_channel(state, stream, action);
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

fn handle_capability_channel(
    state: Arc<State>,
    mut stream: UnixStream,
    action: CapabilityAction,
) -> Result<()> {
    let ready = Response::ok(serde_json::json!({
        "ready": true,
        "protocol_version": CAPABILITY_PROTOCOL_VERSION,
    }));
    serde_json::to_writer(&mut stream, &ready).context("write capability ready response")?;
    stream
        .write_all(b"\n")
        .context("write capability ready newline")?;
    stream.flush().context("flush capability ready response")?;

    let frame = read_request_frame_with_limit(
        &mut stream,
        REQUEST_READ_TIMEOUT,
        MAX_SENSITIVE_CAPABILITY_FRAME_BYTES,
    )
    .map_err(|_| anyhow!("invalid sensitive capability frame"))?;
    if !frame.buffered.is_empty() {
        return write_capability_response(&mut stream, Response::err("invalid capability request"));
    }
    let frame_len = frame.line.len();
    let sensitive: SensitiveCapabilityRequest = serde_json::from_str(&frame.line)
        .map_err(|_| anyhow!("invalid sensitive capability frame ({frame_len} bytes)"))?;
    let response = match (action, sensitive) {
        (CapabilityAction::Input, SensitiveCapabilityRequest::Input { token, data }) => {
            match apply_capability_input(&state, &token, data) {
                Ok(()) => Response::empty(),
                Err(_) => Response::err("capability input rejected"),
            }
        }
        (CapabilityAction::Revoke, SensitiveCapabilityRequest::Revoke { token }) => {
            revoke_input_capability(&state, &token);
            Response::empty()
        }
        _ => Response::err("capability request does not match channel action"),
    };
    write_capability_response(&mut stream, response)
}

fn write_capability_response(stream: &mut UnixStream, response: Response) -> Result<()> {
    serde_json::to_writer(&mut *stream, &response).context("write capability response")?;
    stream
        .write_all(b"\n")
        .context("write capability response newline")?;
    stream.flush().context("flush capability response")
}

#[derive(Debug)]
struct RequestFrame {
    line: String,
    /// Bytes read after the newline while chunk-reading the request header.
    ///
    /// For normal RPC these are ignored because the protocol is one request per
    /// connection. For attach they are already user input bytes and must be
    /// replayed into the PTY after the attach handshake succeeds.
    buffered: Vec<u8>,
}

fn read_request_frame_with_timeout(
    stream: &mut UnixStream,
    timeout: Duration,
) -> Result<RequestFrame> {
    read_request_frame_with_limit(stream, timeout, MAX_REQUEST_BYTES)
}

fn read_request_frame_with_limit(
    stream: &mut UnixStream,
    timeout: Duration,
    max_request_bytes: usize,
) -> Result<RequestFrame> {
    let deadline = Instant::now() + timeout;
    let mut bytes = Vec::new();
    let mut buf = [0_u8; 8192];
    loop {
        let now = Instant::now();
        if now >= deadline {
            bail!("request timed out before newline");
        }
        let remaining = deadline.saturating_duration_since(now);
        if remaining.is_zero() {
            bail!("request timed out before newline");
        }
        stream
            .set_read_timeout(Some(remaining))
            .context("set request read timeout")?;
        let n = match stream.read(&mut buf) {
            Ok(n) => n,
            Err(err) if err.kind() == ErrorKind::Interrupted => continue,
            Err(err)
                if err.kind() == ErrorKind::WouldBlock || err.kind() == ErrorKind::TimedOut =>
            {
                if Instant::now() >= deadline {
                    bail!("request timed out before newline");
                }
                continue;
            }
            Err(err) => return Err(err).context("read request line"),
        };
        if n == 0 {
            break;
        }
        if let Some(frame) = request_frame_from_chunk(&mut bytes, &buf[..n], max_request_bytes)? {
            return Ok(frame);
        }
    }
    if !bytes.is_empty() {
        bail!("request missing newline before EOF");
    }
    Ok(RequestFrame {
        line: String::from_utf8(bytes).context("request is not valid UTF-8")?,
        buffered: Vec::new(),
    })
}

fn request_frame_from_chunk(
    bytes: &mut Vec<u8>,
    chunk: &[u8],
    max_request_bytes: usize,
) -> Result<Option<RequestFrame>> {
    if let Some(pos) = chunk.iter().position(|byte| *byte == b'\n') {
        let line_len = pos + 1;
        ensure_request_capacity(bytes.len(), line_len, max_request_bytes)?;
        bytes.extend_from_slice(&chunk[..line_len]);
        return Ok(Some(RequestFrame {
            line: String::from_utf8(std::mem::take(bytes)).context("request is not valid UTF-8")?,
            buffered: chunk[line_len..].to_vec(),
        }));
    }

    ensure_request_capacity(bytes.len(), chunk.len(), max_request_bytes)?;
    bytes.extend_from_slice(chunk);
    Ok(None)
}

fn ensure_request_capacity(
    current_len: usize,
    additional_len: usize,
    max_request_bytes: usize,
) -> Result<()> {
    let Some(next_len) = current_len.checked_add(additional_len) else {
        bail!("request exceeded {max_request_bytes} bytes");
    };
    if next_len > max_request_bytes {
        bail!("request exceeded {max_request_bytes} bytes");
    }
    Ok(())
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
        Request::Status => {
            let session_count = lock(&state.sessions).by_pane.len();
            Ok(Response::ok(DaemonStatus {
                version: env!("CARGO_PKG_VERSION").to_string(),
                protocol_version: PROTOCOL_VERSION,
                session_count: session_count as u64,
                active_connections: state.active_connections.load(Ordering::SeqCst) as u64,
                shutting_down: state.shutting_down.load(Ordering::SeqCst),
                // SAFETY: geteuid(2) is POSIX-required thread-safe and infallible.
                // 같은 OS 사용자 trust boundary 식별자. doctor가 peer 신원을 보고할 수 있게 한다.
                daemon_uid: Some(unsafe { geteuid() }),
                // state.started_at_unix_secs는 이미 Option<u64> — clock 실패 시 None을
                // 그대로 wire에 전송해 client uptime이 sentinel 0으로 misreport되는 것을 방지.
                started_at_unix_secs: state.started_at_unix_secs,
            }))
        }
        Request::New {
            name,
            command,
            cwd,
            rows,
            cols,
            parent_pane_id,
            parent_token,
            env,
            status_theme,
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
                    status_theme,
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
            env,
            status_theme,
        } => {
            let target = normalize_target(&target);
            if let Ok(session) = resolve_session(state, &target) {
                return Ok(Response::ok(session.info()));
            }
            if target.starts_with('%') {
                bail!(
                    "cannot auto-create a missing pane target: {target}. Pane ids (e.g. %1) cannot be created by name; run `lterm list` to find an active pane or `lterm start <NAME>` to create a new session."
                );
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
                    env,
                    status_theme,
                    tmux: false,
                },
            )?;
            Ok(Response::ok(session.info()))
        }
        Request::List => {
            let sessions: Vec<_> = {
                let sessions = lock(&state.sessions);
                sessions.by_pane.values().cloned().collect()
            };
            let mut infos: Vec<_> = sessions.iter().map(|s| s.info()).collect();
            infos.sort_by_key(|info| info.created_unix_ms);
            Ok(Response::ok(infos))
        }
        Request::Info { target } => Ok(Response::ok(resolve_session(state, &target)?.info())),
        Request::Instrument { target } => Ok(Response::ok(
            resolve_session(state, &target)?.instrument_snapshot_relaxed(),
        )),
        Request::MetadataHistory { target } => Ok(Response::ok(metadata_history(state, &target)?)),
        Request::MetadataUndo { target } => Ok(Response::ok(metadata_step(
            state,
            &target,
            MetadataStepDirection::Undo,
        )?)),
        Request::MetadataRedo { target } => Ok(Response::ok(metadata_step(
            state,
            &target,
            MetadataStepDirection::Redo,
        )?)),
        Request::MetadataPurgeHistory {
            target,
            irreversible,
            session_id,
        } => Ok(Response::ok(metadata_purge_history(
            state,
            &target,
            irreversible,
            &session_id,
        )?)),
        Request::IssueInputCapability {
            target,
            byte_budget,
        } => Ok(Response::ok(issue_input_capability(
            state,
            &target,
            byte_budget,
        )?)),
        Request::Rename { target, name } => Ok(Response::ok(rename_session(state, &target, name)?)),
        Request::SetStatusTheme {
            target,
            status_theme,
        } => Ok(Response::ok(set_status_theme(
            state,
            &target,
            status_theme,
        )?)),
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
        Request::Capture { target, start, end } => {
            let session = resolve_session(state, &target)?;
            Ok(Response::ok(session.capture(start, end)))
        }
        Request::WaitExit { target, timeout_ms } => {
            let _wait_guard = state.try_acquire_blocking_wait().ok_or_else(|| {
                anyhow!(
                    "too many concurrent wait requests (limit {MAX_BLOCKING_WAITS}); retry after another wait finishes or use --timeout"
                )
            })?;
            let session = resolve_session(state, &target)?;
            Ok(Response::ok(wait_for_session_exit(&session, timeout_ms)?))
        }
        Request::WaitContains {
            target,
            needle,
            start,
            timeout_ms,
        } => {
            validate_wait_contains_needle(&needle)?;
            let _wait_guard = state.try_acquire_blocking_wait().ok_or_else(|| {
                anyhow!(
                    "too many concurrent wait requests (limit {MAX_BLOCKING_WAITS}); retry after another wait finishes or use --timeout"
                )
            })?;
            let session = resolve_session(state, &target)?;
            Ok(Response::ok(wait_for_session_contains(
                &session, &needle, start, timeout_ms,
            )?))
        }
        Request::Resize {
            target,
            rows,
            cols,
            subscriber_id,
        } => {
            validate_terminal_geometry("resize", rows, cols)?;
            let session = resolve_session(state, &target)?;
            // PR #15 quad-review HIGH 후속(#1): per-client geometry 변경 / clamp 결정 /
            // master.resize / cached rows·cols 갱신 의 4단계가 다른 attach 의
            // subscribe/unsubscribe/Resize 와 인터리빙되지 않게 본 핸들러 진입과
            // 동시에 geometry_apply 락을 잡는다. 두 분기 (per-attach·legacy) 모두
            // 같은 락을 공유하므로 legacy 경로의 직접 master.resize 도 직렬화된다.
            let geometry_guard = lock(&session.geometry_apply);
            match subscriber_id {
                // PR #15: attach client 발 SIGWINCH 갱신. per-client geometry 만 갱신한
                // 뒤 모든 attach 의 min 으로 PTY 를 재계산한다 (clamp-to-smallest).
                // 매칭되는 id 가 없으면 stale subscriber id 를 보낸 것이므로 silent
                // no-op 대신 명시적 에러로 surface 시켜 client 측 race 를 드러낸다.
                Some(id) => {
                    {
                        let mut subscribers = lock(&session.subscribers);
                        let sub = subscribers
                            .iter_mut()
                            .find(|sub| sub.id == id)
                            .with_context(|| {
                                format!("resize: subscriber id {id} no longer attached")
                            })?;
                        sub.rows = rows;
                        sub.cols = cols;
                    }
                    session.apply_clamped_pty_size(&geometry_guard)?;
                }
                // legacy 경로: `lterm resize` CLI 와 tmux-compat shim 처럼 attach 가
                // 아닌 컨트롤 채널이 직접 PTY 사이즈를 강제하는 케이스. per-client
                // geometry 추적을 거치지 않고 즉시 master.resize 한다 — 와이어
                // 호환성 유지.
                //
                // PR #15 quad-review MEDIUM 후속(#3): attach client 가 살아있는 동안에
                // 본 경로로 사이즈를 강제하면, 다음 attach client 발 Resize 또는
                // (un)subscribe 이벤트에서 clamp-to-smallest 가 다시 PTY 를 덮어쓴다.
                // 즉 legacy 경로는 attach 가 0 명일 때나, 호출자가 이 override race 를
                // 의도적으로 받아들일 때만 안전하다. 자세한 contract 는
                // `protocol::Request::Resize` 의 `subscriber_id` 도큐먼트 참조.
                None => {
                    session.apply_pty_size(rows, cols, "resize pty")?;
                }
            }
            drop(geometry_guard);
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
        Request::Attach { .. } | Request::CapabilityChannel { .. } => {
            unreachable!("handled by handle_connection above")
        }
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
    status_theme: Option<StatusTheme>,
    tmux: bool,
}

fn create_session(state: &Arc<State>, params: NewSessionParams) -> Result<Arc<Session>> {
    let parent_request = parent_request(params.parent_pane_id, params.parent_token);
    if let Some(parent_request) = parent_request.as_ref() {
        validate_parent_request(state, parent_request)?;
    }
    let reservation = reserve_session_identity(state, params.name)?;
    let pty_system = native_pty_system();
    let (rows, cols) = initial_pty_size(params.rows, params.cols)?;
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
    let agent_name = params
        .env
        .get("LTERM_AGENT")
        .map(|value| sanitize::terminal_text(value))
        .filter(|value| !value.trim().is_empty());
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
    scrub_ambient_multiplexer_env(&mut cmd);
    if agent_name.is_some() {
        scrub_ambient_child_color_policy_env(&mut cmd);
    }
    for (key, value) in sanitize_child_env(params.env, params.tmux)? {
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
        metadata: Mutex::new(SessionMetadata::new(name.clone(), params.status_theme)),
        pane_id,
        parent_pane_id: Mutex::new(None),
        parent_session_id: Mutex::new(None),
        parent_token,
        command,
        cwd,
        created_unix_ms: now_unix_ms(),
        process_id: Some(process_id),
        process_group_id,
        agent_name,
        child: Mutex::new(child),
        killer: Mutex::new(killer),
        master: Mutex::new(pair.master),
        writer: Mutex::new(writer),
        ring: Mutex::new(VecDeque::new()),
        terminal_screen: Mutex::new(vt100::Parser::new(rows, cols, 0)),
        terminal_normal_screen: Mutex::new(vt100::Parser::new(rows, cols, 0).screen().clone()),
        terminal_pending: Mutex::new(TerminalPrefixTracker::default()),
        terminal_parser_degraded: AtomicBool::new(false),
        #[cfg(test)]
        terminal_parser_panic_on_next_update: AtomicBool::new(false),
        #[cfg(test)]
        terminal_parser_panic_on_next_snapshot: AtomicBool::new(false),
        #[cfg(test)]
        terminal_parser_panic_on_next_resize: AtomicBool::new(false),
        subscribers: Mutex::new(Vec::new()),
        output_state: Mutex::new(()),
        output_progress: (Mutex::new(OutputProgress::default()), Condvar::new()),
        #[cfg(test)]
        backpressure_hook: Mutex::new(None),
        broadcast_order: Mutex::new(()),
        geometry_apply: Mutex::new(()),
        next_subscriber_id: AtomicU64::new(1),
        alive: AtomicBool::new(true),
        cleanup_started: AtomicBool::new(false),
        cleanup_completion: (Mutex::new(false), Condvar::new()),
        cleanup_complete: AtomicBool::new(false),
        leader_exit_observed: AtomicBool::new(false),
        leader_reaped: AtomicBool::new(false),
        unreaped_cleanup_started: AtomicBool::new(false),
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
        let _output_closed = OutputClosedGuard::new(Arc::clone(&session_for_reader));
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
                    eprintln!("pty read error for {}: {err}", session_for_reader.name());
                    break;
                }
            }
        }
    });

    let state_for_waiter = Arc::clone(state);
    let session_for_waiter = Arc::clone(&session);
    thread::spawn(move || {
        let leader_exit_observed = wait_for_leader_exit_without_reaping(&session_for_waiter);
        let exit_code = {
            let mut child = lock(&session_for_waiter.child);
            if leader_exit_observed {
                terminate_unreaped_process_group(&session_for_waiter, &child);
            }
            let exit_code = match child.wait() {
                Ok(status) => status.exit_code().min(i32::MAX as u32) as i32,
                Err(err) => {
                    eprintln!("wait error for {}: {err}", session_for_waiter.name());
                    1
                }
            };
            // Writer-side half of the stored-PGID invariant: callers may use
            // the unreaped process-group cleanup only while holding this same
            // child lock, so the flag cannot flip between their guard check
            // and residual signal ladder.
            session_for_waiter
                .leader_reaped
                .store(true, Ordering::SeqCst);
            exit_code
        };
        session_for_waiter
            .exit_code
            .store(exit_code, Ordering::SeqCst);
        finalize_session(
            &state_for_waiter,
            &session_for_waiter,
            SessionFinalizeReason::LeaderExited,
        );
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
        if sessions.by_name.contains_key(&self.name)
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
            .insert(self.name.clone(), Arc::clone(&session));
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

fn handle_attach(
    state: Arc<State>,
    mut stream: UnixStream,
    target: &str,
    rows: u16,
    cols: u16,
    buffered_input: Vec<u8>,
) -> Result<()> {
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

    // PR #17 fold-in: attach subscribe, snapshot enqueue, clamp resize 를
    // `geometry_apply > output_state` 단일 critical section 으로 묶는다. 그래야 새
    // subscriber 가 snapshot 이후 첫 live chunk 를 받기 전에 PTY/parser geometry 가
    // 후보 clamp 값으로 업데이트된다.
    let (subscriber_id, rx) =
        match session.subscribe_with_snapshot_and_apply_clamp(rows, cols, on_evict) {
            Ok(subscription) => subscription,
            Err(err) => {
                let response = Response::err(format!("{err:#}"));
                serde_json::to_writer(&mut stream, &response).ok();
                stream.write_all(b"\n").ok();
                return Ok(());
            }
        };
    let subscription = AttachSubscriptionGuard::new(Arc::clone(&session), subscriber_id);
    // PR #15: 클라이언트가 후속 Resize 요청에서 사용할 subscriber id 를 응답에 실어
    // 보낸다. Response 모양은 그대로 두고 result 필드에만 JSON 객체로 박는다.
    //
    // PR #16: 순서 invariant — Response JSON + newline + flush 까지를 output_thread
    // spawn **전에** 동기 stream 으로 모두 마쳐야 한다. output_thread 가 stream 의
    // try_clone 사본에 라이브 chunk 를 즉시 쓰기 시작하면 Response JSON 과 첫 chunk
    // 가 같은 fd 에서 인터리빙되어 클라이언트 측 JSON 파서가 깨진다.
    let response = Response::ok(serde_json::json!({ "subscriber_id": subscriber_id }));
    serde_json::to_writer(&mut stream, &response).context("write attach ok")?;
    stream.write_all(b"\n").context("write attach ok newline")?;
    // PR #16 quad-review MEDIUM 후속 (Codex/Claude 합의): flush 실패는 본 PR 에서
    // 새로 보장하기로 한 "Response JSON 이 첫 chunk 보다 먼저 나간다" invariant 가
    // 깨졌다는 신호다. 무시하면 클라이언트 측 JSON 파서가 깨지면서도 서버는 정상
    // 진행해 ghost subscriber + output_thread 가 매달려 남는다 — apply_clamped 실패
    // 와 동일한 패턴으로 등록한 sub 를 unsubscribe 한 뒤 에러를 surface 한다.
    if let Err(err) = stream.flush() {
        return Err(err).context("flush attach ok before output thread");
    }
    let output = stream.try_clone().context("clone output stream")?;
    let output_session = Arc::clone(&session);
    let output_thread = thread::spawn(move || {
        if forward_attach_output(output, rx) {
            output_session.unsubscribe(subscriber_id);
        }
    });

    let mut input = stream;
    input
        .set_read_timeout(Some(Duration::from_millis(100)))
        .context("set attach input read timeout")?;
    if !buffered_input.is_empty()
        && (!session.alive.load(Ordering::SeqCst)
            || lock(&session.writer).write_all(&buffered_input).is_err())
    {
        // Drop the guard before joining so unsubscribe closes the output
        // channel and lets the forwarder thread exit.
        drop(subscription);
        let _ = output_thread.join();
        return Ok(());
    }
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
    // Drop the guard before joining so unsubscribe closes the output channel
    // and lets the forwarder thread exit.
    drop(subscription);
    let _ = output_thread.join();
    Ok(())
}

struct AttachSubscriptionGuard {
    session: Arc<Session>,
    subscriber_id: u64,
}

impl AttachSubscriptionGuard {
    fn new(session: Arc<Session>, subscriber_id: u64) -> Self {
        Self {
            session,
            subscriber_id,
        }
    }
}

impl Drop for AttachSubscriptionGuard {
    fn drop(&mut self) {
        // `unsubscribe` is idempotent: the output thread may have already
        // removed this subscriber after a write failure, while early-return
        // paths rely on this guard to prevent ghost subscribers.
        self.session.unsubscribe(self.subscriber_id);
    }
}

/// Updates status-bar metadata without touching the PTY or child process.
fn set_status_theme(
    state: &Arc<State>,
    target: &str,
    status_theme: Option<StatusTheme>,
) -> Result<SessionInfo> {
    let target = normalize_target(target);
    let session = {
        let sessions = lock(&state.sessions);
        let session = resolve_session_locked(&sessions, &target).ok_or_else(|| {
            anyhow!(
                "no such lterm session or pane: {}. Run `lterm list` to see active sessions or `lterm start <NAME>` to create one.",
                sanitized_preview(&target)
            )
        })?;
        let mut metadata = lock(&session.metadata);
        if metadata.current.status_theme != status_theme {
            validate_metadata_append(&metadata)?;
            let before = metadata.current.clone();
            let after = MetadataValue {
                name: before.name.clone(),
                status_theme,
            };
            let entry = MetadataJournalEntry {
                operation: MetadataOperation::StatusTheme,
                before,
                after: after.clone(),
            };
            metadata.entries.push(entry);
            metadata.cursor += 1;
            metadata.current = after;
        }
        Arc::clone(&session)
    };
    Ok(session.info())
}

/// Renames an existing session metadata entry without touching its PTY or child process.
///
/// The daemon performs target resolution and index mutation while holding the
/// global session-map lock so that `by_name` stays consistent with the mutable
/// `Session.name` field. Pane ids and UUID session ids remain stable.
fn rename_session(state: &Arc<State>, target: &str, new_name: String) -> Result<SessionInfo> {
    validate_session_name_syntax(&new_name)?;

    let target = normalize_target(target);
    let session = {
        let mut sessions = lock(&state.sessions);
        let session = resolve_session_locked(&sessions, &target).ok_or_else(|| {
            anyhow!(
                "no such lterm session or pane: {}. Run `lterm list` to see active sessions or `lterm start <NAME>` to create one.",
                sanitized_preview(&target)
            )
        })?;

        let mut metadata = lock(&session.metadata);
        if metadata.current.name == new_name {
            if sessions
                .by_name
                .get(&new_name)
                .is_some_and(|candidate| candidate.id == session.id)
            {
                Arc::clone(&session)
            } else {
                bail!(
                    "internal session name index inconsistent for: {}",
                    sanitized_preview(&new_name)
                );
            }
        } else if sessions.reserved_names.contains(&new_name)
            || sessions
                .by_name
                .get(&new_name)
                .is_some_and(|candidate| candidate.id != session.id)
        {
            bail!(
                "session name already exists: {}",
                sanitized_preview(&new_name)
            );
        } else {
            validate_metadata_append(&metadata)?;
            let old_name = metadata.current.name.clone();
            if sessions
                .by_name
                .get(&old_name)
                .is_none_or(|candidate| candidate.id != session.id)
            {
                bail!(
                    "internal session name index inconsistent for: {}",
                    sanitized_preview(&old_name)
                );
            }

            let before = metadata.current.clone();
            let after = MetadataValue {
                name: new_name.clone(),
                status_theme: before.status_theme,
            };
            let entry = MetadataJournalEntry {
                operation: MetadataOperation::Rename,
                before,
                after: after.clone(),
            };

            sessions.by_name.remove(&old_name);
            sessions
                .by_name
                .insert(new_name.clone(), Arc::clone(&session));
            metadata.entries.push(entry);
            metadata.cursor += 1;
            metadata.current = after;
            Arc::clone(&session)
        }
    };

    Ok(session.info())
}

fn validate_metadata_append(metadata: &SessionMetadata) -> Result<()> {
    if metadata.cursor != metadata.entries.len() {
        bail!(
            "metadata history has redo entries; run `lterm metadata redo` until the tip or explicitly purge history before changing metadata"
        );
    }
    if metadata.entries.len() >= MAX_METADATA_JOURNAL_ENTRIES {
        bail!(
            "metadata history is full ({MAX_METADATA_JOURNAL_ENTRIES} entries); explicitly purge history before changing metadata"
        );
    }
    Ok(())
}

fn metadata_history(state: &Arc<State>, target: &str) -> Result<MetadataHistoryResult> {
    let target = normalize_target(target);
    let sessions = lock(&state.sessions);
    let session = resolve_session_locked(&sessions, &target).ok_or_else(|| {
        anyhow!(
            "no such lterm session or pane: {}. Run `lterm list` to see active sessions.",
            sanitized_preview(&target)
        )
    })?;
    let metadata = lock(&session.metadata);
    Ok(MetadataHistoryResult {
        schema_version: "1.0".to_string(),
        session_id: session.id.clone(),
        pane_id: session.pane_id.clone(),
        current: metadata.current.clone(),
        entries: metadata.entries.clone(),
        cursor: metadata.cursor,
        capacity: MAX_METADATA_JOURNAL_ENTRIES,
        purge: metadata.purge.clone(),
    })
}

fn metadata_step(
    state: &Arc<State>,
    target: &str,
    direction: MetadataStepDirection,
) -> Result<MetadataStepResult> {
    let target = normalize_target(target);
    let mut sessions = lock(&state.sessions);
    let session = resolve_session_locked(&sessions, &target).ok_or_else(|| {
        anyhow!(
            "no such lterm session or pane: {}. Run `lterm list` to see active sessions.",
            sanitized_preview(&target)
        )
    })?;
    let mut metadata = lock(&session.metadata);
    let (entry, expected, next, next_cursor) = match direction {
        MetadataStepDirection::Undo => {
            let index = metadata
                .cursor
                .checked_sub(1)
                .context("metadata history has nothing to undo")?;
            let entry = metadata.entries[index].clone();
            (entry.clone(), entry.after, entry.before, index)
        }
        MetadataStepDirection::Redo => {
            let entry = metadata
                .entries
                .get(metadata.cursor)
                .cloned()
                .context("metadata history has nothing to redo")?;
            (
                entry.clone(),
                entry.before,
                entry.after,
                metadata.cursor + 1,
            )
        }
    };
    if metadata.current != expected {
        bail!(
            "metadata current state does not match the journal entry; refusing {} without mutation",
            match direction {
                MetadataStepDirection::Undo => "undo",
                MetadataStepDirection::Redo => "redo",
            }
        );
    }

    validate_metadata_name_transition(&sessions, &session, &metadata.current.name, &next.name)?;
    let result = MetadataStepResult {
        session_id: session.id.clone(),
        pane_id: session.pane_id.clone(),
        direction,
        applied: entry,
        current: next.clone(),
        cursor: next_cursor,
        entry_count: metadata.entries.len(),
    };

    apply_metadata_name_transition(&mut sessions, &session, &metadata.current.name, &next.name);
    metadata.current = next;
    metadata.cursor = next_cursor;
    Ok(result)
}

fn validate_metadata_name_transition(
    sessions: &SessionMaps,
    session: &Session,
    current_name: &str,
    next_name: &str,
) -> Result<()> {
    if sessions
        .by_name
        .get(current_name)
        .is_none_or(|candidate| candidate.id != session.id)
    {
        bail!(
            "internal session name index inconsistent for: {}",
            sanitized_preview(current_name)
        );
    }
    if current_name != next_name {
        validate_session_name_syntax(next_name)?;
        if sessions.reserved_names.contains(next_name)
            || sessions
                .by_name
                .get(next_name)
                .is_some_and(|candidate| candidate.id != session.id)
        {
            bail!(
                "session name already exists: {}",
                sanitized_preview(next_name)
            );
        }
    }
    Ok(())
}

fn apply_metadata_name_transition(
    sessions: &mut SessionMaps,
    session: &Arc<Session>,
    current_name: &str,
    next_name: &str,
) {
    if current_name == next_name {
        return;
    }
    sessions.by_name.remove(current_name);
    sessions
        .by_name
        .insert(next_name.to_string(), Arc::clone(session));
}

fn metadata_purge_history(
    state: &Arc<State>,
    target: &str,
    irreversible: bool,
    claimed_session_id: &str,
) -> Result<MetadataPurgeResult> {
    if !irreversible {
        bail!("metadata history purge requires --irreversible");
    }
    let parsed = Uuid::parse_str(claimed_session_id)
        .context("metadata history purge requires a canonical session UUID")?;
    if parsed.hyphenated().to_string() != claimed_session_id {
        bail!("metadata history purge requires a canonical session UUID");
    }

    let target = normalize_target(target);
    let sessions = lock(&state.sessions);
    let session = resolve_session_locked(&sessions, &target).ok_or_else(|| {
        anyhow!(
            "no such lterm session or pane: {}. Run `lterm list` to see active sessions.",
            sanitized_preview(&target)
        )
    })?;
    if session.id != claimed_session_id {
        bail!("metadata history purge session id does not match the live target");
    }
    let mut metadata = lock(&session.metadata);
    if metadata.entries.is_empty() {
        bail!("metadata history is empty; nothing to purge");
    }
    let purged_entries = metadata.entries.len();
    let purged_entries_u64 = u64::try_from(purged_entries).context("metadata history too large")?;
    let generation = metadata
        .purge
        .generation
        .checked_add(1)
        .context("metadata purge generation overflow")?;
    let purged_entries_total = metadata
        .purge
        .purged_entries_total
        .checked_add(purged_entries_u64)
        .context("metadata purged entry counter overflow")?;
    let purge = MetadataPurgeAggregate {
        generation,
        purged_entries_total,
        last_purged_unix_ms: Some(u64::try_from(now_unix_ms()).unwrap_or(u64::MAX)),
    };
    let result = MetadataPurgeResult {
        session_id: session.id.clone(),
        pane_id: session.pane_id.clone(),
        current: metadata.current.clone(),
        purged_entries,
        cursor: 0,
        entry_count: 0,
        purge: purge.clone(),
    };

    metadata.entries.clear();
    metadata.cursor = 0;
    metadata.purge = purge;
    Ok(result)
}

fn remove_session(state: &Arc<State>, session: &Session) {
    let mut sessions = lock(&state.sessions);
    let name = session.name();
    if sessions
        .by_name
        .get(&name)
        .is_some_and(|s| s.id == session.id)
    {
        sessions.by_name.remove(&name);
    }
    debug_assert!(
        !sessions.by_name.values().any(|s| s.id == session.id),
        "stale session name index after remove_session"
    );
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
    lock(&state.input_capabilities)
        .grants
        .retain(|_, grant| grant.session_id != session.id);
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

#[derive(Clone, Copy, Debug)]
enum SessionFinalizeReason {
    LeaderExited,
    TerminateRequested,
}

fn terminate_session(state: &Arc<State>, session: &Session) {
    finalize_session(state, session, SessionFinalizeReason::TerminateRequested);
}

fn finalize_session(state: &Arc<State>, session: &Session, reason: SessionFinalizeReason) {
    session.alive.store(false, Ordering::SeqCst);
    if session.cleanup_started.swap(true, Ordering::SeqCst) {
        wait_for_cleanup_complete(session);
        return;
    }

    let mut completion_guard = CleanupCompletionGuard::new(session);
    if matches!(reason, SessionFinalizeReason::TerminateRequested) {
        terminate_process_group_for_request(session);
    }
    session.close_subscribers();
    terminate_child_sessions(state, &session.id);
    remove_session(state, session);
    mark_cleanup_complete(session);
    completion_guard.disarm();
}

struct CleanupCompletionGuard<'a> {
    session: &'a Session,
    armed: bool,
}

impl<'a> CleanupCompletionGuard<'a> {
    fn new(session: &'a Session) -> Self {
        Self {
            session,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CleanupCompletionGuard<'_> {
    fn drop(&mut self) {
        if self.armed && !self.session.cleanup_complete.load(Ordering::SeqCst) {
            eprintln!(
                "session cleanup for {} unwound before normal completion; releasing waiters",
                self.session.name()
            );
            mark_cleanup_complete(self.session);
        }
    }
}

fn mark_cleanup_complete(session: &Session) {
    session.cleanup_complete.store(true, Ordering::SeqCst);
    let (complete, changed) = &session.cleanup_completion;
    *lock(complete) = true;
    changed.notify_all();
}

fn wait_for_cleanup_complete(session: &Session) {
    if session.cleanup_complete.load(Ordering::SeqCst) {
        return;
    }
    let (complete, changed) = &session.cleanup_completion;
    let mut complete = lock(complete);
    while !*complete {
        complete = match changed.wait(complete) {
            Ok(guard) => guard,
            Err(poisoned) => {
                eprintln!("recovering poisoned cleanup completion mutex");
                poisoned.into_inner()
            }
        };
    }
}

fn wait_for_session_exit(session: &Session, timeout_ms: Option<u64>) -> Result<WaitExitResult> {
    let timed_out = if session.cleanup_complete.load(Ordering::SeqCst) {
        false
    } else {
        let deadline = timeout_ms
            .map(Duration::from_millis)
            .map(|duration| {
                Instant::now()
                    .checked_add(duration)
                    .context("wait timeout is too large")
            })
            .transpose()?;
        let (complete, changed) = &session.cleanup_completion;
        let mut complete = lock(complete);
        while !*complete {
            if let Some(deadline) = deadline {
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                let remaining = deadline.saturating_duration_since(now);
                let wait_result = changed.wait_timeout(complete, remaining);
                let (guard, timeout_result) = match wait_result {
                    Ok(result) => result,
                    Err(poisoned) => {
                        eprintln!("recovering poisoned cleanup completion mutex");
                        poisoned.into_inner()
                    }
                };
                complete = guard;
                if timeout_result.timed_out() && !*complete {
                    break;
                }
            } else {
                complete = match changed.wait(complete) {
                    Ok(guard) => guard,
                    Err(poisoned) => {
                        eprintln!("recovering poisoned cleanup completion mutex");
                        poisoned.into_inner()
                    }
                };
            }
        }
        !*complete
    };

    let session = session.info();
    Ok(WaitExitResult {
        exited: !timed_out && !session.alive,
        timed_out,
        session,
    })
}

fn wait_for_session_contains(
    session: &Session,
    needle: &str,
    start: Option<i32>,
    timeout_ms: Option<u64>,
) -> Result<WaitContainsResult> {
    validate_wait_contains_needle(needle)?;
    let started = Instant::now();
    let deadline = timeout_ms
        .map(Duration::from_millis)
        .map(|duration| {
            started
                .checked_add(duration)
                .context("wait timeout is too large")
        })
        .transpose()?;

    let mut scanner = WaitContainsScanner::default();
    loop {
        let before_capture;
        let matched = {
            let _output_guard = lock(&session.output_state);
            before_capture = *lock(&session.output_progress.0);
            scanner.contains(session, before_capture.total_bytes, start, needle)
        };
        if matched {
            return Ok(WaitContainsResult {
                session: session.info(),
                matched: true,
                timed_out: false,
                exited: !session.alive.load(Ordering::SeqCst),
            });
        }

        let (progress, changed) = &session.output_progress;
        let progress = lock(progress);
        if progress.revision != before_capture.revision {
            if let Some(deadline) = deadline {
                if Instant::now() >= deadline {
                    return Ok(WaitContainsResult {
                        session: session.info(),
                        matched: false,
                        timed_out: true,
                        exited: !session.alive.load(Ordering::SeqCst),
                    });
                }
            }
            drop(progress);
            thread::yield_now();
            continue;
        }
        if progress.closed {
            return Ok(WaitContainsResult {
                session: session.info(),
                matched: false,
                timed_out: false,
                exited: true,
            });
        }

        if let Some(deadline) = deadline {
            let now = Instant::now();
            if now >= deadline {
                return Ok(WaitContainsResult {
                    session: session.info(),
                    matched: false,
                    timed_out: true,
                    exited: !session.alive.load(Ordering::SeqCst),
                });
            }
            let wait_result =
                changed.wait_timeout(progress, deadline.saturating_duration_since(now));
            let (next_progress, timeout_result) = match wait_result {
                Ok(result) => result,
                Err(poisoned) => {
                    eprintln!("recovering poisoned output progress mutex");
                    poisoned.into_inner()
                }
            };
            if timeout_result.timed_out() && next_progress.revision == before_capture.revision {
                return Ok(WaitContainsResult {
                    session: session.info(),
                    matched: false,
                    timed_out: true,
                    exited: !session.alive.load(Ordering::SeqCst),
                });
            }
        } else {
            let _guard = match changed.wait(progress) {
                Ok(guard) => guard,
                Err(poisoned) => {
                    eprintln!("recovering poisoned output progress mutex");
                    poisoned.into_inner()
                }
            };
        }
    }
}

#[derive(Default)]
struct WaitContainsScanner {
    initialized: bool,
    last_total_bytes: u64,
    last_start: Option<i32>,
    last_start_total: u64,
    sanitized_tail: String,
    sanitizer_state: sanitize::TerminalCaptureState,
    #[cfg(test)]
    full_scan_count: u64,
    #[cfg(test)]
    incremental_scan_count: u64,
}

impl WaitContainsScanner {
    fn contains(
        &mut self,
        session: &Session,
        total_bytes: u64,
        start: Option<i32>,
        needle: &str,
    ) -> bool {
        let ring = lock(&session.ring);
        let start_total = capture_start_total_from_ring(&ring, total_bytes, start);
        self.contains_from_start(&ring, total_bytes, start, start_total, needle)
    }

    fn contains_from_start(
        &mut self,
        ring: &VecDeque<u8>,
        total_bytes: u64,
        start: Option<i32>,
        start_total: u64,
        needle: &str,
    ) -> bool {
        let ring_start_total = total_bytes.saturating_sub(ring.len() as u64);
        let needs_full_scan = !self.initialized
            || self.last_start != start
            || self.last_start_total != start_total
            || self.last_total_bytes < ring_start_total
            || self.last_total_bytes < start_total
            || self.last_total_bytes > total_bytes;

        let (matched, next_state, searchable_tail) = if needs_full_scan {
            #[cfg(test)]
            {
                self.full_scan_count = self.full_scan_count.saturating_add(1);
            }
            let bytes = copy_ring_bytes_from_total(ring, total_bytes, start_total);
            let mut state = sanitize::TerminalCaptureState::default();
            let sanitized = sanitize::terminal_capture_from_state(&bytes, &mut state);
            let matched = sanitized.contains(needle);
            (matched, state, sanitized)
        } else {
            #[cfg(test)]
            {
                self.incremental_scan_count = self.incremental_scan_count.saturating_add(1);
            }
            let new_start = (self.last_total_bytes.saturating_sub(ring_start_total)) as usize;
            let bytes: Vec<u8> = ring.iter().skip(new_start).copied().collect();
            let mut state = self.sanitizer_state;
            let sanitized_delta = sanitize::terminal_capture_from_state(&bytes, &mut state);
            let mut searchable =
                String::with_capacity(self.sanitized_tail.len() + sanitized_delta.len());
            searchable.push_str(&self.sanitized_tail);
            searchable.push_str(&sanitized_delta);
            let matched = searchable.contains(needle);
            (matched, state, searchable)
        };

        if !matched {
            self.initialized = true;
            self.last_total_bytes = total_bytes;
            self.last_start = start;
            self.last_start_total = start_total;
            self.sanitizer_state = next_state;
            self.sanitized_tail = sanitized_tail_for_needle(&searchable_tail, needle);
        }
        matched
    }
}

fn sanitized_tail_for_needle(text: &str, needle: &str) -> String {
    let max_bytes = needle.len().saturating_sub(1);
    if max_bytes == 0 {
        return String::new();
    }
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut start = text.len() - max_bytes;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_string()
}

fn validate_wait_contains_needle(needle: &str) -> Result<()> {
    if needle.is_empty() {
        bail!("wait contains text cannot be empty");
    }
    if needle.len() > MAX_WAIT_CONTAINS_NEEDLE_BYTES {
        bail!(
            "wait contains text exceeds {} bytes",
            MAX_WAIT_CONTAINS_NEEDLE_BYTES
        );
    }
    Ok(())
}

fn terminate_process_group_for_request(session: &Session) {
    // Hold the child lock while signaling so the waiter cannot reap the leader
    // and release the pgid anchor between an unreaped-pgid check and `kill`.
    let reap_guard = lock(&session.child);
    if session.leader_reaped.load(Ordering::SeqCst) {
        return;
    }
    if session.leader_exit_observed.load(Ordering::SeqCst) {
        terminate_unreaped_process_group(session, &reap_guard);
        return;
    }
    terminate_verified_process_group_for_request(session, &reap_guard);
    maybe_terminate_observed_unreaped_process_group(
        session,
        Duration::from_millis(50),
        &reap_guard,
    );
}

fn maybe_terminate_observed_unreaped_process_group(
    session: &Session,
    timeout: Duration,
    reap_guard: &MutexGuard<'_, Box<dyn Child + Send + Sync>>,
) {
    // Keep a short post-SIGKILL observation window for the waiter to publish
    // `leader_exit_observed` before it can acquire `child` and reap the leader.
    // During that unreaped window the stored pgid is still anchored and safe
    // for the residual group-kill ladder; after reap, we must not rely on it.
    // The guard parameter makes that lock requirement explicit at call sites.
    if wait_for_leader_exit_observed(session, timeout)
        && !session.leader_reaped.load(Ordering::SeqCst)
    {
        terminate_unreaped_process_group(session, reap_guard);
    }
}

fn wait_for_leader_exit_observed(session: &Session, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if session.leader_exit_observed.load(Ordering::SeqCst) {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    session.leader_exit_observed.load(Ordering::SeqCst)
}

fn wait_for_leader_exit_without_reaping(session: &Session) -> bool {
    let Some(pid) = session
        .process_id
        .and_then(|pid| libc::pid_t::try_from(pid).ok())
    else {
        return false;
    };

    loop {
        let mut info = MaybeUninit::<libc::siginfo_t>::zeroed();
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                info.as_mut_ptr(),
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if rc == 0 {
            session.leader_exit_observed.store(true, Ordering::SeqCst);
            return true;
        }
        let err = std::io::Error::last_os_error();
        if err.kind() == ErrorKind::Interrupted {
            continue;
        }
        eprintln!(
            "failed to observe leader exit before reap for {}: {}",
            session.name(),
            err
        );
        return false;
    }
}

fn terminate_verified_process_group_for_request(
    session: &Session,
    reap_guard: &MutexGuard<'_, Box<dyn Child + Send + Sync>>,
) {
    signal_process_group(session, libc::SIGHUP);
    match wait_for_process_group_exit_or_leader_observed(session, Duration::from_millis(150)) {
        ProcessGroupWait::LeaderObserved => {
            terminate_unreaped_process_group(session, reap_guard);
            return;
        }
        ProcessGroupWait::Exited => return,
        ProcessGroupWait::TimedOut => {}
    }
    signal_process_group(session, libc::SIGTERM);
    match wait_for_process_group_exit_or_leader_observed(session, Duration::from_millis(350)) {
        ProcessGroupWait::LeaderObserved => {
            terminate_unreaped_process_group(session, reap_guard);
            return;
        }
        ProcessGroupWait::Exited => return,
        ProcessGroupWait::TimedOut => {}
    }
    signal_process_group(session, libc::SIGKILL);
    if matches!(
        wait_for_process_group_exit_or_leader_observed(session, Duration::from_millis(150)),
        ProcessGroupWait::LeaderObserved
    ) {
        terminate_unreaped_process_group(session, reap_guard);
    }
}

fn terminate_unreaped_process_group(
    session: &Session,
    _reap_guard: &MutexGuard<'_, Box<dyn Child + Send + Sync>>,
) {
    // The guard is a witness that callers hold this session's `child` lock:
    // the waiter cannot set `leader_reaped` while this function decides
    // whether the stored pgid is still anchored by the unreaped zombie leader.
    // Keep the explicit reaped check as defense-in-depth for future call paths.
    if session.leader_reaped.load(Ordering::SeqCst) {
        return;
    }
    if session
        .unreaped_cleanup_started
        .swap(true, Ordering::SeqCst)
    {
        return;
    }
    // `waitid(..., WNOWAIT)` has observed leader exit, but `child.wait()` has
    // not reaped it yet. The stored pgid is safe only in this narrow window:
    // the zombie leader still anchors that pid/pgid number, so the kernel
    // cannot recycle it for an unrelated process group while we reap residual
    // PTY holders. Keep this ladder short: the leader already exited, so these
    // are orphaned residuals rather than an interactive foreground shell.
    signal_unreaped_process_group(session, libc::SIGHUP);
    thread::sleep(Duration::from_millis(10));
    signal_unreaped_process_group(session, libc::SIGTERM);
    thread::sleep(Duration::from_millis(10));
    signal_unreaped_process_group(session, libc::SIGKILL);
}

fn signal_unreaped_process_group(session: &Session, signal: libc::c_int) {
    let Some(pgid) = session.process_group_id.filter(|pgid| *pgid > 1) else {
        return;
    };
    let rc = unsafe { libc::kill(-pgid, signal) };
    if rc == 0 {
        return;
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() != Some(libc::ESRCH) {
        eprintln!(
            "failed to signal unreaped process group {} for {}: {}",
            pgid,
            session.name(),
            err
        );
    }
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
                pgid,
                session.name(),
                err
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
                    pid,
                    session.name(),
                    err
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
        pgid,
        session.name(),
        session.process_id
    );
    None
}

fn process_group_still_owns_child(process_id: Option<u32>, pgid: i32) -> bool {
    let Some(pid) = process_id.and_then(|pid| libc::pid_t::try_from(pid).ok()) else {
        return false;
    };
    unsafe { libc::getpgid(pid) == pgid }
}

enum ProcessGroupWait {
    Exited,
    LeaderObserved,
    TimedOut,
}

fn wait_for_process_group_exit_or_leader_observed(
    session: &Session,
    timeout: Duration,
) -> ProcessGroupWait {
    if session.leader_exit_observed.load(Ordering::SeqCst) {
        return ProcessGroupWait::LeaderObserved;
    }
    let Some(pgid) = verified_session_process_group_id(session) else {
        thread::sleep(timeout);
        return if session.leader_exit_observed.load(Ordering::SeqCst) {
            ProcessGroupWait::LeaderObserved
        } else {
            ProcessGroupWait::TimedOut
        };
    };
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if session.leader_exit_observed.load(Ordering::SeqCst) {
            return ProcessGroupWait::LeaderObserved;
        }
        let rc = unsafe { libc::kill(-pgid, 0) };
        if rc != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            return ProcessGroupWait::Exited;
        }
        thread::sleep(Duration::from_millis(25));
    }
    if session.leader_exit_observed.load(Ordering::SeqCst) {
        ProcessGroupWait::LeaderObserved
    } else {
        ProcessGroupWait::TimedOut
    }
}

fn issue_input_capability(
    state: &Arc<State>,
    target: &str,
    byte_budget: u64,
) -> Result<IssueInputCapabilityResult> {
    if !(1..=MAX_INPUT_CAPABILITY_BUDGET).contains(&byte_budget) {
        bail!("input capability byte budget must be between 1 and {MAX_INPUT_CAPABILITY_BUDGET}");
    }
    let normalized = normalize_target(target);
    let sessions = lock(&state.sessions);
    let session = resolve_session_locked(&sessions, &normalized).ok_or_else(|| {
        anyhow!(
            "no such lterm session or pane: {}. Run `lterm list` to see active sessions or `lterm start <NAME>` to create one.",
            sanitized_preview(&normalized)
        )
    })?;
    if !session.alive.load(Ordering::SeqCst) {
        bail!("session is not alive: {}", sanitized_preview(&normalized));
    }

    let mut registry = lock(&state.input_capabilities);
    registry.grants.retain(|_, grant| {
        grant
            .session
            .upgrade()
            .is_some_and(|candidate| candidate.alive.load(Ordering::SeqCst))
    });
    if registry.grants.len() >= MAX_INPUT_CAPABILITIES {
        bail!("too many outstanding input capabilities");
    }
    let session_grants = registry
        .grants
        .values()
        .filter(|grant| grant.session_id == session.id)
        .count();
    if session_grants >= MAX_INPUT_CAPABILITIES_PER_SESSION {
        bail!("too many outstanding input capabilities for session");
    }
    let token = loop {
        let candidate = CapabilityToken::new_random();
        if !registry.grants.contains_key(&candidate) {
            break candidate;
        }
    };
    registry.grants.insert(
        token.clone(),
        InputCapabilityGrant {
            session_id: session.id.clone(),
            session: Arc::downgrade(&session),
            remaining_attempt_bytes: byte_budget,
        },
    );
    drop(registry);
    drop(sessions);
    Ok(IssueInputCapabilityResult { token, byte_budget })
}

fn apply_capability_input(
    state: &Arc<State>,
    token: &CapabilityToken,
    data: Vec<u8>,
) -> Result<()> {
    if data.is_empty() || data.len() > MAX_CAPABILITY_INPUT_BYTES {
        bail!("capability input rejected");
    }
    let data_len = u64::try_from(data.len()).map_err(|_| anyhow!("capability input rejected"))?;
    let session = {
        let mut registry = lock(&state.input_capabilities);
        let Some(grant) = registry.grants.get_mut(token) else {
            bail!("capability input rejected");
        };
        let Some(session) = grant.session.upgrade() else {
            registry.grants.remove(token);
            bail!("capability input rejected");
        };
        if !session.alive.load(Ordering::SeqCst) {
            registry.grants.remove(token);
            bail!("capability input rejected");
        }
        if data_len > grant.remaining_attempt_bytes {
            bail!("capability input rejected");
        }
        grant.remaining_attempt_bytes -= data_len;
        if grant.remaining_attempt_bytes == 0 {
            registry.grants.remove(token);
        }
        session
    };
    lock(&session.writer)
        .write_all(&data)
        .context("capability input write failed")
}

fn revoke_input_capability(state: &Arc<State>, token: &CapabilityToken) {
    lock(&state.input_capabilities).grants.remove(token);
}

fn resolve_session(state: &Arc<State>, target: &str) -> Result<Arc<Session>> {
    let target = normalize_target(target);
    let sessions = lock(&state.sessions);
    if let Some(session) = resolve_session_locked(&sessions, &target) {
        return Ok(session);
    }
    bail!(
        "no such lterm session or pane: {}. Run `lterm list` to see active sessions or `lterm start <NAME>` to create one.",
        sanitized_preview(&target)
    )
}

/// Resolves a session target while the caller already holds `State.sessions`.
///
/// Target precedence matches the public resolver: explicit pane id, session
/// name, UUID session id, then bare pane number fallback.
fn resolve_session_locked(sessions: &SessionMaps, target: &str) -> Option<Arc<Session>> {
    if target.starts_with('%') {
        if let Some(session) = sessions.by_pane.get(target) {
            return Some(Arc::clone(session));
        }
    }
    if let Some(session) = sessions.by_name.get(target) {
        return Some(Arc::clone(session));
    }
    if let Some(session) = sessions.by_id.get(target) {
        return Some(Arc::clone(session));
    }
    if !target.starts_with('%') {
        let pane = format!("%{target}");
        if let Some(session) = sessions.by_pane.get(&pane) {
            return Some(Arc::clone(session));
        }
    }
    None
}

fn validate_session_name_syntax(name: &str) -> Result<()> {
    if name.trim().is_empty() {
        bail!("session name cannot be empty");
    }
    if name.len() > 128 {
        bail!("session name cannot exceed 128 bytes");
    }
    if name.starts_with('%') {
        bail!(
            "session name cannot look like a pane id: {}",
            sanitized_preview(name)
        );
    }
    if name.starts_with('-') {
        bail!(
            "session name cannot start with '-': {}",
            sanitized_preview(name)
        );
    }
    if session_name_looks_like_bare_pane_id(name) {
        bail!(
            "session name cannot look like a bare pane id: {}",
            sanitized_preview(name)
        );
    }
    if Uuid::parse_str(name).is_ok() {
        bail!(
            "session name cannot look like a UUID: {}",
            sanitized_preview(name)
        );
    }
    if !name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
    {
        bail!("session name may only contain ASCII letters, numbers, '.', '_' and '-'");
    }
    Ok(())
}

/// Returns true when a name would shadow lterm's bare pane-number target syntax.
fn session_name_looks_like_bare_pane_id(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|ch| ch.is_ascii_digit())
}

fn scrub_ambient_multiplexer_env(cmd: &mut CommandBuilder) {
    for key in AMBIENT_MULTIPLEXER_ENV {
        cmd.env_remove(key);
    }
    for (key, _) in std::env::vars_os() {
        if os_key_is_private_multiplexer_env(key.as_os_str()) {
            cmd.env_remove(key);
        }
    }
}

fn os_key_is_private_multiplexer_env(key: &std::ffi::OsStr) -> bool {
    os_key_eq_ignore_ascii_case(key, b"TMUX")
        || os_key_eq_ignore_ascii_case(key, b"TMUX_PANE")
        || os_key_eq_ignore_ascii_case(key, b"LTERM_CMUX_MANAGED_ATTACH")
        || os_key_starts_with_cmux_prefix(key)
}

fn os_key_eq_ignore_ascii_case(key: &std::ffi::OsStr, expected: &[u8]) -> bool {
    let bytes = key.as_bytes();
    bytes.len() == expected.len()
        && bytes
            .iter()
            .zip(expected.iter())
            .all(|(actual, expected)| actual.eq_ignore_ascii_case(expected))
}

fn os_key_starts_with_cmux_prefix(key: &std::ffi::OsStr) -> bool {
    let bytes = key.as_bytes();
    bytes.len() >= 5
        && bytes[0].eq_ignore_ascii_case(&b'C')
        && bytes[1].eq_ignore_ascii_case(&b'M')
        && bytes[2].eq_ignore_ascii_case(&b'U')
        && bytes[3].eq_ignore_ascii_case(&b'X')
        && bytes[4] == b'_'
}

const AMBIENT_MULTIPLEXER_ENV: &[&str] = &["TMUX", "TMUX_PANE", "LTERM_CMUX_MANAGED_ATTACH"];

fn scrub_ambient_child_color_policy_env(cmd: &mut CommandBuilder) {
    for key in CHILD_COLOR_POLICY_ENV {
        cmd.env_remove(key);
    }
}

fn sanitize_child_env(
    env: HashMap<String, String>,
    allow_cmux_context: bool,
) -> Result<HashMap<String, String>> {
    let mut safe = HashMap::with_capacity(env.len());
    for (key, value) in env {
        validate_env_key(&key)?;
        validate_env_value(&key, &value)?;
        if is_dangerous_env_key(&key) {
            bail!("refusing dangerous child environment variable: {key}");
        }
        if is_private_multiplexer_env_key(&key, allow_cmux_context) {
            bail!("refusing private child environment variable: {key}");
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

fn is_private_multiplexer_env_key(key: &str, allow_cmux_context: bool) -> bool {
    let upper = key.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "TMUX" | "TMUX_PANE" | "LTERM_CMUX_MANAGED_ATTACH"
    ) || (upper.starts_with("CMUX_")
        && (!allow_cmux_context || !is_allowed_child_cmux_env_key(key)))
}

fn is_allowed_child_cmux_env_key(key: &str) -> bool {
    CMUX_CONTEXT_ENV.contains(&key)
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
        paths::tmux_compat_socket_path()?.display(),
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

fn capture_bytes_from_ring(ring: &VecDeque<u8>, start: Option<i32>, end: Option<i32>) -> Vec<u8> {
    if ring.is_empty() {
        return Vec::new();
    }
    if start.is_some_and(|line| line < 0) || end.is_some_and(|line| line < 0) {
        let line_count = ring_line_count(ring);
        let first = capture_line_index(start.unwrap_or(0), line_count);
        if first >= line_count {
            return Vec::new();
        }
        let last = end.map(|line| capture_end_line_index(line, line_count));
        return copy_ring_lines(ring, first, last);
    }

    // Non-negative capture coordinates are absolute line indexes. Avoid the
    // extra line-count pass here so small bounded captures can stop as soon as
    // the requested inclusive range has been copied.
    let first = start.unwrap_or(0) as usize;
    let last = end.map(|line| line as usize);
    copy_ring_lines(ring, first, last)
}

fn capture_start_total_from_ring(ring: &VecDeque<u8>, total_bytes: u64, start: Option<i32>) -> u64 {
    if ring.is_empty() {
        return total_bytes;
    }
    let ring_start_total = total_bytes.saturating_sub(ring.len() as u64);
    let Some(start) = start else {
        return ring_start_total;
    };
    let line_count = ring_line_count(ring);
    let first = capture_line_index(start, line_count);
    if first >= line_count {
        return total_bytes;
    }
    ring_start_total.saturating_add(ring_byte_index_for_line(ring, first) as u64)
}

fn copy_ring_bytes_from_total(ring: &VecDeque<u8>, total_bytes: u64, start_total: u64) -> Vec<u8> {
    let ring_start_total = total_bytes.saturating_sub(ring.len() as u64);
    let start = start_total
        .saturating_sub(ring_start_total)
        .min(ring.len() as u64) as usize;
    ring.iter().skip(start).copied().collect()
}

fn ring_line_count(ring: &VecDeque<u8>) -> usize {
    if ring.is_empty() {
        return 0;
    }
    let newline_count = ring.iter().filter(|byte| **byte == b'\n').count();
    if ring.back() == Some(&b'\n') {
        newline_count
    } else {
        newline_count + 1
    }
}

fn copy_ring_lines(ring: &VecDeque<u8>, first: usize, last: Option<usize>) -> Vec<u8> {
    if let Some(last) = last {
        if last < first {
            return Vec::new();
        }
    }
    let mut out = Vec::new();
    let mut line_index = 0;
    for byte in ring.iter().copied() {
        // `last` is inclusive. Push the final line's terminating newline before
        // breaking, matching tmux-style capture-pane range semantics.
        if line_index >= first
            && match last {
                Some(last) => line_index <= last,
                None => true,
            }
        {
            out.push(byte);
        }
        if byte == b'\n' {
            line_index += 1;
            if let Some(last) = last {
                if line_index > last {
                    break;
                }
            }
        }
    }
    out
}

fn ring_byte_index_for_line(ring: &VecDeque<u8>, target_line: usize) -> usize {
    if target_line == 0 {
        return 0;
    }
    let mut line_index = 0usize;
    for (byte_index, byte) in ring.iter().enumerate() {
        if *byte == b'\n' {
            line_index += 1;
            if line_index == target_line {
                return byte_index + 1;
            }
        }
    }
    ring.len()
}

fn capture_line_index(line: i32, line_count: usize) -> usize {
    if line < 0 {
        line_count.saturating_sub(line.unsigned_abs() as usize)
    } else {
        line as usize
    }
}

fn capture_end_line_index(line: i32, line_count: usize) -> usize {
    if line < 0 {
        line_count.saturating_sub(line.unsigned_abs() as usize)
    } else {
        (line as usize).min(line_count.saturating_sub(1))
    }
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

fn internal_test_degrade_terminal_parser() -> bool {
    #[cfg(debug_assertions)]
    {
        static DEGRADE_TERMINAL_PARSER: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *DEGRADE_TERMINAL_PARSER.get_or_init(|| {
            env_bool(INTERNAL_TEST_MODE_ENV) && env_bool(INTERNAL_TEST_DEGRADE_TERMINAL_PARSER_ENV)
        })
    }
    #[cfg(not(debug_assertions))]
    {
        false
    }
}

#[cfg(debug_assertions)]
fn env_bool(name: &str) -> bool {
    std::env::var(name).ok().is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

struct UmaskGuard {
    previous: mode_t,
}

impl UmaskGuard {
    fn set(mask: mode_t) -> Self {
        Self {
            previous: unsafe { libc::umask(mask) },
        }
    }
}

impl Drop for UmaskGuard {
    fn drop(&mut self) {
        unsafe {
            libc::umask(self.previous);
        }
    }
}

fn bind_private_socket(socket: &Path) -> Result<UnixListener> {
    let umask_guard = UmaskGuard::set(0o177);
    let listener = UnixListener::bind(socket).with_context(|| format!("bind {}", socket.display()));
    drop(umask_guard);
    listener
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
            match ping_socket(socket) {
                Ok(true) => bail!("lterm daemon already running at {}", socket.display()),
                Ok(false) => bail!(
                    "lterm daemon at {} returned an unsuccessful ping; refusing to remove socket",
                    socket.display()
                ),
                Err(err) if is_stale_socket_ping_error(&err) => {}
                Err(err) => {
                    return Err(err).with_context(|| {
                        format!(
                            "ping existing socket {} failed after connecting or with an unsafe error",
                            socket.display()
                        )
                    });
                }
            }
            fs::remove_file(socket)
                .with_context(|| format!("remove stale socket {}", socket.display()))?;
        }
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => return Err(err).with_context(|| format!("lstat {}", socket.display())),
    }
    Ok(())
}

fn is_stale_socket_ping_error(err: &anyhow::Error) -> bool {
    let Some(io_err) = err.downcast_ref::<std::io::Error>() else {
        return false;
    };
    matches!(
        io_err.kind(),
        ErrorKind::ConnectionRefused | ErrorKind::NotFound
    )
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
    target_os = "netbsd",
    target_os = "linux",
    test
))]
fn verify_peer_uid(peer_uid: u32, expected_uid: u32) -> Result<()> {
    if peer_uid != expected_uid {
        bail!("peer uid {peer_uid} does not match daemon uid {expected_uid}");
    }
    Ok(())
}

#[cfg(any(target_os = "linux", test))]
fn verify_linux_peercred_len(actual_len: usize, expected_len: usize) -> Result<()> {
    if actual_len < expected_len {
        bail!(
            "getsockopt(SO_PEERCRED) returned short credential length {actual_len}, expected at least {expected_len}"
        );
    }
    Ok(())
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
    // SAFETY: getpeereid(3) takes a valid socket fd from a live UnixStream and
    // two out-pointers we own; on error it sets errno and we read it via
    // std::io::Error::last_os_error.
    let rc = unsafe { getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    if rc != 0 {
        bail!("getpeereid failed: {}", std::io::Error::last_os_error());
    }
    // SAFETY: geteuid(2) is POSIX-required thread-safe and infallible.
    let expected = unsafe { geteuid() };
    verify_peer_uid(uid, expected)
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
    verify_linux_peercred_len(len as usize, std::mem::size_of::<UCred>())?;
    // SAFETY: geteuid(2) is POSIX-required thread-safe and infallible.
    let expected = unsafe { geteuid() };
    verify_peer_uid(cred.uid, expected)
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
    use super::{
        ALT_SCREEN_ENTER, AttachSubscriptionGuard, BACKPRESSURE_SEND_TIMEOUT, InputCapabilityGrant,
        MAX_INPUT_CAPABILITIES, MAX_INPUT_CAPABILITIES_PER_SESSION, MAX_TERMINAL_COLS,
        MAX_TERMINAL_ROWS, OutputChunk, OutputProgress, SUBSCRIBER_QUEUE_LIMIT, Session,
        SessionMetadata, State, Subscriber, TerminalPrefixTracker, WaitContainsScanner,
        apply_capability_input, broadcast_chunk, clamp_to_smallest, evict_disconnected_subscribers,
        forward_attach_output, handle_capability_channel, initial_pty_size, issue_input_capability,
        metadata_history, metadata_purge_history, metadata_step, os_key_is_private_multiplexer_env,
        os_key_starts_with_cmux_prefix, process_group_still_owns_child,
        read_request_frame_with_limit, read_request_frame_with_timeout, remove_session,
        rename_session, request_frame_from_chunk, revoke_input_capability, sanitize_child_env,
        sanitized_tail_for_needle, set_status_theme, validate_terminal_geometry,
        verify_linux_peercred_len, verify_peer_uid, wait_for_session_contains,
    };
    use crate::protocol::{
        CapabilityAction, CapabilityToken, MAX_METADATA_JOURNAL_ENTRIES, MetadataStepDirection,
        StatusTheme,
    };
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};
    use std::collections::{HashMap, VecDeque};
    use std::io::{BufRead, Read, Write};
    use std::os::unix::net::UnixStream;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Barrier, Condvar, Mutex, mpsc};
    use std::thread;
    use std::time::{Duration, Instant};
    use uuid::Uuid;

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

    #[test]
    fn peer_uid_check_accepts_same_uid() {
        verify_peer_uid(501, 501).expect("same uid should be accepted");
    }

    #[test]
    fn peer_uid_check_rejects_wrong_uid() {
        let err = verify_peer_uid(502, 501).expect_err("wrong uid should be rejected");
        assert!(
            err.to_string()
                .contains("peer uid 502 does not match daemon uid 501"),
            "wrong-uid error should identify both trust-boundary endpoints: {err:#}"
        );
    }

    #[test]
    fn linux_peercred_len_check_fails_closed_on_short_buffer() {
        verify_linux_peercred_len(12, 12).expect("exact SO_PEERCRED length should pass");
        verify_linux_peercred_len(16, 12).expect("longer SO_PEERCRED length should pass");
        let err =
            verify_linux_peercred_len(11, 12).expect_err("short SO_PEERCRED length must fail");
        assert!(
            err.to_string().contains("short credential length"),
            "unexpected short credential error: {err:#}"
        );
    }

    #[test]
    fn cmux_prefix_detection_is_case_insensitive_and_boundary_safe() {
        assert!(os_key_starts_with_cmux_prefix(std::ffi::OsStr::new(
            "CMUX_SOCKET_PATH"
        )));
        assert!(os_key_starts_with_cmux_prefix(std::ffi::OsStr::new(
            "cmux_socket_path"
        )));
        assert!(os_key_starts_with_cmux_prefix(std::ffi::OsStr::new(
            "CmUx_Surface_Id"
        )));
        assert!(!os_key_starts_with_cmux_prefix(std::ffi::OsStr::new(
            "CMUX"
        )));
        assert!(!os_key_starts_with_cmux_prefix(std::ffi::OsStr::new(
            "CMUX"
        )));
        assert!(!os_key_starts_with_cmux_prefix(std::ffi::OsStr::new(
            "CMUX-FOO"
        )));
    }

    #[test]
    fn private_multiplexer_env_detection_is_case_insensitive() {
        for key in [
            "TMUX",
            "tmux",
            "TmUx_PaNe",
            "lterm_cmux_managed_attach",
            "CMUX_SOCKET_PATH",
            "cmux_socket_path",
        ] {
            assert!(
                os_key_is_private_multiplexer_env(std::ffi::OsStr::new(key)),
                "{key} should be detected as private multiplexer env"
            );
        }
        for key in ["TMUX_EXTRA", "TMUXPANE", "CMUX", "LC_TERMINAL", "TERM"] {
            assert!(
                !os_key_is_private_multiplexer_env(std::ffi::OsStr::new(key)),
                "{key} should not be detected as private multiplexer env"
            );
        }
    }

    #[test]
    fn child_env_rejects_private_multiplexer_keys_but_allows_cmux_context() {
        for key in [
            "TMUX",
            "TMUX_PANE",
            "LTERM_CMUX_MANAGED_ATTACH",
            "CMUX_EXTRA_CONTEXT",
        ] {
            let mut env = HashMap::new();
            env.insert(key.to_string(), "value".to_string());
            let err =
                sanitize_child_env(env, true).expect_err("private multiplexer key should fail");
            assert!(
                err.to_string()
                    .contains("refusing private child environment variable"),
                "unexpected error for {key}: {err:#}"
            );
        }

        let mut env = HashMap::new();
        env.insert(
            "CMUX_WORKSPACE_ID".to_string(),
            "workspace:current".to_string(),
        );
        env.insert("CMUX_SURFACE_ID".to_string(), "surface:current".to_string());
        env.insert("CMUX_WINDOW_ID".to_string(), "window:current".to_string());
        env.insert("CMUX_SOCKET_PATH".to_string(), "/tmp/cmux.sock".to_string());
        let plain_err = sanitize_child_env(env.clone(), false)
            .expect_err("plain sessions must reject even allowlisted CMUX context");
        assert!(
            plain_err
                .to_string()
                .contains("refusing private child environment variable"),
            "unexpected plain-session error: {plain_err:#}"
        );

        let safe = sanitize_child_env(env, true).expect("tmux cmux context allowlist should pass");
        assert_eq!(safe["CMUX_SURFACE_ID"], "surface:current");

        let mut codex_home_env = HashMap::new();
        codex_home_env.insert("CODEX_HOME".to_string(), "/tmp/codex-home".to_string());
        let safe = sanitize_child_env(codex_home_env.clone(), false)
            .expect("CODEX_HOME should remain an ordinary allowed child env key");
        assert_eq!(safe["CODEX_HOME"], "/tmp/codex-home");
        let safe = sanitize_child_env(codex_home_env, true)
            .expect("CODEX_HOME should remain ordinary even when tmux env is allowlisted");
        assert_eq!(safe["CODEX_HOME"], "/tmp/codex-home");

        let mut lowercase_env = HashMap::new();
        lowercase_env.insert("cmux_surface_id".to_string(), "surface:lower".to_string());
        let lowercase_err = sanitize_child_env(lowercase_env, true)
            .expect_err("tmux sessions must reject non-canonical CMUX key casing");
        assert!(
            lowercase_err
                .to_string()
                .contains("refusing private child environment variable"),
            "unexpected lowercase CMUX error: {lowercase_err:#}"
        );
    }

    #[test]
    fn request_chunk_parser_preserves_tail_from_same_read_buffer() {
        let mut bytes = Vec::new();

        let frame =
            request_frame_from_chunk(&mut bytes, b"{\"type\":\"Ping\"}\nBUFFERED_INPUT\n", 1024)
                .unwrap()
                .unwrap();

        assert_eq!(frame.line, "{\"type\":\"Ping\"}\n");
        assert_eq!(frame.buffered, b"BUFFERED_INPUT\n");
        assert!(bytes.is_empty());
    }

    #[test]
    fn request_reader_accepts_newline_terminated_header() {
        let (mut server_end, mut client_end) = UnixStream::pair().expect("unix stream pair");
        client_end
            .write_all(b"{\"type\":\"Ping\"}\n")
            .expect("write request");

        let frame =
            read_request_frame_with_timeout(&mut server_end, Duration::from_secs(1)).unwrap();

        assert_eq!(frame.line, "{\"type\":\"Ping\"}\n");
        assert!(frame.buffered.is_empty());
    }

    #[test]
    fn request_reader_uses_absolute_deadline_for_partial_headers() {
        let (mut server_end, mut client_end) = UnixStream::pair().expect("unix stream pair");
        client_end.write_all(b"{").expect("write partial request");

        let started = Instant::now();
        let err = read_request_frame_with_timeout(&mut server_end, Duration::from_millis(50))
            .unwrap_err();

        assert!(
            started.elapsed() < Duration::from_secs(1),
            "partial request should honor the absolute deadline"
        );
        assert!(
            err.to_string().contains("timed out before newline"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn request_reader_rejects_oversized_headers_without_newline() {
        let (mut server_end, mut client_end) = UnixStream::pair().expect("unix stream pair");
        client_end
            .write_all(b"abcdefghi")
            .expect("write oversized request");

        let err =
            read_request_frame_with_limit(&mut server_end, Duration::from_secs(1), 8).unwrap_err();

        assert!(
            err.to_string().contains("request exceeded 8 bytes"),
            "unexpected error: {err:#}"
        );
    }

    /// shutdown 호출 횟수를 세는 카운터를 가진 테스트용 Subscriber.
    /// `tx` 의 receiver는 보관하지 않으므로 broadcast 가 실제로 일어나는 시나리오는
    /// 검증하지 않는다 — 이 helper 의 책임은 evict 시 shutdown 훅이 정확히 호출되는지.
    /// rows/cols 는 evict 테스트와 무관하므로 24×80 default 로 둔다 — clamp 정책
    /// 단위 테스트는 별도 helper `geom_subscriber` 를 사용한다.
    fn test_subscriber(id: u64, calls: Arc<AtomicU32>) -> Subscriber {
        let (tx, _rx) = mpsc::sync_channel(1);
        let calls_for_closure = Arc::clone(&calls);
        Subscriber {
            id,
            tx,
            on_evict: Arc::new(move || {
                calls_for_closure.fetch_add(1, Ordering::SeqCst);
            }),
            rows: 24,
            cols: 80,
        }
    }

    /// clamp_to_smallest 단위 테스트용 Subscriber. evict 카운터 없이 geometry 만
    /// 의미가 있다 — on_evict 는 호출되지 않으므로 빈 closure 로 둔다.
    fn geom_subscriber(id: u64, rows: u16, cols: u16) -> Subscriber {
        let (tx, _rx) = mpsc::sync_channel(1);
        Subscriber {
            id,
            tx,
            on_evict: Arc::new(|| {}),
            rows,
            cols,
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

    #[test]
    fn attach_output_forwarder_reports_write_failure() {
        let (server_end, client_end) = UnixStream::pair().expect("unix stream pair");
        drop(client_end);
        let (tx, rx) = mpsc::sync_channel(1);
        tx.send(Arc::from(&b"hello"[..]))
            .expect("queue output chunk");
        drop(tx);

        assert!(
            forward_attach_output(server_end, rx),
            "dropped peer should surface as an attach output failure"
        );
    }

    #[test]
    fn attach_output_forwarder_shutdowns_peer_when_channel_closes() {
        let (server_end, mut client_end) = UnixStream::pair().expect("unix stream pair");
        let (tx, rx) = mpsc::sync_channel(1);
        drop(tx);

        assert!(
            !forward_attach_output(server_end, rx),
            "closed channel without write error is a clean output-drain path"
        );
        let mut byte = [0_u8; 1];
        assert_eq!(
            client_end.read(&mut byte).expect("read peer eof"),
            0,
            "forwarder must shutdown the socket so the peer wakes up"
        );
    }

    /// 빈 리스트는 None — 호출자가 "잔여 attach 가 없으니 PTY 를 그대로 두자" 분기를
    /// 자연스럽게 표현하도록 한다 (PR #15: detach 후 마지막 사이즈 보존 정책의 기반).
    #[test]
    fn clamp_to_smallest_returns_none_for_empty_list() {
        let subs: Vec<Subscriber> = Vec::new();
        assert_eq!(clamp_to_smallest(&subs), None);
    }

    /// 단일 subscriber 면 그대로 반환. clamp 의 항등원 케이스.
    #[test]
    fn clamp_to_smallest_returns_single_subscriber_dims() {
        let subs = vec![geom_subscriber(1, 30, 100)];
        assert_eq!(clamp_to_smallest(&subs), Some((30, 100)));
    }

    /// rows/cols 가 서로 어긋난 두 client — 컴포넌트별 min 을 따로 잡는다.
    /// 한 클라이언트는 rows 가 더 좁고 cols 는 더 넓고, 다른 쪽은 rows 가 더 넓고
    /// cols 는 더 좁은 케이스. PTY 는 양쪽 모두 안전하게 표시되도록 컴포넌트별
    /// 가장 좁은 값을 따라가야 한다.
    #[test]
    fn clamp_to_smallest_picks_componentwise_min_of_two() {
        let subs = vec![
            geom_subscriber(1, 24, 200), // 좁은 rows, 넓은 cols
            geom_subscriber(2, 60, 80),  // 넓은 rows, 좁은 cols
        ];
        assert_eq!(clamp_to_smallest(&subs), Some((24, 80)));
    }

    /// 셋 이상이면 가장 좁은 값을 따라간다. desktop+tablet+mobile 시나리오의
    /// canonical assertion.
    #[test]
    fn clamp_to_smallest_picks_narrowest_of_three() {
        let subs = vec![
            geom_subscriber(1, 60, 200), // wide desktop
            geom_subscriber(2, 40, 120), // mid tablet
            geom_subscriber(3, 24, 80),  // narrow mobile
        ];
        assert_eq!(clamp_to_smallest(&subs), Some((24, 80)));
    }

    /// PR #15 quad-review HIGH 후속(#1): geometry_apply 락이 "per-client geometry
    /// 변경 → clamp 결정 → PTY 사이즈 갱신" 을 단일 critical section 으로 묶지 않으면,
    /// narrow client 의 Resize 와 narrow client 의 unsubscribe 가 인터리빙되어
    /// 살아있는 attach (wide) 가 narrow 한 PTY 사이즈에 묶이는 race 가 발생한다.
    ///
    /// 본 테스트는 실제 `Session` 인스턴스 (real PTY backed) 없이도 같은
    /// linearizability 불변식을 직접 모델링한다 — `Mutex<Vec<Subscriber>>` 와
    /// `Mutex<(u16, u16)>` 를 PTY 사이즈 캐시의 stand-in 으로 두고, 모든 mutate
    /// 경로가 별도의 `geometry_apply: Mutex<()>` 를 가장 먼저 잡도록 한다.
    /// 100 회 stress 루프 동안 두 스레드가 Barrier 로 동시 진입한 뒤, race 가
    /// 막혔는지 확인하는 invariant: "최종 PTY 사이즈는 최종 subscriber 의 geometry
    /// 와 일치해야 한다" (남은 단일 attach 의 사이즈를 PTY 가 따라가는 정책).
    ///
    /// race 가 막히지 않으면 narrow 가 unsubscribe 된 뒤 wide 만 남았는데도 PTY 가
    /// (24, 80) 으로 남는 케이스가 일정 확률로 관측된다. 본 테스트는 그 stale 상태가
    /// **단 한 번도** 발생하지 않아야 한다고 강제한다.
    #[test]
    fn concurrent_resize_then_detach_does_not_leave_stale_pty_size() {
        use std::sync::Barrier;
        use std::sync::Mutex;

        const ITERATIONS: usize = 100;
        const WIDE: (u16, u16) = (40, 152);
        const NARROW: (u16, u16) = (24, 80);

        for _ in 0..ITERATIONS {
            let geometry_apply = Arc::new(Mutex::new(()));
            let subscribers: Arc<Mutex<Vec<Subscriber>>> = Arc::new(Mutex::new(vec![
                geom_subscriber(1, WIDE.0, WIDE.1),
                geom_subscriber(2, NARROW.0, NARROW.1),
            ]));
            let pty_size = Arc::new(Mutex::new(NARROW)); // 시작 시 이미 clamp 가 적용됐다고 가정

            let barrier = Arc::new(Barrier::new(2));

            // Thread A: narrow client 가 자기 사이즈를 (24, 80) 으로 갱신 후 clamp.
            // (현실 시나리오에서는 사이즈가 이미 같지만, race 윈도우를 만들기 위해
            //  명시적 갱신 → apply 호출의 두 단계를 거친다.)
            let ga_a = Arc::clone(&geometry_apply);
            let subs_a = Arc::clone(&subscribers);
            let pty_a = Arc::clone(&pty_size);
            let bar_a = Arc::clone(&barrier);
            let thread_a = std::thread::spawn(move || {
                bar_a.wait();
                let _g = ga_a.lock().expect("lock geometry_apply (A)");
                {
                    let mut subs = subs_a.lock().expect("lock subscribers (A)");
                    if let Some(narrow) = subs.iter_mut().find(|s| s.id == 2) {
                        narrow.rows = NARROW.0;
                        narrow.cols = NARROW.1;
                    }
                }
                let target = clamp_to_smallest(&subs_a.lock().expect("lock subscribers (A2)"));
                if let Some((rows, cols)) = target {
                    *pty_a.lock().expect("lock pty (A)") = (rows, cols);
                }
            });

            // Thread B: narrow client 가 unsubscribe → re-clamp.
            let ga_b = Arc::clone(&geometry_apply);
            let subs_b = Arc::clone(&subscribers);
            let pty_b = Arc::clone(&pty_size);
            let bar_b = Arc::clone(&barrier);
            let thread_b = std::thread::spawn(move || {
                bar_b.wait();
                let _g = ga_b.lock().expect("lock geometry_apply (B)");
                {
                    let mut subs = subs_b.lock().expect("lock subscribers (B)");
                    subs.retain(|s| s.id != 2);
                }
                let target = clamp_to_smallest(&subs_b.lock().expect("lock subscribers (B2)"));
                if let Some((rows, cols)) = target {
                    *pty_b.lock().expect("lock pty (B)") = (rows, cols);
                }
            });

            thread_a.join().expect("thread A");
            thread_b.join().expect("thread B");

            // 최종 상태: narrow 가 unsubscribe 되었으므로 wide 만 남고, PTY 사이즈는
            // wide 와 일치해야 한다. geometry_apply 락이 두 스레드를 직렬화하므로
            // 어떤 인터리빙으로 진행되든 마지막 critical section 의 결과가 PTY 에
            // 박혀야 한다 — 그리고 두 critical section 모두 narrow 가 빠졌든 안 빠졌든
            // 끝낼 때 clamp 를 다시 계산하므로, 최종 결과는 항상 살아있는 attach 의
            // geometry 와 정합한다.
            let subs = subscribers.lock().expect("final subs");
            let pty = pty_size.lock().expect("final pty");
            assert_eq!(subs.len(), 1, "narrow subscriber should be removed");
            assert_eq!(subs[0].id, 1);
            // 살아있는 attach 의 사이즈와 PTY 사이즈가 정합해야 한다.
            assert_eq!(
                (subs[0].rows, subs[0].cols),
                *pty,
                "PTY size must match the surviving subscriber's geometry; race left a stale clamp"
            );
        }
    }

    /// PR #16: 단위테스트용 minimal Session 생성. 진짜 PTY 와 child 를 띄워야
    /// `Session` 구조체의 trait object 필드들을 채울 수 있지만, 본 헬퍼는 child 가
    /// 그냥 잠들어 있게 두고 PTY reader 스레드도 띄우지 않는다 — 본 PR 테스트가
    /// 검증하려는 것은 `subscribe_with_snapshot`/`append_output` 의 채널 기반 의미
    /// 이지 PTY 실제 동작이 아니다. 호출자는 명시적으로 `terminate` 같은 정리를 할
    /// 필요가 없도록 child 가 자동 종료되는 짧은 명령 (`true`) 을 띄운다.
    fn build_test_session(name: &str) -> Arc<Session> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty for test session");
        // `true` 는 즉시 종료되지만 PTY 마스터/슬레이브 fd 는 살아 있어 Session 구조에
        // 필요한 trait object 들을 채울 수 있다. process_id 는 보장되지 않으므로 None
        // 으로 두어도 본 단위테스트가 사용하는 어떤 경로도 process_id 에 의존하지 않는다.
        let true_path = ["/bin/true", "/usr/bin/true"]
            .into_iter()
            .find(|path| std::path::Path::new(path).is_file())
            .expect("absolute true for test session");
        let mut cmd = CommandBuilder::new(true_path);
        cmd.cwd(std::env::current_dir().expect("test session cwd"));
        let child = pair
            .slave
            .spawn_command(cmd)
            .expect("spawn `true` for test session");
        let killer = child.clone_killer();
        drop(pair.slave);
        let writer = pair.master.take_writer().expect("take pty writer");
        Arc::new(Session {
            id: Uuid::new_v4().to_string(),
            metadata: Mutex::new(SessionMetadata::new(name.to_string(), None)),
            pane_id: format!("%test-{name}"),
            parent_pane_id: Mutex::new(None),
            parent_session_id: Mutex::new(None),
            parent_token: String::new(),
            command: "true".to_string(),
            cwd: ".".to_string(),
            agent_name: None,
            created_unix_ms: 0,
            process_id: None,
            process_group_id: None,
            child: Mutex::new(child),
            killer: Mutex::new(killer),
            master: Mutex::new(pair.master),
            writer: Mutex::new(writer),
            ring: Mutex::new(VecDeque::new()),
            terminal_screen: Mutex::new(vt100::Parser::new(24, 80, 0)),
            terminal_pending: Mutex::new(TerminalPrefixTracker::default()),
            terminal_normal_screen: Mutex::new(vt100::Parser::new(24, 80, 0).screen().clone()),
            terminal_parser_degraded: AtomicBool::new(false),
            terminal_parser_panic_on_next_update: AtomicBool::new(false),
            terminal_parser_panic_on_next_snapshot: AtomicBool::new(false),
            terminal_parser_panic_on_next_resize: AtomicBool::new(false),
            subscribers: Mutex::new(Vec::new()),
            output_state: Mutex::new(()),
            output_progress: (Mutex::new(OutputProgress::default()), Condvar::new()),
            backpressure_hook: Mutex::new(None),
            broadcast_order: Mutex::new(()),
            geometry_apply: Mutex::new(()),
            next_subscriber_id: AtomicU64::new(1),
            alive: AtomicBool::new(true),
            cleanup_started: AtomicBool::new(false),
            cleanup_completion: (Mutex::new(false), Condvar::new()),
            cleanup_complete: AtomicBool::new(false),
            leader_exit_observed: AtomicBool::new(false),
            leader_reaped: AtomicBool::new(false),
            unreaped_cleanup_started: AtomicBool::new(false),
            exit_code: AtomicI32::new(i32::MIN),
            rows: Mutex::new(24),
            cols: Mutex::new(80),
        })
    }

    fn install_test_capability(
        state: &Arc<State>,
        session: &Arc<Session>,
        remaining: u64,
    ) -> CapabilityToken {
        let token = CapabilityToken::new_random();
        super::lock(&state.input_capabilities).grants.insert(
            token.clone(),
            InputCapabilityGrant {
                session_id: session.id.clone(),
                session: Arc::downgrade(session),
                remaining_attempt_bytes: remaining,
            },
        );
        token
    }

    fn register_test_session(state: &Arc<State>, session: &Arc<Session>) {
        let mut sessions = super::lock(&state.sessions);
        sessions.by_name.insert(session.name(), Arc::clone(session));
        sessions
            .by_pane
            .insert(session.pane_id.clone(), Arc::clone(session));
        sessions
            .by_id
            .insert(session.id.clone(), Arc::clone(session));
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct MetadataFiveState {
        indexed_name: Option<String>,
        current: crate::protocol::MetadataValue,
        entries: Vec<crate::protocol::MetadataJournalEntry>,
        cursor: usize,
        purge: crate::protocol::MetadataPurgeAggregate,
    }

    fn metadata_five_state(state: &Arc<State>, session: &Arc<Session>) -> MetadataFiveState {
        let sessions = super::lock(&state.sessions);
        let metadata = super::lock(&session.metadata);
        MetadataFiveState {
            indexed_name: sessions
                .by_name
                .iter()
                .find_map(|(name, candidate)| (candidate.id == session.id).then(|| name.clone())),
            current: metadata.current.clone(),
            entries: metadata.entries.clone(),
            cursor: metadata.cursor,
            purge: metadata.purge.clone(),
        }
    }

    #[test]
    fn metadata_thousand_mixed_operations_undo_to_baseline_and_redo_to_final() {
        let state = Arc::new(State::default());
        let session = build_test_session("metadata-baseline");
        register_test_session(&state, &session);
        let baseline = metadata_history(&state, &session.pane_id)
            .expect("baseline history")
            .current;

        for index in 0..500 {
            rename_session(&state, &session.pane_id, format!("metadata-{index}"))
                .expect("journal rename");
            let theme = if index % 2 == 0 {
                Some(StatusTheme::Blue)
            } else {
                Some(StatusTheme::Green)
            };
            set_status_theme(&state, &session.pane_id, theme).expect("journal theme");
        }
        let final_state = metadata_history(&state, &session.pane_id).expect("final history");
        assert_eq!(final_state.entries.len(), 1000);
        assert_eq!(final_state.cursor, 1000);

        for _ in 0..1000 {
            metadata_step(&state, &session.pane_id, MetadataStepDirection::Undo)
                .expect("undo exact operation");
        }
        let undone = metadata_history(&state, &session.pane_id).expect("undone history");
        assert_eq!(undone.current, baseline);
        assert_eq!(undone.cursor, 0);
        assert_eq!(undone.entries.len(), 1000);

        for _ in 0..1000 {
            metadata_step(&state, &session.pane_id, MetadataStepDirection::Redo)
                .expect("redo exact operation");
        }
        let redone = metadata_history(&state, &session.pane_id).expect("redone history");
        assert_eq!(redone.current, final_state.current);
        assert_eq!(redone.cursor, 1000);
        assert_eq!(redone.entries, final_state.entries);
    }

    #[test]
    fn metadata_cap_and_redo_branch_reject_without_truncation_but_noop_succeeds() {
        let state = Arc::new(State::default());
        let session = build_test_session("metadata-cap");
        register_test_session(&state, &session);
        for index in 0..MAX_METADATA_JOURNAL_ENTRIES {
            let theme = if index % 2 == 0 {
                Some(StatusTheme::Blue)
            } else {
                Some(StatusTheme::Green)
            };
            set_status_theme(&state, &session.pane_id, theme).expect("fill journal");
        }
        let full = metadata_five_state(&state, &session);
        let same_theme = full.current.status_theme;
        set_status_theme(&state, &session.pane_id, same_theme).expect("no-op at cap");
        assert_eq!(metadata_five_state(&state, &session), full);
        let err = rename_session(&state, &session.pane_id, "metadata-over-cap".to_string())
            .expect_err("1025th mutation must reject");
        assert!(err.to_string().contains("history is full"));
        assert_eq!(metadata_five_state(&state, &session), full);

        metadata_step(&state, &session.pane_id, MetadataStepDirection::Undo)
            .expect("create redo branch");
        let branched = metadata_five_state(&state, &session);
        rename_session(&state, &session.pane_id, branched.current.name.clone())
            .expect("rename no-op on redo branch");
        assert_eq!(metadata_five_state(&state, &session), branched);
        let err = set_status_theme(&state, &session.pane_id, Some(StatusTheme::Red))
            .expect_err("new mutation must not truncate redo branch");
        assert!(err.to_string().contains("redo entries"));
        assert_eq!(metadata_five_state(&state, &session), branched);
    }

    #[test]
    fn metadata_mismatch_conflict_and_reserved_name_fail_all_five_atomically() {
        let state = Arc::new(State::default());
        let session = build_test_session("metadata-source");
        register_test_session(&state, &session);
        rename_session(&state, &session.pane_id, "metadata-destination".to_string())
            .expect("initial rename");

        // Give the conflicting session a distinct immutable pane id, then
        // rename it into the undo destination. Reusing `metadata-source` at
        // construction time would also reuse the test helper's pane id and
        // accidentally replace the session under test in `by_pane`.
        let conflict = build_test_session("metadata-conflict");
        register_test_session(&state, &conflict);
        rename_session(&state, &conflict.pane_id, "metadata-source".to_string())
            .expect("occupy undo destination");
        let before_conflict = metadata_five_state(&state, &session);
        let err = metadata_step(&state, &session.pane_id, MetadataStepDirection::Undo)
            .expect_err("undo destination conflict must fail");
        assert!(err.to_string().contains("already exists"));
        assert_eq!(metadata_five_state(&state, &session), before_conflict);

        remove_session(&state, &conflict);
        {
            let mut sessions = super::lock(&state.sessions);
            sessions
                .reserved_names
                .insert("metadata-source".to_string());
        }
        let before_reserved = metadata_five_state(&state, &session);
        metadata_step(&state, &session.pane_id, MetadataStepDirection::Undo)
            .expect_err("reserved undo destination must fail");
        assert_eq!(metadata_five_state(&state, &session), before_reserved);
        super::lock(&state.sessions)
            .reserved_names
            .remove("metadata-source");

        {
            let mut metadata = super::lock(&session.metadata);
            metadata.current.status_theme = Some(StatusTheme::Amber);
        }
        let before_mismatch = metadata_five_state(&state, &session);
        let err = metadata_step(&state, &session.pane_id, MetadataStepDirection::Undo)
            .expect_err("whole-current mismatch must fail");
        assert!(err.to_string().contains("does not match"));
        assert_eq!(metadata_five_state(&state, &session), before_mismatch);
    }

    #[test]
    fn metadata_purge_gate_uuid_empty_and_overflow_fail_atomically() {
        let state = Arc::new(State::default());
        let session = build_test_session("metadata-purge");
        register_test_session(&state, &session);
        let id = session.id.clone();

        metadata_purge_history(&state, &session.pane_id, true, &id)
            .expect_err("empty history must fail");
        rename_session(
            &state,
            &session.pane_id,
            "metadata-purge-renamed".to_string(),
        )
        .expect("populate history");
        let populated = metadata_five_state(&state, &session);
        metadata_purge_history(&state, &session.pane_id, false, &id)
            .expect_err("missing irreversible gate must fail");
        assert_eq!(metadata_five_state(&state, &session), populated);
        metadata_purge_history(&state, &session.pane_id, true, &Uuid::new_v4().to_string())
            .expect_err("wrong exact UUID must fail");
        assert_eq!(metadata_five_state(&state, &session), populated);
        metadata_purge_history(&state, &session.pane_id, true, &id.to_uppercase())
            .expect_err("noncanonical UUID must fail");
        assert_eq!(metadata_five_state(&state, &session), populated);

        {
            super::lock(&session.metadata).purge.generation = u64::MAX;
        }
        let before_overflow = metadata_five_state(&state, &session);
        metadata_purge_history(&state, &session.pane_id, true, &id)
            .expect_err("purge generation overflow must fail");
        assert_eq!(metadata_five_state(&state, &session), before_overflow);
        super::lock(&session.metadata).purge.generation = 0;

        let current_before = super::lock(&session.metadata).current.clone();
        let purged = metadata_purge_history(&state, &session.pane_id, true, &id)
            .expect("valid irreversible purge");
        assert_eq!(purged.purged_entries, 1);
        assert_eq!(purged.current, current_before);
        let after = metadata_history(&state, &session.pane_id).expect("history after purge");
        assert!(after.entries.is_empty());
        assert_eq!(after.cursor, 0);
        assert_eq!(after.current, current_before);
        assert_eq!(after.purge.generation, 1);
        assert_eq!(after.purge.purged_entries_total, 1);
        assert!(after.purge.last_purged_unix_ms.is_some());
    }

    #[test]
    fn metadata_concurrent_renames_linearize_and_same_destination_has_one_winner() {
        let state = Arc::new(State::default());
        let session = build_test_session("metadata-concurrent");
        register_test_session(&state, &session);
        let barrier = Arc::new(Barrier::new(9));
        let mut handles = Vec::new();
        for index in 0..8 {
            let state = Arc::clone(&state);
            let pane = session.pane_id.clone();
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                barrier.wait();
                rename_session(&state, &pane, format!("metadata-linear-{index}"))
            }));
        }
        barrier.wait();
        for handle in handles {
            handle
                .join()
                .expect("rename thread")
                .expect("unique rename");
        }
        let history = metadata_history(&state, &session.pane_id).expect("linear history");
        assert_eq!(history.entries.len(), 8);
        assert_eq!(history.cursor, 8);
        for pair in history.entries.windows(2) {
            assert_eq!(pair[0].after, pair[1].before);
        }

        let other = build_test_session("metadata-other");
        register_test_session(&state, &other);
        let barrier = Arc::new(Barrier::new(3));
        let mut racers = Vec::new();
        for racer in [Arc::clone(&session), Arc::clone(&other)] {
            let state = Arc::clone(&state);
            let barrier = Arc::clone(&barrier);
            racers.push(thread::spawn(move || {
                barrier.wait();
                rename_session(&state, &racer.pane_id, "metadata-one-winner".to_string())
            }));
        }
        barrier.wait();
        let successes = racers
            .into_iter()
            .map(|handle| handle.join().expect("race thread"))
            .filter(Result::is_ok)
            .count();
        assert_eq!(successes, 1);
    }

    struct PartialThenFailWriter {
        first_write: usize,
        calls: usize,
        accepted: Arc<AtomicUsize>,
    }

    struct BlockingWriter {
        entered: mpsc::SyncSender<()>,
        release: mpsc::Receiver<()>,
    }

    impl Write for BlockingWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.entered.send(()).map_err(std::io::Error::other)?;
            self.release.recv().map_err(std::io::Error::other)?;
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Write for PartialThenFailWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.calls += 1;
            if self.calls == 1 && self.first_write > 0 {
                let accepted = bytes.len().min(self.first_write);
                self.accepted.fetch_add(accepted, Ordering::SeqCst);
                Ok(accepted)
            } else {
                Err(std::io::Error::other("injected writer failure"))
            }
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn capability_reservation_is_atomic_charged_and_revocation_is_idempotent() {
        let state = Arc::new(State::default());
        let session = build_test_session("capability-atomic");
        *super::lock(&session.writer) = Box::new(Vec::<u8>::new());
        let token = install_test_capability(&state, &session, 8);
        let barrier = Arc::new(Barrier::new(3));
        let mut workers = Vec::new();
        for _ in 0..2 {
            let state = Arc::clone(&state);
            let token = token.clone();
            let barrier = Arc::clone(&barrier);
            workers.push(thread::spawn(move || {
                barrier.wait();
                apply_capability_input(&state, &token, vec![b'x'; 6]).is_ok()
            }));
        }
        barrier.wait();
        let successes = workers
            .into_iter()
            .map(|worker| worker.join().expect("capability worker"))
            .filter(|succeeded| *succeeded)
            .count();
        assert_eq!(successes, 1, "8 bytes cannot authorize two 6-byte writes");
        assert_eq!(
            super::lock(&state.input_capabilities)
                .grants
                .get(&token)
                .map(|grant| grant.remaining_attempt_bytes),
            Some(2)
        );
        revoke_input_capability(&state, &token);
        revoke_input_capability(&state, &token);
        assert!(super::lock(&state.input_capabilities).grants.is_empty());
    }

    #[test]
    fn capability_teardown_purges_grants_and_dead_or_unknown_tokens_are_generic() {
        let state = Arc::new(State::default());
        let session = build_test_session("capability-teardown");
        {
            let mut sessions = super::lock(&state.sessions);
            sessions
                .by_name
                .insert(session.name(), Arc::clone(&session));
            sessions
                .by_pane
                .insert(session.pane_id.clone(), Arc::clone(&session));
            sessions
                .by_id
                .insert(session.id.clone(), Arc::clone(&session));
        }
        let token = install_test_capability(&state, &session, 8);
        remove_session(&state, &session);
        assert!(super::lock(&state.input_capabilities).grants.is_empty());
        let error = apply_capability_input(&state, &token, b"x".to_vec())
            .expect_err("removed grant must fail")
            .to_string();
        assert_eq!(error, "capability input rejected");
        assert!(!error.contains(token.as_str()));
    }

    #[test]
    fn capability_partial_and_full_write_failures_charge_the_full_attempt() {
        for first_write in [0, 2] {
            let state = Arc::new(State::default());
            let session = build_test_session(&format!("capability-write-fail-{first_write}"));
            let accepted = Arc::new(AtomicUsize::new(0));
            *super::lock(&session.writer) = Box::new(PartialThenFailWriter {
                first_write,
                calls: 0,
                accepted: Arc::clone(&accepted),
            });
            let token = install_test_capability(&state, &session, 8);
            let error = apply_capability_input(&state, &token, b"abcd".to_vec())
                .expect_err("injected writer must fail")
                .to_string();
            assert!(error.contains("capability input write failed"));
            assert_eq!(accepted.load(Ordering::SeqCst), first_write);
            assert_eq!(
                super::lock(&state.input_capabilities)
                    .grants
                    .get(&token)
                    .map(|grant| grant.remaining_attempt_bytes),
                Some(4),
                "full four-byte attempt must be charged without refund"
            );
        }
    }

    #[test]
    fn capability_issuance_enforces_per_session_and_global_caps() {
        let state = Arc::new(State::default());
        let session = build_test_session("capability-caps");
        {
            let mut sessions = super::lock(&state.sessions);
            sessions
                .by_name
                .insert(session.name(), Arc::clone(&session));
            sessions
                .by_pane
                .insert(session.pane_id.clone(), Arc::clone(&session));
            sessions
                .by_id
                .insert(session.id.clone(), Arc::clone(&session));
        }
        for _ in 0..MAX_INPUT_CAPABILITIES_PER_SESSION {
            issue_input_capability(&state, &session.pane_id, 1)
                .expect("grant below per-session cap");
        }
        assert!(
            issue_input_capability(&state, &session.pane_id, 1)
                .expect_err("per-session cap must reject")
                .to_string()
                .contains("for session")
        );

        super::lock(&state.input_capabilities).grants.clear();
        {
            let mut registry = super::lock(&state.input_capabilities);
            for index in 0..MAX_INPUT_CAPABILITIES {
                registry.grants.insert(
                    CapabilityToken::new_random(),
                    InputCapabilityGrant {
                        session_id: format!("other-{index}"),
                        session: Arc::downgrade(&session),
                        remaining_attempt_bytes: 1,
                    },
                );
            }
        }
        assert!(
            issue_input_capability(&state, &session.pane_id, 1)
                .expect_err("global cap must reject")
                .to_string()
                .contains("too many outstanding input capabilities")
        );
    }

    #[test]
    fn capability_reservation_before_revoke_may_finish_but_later_use_fails() {
        let state = Arc::new(State::default());
        let session = build_test_session("capability-revoke-linearization");
        let (entered_tx, entered_rx) = mpsc::sync_channel(0);
        let (release_tx, release_rx) = mpsc::sync_channel(0);
        *super::lock(&session.writer) = Box::new(BlockingWriter {
            entered: entered_tx,
            release: release_rx,
        });
        let token = install_test_capability(&state, &session, 8);
        let worker_state = Arc::clone(&state);
        let worker_token = token.clone();
        let worker = thread::spawn(move || {
            apply_capability_input(&worker_state, &worker_token, b"abcd".to_vec())
        });
        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("writer entered after reservation");
        revoke_input_capability(&state, &token);
        release_tx.send(()).expect("release writer");
        worker
            .join()
            .expect("reserved writer thread")
            .expect("pre-revoke reservation may finish");
        assert!(apply_capability_input(&state, &token, b"x".to_vec()).is_err());
    }

    #[test]
    fn capability_issue_and_teardown_follow_sessions_then_registry_order() {
        let state = Arc::new(State::default());
        let session = build_test_session("capability-issue-teardown");
        register_test_session(&state, &session);
        let mut sessions = super::lock(&state.sessions);
        let worker_state = Arc::clone(&state);
        let target = session.pane_id.clone();
        let (started_tx, started_rx) = mpsc::sync_channel(0);
        let worker = thread::spawn(move || {
            started_tx.send(()).expect("signal issue attempt");
            issue_input_capability(&worker_state, &target, 8)
        });
        started_rx.recv().expect("issue worker started");
        sessions.by_name.remove(&session.name());
        sessions.by_pane.remove(&session.pane_id);
        sessions.by_id.remove(&session.id);
        super::lock(&state.input_capabilities)
            .grants
            .retain(|_, grant| grant.session_id != session.id);
        drop(sessions);
        assert!(
            worker
                .join()
                .expect("issue worker")
                .expect_err("teardown linearized before issue")
                .to_string()
                .contains("no such lterm session")
        );
        assert!(super::lock(&state.input_capabilities).grants.is_empty());
    }

    #[test]
    fn capability_reservation_before_teardown_may_finish_but_later_use_fails() {
        let state = Arc::new(State::default());
        let session = build_test_session("capability-teardown-linearization");
        register_test_session(&state, &session);
        let (entered_tx, entered_rx) = mpsc::sync_channel(0);
        let (release_tx, release_rx) = mpsc::sync_channel(0);
        *super::lock(&session.writer) = Box::new(BlockingWriter {
            entered: entered_tx,
            release: release_rx,
        });
        let token = install_test_capability(&state, &session, 8);
        let worker_state = Arc::clone(&state);
        let worker_token = token.clone();
        let worker = thread::spawn(move || {
            apply_capability_input(&worker_state, &worker_token, b"abcd".to_vec())
        });
        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("writer entered after reservation");
        remove_session(&state, &session);
        release_tx.send(()).expect("release writer");
        worker
            .join()
            .expect("reserved writer thread")
            .expect("pre-teardown reservation may finish");
        assert!(apply_capability_input(&state, &token, b"x".to_vec()).is_err());
        assert!(super::lock(&state.input_capabilities).grants.is_empty());
    }

    #[test]
    fn capability_rename_and_pane_reuse_never_migrate_grant() {
        let state = Arc::new(State::default());
        let session = build_test_session("capability-original");
        *super::lock(&session.writer) = Box::new(Vec::<u8>::new());
        register_test_session(&state, &session);
        let issued = issue_input_capability(&state, &session.pane_id, 8)
            .expect("issue capability before rename");
        rename_session(&state, &session.pane_id, "capability-renamed".to_string())
            .expect("rename original session");
        apply_capability_input(&state, &issued.token, b"abcd".to_vec())
            .expect("rename keeps immutable-session grant valid");
        let old_pane = session.pane_id.clone();
        remove_session(&state, &session);

        let replacement = build_test_session("capability-replacement");
        *super::lock(&replacement.writer) = Box::new(Vec::<u8>::new());
        {
            let mut sessions = super::lock(&state.sessions);
            sessions
                .by_name
                .insert("capability-original".to_string(), Arc::clone(&replacement));
            sessions.by_pane.insert(old_pane, Arc::clone(&replacement));
            sessions
                .by_id
                .insert(replacement.id.clone(), Arc::clone(&replacement));
        }
        assert!(apply_capability_input(&state, &issued.token, b"x".to_vec()).is_err());
        assert!(super::lock(&state.input_capabilities).grants.is_empty());
    }

    #[test]
    fn malformed_sensitive_capability_frame_error_never_contains_payload_sentinel() {
        let state = Arc::new(State::default());
        let (server_stream, client_stream) = UnixStream::pair().expect("capability stream pair");
        let server = thread::spawn(move || {
            handle_capability_channel(state, server_stream, CapabilityAction::Input)
        });
        let mut reader = std::io::BufReader::new(client_stream);
        let mut ready = String::new();
        reader.read_line(&mut ready).expect("read capability ready");
        assert!(ready.contains("\"ready\":true"));
        assert!(ready.contains("\"protocol_version\":5"));
        const SENTINEL: &str = "MALFORMED_CAPABILITY_SECRET_SENTINEL";
        writeln!(
            reader.get_mut(),
            "{{\"type\":\"input\",\"token\":\"123e4567-e89b-42d3-a456-426614174000\",\"data\":{{\"secret\":\"{SENTINEL}\"}}}}"
        )
        .expect("write malformed sensitive frame");
        reader
            .get_mut()
            .shutdown(std::net::Shutdown::Write)
            .expect("finish malformed frame");
        let error = server
            .join()
            .expect("capability server thread")
            .expect_err("malformed frame must fail")
            .to_string();
        assert!(error.contains("invalid sensitive capability frame"));
        assert!(!error.contains(SENTINEL));
    }

    #[test]
    fn instrument_snapshot_tracks_exact_output_bytes_revision_and_close() {
        let session = build_test_session("instrument-progress");
        let initial = session.instrument_snapshot_relaxed();
        assert_eq!(initial.schema_version, "1.0");
        assert_eq!(initial.output_total_bytes, 0);
        assert_eq!(initial.output_revision, 0);
        assert!(!initial.output_closed);
        assert_eq!(initial.attached_clients, 0);
        assert_eq!((initial.rows, initial.cols), (24, 80));

        session.append_output(b"abc");
        let first = session.instrument_snapshot_relaxed();
        assert_eq!(first.output_total_bytes, 3);
        assert!(first.output_revision > initial.output_revision);
        assert!(!first.output_closed);

        session.append_output(&[0, 0xff, b'\n', 0x1b]);
        let second = session.instrument_snapshot_relaxed();
        assert_eq!(second.output_total_bytes, 7);
        assert!(second.output_revision > first.output_revision);
        assert!(!second.output_closed);

        session.mark_output_closed();
        let closed = session.instrument_snapshot_relaxed();
        assert_eq!(closed.output_total_bytes, 7);
        assert!(closed.output_revision > second.output_revision);
        assert!(closed.output_closed);
    }

    #[test]
    fn wait_contains_zero_timeout_checks_existing_snapshot_before_deadline() {
        let session = build_test_session("wait-zero-timeout");
        session.append_output(b"already-ready");

        let result = wait_for_session_contains(&session, "already-ready", None, Some(0))
            .expect("wait contains");

        assert!(
            result.matched,
            "zero-timeout wait must still inspect already captured output"
        );
        assert!(
            !result.timed_out,
            "pre-existing output match must not report timeout"
        );
        assert!(!result.exited, "live session should not be reported exited");
    }

    #[test]
    fn wait_contains_zero_timeout_without_match_reports_timeout() {
        let session = build_test_session("wait-zero-timeout-missing");

        let result = wait_for_session_contains(&session, "never-ready", None, Some(0))
            .expect("wait contains");

        assert!(!result.matched, "absent needle must not match");
        assert!(result.timed_out, "zero-timeout miss must report timeout");
        assert!(!result.exited, "live session should not be reported exited");
    }

    #[test]
    fn wait_contains_continuous_output_does_not_starve_timeout() {
        let session = build_test_session("wait-continuous-output");
        let running = Arc::new(AtomicBool::new(true));
        let running_for_writer = Arc::clone(&running);
        let session_for_writer = Arc::clone(&session);
        let writer = thread::spawn(move || {
            while running_for_writer.load(Ordering::SeqCst) {
                session_for_writer.append_output(b"noise\n");
                thread::sleep(Duration::from_millis(1));
            }
        });

        let started = Instant::now();
        let result = wait_for_session_contains(&session, "never-appears", None, Some(20))
            .expect("wait contains");
        running.store(false, Ordering::SeqCst);
        writer.join().expect("writer thread");

        assert!(!result.matched, "absent needle must not match");
        assert!(
            result.timed_out,
            "continuous progress must not hide timeout"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "timeout should stay bounded even with continuous output"
        );
    }

    #[test]
    fn wait_contains_scanner_matches_across_incremental_boundary() {
        let session = build_test_session("wait-incremental-boundary");
        let mut scanner = WaitContainsScanner::default();

        session.append_output(b"prefix nee");
        let first_progress = *super::lock(&session.output_progress.0);
        assert!(
            !scanner.contains(&session, first_progress.total_bytes, None, "needle"),
            "first partial chunk must not match"
        );

        session.append_output(b"dle suffix");
        let second_progress = *super::lock(&session.output_progress.0);
        assert!(
            scanner.contains(&session, second_progress.total_bytes, None, "needle"),
            "incremental scan must bridge the cached raw tail and newly appended bytes"
        );
    }

    #[test]
    fn wait_contains_tail_scanner_incremental_same_line_without_recapturing_tail() {
        let session = build_test_session("wait-tail-incremental-same-line");
        let mut scanner = WaitContainsScanner::default();

        session.append_output(b"old-line\nprefix nee");
        let first_progress = *super::lock(&session.output_progress.0);
        assert!(
            !scanner.contains(&session, first_progress.total_bytes, Some(-1), "needle"),
            "first tail scan sees only a partial match"
        );
        assert_eq!(
            scanner.full_scan_count, 1,
            "initial tail search needs one bounded full scan"
        );

        session.append_output(b"dle suffix");
        let second_progress = *super::lock(&session.output_progress.0);
        assert!(
            scanner.contains(&session, second_progress.total_bytes, Some(-1), "needle"),
            "tail scan must bridge cached sanitized suffix and newly appended bytes"
        );
        assert_eq!(
            scanner.full_scan_count, 1,
            "same-line tail progress should not recapture and resanitize the full tail"
        );
        assert_eq!(
            scanner.incremental_scan_count, 1,
            "same-line tail progress should scan only the appended delta"
        );
    }

    #[test]
    fn wait_contains_tail_scanner_rescans_when_tail_window_rolls() {
        let session = build_test_session("wait-tail-rollover-rescan");
        let mut scanner = WaitContainsScanner::default();

        session.append_output(b"one\nmissing");
        let first_progress = *super::lock(&session.output_progress.0);
        assert!(!scanner.contains(&session, first_progress.total_bytes, Some(-1), "needle"));
        assert_eq!(scanner.full_scan_count, 1);

        session.append_output(b"\ntwo\n");
        let second_progress = *super::lock(&session.output_progress.0);
        assert!(!scanner.contains(&session, second_progress.total_bytes, Some(-1), "needle"));
        assert_eq!(
            scanner.full_scan_count, 2,
            "tail rollover changes the capture start, so sanitizer state must reset"
        );
    }

    #[test]
    fn wait_contains_scanner_matches_split_utf8_needle_incrementally() {
        let session = build_test_session("wait-incremental-split-utf8");
        let mut scanner = WaitContainsScanner::default();
        let needle = "완료";
        let bytes = needle.as_bytes();

        session.append_output(&bytes[..1]);
        let first_progress = *super::lock(&session.output_progress.0);
        assert!(
            !scanner.contains(&session, first_progress.total_bytes, None, needle),
            "partial UTF-8 scalar must not be decoded as replacement during incremental scan"
        );

        session.append_output(&bytes[1..5]);
        let second_progress = *super::lock(&session.output_progress.0);
        assert!(
            !scanner.contains(&session, second_progress.total_bytes, None, needle),
            "scanner must keep the second partial scalar pending"
        );

        session.append_output(&bytes[5..]);
        let third_progress = *super::lock(&session.output_progress.0);
        assert!(
            scanner.contains(&session, third_progress.total_bytes, None, needle),
            "incremental scan must bridge UTF-8 scalars split across raw chunks"
        );
    }

    #[test]
    fn wait_contains_scanner_keeps_single_byte_needle_tail_bounded() {
        let session = build_test_session("wait-incremental-single-byte-tail");
        let mut scanner = WaitContainsScanner::default();

        session.append_output(&vec![b'x'; super::MAX_WAIT_CONTAINS_NEEDLE_BYTES * 4]);
        let first_progress = *super::lock(&session.output_progress.0);
        assert!(
            !scanner.contains(&session, first_progress.total_bytes, None, "a"),
            "absent one-byte needle must not match"
        );
        assert_eq!(
            scanner.sanitized_tail.len(),
            0,
            "single-byte needles need no overlap cache"
        );

        session.append_output(b"still-no-hit");
        let second_progress = *super::lock(&session.output_progress.0);
        assert!(
            !scanner.contains(&session, second_progress.total_bytes, None, "a"),
            "subsequent incremental misses must remain bounded"
        );
        assert_eq!(scanner.sanitized_tail.len(), 0);
    }

    #[test]
    fn sanitized_tail_for_single_byte_needle_is_empty() {
        assert_eq!(sanitized_tail_for_needle("large visible text", "a"), "");
        assert_eq!(sanitized_tail_for_needle("완료", "✅"), "");
    }

    #[test]
    fn wait_contains_scanner_does_not_match_hidden_long_osc_payload_incrementally() {
        let session = build_test_session("wait-incremental-long-osc");
        let mut scanner = WaitContainsScanner::default();
        let mut hidden = b"\x1b]52;c;".to_vec();
        hidden.extend(std::iter::repeat_n(
            b'x',
            super::MAX_PENDING_ESCAPE_BYTES + 64,
        ));
        hidden.extend_from_slice(b"needle-hidden");

        session.append_output(&hidden);
        let first_progress = *super::lock(&session.output_progress.0);
        assert!(
            !scanner.contains(&session, first_progress.total_bytes, None, "needle-hidden"),
            "full scan must strip unterminated OSC payload"
        );

        session.append_output(b"\x07visible-after-osc");
        let second_progress = *super::lock(&session.output_progress.0);
        assert!(
            !scanner.contains(&session, second_progress.total_bytes, None, "needle-hidden"),
            "incremental scan must not restart stateless sanitization inside hidden OSC payload"
        );

        session.append_output(b"\nneedle-hidden\n");
        let third_progress = *super::lock(&session.output_progress.0);
        assert!(
            scanner.contains(&session, third_progress.total_bytes, None, "needle-hidden"),
            "visible text after OSC termination should still match"
        );
    }

    #[test]
    fn wait_contains_scanner_does_not_reparse_completed_long_osc_tail_as_visible() {
        let session = build_test_session("wait-incremental-completed-long-osc");
        let mut scanner = WaitContainsScanner::default();
        let mut hidden = b"\x1b]52;c;".to_vec();
        hidden.extend(std::iter::repeat_n(
            b'x',
            super::MAX_PENDING_ESCAPE_BYTES + 64,
        ));
        hidden.extend_from_slice(b"needle-hidden\x07SAFE");

        session.append_output(&hidden);
        let first_progress = *super::lock(&session.output_progress.0);
        assert!(
            !scanner.contains(&session, first_progress.total_bytes, None, "needle-hidden"),
            "full scan must strip completed OSC payload"
        );

        session.append_output(b"\nmore-visible-text\n");
        let second_progress = *super::lock(&session.output_progress.0);
        assert!(
            !scanner.contains(&session, second_progress.total_bytes, None, "needle-hidden"),
            "incremental scan must not reparse a completed hidden OSC tail as visible text"
        );

        session.append_output(b"needle-hidden\n");
        let third_progress = *super::lock(&session.output_progress.0);
        assert!(
            scanner.contains(&session, third_progress.total_bytes, None, "needle-hidden"),
            "visible text after the completed OSC should still match"
        );
    }

    #[test]
    fn wait_contains_closed_progress_reports_exited_without_timeout() {
        let session = build_test_session("wait-closed");
        session.alive.store(false, Ordering::SeqCst);
        session.mark_output_closed();

        let result = wait_for_session_contains(&session, "missing", None, Some(1_000))
            .expect("wait contains");

        assert!(!result.matched);
        assert!(!result.timed_out);
        assert!(result.exited, "closed output should surface exited=true");
    }

    #[test]
    fn capture_bytes_applies_inclusive_end_line() {
        let session = build_test_session("capture-range");
        super::lock(&session.ring).extend(b"ONE\nTWO\nTHREE\n");

        assert_eq!(
            session.capture_bytes(Some(0), Some(1)),
            b"ONE\nTWO\n".to_vec()
        );
        assert_eq!(
            session.capture_bytes(Some(1), Some(99)),
            b"TWO\nTHREE\n".to_vec()
        );
        assert_eq!(
            session.capture_bytes(Some(-2), Some(-1)),
            b"TWO\nTHREE\n".to_vec()
        );
        assert_eq!(
            session.capture_bytes(None, Some(-2)),
            b"ONE\nTWO\n".to_vec()
        );
        assert!(
            session.capture_bytes(Some(2), Some(1)).is_empty(),
            "end before start should capture no lines"
        );
    }

    #[test]
    fn capture_bytes_handles_wrapped_ring_ranges() {
        let mut ring = VecDeque::with_capacity(16);
        ring.extend(b"DROP\nKEEP1\n");
        for _ in 0..5 {
            assert!(ring.pop_front().is_some());
        }
        ring.extend(b"KEEP2\nTAIL");
        assert!(
            !ring.as_slices().1.is_empty(),
            "test setup should force VecDeque wraparound"
        );

        assert_eq!(
            super::capture_bytes_from_ring(&ring, Some(1), Some(1)),
            b"KEEP2\n".to_vec()
        );
        assert_eq!(
            super::capture_bytes_from_ring(&ring, Some(-1), None),
            b"TAIL".to_vec()
        );
        assert_eq!(
            super::capture_bytes_from_ring(&ring, None, Some(-2)),
            b"KEEP1\nKEEP2\n".to_vec()
        );
        assert_eq!(
            super::capture_bytes_from_ring(&ring, Some(0), Some(0)),
            b"KEEP1\n".to_vec()
        );
        assert_eq!(
            super::capture_bytes_from_ring(&ring, Some(2), Some(2)),
            b"TAIL".to_vec()
        );
        assert!(
            super::capture_bytes_from_ring(&ring, Some(9), None).is_empty(),
            "positive start beyond scrollback should capture no lines"
        );
        assert!(
            super::capture_bytes_from_ring(&ring, Some(-1), Some(-2)).is_empty(),
            "negative end before start should capture no lines"
        );
        assert!(
            super::capture_bytes_from_ring(&VecDeque::new(), Some(0), Some(0)).is_empty(),
            "empty scrollback should capture no lines"
        );
    }

    #[test]
    fn terminate_tail_cleanup_waits_for_late_observed_leader() {
        let session = build_test_session("terminate-tail-cleanup");
        let session_for_waiter = Arc::clone(&session);
        let waiter = thread::spawn(move || {
            thread::sleep(Duration::from_millis(15));
            session_for_waiter
                .leader_exit_observed
                .store(true, Ordering::SeqCst);
        });

        let reap_guard = super::lock(&session.child);
        super::maybe_terminate_observed_unreaped_process_group(
            &session,
            Duration::from_millis(200),
            &reap_guard,
        );

        waiter.join().expect("leader-observed notifier panicked");
        assert!(
            session.unreaped_cleanup_started.load(Ordering::SeqCst),
            "late observed-but-unreaped leaders must run the residual process-group cleanup"
        );
    }

    #[test]
    fn terminate_unreaped_cleanup_skips_after_leader_reaped() {
        let session = build_test_session("terminate-reaped-skip");
        session.leader_exit_observed.store(true, Ordering::SeqCst);
        session.leader_reaped.store(true, Ordering::SeqCst);
        let observed_before = session.leader_exit_observed.load(Ordering::SeqCst);
        let reaped_before = session.leader_reaped.load(Ordering::SeqCst);

        let reap_guard = super::lock(&session.child);
        super::terminate_unreaped_process_group(&session, &reap_guard);

        assert!(
            !session.unreaped_cleanup_started.load(Ordering::SeqCst),
            "reaped leaders must not start residual stored-pgid cleanup"
        );
        assert_eq!(
            session.leader_exit_observed.load(Ordering::SeqCst),
            observed_before,
            "reaped cleanup skip should not mutate leader-exit observation"
        );
        assert_eq!(
            session.leader_reaped.load(Ordering::SeqCst),
            reaped_before,
            "reaped cleanup skip should not mutate leader-reaped state"
        );
    }

    #[test]
    fn terminate_unreaped_cleanup_starts_before_leader_reaped() {
        let session = build_test_session("terminate-unreaped-start");
        session.leader_exit_observed.store(true, Ordering::SeqCst);
        session.leader_reaped.store(false, Ordering::SeqCst);

        let reap_guard = super::lock(&session.child);
        super::terminate_unreaped_process_group(&session, &reap_guard);

        assert!(
            session.unreaped_cleanup_started.load(Ordering::SeqCst),
            "unreaped leaders should start residual stored-pgid cleanup"
        );
    }

    #[test]
    fn leader_exit_finalize_marks_not_alive_and_removes_indexes() {
        let state = Arc::new(super::State::default());
        let session = build_test_session("leader-exit-finalize");
        {
            let mut sessions = super::lock(&state.sessions);
            sessions
                .by_name
                .insert(session.name(), Arc::clone(&session));
            sessions
                .by_pane
                .insert(session.pane_id.clone(), Arc::clone(&session));
            sessions
                .by_id
                .insert(session.id.clone(), Arc::clone(&session));
        }

        super::finalize_session(&state, &session, super::SessionFinalizeReason::LeaderExited);

        assert!(
            !session.alive.load(Ordering::SeqCst),
            "leader-exit finalization must clear user-visible alive state"
        );
        assert!(
            session.cleanup_complete.load(Ordering::SeqCst),
            "leader-exit finalization must publish cleanup completion"
        );
        assert!(
            !session.unreaped_cleanup_started.load(Ordering::SeqCst),
            "leader-exit finalization should not run explicit terminate cleanup"
        );
        let sessions = super::lock(&state.sessions);
        assert!(!sessions.by_name.contains_key(&session.name()));
        assert!(!sessions.by_pane.contains_key(&session.pane_id));
        assert!(!sessions.by_id.contains_key(&session.id));
    }

    #[test]
    fn concurrent_finalize_waits_for_single_cleanup_completion() {
        use std::sync::Barrier;

        let state = Arc::new(super::State::default());
        let session = build_test_session("finalize-idempotent");
        {
            let mut sessions = super::lock(&state.sessions);
            let session_name = session.name();
            sessions
                .by_name
                .insert(session_name.clone(), Arc::clone(&session));
            sessions
                .by_pane
                .insert(session.pane_id.clone(), Arc::clone(&session));
            sessions
                .by_id
                .insert(session.id.clone(), Arc::clone(&session));
        }

        // Hold the lock that `close_subscribers` takes so the first finalizer
        // is definitely inside cleanup while the second caller enters the
        // cleanup_started/cleanup_complete wait path.
        let output_guard = super::lock(&session.output_state);
        let first_ready = Arc::new(Barrier::new(2));
        let state_for_first = Arc::clone(&state);
        let session_for_first = Arc::clone(&session);
        let first_ready_for_thread = Arc::clone(&first_ready);
        let first = thread::spawn(move || {
            first_ready_for_thread.wait();
            super::finalize_session(
                &state_for_first,
                &session_for_first,
                super::SessionFinalizeReason::LeaderExited,
            );
        });

        first_ready.wait();
        let deadline = Instant::now() + Duration::from_secs(1);
        while !session.cleanup_started.load(Ordering::SeqCst) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            session.cleanup_started.load(Ordering::SeqCst),
            "first finalizer should enter cleanup before the second caller"
        );

        let state_for_second = Arc::clone(&state);
        let session_for_second = Arc::clone(&session);
        let second = thread::spawn(move || {
            super::terminate_session(&state_for_second, &session_for_second);
        });

        thread::sleep(Duration::from_millis(25));
        assert!(
            !session.cleanup_complete.load(Ordering::SeqCst),
            "cleanup should still be blocked on the first finalizer"
        );
        drop(output_guard);

        first.join().expect("first finalizer panicked");
        second.join().expect("second finalizer panicked");
        assert!(!session.alive.load(Ordering::SeqCst));
        assert!(session.cleanup_complete.load(Ordering::SeqCst));
        let sessions = super::lock(&state.sessions);
        assert!(!sessions.by_name.contains_key(&session.name()));
        assert!(!sessions.by_pane.contains_key(&session.pane_id));
        assert!(!sessions.by_id.contains_key(&session.id));
    }

    #[test]
    fn attach_subscription_guard_unsubscribes_on_drop() {
        let session = build_test_session("attach-guard");
        let on_evict: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});
        let (subscriber_id, _rx) = session
            .subscribe_with_snapshot(24, 80, on_evict)
            .expect("subscribe test attach");
        assert_eq!(session.subscribers.lock().expect("subscribers").len(), 1);

        {
            let _guard = AttachSubscriptionGuard::new(Arc::clone(&session), subscriber_id);
        }

        assert!(
            session.subscribers.lock().expect("subscribers").is_empty(),
            "dropping the guard should remove the subscriber"
        );
    }

    fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    #[test]
    fn terminal_geometry_validation_accepts_common_size_and_rejects_edges() {
        validate_terminal_geometry("test", 24, 80).expect("common terminal size");
        assert!(
            validate_terminal_geometry("test", 0, 80)
                .expect_err("zero rows rejected")
                .to_string()
                .contains("at least 1"),
        );
        assert!(
            validate_terminal_geometry("test", MAX_TERMINAL_ROWS + 1, 80)
                .expect_err("oversized rows rejected")
                .to_string()
                .contains("exceed maximum"),
        );
        assert!(
            validate_terminal_geometry("test", 24, MAX_TERMINAL_COLS + 1)
                .expect_err("oversized cols rejected")
                .to_string()
                .contains("exceed maximum"),
        );
        assert!(
            validate_terminal_geometry("test", MAX_TERMINAL_ROWS, MAX_TERMINAL_COLS)
                .expect_err("oversized area rejected")
                .to_string()
                .contains(&format!(
                    "area {} cells",
                    u32::from(MAX_TERMINAL_ROWS) * u32::from(MAX_TERMINAL_COLS)
                )),
        );
        assert_eq!(initial_pty_size(None, None).expect("defaults"), (24, 80));
        assert!(initial_pty_size(Some(0), Some(80)).is_err());
        assert!(initial_pty_size(Some(MAX_TERMINAL_ROWS), Some(MAX_TERMINAL_COLS)).is_err());
    }

    /// PR #17: attach snapshot 은 raw ring dump 가 아니라 terminal screen state 에서
    /// 합성한 현재 frame 이 broadcast 채널의 첫 chunk 로 들어가야 한다. 이후
    /// `append_output` 으로 들어오는 라이브 chunk 는 같은 채널에서 그 뒤에 순서대로
    /// 도착해야 한다.
    #[test]
    fn subscribe_with_snapshot_pushes_screen_state_as_first_chunk() {
        let session = build_test_session("snap-first");
        // PRELUDE 를 현재 화면 state 에 반영한다. subscriber 가 아직 없으므로 broadcast
        // 부작용 없이 ring 과 terminal_screen 만 갱신된다.
        session.append_output(b"PRELUDE\n");

        let on_evict: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});
        let (_id, rx) = session
            .subscribe_with_snapshot(24, 80, on_evict)
            .expect("subscribe");

        let first = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("first chunk");
        let rendered = String::from_utf8_lossy(&first);
        assert!(
            rendered.contains("PRELUDE"),
            "screen-state snapshot must contain the rendered current frame, got: {rendered:?}",
        );
        assert_ne!(
            first.as_ref(),
            b"PRELUDE\n",
            "snapshot must be formatted screen state, not raw ring bytes",
        );

        // 후속 라이브 chunk 들도 같은 채널을 통해 순서대로 들어와야 한다.
        session.append_output(b"LIVE-1\n");
        session.append_output(b"LIVE-2\n");
        let live1 = rx.recv_timeout(Duration::from_secs(1)).expect("live1");
        let live2 = rx.recv_timeout(Duration::from_secs(1)).expect("live2");
        assert_eq!(live1.as_ref(), b"LIVE-1\n");
        assert_eq!(live2.as_ref(), b"LIVE-2\n");
    }

    /// PR #17: raw ring replay 는 이미 지워진 scrollback/history 를 새 attach 에 다시
    /// 먹이는 문제가 있었다. screen-state replay 는 현재 visible frame 만 합성하므로
    /// clear 된 과거 텍스트가 snapshot 에 들어가지 않아야 한다.
    #[test]
    fn subscribe_with_snapshot_does_not_replay_cleared_ring_history() {
        let session = build_test_session("snap-current-frame");
        session.append_output(b"STALE-HISTORY\n");
        session.append_output(b"\x1b[2J\x1b[HFRESH-FRAME");

        let on_evict: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});
        let (_id, rx) = session
            .subscribe_with_snapshot(24, 80, on_evict)
            .expect("subscribe");

        let first = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("first chunk");
        let rendered = String::from_utf8_lossy(&first);
        assert!(
            rendered.contains("FRESH-FRAME"),
            "snapshot must contain current visible frame, got: {rendered:?}",
        );
        assert!(
            !rendered.contains("STALE-HISTORY"),
            "snapshot must not replay cleared raw ring history, got: {rendered:?}",
        );
    }

    #[test]
    fn subscribe_with_snapshot_uses_parser_state_for_normal_screen_output() {
        let session = build_test_session("snap-normal-parser-state");
        session.append_output(b"shell-normal-here");

        let on_evict: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});
        let (_id, rx) = session
            .subscribe_with_snapshot(24, 80, on_evict)
            .expect("subscribe");

        let first = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("first chunk");
        let rendered = String::from_utf8_lossy(&first);
        assert!(
            rendered.contains("shell-normal-here"),
            "normal-screen snapshot should come from live parser state, got: {rendered:?}",
        );
        assert!(
            find_bytes(first.as_ref(), ALT_SCREEN_ENTER).is_none(),
            "normal-screen snapshot must not enter alternate screen: {rendered:?}",
        );
    }

    #[test]
    fn subscribe_with_snapshot_preserves_alt_screen_state() {
        let session = build_test_session("snap-alt-screen");
        session.append_output(b"\x1b[?1049hALT-FRAME");

        let on_evict: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});
        let (_id, rx) = session
            .subscribe_with_snapshot(24, 80, on_evict)
            .expect("subscribe");

        let first = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("first chunk");
        assert!(
            find_bytes(first.as_ref(), ALT_SCREEN_ENTER).is_some(),
            "alt-screen snapshot must explicitly enter alt buffer, got: {:?}",
            String::from_utf8_lossy(&first)
        );
        assert!(
            String::from_utf8_lossy(&first).contains("ALT-FRAME"),
            "snapshot must include alt-screen contents"
        );
    }

    #[test]
    fn subscribe_with_snapshot_preserves_normal_buffer_before_alt_screen() {
        let session = build_test_session("snap-alt-normal");
        session.append_output(b"NORMAL-FRAME");
        session.append_output(b"\x1b[?1049hALT-FRAME");

        let on_evict: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});
        let (_id, rx) = session
            .subscribe_with_snapshot(24, 80, on_evict)
            .expect("subscribe");

        let first = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("first chunk");
        let normal_pos = find_bytes(first.as_ref(), b"NORMAL-FRAME")
            .expect("snapshot should seed session normal buffer");
        let alt_enter_pos =
            find_bytes(first.as_ref(), ALT_SCREEN_ENTER).expect("snapshot should enter alt buffer");
        let alt_pos =
            find_bytes(first.as_ref(), b"ALT-FRAME").expect("snapshot should render alt buffer");
        assert!(
            normal_pos < alt_enter_pos && alt_enter_pos < alt_pos,
            "snapshot must render normal buffer before entering alt screen: {:?}",
            String::from_utf8_lossy(&first)
        );
    }

    #[test]
    fn append_output_preserves_normal_buffer_when_alt_enter_shares_chunk() {
        let session = build_test_session("snap-alt-single-chunk");
        session.append_output(b"NORMAL-here\x1b[?1049hALT-FRAME");

        let on_evict: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});
        let (_id, rx) = session
            .subscribe_with_snapshot(24, 80, on_evict)
            .expect("subscribe");

        let first = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("first chunk");
        let normal_pos = find_bytes(first.as_ref(), b"NORMAL-here")
            .expect("snapshot should seed session normal buffer");
        let alt_enter_pos =
            find_bytes(first.as_ref(), ALT_SCREEN_ENTER).expect("snapshot should enter alt buffer");
        let alt_pos =
            find_bytes(first.as_ref(), b"ALT-FRAME").expect("snapshot should render alt buffer");
        assert!(
            normal_pos < alt_enter_pos && alt_enter_pos < alt_pos,
            "snapshot must render same-chunk normal buffer before entering alt screen: {:?}",
            String::from_utf8_lossy(&first)
        );
    }

    #[test]
    fn append_output_detects_grouped_alt_enter_private_modes() {
        let session = build_test_session("snap-alt-grouped-private");
        session.append_output(b"NORMAL-GROUP\x1b[?25;1049hALT-GROUP");

        let on_evict: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});
        let (_id, rx) = session
            .subscribe_with_snapshot(24, 80, on_evict)
            .expect("subscribe");

        let first = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("first chunk");
        let normal_pos = find_bytes(first.as_ref(), b"NORMAL-GROUP")
            .expect("snapshot should seed normal buffer for grouped private modes");
        let alt_enter_pos =
            find_bytes(first.as_ref(), ALT_SCREEN_ENTER).expect("snapshot should enter alt buffer");
        let alt_pos =
            find_bytes(first.as_ref(), b"ALT-GROUP").expect("snapshot should render alt buffer");
        assert!(
            normal_pos < alt_enter_pos && alt_enter_pos < alt_pos,
            "grouped private modes must preserve normal buffer before alt screen: {:?}",
            String::from_utf8_lossy(&first)
        );
    }

    #[test]
    fn alt_enter_detector_seeds_split_c1_csi_params() {
        let detector = super::AltEnterDetector::from_pending_prefix(b"\x9b?25;1049");

        assert!(
            detector.is_alt_enter_csi(),
            "split C1 CSI prefixes should seed already-read private-mode params"
        );
    }

    #[test]
    fn append_ring_bytes_preserves_limit_after_large_extend() {
        let mut ring = VecDeque::new();
        ring.extend(std::iter::repeat_n(b'x', super::RING_LIMIT - 4));

        let bytes = vec![b'y'; 16];
        super::append_ring_bytes(&mut ring, &bytes);

        assert_eq!(ring.len(), super::RING_LIMIT);
        assert_eq!(
            ring.iter().filter(|byte| **byte == b'y').count(),
            bytes.len(),
            "new bytes should be appended after draining overflow in one batch"
        );
    }

    #[test]
    fn resolve_session_sanitizes_missing_target() {
        let state = Arc::new(super::State::default());
        let err = match super::resolve_session(&state, "\x1b]52;c;secret\x07missing") {
            Ok(_) => panic!("target should be missing"),
            Err(err) => err.to_string(),
        };

        assert!(
            !err.contains("\x1b]52"),
            "missing-target error must not include raw terminal controls: {err:?}"
        );
        assert!(
            err.contains("missing"),
            "sanitized error should retain safe target context: {err:?}"
        );
    }

    #[test]
    fn subscribe_with_snapshot_preserves_pending_control_prefix() {
        let session = build_test_session("snap-pending-prefix");
        session.append_output(b"\x1b[");

        let on_evict: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});
        let (_id, rx) = session
            .subscribe_with_snapshot(24, 80, on_evict)
            .expect("subscribe");

        let first = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("first chunk");
        assert!(
            first.ends_with(b"\x1b["),
            "snapshot must preserve pending CSI prefix so the next live chunk is not printed literally: {:?}",
            String::from_utf8_lossy(&first)
        );

        session.append_output(b"2J");
        let live = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("live suffix");
        assert_eq!(live.as_ref(), b"2J");
        let mut combined = Vec::from(first.as_ref());
        combined.extend_from_slice(live.as_ref());
        assert!(
            combined
                .windows(b"\x1b[2J".len())
                .any(|window| window == b"\x1b[2J"),
            "combined snapshot+live stream must reconstruct the split CSI"
        );
    }

    #[test]
    fn terminal_prefix_tracker_clears_completed_raw_c1_strings() {
        for bytes in [
            &b"\x90secret\x9cSAFE"[..],
            &b"\x9d52;c;secret\x9cSAFE"[..],
            &b"\x9esecret\x9cSAFE"[..],
            &b"\x9fsecret\x9cSAFE"[..],
        ] {
            let mut tracker = TerminalPrefixTracker::default();
            tracker.process(bytes);

            assert!(
                tracker.pending_bytes().is_empty(),
                "completed raw C1 control string must not be replayed to future attach clients: {bytes:?}"
            );
        }
    }

    #[test]
    fn terminal_prefix_tracker_does_not_treat_utf8_continuations_as_raw_c1() {
        let mut tracker = TerminalPrefixTracker::default();
        tracker.process(b"\x1b]0;\xe6\x9c\x9d");

        assert!(
            !tracker.pending_bytes().is_empty(),
            "UTF-8 continuation bytes inside OSC payload must not terminate the pending control string"
        );

        tracker.process(b"\x07SAFE");
        assert!(tracker.pending_bytes().is_empty());

        let mut ground_tracker = TerminalPrefixTracker::default();
        ground_tracker.process("朝SAFE".as_bytes());
        assert!(ground_tracker.pending_bytes().is_empty());
    }

    #[test]
    fn terminal_prefix_tracker_resynchronizes_escape_inside_csi() {
        let mut tracker = TerminalPrefixTracker::default();
        tracker.process(b"\x1b[?1049\x1b]");

        assert_eq!(
            tracker.pending_bytes(),
            b"\x1b]".to_vec(),
            "ESC inside CSI must start a fresh escape sequence instead of being swallowed into CSI"
        );

        tracker.process(b"0;title\x07SAFE");
        assert!(tracker.pending_bytes().is_empty());
    }

    #[test]
    fn terminal_prefix_tracker_clears_cancelled_control_sequences() {
        for bytes in [
            &b"\x1b]52;c;secret\x18"[..],
            &b"\x1b[?1049\x1a"[..],
            &b"\x9b?1049\x9c"[..],
        ] {
            let mut tracker = TerminalPrefixTracker::default();
            tracker.process(bytes);
            assert!(
                tracker.pending_bytes().is_empty(),
                "cancelled or ST-terminated control prefix must be dropped: {bytes:?}"
            );
        }
    }

    #[test]
    fn terminal_parser_update_panic_degrades_but_keeps_raw_output_live() {
        let session = build_test_session("parser-update-degraded");
        session
            .terminal_parser_panic_on_next_update
            .store(true, Ordering::SeqCst);

        session.append_output(b"RAW-BEFORE-DEGRADE\n");

        assert!(
            session.terminal_parser_degraded(),
            "caught parser panic should quarantine only terminal snapshot state"
        );
        let captured = String::from_utf8_lossy(&session.capture_bytes(None, None)).to_string();
        assert!(
            captured.contains("RAW-BEFORE-DEGRADE"),
            "raw ring/capture must keep bytes appended by the failing update: {captured:?}"
        );
        assert!(
            !super::lock(&session.output_progress.0).closed,
            "parser degradation must not close output progress"
        );

        let on_evict: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});
        let (_id, rx) = session
            .subscribe_with_snapshot(24, 80, on_evict)
            .expect("subscribe after parser degradation");
        assert!(
            matches!(
                rx.recv_timeout(Duration::from_millis(50)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ),
            "degraded parser must skip synthetic attach snapshots instead of replaying stale state"
        );

        session.append_output(b"LIVE-AFTER-DEGRADE\n");
        let live = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("live output after degradation");
        assert_eq!(
            live.as_ref(),
            b"LIVE-AFTER-DEGRADE\n",
            "live broadcast must continue after parser degradation"
        );
    }

    #[test]
    fn terminal_snapshot_panic_degrades_and_future_live_chunks_continue() {
        let session = build_test_session("snapshot-degraded");
        session.append_output(b"SNAPSHOT-CANDIDATE\n");
        session
            .terminal_parser_panic_on_next_snapshot
            .store(true, Ordering::SeqCst);

        let on_evict: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});
        let (_id, rx) = session
            .subscribe_with_snapshot(24, 80, on_evict)
            .expect("subscribe should survive snapshot formatter panic");

        assert!(
            session.terminal_parser_degraded(),
            "snapshot formatter panic should degrade only the terminal parser state"
        );
        assert!(
            matches!(
                rx.recv_timeout(Duration::from_millis(50)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ),
            "failed snapshot should not enqueue a stale or partial initial chunk"
        );

        session.append_output(b"SNAPSHOT-PANIC-LIVE\n");
        let live = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("live output after snapshot degradation");
        assert_eq!(live.as_ref(), b"SNAPSHOT-PANIC-LIVE\n");
    }

    #[test]
    fn terminal_resize_panic_degrades_but_cached_geometry_still_updates() {
        let session = build_test_session("resize-degraded");
        let original_screen_size = session
            .terminal_screen
            .lock()
            .expect("terminal screen")
            .screen()
            .size();
        session
            .terminal_parser_panic_on_next_resize
            .store(true, Ordering::SeqCst);

        session
            .apply_pty_size(12, 40, "test resize degradation")
            .expect("PTY resize should still succeed when parser resize panics");

        assert!(
            session.terminal_parser_degraded(),
            "resize panic should quarantine terminal parser state"
        );
        assert_eq!(*session.rows.lock().expect("rows"), 12);
        assert_eq!(*session.cols.lock().expect("cols"), 40);
        assert_eq!(
            super::lock(&session.terminal_screen).screen().size(),
            original_screen_size,
            "test hook panics before parser resize, but cached PTY geometry must still move on"
        );

        session
            .apply_pty_size(10, 30, "test resize after degradation")
            .expect("later PTY resize should skip parser but keep cached geometry current");
        assert_eq!(*session.rows.lock().expect("rows after degradation"), 10);
        assert_eq!(*session.cols.lock().expect("cols after degradation"), 30);
        assert_eq!(
            super::lock(&session.terminal_screen).screen().size(),
            original_screen_size,
            "degraded parser must be skipped on later resizes"
        );
    }

    #[test]
    fn subscribe_with_snapshot_rejects_oversized_attach_geometry() {
        let session = build_test_session("snap-oversized");
        let on_evict: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});
        let err = match session.subscribe_with_snapshot(MAX_TERMINAL_ROWS + 1, 80, on_evict) {
            Ok(_) => panic!("oversized attach rows should be rejected"),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains("exceed maximum"),
            "unexpected error: {err:#}"
        );

        let on_evict: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});
        let err =
            match session.subscribe_with_snapshot(MAX_TERMINAL_ROWS, MAX_TERMINAL_COLS, on_evict) {
                Ok(_) => panic!("oversized attach area should be rejected"),
                Err(err) => err,
            };
        assert!(
            err.to_string().contains("terminal area"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn subscribe_with_snapshot_and_apply_clamp_updates_size_before_returning() {
        let session = build_test_session("snap-atomic-clamp");
        session.append_output(b"PRELUDE\n");

        let on_evict: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});
        let (_id, rx) = session
            .subscribe_with_snapshot_and_apply_clamp(12, 40, on_evict)
            .expect("subscribe and apply clamp");

        assert_eq!(*session.rows.lock().expect("rows"), 12);
        assert_eq!(*session.cols.lock().expect("cols"), 40);
        assert_eq!(
            session
                .terminal_screen
                .lock()
                .expect("terminal screen")
                .screen()
                .size(),
            (12, 40),
            "attach helper must resize parser before live output can resume"
        );
        let first = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("first chunk");
        assert!(
            String::from_utf8_lossy(&first).contains("PRELUDE"),
            "snapshot should still be queued before live output"
        );
    }

    #[test]
    fn apply_pty_size_updates_terminal_screen_size() {
        let session = build_test_session("screen-resize");
        session
            .apply_pty_size(12, 40, "test resize")
            .expect("resize test pty");
        assert_eq!(*session.rows.lock().expect("rows"), 12);
        assert_eq!(*session.cols.lock().expect("cols"), 40);
        assert_eq!(
            session
                .terminal_screen
                .lock()
                .expect("terminal screen")
                .screen()
                .size(),
            (12, 40),
            "parser screen size must track successful PTY resize"
        );
    }

    /// PR #16 quad-review HIGH 후속 (Forge 고유): 기존 `subscribe_with_snapshot_*`
    /// 단위테스트는 sequential happy-path (먼저 ring 채우고 → subscribe → 라이브 push)
    /// 만 검증해 PR 이 실제로 수정한다고 주장하는 race (subscribe 도중 다른 스레드의
    /// `append_output` 이 들어오는 상황) 를 exercise 하지 못한다. 본 테스트는 백그라운드
    /// 스레드가 라이브 출력을 producing 하는 도중 `subscribe_with_snapshot` 을 호출하고,
    /// 첫 chunk 가 항상 screen-state snapshot 임을 확인한다.
    ///
    /// `append_output` 이 `broadcast_order` 를 잡은 상태로 `output_state` 아래에서
    /// (a) ring/parser 갱신, (b) subscribers 스냅샷 복사를 직렬화하므로, subscribe 의
    /// snapshot push 가 lock 안에서 끝나야 새 sub 의 큐에 라이브 chunk 가 snapshot 보다
    /// 먼저 들어가지 않는다.
    #[test]
    fn subscribe_with_snapshot_first_chunk_is_snapshot_even_under_concurrent_live_output() {
        use std::sync::Barrier;

        let session = build_test_session("snap-race");
        session.append_output(b"PRELUDE");

        // 백그라운드에서 라이브 출력을 계속 produce 하는 스레드. subscribe 시점에 이미
        // append_output 이 동시에 호출되고 있어야 본 PR 의 race 를 실제로 exercise 한다.
        let session_for_thread = Arc::clone(&session);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = Arc::clone(&stop);
        let live_started = Arc::new(Barrier::new(2));
        let live_started_clone = Arc::clone(&live_started);
        let live_thread = std::thread::spawn(move || {
            // 라이브 스레드가 produce 직전임을 main 에 알린다.
            live_started_clone.wait();
            let mut counter = 0u32;
            while !stop_for_thread.load(Ordering::SeqCst) {
                session_for_thread.append_output(format!("\x1b[2;1HLIVE-{counter:<8}").as_bytes());
                counter += 1;
                // CPU 100% 사용을 막기 위한 짧은 sleep — race 자체와 무관.
                std::thread::sleep(Duration::from_micros(50));
            }
        });

        // 라이브 스레드가 막 시작 직전임을 확인한 뒤 추가로 짧게 sleep — 라이브 스레드
        // 가 실제로 append_output 을 몇 번 돌려 진짜로 race 가 발생하게 한다.
        live_started.wait();
        std::thread::sleep(Duration::from_millis(2));

        let on_evict: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});
        let (_id, rx) = session
            .subscribe_with_snapshot(24, 80, on_evict)
            .expect("subscribe");

        // 첫 chunk 는 subscribe 시점의 screen-state snapshot 이어야 한다.
        // `subscribe_with_snapshot` 안의 `output_state` 가드가 snapshot push 와
        // append_output 의 ring/parser update + subscriber snapshot 을 직렬화해
        // 라이브 chunk 가 snapshot 앞에 끼어들 수 없게 한다.
        let first = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("first chunk");
        let rendered = String::from_utf8_lossy(&first);
        assert!(
            rendered.contains("PRELUDE"),
            "first chunk must be formatted screen-state snapshot containing PRELUDE, got: {rendered:?}",
        );
        assert!(
            first.starts_with(b"\x1b"),
            "screen-state snapshot should start with terminal formatting escape, got: {rendered:?}",
        );

        stop.store(true, Ordering::SeqCst);
        live_thread.join().expect("live thread");
    }

    /// PR #16: ring 이 비어 있으면 snapshot push 자체를 건너뛴다 — 빈 chunk 가 채널에
    /// 떨어지면 클라이언트가 의미 없는 0 byte write 를 받아 혼란이 생긴다.
    #[test]
    fn subscribe_with_snapshot_skips_push_when_ring_empty() {
        let session = build_test_session("snap-empty");
        let on_evict: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});
        let (_id, rx) = session
            .subscribe_with_snapshot(24, 80, on_evict)
            .expect("subscribe");

        match rx.try_recv() {
            Err(mpsc::TryRecvError::Empty) => {} // 정상: 빈 ring → 빈 채널
            Ok(chunk) => panic!("expected empty channel, got chunk of {} bytes", chunk.len()),
            Err(err) => panic!("unexpected receiver error: {err}"),
        }
    }

    /// A laggy subscriber may keep `broadcast_chunk` in its backpressure retry
    /// window for up to `BACKPRESSURE_SEND_TIMEOUT`. That delay must not keep
    /// `output_state` locked, otherwise attach snapshot and resize paths stall
    /// behind a slow consumer even though live chunk ordering is now protected
    /// by `broadcast_order`.
    #[test]
    fn append_output_releases_output_state_during_backpressure_wait() {
        let session = build_test_session("output-state-backpressure");
        let on_evict: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});
        let (tx, _rx) = mpsc::sync_channel::<OutputChunk>(1);
        tx.try_send(Arc::from(&b"seed"[..]))
            .expect("pre-fill subscriber queue");
        super::lock(&session.subscribers).push(Subscriber {
            id: 1,
            tx,
            on_evict,
            rows: 24,
            cols: 80,
        });

        let (entered_tx, entered_rx) = mpsc::channel::<()>();
        let release = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let release_for_hook = Arc::clone(&release);
        let first_backpressure = Arc::new(AtomicBool::new(true));
        let first_backpressure_for_hook = Arc::clone(&first_backpressure);
        *super::lock(&session.backpressure_hook) = Some(Arc::new(move || {
            if first_backpressure_for_hook.swap(false, Ordering::SeqCst) {
                entered_tx
                    .send(())
                    .expect("signal first backpressure entry");
                let (release_lock, release_cvar) = &*release_for_hook;
                let released = release_lock.lock().expect("release lock");
                let (released, _) = release_cvar
                    .wait_timeout_while(released, Duration::from_secs(1), |released| !*released)
                    .expect("release wait");
                assert!(*released, "wait for test release");
            }
        }));

        let session_for_thread = Arc::clone(&session);
        let append_thread = std::thread::spawn(move || {
            session_for_thread.append_output(b"blocked");
        });

        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("append should enter broadcast backpressure");
        let guard = session
            .output_state
            .try_lock()
            .expect("output_state should be available during backpressure wait");
        drop(guard);
        {
            let (release_lock, release_cvar) = &*release;
            *release_lock.lock().expect("release lock") = true;
            release_cvar.notify_one();
        }
        append_thread.join().expect("append thread");
        assert!(
            !first_backpressure.load(Ordering::SeqCst),
            "one-shot hook should have observed the backpressure wait"
        );
    }

    #[test]
    fn append_output_preserves_chunk_order_during_backpressure_wait() {
        let session = build_test_session("broadcast-order-backpressure");
        let on_evict: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});
        let (tx, rx) = mpsc::sync_channel::<OutputChunk>(1);
        tx.try_send(Arc::from(&b"seed"[..]))
            .expect("pre-fill subscriber queue");
        super::lock(&session.subscribers).push(Subscriber {
            id: 1,
            tx,
            on_evict,
            rows: 24,
            cols: 80,
        });

        let (entered_tx, entered_rx) = mpsc::channel::<()>();
        let release = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let release_for_hook = Arc::clone(&release);
        let first_backpressure = Arc::new(AtomicBool::new(true));
        let first_backpressure_for_hook = Arc::clone(&first_backpressure);
        *super::lock(&session.backpressure_hook) = Some(Arc::new(move || {
            if first_backpressure_for_hook.swap(false, Ordering::SeqCst) {
                entered_tx
                    .send(())
                    .expect("signal first backpressure entry");
                let (release_lock, release_cvar) = &*release_for_hook;
                let released = release_lock.lock().expect("release lock");
                let (released, _) = release_cvar
                    .wait_timeout_while(released, Duration::from_secs(1), |released| !*released)
                    .expect("release wait");
                assert!(*released, "wait for test release");
            }
        }));

        let session_for_first = Arc::clone(&session);
        let first_thread = std::thread::spawn(move || {
            session_for_first.append_output(b"first");
        });

        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("first append should enter broadcast backpressure");

        let session_for_second = Arc::clone(&session);
        let second_thread = std::thread::spawn(move || {
            session_for_second.append_output(b"second");
        });

        assert_eq!(
            &*rx.recv_timeout(Duration::from_secs(1)).expect("seed chunk"),
            b"seed"
        );
        {
            let (release_lock, release_cvar) = &*release;
            *release_lock.lock().expect("release lock") = true;
            release_cvar.notify_one();
        }
        assert_eq!(
            &*rx.recv_timeout(Duration::from_secs(1))
                .expect("first chunk"),
            b"first",
            "the first live chunk must be delivered before the later append"
        );
        assert_eq!(
            &*rx.recv_timeout(Duration::from_secs(1))
                .expect("second chunk"),
            b"second",
            "broadcast_order must preserve live chunk enqueue order"
        );

        first_thread.join().expect("first append thread");
        second_thread.join().expect("second append thread");
    }

    /// PR #16: 한 번 try_send 가 `Full` 을 반환했더라도, 그 sub 의 consumer 가
    /// `BACKPRESSURE_SEND_TIMEOUT` 안에 한 칸이라도 비워주면 evict 되지 않고 회복해야
    /// 한다. 모바일 SSH 의 50–200ms 트랜지언트 jitter 시나리오의 happy-path 검증.
    ///
    /// `rx` 는 본 함수가 끝날 때까지 살아 있어야 한다 — drain 스레드 안에서만 잡고
    /// 있다 종료 시점에 drop 시키면 채널이 Disconnected 로 전이해 broadcast 의 try_send
    /// 가 Full 이 아닌 Disconnected 를 받아 즉시 evict 된다 (테스트가 실제로 검증하려는
    /// timeout 회복과 무관한 경로). 따라서 `rx` 는 본 함수의 stack-local 로 두고
    /// drain 스레드는 `Receiver` 를 빌리는 식이 아니라, drain 메시지를 본 함수에서
    /// 직접 처리한다 — 단순히 timeout 안에 한 번이라도 recv 가 일어나면 충분.
    ///
    /// PR #16 quad-review HIGH 후속 (Forge): 이전 구현은 drain 스레드가 `sleep(15ms)`
    /// 후 동작한다고 가정하고 main 스레드의 broadcast 가 그 사이 pass 2 에 진입해
    /// 있을 거라 기대했지만, 슬로우 CI / 다른 부하 상황에서는 main 스레드의 broadcast
    /// 가 sleep 보다 늦게 돌아 fail 했다. 또한 wall-clock 의 `elapsed <
    /// BACKPRESSURE_SEND_TIMEOUT + 50ms` assertion 도 fragile 했다. mpsc::channel 시그
    /// 널로 drain 준비 완료 → broadcast 트리거 순서를 강제하고, wall-clock 측정을
    /// 제거해 logical assertion (eviction count + sub presence) 만 남긴다.
    #[test]
    fn append_output_send_timeout_recovers_when_consumer_drains_within_window() {
        let session = build_test_session("recover");
        let on_evict_calls = Arc::new(AtomicU32::new(0));
        let calls_for_closure = Arc::clone(&on_evict_calls);
        let on_evict: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            calls_for_closure.fetch_add(1, Ordering::SeqCst);
        });

        let (_id, rx) = session
            .subscribe_with_snapshot(24, 80, on_evict)
            .expect("subscribe");

        // 채널을 정확히 가득 채운다 — 이 시점 이후의 append_output 은 첫 try_send 가
        // Full 을 반환한다.
        for _ in 0..SUBSCRIBER_QUEUE_LIMIT {
            session.append_output(b"x");
        }

        // mpsc::channel 시그널로 drain 스레드 준비 완료를 기다린 뒤 broadcast 를
        // 트리거한다. 이렇게 하면 slow CI 에서 sleep(15ms) 가 부족해 main 스레드가
        // 먼저 broadcast 에 진입해 timeout 까지 다 소진하는 race 가 사라진다.
        let rx_shared = Arc::new(Mutex::new(rx));
        let rx_for_drain = Arc::clone(&rx_shared);
        let (drain_ready_tx, drain_ready_rx) = mpsc::channel::<()>();
        let drain_thread = std::thread::spawn(move || {
            // drain 스레드 진입 직후 즉시 ready 신호를 보낸다 — main 이 이 신호를
            // 받은 후에 broadcast 를 시작하므로 drain 스레드가 lock 을 잡고 recv
            // 를 도는 시점이 broadcast 의 pass 2 와 겹친다.
            drain_ready_tx.send(()).expect("signal drain ready");
            let rx = rx_for_drain.lock().expect("drain rx lock");
            // 여러 칸 비운다 — broadcast pass 2 polling 이 5ms 마다 try_send 를 시도
            // 하는 동안 recv 가 한 슬롯을 비우면 그 다음 polling 시점에 회복한다.
            for _ in 0..5 {
                if rx.recv_timeout(Duration::from_millis(20)).is_err() {
                    break;
                }
            }
        });

        // drain 스레드가 ready 일 때까지 블록 — slow CI 에서도 deterministic.
        drain_ready_rx.recv().expect("drain ready signal");
        session.append_output(b"recovered");
        drain_thread.join().expect("drain thread");
        // `rx_shared` 는 본 함수의 끝까지 살아있다. drop 시 Disconnected 로 전이하지만
        // assertion 들은 모두 그 전에 evaluate 된다.

        assert_eq!(
            on_evict_calls.load(Ordering::SeqCst),
            0,
            "consumer recovered within timeout — must NOT trigger eviction",
        );
        let remaining = super::lock(&session.subscribers).len();
        assert_eq!(
            remaining, 1,
            "subscriber must still be attached after recovery"
        );
        drop(rx_shared);
    }

    /// PR #16: consumer 가 timeout 안에 절대 회복하지 못하면 그 sub 는 evict 되어야
    /// 한다 — PR #13 의 zombie-attach guard 가 timeout fallback 으로도 유지됨을 확인.
    #[test]
    fn append_output_send_timeout_evicts_when_consumer_persistently_stuck() {
        let session = build_test_session("evict");
        let on_evict_calls = Arc::new(AtomicU32::new(0));
        let calls_for_closure = Arc::clone(&on_evict_calls);
        let on_evict: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            calls_for_closure.fetch_add(1, Ordering::SeqCst);
        });

        let (_id, _rx) = session
            .subscribe_with_snapshot(24, 80, on_evict)
            .expect("subscribe");

        // 채널을 가득 채운다 — _rx 는 drain 하지 않으므로 pass 2 polling 이 timeout
        // 까지 다 소진한 뒤 evict.
        for _ in 0..SUBSCRIBER_QUEUE_LIMIT {
            session.append_output(b"y");
        }

        let started = Instant::now();
        session.append_output(b"trigger-evict");
        let elapsed = started.elapsed();

        assert!(
            elapsed >= BACKPRESSURE_SEND_TIMEOUT,
            "append_output must wait at least the full timeout before evicting; took {elapsed:?}",
        );
        assert_eq!(
            on_evict_calls.load(Ordering::SeqCst),
            1,
            "stuck consumer must trigger eviction exactly once",
        );
        let remaining = super::lock(&session.subscribers).len();
        assert_eq!(remaining, 0, "evicted subscriber must be removed");
    }

    /// PR #16: 3 sub — sub#0 healthy, sub#1 laggy (queue full), sub#2 healthy.
    /// pass 1 에서 sub#0 / sub#2 는 즉시 try_send 로 받고, pass 2 가 sub#1 을 두고
    /// timeout 까지 폴링해도 sub#0 / sub#2 의 chunk 전달은 이미 끝나 있어야 한다.
    #[test]
    fn append_output_two_pass_only_blocks_on_laggy_subscribers() {
        let session = build_test_session("two-pass");
        let calls_0 = Arc::new(AtomicU32::new(0));
        let calls_1 = Arc::new(AtomicU32::new(0));
        let calls_2 = Arc::new(AtomicU32::new(0));
        let make_evict = |c: Arc<AtomicU32>| -> Arc<dyn Fn() + Send + Sync> {
            Arc::new(move || {
                c.fetch_add(1, Ordering::SeqCst);
            })
        };

        let (_id0, rx0) = session
            .subscribe_with_snapshot(24, 80, make_evict(Arc::clone(&calls_0)))
            .expect("subscribe sub#0");
        let (_id1, _rx1) = session
            .subscribe_with_snapshot(24, 80, make_evict(Arc::clone(&calls_1)))
            .expect("subscribe sub#1");
        let (_id2, rx2) = session
            .subscribe_with_snapshot(24, 80, make_evict(Arc::clone(&calls_2)))
            .expect("subscribe sub#2");

        // sub#1 의 채널만 가득 채운다 — sub#1 _rx1 는 drain 하지 않고, sub#0 과 sub#2
        // 는 본 루프가 도는 동안 별도 스레드에서 drain 한다. drain 스레드는 본 함수가
        // 큐를 채우는 동안에도 동작해야 sub#0 / sub#2 가 함께 가득 차지 않는다.
        let rx0_arc = std::sync::Arc::new(std::sync::Mutex::new(rx0));
        let rx2_arc = std::sync::Arc::new(std::sync::Mutex::new(rx2));
        let rx0_drain = Arc::clone(&rx0_arc);
        let rx2_drain = Arc::clone(&rx2_arc);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_drain = Arc::clone(&stop);
        let drainer = std::thread::spawn(move || {
            while !stop_drain.load(Ordering::SeqCst) {
                let _ = rx0_drain
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_millis(5));
                let _ = rx2_drain
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_millis(5));
            }
        });

        // sub#1 의 큐를 가득 채우기 위해 SUBSCRIBER_QUEUE_LIMIT 만큼 broadcast 한다.
        // sub#0 / sub#2 는 drainer 가 받아주므로 안 막힌다.
        for _ in 0..SUBSCRIBER_QUEUE_LIMIT {
            session.append_output(b"f");
        }

        // 이 시점에 sub#1 의 채널이 가득 — 다음 broadcast 가 pass 2 를 트리거한다.
        // sub#1 timeout 동안 sub#0 / sub#2 는 즉시 받아야 한다 (pass 1).
        let started = Instant::now();
        session.append_output(b"final");
        let elapsed = started.elapsed();
        stop.store(true, Ordering::SeqCst);
        drainer.join().expect("drainer thread");

        // 본 broadcast 의 wall time 은 timeout 한 윈도우 + slow-CI 슬랙 안에 들어와야
        // 한다 (sub#1 한 명만 laggy 이므로 K=1). PR #16 quad-review HIGH 후속 (Forge):
        // wall-clock 의 ±80ms 슬랙은 슬로우 CI 에서 fragile — 3x 상한으로 풀어 둔다.
        // logical invariant 는 아래 eviction-count assertion 들이 보호한다.
        assert!(
            elapsed < BACKPRESSURE_SEND_TIMEOUT * 3 + Duration::from_millis(100),
            "two-pass broadcast wall time must stay within timeout × 3 + slack, got {elapsed:?}",
        );
        // sub#0 / sub#2 는 evict 되지 않아야 한다 — 본 PR 의 핵심 invariant.
        assert_eq!(
            calls_0.load(Ordering::SeqCst),
            0,
            "healthy sub#0 must not be evicted",
        );
        assert_eq!(
            calls_2.load(Ordering::SeqCst),
            0,
            "healthy sub#2 must not be evicted",
        );
        // sub#1 은 timeout 후 evict.
        assert_eq!(
            calls_1.load(Ordering::SeqCst),
            1,
            "laggy sub#1 must be evicted exactly once",
        );
    }

    /// PR #16: `broadcast_chunk` 가 pass-1 try_send 로 정상 sub 에 즉시 chunk 를
    /// 전달하고, pass-2 폴링은 laggy sub 에만 적용해야 한다. session/lock 없이도
    /// 정책의 핵심 invariant 를 빠르게 검증할 수 있는 단위테스트.
    #[test]
    fn broadcast_chunk_pass1_delivers_to_healthy_subs_immediately() {
        let (tx_a, rx_a) = mpsc::sync_channel::<OutputChunk>(SUBSCRIBER_QUEUE_LIMIT);
        let (tx_b, _rx_b) = mpsc::sync_channel::<OutputChunk>(2);
        let (tx_c, rx_c) = mpsc::sync_channel::<OutputChunk>(SUBSCRIBER_QUEUE_LIMIT);
        // sub#1 의 채널을 가득 채워 둔다 — pass 2 로 빠지게.
        tx_b.try_send(Arc::from(&b"x"[..])).expect("seed b 1");
        tx_b.try_send(Arc::from(&b"y"[..])).expect("seed b 2");

        let subs = vec![
            Subscriber {
                id: 10,
                tx: tx_a,
                on_evict: Arc::new(|| {}),
                rows: 24,
                cols: 80,
            },
            Subscriber {
                id: 11,
                tx: tx_b,
                on_evict: Arc::new(|| {}),
                rows: 24,
                cols: 80,
            },
            Subscriber {
                id: 12,
                tx: tx_c,
                on_evict: Arc::new(|| {}),
                rows: 24,
                cols: 80,
            },
        ];

        let chunk: OutputChunk = Arc::from(&b"hello"[..]);
        let started = Instant::now();
        let disconnected = broadcast_chunk(&subs, chunk, Duration::from_millis(50), None);
        let elapsed = started.elapsed();

        // sub#1 은 timeout 끝까지 돌고 evict.
        assert_eq!(disconnected, vec![11]);
        // sub#0 / sub#2 의 첫 chunk 는 즉시 도달해 있어야 한다 — pass 2 timeout 진입
        // 전에 try_recv 가 성공해야 함.
        assert_eq!(
            rx_a.try_recv().map(|c| c.as_ref().to_vec()).ok(),
            Some(b"hello".to_vec()),
            "healthy sub#0 must have received chunk in pass 1",
        );
        assert_eq!(
            rx_c.try_recv().map(|c| c.as_ref().to_vec()).ok(),
            Some(b"hello".to_vec()),
            "healthy sub#2 must have received chunk in pass 1",
        );
        // 본 함수 자체는 pass 2 timeout 한 번을 다 소진하지만 sub#0 / sub#2 chunk 는
        // 그 전에 이미 들어가 있다.
        assert!(
            elapsed >= Duration::from_millis(50),
            "broadcast_chunk waits the full timeout for the laggy sub; took {elapsed:?}",
        );
    }
}
