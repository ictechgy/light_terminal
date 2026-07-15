use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;

/// Maximum decoded byte length accepted for `Request::Send` payloads.
///
/// The daemon request-frame cap is 1 MiB, while the compact on-wire format is
/// base64 inside JSON. Keep the decoded cap below that frame limit with margin
/// for base64 expansion plus the request envelope.
pub const MAX_SEND_DATA_BYTES: usize = 700 * 1024;
pub const MAX_CAPABILITY_INPUT_BYTES: usize = 64 * 1024;
pub const MAX_INPUT_CAPABILITY_BUDGET: u64 = 1024 * 1024;
pub const CAPABILITY_PROTOCOL_VERSION: u32 = 5;
pub const PROTOCOL_VERSION: u32 = 8;
pub const MAX_METADATA_JOURNAL_ENTRIES: usize = 1024;
pub const DEFAULT_RECENT_EXITS_LIMIT: u16 = 20;
pub const MAX_RECENT_EXITS_LIMIT: u16 = 100;
pub const CMUX_CONTEXT_ENV: &[&str] = &[
    "CMUX_WORKSPACE_ID",
    "CMUX_SURFACE_ID",
    "CMUX_WINDOW_ID",
    "CMUX_SOCKET_PATH",
];
pub const CHILD_COLOR_POLICY_ENV: &[&str] =
    &["NO_COLOR", "FORCE_COLOR", "CLICOLOR", "CLICOLOR_FORCE"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StatusTheme {
    Blue,
    Green,
    Magenta,
    Cyan,
    Amber,
    Red,
    Gray,
    Plain,
}

impl StatusTheme {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "blue" => Some(Self::Blue),
            "green" => Some(Self::Green),
            "magenta" | "purple" => Some(Self::Magenta),
            "cyan" => Some(Self::Cyan),
            "amber" | "yellow" => Some(Self::Amber),
            "red" => Some(Self::Red),
            "gray" | "grey" => Some(Self::Gray),
            "plain" | "minimal" => Some(Self::Plain),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Blue => "blue",
            Self::Green => "green",
            Self::Magenta => "magenta",
            Self::Cyan => "cyan",
            Self::Amber => "amber",
            Self::Red => "red",
            Self::Gray => "gray",
            Self::Plain => "plain",
        }
    }

    pub fn allowed_values() -> &'static str {
        "blue, green, magenta (purple), cyan, amber (yellow), red, gray (grey), plain (minimal)"
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionExitTrigger {
    LeaderExited,
    CloseRequested,
    DaemonShutdown,
    ParentCascade { parent_session_id: String },
    Unknown,
}

impl SessionExitTrigger {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::LeaderExited => "leader_exited",
            Self::CloseRequested => "close_requested",
            Self::DaemonShutdown => "daemon_shutdown",
            Self::ParentCascade { .. } => "parent_cascade",
            Self::Unknown => "unknown",
        }
    }
}

impl fmt::Display for SessionExitTrigger {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ParentCascade { parent_session_id } => {
                write!(
                    formatter,
                    "parent_cascade(parent_session_id={parent_session_id})"
                )
            }
            trigger => formatter.write_str(trigger.as_str()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SessionLifecycleState {
    Healthy,
    MonitorFailed,
    Ending { trigger: SessionExitTrigger },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitListScope {
    TopLevel,
    Children,
    All,
}

impl ExitListScope {
    pub fn from_flags(all: bool, children: bool) -> Self {
        if all {
            Self::All
        } else if children {
            Self::Children
        } else {
            Self::TopLevel
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitOutcomeState {
    Pending,
    Complete,
    Unknown,
}

impl ExitOutcomeState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Complete => "complete",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitEvidenceState {
    Complete,
    DegradedMissingTriggerEvent,
    Conflicted,
    StorageDegraded,
}

impl ExitEvidenceState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::DegradedMissingTriggerEvent => "degraded_missing_trigger_event",
            Self::Conflicted => "conflicted",
            Self::StorageDegraded => "storage_degraded",
        }
    }
}

/// Raw-free, bounded lifecycle evidence for a finalized session.
///
/// This allowlist intentionally excludes commands, paths, environment values,
/// PTY bytes, scrollback, capability/parent tokens, and process identifiers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentSessionExit {
    pub schema_version: String,
    pub session_id: String,
    pub name: String,
    pub pane_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_pane_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    pub created_unix_ms: u128,
    pub trigger_claimed_unix_ms: u128,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reaped_unix_ms: Option<u128>,
    pub trigger: SessionExitTrigger,
    pub outcome_state: ExitOutcomeState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<String>,
    pub evidence_state: ExitEvidenceState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub name: String,
    pub pane_id: String,
    pub command: String,
    pub cwd: String,
    pub created_unix_ms: u128,
    pub alive: bool,
    pub exit_code: Option<i32>,
    pub rows: u16,
    pub cols: u16,
    #[serde(default)]
    pub parent_pane_id: Option<String>,
    #[serde(default)]
    pub parent_session_id: Option<String>,
    #[serde(default)]
    pub attached_clients: usize,
    #[serde(default)]
    pub process_id: Option<u32>,
    #[serde(default)]
    pub process_group_id: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_theme: Option<StatusTheme>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle_state: Option<SessionLifecycleState>,
}

impl SessionInfo {
    pub fn lifecycle_state(&self) -> SessionLifecycleState {
        self.lifecycle_state.clone().unwrap_or_else(|| {
            if self.alive {
                SessionLifecycleState::Healthy
            } else {
                SessionLifecycleState::Ending {
                    trigger: SessionExitTrigger::Unknown,
                }
            }
        })
    }

    pub fn is_live_work(&self) -> bool {
        match (&self.lifecycle_state, self.alive) {
            (None, alive) => alive,
            (Some(SessionLifecycleState::Healthy | SessionLifecycleState::MonitorFailed), true) => {
                true
            }
            (Some(SessionLifecycleState::Ending { .. }), false) => false,
            _ => false,
        }
    }

    pub fn lifecycle_state_label(&self) -> &'static str {
        match self.lifecycle_state() {
            SessionLifecycleState::Healthy => "alive",
            SessionLifecycleState::MonitorFailed => "degraded",
            SessionLifecycleState::Ending { .. } => "ending",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub version: String,
    pub protocol_version: u32,
    pub session_count: u64,
    pub active_connections: u64,
    pub shutting_down: bool,
    // daemon_uid: 데몬 프로세스의 effective uid. 같은 OS 사용자 trust boundary를
    // doctor에서 한눈에 확인하기 위한 필드. 옛 데몬 응답에는 없으므로
    // `#[serde(default)]`로 backward-compatible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_uid: Option<u32>,
    // started_at_unix_secs: 데몬 시작 시각(UNIX epoch seconds). uptime 계산용.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_unix_secs: Option<u64>,
}

/// Raw-free, read-only measurements for one live or recently exited session.
///
/// Only `output_closed`, `output_revision`, and `output_total_bytes` are copied
/// as one coherent group. The remaining fields are sampled independently, so
/// this is intentionally a relaxed snapshot rather than a transactional view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstrumentSnapshot {
    pub schema_version: String,
    pub observed_unix_ms: u64,
    pub session_id: String,
    pub pane_id: String,
    pub alive: bool,
    pub output_closed: bool,
    pub output_revision: u64,
    pub output_total_bytes: u64,
    pub attached_clients: usize,
    pub rows: u16,
    pub cols: u16,
}

/// Raw-free live session metadata. This intentionally excludes command, cwd,
/// environment, PTY output, capability tokens, and process details.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetadataValue {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_theme: Option<StatusTheme>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetadataOperation {
    Rename,
    StatusTheme,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetadataJournalEntry {
    pub operation: MetadataOperation,
    pub before: MetadataValue,
    pub after: MetadataValue,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetadataPurgeAggregate {
    pub generation: u64,
    pub purged_entries_total: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_purged_unix_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetadataHistoryResult {
    pub schema_version: String,
    pub session_id: String,
    pub pane_id: String,
    pub current: MetadataValue,
    pub entries: Vec<MetadataJournalEntry>,
    pub cursor: usize,
    pub capacity: usize,
    pub purge: MetadataPurgeAggregate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetadataStepDirection {
    Undo,
    Redo,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetadataStepResult {
    pub session_id: String,
    pub pane_id: String,
    pub direction: MetadataStepDirection,
    pub applied: MetadataJournalEntry,
    pub current: MetadataValue,
    pub cursor: usize,
    pub entry_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetadataPurgeResult {
    pub session_id: String,
    pub pane_id: String,
    pub current: MetadataValue,
    pub purged_entries: usize,
    pub cursor: usize,
    pub entry_count: usize,
    pub purge: MetadataPurgeAggregate,
}

/// Opaque daemon-generated input capability. Debug output is deliberately
/// redacted because tokens are bearer credentials inside the cooperative
/// same-UID trust boundary.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct CapabilityToken(String);

impl CapabilityToken {
    pub fn from_canonical(value: String) -> Option<Self> {
        let parsed = uuid::Uuid::parse_str(&value).ok()?;
        (parsed.hyphenated().to_string() == value).then_some(Self(value))
    }

    pub fn new_random() -> Self {
        Self(uuid::Uuid::new_v4().hyphenated().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for CapabilityToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CapabilityToken([REDACTED])")
    }
}

impl Serialize for CapabilityToken {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for CapabilityToken {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::from_canonical(value)
            .ok_or_else(|| serde::de::Error::custom("invalid capability token format"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityAction {
    Input,
    Revoke,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueInputCapabilityResult {
    pub token: CapabilityToken,
    pub byte_budget: u64,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SensitiveCapabilityRequest {
    Input {
        token: CapabilityToken,
        #[serde(with = "capability_data_serde")]
        data: Vec<u8>,
    },
    Revoke {
        token: CapabilityToken,
    },
}

impl fmt::Debug for SensitiveCapabilityRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Input { token, data } => formatter
                .debug_struct("Input")
                .field("token", token)
                .field("data_len", &data.len())
                .finish(),
            Self::Revoke { token } => formatter
                .debug_struct("Revoke")
                .field("token", token)
                .finish(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaitExitResult {
    pub session: SessionInfo,
    pub exited: bool,
    pub timed_out: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaitContainsResult {
    pub session: SessionInfo,
    pub matched: bool,
    pub timed_out: bool,
    pub exited: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Ping,
    Status,
    New {
        name: Option<String>,
        command: Option<String>,
        #[serde(default)]
        cwd: Option<String>,
        rows: Option<u16>,
        cols: Option<u16>,
        #[serde(default)]
        parent_pane_id: Option<String>,
        #[serde(default)]
        parent_token: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tmux_parent_pane_id: Option<String>,
        #[serde(default)]
        env: HashMap<String, String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status_theme: Option<StatusTheme>,
        tmux: bool,
    },
    List,
    RecentExits {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<String>,
        limit: u16,
        scope: ExitListScope,
    },
    Info {
        target: String,
    },
    Instrument {
        target: String,
    },
    MetadataHistory {
        target: String,
    },
    MetadataUndo {
        target: String,
    },
    MetadataRedo {
        target: String,
    },
    MetadataPurgeHistory {
        target: String,
        irreversible: bool,
        session_id: String,
    },
    IssueInputCapability {
        target: String,
        byte_budget: u64,
    },
    CapabilityChannel {
        action: CapabilityAction,
    },
    Rename {
        target: String,
        name: String,
    },
    Kill {
        target: String,
    },
    Send {
        target: String,
        #[serde(with = "send_data_serde")]
        data: Vec<u8>,
    },
    Capture {
        target: String,
        start: Option<i32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        end: Option<i32>,
    },
    WaitExit {
        target: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
    },
    WaitContains {
        target: String,
        needle: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        start: Option<i32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
    },
    Resize {
        target: String,
        rows: u16,
        cols: u16,
        /// PR #15: 호출자 attach client 의 subscriber id.
        ///
        /// **`Some(id)` (per-attach 경로)**: server 는 해당 subscriber 의
        /// per-client geometry 를 갱신한 뒤 attach 중인 모든 클라이언트의
        /// `min(rows)` × `min(cols)` 로 PTY 사이즈를 재계산한다 (clamp-to-smallest).
        /// 클라이언트 SIGWINCH/리사이즈 폴링은 반드시 이 경로를 사용해야 한다.
        ///
        /// **`None` (legacy 직접경로)**: `lterm resize` CLI / tmux-compat shim 처럼
        /// attach 가 아닌 컨트롤 채널이 직접 PTY 사이즈를 강제하는 케이스. server 는
        /// per-client geometry 추적을 건너뛰고 즉시 `master.resize` 한다. **주의**:
        /// 살아있는 attach 가 있는 동안에 `None` 으로 호출하면, 다음 subscribe /
        /// unsubscribe / `Some(id)` Resize 이벤트에서 clamp-to-smallest 가 PTY
        /// 사이즈를 다시 attach 의 min 으로 덮어쓴다. 즉 `None` 은 attach 가 0 명일
        /// 때나, 호출자가 이 override race 를 의도적으로 받아들일 때만 안전하다.
        ///
        /// `#[serde(default)]` 로 두어 구버전 와이어 페이로드와도 호환된다.
        #[serde(default)]
        subscriber_id: Option<u64>,
    },
    Attach {
        target: String,
        /// PR #15: attach 시점의 클라이언트 로컬 터미널 행 수 (status row 차감 후의
        /// PTY rows). `subscribe_with_snapshot` 이 이 값을 Subscriber 에 박아두고
        /// clamp-to-smallest 정책의 인풋으로 사용한다.
        ///
        /// PR #15 quad-review MEDIUM 후속(#4): `#[serde(default)]` 로 두어 미래의
        /// 프로토콜 버저닝 변경과 dev-loop 의 stale daemon (구 lterm 바이너리) 케이스
        /// 와 forward/backward 호환을 유지한다. 새 클라이언트 → 구 daemon 시 daemon
        /// 의 직렬화기는 `rows`/`cols` 를 무시하고 구버전 핸들러로 진입한다 — 이때는
        /// daemon 이 attach 를 거부할 수도, 정상 attach 할 수도 있는데 동작은 구
        /// daemon 에 종속된다. 구 클라이언트 → 새 daemon 시 양 필드는 `0` 으로
        /// default 되어 `subscribe_with_snapshot` 의 zero-rejection guard 가 친절한
        /// 메시지로 attach 를 차단한다 (보통 lterm 재빌드 누락이 원인).
        #[serde(default)]
        rows: u16,
        /// PR #15: attach 시점의 클라이언트 로컬 터미널 열 수. `rows` 와 동일한
        /// `#[serde(default)]` 정책 적용.
        #[serde(default)]
        cols: u16,
    },
    AttachOrNew {
        target: String,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        parent_pane_id: Option<String>,
        #[serde(default)]
        parent_token: Option<String>,
        #[serde(default)]
        env: HashMap<String, String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status_theme: Option<StatusTheme>,
    },
    SetStatusTheme {
        target: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status_theme: Option<StatusTheme>,
    },
    Shutdown,
}

mod send_data_serde {
    use super::MAX_SEND_DATA_BYTES;
    use serde::de::{self, SeqAccess, Visitor};
    use serde::{Deserializer, Serializer};
    use std::fmt;

    const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn serialize<S>(data: &[u8], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&encode_base64(data))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(SendDataVisitor)
    }

    struct SendDataVisitor;

    impl<'de> Visitor<'de> for SendDataVisitor {
        type Value = Vec<u8>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("base64 string or legacy byte array for send data")
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            decode_base64(value).map_err(E::custom)
        }

        fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            self.visit_str(&value)
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            let mut bytes =
                Vec::with_capacity(seq.size_hint().unwrap_or(0).min(MAX_SEND_DATA_BYTES));
            while let Some(byte) = seq.next_element::<u8>()? {
                if bytes.len() >= MAX_SEND_DATA_BYTES {
                    return Err(de::Error::custom(format!(
                        "send data exceeds {MAX_SEND_DATA_BYTES} bytes"
                    )));
                }
                bytes.push(byte);
            }
            Ok(bytes)
        }
    }

    pub(super) fn encode_base64(data: &[u8]) -> String {
        let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
        for chunk in data.chunks(3) {
            let b0 = chunk[0];
            let b1 = *chunk.get(1).unwrap_or(&0);
            let b2 = *chunk.get(2).unwrap_or(&0);

            out.push(BASE64[(b0 >> 2) as usize] as char);
            out.push(BASE64[(((b0 & 0b0000_0011) << 4) | (b1 >> 4)) as usize] as char);
            if chunk.len() > 1 {
                out.push(BASE64[(((b1 & 0b0000_1111) << 2) | (b2 >> 6)) as usize] as char);
            } else {
                out.push('=');
            }
            if chunk.len() > 2 {
                out.push(BASE64[(b2 & 0b0011_1111) as usize] as char);
            } else {
                out.push('=');
            }
        }
        out
    }

    pub(super) fn decode_base64(value: &str) -> Result<Vec<u8>, String> {
        if !value.is_ascii() {
            return Err("base64 data must be ASCII".to_string());
        }
        let trimmed = value.trim_end_matches('=');
        let padding = value.len() - trimmed.len();
        if value[..trimmed.len()].contains('=') {
            return Err("base64 padding may only appear at the end".to_string());
        }
        if padding > 2 || (padding > 0 && value.len() % 4 != 0) || trimmed.len() % 4 == 1 {
            return Err("invalid base64 length".to_string());
        }

        let max_decoded = (trimmed.len() / 4) * 3
            + match trimmed.len() % 4 {
                0 => 0,
                2 => 1,
                3 => 2,
                _ => return Err("invalid base64 length".to_string()),
            };
        if max_decoded > MAX_SEND_DATA_BYTES {
            return Err(format!("send data exceeds {MAX_SEND_DATA_BYTES} bytes"));
        }

        let mut out = Vec::with_capacity(max_decoded);
        let mut quartet = [0_u8; 4];
        let mut quartet_len = 0;
        for byte in trimmed.bytes() {
            quartet[quartet_len] = decode_char(byte)?;
            quartet_len += 1;
            if quartet_len == 4 {
                push_decoded_quartet(&mut out, quartet, 3)?;
                quartet_len = 0;
            }
        }

        match quartet_len {
            0 => {}
            2 => {
                if quartet[1] & 0x0f != 0 {
                    return Err("non-canonical base64 tail bits".to_string());
                }
                push_decoded_quartet(&mut out, quartet, 1)?;
            }
            3 => {
                if quartet[2] & 0x03 != 0 {
                    return Err("non-canonical base64 tail bits".to_string());
                }
                push_decoded_quartet(&mut out, quartet, 2)?;
            }
            _ => return Err("invalid base64 length".to_string()),
        }
        Ok(out)
    }

    fn push_decoded_quartet(
        out: &mut Vec<u8>,
        quartet: [u8; 4],
        bytes: usize,
    ) -> Result<(), String> {
        if out.len() + bytes > MAX_SEND_DATA_BYTES {
            return Err(format!("send data exceeds {MAX_SEND_DATA_BYTES} bytes"));
        }
        out.push((quartet[0] << 2) | (quartet[1] >> 4));
        if bytes > 1 {
            out.push((quartet[1] << 4) | (quartet[2] >> 2));
        }
        if bytes > 2 {
            out.push((quartet[2] << 6) | quartet[3]);
        }
        Ok(())
    }

    fn decode_char(byte: u8) -> Result<u8, String> {
        match byte {
            b'A'..=b'Z' => Ok(byte - b'A'),
            b'a'..=b'z' => Ok(byte - b'a' + 26),
            b'0'..=b'9' => Ok(byte - b'0' + 52),
            b'+' => Ok(62),
            b'/' => Ok(63),
            _ => Err(format!("invalid base64 character 0x{byte:02x}")),
        }
    }
}

mod capability_data_serde {
    use super::{MAX_CAPABILITY_INPUT_BYTES, send_data_serde};
    use serde::de::{self, SeqAccess, Visitor};
    use serde::{Deserializer, Serializer};
    use std::fmt;

    pub fn serialize<S>(data: &[u8], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&send_data_serde::encode_base64(data))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(CapabilityDataVisitor)
    }

    struct CapabilityDataVisitor;

    impl<'de> Visitor<'de> for CapabilityDataVisitor {
        type Value = Vec<u8>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("bounded base64 capability input")
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            let max_encoded = MAX_CAPABILITY_INPUT_BYTES.div_ceil(3) * 4;
            if value.len() > max_encoded {
                return Err(E::custom(format!(
                    "capability input exceeds {MAX_CAPABILITY_INPUT_BYTES} bytes"
                )));
            }
            let bytes = send_data_serde::decode_base64(value).map_err(E::custom)?;
            if bytes.len() > MAX_CAPABILITY_INPUT_BYTES {
                return Err(E::custom(format!(
                    "capability input exceeds {MAX_CAPABILITY_INPUT_BYTES} bytes"
                )));
            }
            Ok(bytes)
        }

        fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            self.visit_str(&value)
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            let mut bytes =
                Vec::with_capacity(seq.size_hint().unwrap_or(0).min(MAX_CAPABILITY_INPUT_BYTES));
            while let Some(byte) = seq.next_element::<u8>()? {
                if bytes.len() >= MAX_CAPABILITY_INPUT_BYTES {
                    return Err(de::Error::custom(format!(
                        "capability input exceeds {MAX_CAPABILITY_INPUT_BYTES} bytes"
                    )));
                }
                bytes.push(byte);
            }
            Ok(bytes)
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
}

impl fmt::Debug for Response {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Response")
            .field("ok", &self.ok)
            .field("error_present", &self.error.is_some())
            .field("result_present", &self.result.is_some())
            .finish()
    }
}

impl Response {
    pub fn ok(result: impl Serialize) -> Self {
        Self {
            ok: true,
            error: None,
            result: Some(
                serde_json::to_value(result)
                    .expect("serializing lterm protocol response should be infallible"),
            ),
        }
    }

    pub fn empty() -> Self {
        Self {
            ok: true,
            error: None,
            result: None,
        }
    }

    pub fn err(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(message.into()),
            result: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CapabilityAction, CapabilityToken, ExitEvidenceState, ExitListScope, ExitOutcomeState,
        InstrumentSnapshot, MAX_CAPABILITY_INPUT_BYTES, MAX_METADATA_JOURNAL_ENTRIES,
        MAX_RECENT_EXITS_LIMIT, MAX_SEND_DATA_BYTES, MetadataHistoryResult, MetadataJournalEntry,
        MetadataOperation, MetadataPurgeAggregate, MetadataValue, RecentSessionExit, Request,
        SensitiveCapabilityRequest, SessionExitTrigger, SessionInfo, SessionLifecycleState,
        StatusTheme,
    };

    #[test]
    fn status_theme_parse_aliases_and_round_trips_canonical_names() {
        for (input, theme, canonical) in [
            (" blue ", StatusTheme::Blue, "blue"),
            ("GREEN", StatusTheme::Green, "green"),
            ("purple", StatusTheme::Magenta, "magenta"),
            ("yellow", StatusTheme::Amber, "amber"),
            ("grey", StatusTheme::Gray, "gray"),
            ("minimal", StatusTheme::Plain, "plain"),
            ("red", StatusTheme::Red, "red"),
            ("cyan", StatusTheme::Cyan, "cyan"),
        ] {
            assert_eq!(StatusTheme::parse(input), Some(theme), "{input:?}");
            assert_eq!(theme.as_str(), canonical, "{input:?}");
        }
        assert_eq!(StatusTheme::parse("unknown"), None);
        assert!(StatusTheme::allowed_values().contains("purple"));
        assert!(StatusTheme::allowed_values().contains("minimal"));
    }

    #[test]
    fn session_info_accepts_pre_process_metadata_json() {
        let info: SessionInfo = serde_json::from_str(
            r#"{
                "id": "session-id",
                "name": "api",
                "pane_id": "%0",
                "command": "sh",
                "cwd": "/tmp",
                "created_unix_ms": 1,
                "alive": true,
                "exit_code": null,
                "rows": 24,
                "cols": 80
            }"#,
        )
        .expect("deserialize legacy SessionInfo without process metadata");

        assert_eq!(info.process_id, None);
        assert_eq!(info.process_group_id, None);
        assert_eq!(info.parent_pane_id, None);
        assert_eq!(info.parent_session_id, None);
        assert_eq!(info.attached_clients, 0);
        assert_eq!(info.status_theme, None);
        assert_eq!(info.lifecycle_state, None);
    }

    #[test]
    fn lifecycle_request_and_raw_free_exit_summary_round_trip() {
        let request = Request::RecentExits {
            target: Some("opaque-session-id".to_string()),
            limit: MAX_RECENT_EXITS_LIMIT,
            scope: ExitListScope::All,
        };
        let request_value = serde_json::to_value(&request).expect("serialize recent exits request");
        assert_eq!(
            request_value,
            serde_json::json!({
                "type": "recent_exits",
                "target": "opaque-session-id",
                "limit": 100,
                "scope": "all"
            })
        );

        let exit = RecentSessionExit {
            schema_version: "1.0".to_string(),
            session_id: "opaque-session-id".to_string(),
            name: "agent".to_string(),
            pane_id: "%7".to_string(),
            parent_session_id: None,
            parent_pane_id: None,
            agent_name: Some("codex".to_string()),
            created_unix_ms: 10,
            trigger_claimed_unix_ms: 20,
            reaped_unix_ms: Some(30),
            trigger: SessionExitTrigger::LeaderExited,
            outcome_state: ExitOutcomeState::Complete,
            exit_code: Some(37),
            signal: None,
            evidence_state: ExitEvidenceState::Complete,
        };
        let value = serde_json::to_value(&exit).expect("serialize recent exit");
        let object = value.as_object().expect("recent exit object");
        for forbidden in [
            "command",
            "cwd",
            "environment",
            "output",
            "scrollback",
            "capability_token",
            "parent_token",
            "process_id",
            "process_group_id",
        ] {
            assert!(!object.contains_key(forbidden), "forbidden key {forbidden}");
        }
        let decoded: RecentSessionExit =
            serde_json::from_value(value).expect("round trip recent exit");
        assert_eq!(decoded, exit);
    }

    #[test]
    fn session_lifecycle_state_is_optional_and_controls_live_work_presentation() {
        let mut info: SessionInfo = serde_json::from_str(
            r#"{
                "id":"id","name":"name","pane_id":"%1","command":"sh","cwd":"/tmp",
                "created_unix_ms":1,"alive":true,"exit_code":null,"rows":24,"cols":80
            }"#,
        )
        .expect("legacy session info");
        assert!(info.is_live_work());
        assert_eq!(info.lifecycle_state(), SessionLifecycleState::Healthy);

        info.lifecycle_state = Some(SessionLifecycleState::MonitorFailed);
        assert!(info.is_live_work(), "monitor-failed work remains listable");
        assert_eq!(info.lifecycle_state_label(), "degraded");

        info.alive = false;
        info.lifecycle_state = Some(SessionLifecycleState::Ending {
            trigger: SessionExitTrigger::CloseRequested,
        });
        assert!(
            !info.is_live_work(),
            "ending work is never reconnectable/listable"
        );
        assert_eq!(info.lifecycle_state_label(), "ending");
    }

    #[test]
    fn instrument_request_and_snapshot_round_trip_with_exact_raw_free_keys() {
        let request = Request::Instrument {
            target: "%7".to_string(),
        };
        let request_value = serde_json::to_value(&request).expect("serialize instrument request");
        assert_eq!(
            request_value,
            serde_json::json!({"type": "instrument", "target": "%7"})
        );
        let round_trip: Request =
            serde_json::from_value(request_value).expect("deserialize instrument request");
        assert!(matches!(
            round_trip,
            Request::Instrument { target } if target == "%7"
        ));

        let snapshot = InstrumentSnapshot {
            schema_version: "1.0".to_string(),
            observed_unix_ms: 42,
            session_id: "opaque-session-id".to_string(),
            pane_id: "%7".to_string(),
            alive: true,
            output_closed: false,
            output_revision: 3,
            output_total_bytes: 17,
            attached_clients: 1,
            rows: 24,
            cols: 80,
        };
        let value = serde_json::to_value(&snapshot).expect("serialize instrument snapshot");
        let keys = value
            .as_object()
            .expect("instrument snapshot object")
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            keys,
            [
                "alive",
                "attached_clients",
                "cols",
                "observed_unix_ms",
                "output_closed",
                "output_revision",
                "output_total_bytes",
                "pane_id",
                "rows",
                "schema_version",
                "session_id",
            ]
            .into_iter()
            .collect()
        );
        for forbidden in ["name", "command", "cwd", "env", "output", "bytes"] {
            assert!(value.get(forbidden).is_none(), "leaked field {forbidden}");
        }
        let decoded: InstrumentSnapshot =
            serde_json::from_value(value).expect("deserialize instrument snapshot");
        assert_eq!(decoded, snapshot);
    }

    #[test]
    fn metadata_history_round_trip_has_exact_raw_free_allowlist() {
        let request = Request::MetadataPurgeHistory {
            target: "%7".to_string(),
            irreversible: true,
            session_id: "123e4567-e89b-42d3-a456-426614174000".to_string(),
        };
        let round_trip: Request = serde_json::from_value(
            serde_json::to_value(&request).expect("serialize metadata request"),
        )
        .expect("deserialize metadata request");
        assert!(matches!(
            round_trip,
            Request::MetadataPurgeHistory {
                target,
                irreversible: true,
                session_id,
            } if target == "%7" && session_id == "123e4567-e89b-42d3-a456-426614174000"
        ));

        let history = MetadataHistoryResult {
            schema_version: "1.0".to_string(),
            session_id: "opaque-session-id".to_string(),
            pane_id: "%7".to_string(),
            current: MetadataValue {
                name: "example".to_string(),
                status_theme: Some(StatusTheme::Blue),
            },
            entries: vec![MetadataJournalEntry {
                operation: MetadataOperation::Rename,
                before: MetadataValue {
                    name: "old".to_string(),
                    status_theme: None,
                },
                after: MetadataValue {
                    name: "example".to_string(),
                    status_theme: None,
                },
            }],
            cursor: 1,
            capacity: MAX_METADATA_JOURNAL_ENTRIES,
            purge: MetadataPurgeAggregate {
                generation: 0,
                purged_entries_total: 0,
                last_purged_unix_ms: None,
            },
        };
        let value = serde_json::to_value(&history).expect("serialize metadata history");
        let keys = value
            .as_object()
            .expect("metadata history object")
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            keys,
            [
                "capacity",
                "current",
                "cursor",
                "entries",
                "pane_id",
                "purge",
                "schema_version",
                "session_id",
            ]
            .into_iter()
            .collect()
        );
        let encoded = value.to_string();
        for forbidden in [
            "command",
            "cwd",
            "environment",
            "output",
            "scrollback",
            "token",
            "process_id",
        ] {
            assert!(!encoded.contains(forbidden), "leaked field {forbidden}");
        }
        let decoded: MetadataHistoryResult =
            serde_json::from_value(value).expect("deserialize metadata history");
        assert_eq!(decoded, history);
    }

    #[test]
    fn send_request_serializes_data_as_base64_string() {
        let request = Request::Send {
            target: "%0".to_string(),
            data: b"hello\r\n\0".to_vec(),
        };
        let value = serde_json::to_value(&request).expect("serialize send request");
        assert_eq!(value["data"], "aGVsbG8NCgA=");

        let round_trip: Request = serde_json::from_value(value).expect("deserialize send request");
        assert!(matches!(
            round_trip,
            Request::Send { data, .. } if data == b"hello\r\n\0"
        ));
    }

    #[test]
    fn capability_frames_are_separate_and_token_debug_is_redacted() {
        let token =
            CapabilityToken::from_canonical("123e4567-e89b-42d3-a456-426614174000".to_string())
                .expect("canonical v4-shaped UUID");
        assert_eq!(format!("{token:?}"), "CapabilityToken([REDACTED])");
        assert!(!format!("{token:?}").contains(token.as_str()));

        let hello = serde_json::to_value(Request::CapabilityChannel {
            action: CapabilityAction::Input,
        })
        .expect("serialize nonsecret hello");
        assert_eq!(
            hello,
            serde_json::json!({"type":"capability_channel","action":"input"})
        );
        assert!(!hello.to_string().contains(token.as_str()));

        let sensitive = SensitiveCapabilityRequest::Input {
            token: token.clone(),
            data: b"\0\xff\r\n\x1b".to_vec(),
        };
        let debug = format!("{sensitive:?}");
        assert!(debug.contains("data_len"));
        assert!(!debug.contains(token.as_str()));
        assert!(!debug.contains("xff"));
        let value = serde_json::to_value(&sensitive).expect("serialize sensitive input");
        let decoded: SensitiveCapabilityRequest =
            serde_json::from_value(value).expect("deserialize sensitive input");
        assert!(
            matches!(decoded, SensitiveCapabilityRequest::Input { token: actual, data } if actual == token && data == b"\0\xff\r\n\x1b")
        );

        let issued = super::IssueInputCapabilityResult {
            token: token.clone(),
            byte_budget: 8,
        };
        let response = super::Response::ok(issued);
        let response_debug = format!("{response:?}");
        assert_eq!(
            response_debug,
            "Response { ok: true, error_present: false, result_present: true }"
        );
        assert!(!response_debug.contains(token.as_str()));
    }

    #[test]
    fn sensitive_capability_input_rejects_over_64k_during_decode() {
        let encoded = "A".repeat((MAX_CAPABILITY_INPUT_BYTES + 1).div_ceil(3) * 4);
        let err = serde_json::from_value::<SensitiveCapabilityRequest>(serde_json::json!({
            "type": "input",
            "token": "123e4567-e89b-42d3-a456-426614174000",
            "data": encoded,
        }))
        .expect_err("oversized capability input should fail");
        assert!(err.to_string().contains("capability input exceeds"));
    }

    #[test]
    fn send_request_deserializes_legacy_byte_array() {
        let request: Request = serde_json::from_value(serde_json::json!({
            "type": "send",
            "target": "%0",
            "data": [104, 105, 13]
        }))
        .expect("deserialize legacy send array");

        assert!(matches!(request, Request::Send { data, .. } if data == b"hi\r"));
    }

    #[test]
    fn send_request_rejects_oversized_base64_data() {
        let oversized = "A".repeat(((MAX_SEND_DATA_BYTES + 1).div_ceil(3)) * 4);
        let err = serde_json::from_value::<Request>(serde_json::json!({
            "type": "send",
            "target": "%0",
            "data": oversized
        }))
        .expect_err("oversized send data should fail");

        assert!(err.to_string().contains("send data exceeds"));
    }

    #[test]
    fn send_request_rejects_non_canonical_base64_tail_bits() {
        for data in ["AB==", "AAB="] {
            let err = serde_json::from_value::<Request>(serde_json::json!({
                "type": "send",
                "target": "%0",
                "data": data
            }))
            .expect_err("non-canonical base64 tail bits should fail");

            assert!(
                err.to_string().contains("non-canonical base64 tail bits"),
                "unexpected error for {data:?}: {err}"
            );
        }
    }

    #[test]
    fn send_request_rejects_non_ascii_base64_data() {
        let err = serde_json::from_value::<Request>(serde_json::json!({
            "type": "send",
            "target": "%0",
            "data": "안녕"
        }))
        .expect_err("non-ascii base64 should fail");

        assert!(err.to_string().contains("base64 data must be ASCII"));
    }

    #[test]
    fn capture_request_end_is_optional_on_the_wire() {
        let request = Request::Capture {
            target: "%0".to_string(),
            start: None,
            end: None,
        };
        let value = serde_json::to_value(&request).expect("serialize capture request");

        assert!(
            value.get("end").is_none(),
            "new clients should omit absent capture end fields for older daemons: {value}"
        );

        let legacy_request: Request = serde_json::from_value(serde_json::json!({
            "type": "capture",
            "target": "%0",
            "start": null
        }))
        .expect("deserialize capture request without end");

        assert!(matches!(legacy_request, Request::Capture { end: None, .. }));
    }
}
