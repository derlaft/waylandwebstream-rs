use serde::{Deserialize, Serialize};

/// Detailed latency breakdown for end-to-end performance tracking
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LatencyReport {
    /// Time from input event to frame capture (input processing latency)
    pub input_ms: Option<f64>,

    /// Time from frame capture to encoding start
    pub capture_to_encode_ms: Option<f64>,

    /// Time spent encoding the frame
    pub encoding_ms: Option<f64>,

    /// Time from encoding complete to network send
    pub encode_to_send_ms: Option<f64>,

    /// Network round-trip time / 2 (one-way network latency)
    pub network_ms: Option<f64>,

    /// Jitter buffer delay on client side
    pub jitter_buffer_ms: Option<f64>,

    /// Time from packet arrival to decode start
    pub receive_to_decode_ms: Option<f64>,

    /// Time spent decoding the frame
    pub decoding_ms: Option<f64>,

    /// Time from decode to display
    pub decode_to_display_ms: Option<f64>,

    /// Total end-to-end latency (sum of all components)
    pub total_ms: f64,

    /// Timestamp when this report was generated
    pub timestamp: std::time::SystemTime,
}

impl LatencyReport {
    /// Create a report with all fields set to None
    pub fn new() -> Self {
        Self {
            input_ms: None,
            capture_to_encode_ms: None,
            encoding_ms: None,
            encode_to_send_ms: None,
            network_ms: None,
            jitter_buffer_ms: None,
            receive_to_decode_ms: None,
            decoding_ms: None,
            decode_to_display_ms: None,
            total_ms: 0.0,
            timestamp: std::time::SystemTime::now(),
        }
    }
}

impl Default for LatencyReport {
    fn default() -> Self {
        Self::new()
    }
}
