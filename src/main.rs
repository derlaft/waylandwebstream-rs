use anyhow::{Context, Result};
use clap::Parser;
use smithay::reexports::{
    calloop::EventLoop,
    wayland_server::Display,
};
use std::sync::Arc;
use tracing::{info, Level};
use tracing_subscriber::FmtSubscriber;

mod compositor;
mod config;
mod encoder;
mod input;
mod web;
mod webrtc;

use compositor::CompositorState;
use encoder::{EncoderConfig, spawn_encoder};

fn main() -> Result<()> {
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

    info!("\n╔══════════════════════════════════════════════════════════════╗");
    info!("║  Phase 2 Implementation Complete: Video Encoding Pipeline   ║");
    info!("╠══════════════════════════════════════════════════════════════╣");
    info!("║  ✓ FFmpeg x264 encoder initialized                          ║");
    info!("║  ✓ Codec: H.264 baseline, zerolatency tune                  ║");
    info!("║  ✓ Resolution: {}x{} @ 30fps                       ║", width, height);
    info!("║  ✓ Bitrate: 2 Mbps, keyframe interval: 2s                   ║");
    info!("║  ✓ Pixel format conversion: BGRA -> YUV420P                 ║");
    info!("║  ✓ Frame pacing with drop-on-full policy                    ║");
    info!("║  ✓ Dynamic resolution support (encoder reinit)              ║");
    info!("╠══════════════════════════════════════════════════════════════╣");
    info!("║  Next Steps (Phase 3):                                      ║");
    info!("║  - Implement WebRTC signaling server                        ║");
    info!("║  - Set up RTP packetization for H.264                       ║");
    info!("║  - Create browser client for video playback                 ║");
    info!("╚══════════════════════════════════════════════════════════════╝\n");

    info!("Compositor and encoder running. Press Ctrl+C to stop.");
    info!("Connect clients with: WAYLAND_DISPLAY={}", config.display_name);
    
    // Dispatch initial Wayland events
    display.dispatch_clients(&mut state)
        .context("Failed to dispatch Wayland clients")?;
    
    // Main event loop
    // For Phase 2, we integrate the encoder with a simple render loop
    // The Wayland event loop runs synchronously and we generate test frames
    let mut frame_count = 0u64;
    let frame_interval = std::time::Duration::from_millis(33); // ~30fps
    let mut last_frame = std::time::Instant::now();
    
    loop {
        // Dispatch Wayland events (non-blocking with 16ms timeout)
        event_loop.dispatch(std::time::Duration::from_millis(16), &mut state)
            .context("Event loop dispatch failed")?;
            
        display.dispatch_clients(&mut state)
            .context("Failed to dispatch Wayland clients")?;
        
        display.flush_clients()
            .context("Failed to flush Wayland clients")?;

        // Check if it's time to generate a frame
        let now = std::time::Instant::now();
        if now.duration_since(last_frame) >= frame_interval {
            // Generate a simple test frame (black screen for now)
            // In Phase 3+, this will be actual compositor output
            let framebuffer = vec![0u8; (width * height * 4) as usize];
            
            let raw_frame = encoder::RawFrame {
                data: framebuffer,
                width,
                height,
                timestamp: (frame_count * 3000) as i64, // 90kHz clock, 30fps
            };
            
            // Try to send frame (non-blocking)
            if encoder.try_send_frame(raw_frame).is_ok() {
                frame_count += 1;
                
                if frame_count % 300 == 0 {
                    info!("Generated {} frames", frame_count);
                }
            }
            
            last_frame = now;
        }
    }
}
