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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Ping,
    New {
        name: Option<String>,
        command: Option<String>,
        cwd: Option<String>,
        rows: Option<u16>,
        cols: Option<u16>,
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
            result: Some(serde_json::to_value(result).unwrap_or(serde_json::Value::Null)),
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
