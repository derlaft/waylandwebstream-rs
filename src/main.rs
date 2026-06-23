use anyhow::{Context, Result};
use clap::Parser;
use smithay::reexports::{
    calloop::EventLoop,
    wayland_server::Display,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{info, warn, debug, Level};
use tracing_subscriber::FmtSubscriber;

mod adaptive_bitrate;
mod compositor;
mod config;
mod encoder;
mod input;
mod latency;
mod server;
mod web;

use adaptive_bitrate::{AdaptiveBitrateConfig, AdaptiveBitrateController, BitrateEvent};
use compositor::CompositorState;
use encoder::{EncoderConfig, RateControl, spawn_encoder};
use input::mouse::MouseHandler;
use input::touch::TouchHandler;
use server::{SignalingServer, SignalingState};

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    // Parse CLI arguments
    let config = config::Config::parse();
    info!("Starting WaylandWebStream");
    info!("Configuration: {:?}", config);

    // Parse initial resolution
    let (width, height) = config.get_initial_resolution()?;
    info!("Initial resolution: {}x{}", width, height);

    // Create Wayland display and event loop
    let mut event_loop: EventLoop<CompositorState> = EventLoop::try_new()
        .context("Failed to create event loop")?;

    let mut display: Display<CompositorState> = Display::new()
        .context("Failed to create Wayland display")?;

    // Initialize compositor state
    let mut state = CompositorState::new(
        &mut event_loop,
        &mut display,
        width,
        height,
    );

    // Set up Wayland socket using Smithay's helper
    let socket_source = smithay::wayland::socket::ListeningSocketSource::with_name(&config.display_name)
        .context("Failed to create Wayland listening socket")?;

    let mut display_handle = display.handle();
    event_loop
        .handle()
        .insert_source(
            socket_source,
            move |client_stream, _, _state| {
                // Accept new clients with proper client state
                let client_state = compositor::state::ClientState {
                    compositor_state: smithay::wayland::compositor::CompositorClientState::default(),
                };
                if let Err(e) = display_handle.insert_client(client_stream, Arc::new(client_state)) {
                    info!("Failed to insert client: {}", e);
                } else {
                    info!("New client connected");
                }
            }
        )
        .context("Failed to insert listening socket into event loop")?;

    // Initialize encoder
    let keyframe_interval = config.keyframe_interval.unwrap_or(config.framerate * 2);
    let rate_control = match config.crf {
        Some(crf) => RateControl::Quality(crf),
        None => RateControl::Bitrate(config.bitrate),
    };
    // Adaptive bitrate only makes sense in bitrate mode -- CRF mode has no
    // bitrate target to adapt.
    let adaptive_bitrate_enabled = !config.no_adaptive_bitrate && config.crf.is_none();
    if config.no_adaptive_bitrate && config.crf.is_some() {
        warn!("--no-adaptive-bitrate has no effect with --crf (constant quality mode never adapts bitrate)");
    }
    if config.min_bitrate >= config.max_bitrate {
        anyhow::bail!(
            "--min-bitrate ({}) must be less than --max-bitrate ({})",
            config.min_bitrate,
            config.max_bitrate
        );
    }
    let encoder_config = EncoderConfig {
        width,
        height,
        framerate: config.framerate,
        rate_control,
        keyframe_interval,
    };
    
    let (encoder, buffer_return_rx) = spawn_encoder(encoder_config)?;

    // Create channels for the server
    let (resize_tx, mut resize_rx) = mpsc::channel::<(u32, u32)>(4);
    let (touch_tx, mut touch_rx) = mpsc::channel(32); // Higher capacity for touch events
    let (mouse_tx, mut mouse_rx) = mpsc::channel(64); // Higher capacity for pointer moves
    // Forwards a client's `ping` (control channel) to the packet-forwarding
    // loop below, which stamps it onto the next outgoing video frame.
    let (pending_ping_tx, mut pending_ping_rx) = mpsc::channel::<f64>(8);
    // Current encoder target bitrate, surfaced to clients over `/ws`. CRF
    // (constant-quality) mode has no bitrate target, hence the 0 sentinel.
    let initial_bitrate_for_display = match rate_control {
        RateControl::Bitrate(bps) => bps,
        RateControl::Quality(_) => 0,
    };
    let (bitrate_tx, bitrate_rx) = tokio::sync::watch::channel(initial_bitrate_for_display);

    // Create touch and pointer handlers
    let mut touch_handler = TouchHandler::new(width, height);
    let mut mouse_handler = MouseHandler::new(width, height);

    info!("\n╔══════════════════════════════════════════════════════════════╗");
    info!("║  WaylandWebStream - Latency Reporting Enabled               ║");
    info!("╠══════════════════════════════════════════════════════════════╣");
    info!("║  ✓ H.264 video over a binary WebSocket (/stream)            ║");
    info!("║  ✓ Browser-side WebCodecs decode into a <canvas>             ║");
    info!("║  ✓ HTTP/WebSocket control channel                           ║");
    info!("║  ✓ Touch input handling (multi-touch support)               ║");
    info!("║  ✓ Client-to-server latency reporting                       ║");
    info!("╠══════════════════════════════════════════════════════════════╣");
    info!("║  Server Configuration:                                       ║");
    info!("║  - Resolution: {}x{} @ {}fps                       ║", width, height, config.framerate);
    match rate_control {
        RateControl::Bitrate(bps) if adaptive_bitrate_enabled => info!(
            "║  - Bitrate: adaptive, {} bps initial ({}-{} bps)        ║",
            bps, config.min_bitrate, config.max_bitrate
        ),
        RateControl::Bitrate(bps) => info!("║  - Bitrate: {} bps (fixed)                                  ║", bps),
        RateControl::Quality(crf) => info!("║  - Quality: CRF {}                                            ║", crf),
    }
    info!("║  - Keyframe interval: {} frames                              ║", keyframe_interval);
    info!("║  - HTTP listen address: {}:{}                          ║", config.listen_addr, config.port);
    info!("║  - Wayland display: {}                         ║", config.display_name);
    info!("╠══════════════════════════════════════════════════════════════╣");
    info!("║  Connect with browser:                                       ║");
    info!("║  http://localhost:{}                                      ║", config.port);
    info!("╚══════════════════════════════════════════════════════════════╝\n");

    info!("Server starting on port {}...", config.port);
    
    // Get frame sender, encoder control sender, and resize sender before moving encoder
    let frame_sender = encoder.get_frame_sender();
    let encoder_control = encoder.get_control_sender();
    let encoder_resize = encoder.get_resize_sender();

    // Set up latency reporting pipeline
    let latency_tx = {
        use crate::latency::LatencyReport;
        let (latency_tx, mut latency_rx) = mpsc::channel::<LatencyReport>(16);
        
        info!("Latency reporting pipeline initialized");
        
        // Spawn task to log detailed latency reports
        tokio::spawn(async move {
            info!("Latency reporting task started, waiting for reports...");
            while let Some(report) = latency_rx.recv().await {
                info!("═══ Latency Report ═══");
                if let Some(v) = report.input_ms {
                    info!("  Input:          {:>6.1} ms", v);
                }
                if let Some(v) = report.capture_to_encode_ms {
                    info!("  Capture→Encode: {:>6.1} ms", v);
                }
                if let Some(v) = report.encoding_ms {
                    info!("  Encoding:       {:>6.1} ms", v);
                }
                if let Some(v) = report.encode_to_send_ms {
                    info!("  Encode→Send:    {:>6.1} ms", v);
                }
                if let Some(v) = report.network_ms {
                    info!("  Network:        {:>6.1} ms", v);
                }
                if let Some(v) = report.jitter_buffer_ms {
                    info!("  Jitter buffer:  {:>6.1} ms", v);
                }
                if let Some(v) = report.receive_to_decode_ms {
                    info!("  Receive→Decode: {:>6.1} ms", v);
                }
                if let Some(v) = report.decoding_ms {
                    info!("  Decoding:       {:>6.1} ms", v);
                }
                if let Some(v) = report.decode_to_display_ms {
                    info!("  Decode→Display: {:>6.1} ms", v);
                }
                info!("  ══════════════════════");
                info!("  TOTAL:          {:>6.1} ms", report.total_ms);
            }
        });
        
        Some(latency_tx)
    };

    // Adaptive bitrate: feeds off client keyframe-resync requests (primary,
    // loss-equivalent congestion signal) and decode latency reports
    // (secondary, holds off growth) to steer the encoder's bitrate between
    // --min-bitrate and --max-bitrate. See src/adaptive_bitrate.rs.
    let bitrate_event_tx = if adaptive_bitrate_enabled {
        let (bitrate_event_tx, bitrate_event_rx) = mpsc::channel::<BitrateEvent>(32);
        let adaptive_config = AdaptiveBitrateConfig {
            min_bitrate: config.min_bitrate,
            max_bitrate: config.max_bitrate,
            initial_bitrate: config.bitrate,
            ..Default::default()
        };
        let controller = AdaptiveBitrateController::new(
            adaptive_config,
            encoder_control.clone(),
            bitrate_event_rx,
            bitrate_tx.clone(),
        );
        tokio::spawn(controller.run());
        Some(bitrate_event_tx)
    } else {
        info!("Adaptive bitrate disabled, using fixed rate control: {:?}", rate_control);
        None
    };

    // Set by the session manager (and the `/stream` handler) when a new
    // client connects, so the capture loop renders+sends a frame right away
    // even if the screen hasn't changed -- otherwise a newly connected
    // client would see nothing until the next damage or the next periodic
    // keyframe-cadence render.
    let force_render = Arc::new(AtomicBool::new(false));

    // Create signaling state and server
    let signaling_state = SignalingState::new(
        resize_tx,
        touch_tx,
        mouse_tx,
        latency_tx,
        bitrate_event_tx,
        encoder_control,
        force_render.clone(),
        pending_ping_tx,
        bitrate_rx,
    );
    let video_tx = signaling_state.get_video_sender();
    let signaling_server = SignalingServer::new(signaling_state);

    // Spawn the signaling server
    let listen_addr = config.listen_addr.clone();
    let port = config.port;
    tokio::spawn(async move {
        if let Err(e) = signaling_server.serve(&listen_addr, port).await {
            tracing::error!("Signaling server error: {}", e);
        }
    });

    // Spawn the encoder packet forwarding task: every encoded packet goes to
    // the `/stream` WebSocket broadcast for WebCodecs clients. Also where a
    // pending client ping gets stamped onto the next packet (see
    // `SignalingMessage::Ping` in src/server.rs), and where the server-only
    // legs of the latency pipeline (capture→encode, encoding, encode→send)
    // get aggregated and logged -- these don't need synchronized clocks
    // since they're plain `Instant` deltas on this side only.
    tokio::spawn(async move {
        let mut encoder_handle = encoder;
        let mut stage_totals_ms = (0.0f64, 0.0f64, 0.0f64); // (capture_to_encode, encoding, encode_to_send)
        let mut stage_count = 0u32;
        let mut last_stage_log = std::time::Instant::now();
        const STAGE_LOG_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

        while let Some(mut packet) = encoder_handle.recv_packet().await {
            // Drain to the *latest* pending ping, not just the next one in
            // the queue: frames are only forced out every `keyframe_interval`
            // ticks when the screen is idle, so pings (sent every second)
            // can back up several deep. Echoing the oldest queued one would
            // measure a fake multi-second round trip against a ping that's
            // long since stale, even though the live one is fine.
            let mut latest_ping = None;
            while let Ok(client_ts) = pending_ping_rx.try_recv() {
                latest_ping = Some(client_ts);
            }
            if let Some(client_ts) = latest_ping {
                packet.ping_echo_client_ts = Some(client_ts);
            }

            let encode_to_send_ms = packet.encode_complete.elapsed().as_secs_f64() * 1000.0;
            stage_totals_ms.0 += packet.capture_to_encode_ms;
            stage_totals_ms.1 += packet.encoding_ms;
            stage_totals_ms.2 += encode_to_send_ms;
            stage_count += 1;

            if last_stage_log.elapsed() >= STAGE_LOG_INTERVAL {
                debug!(
                    "Server pipeline (avg over {} frames): capture→encode {:.1}ms, encoding {:.1}ms, encode→send {:.1}ms",
                    stage_count,
                    stage_totals_ms.0 / stage_count as f64,
                    stage_totals_ms.1 / stage_count as f64,
                    stage_totals_ms.2 / stage_count as f64,
                );
                stage_totals_ms = (0.0, 0.0, 0.0);
                stage_count = 0;
                last_stage_log = std::time::Instant::now();
            }

            let _ = video_tx.send(packet);
        }
    });

    info!("All systems ready. Connect Wayland clients with: WAYLAND_DISPLAY={}", config.display_name);
    
    // Dispatch initial Wayland events
    display.dispatch_clients(&mut state)
        .context("Failed to dispatch Wayland clients")?;
    
    info!("Starting compositor render loop");
    
    // Main event loop for Wayland compositor (synchronous)
    let frame_interval = std::time::Duration::from_secs_f64(1.0 / config.framerate as f64);
    // Self-correcting deadline: advanced by exactly `frame_interval` each frame
    // rather than snapped to wake time, so timing error doesn't accumulate.
    let mut next_frame = std::time::Instant::now() + frame_interval;
    // Buffers the encoder thread has finished with, recycled into render()
    // instead of allocating a fresh ~8MB framebuffer every frame.
    let mut spare_buffers: Vec<Vec<u8>> = Vec::new();
    // Ticks since the last actual render+encode. Bounds how stale the stream
    // can get on an unchanging screen: even with nothing dirty, force a
    // render every `keyframe_interval` ticks so a fresh keyframe still goes
    // out periodically (decoder resync after loss) rather than only once at
    // stream start.
    let mut ticks_since_render = 0u32;
    // Total frames lost to `frame_sender.try_send` finding the encoder queue
    // full (capacity 4). Expected as backpressure when the encoder lags, but
    // worth surfacing -- otherwise dropped frames look identical to a pacing
    // bug from the receiving end.
    let mut dropped_frames = 0u64;

    loop {
        let loop_start = std::time::Instant::now();

        // Check for resize requests (non-blocking)
        if let Ok((req_width, req_height)) = resize_rx.try_recv() {
            // Ensure dimensions are divisible by 16 for optimal H.264 encoding
            let new_width = (req_width / 16) * 16;
            let new_height = (req_height / 16) * 16;
            
            // Validate minimum dimensions (minimum 16x16 after rounding)
            if new_width < 16 || new_height < 16 {
                warn!("Ignoring resize request with dimensions too small: {}x{}", req_width, req_height);
                continue;
            }
            
            info!("Processing resize request: {}x{}", new_width, new_height);
            
            // Resize compositor output
            state.resize_output(new_width, new_height);
            
            // Update touch and pointer handler dimensions
            touch_handler.set_dimensions(new_width, new_height);
            mouse_handler.set_dimensions(new_width, new_height);
            
            // Resize encoder
            if let Err(e) = encoder_resize.send(Some(encoder::ResolutionChange {
                width: new_width,
                height: new_height,
            })) {
                warn!("Failed to send resize to encoder: {}", e);
            }
            
            info!("Resize complete: {}x{}", new_width, new_height);
        }
        
        // Process touch and pointer events (non-blocking, drain all available)
        while let Ok(touch_event) = touch_rx.try_recv() {
            touch_handler.handle_event(touch_event, &mut state);
        }
        while let Ok(mouse_event) = mouse_rx.try_recv() {
            mouse_handler.handle_event(mouse_event, &mut state);
        }
        
        // Dispatch Wayland events, capped at 16ms but never waiting past the
        // next frame deadline — otherwise this wait dominates the loop period
        // and capture lands at ~2x frame_interval instead of on cadence.
        let dispatch_timeout = next_frame
            .saturating_duration_since(loop_start)
            .min(std::time::Duration::from_millis(16));
        event_loop.dispatch(dispatch_timeout, &mut state)
            .context("Event loop dispatch failed")?;

        display.dispatch_clients(&mut state)
            .context("Failed to dispatch Wayland clients")?;

        display.flush_clients()
            .context("Failed to flush Wayland clients")?;

        // Render and send frame at target framerate. Frame callbacks are sent
        // from here too, at the same cadence, rather than every loop tick:
        // clients that redraw on every `frame.done` (e.g. cage) would otherwise
        // repaint as fast as the event loop spins instead of at the rate we
        // actually capture and encode, burning CPU for frames nobody captures.
        let now = std::time::Instant::now();
        if now >= next_frame {
            state.send_frames();

            while let Ok(buf) = buffer_return_rx.try_recv() {
                spare_buffers.push(buf);
            }

            // `take_dirty()` must run unconditionally (not short-circuited by
            // `||`) so the flag is always consumed, even on ticks where a
            // forced render makes its value moot.
            let screen_dirty = state.take_dirty();
            let new_client = force_render.swap(false, Ordering::Relaxed);
            let stale = ticks_since_render >= keyframe_interval;
            if screen_dirty || new_client || stale {
                if let Some(framebuffer) = state.render(spare_buffers.pop()) {
                    let raw_frame = encoder::RawFrame {
                        data: framebuffer,
                        capture_instant: std::time::Instant::now(),
                    };

                    // Send frame to encoder (non-blocking)
                    match frame_sender.try_send(raw_frame) {
                        Ok(()) => {
                            ticks_since_render = 0;
                        }
                        Err(_) => {
                            // Queue full: the encoder hasn't drained the
                            // previous frame(s) yet. Counts toward staleness
                            // too -- the encoder didn't actually get a fresh
                            // frame this tick.
                            dropped_frames += 1;
                            ticks_since_render += 1;
                            if dropped_frames == 1 || dropped_frames % 30 == 0 {
                                warn!(
                                    "Encoder queue full, dropped {} frame(s) so far",
                                    dropped_frames
                                );
                            }
                        }
                    }
                }
            } else {
                ticks_since_render += 1;
            }

            // Advance by exactly one interval (self-correcting cadence) rather
            // than snapping to `now`, which would let timing error accumulate.
            // If we've fallen behind by more than an interval (e.g. a stall),
            // resync to now instead of bursting frames to catch up.
            next_frame += frame_interval;
            if next_frame < now {
                next_frame = now + frame_interval;
            }

            display.flush_clients()
                .context("Failed to flush Wayland clients")?;
        }
    }
}
