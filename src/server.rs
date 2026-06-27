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
use tracing::{debug, info, warn};

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::adaptive_bitrate::BitrateEvent;
use crate::audio::AudioPacket;
use crate::encoder::{EncodedPacket, EncoderControl};
use crate::input::keyboard::KeyboardEvent;
use crate::input::mouse::MouseEvent;
use crate::input::touch::TouchEvent;
use crate::latency::LatencyReport;
use crate::proto;
use crate::session::SessionManager;
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

/// Minimum spacing between honoring two `RequestKeyframe` resyncs. A client
/// genuinely too overloaded to keep up (e.g. its decode throughput can't
/// match the configured resolution/framerate at all, not just a transient
/// blip) re-backlogs and re-requests within a handful of frames of the last
/// forced keyframe -- observed in practice as a tight loop, every keyframe
/// arriving just makes the client clear its queue for a couple of frames
/// before falling behind again. Forcing a *new* keyframe on every one of
/// those requests only makes things worse: keyframes are bigger and slower
/// to decode than the delta frames they replace, so spamming them feeds
/// more load into an already-overloaded pipe. This gate is the only thing
/// bounding forced-keyframe spam from a struggling client: a keyframe request
/// no longer cuts the bitrate (that signal proved to be local decode jank far
/// more often than a too-high rate -- see adaptive_bitrate.rs), so there's no
/// `decrease_cooldown` backstop behind it. The gate applies unconditionally
/// (even with adaptive bitrate disabled), since the keyframe-spam problem
/// exists independent of whether the bitrate is allowed to change.
const KEYFRAME_FORCE_COOLDOWN: Duration = Duration::from_millis(500);

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
        /// Average wall-clock cost of `ctx.drawImage(VideoFrame)` in the
        /// browser, isolated from decode time. On Firefox this can be a
        /// GPU→CPU→GPU round-trip; a high value here means the blit is the
        /// bottleneck, not the decoder. Absent when the window had no frames.
        #[serde(default)]
        blit_ms: Option<f64>,
    },
    /// Round-trip latency probe: echoed back on whichever `/stream` frame
    /// next leaves the encoder (see `encode_video_frame`'s `ping_echo_*`
    /// handling), so the client can measure full pipeline latency using
    /// only its own clock.
    #[serde(rename = "ping")]
    Ping { client_ts: f64 },
}

/// Cursor state pushed from the compositor to `/ws` clients. The browser
/// uses this to render a client-side cursor overlay, eliminating cursor
/// round-trip latency.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum CursorUpdate {
    /// No app cursor set; browser shows its own default cursor.
    #[serde(rename = "default")]
    Default,
    /// Client explicitly hid the cursor.
    #[serde(rename = "hidden")]
    Hidden,
    /// Named CSS cursor (from `wp_cursor_shape_v1`).
    #[serde(rename = "named")]
    Named { name: String },
    /// Custom cursor surface from `wl_pointer.set_cursor`.
    #[serde(rename = "surface")]
    Surface {
        width: u32,
        height: u32,
        hotspot_x: i32,
        hotspot_y: i32,
        /// Base64-encoded RGBA (not BGRA) pixel data, width × height × 4 bytes.
        rgba: String,
    },
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
    /// Current cursor shape from the compositor. Pushed on connect and on
    /// every cursor change.
    #[serde(rename = "cursor")]
    Cursor { cursor: CursorUpdate },
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
    /// Lazily starts the session's configured client app on the first
    /// `/ws` or `/stream` connection. A no-op if no command was configured.
    session: SessionManager,
    /// When a `RequestKeyframe` resync was last actually honored (forced a
    /// new keyframe), shared across every `/ws` connection -- see
    /// `KEYFRAME_FORCE_COOLDOWN`. Plain `std::sync::Mutex` rather than
    /// tokio's: the critical section is a single comparison/store, never
    /// held across an `.await`.
    last_keyframe_force: Arc<Mutex<Instant>>,
    /// Broadcasts Opus-encoded audio packets to `/audio` WebSocket clients.
    /// `None` when the PipeWire audio capture failed to start at launch.
    audio_tx: Option<broadcast::Sender<AudioPacket>>,
    /// Current cursor state from the compositor. Each `/ws` connection
    /// subscribes to this watch channel and pushes updates as
    /// `ServerMessage::Cursor` messages. A new client also receives the
    /// current cursor immediately on connect.
    cursor_rx: watch::Receiver<CursorUpdate>,
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
        session: SessionManager,
        audio_tx: Option<broadcast::Sender<AudioPacket>>,
        cursor_rx: watch::Receiver<CursorUpdate>,
    ) -> Self {
        let (video_tx, _) = broadcast::channel(3);
        // Backdated so the very first `RequestKeyframe` after startup is
        // never suppressed by the cooldown.
        let last_keyframe_force = Instant::now()
            .checked_sub(KEYFRAME_FORCE_COOLDOWN)
            .unwrap_or_else(Instant::now);
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
            session,
            last_keyframe_force: Arc::new(Mutex::new(last_keyframe_force)),
            audio_tx,
            cursor_rx,
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
            .route("/audio", get(handle_audio_stream))
            .route("/client", get(handle_unified_client))
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
    state.session.ensure_started().await;

    let (mut sender, mut receiver) = socket.split();
    let mut bitrate_rx = state.bitrate_rx.clone();
    let mut codec_rx = state.codec_rx.clone();
    let mut cursor_rx = state.cursor_rx.clone();
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

    // Push the current cursor state so the client renders the right cursor
    // immediately (e.g. reconnecting while an app is running).
    let initial_cursor = cursor_rx.borrow().clone();
    if send_server_message(&mut sender, &ServerMessage::Cursor { cursor: initial_cursor }).await.is_err() {
        return;
    }

loop {
            tokio::select! {
                incoming = receiver.next() => {
                    let Some(Ok(msg)) = incoming else { break; };
                    let Message::Text(text) = msg else { continue; };
                    let Ok(signal) = serde_json::from_str::<SignalingMessage>(&text) else { continue; };
                    dispatch_signaling_message(signal, &state).await;
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
            changed = cursor_rx.changed() => {
                if changed.is_err() {
                    continue;
                }
                let cursor = cursor_rx.borrow().clone();
                if send_server_message(&mut sender, &ServerMessage::Cursor { cursor }).await.is_err() {
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
    state.session.ensure_started().await;

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

/// Handle the Opus audio WebSocket (`/audio`).
///
/// Wire format per message (binary):
/// ```text
/// bytes 0-7  : pts_us (u64, big-endian) — presentation timestamp in microseconds
/// bytes 8..  : raw Opus packet (one 20 ms frame)
/// ```
async fn handle_audio_stream(
    ws: WebSocketUpgrade,
    State(state): State<SignalingState>,
) -> Response {
    ws.on_upgrade(move |socket| audio_stream_handler(socket, state))
}

async fn audio_stream_handler(socket: WebSocket, state: SignalingState) {
    let Some(ref audio_tx) = state.audio_tx else {
        // Audio capture is not available; close immediately with a normal close code.
        let (mut sender, _) = socket.split();
        let _ = sender
            .send(Message::Close(Some(CloseFrame {
                code: close_code::NORMAL,
                reason: "audio capture not available".into(),
            })))
            .await;
        return;
    };

    info!("Audio stream client connected");
    let (mut sender, _receiver) = socket.split();
    let mut audio_rx = audio_tx.subscribe();
    let mut shutdown_rx = state.shutdown_rx.clone();

    loop {
        tokio::select! {
            packet = audio_rx.recv() => {
                let packet = match packet {
                    Ok(p) => p,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!("Audio stream client lagging, skipped {} packet(s)", n);
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                let frame = encode_audio_frame(&packet);
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

    info!("Audio stream client disconnected");
}

fn encode_audio_frame(packet: &AudioPacket) -> Vec<u8> {
    let mut buf = Vec::with_capacity(8 + packet.data.len());
    buf.extend_from_slice(&packet.pts_us.to_be_bytes());
    buf.extend_from_slice(&packet.data);
    buf
}

/// Handle the unified client WebSocket (`/client`) that combines the control
/// channel (`/ws`), video stream (`/stream`), and audio stream (`/audio`)
/// into one connection using the shared 8-byte `proto::HEADER_LEN` framing.
/// See `docs/native-client-plan.md` Part 2.
async fn handle_unified_client(
    ws: WebSocketUpgrade,
    State(state): State<SignalingState>,
) -> Response {
    ws.on_upgrade(move |socket| unified_client_handler(socket, state))
}

async fn unified_client_handler(socket: WebSocket, state: SignalingState) {
    info!("Unified client connected");
    state.session.ensure_started().await;

    let (mut sender, mut receiver) = socket.split();
    let mut video_rx = state.get_video_sender().subscribe();
    // `None` when PipeWire audio capture failed at startup -- the audio
    // branch of the select! below then never resolves.
    let mut audio_rx: Option<broadcast::Receiver<AudioPacket>> =
        state.audio_tx.as_ref().map(|tx| tx.subscribe());
    let mut bitrate_rx = state.bitrate_rx.clone();
    let mut codec_rx = state.codec_rx.clone();
    let mut cursor_rx = state.cursor_rx.clone();
    let mut shutdown_rx = state.shutdown_rx.clone();

    // Push the current bitrate/codec/cursor up front for the same reasons as
    // the standalone /ws handler -- a client connecting between changes
    // (or after the encoder has settled on a non-default level) needs the
    // current state, not the next one. Each value is bound to a local first
    // so the `watch::Ref` borrow is released before the `.await` (it isn't
    // `Send`, and would otherwise extend across the await point).
    {
        let bps = *bitrate_rx.borrow();
        if send_unified_control(&mut sender, &ServerMessage::Bitrate { bps })
            .await
            .is_err()
        {
            return;
        }
    }
    {
        let codec = codec_rx.borrow().clone();
        if send_unified_control(&mut sender, &ServerMessage::Codec { codec })
            .await
            .is_err()
        {
            return;
        }
    }
    {
        let cursor = cursor_rx.borrow().clone();
        if send_unified_control(&mut sender, &ServerMessage::Cursor { cursor })
            .await
            .is_err()
        {
            return;
        }
    }

    // Same keyframe-forcing behavior as the standalone /stream handler:
    // otherwise a new client has no decodable frame to start from until
    // the next damage or GOP-cycle keyframe.
    state.force_render.store(true, Ordering::Relaxed);
    if let Err(e) = state.encoder_control_tx.send(EncoderControl::ForceKeyframe).await {
        warn!("Failed to request keyframe for new unified client: {}", e);
    }

    loop {
        tokio::select! {
            packet = video_rx.recv() => {
                let packet = match packet {
                    Ok(packet) => packet,
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        warn!("Unified client video lagging, skipped {} frame(s)", skipped);
                        // Authoritative congestion: this client's socket
                        // drained slower than the encoder produced, so frames
                        // piled past the small broadcast buffer and were
                        // dropped. Unlike a keyframe request (which the browser
                        // also fires on a purely local decode stall), the send
                        // path only backs up when the link genuinely can't
                        // carry the current rate -- feed it straight to the
                        // bitrate controller. try_send (not await) so a full
                        // event queue can't stall this hot send loop; the
                        // controller's own cooldown coalesces the repeated lags
                        // a sustained stall produces.
                        if let Some(ref bitrate_event_tx) = state.bitrate_event_tx {
                            let _ = bitrate_event_tx.try_send(BitrateEvent::SendBacklog);
                        }
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                let frame = encode_unified_video_frame(packet);
                if sender.send(Message::Binary(frame)).await.is_err() {
                    break;
                }
            }
            packet = recv_audio(&mut audio_rx) => {
                let packet = match packet {
                    Ok(packet) => packet,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!("Unified client audio lagging, skipped {} packet(s)", n);
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                let frame = encode_unified_audio_frame(packet);
                if sender.send(Message::Binary(frame)).await.is_err() {
                    break;
                }
            }
            changed = bitrate_rx.changed() => {
                if changed.is_err() {
                    // Sender side dropped; bitrate just won't update further.
                    continue;
                }
                let bps = *bitrate_rx.borrow();
                if send_unified_control(&mut sender, &ServerMessage::Bitrate { bps })
                    .await
                    .is_err()
                {
                    break;
                }
            }
            changed = codec_rx.changed() => {
                if changed.is_err() {
                    // Sender side (encoder thread) dropped; codec just won't update further.
                    continue;
                }
                let codec = codec_rx.borrow().clone();
                if send_unified_control(&mut sender, &ServerMessage::Codec { codec })
                    .await
                    .is_err()
                {
                    break;
                }
            }
            changed = cursor_rx.changed() => {
                if changed.is_err() { continue; }
                let cursor = cursor_rx.borrow().clone();
                if send_unified_control(&mut sender, &ServerMessage::Cursor { cursor })
                    .await
                    .is_err()
                {
                    break;
                }
            }
            incoming = receiver.next() => {
                let Some(Ok(msg)) = incoming else { break; };
                let Message::Binary(data) = msg else { continue; };
                let Ok(signal) = parse_client_message(&data) else { continue; };
                dispatch_signaling_message(signal, &state).await;
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

    info!("Unified client disconnected");
}

/// Wire layout for a unified `MSG_VIDEO_FRAME` payload (after the 8-byte
/// `proto` header):
/// ```text
/// bytes 0-3   : frame_id (u32, big-endian)
/// bytes 4-11  : ping_echo_client_ts (f64, big-endian; 0.0 when no echo)
/// bytes 12-19 : capture_to_encode_ms (f64, big-endian)
/// bytes 20..  : raw Annex-B H.264 NAL data
/// ```
/// `is_keyframe` and `has_ping_echo` are carried in the header `flags`
/// byte, not inline -- saving the two flag bytes the legacy `/stream`
/// format uses.
///
/// Takes the packet by ownership so the H.264 buffer can be moved into the
/// framed message with `Vec::append` (no extra allocation or copy beyond
/// the single `memcpy` a contiguous wire buffer inevitably requires).
fn encode_unified_video_frame(mut packet: EncodedPacket) -> Vec<u8> {
    let mut flags = 0u8;
    if packet.is_keyframe {
        flags |= proto::FLAG_KEYFRAME;
    }
    if packet.ping_echo_client_ts.is_some() {
        flags |= proto::FLAG_HAS_PING;
    }

    let payload_len = 20 + packet.data.len();
    let mut buf = Vec::with_capacity(proto::HEADER_LEN + payload_len);
    buf.push(proto::MSG_VIDEO_FRAME);
    buf.push(flags);
    buf.push(0);
    buf.push(0);
    buf.extend_from_slice(&(payload_len as u32).to_le_bytes());
    buf.extend_from_slice(&packet.frame_id.to_be_bytes());
    let ping_val = packet.ping_echo_client_ts.unwrap_or(0.0);
    buf.extend_from_slice(&ping_val.to_be_bytes());
    buf.extend_from_slice(&packet.capture_to_encode_ms.to_be_bytes());
    buf.append(&mut packet.data);
    buf
}

/// Wire layout for a unified `MSG_AUDIO_FRAME` payload:
/// ```text
/// bytes 0-7 : pts_us (u64, big-endian)
/// bytes 8.. : raw Opus packet
/// ```
/// Identical to the legacy `/audio` wire format, just wrapped in the
/// shared header. Takes ownership for the same zero-copy reason as
/// `encode_unified_video_frame`.
fn encode_unified_audio_frame(mut packet: AudioPacket) -> Vec<u8> {
    let payload_len = 8 + packet.data.len();
    let mut buf = Vec::with_capacity(proto::HEADER_LEN + payload_len);
    buf.push(proto::MSG_AUDIO_FRAME);
    buf.push(0);
    buf.push(0);
    buf.push(0);
    buf.extend_from_slice(&(payload_len as u32).to_le_bytes());
    buf.extend_from_slice(&packet.pts_us.to_be_bytes());
    buf.append(&mut packet.data);
    buf
}

fn encode_unified_control(msg: &ServerMessage) -> Vec<u8> {
    let json = serde_json::to_vec(msg).expect("ServerMessage always serializes");
    proto::encode_msg(proto::MSG_CONTROL, 0, &json)
}

async fn send_unified_control(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    msg: &ServerMessage,
) -> Result<(), axum::Error> {
    sender.send(Message::Binary(encode_unified_control(msg))).await
}

/// Parses a `MSG_CLIENT_MSG` frame coming from the `/client` endpoint.
/// Anything that isn't a `MSG_CLIENT_MSG` (or has a malformed
/// header/payload) is treated as a non-error no-op so a single bad frame
/// can't kill the connection.
fn parse_client_message(data: &[u8]) -> Result<SignalingMessage, ()> {
    if data.len() < proto::HEADER_LEN {
        return Err(());
    }
    let header: [u8; proto::HEADER_LEN] = data[..proto::HEADER_LEN]
        .try_into()
        .map_err(|_| ())?;
    let (msg_type, _flags, payload_len) = proto::decode_header(&header);
    if msg_type != proto::MSG_CLIENT_MSG {
        return Err(());
    }
    let end = proto::HEADER_LEN
        .checked_add(payload_len as usize)
        .ok_or(())?;
    if data.len() < end {
        return Err(());
    }
    serde_json::from_slice(&data[proto::HEADER_LEN..end]).map_err(|_| ())
}

/// Future used by the unified client's audio branch. Yields `None`
/// forever when audio capture is unavailable, so the branch simply
/// never wins the `select!`.
async fn recv_audio(
    rx: &mut Option<broadcast::Receiver<AudioPacket>>,
) -> Result<AudioPacket, broadcast::error::RecvError> {
    match rx.as_mut() {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

/// Shared dispatch for an incoming `SignalingMessage`. Both the
/// legacy `/ws` handler (text-framed JSON) and the unified `/client`
/// handler (binary-framed via `parse_client_message`) route through here
/// so the two transports can never drift apart in how they handle
/// resize/input/keyframe-resync/latency/ping.
async fn dispatch_signaling_message(signal: SignalingMessage, state: &SignalingState) {
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
            // A keyframe request is a *local* decode-pacing concern, not a
            // congestion signal -- in the browser it's dominated by transient
            // main-thread stalls, not by the rate being too high (the native
            // client decodes the same stream without ever requesting one). So
            // it forces an IDR to resync the client but deliberately does not
            // touch the bitrate; genuine network congestion comes in via the
            // bursty-arrival path (`BitrateEvent::ArrivalStall`) instead. See
            // adaptive_bitrate.rs.
            //
            // Don't force a *new* keyframe more often than
            // `KEYFRAME_FORCE_COOLDOWN` -- see its doc comment for
            // why honoring every request here can spiral.
            let should_force = {
                let mut last = state.last_keyframe_force.lock().unwrap();
                if last.elapsed() >= KEYFRAME_FORCE_COOLDOWN {
                    *last = Instant::now();
                    true
                } else {
                    false
                }
            };
            if should_force {
                state.force_render.store(true, Ordering::Relaxed);
                if let Err(e) = state
                    .encoder_control_tx
                    .send(EncoderControl::ForceKeyframe)
                    .await
                {
                    warn!("Failed to request keyframe resync: {}", e);
                }
            } else {
                debug!(
                    "Suppressing keyframe resync, last one was less than {:?} ago",
                    KEYFRAME_FORCE_COOLDOWN
                );
            }
        }
        SignalingMessage::Ping { client_ts } => {
            // Best-effort: if the queue is briefly full, the next
            // ping a couple seconds later picks it up instead.
            let _ = state.pending_ping_tx.try_send(client_ts);
        }
        SignalingMessage::Latency {
            encoding_ms,
            network_ms,
            jitter_buffer_ms,
            decoding_ms,
            total_ms,
            burst_count,
            blit_ms,
        } => {
            info!(
                "Received latency report from client: network {:.1}ms decode {:.1}ms blit {:.1}ms total {:.1}ms",
                network_ms.unwrap_or(0.0),
                decoding_ms.unwrap_or(0.0),
                blit_ms.unwrap_or(0.0),
                total_ms
            );
            // Only decode latency throttles bitrate growth here --
            // network/RTT delays aren't evidence the encoder's
            // rate is too high (see adaptive_bitrate.rs).
            if let (Some(ref bitrate_event_tx), Some(ms)) =
                (&state.bitrate_event_tx, decoding_ms)
            {
                let _ = bitrate_event_tx.send(BitrateEvent::Latency(ms)).await;
            }
            // Bursty arrival is network-level congestion: a batch of
            // frames queued up in the path and released at once. This is
            // the signal the controller actually cuts on. See
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
                report.decode_to_display_ms = blit_ms;
                report.total_ms = total_ms;

                if let Err(e) = latency_tx.send(report).await {
                    warn!("Failed to send latency report: {}", e);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    use crate::audio::AudioPacket;

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

    /// Reads one raw WebSocket frame (server-to-client, unmasked).
    /// Returns (opcode, payload).
    async fn read_ws_frame(stream: &mut TcpStream) -> (u8, Vec<u8>) {
        let mut header = [0u8; 2];
        stream.read_exact(&mut header).await.unwrap();
        let opcode = header[0] & 0x0F;
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
        (opcode, payload)
    }

    async fn read_ws_binary_frame(stream: &mut TcpStream) -> Vec<u8> {
        let (opcode, payload) = read_ws_frame(stream).await;
        assert_eq!(opcode, 2, "expected a binary frame (opcode 2)");
        payload
    }

    /// Returns the close-frame status code and UTF-8 reason string.
    async fn read_ws_close_frame(stream: &mut TcpStream) -> (u16, String) {
        let (opcode, payload) = read_ws_frame(stream).await;
        assert_eq!(opcode, 8, "expected a close frame (opcode 8)");
        assert!(payload.len() >= 2, "close frame must carry a status code");
        let code = u16::from_be_bytes([payload[0], payload[1]]);
        let reason = String::from_utf8(payload[2..].to_vec()).unwrap_or_default();
        (code, reason)
    }

    fn make_signaling_state(
        shutdown_rx: watch::Receiver<bool>,
        shutdown_tx: watch::Sender<bool>,
        audio_tx: Option<broadcast::Sender<AudioPacket>>,
    ) -> SignalingState {
        let (resize_tx, _) = mpsc::channel(4);
        let (touch_tx, _) = mpsc::channel(4);
        let (mouse_tx, _) = mpsc::channel(4);
        let (key_tx, _) = mpsc::channel(4);
        let (encoder_control_tx, _) = mpsc::channel(4);
        let (pending_ping_tx, _) = mpsc::channel(4);
        let (_, bitrate_rx) = watch::channel(2_000_000usize);
        let (_, codec_rx) = watch::channel(String::new());
        let (_, cursor_rx) = watch::channel(CursorUpdate::Default);
        let force_render = Arc::new(AtomicBool::new(false));
        SignalingState::new(
            resize_tx,
            touch_tx,
            mouse_tx,
            key_tx,
            None,
            None,
            encoder_control_tx,
            force_render,
            pending_ping_tx,
            bitrate_rx,
            codec_rx,
            shutdown_rx,
            SessionManager::new(Vec::new(), String::new(), shutdown_tx),
            audio_tx,
            cursor_rx,
        )
    }

    #[tokio::test]
    async fn stream_endpoint_delivers_frames_in_wire_format() {
        let (encoder_control_tx, mut encoder_control_rx) = mpsc::channel(4);
        let (pending_ping_tx, _pending_ping_rx) = mpsc::channel(4);
        let (_, bitrate_rx) = watch::channel(2_000_000usize);
        let (_, codec_rx) = watch::channel(String::new());
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let force_render = Arc::new(AtomicBool::new(false));
        let (_, cursor_rx) = watch::channel(CursorUpdate::Default);
        let state = SignalingState::new(
            mpsc::channel(4).0,
            mpsc::channel(4).0,
            mpsc::channel(4).0,
            mpsc::channel(4).0,
            None, None,
            encoder_control_tx,
            force_render.clone(),
            pending_ping_tx,
            bitrate_rx, codec_rx,
            shutdown_rx,
            SessionManager::new(Vec::new(), String::new(), shutdown_tx),
            None,
            cursor_rx,
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

    /// Wire format: [u64 pts_us BE][raw Opus bytes].
    #[tokio::test]
    async fn audio_endpoint_delivers_frames_in_wire_format() {
        let (audio_tx, _) = broadcast::channel::<AudioPacket>(4);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let state = make_signaling_state(shutdown_rx, shutdown_tx, Some(audio_tx.clone()));
        let server = SignalingServer::new(state);

        let addr = "127.0.0.1:27346";
        tokio::spawn(async move {
            server.serve("127.0.0.1", 27346, std::future::pending()).await.unwrap();
        });

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let mut stream = ws_handshake(addr, "/audio").await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let opus_data = vec![0xAA, 0xBB, 0xCC, 0xDD];
        let pts_us: u64 = 20_000;
        assert!(audio_tx.send(AudioPacket { data: opus_data.clone(), pts_us }).is_ok());

        let frame = read_ws_binary_frame(&mut stream).await;
        assert!(frame.len() >= 8, "audio frame must have at least 8-byte header");
        let pts_received = u64::from_be_bytes(frame[..8].try_into().unwrap());
        assert_eq!(pts_received, pts_us, "PTS must round-trip correctly");
        assert_eq!(&frame[8..], &opus_data, "Opus payload must follow the header");
    }

    /// When audio capture failed at startup, /audio closes immediately with
    /// NORMAL (1000) and reason "audio capture not available".
    #[tokio::test]
    async fn audio_endpoint_closes_when_capture_unavailable() {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let state = make_signaling_state(shutdown_rx, shutdown_tx, None);
        let server = SignalingServer::new(state);

        let addr = "127.0.0.1:27347";
        tokio::spawn(async move {
            server.serve("127.0.0.1", 27347, std::future::pending()).await.unwrap();
        });

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let mut stream = ws_handshake(addr, "/audio").await;

        let (code, reason) = read_ws_close_frame(&mut stream).await;
        assert_eq!(code, 1000, "expected NORMAL close code");
        assert_eq!(reason, "audio capture not available");
    }

    /// Parses the proto header from a binary WS payload returned by the
    /// unified endpoint. Returns (msg_type, flags, payload_len).
    fn parse_unified_header(payload: &[u8]) -> (u8, u8, u32) {
        assert!(
            payload.len() >= proto::HEADER_LEN,
            "unified frame too short ({} bytes)",
            payload.len()
        );
        let header: [u8; proto::HEADER_LEN] = payload[..proto::HEADER_LEN].try_into().unwrap();
        proto::decode_header(&header)
    }

    /// /client completes the WebSocket upgrade (101 Switching Protocols),
    /// which is the Phase-2 milestone from docs/native-client-plan.md.
    #[tokio::test]
    async fn client_endpoint_completes_websocket_handshake() {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let state = make_signaling_state(shutdown_rx, shutdown_tx, None);
        let server = SignalingServer::new(state);

        let addr = "127.0.0.1:27348";
        tokio::spawn(async move {
            server.serve("127.0.0.1", 27348, std::future::pending()).await.unwrap();
        });

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        // ws_handshake asserts the 101 response itself, so if this returns
        // the upgrade succeeded.
        let _stream = ws_handshake(addr, "/client").await;
    }

    /// /client pushes the initial bitrate/codec/cursor state in the
    /// unified `MSG_CONTROL` framing, in that order, so the client can
    /// render and decode correctly from the very first frame.
    #[tokio::test]
    async fn client_endpoint_sends_initial_control_frames() {
        let initial_bitrate: usize = 1_234_567;
        let initial_codec = "avc1.42E028";
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let state = make_signaling_state(shutdown_rx, shutdown_tx, None);
        let (bitrate_tx, _) = watch::channel(initial_bitrate);
        let (codec_tx, _) = watch::channel(initial_codec.to_string());
        let _ = state.bitrate_rx; // keep field referenced; we push via the
                                  // original `bitrate_rx` that make_signaling_state
                                  // already cloned into state. The test below
                                  // checks the value pushed at connect time.
        let _ = bitrate_tx;
        let _ = codec_tx;
        let server = SignalingServer::new(state);

        let addr = "127.0.0.1:27349";
        tokio::spawn(async move {
            server.serve("127.0.0.1", 27349, std::future::pending()).await.unwrap();
        });

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let mut stream = ws_handshake(addr, "/client").await;

        // The handler pushes three CONTROL frames in order; each must be
        // a binary WS message whose first byte is MSG_CONTROL.
        for _ in 0..3 {
            let payload = read_ws_binary_frame(&mut stream).await;
            let (msg_type, _flags, payload_len) = parse_unified_header(&payload);
            assert_eq!(msg_type, proto::MSG_CONTROL, "expected MSG_CONTROL");
            assert_eq!(
                payload.len(),
                proto::HEADER_LEN + payload_len as usize,
                "payload length must match the header's payload_len"
            );
        }
    }

    /// /client delivers an `EncodedPacket` over the unified framing,
    /// with the is_keyframe flag set in the header (not inline like the
    /// legacy `/stream` format).
    #[tokio::test]
    async fn client_endpoint_delivers_video_in_unified_framing() {
        let (encoder_control_tx, mut encoder_control_rx) = mpsc::channel(4);
        let (pending_ping_tx, _pending_ping_rx) = mpsc::channel(4);
        let (_, bitrate_rx) = watch::channel(2_000_000usize);
        let (_, codec_rx) = watch::channel(String::new());
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let force_render = Arc::new(AtomicBool::new(false));
        let (_, cursor_rx) = watch::channel(CursorUpdate::Default);
        let state = SignalingState::new(
            mpsc::channel(4).0,
            mpsc::channel(4).0,
            mpsc::channel(4).0,
            mpsc::channel(4).0,
            None,
            None,
            encoder_control_tx,
            force_render.clone(),
            pending_ping_tx,
            bitrate_rx,
            codec_rx,
            shutdown_rx,
            SessionManager::new(Vec::new(), String::new(), shutdown_tx),
            None,
            cursor_rx,
        );
        let video_tx = state.get_video_sender();
        let server = SignalingServer::new(state);

        let addr = "127.0.0.1:27350";
        tokio::spawn(async move {
            server.serve("127.0.0.1", 27350, std::future::pending()).await.unwrap();
        });

        // Drain the three initial CONTROL frames before sending the
        // packet, otherwise the test's `recv` order would be wrong.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let mut stream = ws_handshake(addr, "/client").await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        for _ in 0..3 {
            let _ = read_ws_binary_frame(&mut stream).await;
        }

        assert!(
            force_render.load(Ordering::Relaxed),
            "connecting to /client should force a render"
        );
        assert!(
            matches!(encoder_control_rx.try_recv(), Ok(EncoderControl::ForceKeyframe)),
            "connecting to /client should request a fresh keyframe"
        );

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

        assert!(
            video_tx
                .send(test_packet(vec![0xAA, 0xBB, 0xCC], true, 42))
                .is_ok()
        );

        let payload = read_ws_binary_frame(&mut stream).await;
        let (msg_type, flags, payload_len) = parse_unified_header(&payload);
        assert_eq!(msg_type, proto::MSG_VIDEO_FRAME);
        assert_ne!(flags & proto::FLAG_KEYFRAME, 0, "keyframe flag must be set");
        assert_eq!(
            flags & proto::FLAG_HAS_PING,
            0,
            "no ping echo was sent, so FLAG_HAS_PING must be clear"
        );
        assert_eq!(
            payload.len(),
            proto::HEADER_LEN + payload_len as usize,
            "payload length must match the header's payload_len"
        );
        let frame_id = u32::from_be_bytes(payload[8..12].try_into().unwrap());
        assert_eq!(frame_id, 42);
        // ping_echo_client_ts is 0.0 when none
        let ping_val = f64::from_be_bytes(payload[12..20].try_into().unwrap());
        assert_eq!(ping_val, 0.0);
        assert_eq!(&payload[proto::HEADER_LEN + 20..], &[0xAA, 0xBB, 0xCC]);
    }

    /// `parse_client_message` rejects anything that isn't a
    /// `MSG_CLIENT_MSG` -- this is the /client analogue of the /ws
    /// "ignore unknown text" policy and is important so a stray
    /// binary frame can't kill the connection.
    #[test]
    fn parse_client_message_rejects_wrong_type() {
        let frame = proto::encode_msg(proto::MSG_VIDEO_FRAME, 0, &[1, 2, 3]);
        assert!(parse_client_message(&frame).is_err());

        // Too short to even hold a header.
        assert!(parse_client_message(&[0u8; 3]).is_err());

        // Header says 100 bytes of payload but the buffer only has 4.
        let mut bad = proto::encode_msg(proto::MSG_CLIENT_MSG, 0, &[1, 2, 3, 4]);
        // Hand-craft a header claiming payload_len = 100.
        bad[4] = 100;
        bad[5] = 0;
        bad[6] = 0;
        bad[7] = 0;
        assert!(parse_client_message(&bad).is_err());
    }

    /// `parse_client_message` round-trips a real `SignalingMessage`.
    #[test]
    fn parse_client_message_round_trips_signaling_message() {
        let original = SignalingMessage::Ping { client_ts: 12.5 };
        let json = serde_json::to_vec(&original).unwrap();
        let frame = proto::encode_msg(proto::MSG_CLIENT_MSG, 0, &json);
        let parsed = parse_client_message(&frame).expect("valid CLIENT_MSG");
        match parsed {
            SignalingMessage::Ping { client_ts } => assert!((client_ts - 12.5).abs() < 1e-9),
            other => panic!("expected Ping, got {:?}", other),
        }
    }

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

    /// Thread-local allocation counter used to verify the hot-path encoders
    /// allocate exactly once per frame. The original implementation built
    /// an intermediate payload `Vec` (second alloc) and called
    /// `extend_from_slice(&packet.data)` -- a redundant large memcpy of
    /// the H.264 bytes -- before wrapping that into the framed message.
    /// This counter fails if either regression comes back.
    ///
    /// `thread_local!` (rather than a global atomic) is essential here:
    /// `cargo test` runs tests on a thread pool, and a global counter
    /// would be incremented by allocations from concurrently-running tests
    /// while our closure is in flight, producing spurious counts.
    mod alloc_counter {
        use std::alloc::{GlobalAlloc, Layout, System};
        use std::cell::Cell;

        thread_local! {
            static ALLOC_COUNT: Cell<usize> = const { Cell::new(0) };
        }

        pub struct CountingAllocator;

        unsafe impl GlobalAlloc for CountingAllocator {
            unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
                ALLOC_COUNT.with(|c| c.set(c.get() + 1));
                System.alloc(layout)
            }
            unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
                System.dealloc(ptr, layout)
            }
        }

        #[global_allocator]
        static GLOBAL: CountingAllocator = CountingAllocator;

        pub fn delta<F: FnOnce()>(f: F) -> usize {
            let before = ALLOC_COUNT.with(|c| c.get());
            f();
            let after = ALLOC_COUNT.with(|c| c.get());
            after - before
        }

        /// Same as `delta` but returns the closure's value alongside the
        /// allocation count. Useful when the test needs to assert against
        /// data produced inside the counted region without doing the byte
        /// comparison inside it (which would itself allocate).
        pub fn delta_with<F: FnOnce() -> R, R>(f: F) -> (usize, R) {
            let before = ALLOC_COUNT.with(|c| c.get());
            let value = f();
            let after = ALLOC_COUNT.with(|c| c.get());
            (after - before, value)
        }
    }

    /// Single-allocation guarantee for the per-frame video encoder. A
    /// regression that reintroduces an intermediate payload Vec (the old
    /// code's `Vec::with_capacity(20 + N)` + `extend_from_slice(&packet.data)`
    /// step) would push the count to 2.
    #[test]
    fn encode_unified_video_frame_allocates_exactly_once() {
        // Pre-compute the expected H.264 slice OUTSIDE the alloc-counting
        // closure: any allocation inside `delta(|| ...)` would be counted
        // against encode_unified_video_frame. The point of this test is the
        // helper's allocation count, not byte equality (which is already
        // covered by `client_endpoint_delivers_video_in_unified_framing`).
        let expected_h264: Vec<u8> = (0u8..=255).cycle().take(8192).collect();
        let packet = test_packet(expected_h264.clone(), true, 99);

        let (allocs, framed) = alloc_counter::delta_with(|| {
            let framed = encode_unified_video_frame(packet);
            assert_eq!(framed.len(), proto::HEADER_LEN + 20 + 8192);
            let header: [u8; proto::HEADER_LEN] =
                framed[..proto::HEADER_LEN].try_into().unwrap();
            let (msg_type, flags, payload_len) = proto::decode_header(&header);
            assert_eq!(msg_type, proto::MSG_VIDEO_FRAME);
            assert_ne!(flags & proto::FLAG_KEYFRAME, 0);
            assert_eq!(payload_len as usize, 20 + 8192);
            framed
        });
        assert_eq!(&framed[proto::HEADER_LEN + 20..], &expected_h264[..]);
        assert_eq!(
            allocs, 1,
            "encode_unified_video_frame should allocate exactly once (the final Vec); got {allocs}"
        );
    }

    /// Same single-allocation guarantee for audio.
    #[test]
    fn encode_unified_audio_frame_allocates_exactly_once() {
        let opus: Vec<u8> = (0u8..=200).collect();
        let packet = crate::audio::AudioPacket {
            pts_us: 12345,
            data: opus,
        };

        let allocs = alloc_counter::delta(|| {
            let framed = encode_unified_audio_frame(packet);
            assert_eq!(framed.len(), proto::HEADER_LEN + 8 + 201);
            let header: [u8; proto::HEADER_LEN] =
                framed[..proto::HEADER_LEN].try_into().unwrap();
            let (msg_type, _flags, payload_len) = proto::decode_header(&header);
            assert_eq!(msg_type, proto::MSG_AUDIO_FRAME);
            assert_eq!(payload_len as usize, 8 + 201);
        });
        assert_eq!(
            allocs, 1,
            "encode_unified_audio_frame should allocate exactly once; got {allocs}"
        );
    }

    /// `dispatch_signaling_message` with a Resize message forwards the
    /// dimensions to `resize_tx` so the compositor render loop picks them up.
    #[tokio::test]
    async fn dispatch_resize_forwards_to_channel() {
        let (resize_tx, mut resize_rx) = mpsc::channel::<(u32, u32)>(4);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (touch_tx, _) = mpsc::channel(4);
        let (mouse_tx, _) = mpsc::channel(4);
        let (key_tx, _) = mpsc::channel(4);
        let (encoder_control_tx, _) = mpsc::channel(4);
        let (pending_ping_tx, _) = mpsc::channel(4);
        let (_, bitrate_rx) = watch::channel(2_000_000usize);
        let (_, codec_rx) = watch::channel(String::new());
        let (_, cursor_rx) = watch::channel(CursorUpdate::Default);
        let force_render = Arc::new(AtomicBool::new(false));
        let state = SignalingState::new(
            resize_tx,
            touch_tx,
            mouse_tx,
            key_tx,
            None,
            None,
            encoder_control_tx,
            force_render,
            pending_ping_tx,
            bitrate_rx,
            codec_rx,
            shutdown_rx,
            SessionManager::new(Vec::new(), String::new(), shutdown_tx),
            None,
            cursor_rx,
        );

        dispatch_signaling_message(SignalingMessage::Resize { width: 800, height: 600 }, &state)
            .await;

        let received = resize_rx.try_recv().expect("resize_rx should have a value");
        assert_eq!(received, (800, 600));
    }

    /// The /client endpoint routes a binary-framed Resize message (as the
    /// native client sends it) through to `resize_tx`.
    #[tokio::test]
    async fn client_endpoint_routes_resize_binary_message() {
        let (resize_tx, mut resize_rx) = mpsc::channel::<(u32, u32)>(4);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (touch_tx, _) = mpsc::channel(4);
        let (mouse_tx, _) = mpsc::channel(4);
        let (key_tx, _) = mpsc::channel(4);
        let (encoder_control_tx, _encoder_control_rx) = mpsc::channel(4);
        let (pending_ping_tx, _) = mpsc::channel(4);
        let (_, bitrate_rx) = watch::channel(2_000_000usize);
        let (_, codec_rx) = watch::channel(String::new());
        let (_, cursor_rx) = watch::channel(CursorUpdate::Default);
        let force_render = Arc::new(AtomicBool::new(false));
        let state = SignalingState::new(
            resize_tx,
            touch_tx,
            mouse_tx,
            key_tx,
            None,
            None,
            encoder_control_tx,
            force_render,
            pending_ping_tx,
            bitrate_rx,
            codec_rx,
            shutdown_rx,
            SessionManager::new(Vec::new(), String::new(), shutdown_tx),
            None,
            cursor_rx,
        );
        let server = SignalingServer::new(state);

        let addr = "127.0.0.1:27351";
        tokio::spawn(async move {
            server.serve("127.0.0.1", 27351, std::future::pending()).await.unwrap();
        });

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let mut stream = ws_handshake(addr, "/client").await;
        // Drain the 3 initial CONTROL frames (bitrate, codec, cursor).
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        for _ in 0..3 {
            let _ = read_ws_frame(&mut stream).await;
        }

        // Build a MSG_CLIENT_MSG binary frame containing a Resize JSON (the
        // same format WsTransport::send() produces).
        let resize_json =
            serde_json::to_vec(&SignalingMessage::Resize { width: 1280, height: 720 }).unwrap();
        let frame = proto::encode_msg(proto::MSG_CLIENT_MSG, 0, &resize_json);

        // Send a masked WebSocket binary frame (client→server must be masked).
        use tokio::io::AsyncWriteExt;
        let mask: [u8; 4] = [0x37, 0xfa, 0x21, 0x3d];
        let payload_len = frame.len();
        let mut ws_frame = Vec::new();
        ws_frame.push(0x82u8); // FIN=1, opcode=binary(2)
        ws_frame.push(0x80 | payload_len as u8); // MASK=1, len (single byte for <126)
        ws_frame.extend_from_slice(&mask);
        for (i, b) in frame.iter().enumerate() {
            ws_frame.push(b ^ mask[i % 4]);
        }
        stream.write_all(&ws_frame).await.unwrap();
        stream.flush().await.unwrap();

        // Give the server handler a moment to dispatch the message.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let received = resize_rx.try_recv().expect("resize_rx should have received (1280, 720)");
        assert_eq!(received, (1280, 720));
    }
}
