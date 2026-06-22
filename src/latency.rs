use serde::{Deserialize, Serialize};
use std::time::Instant;

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
    /// Calculate total latency from available components
    pub fn calculate_total(&self) -> f64 {
        let mut total = 0.0;
        
        if let Some(v) = self.input_ms { total += v; }
        if let Some(v) = self.capture_to_encode_ms { total += v; }
        if let Some(v) = self.encoding_ms { total += v; }
        if let Some(v) = self.encode_to_send_ms { total += v; }
        if let Some(v) = self.network_ms { total += v; }
        if let Some(v) = self.jitter_buffer_ms { total += v; }
        if let Some(v) = self.receive_to_decode_ms { total += v; }
        if let Some(v) = self.decoding_ms { total += v; }
        if let Some(v) = self.decode_to_display_ms { total += v; }
        
        total
    }
    
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
    
    /// Print a human-readable breakdown of latency
    pub fn print_breakdown(&self) {
        println!("=== Latency Breakdown ===");
        
        if let Some(v) = self.input_ms {
            println!("  Input processing:      {:>6.1} ms", v);
        }
        if let Some(v) = self.capture_to_encode_ms {
            println!("  Capture → Encode:      {:>6.1} ms", v);
        }
        if let Some(v) = self.encoding_ms {
            println!("  Encoding:              {:>6.1} ms", v);
        }
        if let Some(v) = self.encode_to_send_ms {
            println!("  Encode → Send:         {:>6.1} ms", v);
        }
        if let Some(v) = self.network_ms {
            println!("  Network (one-way):     {:>6.1} ms", v);
        }
        if let Some(v) = self.jitter_buffer_ms {
            println!("  Jitter buffer:         {:>6.1} ms", v);
        }
        if let Some(v) = self.receive_to_decode_ms {
            println!("  Receive → Decode:      {:>6.1} ms", v);
        }
        if let Some(v) = self.decoding_ms {
            println!("  Decoding:              {:>6.1} ms", v);
        }
        if let Some(v) = self.decode_to_display_ms {
            println!("  Decode → Display:      {:>6.1} ms", v);
        }
        
        println!("  ────────────────────────────");
        println!("  TOTAL:                 {:>6.1} ms", self.total_ms);
        println!("========================");
    }
}

impl Default for LatencyReport {
    fn default() -> Self {
        Self::new()
    }
}

/// Frame timing tracker for measuring encoding latency
#[derive(Clone)]
pub struct FrameTimer {
    pub capture_time: Instant,
    pub frame_number: u64,
}

impl FrameTimer {
    pub fn new(frame_number: u64) -> Self {
        Self {
            capture_time: Instant::now(),
            frame_number,
        }
    }
    
    /// Get time since capture in milliseconds
    pub fn elapsed_ms(&self) -> f64 {
        self.capture_time.elapsed().as_secs_f64() * 1000.0
    }
}
