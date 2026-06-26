// Wire-protocol message types -- byte-for-byte copies of the structs/enums
// in src/server.rs, src/input/{keyboard,mouse,touch}.rs. The native client
// must serialize these to identical JSON, so a shared workspace crate would
// be nicer but a copy is simpler until the types stabilize.
// See docs/native-client-plan.md Part 3.4.

use serde::{Deserialize, Serialize};

/// Signaling messages from the client to the server.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SignalingMessage {
    #[serde(rename = "ready")]
    Ready,
    #[serde(rename = "resize")]
    Resize { width: u32, height: u32 },
    #[serde(rename = "touch")]
    Touch {
        #[serde(flatten)]
        event: TouchEvent,
    },
    #[serde(rename = "pointer")]
    Pointer {
        #[serde(flatten)]
        event: MouseEvent,
    },
    #[serde(rename = "key")]
    Key {
        #[serde(flatten)]
        event: KeyboardEvent,
    },
    /// Decoder fell behind and wants a fresh keyframe to resync.
    #[serde(rename = "request_keyframe")]
    RequestKeyframe,
    #[serde(rename = "latency")]
    Latency {
        #[serde(default)]
        encoding_ms: Option<f64>,
        #[serde(default)]
        network_ms: Option<f64>,
        #[serde(default)]
        jitter_buffer_ms: Option<f64>,
        #[serde(default)]
        decoding_ms: Option<f64>,
        total_ms: f64,
        #[serde(default)]
        burst_count: u32,
        #[serde(default)]
        blit_ms: Option<f64>,
    },
    /// Round-trip latency probe -- server stamps the next frame's
    /// `ping_echo_client_ts` with this value so the client can measure
    /// pipeline latency using only its own clock.
    #[serde(rename = "ping")]
    Ping { client_ts: f64 },
}

/// Messages the server pushes to the client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ServerMessage {
    #[serde(rename = "bitrate")]
    Bitrate { bps: usize },
    /// WebCodecs codec string (profile/level).
    #[serde(rename = "codec")]
    Codec { codec: String },
    /// Current cursor shape from the compositor.
    #[serde(rename = "cursor")]
    Cursor { cursor: CursorUpdate },
}

/// Cursor state pushed from the compositor.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum CursorUpdate {
    #[serde(rename = "default")]
    Default,
    #[serde(rename = "hidden")]
    Hidden,
    #[serde(rename = "named")]
    Named { name: String },
    #[serde(rename = "surface")]
    Surface {
        width: u32,
        height: u32,
        hotspot_x: i32,
        hotspot_y: i32,
        /// Base64-encoded RGBA (not BGRA) pixel data.
        rgba: String,
    },
}

// Pointer (mouse/pen) events from the browser. Touch contacts have their
// own dedicated TouchEvent path; this is only mouse and pen/stylus.

/// Pointer event types from the browser's Pointer Events API.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "eventType")]
pub enum MouseEvent {
    #[serde(rename = "pointerdown")]
    Down { pointer: PointerPoint },
    #[serde(rename = "pointermove")]
    Move { pointer: PointerPoint },
    #[serde(rename = "pointerup")]
    Up { pointer: PointerPoint },
    #[serde(rename = "pointercancel")]
    Cancel { pointer: PointerPoint },
    #[serde(rename = "wheel")]
    Wheel {
        x: f64,
        y: f64,
        #[serde(rename = "deltaX")]
        delta_x: f64,
        #[serde(rename = "deltaY")]
        delta_y: f64,
    },
}

fn default_pointer_type() -> String {
    "mouse".to_string()
}

/// A single pointer sample from the browser.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PointerPoint {
    /// X coordinate relative to the video element (0.0 to 1.0)
    pub x: f64,
    /// Y coordinate relative to the video element (0.0 to 1.0)
    pub y: f64,
    /// `PointerEvent.button` index; only meaningful for down/up.
    #[serde(default)]
    pub button: i32,
    /// "mouse" or "pen" -- touch contacts are routed through TouchEvent instead.
    #[serde(rename = "pointerType", default = "default_pointer_type")]
    pub pointer_type: String,
    /// Pressure/force, 0.0 to 1.0 (meaningful for pen tablets).
    #[serde(default)]
    pub pressure: f64,
}

/// Keyboard event types from the browser's KeyboardEvent API.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "eventType")]
pub enum KeyboardEvent {
    #[serde(rename = "keydown")]
    Down { code: String },
    #[serde(rename = "keyup")]
    Up { code: String },
}

/// Touch event types from the browser.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "eventType")]
pub enum TouchEvent {
    #[serde(rename = "touchstart")]
    Start { touches: Vec<TouchPoint> },
    #[serde(rename = "touchmove")]
    Move { touches: Vec<TouchPoint> },
    #[serde(rename = "touchend")]
    End { touches: Vec<TouchPoint> },
    #[serde(rename = "touchcancel")]
    Cancel { touches: Vec<TouchPoint> },
}

/// A single touch point from the browser.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TouchPoint {
    /// Unique identifier for this touch.
    pub identifier: i32,
    /// X coordinate relative to the video element (0.0 to 1.0)
    pub x: f64,
    /// Y coordinate relative to the video element (0.0 to 1.0)
    pub y: f64,
    /// Pressure/force of the touch (0.0 to 1.0)
    #[serde(default)]
    pub pressure: f64,
}