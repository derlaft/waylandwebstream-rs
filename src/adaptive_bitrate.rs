use anyhow::Result;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::encoder::EncoderControl;

/// Configuration for adaptive bitrate control
#[derive(Clone, Debug)]
pub struct AdaptiveBitrateConfig {
    /// Minimum bitrate in bits per second
    pub min_bitrate: usize,
    /// Maximum bitrate in bits per second
    pub max_bitrate: usize,
    /// Initial bitrate in bits per second
    pub initial_bitrate: usize,
    /// Target latency in milliseconds
    pub target_latency_ms: u64,
    /// Latency threshold above which bitrate should drop (as multiple of target)
    pub latency_threshold_factor: f64,
    /// Rate at which to increase bitrate when latency is good (bps per second)
    pub increase_rate: usize,
    /// Factor by which to decrease bitrate when latency is bad (0.0-1.0)
    pub decrease_factor: f64,
    /// How often to check and adjust bitrate
    pub adjustment_interval: Duration,
}

impl Default for AdaptiveBitrateConfig {
    fn default() -> Self {
        Self {
            min_bitrate: 500_000,        // 500 Kbps minimum
            max_bitrate: 10_000_000,     // 10 Mbps maximum
            initial_bitrate: 2_000_000,  // 2 Mbps starting point
            target_latency_ms: 100,      // Target 100ms latency
            latency_threshold_factor: 1.5, // Drop bitrate if latency > 150ms
            increase_rate: 100_000,      // Increase by 100 Kbps per second when stable
            decrease_factor: 0.7,        // Drop to 70% when latency is bad
            adjustment_interval: Duration::from_millis(500), // Check every 500ms
        }
    }
}

/// Metrics for tracking network and encoder performance
#[derive(Clone, Debug, Default)]
pub struct NetworkMetrics {
    /// Encoder queue depth (number of frames waiting to be encoded)
    pub encoder_queue_depth: usize,
    /// Round-trip time in milliseconds (from RTCP if available)
    pub rtt_ms: Option<u64>,
    /// Packet loss percentage (from RTCP if available)
    pub packet_loss_percent: Option<f64>,
    /// Jitter in milliseconds (from RTCP if available)
    pub jitter_ms: Option<u64>,
    /// Number of frames dropped by encoder
    pub dropped_frames: u64,
}

impl NetworkMetrics {
    /// Estimate current latency based on available metrics
    pub fn estimated_latency_ms(&self) -> u64 {
        // Start with encoder queue depth estimate
        // Assume each frame in queue adds ~16ms at 60fps
        let queue_latency = (self.encoder_queue_depth as u64) * 16;
        
        // Add RTT if available
        let rtt_latency = self.rtt_ms.unwrap_or(0);
        
        // Add jitter if available
        let jitter_latency = self.jitter_ms.unwrap_or(0);
        
        queue_latency + rtt_latency + jitter_latency
    }
    
    /// Check if we have signs of network congestion
    pub fn has_congestion(&self) -> bool {
        // High packet loss indicates congestion
        if let Some(loss) = self.packet_loss_percent {
            if loss > 2.0 {
                return true;
            }
        }
        
        // Large encoder queue indicates we can't keep up
        if self.encoder_queue_depth > 2 {
            return true;
        }
        
        // Dropped frames indicate encoder overload
        if self.dropped_frames > 0 {
            return true;
        }
        
        false
    }
}

/// Adaptive bitrate controller
pub struct AdaptiveBitrateController {
    config: AdaptiveBitrateConfig,
    current_bitrate: usize,
    encoder_control_tx: mpsc::Sender<EncoderControl>,
    metrics_rx: mpsc::Receiver<NetworkMetrics>,
    last_adjustment: Instant,
    last_increase: Instant,
}

impl AdaptiveBitrateController {
    /// Create a new adaptive bitrate controller
    pub fn new(
        config: AdaptiveBitrateConfig,
        encoder_control_tx: mpsc::Sender<EncoderControl>,
        metrics_rx: mpsc::Receiver<NetworkMetrics>,
    ) -> Self {
        let current_bitrate = config.initial_bitrate;
        let now = Instant::now();
        
        Self {
            config,
            current_bitrate,
            encoder_control_tx,
            metrics_rx,
            last_adjustment: now,
            last_increase: now,
        }
    }
    
    /// Run the adaptive bitrate control loop
    pub async fn run(mut self) -> Result<()> {
        info!("Adaptive bitrate controller started");
        info!("  Bitrate range: {} - {} bps", self.config.min_bitrate, self.config.max_bitrate);
        info!("  Target latency: {} ms", self.config.target_latency_ms);
        info!("  Initial bitrate: {} bps", self.current_bitrate);
        info!("  Waiting for metrics...");
        
        let mut interval = tokio::time::interval(self.config.adjustment_interval);
        let mut last_metrics: Option<NetworkMetrics> = None;
        let mut tick_count = 0u32;
        
        loop {
            tokio::select! {
                // Receive updated metrics
                Some(metrics) = self.metrics_rx.recv() => {
                    debug!("Received metrics: latency={} ms, queue_depth={}, dropped={}",
                           metrics.estimated_latency_ms(), 
                           metrics.encoder_queue_depth,
                           metrics.dropped_frames);
                    last_metrics = Some(metrics);
                }
                
                // Periodic adjustment check
                _ = interval.tick() => {
                    tick_count += 1;
                    if let Some(ref metrics) = last_metrics {
                        if let Err(e) = self.adjust_bitrate(metrics).await {
                            warn!("Failed to adjust bitrate: {}", e);
                        }
                    } else if tick_count % 10 == 0 {
                        debug!("No metrics received yet (tick {})", tick_count);
                    }
                }
                
                else => break,
            }
        }
        
        info!("Adaptive bitrate controller stopped");
        Ok(())
    }
    
    /// Adjust bitrate based on current metrics
    async fn adjust_bitrate(&mut self, metrics: &NetworkMetrics) -> Result<()> {
        let now = Instant::now();
        let time_since_adjustment = now.duration_since(self.last_adjustment);
        
        // Don't adjust too frequently
        if time_since_adjustment < self.config.adjustment_interval {
            return Ok(());
        }
        
        let latency_ms = metrics.estimated_latency_ms();
        let target_ms = self.config.target_latency_ms;
        let threshold_ms = (target_ms as f64 * self.config.latency_threshold_factor) as u64;
        
        debug!("Bitrate check: current={} bps, latency={} ms (target={} ms, threshold={} ms)",
               self.current_bitrate, latency_ms, target_ms, threshold_ms);
        
        let new_bitrate = if latency_ms > threshold_ms || metrics.has_congestion() {
            // Latency is too high or we have congestion - decrease bitrate quickly
            let decreased = (self.current_bitrate as f64 * self.config.decrease_factor) as usize;
            let new_rate = decreased.max(self.config.min_bitrate);
            
            if new_rate < self.current_bitrate {
                warn!("High latency detected ({} ms > {} ms) or congestion, dropping bitrate from {} to {} bps",
                      latency_ms, threshold_ms, self.current_bitrate, new_rate);
            }
            
            new_rate
        } else if latency_ms < target_ms {
            // Latency is good - slowly increase bitrate
            let time_since_increase = now.duration_since(self.last_increase).as_secs_f64();
            
            // Only increase if we've been stable for a bit
            if time_since_increase >= 1.0 {
                let increase = (self.config.increase_rate as f64 * time_since_increase) as usize;
                let increased = self.current_bitrate + increase;
                let new_rate = increased.min(self.config.max_bitrate);
                
                if new_rate > self.current_bitrate {
                    info!("Latency stable ({} ms < {} ms), increasing bitrate from {} to {} bps",
                          latency_ms, target_ms, self.current_bitrate, new_rate);
                    self.last_increase = now;
                }
                
                new_rate
            } else {
                self.current_bitrate
            }
        } else {
            // Latency is between target and threshold - maintain current bitrate
            self.current_bitrate
        };
        
        // Apply bitrate change if needed
        if new_bitrate != self.current_bitrate {
            self.encoder_control_tx
                .send(EncoderControl::ChangeBitrate(new_bitrate))
                .await?;
            
            self.current_bitrate = new_bitrate;
            self.last_adjustment = now;
        }
        
        Ok(())
    }
}

/// Metrics collector that monitors encoder and network state
#[derive(Clone)]
pub struct MetricsCollector {
    metrics_tx: mpsc::Sender<NetworkMetrics>,
    update_interval: Duration,
}

impl MetricsCollector {
    /// Create a new metrics collector
    pub fn new(metrics_tx: mpsc::Sender<NetworkMetrics>, update_interval: Duration) -> Self {
        Self {
            metrics_tx,
            update_interval,
        }
    }
    
    /// Report current metrics
    pub async fn report_metrics(&self, metrics: NetworkMetrics) -> Result<()> {
        self.metrics_tx.send(metrics).await?;
        Ok(())
    }
    
    /// Get the update interval
    pub fn update_interval(&self) -> Duration {
        self.update_interval
    }
}
