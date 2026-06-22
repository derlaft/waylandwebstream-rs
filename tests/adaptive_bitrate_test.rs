use tokio::sync::mpsc;
use std::time::Duration;

// Import from the main crate
use waylandwebstream::adaptive_bitrate::{
    AdaptiveBitrateConfig, AdaptiveBitrateController, NetworkMetrics,
};
use waylandwebstream::encoder::EncoderControl;

#[tokio::test]
async fn test_bitrate_decreases_on_high_latency() {
    // Create channels
    let (encoder_tx, mut encoder_rx) = mpsc::channel(16);
    let (metrics_tx, metrics_rx) = mpsc::channel(16);

    // Configure controller with low thresholds for testing
    let config = AdaptiveBitrateConfig {
        min_bitrate: 500_000,
        max_bitrate: 10_000_000,
        initial_bitrate: 2_000_000,
        target_latency_ms: 50,  // Low target for testing
        latency_threshold_factor: 1.5,
        increase_rate: 100_000,
        decrease_factor: 0.7,
        adjustment_interval: Duration::from_millis(100), // Fast for testing
    };

    let controller = AdaptiveBitrateController::new(
        config.clone(),
        encoder_tx,
        metrics_rx,
    );

    // Spawn the controller
    let controller_handle = tokio::spawn(async move {
        controller.run().await
    });

    // Give it time to start
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Send high latency metrics (150ms > threshold of 75ms)
    let high_latency = NetworkMetrics {
        encoder_queue_depth: 0,
        rtt_ms: Some(150),
        packet_loss_percent: None,
        jitter_ms: None,
        dropped_frames: 0,
    };

    metrics_tx.send(high_latency).await.unwrap();

    // Wait for adjustment interval
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Should receive a bitrate change command
    let control_msg = tokio::time::timeout(
        Duration::from_millis(500),
        encoder_rx.recv()
    ).await;

    assert!(control_msg.is_ok(), "Should receive encoder control message");
    
    if let Ok(Some(EncoderControl::ChangeBitrate(new_bitrate))) = control_msg {
        // Should decrease to ~70% of 2_000_000 = 1_400_000
        assert!(new_bitrate < 2_000_000, "Bitrate should decrease from 2Mbps, got {}", new_bitrate);
        assert!(new_bitrate >= 1_300_000 && new_bitrate <= 1_500_000, 
                "Bitrate should be around 1.4Mbps (70% of 2Mbps), got {}", new_bitrate);
        println!("✓ Bitrate decreased to {} bps on high latency", new_bitrate);
    } else {
        panic!("Expected ChangeBitrate message");
    }

    controller_handle.abort();
}

#[tokio::test]
async fn test_bitrate_increases_on_low_latency() {
    // Create channels
    let (encoder_tx, mut encoder_rx) = mpsc::channel(16);
    let (metrics_tx, metrics_rx) = mpsc::channel(16);

    // Configure controller
    let config = AdaptiveBitrateConfig {
        min_bitrate: 500_000,
        max_bitrate: 10_000_000,
        initial_bitrate: 2_000_000,
        target_latency_ms: 100,
        latency_threshold_factor: 1.5,
        increase_rate: 200_000,  // Faster increase for testing
        decrease_factor: 0.7,
        adjustment_interval: Duration::from_millis(100),
    };

    let controller = AdaptiveBitrateController::new(
        config.clone(),
        encoder_tx,
        metrics_rx,
    );

    // Spawn the controller
    let controller_handle = tokio::spawn(async move {
        controller.run().await
    });

    // Give it time to start
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Send low latency metrics repeatedly (30ms < 100ms target)
    let low_latency = NetworkMetrics {
        encoder_queue_depth: 0,
        rtt_ms: Some(30),
        packet_loss_percent: None,
        jitter_ms: None,
        dropped_frames: 0,
    };

    // Send metrics multiple times to trigger increase
    for _ in 0..5 {
        metrics_tx.send(low_latency.clone()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
    }

    // Wait for adjustment
    tokio::time::sleep(Duration::from_millis(1200)).await;

    // Should receive at least one bitrate increase
    let mut received_increase = false;
    let mut latest_bitrate = 2_000_000;

    while let Ok(msg) = encoder_rx.try_recv() {
        if let EncoderControl::ChangeBitrate(new_bitrate) = msg {
            if new_bitrate > latest_bitrate {
                received_increase = true;
            }
            latest_bitrate = new_bitrate;
        }
    }

    assert!(received_increase, "Bitrate should increase on low latency");
    assert!(latest_bitrate > 2_000_000, "Final bitrate {} should be higher than initial 2Mbps", latest_bitrate);
    println!("✓ Bitrate increased to {} bps on low latency", latest_bitrate);

    controller_handle.abort();
}

#[tokio::test]
async fn test_bitrate_respects_min_max_bounds() {
    // Create channels
    let (encoder_tx, mut encoder_rx) = mpsc::channel(16);
    let (metrics_tx, metrics_rx) = mpsc::channel(16);

    // Configure controller with tight bounds
    let config = AdaptiveBitrateConfig {
        min_bitrate: 1_000_000,
        max_bitrate: 3_000_000,
        initial_bitrate: 2_000_000,
        target_latency_ms: 50,
        latency_threshold_factor: 1.5,
        increase_rate: 500_000,  // Large increase
        decrease_factor: 0.5,    // Large decrease
        adjustment_interval: Duration::from_millis(100),
    };

    let controller = AdaptiveBitrateController::new(
        config.clone(),
        encoder_tx,
        metrics_rx,
    );

    // Spawn the controller
    let controller_handle = tokio::spawn(async move {
        controller.run().await
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    // Send very high latency to try to push below min
    let very_high_latency = NetworkMetrics {
        encoder_queue_depth: 4,
        rtt_ms: Some(500),
        packet_loss_percent: Some(10.0),
        jitter_ms: Some(100),
        dropped_frames: 10,
    };

    // Send multiple times to trigger multiple decreases
    for _ in 0..5 {
        metrics_tx.send(very_high_latency.clone()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
    }

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Collect all bitrate changes
    let mut min_seen = u64::MAX;
    while let Ok(msg) = encoder_rx.try_recv() {
        if let EncoderControl::ChangeBitrate(bitrate) = msg {
            min_seen = min_seen.min(bitrate as u64);
        }
    }

    assert!(min_seen >= config.min_bitrate as u64, 
            "Bitrate {} should not go below min {}", min_seen, config.min_bitrate);
    println!("✓ Bitrate respected minimum bound: {} >= {}", min_seen, config.min_bitrate);

    controller_handle.abort();
}

#[tokio::test]
async fn test_congestion_detection() {
    // Create channels
    let (encoder_tx, mut encoder_rx) = mpsc::channel(16);
    let (metrics_tx, metrics_rx) = mpsc::channel(16);

    let config = AdaptiveBitrateConfig {
        min_bitrate: 500_000,
        max_bitrate: 10_000_000,
        initial_bitrate: 2_000_000,
        target_latency_ms: 100,
        latency_threshold_factor: 1.5,
        increase_rate: 100_000,
        decrease_factor: 0.7,
        adjustment_interval: Duration::from_millis(100),
    };

    let controller = AdaptiveBitrateController::new(
        config.clone(),
        encoder_tx,
        metrics_rx,
    );

    let controller_handle = tokio::spawn(async move {
        controller.run().await
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    // Send metrics with low latency but high packet loss (congestion)
    let congestion_metrics = NetworkMetrics {
        encoder_queue_depth: 0,
        rtt_ms: Some(50),  // Low latency
        packet_loss_percent: Some(5.0),  // High packet loss
        jitter_ms: None,
        dropped_frames: 0,
    };

    metrics_tx.send(congestion_metrics).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Should still decrease bitrate due to congestion
    let control_msg = tokio::time::timeout(
        Duration::from_millis(500),
        encoder_rx.recv()
    ).await;

    assert!(control_msg.is_ok(), "Should receive encoder control message");
    
    if let Ok(Some(EncoderControl::ChangeBitrate(new_bitrate))) = control_msg {
        assert!(new_bitrate < 2_000_000, 
                "Bitrate should decrease due to congestion despite low latency, got {}", new_bitrate);
        println!("✓ Bitrate decreased to {} bps on congestion", new_bitrate);
    } else {
        panic!("Expected ChangeBitrate message");
    }

    controller_handle.abort();
}
