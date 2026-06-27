//! Adaptive CBR control.
//!
//! Modeled on TCP Reno: slow start probes upward fast until the first
//! congestion signal, then switches to additive-increase / multiplicative-
//! decrease (AIMD) congestion avoidance, remembering the post-cut rate as a
//! `ssthresh` ceiling so future growth stops there before probing further.
//!
//! The congestion signal is the client's bursty-arrival report
//! (`BitrateEvent::ArrivalStall`, derived from `burst_count` in
//! `SignalingMessage::Latency`): several frames landing within milliseconds
//! of each other means a batch queued up somewhere in the network path and
//! released all at once -- loss-equivalent evidence the current rate doesn't
//! fit the path, the same way a dropped TCP segment is evidence a window was
//! too large.
//!
//! A client's keyframe-request (`SignalingMessage::RequestKeyframe`) is
//! deliberately *not* a congestion signal. It fires whenever the client's
//! decode queue backs up, but in the browser that is dominated by transient
//! main-thread stalls (e.g. Firefox's synchronous GPU readback on the
//! VideoFrame->canvas blit), not by the rate being too high -- the native
//! client decodes the same stream without ever backing up. Treating it as
//! congestion let a purely local rendering hiccup cut the shared encoder's
//! rate and then kept it suppressed for seconds while AIMD crawled back. The
//! keyframe request now only forces an IDR so the client can resync; it has
//! no bitrate effect.
//!
//! Client-reported decode latency is a secondary, softer signal: it can't
//! trigger a cut on its own (a single slow decode doesn't mean the rate is
//! wrong), but it holds off growth while elevated so the controller doesn't
//! keep climbing into a backlog that just hasn't shown up as bursty
//! arrival yet.
//!
//! All decisions live in `BitrateAlgorithm`, which takes plain `Instant`s
//! and returns the new target without touching any channel -- this keeps
//! the AIMD logic deterministically testable without real sleeps.
//! `AdaptiveBitrateController` is the thin async wrapper that drives it from
//! real time and an encoder control channel.
//!
//! Note: one encoder feeds every connected `/stream` client (see
//! `SignalingState::video_tx` in `src/server.rs`), so a cut triggered by one
//! struggling client lowers the rate for all of them. That's an existing
//! property of the single shared-encoder broadcast design, not something
//! this controller can address per-client.

use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};
use tracing::{debug, info};

use crate::encoder::EncoderControl;

/// Signal fed into the controller from the server's signaling handlers.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BitrateEvent {
    /// The client reported several frames landing within milliseconds of
    /// each other (see `SignalingMessage::Latency::burst_count` in
    /// src/server.rs): a batch that queued up somewhere in the network path
    /// between server and client and released all at once. This is the
    /// controller's congestion signal -- it means the current rate doesn't
    /// fit the path right now, the same way a dropped TCP segment means a
    /// window was too large.
    ArrivalStall,
    /// A client's self-reported average decode latency (ms) over its most
    /// recent reporting window.
    Latency(f64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    SlowStart,
    CongestionAvoidance,
}

#[derive(Clone, Debug)]
pub struct AdaptiveBitrateConfig {
    pub min_bitrate: usize,
    pub max_bitrate: usize,
    pub initial_bitrate: usize,
    /// Multiplicative cut applied to the current bitrate on a congestion
    /// signal. TCP Reno halves cwnd (0.5); video is less elastic and
    /// x264's VBV cap already bounds the worst-case frame size, so this
    /// defaults to a gentler cut.
    pub decrease_factor: f64,
    /// Minimum spacing between cuts -- coalesces a burst of congestion
    /// signals from the same underlying stall into a single cut, and gives
    /// the new rate a moment to settle before growth resumes.
    pub decrease_cooldown: Duration,
    /// Per-tick multiplicative growth while in slow start.
    pub slow_start_factor: f64,
    /// Per-tick additive growth (bps) once in congestion avoidance.
    pub additive_increase: usize,
    /// How often `tick()` is driven.
    pub adjustment_interval: Duration,
    /// Reported decode latency (ms) above which growth is held off even
    /// without a keyframe request.
    pub latency_ceiling_ms: f64,
}

impl Default for AdaptiveBitrateConfig {
    fn default() -> Self {
        Self {
            min_bitrate: 500_000,
            max_bitrate: 12_000_000,
            initial_bitrate: 2_000_000,
            decrease_factor: 0.75,
            decrease_cooldown: Duration::from_secs(2),
            slow_start_factor: 1.4,
            additive_increase: 150_000,
            adjustment_interval: Duration::from_secs(1),
            latency_ceiling_ms: 150.0,
        }
    }
}

/// Pure AIMD decision logic, separated from the async plumbing so it can be
/// driven with synthetic `Instant`s in tests instead of real sleeps.
pub struct BitrateAlgorithm {
    config: AdaptiveBitrateConfig,
    current_bitrate: usize,
    /// Ceiling slow start grows toward before switching to congestion
    /// avoidance. Starts at `max_bitrate` (i.e. "unknown" -- probe as far
    /// as allowed) and gets pulled down to the post-cut rate on the first
    /// congestion signal, exactly like TCP's ssthresh.
    ssthresh: usize,
    phase: Phase,
    last_decrease: Option<Instant>,
    last_latency_ms: Option<f64>,
}

impl BitrateAlgorithm {
    pub fn new(config: AdaptiveBitrateConfig) -> Self {
        let current_bitrate = config.initial_bitrate;
        let ssthresh = config.max_bitrate;
        Self {
            config,
            current_bitrate,
            ssthresh,
            phase: Phase::SlowStart,
            last_decrease: None,
            last_latency_ms: None,
        }
    }

    pub fn current_bitrate(&self) -> usize {
        self.current_bitrate
    }

    fn in_cooldown(&self, now: Instant) -> bool {
        self.last_decrease
            .is_some_and(|last| now.duration_since(last) < self.config.decrease_cooldown)
    }

    /// Apply a congestion signal (bursty arrival -- see
    /// `BitrateEvent::ArrivalStall`). Returns the new target bitrate if it
    /// changed, or `None` if this was coalesced into an already-active
    /// cooldown or the rate was already at the floor.
    pub fn on_congestion(&mut self, now: Instant) -> Option<usize> {
        if self.in_cooldown(now) {
            return None;
        }

        let cut = (self.current_bitrate as f64 * self.config.decrease_factor) as usize;
        let new_rate = cut.max(self.config.min_bitrate);

        self.ssthresh = new_rate;
        self.phase = Phase::CongestionAvoidance;
        self.last_decrease = Some(now);

        if new_rate != self.current_bitrate {
            self.current_bitrate = new_rate;
            Some(new_rate)
        } else {
            None
        }
    }

    /// Record a client's self-reported decode latency.
    pub fn on_latency_report(&mut self, latency_ms: f64) {
        self.last_latency_ms = Some(latency_ms);
    }

    /// Periodic growth check. Returns the new target bitrate if it changed.
    pub fn tick(&mut self, now: Instant) -> Option<usize> {
        if self.in_cooldown(now) {
            return None;
        }
        if self
            .last_latency_ms
            .is_some_and(|ms| ms > self.config.latency_ceiling_ms)
        {
            return None;
        }

        let proposed = match self.phase {
            Phase::SlowStart => {
                let grown = (self.current_bitrate as f64 * self.config.slow_start_factor) as usize;
                grown.min(self.ssthresh)
            }
            Phase::CongestionAvoidance => self.current_bitrate + self.config.additive_increase,
        };
        let new_rate = proposed.min(self.config.max_bitrate);

        if self.phase == Phase::SlowStart && new_rate >= self.ssthresh {
            self.phase = Phase::CongestionAvoidance;
        }

        if new_rate != self.current_bitrate {
            self.current_bitrate = new_rate;
            Some(new_rate)
        } else {
            None
        }
    }
}

/// Async driver: owns the encoder control channel and the event channel fed
/// by the signaling server, and turns `BitrateAlgorithm` decisions into
/// `EncoderControl::ChangeBitrate` messages.
pub struct AdaptiveBitrateController {
    algo: BitrateAlgorithm,
    adjustment_interval: Duration,
    encoder_control_tx: mpsc::Sender<EncoderControl>,
    event_rx: mpsc::Receiver<BitrateEvent>,
    /// Surfaces the current target bitrate to `/ws` clients (see
    /// `SignalingState::bitrate_rx` in src/server.rs).
    bitrate_tx: watch::Sender<usize>,
}

impl AdaptiveBitrateController {
    pub fn new(
        config: AdaptiveBitrateConfig,
        encoder_control_tx: mpsc::Sender<EncoderControl>,
        event_rx: mpsc::Receiver<BitrateEvent>,
        bitrate_tx: watch::Sender<usize>,
    ) -> Self {
        let adjustment_interval = config.adjustment_interval;
        Self {
            algo: BitrateAlgorithm::new(config),
            adjustment_interval,
            encoder_control_tx,
            event_rx,
            bitrate_tx,
        }
    }

    pub async fn run(mut self) {
        info!(
            "Adaptive bitrate controller started: {}-{} bps, initial {} bps",
            self.algo.config.min_bitrate, self.algo.config.max_bitrate, self.algo.current_bitrate
        );

        let mut interval = tokio::time::interval(self.adjustment_interval);

        loop {
            tokio::select! {
                event = self.event_rx.recv() => {
                    match event {
                        Some(BitrateEvent::ArrivalStall) => {
                            if let Some(new_rate) = self.algo.on_congestion(Instant::now()) {
                                self.apply(new_rate).await;
                            }
                        }
                        Some(BitrateEvent::Latency(ms)) => self.algo.on_latency_report(ms),
                        None => break,
                    }
                }
                _ = interval.tick() => {
                    match self.algo.tick(Instant::now()) {
                        Some(new_rate) => self.apply(new_rate).await,
                        None => debug!("Adaptive bitrate: holding at {} bps", self.algo.current_bitrate()),
                    }
                }
            }
        }

        info!("Adaptive bitrate controller stopped");
    }

    async fn apply(&self, new_rate: usize) {
        info!("Adaptive bitrate: -> {} bps", new_rate);
        let _ = self
            .encoder_control_tx
            .send(EncoderControl::ChangeBitrate(new_rate))
            .await;
        let _ = self.bitrate_tx.send(new_rate);
    }
}
