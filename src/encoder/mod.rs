pub mod frame;

use anyhow::{Context, Result};
use ffmpeg_next as ffmpeg;
use tokio::sync::{mpsc, watch};
use tracing::{error, info, warn};

/// Raw frame data to be encoded
#[derive(Clone)]
pub struct RawFrame {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub timestamp: i64,
}

/// Encoded video packet (H.264 NAL units)
#[derive(Clone)]
pub struct EncodedPacket {
    pub data: Vec<u8>,
    pub timestamp: i64,
    pub is_keyframe: bool,
}

/// Resolution change event
#[derive(Clone, Debug)]
pub struct ResolutionChange {
    pub width: u32,
    pub height: u32,
}

/// Encoder configuration
#[derive(Clone, Debug)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub framerate: u32,
    pub bitrate: usize,
    pub keyframe_interval: u32,
}

impl Default for EncoderConfig {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 720,
            framerate: 30,
            bitrate: 2_000_000, // 2 Mbps
            keyframe_interval: 60, // 2 seconds at 30fps
        }
    }
}

/// Control messages for the encoder
#[derive(Clone, Debug)]
pub enum EncoderControl {
    ForceKeyframe,
}

/// Handle for controlling the encoder thread
pub struct EncoderHandle {
    frame_tx: mpsc::Sender<RawFrame>,
    packet_rx: mpsc::Receiver<EncodedPacket>,
    resize_tx: watch::Sender<Option<ResolutionChange>>,
    control_tx: mpsc::Sender<EncoderControl>,
}

impl EncoderHandle {
    /// Send a frame to be encoded
    pub async fn send_frame(&self, frame: RawFrame) -> Result<()> {
        self.frame_tx
            .send(frame)
            .await
            .context("Failed to send frame to encoder")
    }

    /// Try to send a frame without blocking
    pub fn try_send_frame(&self, frame: RawFrame) -> Result<()> {
        self.frame_tx
            .try_send(frame)
            .context("Failed to send frame to encoder (queue full)")
    }

    /// Receive an encoded packet
    pub async fn recv_packet(&mut self) -> Option<EncodedPacket> {
        self.packet_rx.recv().await
    }

    /// Request a resolution change
    pub fn resize(&self, width: u32, height: u32) -> Result<()> {
        self.resize_tx
            .send(Some(ResolutionChange { width, height }))
            .context("Failed to send resize request")
    }

    /// Get a cloneable frame sender for use in other threads
    pub fn get_frame_sender(&self) -> mpsc::Sender<RawFrame> {
        self.frame_tx.clone()
    }

    /// Request a keyframe to be generated
    pub async fn request_keyframe(&self) -> Result<()> {
        self.control_tx
            .send(EncoderControl::ForceKeyframe)
            .await
            .context("Failed to send keyframe request")
    }

    /// Get a cloneable control sender
    pub fn get_control_sender(&self) -> mpsc::Sender<EncoderControl> {
        self.control_tx.clone()
    }

    /// Get a cloneable resize sender
    pub fn get_resize_sender(&self) -> watch::Sender<Option<ResolutionChange>> {
        self.resize_tx.clone()
    }
}

/// Spawn the encoder thread
pub fn spawn_encoder(config: EncoderConfig) -> Result<EncoderHandle> {
    // Initialize FFmpeg
    ffmpeg::init().context("Failed to initialize FFmpeg")?;

    let (frame_tx, frame_rx) = mpsc::channel::<RawFrame>(4); // Bounded channel with small buffer
    let (packet_tx, packet_rx) = mpsc::channel::<EncodedPacket>(16);
    let (resize_tx, resize_rx) = watch::channel::<Option<ResolutionChange>>(None);
    let (control_tx, control_rx) = mpsc::channel::<EncoderControl>(8);

    // Spawn encoder thread
    std::thread::spawn(move || {
        if let Err(e) = encoder_thread(config, frame_rx, packet_tx, resize_rx, control_rx) {
            error!("Encoder thread failed: {}", e);
        }
    });

    Ok(EncoderHandle {
        frame_tx,
        packet_rx,
        resize_tx,
        control_tx,
    })
}

/// Encoder thread main loop
fn encoder_thread(
    mut config: EncoderConfig,
    mut frame_rx: mpsc::Receiver<RawFrame>,
    packet_tx: mpsc::Sender<EncodedPacket>,
    mut resize_rx: watch::Receiver<Option<ResolutionChange>>,
    mut control_rx: mpsc::Receiver<EncoderControl>,
) -> Result<()> {
    info!("Encoder thread started with config: {:?}", config);

    // Create initial encoder context
    let mut encoder = create_encoder(&config)?;
    let mut scaler = create_scaler(&config)?;
    let mut frame_count = 0i64;
    let mut force_keyframe = false;

    loop {
        // Check for control messages (non-blocking)
        while let Ok(control) = control_rx.try_recv() {
            match control {
                EncoderControl::ForceKeyframe => {
                    info!("Keyframe requested");
                    force_keyframe = true;
                }
            }
        }

        // Check for resize events
        if resize_rx.has_changed().unwrap_or(false) {
            let resize = resize_rx.borrow_and_update().clone();
            if let Some(resize) = resize {
                info!("Encoder resizing to {}x{}", resize.width, resize.height);
                
                // Update config
                config.width = resize.width;
                config.height = resize.height;

                // Reinitialize encoder and scaler
                match (create_encoder(&config), create_scaler(&config)) {
                    (Ok(new_encoder), Ok(new_scaler)) => {
                        encoder = new_encoder;
                        scaler = new_scaler;
                        frame_count = 0; // Reset frame count to force IDR
                        info!("Encoder reinitialized successfully");
                    }
                    (Err(e), _) | (_, Err(e)) => {
                        error!("Failed to reinitialize encoder: {}", e);
                        return Err(e);
                    }
                }
            }
        }

        // Receive frame with timeout
        let raw_frame = match frame_rx.blocking_recv() {
            Some(frame) => frame,
            None => {
                info!("Frame channel closed, encoder thread exiting");
                break;
            }
        };

        // Force keyframe if requested by resetting frame count
        if force_keyframe {
            info!("Forcing keyframe");
            frame_count = 0;
            force_keyframe = false;
        }

        // Encode the frame
        match encode_frame(&mut encoder, &mut scaler, &raw_frame, frame_count) {
            Ok(packets) => {
                for packet in packets {
                    if packet_tx.blocking_send(packet).is_err() {
                        warn!("Failed to send encoded packet (channel closed)");
                        return Ok(());
                    }
                }
                frame_count += 1;
            }
            Err(e) => {
                error!("Failed to encode frame: {}", e);
            }
        }
    }

    Ok(())
}

/// Create FFmpeg encoder context
fn create_encoder(config: &EncoderConfig) -> Result<ffmpeg::encoder::Video> {
    let codec = ffmpeg::encoder::find(ffmpeg::codec::Id::H264)
        .context("H.264 encoder not found")?;

    let mut encoder = ffmpeg::codec::context::Context::new_with_codec(codec)
        .encoder()
        .video()?;
    
    encoder.set_width(config.width);
    encoder.set_height(config.height);
    encoder.set_format(ffmpeg::format::Pixel::YUV420P);
    encoder.set_frame_rate(Some(ffmpeg::Rational::new(config.framerate as i32, 1)));
    encoder.set_time_base(ffmpeg::Rational::new(1, config.framerate as i32));
    encoder.set_bit_rate(config.bitrate);

    // Set x264-specific options for low latency
    let mut opts = ffmpeg::Dictionary::new();
    opts.set("preset", "ultrafast");
    opts.set("tune", "zerolatency");
    opts.set("profile", "baseline");
    opts.set("level", "3.1");
    opts.set("bframes", "0"); // No B-frames for low latency
    opts.set("g", &config.keyframe_interval.to_string()); // GOP size
    opts.set("keyint_min", &config.keyframe_interval.to_string());
    opts.set("sc_threshold", "0"); // Disable scene change detection
    opts.set("repeat_headers", "1"); // Include SPS/PPS with every keyframe
    opts.set("annex_b", "1"); // Use Annex B format (required for RTP)

    let encoder = encoder.open_with(opts)?;
    
    info!("Encoder initialized: {}x{} @ {}fps, {} bps", 
          config.width, config.height, config.framerate, config.bitrate);

    Ok(encoder)
}

/// Create swscale context for pixel format conversion
fn create_scaler(config: &EncoderConfig) -> Result<ffmpeg::software::scaling::Context> {
    let scaler = ffmpeg::software::scaling::Context::get(
        ffmpeg::format::Pixel::BGRA,
        config.width,
        config.height,
        ffmpeg::format::Pixel::YUV420P,
        config.width,
        config.height,
        ffmpeg::software::scaling::Flags::FAST_BILINEAR,
    )?;

    Ok(scaler)
}

/// Encode a single frame
fn encode_frame(
    encoder: &mut ffmpeg::encoder::Video,
    scaler: &mut ffmpeg::software::scaling::Context,
    raw_frame: &RawFrame,
    frame_number: i64,
) -> Result<Vec<EncodedPacket>> {
    // Create input frame (BGRA)
    let mut input_frame = ffmpeg::frame::Video::new(
        ffmpeg::format::Pixel::BGRA,
        raw_frame.width,
        raw_frame.height,
    );
    
    // Copy raw data to frame
    input_frame.data_mut(0).copy_from_slice(&raw_frame.data);

    // Create output frame (YUV420P)
    let mut yuv_frame = ffmpeg::frame::Video::new(
        ffmpeg::format::Pixel::YUV420P,
        raw_frame.width,
        raw_frame.height,
    );

    // Convert BGRA to YUV420P
    scaler.run(&input_frame, &mut yuv_frame)?;

    // Set frame properties
    yuv_frame.set_pts(Some(frame_number));
    yuv_frame.set_kind(ffmpeg::picture::Type::None);

    // Encode frame
    encoder.send_frame(&yuv_frame)?;

    // Receive encoded packets
    let mut packets = Vec::new();
    loop {
        let mut encoded_packet = ffmpeg::Packet::empty();
        match encoder.receive_packet(&mut encoded_packet) {
            Ok(_) => {
                let is_keyframe = encoded_packet.is_key();
                let data = encoded_packet.data().unwrap_or(&[]).to_vec();
                
                packets.push(EncodedPacket {
                    data,
                    timestamp: raw_frame.timestamp,
                    is_keyframe,
                });
            }
            Err(ffmpeg::Error::Other { errno: ffmpeg::error::EAGAIN }) => {
                // No more packets available
                break;
            }
            Err(e) => return Err(e.into()),
        }
    }

    Ok(packets)
}
