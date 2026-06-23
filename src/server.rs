// HTTP/WebSocket server: control channel (input/resize/latency) and the
// binary video stream consumed by the browser's WebCodecs decoder.

use anyhow::{Context, Result};
use axum::{
    extract::{
        ws::{close_code, CloseFrame, Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::Response,
    routing::get,
    Router,
};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, watch};
use tower_http::trace::TraceLayer;
use tracing::{info, warn};

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::adaptive_bitrate::BitrateEvent;
use crate::encoder::{EncodedPacket, EncoderControl};
use crate::input::keyboard::KeyboardEvent;
use crate::input::mouse::MouseEvent;
use crate::input::touch::TouchEvent;
use crate::latency::LatencyReport;
use crate::web::{serve_asset, serve_index};

/// Number of within-~3ms frame arrivals (see
/// `SignalingMessage::Latency::burst_count`) in a single 5s reporting
/// window above which we treat the stream as network-congested. Burst
/// count, not arrival-gap latency, specifically because an idle screen only
/// produces a frame every `keyframe_interval` ticks (nothing to capture
/// otherwise) -- a long gap there is expected silence, not a stall, so a
/// gap-based threshold false-positives on every idle period. A burst can
/// only happen if frames actually piled up somewhere in transit and were
/// released together; idle periods have nothing queued to release, so this
/// is robust to them. A handful of incidental near-simultaneous arrivals
/// can happen by chance, hence a small floor rather than firing on >=1.
const ARRIVAL_STALL_BURST_THRESHOLD: u32 = 5;

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
    #[serde(rename = "key")]
    Key {
        #[serde(flatten)]
        event: KeyboardEvent,
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
        /// Count of `/stream` frame arrivals within ~3ms of the previous
        /// one this window (see `VideoStream.flushDiagnostics` in
        /// web/src/lib/stream.ts) -- several frames landing almost
        /// simultaneously, which only happens if they piled up somewhere in
        /// transit and got released together. Network-level congestion the
        /// decode-queue-depth signal (`RequestKeyframe`, below) can't see,
        /// since the decoder drains a burst faster than its queue can back
        /// up.
        #[serde(default)]
        burst_count: u32,
    },
    /// Round-trip latency probe: echoed back on whichever `/stream` frame
    /// next leaves the encoder (see `encode_video_frame`'s `ping_echo_*`
    /// handling), so the client can measure full pipeline latency using
    /// only its own clock.
    #[serde(rename = "ping")]
    Ping { client_ts: f64 },
}

/// Messages the server pushes to the client over `/ws`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ServerMessage {
    #[serde(rename = "bitrate")]
    Bitrate { bps: usize },
    /// WebCodecs codec string (profile/level) for the client's
    /// `VideoDecoder.configure()`. Pushed on connect and again whenever a
    /// resolution change makes the encoder pick a different H.264 level --
    /// see `encoder::h264_codec_string`.
    #[serde(rename = "codec")]
    Codec { codec: String },
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
    /// Channel to send keyboard events from clients
    key_tx: mpsc::Sender<KeyboardEvent>,
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
    /// Forwards a client's `ping` to the encoder packet-forwarding loop in
    /// main.rs, which stamps it onto the next outgoing `EncodedPacket` as
    /// `ping_echo_client_ts`. Small queue: pings arrive far slower than
    /// frames are forwarded, so this never needs to hold more than one.
    pending_ping_tx: mpsc::Sender<f64>,
    /// Current encoder target bitrate, updated by the adaptive bitrate
    /// controller (or fixed forever in constant-bitrate/CRF mode). Each
    /// `/ws` connection gets its own clone to push `ServerMessage::Bitrate`
    /// updates to that client.
    bitrate_rx: watch::Receiver<usize>,
    /// Current WebCodecs codec string, updated by the encoder thread when a
    /// resolution change picks a different H.264 level. Each `/ws`
    /// connection gets its own clone to push `ServerMessage::Codec` updates.
    codec_rx: watch::Receiver<String>,
    /// Flips to `true` when the process is shutting down. Each connection
    /// handler clones this and races it against its normal work so it can
    /// send a proper WebSocket close frame and return -- letting
    /// `axum::serve`'s graceful shutdown actually complete -- instead of
    /// only ending when the client happens to disconnect on its own.
    shutdown_rx: watch::Receiver<bool>,
}

impl SignalingState {
    pub fn new(
        resize_tx: mpsc::Sender<(u32, u32)>,
        touch_tx: mpsc::Sender<TouchEvent>,
        mouse_tx: mpsc::Sender<MouseEvent>,
        key_tx: mpsc::Sender<KeyboardEvent>,
        latency_tx: Option<mpsc::Sender<LatencyReport>>,
        bitrate_event_tx: Option<mpsc::Sender<BitrateEvent>>,
        encoder_control_tx: mpsc::Sender<EncoderControl>,
        force_render: Arc<AtomicBool>,
        pending_ping_tx: mpsc::Sender<f64>,
        bitrate_rx: watch::Receiver<usize>,
        codec_rx: watch::Receiver<String>,
        shutdown_rx: watch::Receiver<bool>,
    ) -> Self {
        let (video_tx, _) = broadcast::channel(3);
        Self {
            resize_tx,
            touch_tx,
            mouse_tx,
            key_tx,
            latency_tx,
            bitrate_event_tx,
            video_tx,
            encoder_control_tx,
            force_render,
            pending_ping_tx,
            bitrate_rx,
            codec_rx,
            shutdown_rx,
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
            .route("/", get(serve_index))
            .route("/ws", get(handle_websocket))
            .route("/stream", get(handle_video_stream))
            .fallback(serve_asset)
            .layer(TraceLayer::new_for_http())
            .with_state(state);

        Self { router }
    }

    /// Serves until `shutdown` resolves, at which point the listener stops
    /// accepting new connections and this only returns once every
    /// in-flight handler has returned -- each of which races its own work
    /// against the same shutdown signal (via `SignalingState::shutdown_rx`)
    /// so that actually happens promptly instead of waiting on clients to
    /// disconnect on their own.
    pub async fn serve(
        self,
        listen_addr: &str,
        port: u16,
        shutdown: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> Result<()> {
        let addr = format!("{}:{}", listen_addr, port);
        info!("Starting signaling server on {}", addr);

        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .context("Failed to bind signaling server")?;

        axum::serve(listener, self.router)
            .with_graceful_shutdown(shutdown)
            .await
            .context("Signaling server error")
    }
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
    let (mut sender, mut receiver) = socket.split();
    let mut bitrate_rx = state.bitrate_rx.clone();
    let mut codec_rx = state.codec_rx.clone();
    let mut shutdown_rx = state.shutdown_rx.clone();

    // Push the current bitrate right away -- otherwise a client connecting
    // between adaptive-bitrate adjustments would see nothing until the next
    // change, which on a settled stream might be a long time off.
    let initial_bitrate = *bitrate_rx.borrow();
    if send_server_message(&mut sender, &ServerMessage::Bitrate { bps: initial_bitrate }).await.is_err() {
        return;
    }

    // Same idea for the codec string: a client connecting after the
    // encoder has already settled on a non-default level (e.g. after a
    // resolution change before this client connected) needs that level up
    // front, not just on the next change.
    let initial_codec = codec_rx.borrow().clone();
    if send_server_message(&mut sender, &ServerMessage::Codec { codec: initial_codec }).await.is_err() {
        return;
    }

    loop {
        tokio::select! {
            incoming = receiver.next() => {
                let Some(Ok(msg)) = incoming else { break; };
                let Message::Text(text) = msg else { continue; };
                let Ok(signal) = serde_json::from_str::<SignalingMessage>(&text) else { continue; };
                match signal {
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
                    SignalingMessage::Key { event } => {
                        if let Err(e) = state.key_tx.send(event).await {
                            warn!("Failed to send key event: {}", e);
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
                    SignalingMessage::Ping { client_ts } => {
                        // Best-effort: if the queue is briefly full, the next
                        // ping a couple seconds later picks it up instead.
                        let _ = state.pending_ping_tx.try_send(client_ts);
                    }
                    SignalingMessage::Latency { encoding_ms, network_ms, jitter_buffer_ms, decoding_ms, total_ms, burst_count } => {
                        info!(
                            "Received latency report from client: network {:.1}ms decode {:.1}ms total {:.1}ms",
                            network_ms.unwrap_or(0.0), decoding_ms.unwrap_or(0.0), total_ms
                        );
                        // Only decode latency throttles bitrate growth here --
                        // network/RTT delays aren't evidence the encoder's
                        // rate is too high (see adaptive_bitrate.rs).
                        if let (Some(ref bitrate_event_tx), Some(ms)) = (&state.bitrate_event_tx, decoding_ms) {
                            let _ = bitrate_event_tx.send(BitrateEvent::Latency(ms)).await;
                        }
                        // Bursty arrival with a shallow decode queue is
                        // network-level congestion the keyframe-request
                        // signal can't see -- cut on it directly. See
                        // `BitrateEvent::ArrivalStall`.
                        if burst_count >= ARRIVAL_STALL_BURST_THRESHOLD {
                            warn!(
                                "Client reported {} bursty frame arrivals (>= {}) with no decode backlog -- treating as network congestion",
                                burst_count, ARRIVAL_STALL_BURST_THRESHOLD
                            );
                            if let Some(ref bitrate_event_tx) = state.bitrate_event_tx {
                                let _ = bitrate_event_tx.send(BitrateEvent::ArrivalStall).await;
                            }
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
                            }
                        }
                    }
                }
            }
            changed = bitrate_rx.changed() => {
                if changed.is_err() {
                    // Sender side dropped; bitrate just won't update further.
                    continue;
                }
                let bps = *bitrate_rx.borrow();
                if send_server_message(&mut sender, &ServerMessage::Bitrate { bps }).await.is_err() {
                    break;
                }
            }
            changed = codec_rx.changed() => {
                if changed.is_err() {
                    // Sender side (encoder thread) dropped; codec just won't update further.
                    continue;
                }
                let codec = codec_rx.borrow().clone();
                if send_server_message(&mut sender, &ServerMessage::Codec { codec }).await.is_err() {
                    break;
                }
            }
            _ = shutdown_rx.changed() => {
                let _ = sender.send(Message::Close(Some(CloseFrame {
                    code: close_code::AWAY,
                    reason: "server shutting down".into(),
                }))).await;
                break;
            }
        }
    }
}

async fn send_server_message(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    msg: &ServerMessage,
) -> Result<(), axum::Error> {
    sender
        .send(Message::Text(serde_json::to_string(msg).expect("ServerMessage always serializes")))
        .await
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
/// byte 0     : frame_type (0 = delta, 1 = key)
/// bytes 1-4  : frame_id (u32, big-endian)
/// byte 5     : has_ping_echo (0 or 1)
/// bytes 6-13 : ping_echo_client_ts (f64, big-endian; valid only if byte 5 == 1)
/// bytes 14.. : raw Annex-B H.264 for the whole frame
/// ```
/// The ping echo round-trips a client's `ping` (`SignalingMessage::Ping`)
/// back on whichever frame next leaves the encoder, so the client can
/// measure full pipeline latency (its own clock only, no sync needed) --
/// see `VideoStream` in web/src/lib/stream.ts.
const STREAM_FRAME_HEADER_BYTES: usize = 14;

fn encode_video_frame(packet: &EncodedPacket) -> Vec<u8> {
    let mut buf = Vec::with_capacity(STREAM_FRAME_HEADER_BYTES + packet.data.len());
    buf.push(packet.is_keyframe as u8);
    buf.extend_from_slice(&packet.frame_id.to_be_bytes());
    match packet.ping_echo_client_ts {
        Some(ts) => {
            buf.push(1);
            buf.extend_from_slice(&ts.to_be_bytes());
        }
        None => {
            buf.push(0);
            buf.extend_from_slice(&0f64.to_be_bytes());
        }
    }
    buf.extend_from_slice(&packet.data);
    buf
}

async fn video_stream_handler(socket: WebSocket, state: SignalingState) {
    info!("Video stream client connected");
    let (mut sender, _receiver) = socket.split();
    let mut video_rx = state.get_video_sender().subscribe();
    let mut shutdown_rx = state.shutdown_rx.clone();

    // Request a fresh keyframe and force a render right away -- otherwise
    // this client has no decodable frame to start from until the screen
    // happens to change or the next GOP-cycle keyframe comes around.
    state.force_render.store(true, Ordering::Relaxed);
    if let Err(e) = state.encoder_control_tx.send(EncoderControl::ForceKeyframe).await {
        warn!("Failed to request keyframe for new video stream client: {}", e);
    }

    loop {
        tokio::select! {
            packet = video_rx.recv() => {
                let packet = match packet {
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
            _ = shutdown_rx.changed() => {
                let _ = sender.send(Message::Close(Some(CloseFrame {
                    code: close_code::AWAY,
                    reason: "server shutting down".into(),
                }))).await;
                break;
            }
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
        let (key_tx, _key_rx) = mpsc::channel(4);
        let (encoder_control_tx, mut encoder_control_rx) = mpsc::channel(4);
        let (pending_ping_tx, _pending_ping_rx) = mpsc::channel(4);
        let (_bitrate_tx, bitrate_rx) = watch::channel(2_000_000usize);
        let (_codec_tx, codec_rx) = watch::channel(String::new());
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let force_render = Arc::new(AtomicBool::new(false));

        let state = SignalingState::new(
            resize_tx,
            touch_tx,
            mouse_tx,
            key_tx,
            None,
            None,
            encoder_control_tx,
            force_render.clone(),
            pending_ping_tx,
            bitrate_rx,
            codec_rx,
            shutdown_rx,
        );
        let video_tx = state.get_video_sender();
        let server = SignalingServer::new(state);

        let addr = "127.0.0.1:27345";
        tokio::spawn(async move {
            server.serve("127.0.0.1", 27345, std::future::pending()).await.unwrap();
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

        fn test_packet(data: Vec<u8>, is_keyframe: bool, frame_id: u32) -> EncodedPacket {
            EncodedPacket {
                data,
                is_keyframe,
                frame_id,
                capture_to_encode_ms: 0.0,
                encoding_ms: 0.0,
                encode_complete: std::time::Instant::now(),
                ping_echo_client_ts: None,
            }
        }

        assert!(video_tx.send(test_packet(vec![0xAA, 0xBB, 0xCC], true, 42)).is_ok());
        assert!(video_tx.send(test_packet(vec![0xDD], false, 43)).is_ok());

        let frame1 = read_ws_binary_frame(&mut stream).await;
        assert_eq!(frame1[0], 1, "expected keyframe flag");
        assert_eq!(u32::from_be_bytes([frame1[1], frame1[2], frame1[3], frame1[4]]), 42);
        assert_eq!(frame1[5], 0, "expected no ping echo on this frame");
        assert_eq!(&frame1[STREAM_FRAME_HEADER_BYTES..], &[0xAA, 0xBB, 0xCC]);

        let frame2 = read_ws_binary_frame(&mut stream).await;
        assert_eq!(frame2[0], 0, "expected delta flag");
        assert_eq!(u32::from_be_bytes([frame2[1], frame2[2], frame2[3], frame2[4]]), 43);
        assert_eq!(&frame2[STREAM_FRAME_HEADER_BYTES..], &[0xDD]);
    }
}
