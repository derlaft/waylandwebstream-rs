// HTTP/WebSocket server: control channel (input/resize/latency) and the
// binary video stream consumed by the browser's WebCodecs decoder.

use anyhow::{Context, Result};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::{Html, Response},
    routing::get,
    Router,
};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc};
use tower_http::trace::TraceLayer;
use tracing::{info, warn};

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::adaptive_bitrate::BitrateEvent;
use crate::encoder::{EncodedPacket, EncoderControl};
use crate::input::mouse::MouseEvent;
use crate::input::touch::TouchEvent;
use crate::latency::LatencyReport;
use crate::web::client_html::CLIENT_HTML;

/// Signaling messages between client and server
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SignalingMessage {
    #[serde(rename = "ready")]
    Ready,
    #[serde(rename = "resize")]
    Resize { width: u32, height: u32 },
    #[serde(rename = "touch")]
    Touch {
        #[serde(flatten)]
        event: TouchEvent,
    },
    #[serde(rename = "pointer")]
    Pointer {
        #[serde(flatten)]
        event: MouseEvent,
    },
    /// Sent when the client's WebCodecs decoder falls behind and has to drop
    /// frames -- waiting out the periodic GOP-cycle keyframe (every
    /// `keyframe_interval`, seconds by default) would freeze the picture for
    /// multiple seconds, so the client asks for an immediate resync instead.
    #[serde(rename = "request_keyframe")]
    RequestKeyframe,
    #[serde(rename = "latency")]
    Latency {
        #[serde(default)]
        encoding_ms: Option<f64>,
        #[serde(default)]
        network_ms: Option<f64>,
        #[serde(default)]
        jitter_buffer_ms: Option<f64>,
        #[serde(default)]
        decoding_ms: Option<f64>,
        total_ms: f64,
    },
}

/// Shared state for the server
#[derive(Clone)]
pub struct SignalingState {
    /// Channel to send resize requests from clients
    resize_tx: mpsc::Sender<(u32, u32)>,
    /// Channel to send touch events from clients
    touch_tx: mpsc::Sender<TouchEvent>,
    /// Channel to send pointer (mouse/pen) events from clients
    mouse_tx: mpsc::Sender<MouseEvent>,
    /// Channel to send latency reports from clients
    latency_tx: Option<mpsc::Sender<LatencyReport>>,
    /// Feeds keyframe-request and latency signals to the adaptive bitrate
    /// controller. `None` when adaptive bitrate is disabled (fixed bitrate
    /// or constant-quality mode).
    bitrate_event_tx: Option<mpsc::Sender<BitrateEvent>>,
    /// Broadcasts encoded video packets to `/stream` WebSocket clients. Small
    /// capacity is deliberate: a slow client should skip forward to a recent
    /// frame rather than build up a backlog, since H.264 P-frames in the
    /// backlog would be stale by the time they're sent anyway. The next
    /// periodic keyframe resyncs any client that fell behind and missed some.
    video_tx: broadcast::Sender<EncodedPacket>,
    /// Lets a new `/stream` client request a fresh keyframe -- without this,
    /// a client connecting while the screen is idle could wait until the
    /// next damage or GOP-cycle keyframe before seeing anything decodable.
    encoder_control_tx: mpsc::Sender<EncoderControl>,
    /// Forces the capture loop to render+encode a frame right away for a
    /// newly connected client to ride on, rather than waiting on damage or
    /// the periodic keyframe cadence.
    force_render: Arc<AtomicBool>,
}

impl SignalingState {
    pub fn new(
        resize_tx: mpsc::Sender<(u32, u32)>,
        touch_tx: mpsc::Sender<TouchEvent>,
        mouse_tx: mpsc::Sender<MouseEvent>,
        latency_tx: Option<mpsc::Sender<LatencyReport>>,
        bitrate_event_tx: Option<mpsc::Sender<BitrateEvent>>,
        encoder_control_tx: mpsc::Sender<EncoderControl>,
        force_render: Arc<AtomicBool>,
    ) -> Self {
        let (video_tx, _) = broadcast::channel(3);
        Self {
            resize_tx,
            touch_tx,
            mouse_tx,
            latency_tx,
            bitrate_event_tx,
            video_tx,
            encoder_control_tx,
            force_render,
        }
    }

    /// Cloneable sender for feeding encoded video packets in from the
    /// encoder forwarding task; every `/stream` client subscribes to the
    /// same underlying broadcast channel.
    pub fn get_video_sender(&self) -> broadcast::Sender<EncodedPacket> {
        self.video_tx.clone()
    }
}

pub struct SignalingServer {
    router: Router,
}

impl SignalingServer {
    pub fn new(state: SignalingState) -> Self {
        let router = Router::new()
            .route("/", get(serve_client))
            .route("/ws", get(handle_websocket))
            .route("/stream", get(handle_video_stream))
            .layer(TraceLayer::new_for_http())
            .with_state(state);

        Self { router }
    }

    pub async fn serve(self, listen_addr: &str, port: u16) -> Result<()> {
        let addr = format!("{}:{}", listen_addr, port);
        info!("Starting signaling server on {}", addr);

        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .context("Failed to bind signaling server")?;

        axum::serve(listener, self.router)
            .await
            .context("Signaling server error")
    }
}

/// Serve the HTML/JS client
async fn serve_client() -> Html<&'static str> {
    Html(CLIENT_HTML)
}

/// Handle the control WebSocket (`/ws`): touch/pointer/resize/latency and
/// keyframe-resync requests.
async fn handle_websocket(
    ws: WebSocketUpgrade,
    State(state): State<SignalingState>,
) -> Response {
    ws.on_upgrade(move |socket| websocket_handler(socket, state))
}

async fn websocket_handler(socket: WebSocket, state: SignalingState) {
    let (_sender, mut receiver) = socket.split();

    while let Some(Ok(msg)) = receiver.next().await {
        if let Message::Text(text) = msg {
            if let Ok(msg) = serde_json::from_str::<SignalingMessage>(&text) {
                match msg {
                    SignalingMessage::Ready => {
                        info!("Client is ready");
                    }
                    SignalingMessage::Resize { width, height } => {
                        info!("Received resize request from client: {}x{}", width, height);
                        let _ = state.resize_tx.send((width, height)).await;
                    }
                    SignalingMessage::Touch { event } => {
                        // Touch events can be frequent, so only log at debug level
                        if let Err(e) = state.touch_tx.send(event).await {
                            warn!("Failed to send touch event: {}", e);
                        }
                    }
                    SignalingMessage::Pointer { event } => {
                        // Pointer events can be frequent, so only log at debug level
                        if let Err(e) = state.mouse_tx.send(event).await {
                            warn!("Failed to send pointer event: {}", e);
                        }
                    }
                    SignalingMessage::RequestKeyframe => {
                        info!("Client requested a keyframe resync (decoder fell behind)");
                        state.force_render.store(true, Ordering::Relaxed);
                        if let Err(e) = state.encoder_control_tx.send(EncoderControl::ForceKeyframe).await {
                            warn!("Failed to request keyframe resync: {}", e);
                        }
                        if let Some(ref bitrate_event_tx) = state.bitrate_event_tx {
                            let _ = bitrate_event_tx.send(BitrateEvent::KeyframeRequested).await;
                        }
                    }
                    SignalingMessage::Latency { encoding_ms, network_ms, jitter_buffer_ms, decoding_ms, total_ms } => {
                        info!("Received latency message from client: {:.1}ms total", total_ms);
                        if let Some(ref bitrate_event_tx) = state.bitrate_event_tx {
                            let _ = bitrate_event_tx.send(BitrateEvent::Latency(total_ms)).await;
                        }
                        if let Some(ref latency_tx) = state.latency_tx {
                            let mut report = LatencyReport::new();
                            report.encoding_ms = encoding_ms;
                            report.network_ms = network_ms;
                            report.jitter_buffer_ms = jitter_buffer_ms;
                            report.decoding_ms = decoding_ms;
                            report.total_ms = total_ms;

                            if let Err(e) = latency_tx.send(report).await {
                                warn!("Failed to send latency report: {}", e);
                            } else {
                                info!("Latency report forwarded to handler");
                            }
                        } else {
                            warn!("Received latency report but latency_tx is None");
                        }
                    }
                }
            }
        }
    }
}

/// Handle the binary video WebSocket (`/stream`). Each connected client gets
/// its own subscription to the shared `video_tx` broadcast channel and a
/// dedicated send loop; one WebSocket message per encoded frame, no RTP/SDP
/// involved.
async fn handle_video_stream(
    ws: WebSocketUpgrade,
    State(state): State<SignalingState>,
) -> Response {
    ws.on_upgrade(move |socket| video_stream_handler(socket, state))
}

/// Wire format for each binary frame sent to the client:
/// ```text
/// byte 0    : frame_type (0 = delta, 1 = key)
/// bytes 1-4 : frame_id (u32, big-endian)
/// bytes 5.. : raw Annex-B H.264 for the whole frame
/// ```
fn encode_video_frame(packet: &EncodedPacket) -> Vec<u8> {
    let mut buf = Vec::with_capacity(5 + packet.data.len());
    buf.push(packet.is_keyframe as u8);
    buf.extend_from_slice(&packet.frame_id.to_be_bytes());
    buf.extend_from_slice(&packet.data);
    buf
}

async fn video_stream_handler(socket: WebSocket, state: SignalingState) {
    info!("Video stream client connected");
    let (mut sender, _receiver) = socket.split();
    let mut video_rx = state.get_video_sender().subscribe();

    // Request a fresh keyframe and force a render right away -- otherwise
    // this client has no decodable frame to start from until the screen
    // happens to change or the next GOP-cycle keyframe comes around.
    state.force_render.store(true, Ordering::Relaxed);
    if let Err(e) = state.encoder_control_tx.send(EncoderControl::ForceKeyframe).await {
        warn!("Failed to request keyframe for new video stream client: {}", e);
    }

    loop {
        let packet = match video_rx.recv().await {
            Ok(packet) => packet,
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                warn!("Video stream client lagging, skipped {} frame(s)", skipped);
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => break,
        };

        let frame = encode_video_frame(&packet);
        if sender.send(Message::Binary(frame)).await.is_err() {
            break;
        }
    }

    info!("Video stream client disconnected");
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    async fn ws_handshake(addr: &str, path: &str) -> TcpStream {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let request = format!(
            "GET {path} HTTP/1.1\r\n\
             Host: {addr}\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).await.unwrap();

        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            stream.read_exact(&mut byte).await.unwrap();
            buf.push(byte[0]);
            if buf.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let headers = String::from_utf8_lossy(&buf);
        assert!(headers.starts_with("HTTP/1.1 101"), "handshake failed: {}", headers);
        stream
    }

    async fn read_ws_binary_frame(stream: &mut TcpStream) -> Vec<u8> {
        let mut header = [0u8; 2];
        stream.read_exact(&mut header).await.unwrap();
        assert_eq!(header[0] & 0x0F, 2, "expected a binary frame");
        assert_eq!(header[1] & 0x80, 0, "server frames must not be masked");

        let len = match header[1] & 0x7F {
            126 => {
                let mut ext = [0u8; 2];
                stream.read_exact(&mut ext).await.unwrap();
                u16::from_be_bytes(ext) as usize
            }
            127 => {
                let mut ext = [0u8; 8];
                stream.read_exact(&mut ext).await.unwrap();
                u64::from_be_bytes(ext) as usize
            }
            n => n as usize,
        };

        let mut payload = vec![0u8; len];
        stream.read_exact(&mut payload).await.unwrap();
        payload
    }

    #[tokio::test]
    async fn stream_endpoint_delivers_frames_in_wire_format() {
        let (resize_tx, _resize_rx) = mpsc::channel(4);
        let (touch_tx, _touch_rx) = mpsc::channel(4);
        let (mouse_tx, _mouse_rx) = mpsc::channel(4);
        let (encoder_control_tx, mut encoder_control_rx) = mpsc::channel(4);
        let force_render = Arc::new(AtomicBool::new(false));

        let state = SignalingState::new(
            resize_tx,
            touch_tx,
            mouse_tx,
            None,
            None,
            encoder_control_tx,
            force_render.clone(),
        );
        let video_tx = state.get_video_sender();
        let server = SignalingServer::new(state);

        let addr = "127.0.0.1:27345";
        tokio::spawn(async move {
            server.serve("127.0.0.1", 27345).await.unwrap();
        });

        // Give the server a moment to start accepting connections, and the
        // handler a moment to subscribe -- the broadcast channel has no
        // replay for late subscribers.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let mut stream = ws_handshake(addr, "/stream").await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert!(force_render.load(Ordering::Relaxed), "connecting should force a render");
        assert!(matches!(
            encoder_control_rx.try_recv(),
            Ok(EncoderControl::ForceKeyframe)
        ), "connecting should request a fresh keyframe");

        assert!(video_tx
            .send(EncodedPacket {
                data: vec![0xAA, 0xBB, 0xCC],
                is_keyframe: true,
                frame_id: 42,
            })
            .is_ok());
        assert!(video_tx
            .send(EncodedPacket {
                data: vec![0xDD],
                is_keyframe: false,
                frame_id: 43,
            })
            .is_ok());

        let frame1 = read_ws_binary_frame(&mut stream).await;
        assert_eq!(frame1[0], 1, "expected keyframe flag");
        assert_eq!(u32::from_be_bytes([frame1[1], frame1[2], frame1[3], frame1[4]]), 42);
        assert_eq!(&frame1[5..], &[0xAA, 0xBB, 0xCC]);

        let frame2 = read_ws_binary_frame(&mut stream).await;
        assert_eq!(frame2[0], 0, "expected delta flag");
        assert_eq!(u32::from_be_bytes([frame2[1], frame2[2], frame2[3], frame2[4]]), 43);
        assert_eq!(&frame2[5..], &[0xDD]);
    }
}
