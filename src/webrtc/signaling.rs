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

use crate::web::client_html::CLIENT_HTML;

/// SDP offer from the browser
#[derive(Debug, Deserialize)]
pub struct SdpOffer {
    pub sdp: String,
    #[serde(rename = "type")]
    pub sdp_type: String,
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
}

/// Shared state for the signaling server
#[derive(Clone)]
pub struct SignalingState {
    /// Broadcast channel for ICE candidates from server to clients
    ice_tx: broadcast::Sender<IceCandidate>,
    /// Channel to send offers from clients to the WebRTC session manager
    offer_tx: mpsc::Sender<(SdpOffer, tokio::sync::oneshot::Sender<SdpAnswer>)>,
    /// Channel to send resize requests from clients
    resize_tx: mpsc::Sender<(u32, u32)>,
}

impl SignalingState {
    pub fn new(
        offer_tx: mpsc::Sender<(SdpOffer, tokio::sync::oneshot::Sender<SdpAnswer>)>,
        resize_tx: mpsc::Sender<(u32, u32)>,
    ) -> Self {
        let (ice_tx, _) = broadcast::channel(16);
        Self { ice_tx, offer_tx, resize_tx }
    }

    pub fn get_ice_receiver(&self) -> broadcast::Receiver<IceCandidate> {
        self.ice_tx.subscribe()
    }

    pub fn send_ice_candidate(&self, candidate: IceCandidate) -> Result<()> {
        self.ice_tx
            .send(candidate)
            .context("Failed to broadcast ICE candidate")?;
        Ok(())
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
            .route("/ws", get(handle_websocket))
            .layer(TraceLayer::new_for_http())
            .with_state(state);

        Self { router }
    }

    pub fn router(self) -> Router {
        self.router
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
                        // TODO: Forward to WebRTC peer connection
                    }
                    SignalingMessage::Ready => {
                        info!("Client is ready");
                    }
                    SignalingMessage::Resize { width, height } => {
                        info!("Received resize request from client: {}x{}", width, height);
                        let _ = state.resize_tx.send((width, height)).await;
                    }
                }
            }
        }
    }

    send_task.abort();
}
