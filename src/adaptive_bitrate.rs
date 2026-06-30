//! Adaptive CBR control.
//!
//! Modeled on TCP Reno: slow start probes upward fast until the first
//! congestion signal, then switches to additive-increase / multiplicative-
//! decrease (AIMD) congestion avoidance, remembering the post-cut rate as a
//! `ssthresh` ceiling so future growth stops there before probing further.
//!
//! The primary congestion signal is server-side send backpressure
//! (`BitrateEvent::SendBacklog`): when a client's outbound socket drains
//! slower than the encoder produces, frames pile past the small per-client
//! broadcast buffer and get dropped (the `Lagged` arm in `src/server.rs`).
//! That is measured right at the bottleneck -- the send socket -- and is
//! loss-equivalent: direct evidence the current rate doesn't fit this
//! client's link, the same way a dropped TCP segment is evidence a window
//! was too large. A client's bursty-arrival report
//! (`BitrateEvent::ArrivalStall`, derived from `burst_count` in
//! `SignalingMessage::Latency`) is a secondary, client-side corroborator of
//! the same condition for paths where the server's own writes don't visibly
//! stall. It is treated *strictly* as a corroborator: a burst cuts only when a
//! `SendBacklog` also fired recently (see `ARRIVAL_STALL_CORROBORATION_WINDOW`),
//! never on its own -- in the browser a burst is dominated by the client's own
//! frame-delivery clustering (notably Chromium's decode-worker delivery), which
//! is not congestion and otherwise pinned the shared rate at the floor.
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
//! Note: one encoder feeds every connected `/client` client (see
//! `SignalingState::video_tx` in `src/server.rs`), so a cut triggered by one
//! struggling client lowers the rate for all of them. That's an existing
//! property of the single shared-encoder broadcast design, not something
//! this controller can address per-client.

use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};

use crate::encoder::EncoderControl;

/// Fraction of the last-applied rate the AIMD target must climb past before a
/// growth step is actually pushed to the encoder. Each `ChangeBitrate`
/// rebuilds the encoder and emits an IDR (libavcodec exposes no in-place
/// x264/VAAPI bitrate reconfig), so without this the per-tick
/// `additive_increase` growth would rebuild -- and spike a keyframe -- roughly
/// every second, on top of the periodic GOP keyframes, fighting the very
/// jitter buffer the VBV cap protects. Coalescing growth into ~15% jumps makes
/// bitrate-driven IDRs rarer than the natural keyframe-interval IDR. Decreases
/// are never coalesced (see `should_actuate`).
const APPLY_THRESHOLD_FRACTION: f64 = 0.15;

/// Whether a freshly computed `target` rate is worth pushing to the encoder
/// given the rate it last received (`last_applied`). A growth step must clear
/// `APPLY_THRESHOLD_FRACTION` of the applied rate to actuate; a decrease (a
/// congestion cut) or reaching the `max_bitrate` ceiling always actuates
/// immediately -- relieving a bottleneck is urgent, and the ceiling flush
/// ensures the encoder actually reaches the cap instead of stalling one
/// coalescing band below it.
fn should_actuate(target: usize, last_applied: usize, max_bitrate: usize) -> bool {
    if target <= last_applied || target >= max_bitrate {
        return true;
    }
    let threshold = ((last_applied as f64) * APPLY_THRESHOLD_FRACTION) as usize;
    target - last_applied >= threshold.max(1)
}

/// How recently a server-side `SendBacklog` must have fired for a client's
/// `ArrivalStall` (bursty arrival) to be honored as a congestion cut rather
/// than ignored. `ArrivalStall` is a *secondary corroborator*, not independent
/// evidence: in the browser a "burst" is dominated by the client's own
/// frame-delivery clustering, not the link. Measured 2026-06-30: Chromium's
/// decode-worker `postMessage` delivery clusters frames (~20% sub-3ms arrival
/// gaps vs Firefox ~1%), so the client's burst-count heuristic tripped
/// constantly with no real congestion -- 41 `ArrivalStall` events against 0
/// `SendBacklog` in one session -- and, since each cut also rebuilds the
/// encoder + emits an IDR, pinned the shared encoder at the floor on Chromium
/// while Firefox climbed to the ceiling. Honoring a burst only when the
/// authoritative server-side signal also fired recently keeps the corroborator
/// role the module doc describes without letting it cut on its own.
const ARRIVAL_STALL_CORROBORATION_WINDOW: Duration = Duration::from_secs(5);

/// Signal fed into the controller from the server's signaling handlers.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BitrateEvent {
    /// The server's per-client send path fell behind: frames piled past the
    /// small outbound broadcast buffer and were dropped because the socket
    /// drained slower than the encoder produced (the `Lagged` arm in
    /// src/server.rs). The most direct congestion signal there is -- measured
    /// at the actual bottleneck -- and one a client's *local* decode/render
    /// stall can't fake, since that doesn't slow the server's TCP writes (the
    /// browser keeps draining the socket into its message queue regardless of
    /// how fast it decodes).
    SendBacklog,
    /// The client reported several frames landing within milliseconds of
    /// each other (see `SignalingMessage::Latency::burst_count` in
    /// src/server.rs): a batch that queued up somewhere in the network path
    /// between server and client and released all at once. A secondary,
    /// client-side corroborator of the same "rate doesn't fit the path"
    /// condition that `SendBacklog` reports from the server side.
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
    /// When a server-side `SendBacklog` was last seen. A client's
    /// `ArrivalStall` only counts as congestion if it lands within
    /// `ARRIVAL_STALL_CORROBORATION_WINDOW` of one -- see `on_arrival_stall`.
    last_backlog: Option<Instant>,
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
            last_backlog: None,
        }
    }

    pub fn current_bitrate(&self) -> usize {
        self.current_bitrate
    }

    fn in_cooldown(&self, now: Instant) -> bool {
        self.last_decrease
            .is_some_and(|last| now.duration_since(last) < self.config.decrease_cooldown)
    }

    /// Server-side send backpressure (`BitrateEvent::SendBacklog`): the
    /// authoritative, loss-equivalent congestion signal. Cuts immediately and
    /// records the time so a following `ArrivalStall` can corroborate it.
    /// Returns the new target bitrate, or `None` if coalesced into an active
    /// cooldown or already at the floor.
    pub fn on_send_backlog(&mut self, now: Instant) -> Option<usize> {
        self.last_backlog = Some(now);
        self.cut(now)
    }

    /// Client-reported bursty arrival (`BitrateEvent::ArrivalStall`). Only a
    /// *secondary corroborator* of `SendBacklog`, never independent evidence
    /// (see `ARRIVAL_STALL_CORROBORATION_WINDOW`): a burst with no recent
    /// server-side backlog is the client's own delivery clustering, not the
    /// link, so it's ignored. When the authoritative signal did fire recently,
    /// a burst cuts identically -- reinforcing a sustained stall past the
    /// cooldown.
    pub fn on_arrival_stall(&mut self, now: Instant) -> Option<usize> {
        let corroborated = self
            .last_backlog
            .is_some_and(|t| now.duration_since(t) < ARRIVAL_STALL_CORROBORATION_WINDOW);
        if corroborated {
            self.cut(now)
        } else {
            None
        }
    }

    /// Multiplicative congestion cut shared by both signals. Returns the new
    /// target bitrate if it changed, or `None` if coalesced into an active
    /// cooldown or the rate was already at the floor.
    fn cut(&mut self, now: Instant) -> Option<usize> {
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
    /// Surfaces the current target bitrate to `/client` clients (see
    /// `SignalingState::bitrate_rx` in src/server.rs).
    bitrate_tx: watch::Sender<usize>,
    /// The rate last actually pushed to the encoder. The algorithm's target
    /// climbs finely every tick, but only crosses into a real `ChangeBitrate`
    /// (and its rebuild+IDR) once it has pulled this far enough ahead -- see
    /// `should_actuate`. Seeded with the initial bitrate the encoder started at.
    last_applied_bitrate: usize,
}

impl AdaptiveBitrateController {
    pub fn new(
        config: AdaptiveBitrateConfig,
        encoder_control_tx: mpsc::Sender<EncoderControl>,
        event_rx: mpsc::Receiver<BitrateEvent>,
        bitrate_tx: watch::Sender<usize>,
    ) -> Self {
        let adjustment_interval = config.adjustment_interval;
        let initial_bitrate = config.initial_bitrate;
        Self {
            algo: BitrateAlgorithm::new(config),
            adjustment_interval,
            encoder_control_tx,
            event_rx,
            bitrate_tx,
            last_applied_bitrate: initial_bitrate,
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
                        Some(BitrateEvent::SendBacklog) => {
                            if let Some(new_rate) = self.algo.on_send_backlog(Instant::now()) {
                                self.maybe_apply(new_rate).await;
                            }
                        }
                        Some(BitrateEvent::ArrivalStall) => {
                            if let Some(new_rate) = self.algo.on_arrival_stall(Instant::now()) {
                                self.maybe_apply(new_rate).await;
                            }
                        }
                        Some(BitrateEvent::Latency(ms)) => self.algo.on_latency_report(ms),
                        None => break,
                    }
                }
                _ = interval.tick() => {
                    match self.algo.tick(Instant::now()) {
                        Some(new_rate) => self.maybe_apply(new_rate).await,
                        None => debug!("Adaptive bitrate: holding at {} bps", self.algo.current_bitrate()),
                    }
                }
            }
        }

        info!("Adaptive bitrate controller stopped");
    }

    /// Push `new_rate` to the encoder only if it's worth a rebuild+IDR (see
    /// `should_actuate`); otherwise defer -- the algorithm keeps tracking the
    /// finer target and a later tick will cross the band. Decreases and the
    /// ceiling always pass through.
    async fn maybe_apply(&mut self, new_rate: usize) {
        if !should_actuate(new_rate, self.last_applied_bitrate, self.algo.config.max_bitrate) {
            debug!(
                "Adaptive bitrate: target {} bps within coalescing band of applied {} bps; deferring rebuild",
                new_rate, self.last_applied_bitrate
            );
            return;
        }
        self.apply(new_rate).await;
    }

    async fn apply(&mut self, new_rate: usize) {
        debug!("Adaptive bitrate: -> {} bps", new_rate);
        // Control-plane send: a closed channel means the encoder thread is gone,
        // so surface it rather than silently dropping the rate change.
        if self
            .encoder_control_tx
            .send(EncoderControl::ChangeBitrate(new_rate))
            .await
            .is_err()
        {
            warn!("Adaptive bitrate: encoder control channel closed; rate change dropped");
        }
        // bitrate_tx is a telemetry watch; a missing receiver is benign.
        let _ = self.bitrate_tx.send(new_rate);
        self.last_applied_bitrate = new_rate;
    }
}

#[cfg(test)]
mod tests {
    use super::should_actuate;

    const MAX: usize = 12_000_000;

    #[test]
    fn decrease_always_actuates() {
        // A congestion cut must take effect immediately, never coalesced.
        assert!(should_actuate(1_500_000, 2_000_000, MAX));
        // Equal is harmless: the encoder's own change_bitrate no-ops it.
        assert!(should_actuate(2_000_000, 2_000_000, MAX));
    }

    #[test]
    fn small_growth_is_coalesced() {
        // +150k on a 2M applied rate is 7.5% < 15% threshold -> defer.
        assert!(!should_actuate(2_150_000, 2_000_000, MAX));
    }

    #[test]
    fn growth_past_threshold_actuates() {
        // +300k on 2M is exactly 15% -> actuate.
        assert!(should_actuate(2_300_000, 2_000_000, MAX));
        // Just below the band stays deferred.
        assert!(!should_actuate(2_299_000, 2_000_000, MAX));
    }

    #[test]
    fn ceiling_flushes_even_within_band() {
        // Target at the cap but only a hair above applied: still actuate so
        // the encoder actually reaches max instead of stalling below it.
        assert!(should_actuate(MAX, 11_900_000, MAX));
    }

    use super::{AdaptiveBitrateConfig, BitrateAlgorithm, ARRIVAL_STALL_CORROBORATION_WINDOW};
    use std::time::{Duration, Instant};

    fn algo() -> BitrateAlgorithm {
        BitrateAlgorithm::new(AdaptiveBitrateConfig::default())
    }

    #[test]
    fn arrival_stall_without_backlog_never_cuts() {
        // The Chromium false-positive: bursts with no server-side backlog must
        // not touch the rate, no matter how many arrive.
        let mut a = algo();
        let t = Instant::now();
        let start = a.current_bitrate();
        assert_eq!(a.on_arrival_stall(t), None);
        assert_eq!(a.on_arrival_stall(t + Duration::from_secs(1)), None);
        assert_eq!(a.current_bitrate(), start);
    }

    #[test]
    fn send_backlog_cuts_then_a_burst_corroborates_past_the_cooldown() {
        let cfg = AdaptiveBitrateConfig::default();
        let mut a = algo();
        let t = Instant::now();
        let start = a.current_bitrate();

        // Authoritative signal cuts immediately.
        let cut1 = a.on_send_backlog(t).expect("backlog should cut");
        assert!(cut1 < start);

        // A burst inside the cooldown is coalesced (no second cut)...
        assert_eq!(a.on_arrival_stall(t + Duration::from_millis(50)), None);

        // ...but once the cooldown clears, a burst still within the
        // corroboration window reinforces the sustained stall and cuts again.
        let after_cooldown = t + cfg.decrease_cooldown + Duration::from_millis(1);
        assert!(after_cooldown.duration_since(t) < ARRIVAL_STALL_CORROBORATION_WINDOW);
        let cut2 = a
            .on_arrival_stall(after_cooldown)
            .expect("corroborated burst should cut");
        assert!(cut2 < cut1);
    }

    #[test]
    fn burst_after_the_corroboration_window_is_ignored() {
        let mut a = algo();
        let t = Instant::now();
        a.on_send_backlog(t);
        let rate_after_backlog = a.current_bitrate();

        // Well past the window (and the cooldown): with no fresh backlog the
        // burst is treated as a client-side artifact, not congestion.
        let late = t + ARRIVAL_STALL_CORROBORATION_WINDOW + Duration::from_secs(1);
        assert_eq!(a.on_arrival_stall(late), None);
        assert_eq!(a.current_bitrate(), rate_after_backlog);
    }
}
