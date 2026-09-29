//! JSON-lines protocol spoken with the shell plugin over stdin/stdout.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncWriteExt, Stdout};

pub const PROTOCOL: u32 = 1;

#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Unconfigured,
    Connecting,
    Online,
    Offline,
    Auth,
}

#[derive(Serialize)]
pub struct CameraOut<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub model: &'a str,
    pub online: bool,
    pub kinds: &'a [String],
    pub snapshot: &'a str,
    pub seq: u64,
}

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Start,
    Update,
    End,
}

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Warn,
    Error,
}

#[derive(Serialize)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum Msg<'a> {
    Hello {
        version: &'static str,
        protocol: u32,
    },
    Status {
        state: State,
        message: Option<&'a str>,
        protect: Option<&'a str>,
        /// The active address when it is a fallback, not `host`; `null` otherwise.
        via: Option<&'a str>,
    },
    Cameras {
        cameras: Vec<CameraOut<'a>>,
    },
    Snapshot {
        camera: &'a str,
        seq: u64,
        path: &'a str,
    },
    Event {
        phase: Phase,
        id: &'a str,
        camera: &'a str,
        #[serde(rename = "type")]
        event_type: &'a str,
        kinds: &'a [String],
        #[serde(rename = "match")]
        matched: bool,
        start: i64,
        end: Option<i64>,
    },
    Live {
        camera: &'a str,
        ok: bool,
        message: Option<&'a str>,
    },
    Log {
        level: Level,
        message: &'a str,
    },
}

#[derive(Deserialize)]
#[serde(tag = "cmd", rename_all = "lowercase")]
pub enum Command {
    Filter {
        notify: Option<Vec<String>>,
        cameras: Option<Vec<String>>,
        desktop: Option<bool>,
    },
    Watch {
        on: bool,
        interval: Option<f64>,
    },
    Snapshot {
        camera: String,
    },
    Live {
        camera: String,
    },
    Reload,
}

/// Line-oriented stdout writer; flushes after every message.
pub struct Out {
    stdout: Stdout,
    line: Vec<u8>,
}

impl Out {
    pub fn new() -> Self {
        Self { stdout: tokio::io::stdout(), line: Vec::with_capacity(1024) }
    }

    pub async fn send(&mut self, msg: &Msg<'_>) -> Result<()> {
        self.line.clear();
        serde_json::to_writer(&mut self.line, msg)?;
        self.line.push(b'\n');
        self.stdout.write_all(&self.line).await?;
        self.stdout.flush().await?;
        Ok(())
    }
}
