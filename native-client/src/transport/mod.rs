// Transport abstraction over the framed binary protocol. The same `Frame`
// enum and `Transport` trait are used regardless of whether the underlying
// transport is a WebSocket, a Unix socket, or stdio -- the proto header
// is identical on all of them. See docs/native-client-plan.md Part 3.3.

use anyhow::Result;

use crate::types::ServerMessage;

/// One decoded message from the server. Frames hold owned `Vec<u8>` for the
/// payload because the wire bytes are produced by the encoder/capture path
/// and we never need to share them with another consumer.
#[derive(Debug)]
pub enum Frame {
    /// Encoded H.264 video frame. `is_keyframe` is read from the header
    /// `flags` byte rather than carried inline.
    VideoFrame {
        is_keyframe: bool,
        frame_id: u32,
        /// Echoes the client's last `Ping{client_ts}`; `0.0` when the
        /// header's `FLAG_HAS_PING` bit is clear.
        ping_echo: f64,
        capture_to_encode_ms: f64,
        data: Vec<u8>,
    },
    AudioFrame {
        pts_us: u64,
        data: Vec<u8>,
    },
    Control(ServerMessage),
}

/// Errors specific to the wire-protocol parser. Distinct from transport
/// errors so callers can tell "the connection broke" apart from "the
/// server sent a malformed frame".
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("transport closed")]
    Closed,
    #[error("bad frame header: {0}")]
    BadHeader(String),
    #[error("unknown message type 0x{0:02x}")]
    UnknownType(u8),
    #[error("control payload was not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
}

/// Transport trait. `recv` returns a fully-decoded `Frame`; `send` wraps a
/// JSON `SignalingMessage` in a `MSG_CLIENT_MSG` framed message before
/// shipping it -- callers don't see the proto header.
#[allow(async_fn_in_trait)] // no Send bound needed; both impls are single-threaded
pub trait Transport: Send {
    async fn recv(&mut self) -> Result<Frame>;
    async fn send(&mut self, json: &str) -> Result<()>;
}

pub mod websocket;