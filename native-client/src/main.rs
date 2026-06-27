// CLI entry point. Phase 5 wires up the SW H.264 decoder + SHM
// renderer on top of Phase 3's transport and Phase 4's window:
//
//   1. spawn the Wayland display thread (creates the window titled
//      "waylandwebstream" + the ShmRenderer that blits frames into it)
//   2. parse `ws <URL>` from argv, connect the WebSocket transport
//   3. spawn the H.264 -> ARGB decoder thread; bridge the WS
//      transport to it via an H.264 packet channel
//   4. send `SignalingMessage::Ready`, then `Resize` with the initial size
//   5. recv frames in a loop, forward H.264 packets to the decoder
//      thread; the decoder feeds the display thread's renderer
//   6. exit on window-close, transport error, or Ctrl-C
//
// Audio decoding and input forwarding land in Phases 6 and 7.

use anyhow::{Context, Result};
use clap::Parser;
use std::sync::mpsc;
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;

mod decode;
mod display;
mod proto;
mod render;
mod transport;
mod types;

use decode::sw::{spawn_decoder_thread, DecodedFrame};
use display::spawn_display_thread;
use transport::{Frame, Transport};
use types::SignalingMessage;

/// Initial window size. Phase 4 waits for the compositor's first
/// xdg_toplevel Configure so we tell the server a size that matches
/// what we're actually drawing into. Until then we fall back to this.
const INITIAL_WINDOW_SIZE: (u32, u32) = (1280, 720);

#[derive(Parser, Debug)]
#[command(
    name = "wws-client",
    about = "Native Wayland client for the waylandwebstream /client endpoint"
)]
struct Args {
    /// Transport spec: `<kind> <args...>`. Phase 3 supports only `ws <URL>`,
    /// e.g. `ws ws://localhost:8080/client`. Other transports (unix/tcp/stdio)
    /// land in Phase 11.
    #[arg(required = true)]
    transport: Vec<String>,
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
    info!("wws-client starting ({:?})", transport_spec);

    // Build both channels up front, then hand each half to its
    // respective thread. `packet_tx` -> packet_rx feeds H.264 packets
    // to the decoder; `frame_tx` -> frame_rx feeds decoded frames to
    // the renderer.
    //
    // packet_tx: capacity 4 -- dropping *encoded* packets breaks the H.264
    // reference chain, corrupting the picture until the next IDR. A little
    // slack absorbs scheduler jitter without ever dropping. Capacity 1 was
    // too aggressive: any single-frame delay in the decoder caused corruption.
    //
    // frame_tx: capacity 1 -- dropping decoded frames is harmless (just
    // shows the previous frame for one tick), so stay tight here.
    let (packet_tx, packet_rx) = mpsc::sync_channel::<Vec<u8>>(4);
    let (frame_tx, frame_rx) = mpsc::sync_channel::<DecodedFrame>(1);

    // Display thread first: we want the window up before we start
    // burning CPU decoding frames into the void.
    let mut display =
        spawn_display_thread(INITIAL_WINDOW_SIZE, frame_rx).context("Wayland display")?;
    let initial_size = *display.size_rx.borrow();
    info!("window initial size: {}x{}", initial_size.0, initial_size.1);

    let mut transport = match transport_spec {
        TransportSpec::Ws(url) => transport::websocket::WsTransport::connect(&url)
            .await
            .with_context(|| format!("failed to connect to {}", url))?,
    };
    info!("connected; sending Ready + Resize");

    // Handshake.
    let ready = serde_json::to_string(&SignalingMessage::Ready)
        .context("SignalingMessage::Ready always serializes")?;
    transport
        .send(&ready)
        .await
        .context("failed to send Ready")?;
    let resize = serde_json::to_string(&SignalingMessage::Resize {
        width: initial_size.0,
        height: initial_size.1,
    })?;
    transport.send(&resize).await?;

    // Spawn the H.264 -> ARGB decoder thread. It reads packet bodies
    // from `packet_rx` (one H.264 Annex-B NAL unit per message) and
    // pushes `DecodedFrame`s through `frame_tx` to the display
    // thread's renderer.
    let (_decoder_join, _decoder_count) = spawn_decoder_thread(packet_rx, frame_tx);
    debug!("decoder thread spawned");

    loop {
        tokio::select! {
            frame = transport.recv() => {
                let frame = match frame {
                    Ok(f) => f,
                    Err(e) => {
                        warn!("recv error: {}; exiting", e);
                        break;
                    }
                };
                handle_frame(frame, &packet_tx);
            }
            changed = display.size_rx.changed() => {
                if changed.is_ok() {
                    let (w, h) = *display.size_rx.borrow();
                    info!("window resized to {w}x{h}; notifying server");
                    let resize = serde_json::to_string(&SignalingMessage::Resize {
                        width: w, height: h,
                    })?;
                    if let Err(e) = transport.send(&resize).await {
                        warn!("failed to send Resize: {}; exiting", e);
                        break;
                    }
                }
            }
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

/// Hand a wire frame off to the right consumer. Phase 5 only routes
/// `VideoFrame` to the decoder thread; `AudioFrame` is logged (Phase
/// 6 plays it) and `Control` is logged verbatim.
fn handle_frame(frame: Frame, packet_tx: &mpsc::SyncSender<Vec<u8>>) {
    match frame {
        Frame::VideoFrame {
            is_keyframe,
            frame_id,
            ping_echo,
            data,
            capture_to_encode_ms,
        } => {
            debug!(
                "VideoFrame id={frame_id} keyframe={is_keyframe} {} bytes (c2e={capture_to_encode_ms:.2}ms)",
                data.len()
            );
            if ping_echo != 0.0 {
                debug!("  ping_echo_client_ts={ping_echo}");
            }
            // `try_send`: if the decoder is consistently behind (4+
            // frames), drop. The next IDR (~2s away at default GOP)
            // will resync. Channel capacity 4 absorbs transient jitter
            // so we only drop when genuinely overwhelmed.
            if packet_tx.try_send(data).is_err() {
                debug!("decoder behind; dropping H.264 packet");
            }
        }
        Frame::AudioFrame { pts_us, data } => {
            debug!("AudioFrame pts={pts_us}us {} bytes (Phase 6 plays it)", data.len());
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
