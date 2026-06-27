// Latency tracking for Phase 8.
//
// Tracks RTT (via ping/pong echo in VIDEO_FRAME), burst arrivals, and
// assembles 5-second latency reports to send to the server. Decode timing
// is not tracked here because decoding runs on a separate OS thread and
// there is no low-overhead way to retrieve per-frame timings from it
// without adding another channel; the report sends `decoding_ms: None`.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::types::SignalingMessage;

pub struct LatencyTracker {
    last_network_ms: Option<f64>,
    pending_ping_ts: Option<f64>,
    last_ping_sent: Instant,
    burst_count: u32,
    last_frame_arrival: Option<Instant>,
}

impl LatencyTracker {
    pub fn new() -> Self {
        Self {
            last_network_ms: None,
            pending_ping_ts: None,
            // Trigger the first ping promptly (at the 5s tick).
            last_ping_sent: Instant::now()
                .checked_sub(std::time::Duration::from_secs(10))
                .unwrap_or_else(Instant::now),
            burst_count: 0,
            last_frame_arrival: None,
        }
    }

    /// Call on every VideoFrame arrival. Updates burst count and RTT if
    /// `ping_echo` matches our outstanding ping timestamp.
    pub fn record_arrival(&mut self, ping_echo: f64) {
        let now = Instant::now();
        if let Some(last) = self.last_frame_arrival {
            if now.duration_since(last).as_millis() < 3 {
                self.burst_count = self.burst_count.saturating_add(1);
            }
        }
        self.last_frame_arrival = Some(now);

        if ping_echo != 0.0 {
            if self.pending_ping_ts == Some(ping_echo) {
                let rtt_ms = now_ms() - ping_echo;
                if rtt_ms > 0.0 {
                    self.last_network_ms = Some(rtt_ms / 2.0);
                }
                self.pending_ping_ts = None;
            }
        }
    }

    /// Returns a `Ping` message if 5 s have elapsed since the last one.
    /// Call this on the periodic tick and send the result if Some.
    pub fn maybe_ping(&mut self) -> Option<SignalingMessage> {
        if self.last_ping_sent.elapsed().as_secs() < 5 {
            return None;
        }
        let ts = now_ms();
        self.pending_ping_ts = Some(ts);
        self.last_ping_sent = Instant::now();
        Some(SignalingMessage::Ping { client_ts: ts })
    }

    /// Assemble and return the latency report for the current window,
    /// then reset per-window counters.
    pub fn flush_report(&mut self) -> SignalingMessage {
        let burst = self.burst_count;
        self.burst_count = 0;
        SignalingMessage::Latency {
            encoding_ms: None,
            network_ms: self.last_network_ms,
            jitter_buffer_ms: None,
            decoding_ms: None,
            total_ms: self.last_network_ms.unwrap_or(0.0),
            burst_count: burst,
            blit_ms: None,
        }
    }
}

fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
        * 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SignalingMessage;

    fn latency_fields(msg: SignalingMessage) -> (Option<f64>, u32) {
        match msg {
            SignalingMessage::Latency { network_ms, burst_count, .. } => {
                (network_ms, burst_count)
            }
            other => panic!("expected Latency, got {other:?}"),
        }
    }

    fn ping_ts(msg: SignalingMessage) -> f64 {
        match msg {
            SignalingMessage::Ping { client_ts } => client_ts,
            other => panic!("expected Ping, got {other:?}"),
        }
    }

    // `new()` back-dates `last_ping_sent` by 10 s so the very first tick
    // fires a ping without sleeping in tests.
    #[test]
    fn first_maybe_ping_fires_immediately() {
        let mut t = LatencyTracker::new();
        assert!(t.maybe_ping().is_some(), "expected immediate first ping");
    }

    #[test]
    fn second_maybe_ping_does_not_fire() {
        let mut t = LatencyTracker::new();
        t.maybe_ping(); // consume the initial one
        assert!(t.maybe_ping().is_none(), "should not ping twice without 5 s gap");
    }

    // Simulate the server echoing the ping timestamp back inside a video
    // frame: record_arrival with the matching echo updates network_ms.
    #[test]
    fn record_arrival_computes_rtt_from_matching_echo() {
        let mut t = LatencyTracker::new();
        let ts = ping_ts(t.maybe_ping().unwrap());

        // Deliver the echo (server stamped the frame with our ts).
        t.record_arrival(ts);

        let (network_ms, _) = latency_fields(t.flush_report());
        let ms = network_ms.expect("network_ms should be Some after echo");
        assert!(ms >= 0.0, "RTT must be non-negative: {ms}");
        assert!(ms < 5_000.0, "RTT suspiciously large: {ms}");
    }

    // Echo that doesn't match the outstanding ping is silently ignored.
    #[test]
    fn record_arrival_ignores_nonmatching_echo() {
        let mut t = LatencyTracker::new();
        t.maybe_ping(); // sets pending_ping_ts = Some(X)

        // Feed a different timestamp.
        t.record_arrival(1.0);

        let (network_ms, _) = latency_fields(t.flush_report());
        assert!(network_ms.is_none(), "should not compute RTT from stale echo");
    }

    // echo=0.0 is the sentinel for "no echo in this frame" and must be ignored.
    #[test]
    fn record_arrival_zero_echo_is_ignored() {
        let mut t = LatencyTracker::new();
        t.maybe_ping();
        t.record_arrival(0.0);
        let (network_ms, _) = latency_fields(t.flush_report());
        assert!(network_ms.is_none());
    }

    // Two consecutive arrivals should both be within 3 ms of each other
    // (normal code execution is nanoseconds) → burst_count increments.
    #[test]
    fn burst_counting_on_rapid_arrivals() {
        let mut t = LatencyTracker::new();
        t.record_arrival(0.0); // first: sets last_frame_arrival
        t.record_arrival(0.0); // second: gap < 3 ms → burst_count = 1
        t.record_arrival(0.0); // third → burst_count = 2
        let (_, burst) = latency_fields(t.flush_report());
        assert!(burst >= 2, "expected burst_count >= 2, got {burst}");
    }

    #[test]
    fn flush_report_resets_burst_count() {
        let mut t = LatencyTracker::new();
        t.record_arrival(0.0);
        t.record_arrival(0.0); // burst_count = 1
        let (_, burst1) = latency_fields(t.flush_report()); // consumes count
        assert_eq!(burst1, 1);

        let (_, burst2) = latency_fields(t.flush_report()); // count was reset
        assert_eq!(burst2, 0);
    }

    #[test]
    fn flush_report_with_no_activity_is_zero() {
        let mut t = LatencyTracker::new();
        let (network_ms, burst) = latency_fields(t.flush_report());
        assert!(network_ms.is_none());
        assert_eq!(burst, 0);
    }

    // network_ms persists across flush cycles (we keep the last measured RTT).
    #[test]
    fn network_ms_persists_across_flushes() {
        let mut t = LatencyTracker::new();
        let ts = ping_ts(t.maybe_ping().unwrap());
        t.record_arrival(ts);

        let (ms1, _) = latency_fields(t.flush_report());
        let (ms2, _) = latency_fields(t.flush_report()); // second flush, no new ping
        assert_eq!(ms1, ms2, "network_ms should persist until a new RTT is measured");
    }
}
