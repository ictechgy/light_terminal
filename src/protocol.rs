use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Maximum decoded byte length accepted for `Request::Send` payloads.
///
/// The daemon request-frame cap is 1 MiB, while the compact on-wire format is
/// base64 inside JSON. Keep the decoded cap below that frame limit with margin
/// for base64 expansion plus the request envelope.
pub const MAX_SEND_DATA_BYTES: usize = 700 * 1024;
pub const PROTOCOL_VERSION: u32 = 3;
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
        #[serde(default)]
        env: HashMap<String, String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status_theme: Option<StatusTheme>,
        tmux: bool,
    },
    List,
    Info {
        target: String,
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

    fn encode_base64(data: &[u8]) -> String {
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

    fn decode_base64(value: &str) -> Result<Vec<u8>, String> {
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
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
    use super::{MAX_SEND_DATA_BYTES, Request, SessionInfo, StatusTheme};

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
