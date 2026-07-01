// Native Wayland client — Phases 6-8 wiring.
//
// Phase 6: AudioPlayer decodes Opus frames from the server and pushes
//          PCM to a PipeWire output stream on a dedicated thread.
// Phase 7: Input events (pointer, keyboard) from the Wayland event loop
//          are forwarded to the server as SignalingMessage JSON.
// Phase 8: LatencyTracker records frame arrivals and RTT; sends Ping
//          and Latency reports to the server on a 5-second interval.
//
// Earlier phases (1-5) provide the transport, H.264 decoder, SHM
// renderer, and Wayland window.

use anyhow::{Context, Result};
use clap::Parser;
use std::sync::mpsc;
use std::time::Duration;
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;

mod audio;
mod decode;
mod display;
mod input;
mod latency;
mod proto;
mod render;
mod transport;
mod types;

use audio::AudioPlayer;
use decode::sw::{spawn_decoder_thread, DecodedFrame};
use display::{spawn_display_thread, RendererKind};
use latency::LatencyTracker;
use transport::{Frame, FrameError, Transport};
use types::SignalingMessage;

const INITIAL_WINDOW_SIZE: (u32, u32) = (1280, 720);
// Interval for ping + latency report (matches plan §3.12).
const LATENCY_INTERVAL_SECS: u64 = 5;

#[derive(Parser, Debug)]
#[command(
    name = "wws-client",
    about = "Native Wayland client for the waylandwebstream /client endpoint"
)]
struct Args {
    /// Transport spec: `<kind> <args...>`. Only `ws <URL>` is supported
    /// in this phase; e.g. `ws ws://localhost:8080/client`.
    #[arg(required = true)]
    transport: Vec<String>,

    /// Disable audio playback (skip PipeWire initialization).
    #[arg(long)]
    no_audio: bool,

    /// Rendering backend: `shm` (CPU blit, default) or `egl` (OpenGL ES).
    #[arg(long, default_value = "shm")]
    renderer: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    let transport_spec = parse_transport(&args.transport)?;
    let renderer_kind = match args.renderer.as_str() {
        "egl" => RendererKind::Egl,
        _ => RendererKind::Shm,
    };
    info!(
        "wws-client starting ({:?}, renderer={:?})",
        transport_spec, renderer_kind
    );

    // --- channels ---
    // capacity 4: small but not 1; absorbs one-frame scheduler jitter
    // without risking H.264 reference-chain corruption from drops.
    let (packet_tx, packet_rx) = mpsc::sync_channel::<Vec<u8>>(4);
    let (frame_tx, frame_rx) = mpsc::sync_channel::<DecodedFrame>(1);

    // --- Wayland display (Phase 4+) + input channel (Phase 7) ---
    let mut display = spawn_display_thread(INITIAL_WINDOW_SIZE, frame_rx, renderer_kind)
        .context("Wayland display")?;

    // Wait for the display thread's initial xdg_toplevel::configure so we
    // send the real compositor-assigned size, not the hard-coded fallback.
    // Local Wayland IPC is fast; 2 s is a very generous ceiling.
    let _ = tokio::time::timeout(Duration::from_secs(2), display.size_rx.changed()).await;
    // borrow_and_update marks this version as seen so the resize arm in the
    // select! loop below doesn't fire again for the initial configure.
    let initial_size = *display.size_rx.borrow_and_update();
    info!("window initial size: {}x{}", initial_size.0, initial_size.1);

    // --- transport (Phase 3) ---
    let mut transport = match transport_spec {
        TransportSpec::Ws(url) => transport::websocket::WsTransport::connect(&url)
            .await
            .with_context(|| format!("failed to connect to {url}"))?,
    };
    info!("connected; sending Ready + Resize");

    let ready = serde_json::to_string(&SignalingMessage::Ready)?;
    transport.send(&ready).await.context("send Ready")?;
    let resize = serde_json::to_string(&SignalingMessage::Resize {
        width: initial_size.0,
        height: initial_size.1,
    })?;
    transport.send(&resize).await?;

    // --- H.264 decoder thread (Phase 5) ---
    let (_decoder_join, _decoder_count) = spawn_decoder_thread(packet_rx, frame_tx);
    debug!("decoder thread spawned");

    // --- audio player (Phase 6) ---
    let mut audio: Option<AudioPlayer> = if args.no_audio {
        info!("audio disabled by --no-audio");
        None
    } else {
        match AudioPlayer::spawn() {
            Ok(p) => {
                info!("audio player started");
                Some(p)
            }
            Err(e) => {
                warn!("audio init failed ({e:#}); continuing without audio");
                None
            }
        }
    };

    // --- latency tracker (Phase 8) ---
    let mut tracker = LatencyTracker::new();
    let mut latency_tick = tokio::time::interval(Duration::from_secs(LATENCY_INTERVAL_SECS));
    // Don't fire immediately on startup — let the pipeline warm up.
    latency_tick.tick().await;

    // --- main event loop ---
    loop {
        tokio::select! {
            // Incoming frame from server.
            frame = transport.recv() => {
                match frame {
                    Ok(f) => handle_frame(f, &packet_tx, audio.as_mut(), &mut tracker),
                    Err(e) => {
                        // A clean server-side close (kicked because another
                        // client connected, or server shutdown) is an
                        // expected, graceful exit -- not an error. There's no
                        // auto-reconnect in the native client, so we just
                        // exit either way.
                        if matches!(e.downcast_ref::<FrameError>(), Some(FrameError::Closed)) {
                            info!("connection closed by server; exiting");
                        } else {
                            warn!("recv error: {}; exiting", e);
                        }
                        break;
                    }
                }
            }

            // Input event from Wayland display thread (Phase 7).
            Some(msg) = display.input_rx.recv() => {
                match serde_json::to_string(&msg) {
                    Ok(json) => {
                        if let Err(e) = transport.send(&json).await {
                            warn!("failed to forward input: {e}; exiting");
                            break;
                        }
                    }
                    Err(e) => warn!("input serialize error: {e}"),
                }
            }

            // Ping + latency report interval (Phase 8).
            _ = latency_tick.tick() => {
                if let Some(ping) = tracker.maybe_ping() {
                    match serde_json::to_string(&ping) {
                        Ok(json) => {
                            if let Err(e) = transport.send(&json).await {
                                warn!("failed to send ping: {e}; exiting");
                                break;
                            }
                        }
                        Err(e) => warn!("ping serialize error: {e}"),
                    }
                }
                let report = tracker.flush_report();
                match serde_json::to_string(&report) {
                    Ok(json) => {
                        if let Err(e) = transport.send(&json).await {
                            warn!("failed to send latency report: {e}; exiting");
                            break;
                        }
                        debug!("sent latency report");
                    }
                    Err(e) => warn!("latency report serialize error: {e}"),
                }
            }

            // Window resized by compositor.
            changed = display.size_rx.changed() => {
                if changed.is_ok() {
                    let (w, h) = *display.size_rx.borrow();
                    info!("window resized to {w}x{h}; notifying server");
                    let resize = serde_json::to_string(&SignalingMessage::Resize {
                        width: w, height: h,
                    })?;
                    if let Err(e) = transport.send(&resize).await {
                        warn!("failed to send Resize: {e}; exiting");
                        break;
                    }
                }
            }

            // Window closed by compositor.
            changed = display.close_rx.changed() => {
                if changed.is_ok() && *display.close_rx.borrow() {
                    info!("window closed; exiting");
                    break;
                }
            }

            _ = tokio::signal::ctrl_c() => {
                info!("Ctrl-C received; exiting");
                break;
            }
        }
    }
    Ok(())
}

/// Dispatch one wire frame from the server to the appropriate consumer.
fn handle_frame(
    frame: Frame,
    packet_tx: &mpsc::SyncSender<Vec<u8>>,
    audio: Option<&mut AudioPlayer>,
    tracker: &mut LatencyTracker,
) {
    match frame {
        Frame::VideoFrame {
            is_keyframe,
            frame_id,
            ping_echo,
            data,
            capture_to_encode_ms,
        } => {
            debug!(
                "VideoFrame id={frame_id} keyframe={is_keyframe} {} bytes \
                 (c2e={capture_to_encode_ms:.2}ms)",
                data.len()
            );
            tracker.record_arrival(ping_echo);
            if packet_tx.try_send(data).is_err() {
                debug!("decoder behind; dropping H.264 packet");
            }
        }
        Frame::AudioFrame { pts_us, data } => {
            debug!("AudioFrame pts={pts_us}us {} bytes", data.len());
            if let Some(player) = audio {
                if let Err(e) = player.push_opus(&data) {
                    warn!("Opus decode error: {e:#}");
                }
            }
        }
        Frame::Control(msg) => {
            info!("control: {msg:?}");
        }
    }
}

#[derive(Debug)]
enum TransportSpec {
    Ws(String),
}

fn parse_transport(args: &[String]) -> Result<TransportSpec> {
    let mut iter = args.iter();
    let kind = iter
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing transport kind (expected `ws <URL>`)"))?;
    match kind.as_str() {
        "ws" => {
            let url = iter
                .next()
                .ok_or_else(|| anyhow::anyhow!("`ws` requires a URL argument"))?
                .clone();
            if iter.next().is_some() {
                anyhow::bail!("`ws` takes exactly one argument (the URL)");
            }
            Ok(TransportSpec::Ws(url))
        }
        other => anyhow::bail!(
            "unknown transport `{}`; only `ws <URL>` is supported in this phase",
            other
        ),
    }
}
