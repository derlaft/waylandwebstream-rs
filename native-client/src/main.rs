// CLI entry point. Phase 3 wires up just enough to satisfy the
// "client connects and prints frames" milestone:
//   1. parse `ws <URL>` from argv
//   2. connect the WebSocket transport
//   3. send `SignalingMessage::Ready`
//   4. recv frames in a loop, print type/size/keyframe flag, exit on Ctrl-C
//
// Audio decoding, video decoding, rendering, and input forwarding are
// added in later phases -- see docs/native-client-plan.md Part 4.

use anyhow::{Context, Result};
use clap::Parser;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

mod proto;
mod transport;
mod types;

use transport::{Frame, Transport};
use types::SignalingMessage;

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

    let mut transport = match transport_spec {
        TransportSpec::Ws(url) => transport::websocket::WsTransport::connect(&url)
            .await
            .with_context(|| format!("failed to connect to {}", url))?,
    };
    info!("connected; sending Ready");

    let ready = serde_json::to_string(&SignalingMessage::Ready)
        .context("SignalingMessage::Ready always serializes")?;
    transport
        .send(&ready)
        .await
        .context("failed to send Ready")?;

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