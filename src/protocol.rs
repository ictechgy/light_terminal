use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Ping,
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
        tmux: bool,
    },
    List,
    Info {
        target: String,
    },
    Kill {
        target: String,
    },
    Send {
        target: String,
        data: Vec<u8>,
    },
    Capture {
        target: String,
        start: Option<i32>,
        #[serde(default)]
        end: Option<i32>,
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
    },
    Shutdown,
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
    use super::SessionInfo;

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
    }
}
