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
    },
    Resize {
        target: String,
        rows: u16,
        cols: u16,
    },
    Attach {
        target: String,
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
