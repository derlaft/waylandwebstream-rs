use std::time::{Duration, Instant};

use waylandwebstream::adaptive_bitrate::{AdaptiveBitrateConfig, BitrateAlgorithm};

// All tests drive `BitrateAlgorithm` with synthetic `Instant`s computed from
// a fixed base time rather than real sleeps, so they're deterministic and
// fast -- the old RTCP-based controller's tests relied on real
// `tokio::time::sleep` calls and were correspondingly slow and flaky.

fn config() -> AdaptiveBitrateConfig {
    AdaptiveBitrateConfig {
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

#[test]
fn slow_start_grows_multiplicatively_until_the_ceiling() {
    let mut algo = BitrateAlgorithm::new(config());
    let t0 = Instant::now();

    let before = algo.current_bitrate();
    let after = algo.tick(t0 + Duration::from_secs(1)).expect("should grow");
    assert_eq!(after, (before as f64 * 1.4) as usize);

    // Keep ticking until growth stops -- should clamp at max_bitrate, not
    // overshoot it.
    let mut now = t0 + Duration::from_secs(1);
    for _ in 0..50 {
        now += Duration::from_secs(1);
        if algo.tick(now).is_none() {
            break;
        }
    }
    assert_eq!(algo.current_bitrate(), 12_000_000);
}

#[test]
fn congestion_cuts_bitrate_multiplicatively() {
    let mut algo = BitrateAlgorithm::new(config());
    let t0 = Instant::now();

    let new_rate = algo.on_congestion(t0).expect("should cut");
    assert_eq!(new_rate, (2_000_000.0 * 0.75) as usize);
    assert_eq!(algo.current_bitrate(), new_rate);
}

#[test]
fn repeated_congestion_within_cooldown_are_coalesced() {
    let mut algo = BitrateAlgorithm::new(config());
    let t0 = Instant::now();

    let first_cut = algo.on_congestion(t0).expect("should cut");

    // A second signal 500ms later (well inside the 2s cooldown) is the
    // same underlying stall, not a fresh one -- shouldn't cut again.
    let second = algo.on_congestion(t0 + Duration::from_millis(500));
    assert_eq!(second, None);
    assert_eq!(algo.current_bitrate(), first_cut);
}

#[test]
fn congestion_after_cooldown_cuts_again() {
    let mut algo = BitrateAlgorithm::new(config());
    let t0 = Instant::now();

    let first_cut = algo.on_congestion(t0).expect("should cut");
    let second_cut = algo
        .on_congestion(t0 + Duration::from_secs(3))
        .expect("cooldown elapsed, should cut again");

    assert_eq!(second_cut, (first_cut as f64 * 0.75) as usize);
}

#[test]
fn growth_holds_during_post_cut_cooldown() {
    let mut algo = BitrateAlgorithm::new(config());
    let t0 = Instant::now();

    algo.on_congestion(t0).expect("should cut");
    let cut_rate = algo.current_bitrate();

    // A tick 1s later is still inside the 2s cooldown -- bitrate should
    // hold, not grow, while the cut settles.
    assert_eq!(algo.tick(t0 + Duration::from_secs(1)), None);
    assert_eq!(algo.current_bitrate(), cut_rate);

    // Once the cooldown has elapsed, growth resumes.
    assert!(algo.tick(t0 + Duration::from_secs(3)).is_some());
}

#[test]
fn growth_holds_while_latency_is_elevated() {
    let mut algo = BitrateAlgorithm::new(config());
    let t0 = Instant::now();

    algo.on_latency_report(300.0); // above the 150ms ceiling
    assert_eq!(algo.tick(t0 + Duration::from_secs(1)), None);

    algo.on_latency_report(50.0); // back under the ceiling
    assert!(algo.tick(t0 + Duration::from_secs(2)).is_some());
}

#[test]
fn congestion_floors_at_min_bitrate() {
    let mut algo = BitrateAlgorithm::new(AdaptiveBitrateConfig {
        initial_bitrate: 600_000,
        min_bitrate: 500_000,
        decrease_cooldown: Duration::from_millis(0),
        ..config()
    });
    let mut now = Instant::now();

    // Repeated cuts should floor at min_bitrate, never go below it.
    for _ in 0..10 {
        algo.on_congestion(now);
        now += Duration::from_secs(10);
    }
    assert_eq!(algo.current_bitrate(), 500_000);
}

#[test]
fn switches_to_additive_increase_after_a_cut() {
    let mut algo = BitrateAlgorithm::new(config());
    let t0 = Instant::now();

    // Grow a bit in slow start first so the cut has somewhere to fall from.
    algo.tick(t0 + Duration::from_secs(1));
    algo.tick(t0 + Duration::from_secs(2));

    algo.on_congestion(t0 + Duration::from_secs(2));
    let post_cut = algo.current_bitrate();

    // Past the cooldown, growth should now be the fixed additive step
    // (congestion avoidance), not another multiplicative jump (slow start).
    let now = t0 + Duration::from_secs(5);
    let grown = algo.tick(now).expect("should grow");
    assert_eq!(grown, post_cut + 150_000);
}
