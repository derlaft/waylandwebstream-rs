// CLI entry point. Phase 4 wires up the Wayland window on top of Phase 3's
// transport:
//   1. spawn the Wayland display thread (creates the window titled
//      "waylandwebstream")
//   2. parse `ws <URL>` from argv, connect the WebSocket transport
//   3. send `SignalingMessage::Ready`, then `Resize` with the initial size
//   4. recv frames in a loop, print type/size/keyframe flag, exit on
//      Ctrl-C or window-close
//
// Audio decoding, video decoding, rendering, and input forwarding are
// added in later phases -- see docs/native-client-plan.md Part 4.

use anyhow::{Context, Result};
use clap::Parser;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

mod display;
mod proto;
mod transport;
mod types;

use display::spawn_display_thread;
use transport::{Frame, Transport};
use types::SignalingMessage;

/// Initial window size. Phase 5+ will resync this to whatever the
/// compositor assigns via xdg_toplevel::Configure.
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

    let mut display = spawn_display_thread(INITIAL_WINDOW_SIZE).context("Wayland display")?;
    // Wait for the compositor's first Configure so we tell the server a
    // size that matches what we're actually drawing into. Until then we
    // fall back to INITIAL_WINDOW_SIZE.
    let initial_size = *display.size_rx.borrow_and_update();
    info!("window initial size: {}x{}", initial_size.0, initial_size.1);

    let mut transport = match transport_spec {
        TransportSpec::Ws(url) => transport::websocket::WsTransport::connect(&url)
            .await
            .with_context(|| format!("failed to connect to {}", url))?,
    };
    info!("connected; sending Ready + Resize");

    let ready = serde_json::to_string(&SignalingMessage::Ready)
        .context("SignalingMessage::Ready always serializes")?;
    transport
        .send(&ready)
        .await
        .context("failed to send Ready")?;
    let resize = serde_json::to_string(&SignalingMessage::Resize {
        width: initial_size.0,
        height: initial_size.1,
    })
    .context("SignalingMessage::Resize always serializes")?;
    transport
        .send(&resize)
        .await
        .context("failed to send Resize")?;

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
                print_frame(&frame);
            }
            changed = display.size_rx.changed() => {
                if changed.is_ok() {
                    let (w, h) = *display.size_rx.borrow();
                    info!("window resized to {w}x{h}; notifying server");
                    let resize = serde_json::to_string(&SignalingMessage::Resize {
                        width: w,
                        height: h,
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

    // The OS will close the socket on exit; the server's broadcast channel
    // will see the lag and the WS handler will return on its own. A proper
    // graceful-close handshake lands when input forwarding is added (Phase 7).
    Ok(())
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

fn print_frame(frame: &Frame) {
    match frame {
        Frame::VideoFrame {
            is_keyframe,
            frame_id,
            ping_echo,
            data,
            capture_to_encode_ms,
        } => {
            println!(
                "VideoFrame id={} keyframe={} {} bytes (capture_to_encode={:.2}ms)",
                frame_id, is_keyframe, data.len(), capture_to_encode_ms
            );
            if *ping_echo != 0.0 {
                println!("  ping_echo_client_ts={}", ping_echo);
            }
        }
        Frame::AudioFrame { pts_us, data } => {
            println!("AudioFrame pts={}us {} bytes", pts_us, data.len());
        }
        Frame::Control(msg) => {
            println!("Control: {:?}", msg);
        }
    }
}