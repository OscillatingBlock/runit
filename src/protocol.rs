//! Wire protocol shared between the init system (orchastrator) and the CLI client.
//!
//! Framing in both directions is identical:
//!   1. a 4-byte big-endian length prefix,
//!   2. followed by the JSON-serialized message of exactly that many bytes.

use anyhow::Context;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};

/// Size of the length prefix in bytes.
const HEADER_LEN: usize = 4;
/// Upper bound for a single message to guard against a huge length prefix.
const MAX_MESSAGE_LEN: u32 = 16 * 1024 * 1024;

/// Commands a client can send to the init system over the unix socket.
#[derive(Debug, Deserialize, Serialize)]
pub enum CliCommand {
    Status,
    Reload { pid: i32 },
    Restart,
    Stop { pid: i32 },
    Shutdown,
}

/// Replies the init system sends back after handling a command.
#[derive(Debug, Deserialize, Serialize)]
pub enum CliResponse {
    Error { error: String },
    StatusResponse { status: String },
    RestartSuccess,
    KillSuccess { pid: i32 },
    ReloadSuccess { old_pid: i32, new_pid: i32 },
    ShutdownSuccess,
}

/// Length-prefixes `msg` and writes it to `stream`.
pub fn write_message<T: Serialize>(mut stream: impl Write, msg: &T) -> anyhow::Result<()> {
    let json = serde_json::to_vec(msg).context("failed to serialize message")?;
    let len = u32::try_from(json.len()).context("message payload exceeds u32::MAX")?;

    stream
        .write_all(&len.to_be_bytes())
        .context("failed to write length prefix")?;
    stream
        .write_all(&json)
        .context("failed to write message payload")?;

    Ok(())
}

/// Reads a length-prefixed message from `stream` and deserializes it.
pub fn read_message<T: DeserializeOwned>(mut stream: impl Read) -> anyhow::Result<T> {
    let mut header = [0u8; HEADER_LEN];
    stream
        .read_exact(&mut header)
        .context("failed to read length prefix")?;

    let len = u32::from_be_bytes(header);
    if len > MAX_MESSAGE_LEN {
        anyhow::bail!("message length prefix of {len} bytes exceeds the allowed maximum");
    }

    let mut json = vec![0u8; len as usize];
    stream
        .read_exact(&mut json)
        .context("failed to read message payload")?;

    serde_json::from_slice(&json).context("failed to parse message payload - unknown format")
}
