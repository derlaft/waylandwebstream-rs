use anyhow::Result;
use std::time::{Duration, Instant};
use tokio::time;
use tracing::{debug, warn};

use super::{EncoderHandle, RawFrame};

/// Frame capture and pacing controller
pub struct FrameCapture {
    target_framerate: u32,
    frame_interval: Duration,
    last_frame_time: Instant,
    frame_number: i64,
    dropped_frames: u64,
}

impl FrameCapture {
    /// Create a new frame capture instance
    pub fn new(target_framerate: u32) -> Self {
        let frame_interval = Duration::from_secs_f64(1.0 / target_framerate as f64);
        
        Self {
            target_framerate,
            frame_interval,
            last_frame_time: Instant::now(),
            frame_number: 0,
            dropped_frames: 0,
        }
    }

    /// Capture and send a frame to the encoder
    /// Returns true if frame was sent, false if dropped
    pub async fn capture_and_encode(
        &mut self,
        encoder: &EncoderHandle,
        framebuffer: Vec<u8>,
    ) -> Result<bool> {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_frame_time);

        // Check if enough time has passed for the next frame
        if elapsed < self.frame_interval {
            // Too early, skip this frame
            return Ok(false);
        }

        // Calculate PTS (90kHz clock for video)
        let timestamp = self.frame_number * 90000 / self.target_framerate as i64;

        let frame = RawFrame {
            data: framebuffer,
            timestamp,
            capture_time: std::time::Instant::now(),
        };

        // Try to send frame (non-blocking)
        match encoder.try_send_frame(frame) {
            Ok(_) => {
                self.last_frame_time = now;
                self.frame_number += 1;
                
                if self.frame_number % 300 == 0 {
                    debug!(
                        "Encoded {} frames, dropped {} frames",
                        self.frame_number, self.dropped_frames
                    );
                }
                
                Ok(true)
            }
            Err(_) => {
                // Encoder queue is full, drop frame
                self.dropped_frames += 1;
                
                if self.dropped_frames % 10 == 0 {
                    warn!(
                        "Encoder queue full, dropped {} frames total",
                        self.dropped_frames
                    );
                }
                
                Ok(false)
            }
        }
    }

    /// Wait for the next frame interval
    pub async fn wait_next_frame(&self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_frame_time);
        
        if elapsed < self.frame_interval {
            let sleep_duration = self.frame_interval - elapsed;
            time::sleep(sleep_duration).await;
        }
    }

    /// Reset frame timing (useful after resolution changes)
    pub fn reset_timing(&mut self) {
        self.last_frame_time = Instant::now();
    }

    /// Update target framerate
    pub fn set_framerate(&mut self, framerate: u32) {
        self.target_framerate = framerate;
        self.frame_interval = Duration::from_secs_f64(1.0 / framerate as f64);
        self.reset_timing();
    }

    /// Get statistics
    pub fn stats(&self) -> FrameCaptureStats {
        FrameCaptureStats {
            total_frames: self.frame_number,
            dropped_frames: self.dropped_frames,
            target_framerate: self.target_framerate,
        }
    }
}

/// Frame capture statistics
#[derive(Debug, Clone)]
pub struct FrameCaptureStats {
    pub total_frames: i64,
    pub dropped_frames: u64,
    pub target_framerate: u32,
}
