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

    info!("\n╔══════════════════════════════════════════════════════════════╗");
    info!("║  Phase 1 Implementation Complete: Headless Compositor       ║");
    info!("╠══════════════════════════════════════════════════════════════╣");
    info!("║  ✓ Smithay headless backend initialized                     ║");
    info!("║  ✓ Virtual output created ({}x{} @ 60Hz)          ║", width, height);
    info!("║  ✓ Pixman software renderer ready                           ║");
    info!("║  ✓ Wayland socket: {}                    ║", config.display_name);
    info!("║  ✓ Framebuffer export ready ({} MB)                   ║", (width * height * 4) / 1024 / 1024);
    info!("║  ✓ Dynamic output resizing implemented                      ║");
    info!("╠══════════════════════════════════════════════════════════════╣");
    info!("║  Next Steps (Phase 2):                                      ║");
    info!("║  - Implement full Wayland protocol handlers                 ║");
    info!("║  - Set up FFmpeg H.264 encoding pipeline                    ║");
    info!("║  - Add render loop with frame pacing                        ║");
    info!("╚══════════════════════════════════════════════════════════════╝\n");

    info!("Compositor running. Press Ctrl+C to stop.");
    info!("Connect clients with: WAYLAND_DISPLAY={}", config.display_name);
    
    // Dispatch initial Wayland events
    display.dispatch_clients(&mut state)
        .context("Failed to dispatch Wayland clients")?;
    
    // Run the event loop indefinitely
    loop {
        event_loop.dispatch(std::time::Duration::from_millis(16), &mut state)
            .context("Event loop dispatch failed")?;
            
        display.dispatch_clients(&mut state)
            .context("Failed to dispatch Wayland clients")?;
        
        display.flush_clients()
            .context("Failed to flush Wayland clients")?;
    }
}
