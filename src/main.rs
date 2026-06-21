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
mod web;
mod webrtc;

use compositor::CompositorState;
use encoder::{EncoderConfig, spawn_encoder};
use webrtc::session::SessionManager;
use webrtc::signaling::{SignalingServer, SignalingState};

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
                // Accept new clients with empty client data
                if let Err(e) = display_handle.insert_client(client_stream, Arc::new(())) {
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
        framerate: 30,
        bitrate: 2_000_000,
        keyframe_interval: 60,
    };
    
    let encoder = spawn_encoder(encoder_config)?;

    // Create channels for WebRTC
    let (offer_tx, offer_rx) = mpsc::channel(4);
    let (packet_tx, packet_rx) = mpsc::channel(16);

    // Create signaling state and server
    let signaling_state = SignalingState::new(offer_tx);
    let ice_tx = signaling_state.get_ice_sender();
    let signaling_server = SignalingServer::new(signaling_state.clone());

    info!("\n╔══════════════════════════════════════════════════════════════╗");
    info!("║  Phase 3 Implementation Complete: WebRTC Streaming          ║");
    info!("╠══════════════════════════════════════════════════════════════╣");
    info!("║  ✓ WebRTC peer connection with H.264 support                ║");
    info!("║  ✓ HTTP/WebSocket signaling server                          ║");
    info!("║  ✓ RTP packetization (via webrtc-rs)                        ║");
    info!("║  ✓ Browser client with video playback                       ║");
    info!("║  ✓ ICE/STUN support for NAT traversal                       ║");
    info!("╠══════════════════════════════════════════════════════════════╣");
    info!("║  Server Configuration:                                       ║");
    info!("║  - Resolution: {}x{} @ 30fps                       ║", width, height);
    info!("║  - Bitrate: 2 Mbps, H.264 baseline profile                  ║");
    info!("║  - HTTP port: {}                                         ║", config.port);
    info!("║  - Wayland display: {}                         ║", config.display_name);
    info!("╠══════════════════════════════════════════════════════════════╣");
    info!("║  Connect with browser:                                       ║");
    info!("║  http://localhost:{}                                      ║", config.port);
    info!("╠══════════════════════════════════════════════════════════════╣");
    info!("║  Next Steps (Phase 4):                                      ║");
    info!("║  - Implement touch input handling                            ║");
    info!("║  - Add keyboard/mouse support                                ║");
    info!("║  - Bidirectional data channel communication                  ║");
    info!("╚══════════════════════════════════════════════════════════════╝\n");

    info!("Server starting on port {}...", config.port);
    
    // Spawn the signaling server
    let port = config.port;
    tokio::spawn(async move {
        if let Err(e) = signaling_server.serve(port).await {
            tracing::error!("Signaling server error: {}", e);
        }
    });

    // Get frame sender and encoder control sender before moving encoder
    let frame_sender = encoder.get_frame_sender();
    let encoder_control = encoder.get_control_sender();

    // Spawn the session manager with ICE sender and encoder control
    let session_manager = SessionManager::new(offer_rx, packet_rx, ice_tx, encoder_control);
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
    
    // Spawn a tokio task to generate test frames
    let frame_width = width;
    let frame_height = height;
    tokio::spawn(async move {
        info!("Test pattern generator started: {}x{} @ 30fps", frame_width, frame_height);
        let mut frame_count = 0u64;
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(33)); // ~30fps
        
        loop {
            interval.tick().await;
            
            // Generate animated gradient test pattern
            let mut framebuffer = vec![0u8; (frame_width * frame_height * 4) as usize];
            for y in 0..frame_height {
                for x in 0..frame_width {
                    let idx = ((y * frame_width + x) * 4) as usize;
                    framebuffer[idx] = ((x * 255) / frame_width) as u8;     // Blue
                    framebuffer[idx + 1] = ((y * 255) / frame_height) as u8; // Green
                    framebuffer[idx + 2] = (frame_count % 255) as u8; // Red (animated)
                    framebuffer[idx + 3] = 255; // Alpha
                }
            }
            
            let raw_frame = encoder::RawFrame {
                data: framebuffer,
                width: frame_width,
                height: frame_height,
                timestamp: (frame_count * 3000) as i64, // 90kHz clock, 30fps
            };
            
            // Send frame to encoder
            if frame_sender.try_send(raw_frame).is_ok() {
                frame_count += 1;
            }
        }
    });
    
    // Main event loop for Wayland compositor (synchronous)
    loop {
        // Dispatch Wayland events (non-blocking with 16ms timeout)
        event_loop.dispatch(std::time::Duration::from_millis(16), &mut state)
            .context("Event loop dispatch failed")?;
            
        display.dispatch_clients(&mut state)
            .context("Failed to dispatch Wayland clients")?;
        
        display.flush_clients()
            .context("Failed to flush Wayland clients")?;
    }
}
