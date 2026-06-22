// HTTP/WebSocket signaling server (offer/answer/ICE)

use anyhow::{Context, Result};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc};
use tower_http::trace::TraceLayer;
use tracing::{info, warn};

use crate::input::mouse::MouseEvent;
use crate::input::touch::TouchEvent;
use crate::latency::LatencyReport;
use crate::web::client_html::CLIENT_HTML;
use crate::webrtc::turn_server::IceServerConfig;

/// A single ICE server entry, shaped to match the `RTCIceServer` dictionary
/// the browser's `RTCPeerConnection` constructor expects.
#[derive(Serialize)]
pub struct IceServerJson {
    pub urls: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IceConfigResponse {
    pub ice_servers: Vec<IceServerJson>,
}

/// SDP offer from the browser
#[derive(Debug, Deserialize)]
pub struct SdpOffer {
    pub sdp: String,
}

/// SDP answer to the browser
#[derive(Debug, Serialize)]
pub struct SdpAnswer {
    pub sdp: String,
    #[serde(rename = "type")]
    pub sdp_type: String,
}

/// ICE candidate message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IceCandidate {
    pub candidate: String,
    #[serde(rename = "sdpMLineIndex")]
    pub sdp_mline_index: u16,
    #[serde(rename = "sdpMid")]
    pub sdp_mid: Option<String>,
}

/// Signaling messages between client and server
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SignalingMessage {
    #[serde(rename = "ice")]
    Ice { candidate: IceCandidate },
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

/// Shared state for the signaling server
#[derive(Clone)]
pub struct SignalingState {
    /// Broadcast channel for ICE candidates from server to clients
    ice_tx: broadcast::Sender<IceCandidate>,
    /// Channel to send offers from clients to the WebRTC session manager
    offer_tx: mpsc::Sender<(SdpOffer, tokio::sync::oneshot::Sender<SdpAnswer>)>,
    /// Channel to forward ICE candidates trickled by clients to the WebRTC session manager
    remote_ice_tx: mpsc::Sender<IceCandidate>,
    /// Channel to send resize requests from clients
    resize_tx: mpsc::Sender<(u32, u32)>,
    /// Channel to send touch events from clients
    touch_tx: mpsc::Sender<TouchEvent>,
    /// Channel to send pointer (mouse/pen) events from clients
    mouse_tx: mpsc::Sender<MouseEvent>,
    /// Channel to send latency reports from clients
    latency_tx: Option<mpsc::Sender<LatencyReport>>,
    /// STUN/TURN server list handed out to clients via `/ice-config`
    ice_config: IceServerConfig,
}

impl SignalingState {
    pub fn new(
        offer_tx: mpsc::Sender<(SdpOffer, tokio::sync::oneshot::Sender<SdpAnswer>)>,
        remote_ice_tx: mpsc::Sender<IceCandidate>,
        resize_tx: mpsc::Sender<(u32, u32)>,
        touch_tx: mpsc::Sender<TouchEvent>,
        mouse_tx: mpsc::Sender<MouseEvent>,
        latency_tx: Option<mpsc::Sender<LatencyReport>>,
        ice_config: IceServerConfig,
    ) -> Self {
        let (ice_tx, _) = broadcast::channel(16);
        Self { ice_tx, offer_tx, remote_ice_tx, resize_tx, touch_tx, mouse_tx, latency_tx, ice_config }
    }

    pub fn get_ice_receiver(&self) -> broadcast::Receiver<IceCandidate> {
        self.ice_tx.subscribe()
    }

    pub fn get_ice_sender(&self) -> mpsc::Sender<IceCandidate> {
        let tx = self.ice_tx.clone();
        let (ice_mpsc_tx, mut ice_mpsc_rx) = mpsc::channel::<IceCandidate>(16);
        
        // Spawn a task to forward mpsc messages to broadcast
        tokio::spawn(async move {
            while let Some(candidate) = ice_mpsc_rx.recv().await {
                let _ = tx.send(candidate);
            }
        });
        
        ice_mpsc_tx
    }
}

pub struct SignalingServer {
    router: Router,
}

impl SignalingServer {
    pub fn new(state: SignalingState) -> Self {
        let router = Router::new()
            .route("/", get(serve_client))
            .route("/offer", post(handle_offer))
            .route("/ice-config", get(handle_ice_config))
            .route("/ws", get(handle_websocket))
            .layer(TraceLayer::new_for_http())
            .with_state(state);

        Self { router }
    }

    pub async fn serve(self, port: u16) -> Result<()> {
        let addr = format!("0.0.0.0:{}", port);
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

/// Serve the STUN/TURN server list the client should use for ICE
async fn handle_ice_config(State(state): State<SignalingState>) -> Json<IceConfigResponse> {
    let ice_config = &state.ice_config;
    Json(IceConfigResponse {
        ice_servers: vec![
            IceServerJson {
                urls: vec![ice_config.stun_url.clone()],
                username: None,
                credential: None,
            },
            IceServerJson {
                urls: vec![ice_config.turn_url.clone()],
                username: Some(ice_config.turn_username.clone()),
                credential: Some(ice_config.turn_password.clone()),
            },
        ],
    })
}

/// Handle SDP offer from browser
async fn handle_offer(
    State(state): State<SignalingState>,
    Json(offer): Json<SdpOffer>,
) -> Result<Json<SdpAnswer>, Response> {
    info!("Received SDP offer from client");

    // Create a oneshot channel to receive the answer
    let (tx, rx) = tokio::sync::oneshot::channel();

    // Send the offer to the WebRTC session manager
    state
        .offer_tx
        .send((offer, tx))
        .await
        .map_err(|_| {
            warn!("Failed to send offer to WebRTC session");
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "WebRTC session not available",
            )
                .into_response()
        })?;

    // Wait for the answer
    let answer = rx.await.map_err(|_| {
        warn!("Failed to receive answer from WebRTC session");
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to generate answer",
        )
            .into_response()
    })?;

    info!("Sending SDP answer to client");
    Ok(Json(answer))
}

/// Handle WebSocket connection for ICE candidate trickle
async fn handle_websocket(
    ws: WebSocketUpgrade,
    State(state): State<SignalingState>,
) -> Response {
    ws.on_upgrade(move |socket| websocket_handler(socket, state))
}

/// WebSocket handler for ICE candidate exchange
async fn websocket_handler(socket: WebSocket, state: SignalingState) {
    let (mut sender, mut receiver) = socket.split();
    let mut ice_rx = state.get_ice_receiver();

    // Spawn task to forward server ICE candidates to client
    let send_task = tokio::spawn(async move {
        while let Ok(candidate) = ice_rx.recv().await {
            let msg = SignalingMessage::Ice { candidate };
            if let Ok(json) = serde_json::to_string(&msg) {
                if sender.send(Message::Text(json)).await.is_err() {
                    break;
                }
            }
        }
    });

    // Handle incoming messages from client
    while let Some(Ok(msg)) = receiver.next().await {
        if let Message::Text(text) = msg {
            if let Ok(msg) = serde_json::from_str::<SignalingMessage>(&text) {
                match msg {
                    SignalingMessage::Ice { candidate } => {
                        info!("Received ICE candidate from client: {:?}", candidate);
                        if let Err(e) = state.remote_ice_tx.send(candidate).await {
                            warn!("Failed to forward ICE candidate to session manager: {}", e);
                        }
                    }
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
                    SignalingMessage::Latency { encoding_ms, network_ms, jitter_buffer_ms, decoding_ms, total_ms } => {
                        info!("Received latency message from client: {:.1}ms total", total_ms);
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

    send_task.abort();
}
