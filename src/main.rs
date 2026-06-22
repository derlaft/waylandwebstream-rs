use anyhow::{Context, Result};
use clap::Parser;
use smithay::reexports::{
    calloop::EventLoop,
    wayland_server::Display,
};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{info, warn, Level};
use tracing_subscriber::FmtSubscriber;

mod compositor;
mod config;
mod encoder;
mod input;
mod latency;
mod web;
mod webrtc;


use compositor::CompositorState;
use encoder::{EncoderConfig, spawn_encoder};
use input::mouse::MouseHandler;
use input::touch::TouchHandler;
use webrtc::session::SessionManager;
use webrtc::signaling::{SignalingServer, SignalingState};
use webrtc::turn_server::{self, IceServerConfig, TurnCredentials};

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
    let encoder_config = EncoderConfig {
        width,
        height,
        framerate: config.framerate,
        bitrate: config.bitrate,
        keyframe_interval: config.framerate * 2,
    };
    
    let encoder = spawn_encoder(encoder_config)?;

    // Create channels for WebRTC
    let (offer_tx, offer_rx) = mpsc::channel(4);
    let (packet_tx, packet_rx) = mpsc::channel(16);
    let (remote_ice_tx, remote_ice_rx) = mpsc::channel(16);
    let (resize_tx, mut resize_rx) = mpsc::channel::<(u32, u32)>(4);
    let (touch_tx, mut touch_rx) = mpsc::channel(32); // Higher capacity for touch events
    let (mouse_tx, mut mouse_rx) = mpsc::channel(64); // Higher capacity for pointer moves

    // Start the embedded TURN relay. Browsers' mDNS-obfuscated host
    // candidates can't be resolved over networks (like netbird's WireGuard
    // overlay) that don't carry multicast traffic, so a TURN relay is needed
    // for ICE to have any usable candidate pair.
    let turn_relay_ip = if let Some(ip_str) = &config.turn_public_ip {
        ip_str
            .parse()
            .context("Invalid --turn-public-ip address")?
    } else {
        turn_server::detect_relay_address()
            .context("Failed to auto-detect a TURN relay address; pass --turn-public-ip")?
    };
    info!("Embedded TURN relay address: {}:{}", turn_relay_ip, config.turn_port);

    let turn_credentials = TurnCredentials::generate();
    let ice_config = IceServerConfig {
        stun_url: config.stun.clone(),
        turn_url: format!("turn:{}:{}", turn_relay_ip, config.turn_port),
        turn_username: turn_credentials.username.clone(),
        turn_password: turn_credentials.password.clone(),
    };

    // Kept alive for the lifetime of the process; the main loop below never returns.
    let _turn_server = turn_server::spawn_turn_server(config.turn_port, turn_relay_ip, &turn_credentials)
        .await
        .context("Failed to start embedded TURN server")?;

    // Create touch and pointer handlers
    let mut touch_handler = TouchHandler::new(width, height);
    let mut mouse_handler = MouseHandler::new(width, height);

    info!("\n╔══════════════════════════════════════════════════════════════╗");
    info!("║  WaylandWebStream - Latency Reporting Enabled               ║");
    info!("╠══════════════════════════════════════════════════════════════╣");
    info!("║  ✓ WebRTC peer connection with H.264 support                ║");
    info!("║  ✓ HTTP/WebSocket signaling server                          ║");
    info!("║  ✓ RTP packetization (via webrtc-rs)                        ║");
    info!("║  ✓ Browser client with video playback                       ║");
    info!("║  ✓ ICE/STUN support for NAT traversal                       ║");
    info!("║  ✓ Touch input handling (multi-touch support)               ║");
    info!("║  ✓ Client-to-server latency reporting                       ║");
    info!("╠══════════════════════════════════════════════════════════════╣");
    info!("║  Server Configuration:                                       ║");
    info!("║  - Resolution: {}x{} @ {}fps                       ║", width, height, config.framerate);
    info!("║  - Bitrate: {} bps                                          ║", config.bitrate);
    info!("║  - HTTP port: {}                                         ║", config.port);
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
        
        // Spawn task to log detailed latency reports
        tokio::spawn(async move {
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

    // Create signaling state and server
    let signaling_state = SignalingState::new(offer_tx.clone(), remote_ice_tx.clone(), resize_tx, touch_tx, mouse_tx, latency_tx, ice_config.clone());
    let ice_tx = signaling_state.get_ice_sender();
    let signaling_server = SignalingServer::new(signaling_state.clone());
    
    // Spawn the signaling server
    let port = config.port;
    tokio::spawn(async move {
        if let Err(e) = signaling_server.serve(port).await {
            tracing::error!("Signaling server error: {}", e);
        }
    });

    // Spawn the session manager
    let session_manager = SessionManager::new(offer_rx, packet_rx, remote_ice_rx, ice_tx.clone(), encoder_control, ice_config, config.framerate);
    tokio::spawn(async move {
        if let Err(e) = session_manager.run().await {
            tracing::error!("Session manager error: {}", e);
        }
    });

    // Spawn the encoder packet forwarding task
    tokio::spawn(async move {
        let mut encoder_handle = encoder;
        while let Some(packet) = encoder_handle.recv_packet().await {
            if packet_tx.send(packet).await.is_err() {
                break;
            }
        }
    });

    info!("All systems ready. Connect Wayland clients with: WAYLAND_DISPLAY={}", config.display_name);
    
    // Dispatch initial Wayland events
    display.dispatch_clients(&mut state)
        .context("Failed to dispatch Wayland clients")?;
    
    info!("Starting compositor render loop");
    
    // Main event loop for Wayland compositor (synchronous)
    let mut frame_count = 0u64;
    let frame_interval = std::time::Duration::from_secs_f64(1.0 / config.framerate as f64);
    let frame_timestamp_step = 90_000 / config.framerate as i64; // 90kHz RTP clock
    let mut last_frame = std::time::Instant::now();
    
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
        
        // Dispatch Wayland events (non-blocking with 16ms timeout)
        event_loop.dispatch(std::time::Duration::from_millis(16), &mut state)
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
        if loop_start.duration_since(last_frame) >= frame_interval {
            state.send_frames();

            if let Some(framebuffer) = state.render() {
                let raw_frame = encoder::RawFrame {
                    data: framebuffer,
                    width: state.width,
                    height: state.height,
                    timestamp: (frame_count as i64) * frame_timestamp_step,
                    capture_time: std::time::Instant::now(),
                };

                // Send frame to encoder (non-blocking)
                if frame_sender.try_send(raw_frame).is_ok() {
                    frame_count += 1;
                }
            }
            last_frame = loop_start;

            display.flush_clients()
                .context("Failed to flush Wayland clients")?;
        }
    }
}
