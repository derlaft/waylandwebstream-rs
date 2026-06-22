// Per-client WebRTC session

use anyhow::{Context, Result};
use bytes::Bytes;
use interceptor::registry::Registry;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::mpsc;
use tracing::{error, info, warn};
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_H264};
use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::APIBuilder;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
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
use crate::webrtc::turn_server::IceServerConfig;

/// WebRTC session for a single client
pub struct Session {
    peer_connection: Arc<webrtc::peer_connection::RTCPeerConnection>,
    video_track: Arc<TrackLocalStaticSample>,
    frame_duration: Duration,
    /// (capture Instant, wall-clock SystemTime) of the first packet sent,
    /// used to translate later packets' `capture_time` into a SystemTime
    /// that tracks capture cadence rather than send-time jitter.
    capture_epoch: Mutex<Option<(Instant, SystemTime)>>,
}

impl Session {
    /// Create a new WebRTC session
    pub async fn new(ice_tx: mpsc::Sender<IceCandidate>, ice_config: &IceServerConfig, framerate: u32) -> Result<Self> {
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

        // Configure ICE servers: STUN plus our embedded TURN relay, needed
        // for networks like netbird's WireGuard overlay that can't carry the
        // multicast mDNS traffic required to resolve browsers' obfuscated
        // host candidates.
        let config = RTCConfiguration {
            ice_servers: vec![
                RTCIceServer {
                    urls: vec![ice_config.stun_url.clone()],
                    ..Default::default()
                },
                RTCIceServer {
                    urls: vec![ice_config.turn_url.clone()],
                    username: ice_config.turn_username.clone(),
                    credential: ice_config.turn_password.clone(),
                },
            ],
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
            while let Ok((packets, _)) = rtp_sender.read(&mut rtcp_buf).await {
                if !packets.is_empty() {
                    tracing::debug!("Received {} RTCP feedback packet(s)", packets.len());
                }
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
            frame_duration: Duration::from_secs_f64(1.0 / framerate as f64),
            capture_epoch: Mutex::new(None),
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

    /// Add a remote ICE candidate received (trickled) from the client
    pub async fn add_ice_candidate(&self, candidate: IceCandidate) -> Result<()> {
        self.peer_connection
            .add_ice_candidate(RTCIceCandidateInit {
                candidate: candidate.candidate,
                sdp_mid: candidate.sdp_mid,
                sdp_mline_index: Some(candidate.sdp_mline_index),
                ..Default::default()
            })
            .await
            .context("Failed to add remote ICE candidate")
    }

    /// Send an encoded video packet over the track
    pub async fn send_video_packet(&self, packet: EncodedPacket) -> Result<()> {
        // Anchor capture_time (an Instant) to a SystemTime once, then derive
        // every later timestamp from that anchor plus elapsed capture time.
        // This makes RTP timestamps track capture cadence instead of jittery
        // send time (which varies with encoder/channel scheduling delay).
        let timestamp = {
            let mut epoch = self.capture_epoch.lock().unwrap();
            let (base_instant, base_systime) = *epoch.get_or_insert_with(|| (packet.capture_time, SystemTime::now()));
            base_systime + packet.capture_time.duration_since(base_instant)
        };

        let sample = Sample {
            data: Bytes::from(packet.data),
            duration: self.frame_duration,
            timestamp,
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
    remote_ice_rx: mpsc::Receiver<IceCandidate>,
    ice_tx: mpsc::Sender<IceCandidate>,
    encoder_control_tx: mpsc::Sender<EncoderControl>,
    ice_config: IceServerConfig,
    framerate: u32,
    active_session: Option<Arc<Session>>,
    /// Set when a new session is established so the capture loop renders a
    /// frame immediately instead of waiting on damage or the next periodic
    /// keyframe-cadence render -- otherwise a newly connected client sees
    /// nothing until the screen happens to change.
    force_render: Arc<std::sync::atomic::AtomicBool>,
}

impl SessionManager {
    pub fn new(
        offer_rx: mpsc::Receiver<(SdpOffer, tokio::sync::oneshot::Sender<SdpAnswer>)>,
        packet_rx: mpsc::Receiver<EncodedPacket>,
        remote_ice_rx: mpsc::Receiver<IceCandidate>,
        ice_tx: mpsc::Sender<IceCandidate>,
        encoder_control_tx: mpsc::Sender<EncoderControl>,
        ice_config: IceServerConfig,
        framerate: u32,
        force_render: Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        Self {
            offer_rx,
            packet_rx,
            remote_ice_rx,
            ice_tx,
            encoder_control_tx,
            ice_config,
            framerate,
            active_session: None,
            force_render,
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
                    
                    match Session::new(self.ice_tx.clone(), &self.ice_config, self.framerate).await {
                        Ok(session) => {
                            match session.handle_offer(offer).await {
                                Ok(answer) => {
                                    let _ = answer_tx.send(answer);

                                    // Tear down the previous session (if any) now that the
                                    // new one is in place, rather than just dropping it --
                                    // otherwise its RTCPeerConnection and ICE agent linger
                                    // instead of being explicitly closed.
                                    if let Some(old_session) = self.active_session.take() {
                                        if let Err(e) = old_session.close().await {
                                            warn!("Failed to close previous session: {}", e);
                                        }
                                    }
                                    self.active_session = Some(Arc::new(session));
                                    info!("WebRTC session established");
                                    
                                    // Request keyframe for new session, and make sure the
                                    // capture loop actually renders+sends a frame for it to
                                    // ride on -- otherwise, with damage tracking, an idle
                                    // screen would leave this client with no video until the
                                    // next change or periodic keyframe-cadence render.
                                    self.force_render.store(true, std::sync::atomic::Ordering::Relaxed);
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

                // Forward trickled ICE candidates from the client to the active session
                Some(candidate) = self.remote_ice_rx.recv() => {
                    if let Some(ref session) = self.active_session {
                        if let Err(e) = session.add_ice_candidate(candidate).await {
                            warn!("Failed to add remote ICE candidate: {}", e);
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
