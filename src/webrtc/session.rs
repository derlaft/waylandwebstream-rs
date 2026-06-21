// Per-client WebRTC session

use anyhow::{Context, Result};
use bytes::Bytes;
use interceptor::registry::Registry;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{error, info, warn};
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_H264};
use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::APIBuilder;
use webrtc::ice_transport::ice_connection_state::RTCIceConnectionState;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::media::Sample;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::rtp_codec::{RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType};
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::track::track_local::TrackLocal;

use crate::encoder::{EncodedPacket, EncoderControl};
use crate::webrtc::signaling::{IceCandidate, SdpAnswer, SdpOffer};

/// WebRTC session for a single client
pub struct Session {
    peer_connection: Arc<webrtc::peer_connection::RTCPeerConnection>,
    video_track: Arc<TrackLocalStaticSample>,
}

impl Session {
    /// Create a new WebRTC session
    pub async fn new(ice_tx: mpsc::Sender<IceCandidate>) -> Result<Self> {
        // Create a MediaEngine with H.264 support
        let mut media_engine = MediaEngine::default();
        
        media_engine
            .register_codec(
                RTCRtpCodecParameters {
                    capability: RTCRtpCodecCapability {
                        mime_type: MIME_TYPE_H264.to_owned(),
                        clock_rate: 90000,
                        channels: 0,
                        sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                            .to_owned(),
                        rtcp_feedback: vec![],
                    },
                    payload_type: 96,
                    ..Default::default()
                },
                RTPCodecType::Video,
            )
            .context("Failed to register H.264 codec")?;

        // Create an InterceptorRegistry
        let mut registry = Registry::new();
        registry = register_default_interceptors(registry, &mut media_engine)?;

        // Create SettingEngine for network configuration
        let mut setting_engine = SettingEngine::default();
        
        // Detect local network interfaces for WebRTC
        if let Ok(local_ips) = local_ip_address::list_afinet_netifas() {
            // Find best non-loopback IPv4 address
            for (name, ip) in local_ips {
                if !ip.is_loopback() && matches!(ip, IpAddr::V4(_)) {
                    info!("WebRTC using network interface: {} ({})", name, ip);
                    setting_engine.set_nat_1to1_ips(
                        vec![ip.to_string()], 
                        webrtc::ice_transport::ice_candidate_type::RTCIceCandidateType::Host
                    );
                    break;
                }
            }
        }

        // Create the API object with the MediaEngine and SettingEngine
        let api = APIBuilder::new()
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .with_setting_engine(setting_engine)
            .build();

        // Configure ICE servers (STUN)
        let config = RTCConfiguration {
            ice_servers: vec![RTCIceServer {
                urls: vec!["stun:stun.l.google.com:19302".to_owned()],
                ..Default::default()
            }],
            ..Default::default()
        };

        // Create a new RTCPeerConnection
        let peer_connection = Arc::new(api.new_peer_connection(config).await?);

        // Create a video track (using Sample-based track for proper H.264 handling)
        let video_track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_H264.to_owned(),
                clock_rate: 90000,
                channels: 0,
                sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                    .to_owned(),
                rtcp_feedback: vec![],
            },
            "video".to_owned(),
            "waylandwebstream".to_owned(),
        ));

        // Add the track to the peer connection
        let rtp_sender = peer_connection
            .add_track(Arc::clone(&video_track) as Arc<dyn TrackLocal + Send + Sync>)
            .await?;

        // Read RTCP packets in a separate task
        tokio::spawn(async move {
            let mut rtcp_buf = vec![0u8; 1500];
            while let Ok((_, _)) = rtp_sender.read(&mut rtcp_buf).await {
                // Handle RTCP feedback (for adaptive bitrate in the future)
            }
        });

        // Set up connection state handlers
        let pc = Arc::downgrade(&peer_connection);
        peer_connection.on_peer_connection_state_change(Box::new(move |state: RTCPeerConnectionState| {
            info!("Peer connection state changed: {}", state);
            if state == RTCPeerConnectionState::Failed || state == RTCPeerConnectionState::Closed {
                if let Some(pc) = pc.upgrade() {
                    tokio::spawn(async move {
                        if let Err(e) = pc.close().await {
                            warn!("Error closing peer connection: {}", e);
                        }
                    });
                }
            }
            Box::pin(async {})
        }));

        peer_connection.on_ice_connection_state_change(Box::new(move |state: RTCIceConnectionState| {
            match state {
                RTCIceConnectionState::Connected => info!("ICE connection established"),
                RTCIceConnectionState::Failed => error!("ICE connection failed"),
                RTCIceConnectionState::Disconnected => warn!("ICE connection disconnected"),
                _ => {}
            }
            Box::pin(async {})
        }));

        // Set up ICE candidate handler
        peer_connection.on_ice_candidate(Box::new(move |candidate| {
            let ice_tx = ice_tx.clone();
            Box::pin(async move {
                if let Some(candidate) = candidate {
                    if let Ok(json) = candidate.to_json() {
                        let ice_candidate = IceCandidate {
                            candidate: json.candidate,
                            sdp_mline_index: json.sdp_mline_index.unwrap_or(0),
                            sdp_mid: json.sdp_mid,
                        };
                        
                        if let Err(e) = ice_tx.send(ice_candidate).await {
                            warn!("Failed to send ICE candidate: {}", e);
                        }
                    }
                }
            })
        }));

        Ok(Self {
            peer_connection,
            video_track,
        })
    }

    /// Handle an SDP offer and generate an answer
    pub async fn handle_offer(&self, offer: SdpOffer) -> Result<SdpAnswer> {
        info!("Processing SDP offer");

        // Parse the offer
        let offer_sdp = RTCSessionDescription::offer(offer.sdp)?;

        // Set the remote description
        self.peer_connection
            .set_remote_description(offer_sdp)
            .await
            .context("Failed to set remote description")?;

        // Create an answer
        let answer = self
            .peer_connection
            .create_answer(None)
            .await
            .context("Failed to create answer")?;

        // Set the local description (this will trigger ICE gathering)
        self.peer_connection
            .set_local_description(answer.clone())
            .await
            .context("Failed to set local description")?;

        // Wait for ICE gathering to complete
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Get the updated local description with ICE candidates
        let local_desc = self.peer_connection.local_description().await;
        let final_sdp = if let Some(desc) = local_desc {
            desc.sdp
        } else {
            answer.sdp
        };

        Ok(SdpAnswer {
            sdp: final_sdp,
            sdp_type: "answer".to_string(),
        })
    }

    /// Send an encoded video packet over the track
    pub async fn send_video_packet(&self, packet: EncodedPacket) -> Result<()> {
        // Create a Sample for the track (duration ~33ms for 30fps)
        let sample = Sample {
            data: Bytes::from(packet.data),
            duration: Duration::from_millis(33),
            timestamp: std::time::SystemTime::now(),
            ..Default::default()
        };
        
        // Write sample to the track
        self.video_track
            .write_sample(&sample)
            .await
            .context("Failed to write video sample")?;
        
        Ok(())
    }

    /// Close the session
    pub async fn close(&self) -> Result<()> {
        self.peer_connection
            .close()
            .await
            .context("Failed to close peer connection")
    }
}

/// Session manager that handles offers and manages active sessions
pub struct SessionManager {
    offer_rx: mpsc::Receiver<(SdpOffer, tokio::sync::oneshot::Sender<SdpAnswer>)>,
    packet_rx: mpsc::Receiver<EncodedPacket>,
    ice_tx: mpsc::Sender<IceCandidate>,
    encoder_control_tx: mpsc::Sender<EncoderControl>,
    active_session: Option<Arc<Session>>,
}

impl SessionManager {
    pub fn new(
        offer_rx: mpsc::Receiver<(SdpOffer, tokio::sync::oneshot::Sender<SdpAnswer>)>,
        packet_rx: mpsc::Receiver<EncodedPacket>,
        ice_tx: mpsc::Sender<IceCandidate>,
        encoder_control_tx: mpsc::Sender<EncoderControl>,
    ) -> Self {
        Self {
            offer_rx,
            packet_rx,
            ice_tx,
            encoder_control_tx,
            active_session: None,
        }
    }

    /// Run the session manager
    pub async fn run(mut self) -> Result<()> {
        info!("Session manager started");

        loop {
            tokio::select! {
                // Handle new offers
                Some((offer, answer_tx)) = self.offer_rx.recv() => {
                    info!("Received new offer, creating session");
                    
                    match Session::new(self.ice_tx.clone()).await {
                        Ok(session) => {
                            match session.handle_offer(offer).await {
                                Ok(answer) => {
                                    let _ = answer_tx.send(answer);
                                    self.active_session = Some(Arc::new(session));
                                    info!("WebRTC session established");
                                    
                                    // Request keyframe for new session
                                    if let Err(e) = self.encoder_control_tx.send(EncoderControl::ForceKeyframe).await {
                                        warn!("Failed to request keyframe: {}", e);
                                    }
                                }
                                Err(e) => {
                                    error!("Failed to handle offer: {}", e);
                                }
                            }
                        }
                        Err(e) => {
                            error!("Failed to create session: {}", e);
                        }
                    }
                }

                // Forward encoded packets to the active session
                Some(packet) = self.packet_rx.recv() => {
                    if let Some(ref session) = self.active_session {
                        if let Err(e) = session.send_video_packet(packet).await {
                            warn!("Failed to send video packet: {}", e);
                        }
                    }
                }

                else => break,
            }
        }

        info!("Session manager stopped");
        Ok(())
    }
}
