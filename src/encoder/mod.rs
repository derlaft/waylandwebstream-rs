use anyhow::{Context, Result};
use ffmpeg_next as ffmpeg;
use smithay::backend::allocator::dmabuf::Dmabuf;
use tokio::sync::{mpsc, watch};
use tracing::{error, info, warn};

mod vaapi;

/// Raw frame data to be encoded
#[derive(Clone)]
pub struct RawFrame {
    pub data: Vec<u8>,
    /// Dimensions `data` was rendered at. Carried alongside the buffer
    /// (rather than inferred from the encoder's current `EncoderConfig`)
    /// because a resize notification on `resize_rx` and the first frame
    /// rendered at the new size can both reach the encoder thread before it
    /// next checks `resize_rx` -- see the mismatch check in `encoder_thread`.
    pub width: u32,
    pub height: u32,
    /// When the render loop captured this frame -- the start of the
    /// server-side latency pipeline (see `capture_to_encode_ms` below).
    pub capture_instant: std::time::Instant,
}

/// A frame handed from a `Compositor` to a `VideoEncoder`, in whichever
/// memory the compositor produced it in. Today only `Cpu` is ever
/// constructed (the manual memcpy compositor); `Gpu` exists so a future GL
/// compositor (hardware-acceleration-plan.md Phase B) can hand a dmabuf
/// straight to a VAAPI encoder without a CPU round-trip.
#[derive(Clone)]
pub enum CapturedFrame {
    Cpu(RawFrame),
    // Not constructed until a GL compositor exists to produce one --
    // expected to be dead code until then.
    #[allow(dead_code)]
    Gpu {
        dmabuf: Dmabuf,
        width: u32,
        height: u32,
        capture_instant: std::time::Instant,
    },
}

impl CapturedFrame {
    pub fn dimensions(&self) -> (u32, u32) {
        match self {
            CapturedFrame::Cpu(frame) => (frame.width, frame.height),
            CapturedFrame::Gpu { width, height, .. } => (*width, *height),
        }
    }

    pub fn capture_instant(&self) -> std::time::Instant {
        match self {
            CapturedFrame::Cpu(frame) => frame.capture_instant,
            CapturedFrame::Gpu { capture_instant, .. } => *capture_instant,
        }
    }
}

/// Which codec implementation the encoder thread drives. `Vaapi` is the
/// hardware-acceleration-plan.md Phase A backend (`vaapi::VaapiEncoder`):
/// `hwupload,scale_vaapi=format=nv12` does BGRA->NV12 on the GPU, then
/// `h264_vaapi` encodes the result.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum EncoderBackend {
    #[default]
    X264,
    Vaapi,
}

/// Encoded video packet (H.264 NAL units)
#[derive(Clone)]
pub struct EncodedPacket {
    pub data: Vec<u8>,
    /// Whether this packet is an IDR/keyframe, as reported by the encoder.
    /// WebCodecs needs each chunk tagged `key` or `delta` to know which ones
    /// it can start decoding from.
    pub is_keyframe: bool,
    /// Monotonic packet id (wraps), used by consumers to detect drops/gaps
    /// without depending on RTP sequencing.
    pub frame_id: u32,
    /// Time the raw frame spent queued before the encoder thread picked it
    /// up, i.e. `encode_start - RawFrame::capture_instant`.
    pub capture_to_encode_ms: f64,
    /// Time libx264 spent actually encoding this frame.
    pub encoding_ms: f64,
    /// When encoding finished. Used by the packet-forwarding loop (not sent
    /// over the wire) to measure encode→send/broadcast queueing time.
    pub encode_complete: std::time::Instant,
    /// Echoes a client's `ping` (`SignalingMessage::Ping` in src/server.rs)
    /// back on the next frame to leave the encoder, so the client can
    /// measure full round-trip latency (network + server pipeline) without
    /// needing synchronized clocks -- see src/server.rs's `encode_video_frame`.
    pub ping_echo_client_ts: Option<f64>,
}

/// Resolution change event
#[derive(Clone, Debug)]
pub struct ResolutionChange {
    pub width: u32,
    pub height: u32,
}

/// How the encoder targets output size vs. quality
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RateControl {
    /// Target an average bitrate (bits per second), capped by a VBV window
    /// so individual frames (e.g. keyframes) can't balloon the jitter buffer.
    Bitrate(usize),
    /// Target a constant quality via x264's CRF: each frame gets whatever
    /// bits it needs to hit the quality level, so output size varies with
    /// scene complexity instead of being capped. Range 0-51, lower = better
    /// quality/bigger frames; x264's default is 23.
    Quality(u8),
}

/// Encoder configuration
#[derive(Clone, Debug)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub framerate: u32,
    pub rate_control: RateControl,
    pub keyframe_interval: u32,
    pub encoder_backend: EncoderBackend,
    /// DRM render node opened by `EncoderBackend::Vaapi`. Unused by `X264`.
    pub vaapi_device: String,
}

impl Default for EncoderConfig {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 720,
            framerate: 30,
            rate_control: RateControl::Bitrate(2_000_000), // 2 Mbps
            keyframe_interval: 60, // 2 seconds at 30fps
            encoder_backend: EncoderBackend::X264,
            vaapi_device: "/dev/dri/renderD128".to_string(),
        }
    }
}

/// Control messages for the encoder
#[derive(Clone, Debug)]
pub enum EncoderControl {
    ForceKeyframe,
    ChangeBitrate(usize),
}

/// Handle for controlling the encoder thread
pub struct EncoderHandle {
    frame_tx: mpsc::Sender<CapturedFrame>,
    packet_rx: mpsc::Receiver<EncodedPacket>,
    resize_tx: watch::Sender<Option<ResolutionChange>>,
    control_tx: mpsc::Sender<EncoderControl>,
}

/// Receives back `RawFrame` buffers once the encoder thread has copied their
/// contents into its own frame and no longer needs them, so the render loop
/// can reuse them instead of allocating a fresh buffer every frame.
pub type BufferReturnReceiver = std::sync::mpsc::Receiver<Vec<u8>>;

impl EncoderHandle {
    /// Receive an encoded packet
    pub async fn recv_packet(&mut self) -> Option<EncodedPacket> {
        self.packet_rx.recv().await
    }

    /// Get a cloneable frame sender for use in other threads
    pub fn get_frame_sender(&self) -> mpsc::Sender<CapturedFrame> {
        self.frame_tx.clone()
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

/// Spawn the encoder thread. `codec_tx` is updated with a fresh WebCodecs
/// codec string (see `h264_codec_string`) whenever a resolution change makes
/// the encoder pick a different H.264 level, so callers can forward it to
/// connected clients.
///
/// The returned `JoinHandle` lets a caller wait for the thread to actually
/// exit during shutdown (it terminates once every `RawFrame` sender --
/// `EncoderHandle::frame_tx` and any clones -- has been dropped) instead of
/// leaving it to be torn down whenever the process happens to exit.
pub fn spawn_encoder(
    config: EncoderConfig,
    codec_tx: watch::Sender<String>,
) -> Result<(EncoderHandle, BufferReturnReceiver, std::thread::JoinHandle<()>)> {
    // Initialize FFmpeg
    ffmpeg::init().context("Failed to initialize FFmpeg")?;

    let (frame_tx, frame_rx) = mpsc::channel::<CapturedFrame>(4); // Bounded channel with small buffer
    let (packet_tx, packet_rx) = mpsc::channel::<EncodedPacket>(16);
    let (resize_tx, resize_rx) = watch::channel::<Option<ResolutionChange>>(None);
    let (control_tx, control_rx) = mpsc::channel::<EncoderControl>(8);
    let (buffer_return_tx, buffer_return_rx) = std::sync::mpsc::channel::<Vec<u8>>();

    // Spawn encoder thread
    let join_handle = std::thread::spawn(move || {
        if let Err(e) = encoder_thread(config, frame_rx, packet_tx, resize_rx, control_rx, buffer_return_tx, codec_tx) {
            error!("Encoder thread failed: {}", e);
        }
    });

    Ok((
        EncoderHandle {
            frame_tx,
            packet_rx,
            resize_tx,
            control_tx,
        },
        buffer_return_rx,
        join_handle,
    ))
}

/// A pluggable video encoder backend, driven by the encoder thread. Backend
/// selection happens once at thread start (see `build_video_encoder`), based
/// on `EncoderConfig::encoder_backend` -- the thread loop itself never
/// touches a concrete codec type again, so a future VAAPI backend
/// (hardware-acceleration-plan.md Phase A) drops in without reshaping this
/// loop a second time.
pub trait VideoEncoder {
    /// Encode one frame, returning zero or more ready packets. Implementors
    /// that consume `CapturedFrame::Cpu` are responsible for returning the
    /// buffer via whatever `BufferReturnReceiver`-side sender they were
    /// constructed with, regardless of whether encoding succeeded --
    /// mirrors the original encoder_thread's unconditional buffer-return.
    fn submit(
        &mut self,
        frame: CapturedFrame,
        capture_to_encode_ms: f64,
        force_keyframe: bool,
    ) -> Result<Vec<EncodedPacket>>;

    /// Tear down and rebuild for a new resolution, resetting GOP state so
    /// the next frame starts a fresh IDR.
    fn reinitialize(&mut self, width: u32, height: u32) -> Result<()>;

    /// Apply a new bitrate target. Returns whether it actually changed
    /// anything -- a constant-quality (CRF/CQP) backend ignores this and
    /// returns `false`, matching today's warn-and-ignore behavior.
    fn change_bitrate(&mut self, bitrate: usize) -> bool;

    /// Current WebCodecs codec string (see `h264_codec_string`) for the
    /// active resolution/profile, forwarded to `codec_tx` by the caller
    /// after every successful `reinitialize`.
    fn codec_string(&self) -> String;

    fn width(&self) -> u32;
    fn height(&self) -> u32;
}

/// Software x264 backend -- today's only `VideoEncoder` implementation.
/// Wraps exactly what `encoder_thread`'s locals used to hold.
struct X264Encoder {
    config: EncoderConfig,
    encoder: ffmpeg::encoder::Video,
    scaler: ffmpeg::software::scaling::Context,
    input_frame: ffmpeg::frame::Video,
    // Unlike `input_frame`, this owns a real (refcounted) buffer, so
    // `avcodec_send_frame` is allowed to keep a reference to it instead of
    // copying -- see the safety note in `encode_frame` on why reusing it
    // across calls is still fine here.
    yuv_frame: ffmpeg::frame::Video,
    frame_count: i64,
    next_frame_id: u32,
    buffer_return_tx: std::sync::mpsc::Sender<Vec<u8>>,
}

impl X264Encoder {
    fn new(config: EncoderConfig, buffer_return_tx: std::sync::mpsc::Sender<Vec<u8>>) -> Result<Self> {
        let encoder = create_encoder(&config)?;
        let scaler = create_scaler(&config)?;
        let input_frame = create_input_frame(config.width, config.height);
        let yuv_frame = ffmpeg::frame::Video::new(ffmpeg::format::Pixel::YUV420P, config.width, config.height);
        Ok(Self {
            config,
            encoder,
            scaler,
            input_frame,
            yuv_frame,
            frame_count: 0,
            next_frame_id: 0,
            buffer_return_tx,
        })
    }
}

impl VideoEncoder for X264Encoder {
    fn submit(
        &mut self,
        frame: CapturedFrame,
        capture_to_encode_ms: f64,
        force_keyframe: bool,
    ) -> Result<Vec<EncodedPacket>> {
        let raw_frame = match frame {
            CapturedFrame::Cpu(raw_frame) => raw_frame,
            CapturedFrame::Gpu { .. } => {
                anyhow::bail!("x264 backend cannot encode a GPU frame; GPU compositing needs --encoder vaapi");
            }
        };

        let result = encode_frame(
            &mut self.encoder,
            &mut self.scaler,
            &mut self.input_frame,
            &mut self.yuv_frame,
            &raw_frame,
            self.frame_count,
            &mut self.next_frame_id,
            force_keyframe,
            capture_to_encode_ms,
        );

        // The encoder has already copied everything it needs out of
        // raw_frame.data (encode_frame only borrows it) -- hand the buffer
        // back to the render loop so it can reuse it instead of allocating a
        // fresh one next frame, regardless of whether encoding succeeded.
        // Ignore failure: it just means the render loop has dropped the
        // receiver, in which case the buffer is freed normally.
        let _ = self.buffer_return_tx.send(raw_frame.data);

        let packets = result?;
        self.frame_count += 1;
        Ok(packets)
    }

    fn reinitialize(&mut self, width: u32, height: u32) -> Result<()> {
        self.config.width = width;
        self.config.height = height;

        self.encoder = create_encoder(&self.config)?;
        self.scaler = create_scaler(&self.config)?;
        self.input_frame = create_input_frame(width, height);
        self.yuv_frame = ffmpeg::frame::Video::new(ffmpeg::format::Pixel::YUV420P, width, height);
        self.frame_count = 0;

        Ok(())
    }

    fn change_bitrate(&mut self, bitrate: usize) -> bool {
        if self.config.rate_control == RateControl::Bitrate(bitrate) {
            return false;
        }
        if !matches!(self.config.rate_control, RateControl::Bitrate(_)) {
            warn!("Ignoring bitrate change request: encoder is in constant-quality mode");
            return false;
        }
        info!("Changing bitrate from {:?} to {} bps", self.config.rate_control, bitrate);
        self.config.rate_control = RateControl::Bitrate(bitrate);

        match create_encoder(&self.config) {
            Ok(new_encoder) => {
                self.encoder = new_encoder;
                self.frame_count = 0; // Reset frame count to force IDR
                info!("Encoder reinitialized with new bitrate");
                true
            }
            Err(e) => {
                error!("Failed to reinitialize encoder with new bitrate: {}", e);
                false
            }
        }
    }

    fn codec_string(&self) -> String {
        h264_codec_string(self.config.width, self.config.height, self.config.framerate)
    }

    fn width(&self) -> u32 {
        self.config.width
    }

    fn height(&self) -> u32 {
        self.config.height
    }
}

/// Builds the `VideoEncoder` backend selected by `config.encoder_backend`.
fn build_video_encoder(
    config: &EncoderConfig,
    buffer_return_tx: std::sync::mpsc::Sender<Vec<u8>>,
) -> Result<Box<dyn VideoEncoder>> {
    match config.encoder_backend {
        EncoderBackend::X264 => Ok(Box::new(X264Encoder::new(config.clone(), buffer_return_tx)?)),
        EncoderBackend::Vaapi => Ok(Box::new(vaapi::VaapiEncoder::new(config.clone(), buffer_return_tx)?)),
    }
}

/// Drain and apply any pending control messages without blocking. Split out
/// so it can be called both before and after waiting for the next raw
/// frame -- see the call site after `blocking_recv` for why that second
/// call matters.
fn drain_control_messages(
    control_rx: &mut mpsc::Receiver<EncoderControl>,
    video_encoder: &mut dyn VideoEncoder,
    force_keyframe: &mut bool,
) {
    while let Ok(control) = control_rx.try_recv() {
        match control {
            EncoderControl::ForceKeyframe => {
                info!("Keyframe requested");
                *force_keyframe = true;
            }
            EncoderControl::ChangeBitrate(new_bitrate) => {
                video_encoder.change_bitrate(new_bitrate);
            }
        }
    }
}

/// Encoder thread main loop
fn encoder_thread(
    config: EncoderConfig,
    mut frame_rx: mpsc::Receiver<CapturedFrame>,
    packet_tx: mpsc::Sender<EncodedPacket>,
    mut resize_rx: watch::Receiver<Option<ResolutionChange>>,
    mut control_rx: mpsc::Receiver<EncoderControl>,
    buffer_return_tx: std::sync::mpsc::Sender<Vec<u8>>,
    codec_tx: watch::Sender<String>,
) -> Result<()> {
    info!("Encoder thread started with config: {:?}", config);

    let mut video_encoder = build_video_encoder(&config, buffer_return_tx.clone())?;
    let mut force_keyframe = false;

    loop {
        // Check for control messages (non-blocking)
        drain_control_messages(&mut control_rx, video_encoder.as_mut(), &mut force_keyframe);

        // Check for resize events
        if resize_rx.has_changed().unwrap_or(false) {
            let resize = resize_rx.borrow_and_update().clone();
            if let Some(resize) = resize {
                info!("Encoder resizing to {}x{}", resize.width, resize.height);

                // Drain any frames already sitting in frame_rx: the render
                // loop (src/main.rs) only switches its own buffer size to
                // match a resize *after* sending this `ResolutionChange`, so
                // anything queued here was captured at the old resolution.
                // Without this, the next `blocking_recv` below could hand
                // the freshly-resized encoder an undersized buffer.
                while let Ok(stale_frame) = frame_rx.try_recv() {
                    if let CapturedFrame::Cpu(raw_frame) = stale_frame {
                        let _ = buffer_return_tx.send(raw_frame.data);
                    }
                }

                // Reinitialize encoder and scaler
                if let Err(e) = video_encoder.reinitialize(resize.width, resize.height) {
                    error!("Failed to reinitialize encoder: {}", e);
                    return Err(e);
                }
                let _ = codec_tx.send(video_encoder.codec_string());
                info!("Encoder reinitialized successfully");
            }
        }

        // Receive frame with timeout
        let captured_frame = match frame_rx.blocking_recv() {
            Some(frame) => frame,
            None => {
                info!("Frame channel closed, encoder thread exiting");
                break;
            }
        };

        // Safety net: normally the resize check above already reconfigures
        // the encoder ahead of the frame that needs it (and drains anything
        // captured at the old size). But `resize_rx` and `frame_rx` are
        // separate channels with no joint ordering guarantee on the consumer
        // side -- this thread can park in `blocking_recv` and wake up to a
        // frame rendered at a *new* size before it ever observes the resize
        // notification for it (e.g. the very first frame after startup,
        // sized to whatever viewport the connecting client reports). Trust
        // the frame's own declared dimensions over the encoder's current
        // config so that case still encodes correctly instead of bailing
        // with a buffer-size mismatch.
        let (frame_width, frame_height) = captured_frame.dimensions();
        if frame_width != video_encoder.width() || frame_height != video_encoder.height() {
            info!(
                "Frame arrived at {}x{} while encoder was configured for {}x{}; reinitializing to match",
                frame_width, frame_height, video_encoder.width(), video_encoder.height()
            );
            if let Err(e) = video_encoder.reinitialize(frame_width, frame_height) {
                error!("Failed to reinitialize encoder for frame's actual resolution: {}", e);
                if let CapturedFrame::Cpu(raw_frame) = captured_frame {
                    let _ = buffer_return_tx.send(raw_frame.data);
                }
                return Err(e);
            }
            let _ = codec_tx.send(video_encoder.codec_string());
        }

        // Drain again now that we actually have a frame in hand: a
        // ForceKeyframe sent right before this exact frame was produced
        // (the common case -- a new client's connect handler requests one
        // and the capture loop renders+sends a frame for it moments later)
        // would otherwise sit unseen until the *next* frame, since the
        // check above can run before the request even arrives if the
        // thread was already parked in `blocking_recv`.
        drain_control_messages(&mut control_rx, video_encoder.as_mut(), &mut force_keyframe);

        // Resetting GOP state here did nothing useful: libx264 places IDRs
        // based on its own internal frame counter against `g`/`keyint_min`,
        // not on the PTS values we assign. The actual way to force an IDR
        // out of libx264 via ffmpeg is to tag the frame `AV_PICTURE_TYPE_I`
        // before sending it -- see `encode_frame`.
        let force_this_frame = force_keyframe;
        if force_this_frame {
            info!("Forcing keyframe");
            force_keyframe = false;
        }

        let capture_to_encode_ms = captured_frame.capture_instant().elapsed().as_secs_f64() * 1000.0;
        let encode_start = std::time::Instant::now();

        let encode_result = video_encoder.submit(captured_frame, capture_to_encode_ms, force_this_frame);

        let encoding_ms = encode_start.elapsed().as_secs_f64() * 1000.0;
        let encode_complete = std::time::Instant::now();

        match encode_result {
            Ok(mut packets) => {
                for packet in &mut packets {
                    packet.encoding_ms = encoding_ms;
                    packet.encode_complete = encode_complete;
                }
                for packet in packets {
                    if packet_tx.blocking_send(packet).is_err() {
                        warn!("Failed to send encoded packet (channel closed)");
                        return Ok(());
                    }
                }
            }
            Err(e) => {
                error!("Failed to encode frame: {}", e);
            }
        }
    }

    Ok(())
}

/// H.264 level table (ITU-T H.264 Table A-1): level_idc (level * 10, except
/// the unused "1b" tier), MaxMBPS (macroblocks/sec), MaxFS (macroblocks/frame).
/// Used to pick the lowest level that actually covers a given
/// resolution/framerate, since hardcoding one level breaks the moment
/// resolution or framerate changes -- see `select_h264_level`.
const H264_LEVELS: &[(u8, u32, u32)] = &[
    (10, 1_485, 99),
    (11, 3_000, 396),
    (12, 6_000, 396),
    (13, 11_880, 396),
    (20, 11_880, 396),
    (21, 19_800, 792),
    (22, 20_250, 1_620),
    (30, 40_500, 1_620),
    (31, 108_000, 3_600),
    (32, 216_000, 5_120),
    (40, 245_760, 8_192),
    (41, 245_760, 8_192),
    (42, 522_240, 8_704),
    (50, 589_824, 22_080),
    (51, 983_040, 36_864),
    (52, 2_073_600, 36_864),
];

/// Picks the lowest H.264 level whose macroblock-rate and frame-size limits
/// cover this resolution/framerate, returning its level_idc (e.g. 31 for
/// level 3.1). Falls back to the highest known level if even that's exceeded
/// (e.g. resolutions well past 4K@60) rather than silently encoding a level
/// the stream can't possibly conform to.
pub(crate) fn select_h264_level(width: u32, height: u32, framerate: u32) -> u8 {
    let mbs_per_frame = width.div_ceil(16) * height.div_ceil(16);
    let mbps = mbs_per_frame * framerate;
    H264_LEVELS
        .iter()
        .find(|&&(_, max_mbps, max_fs)| mbps <= max_mbps && mbs_per_frame <= max_fs)
        .map(|&(idc, _, _)| idc)
        .unwrap_or_else(|| {
            warn!(
                "{}x{}@{}fps exceeds even H.264 level 5.2's limits; using 5.2 anyway",
                width, height, framerate
            );
            H264_LEVELS.last().unwrap().0
        })
}

/// Formats a level_idc as the dotted string x264's `level` option expects,
/// e.g. 31 -> "3.1", 40 -> "4".
pub(crate) fn h264_level_option(level_idc: u8) -> String {
    if level_idc % 10 == 0 {
        (level_idc / 10).to_string()
    } else {
        format!("{}.{}", level_idc / 10, level_idc % 10)
    }
}

/// Codec string for WebCodecs' `VideoDecoderConfig.codec`, e.g.
/// "avc1.42E01F" for Baseline profile (0x42), constrained-baseline
/// constraint flags (0xE0), level 3.1 (0x1F) -- see `select_h264_level` for
/// how the level is chosen. Kept in sync with the `profile`/`level` options
/// `create_encoder` passes to x264.
pub fn h264_codec_string(width: u32, height: u32, framerate: u32) -> String {
    format!("avc1.42E0{:02X}", select_h264_level(width, height, framerate))
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

    // Set x264-specific options for low latency
    let mut opts = ffmpeg::Dictionary::new();
    opts.set("preset", "ultrafast");
    opts.set("tune", "zerolatency");
    opts.set("profile", "baseline");
    opts.set("level", &h264_level_option(select_h264_level(config.width, config.height, config.framerate)));
    opts.set("bframes", "0"); // No B-frames for low latency
    opts.set("g", &config.keyframe_interval.to_string()); // GOP size
    opts.set("keyint_min", &config.keyframe_interval.to_string());
    opts.set("sc_threshold", "0"); // Disable scene change detection
    opts.set("repeat_headers", "1"); // Include SPS/PPS with every keyframe
    opts.set("annex_b", "1"); // Use Annex B format (required for RTP)

    match config.rate_control {
        RateControl::Bitrate(bitrate) => {
            encoder.set_bit_rate(bitrate);

            // Cap how far a single frame's size can exceed the target bitrate.
            // Without a VBV limit, x264's "bitrate" is only an average over the
            // whole stream - an IDR frame (every keyframe_interval frames) can come
            // out several times larger than a P-frame, and at 2Mbps a ~250KB
            // keyframe alone takes ~1 second to drain through the link. That shows
            // up as the receive-side jitter buffer ballooning every GOP and then
            // draining back down. Bounding vbv-bufsize caps that worst case.
            let vbv_maxrate_kbps = (bitrate / 1000).max(1);
            let vbv_bufsize_kbps = (vbv_maxrate_kbps / 4).max(1); // ~250ms worth of frames
            opts.set("vbv-maxrate", &vbv_maxrate_kbps.to_string());
            opts.set("vbv-bufsize", &vbv_bufsize_kbps.to_string());
        }
        RateControl::Quality(crf) => {
            // No VBV cap here: constant quality means frame size is whatever
            // the scene needs, so a busy frame (e.g. a keyframe) can be much
            // larger than at a fixed bitrate. Capping it would defeat the
            // point of asking for constant quality.
            opts.set("crf", &crf.to_string());
        }
    }

    let encoder = encoder.open_with(opts)?;

    info!("Encoder initialized: {}x{} @ {}fps, {:?}",
          config.width, config.height, config.framerate, config.rate_control);

    Ok(encoder)
}

/// Create swscale context for pixel format conversion. Source and
/// destination dimensions are always identical here (the encoder is
/// reinitialized to match `render()`'s output on every resize) -- this
/// context only ever does a colorspace conversion, never a resize. swscale
/// picks a dedicated "unscaled" SIMD converter for that case regardless of
/// the resampling flag (the flag only affects the filter built for an
/// actual scale), so `POINT` (cheapest, no interpolation) communicates
/// intent accurately without claiming a bilinear filter is in play.
fn create_scaler(config: &EncoderConfig) -> Result<ffmpeg::software::scaling::Context> {
    let scaler = ffmpeg::software::scaling::Context::get(
        ffmpeg::format::Pixel::BGRA,
        config.width,
        config.height,
        ffmpeg::format::Pixel::YUV420P,
        config.width,
        config.height,
        ffmpeg::software::scaling::Flags::POINT,
    )?;

    Ok(scaler)
}

/// Create a BGRA frame with no backing buffer of its own -- `encode_frame`
/// points its `data[0]`/`linesize[0]` directly at each `RawFrame`'s buffer
/// instead of copying into it, so this never needs (and must never get)
/// an owned buffer via `alloc()`.
pub(crate) fn create_input_frame(width: u32, height: u32) -> ffmpeg::frame::Video {
    let mut frame = ffmpeg::frame::Video::empty();
    frame.set_format(ffmpeg::format::Pixel::BGRA);
    frame.set_width(width);
    frame.set_height(height);
    frame
}

/// Encode a single frame
fn encode_frame(
    encoder: &mut ffmpeg::encoder::Video,
    scaler: &mut ffmpeg::software::scaling::Context,
    input_frame: &mut ffmpeg::frame::Video,
    yuv_frame: &mut ffmpeg::frame::Video,
    raw_frame: &RawFrame,
    frame_number: i64,
    next_frame_id: &mut u32,
    force_keyframe: bool,
    capture_to_encode_ms: f64,
) -> Result<Vec<EncodedPacket>> {
    // Point the input frame straight at the render buffer instead of
    // copying into an owned one -- swscale only reads through this
    // pointer within this call, and `raw_frame` outlives it. Stride is
    // `width * 4`: that's how `render()` packs `render_buffer`, with no
    // row padding.
    let expected_len = (input_frame.width() * input_frame.height() * 4) as usize;
    if raw_frame.data.len() < expected_len {
        anyhow::bail!(
            "raw frame buffer ({} bytes) too small for {}x{} BGRA ({} bytes expected)",
            raw_frame.data.len(),
            input_frame.width(),
            input_frame.height(),
            expected_len
        );
    }
    unsafe {
        let ptr = input_frame.as_mut_ptr();
        (*ptr).data[0] = raw_frame.data.as_ptr() as *mut u8;
        (*ptr).linesize[0] = (input_frame.width() * 4) as i32;
    }

    // Convert BGRA to YUV420P
    scaler.run(input_frame, yuv_frame)?;

    // Set frame properties. Tagging the frame `I` is what actually forces
    // libx264 to emit an IDR on demand -- `None` lets it decide normally per
    // the `g`/`keyint_min` GOP settings.
    yuv_frame.set_pts(Some(frame_number));
    yuv_frame.set_kind(if force_keyframe {
        ffmpeg::picture::Type::I
    } else {
        ffmpeg::picture::Type::None
    });

    // Encode frame
    encoder.send_frame(yuv_frame)?;

    // Safety note on reusing `yuv_frame`: avcodec_send_frame() is allowed to
    // keep a reference to a refcounted AVFrame rather than copy it, and
    // sws_scale (above, next call) writes into it without checking whether
    // anyone else still holds it. That's only safe here because the encoder
    // is configured (tune=zerolatency, bframes=0, no lookahead) to have zero
    // frame delay, so draining receive_packet() below to EAGAIN guarantees
    // libx264 is done reading this frame's pixels before we return and the
    // next call's scaler.run() overwrites the same buffer. If the encoder
    // config ever gains buffering/reordering (B-frames, lookahead), this
    // assumption breaks and yuv_frame would need to go back to being
    // allocated fresh per call (or use av_frame_make_writable first).
    drain_packets(encoder, next_frame_id, capture_to_encode_ms)
}

/// Drains every packet currently available from `encoder` (looping until
/// EAGAIN), tagging each with the per-frame timing fields the caller already
/// knows. Shared by the x264 and VAAPI submit paths -- the only difference
/// between them is how the frame got encoded, not how packets come back out.
pub(crate) fn drain_packets(
    encoder: &mut ffmpeg::encoder::Video,
    next_frame_id: &mut u32,
    capture_to_encode_ms: f64,
) -> Result<Vec<EncodedPacket>> {
    let mut packets = Vec::new();
    loop {
        let mut encoded_packet = ffmpeg::Packet::empty();
        match encoder.receive_packet(&mut encoded_packet) {
            Ok(_) => {
                let data = encoded_packet.data().unwrap_or(&[]).to_vec();
                let is_keyframe = encoded_packet.is_key();
                let frame_id = *next_frame_id;
                *next_frame_id = next_frame_id.wrapping_add(1);

                packets.push(EncodedPacket {
                    data,
                    is_keyframe,
                    frame_id,
                    capture_to_encode_ms,
                    // Overwritten by the caller right after this returns,
                    // once the actual encode duration is known.
                    encoding_ms: 0.0,
                    encode_complete: std::time::Instant::now(),
                    ping_echo_client_ts: None,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn make_raw_frame(width: u32, height: u32) -> RawFrame {
        RawFrame {
            data: vec![0u8; (width * height * 4) as usize],
            width,
            height,
            capture_instant: std::time::Instant::now(),
        }
    }

    /// Regression test for a real bug found while wiring up forced
    /// keyframes for new `/stream` clients: `EncoderControl::ForceKeyframe`
    /// used to just reset a local PTS counter, which libx264 ignores for
    /// IDR placement (it uses its own internal counter against
    /// `g`/`keyint_min`), so requested keyframes silently never happened.
    /// The fix tags the frame `AV_PICTURE_TYPE_I`, which libx264 does honor.
    ///
    /// Also exercises the case that mirrors production: the keyframe
    /// request and the frame it's meant to apply to arrive back-to-back
    /// (a `/stream` connect requests a keyframe, and the capture loop
    /// renders+sends a frame for it moments later) -- this depends on the
    /// encoder thread draining the control channel again after
    /// `blocking_recv`, not just before it.
    #[tokio::test]
    async fn force_keyframe_actually_forces_an_idr() {
        let config = EncoderConfig {
            width: 64,
            height: 64,
            framerate: 30,
            rate_control: RateControl::Bitrate(500_000),
            // Large enough that nothing in this test's frame count crosses
            // a natural GOP boundary on its own -- every keyframe we see
            // must come from an explicit ForceKeyframe.
            keyframe_interval: 1000,
            encoder_backend: EncoderBackend::X264,
            vaapi_device: "/dev/dri/renderD128".to_string(),
        };
        let (codec_tx, _codec_rx) = watch::channel(String::new());
        let (mut handle, _buffer_return_rx, _join_handle) =
            spawn_encoder(config.clone(), codec_tx).expect("failed to spawn encoder");

        let frame_tx = handle.get_frame_sender();
        let control_tx = handle.get_control_sender();

        // The first frame of a fresh GOP is always a keyframe -- baseline
        // sanity check, not the thing under test.
        frame_tx.send(CapturedFrame::Cpu(make_raw_frame(config.width, config.height))).await.unwrap();
        let packet = handle.recv_packet().await.expect("expected a packet");
        assert!(packet.is_keyframe, "first frame of a GOP should be a keyframe");

        // Ordinary frames with nothing requested should be P-frames.
        for _ in 0..3 {
            frame_tx.send(CapturedFrame::Cpu(make_raw_frame(config.width, config.height))).await.unwrap();
            let packet = handle.recv_packet().await.expect("expected a packet");
            assert!(!packet.is_keyframe, "frame without a keyframe request should not be an IDR");
        }

        // Request a keyframe, then immediately send the next frame with no
        // delay -- exercises the post-`blocking_recv` drain.
        control_tx.send(EncoderControl::ForceKeyframe).await.unwrap();
        frame_tx.send(CapturedFrame::Cpu(make_raw_frame(config.width, config.height))).await.unwrap();
        let packet = handle.recv_packet().await.expect("expected a packet");
        assert!(packet.is_keyframe, "ForceKeyframe should make the next frame an IDR");

        // The request should not stick beyond that one frame.
        frame_tx.send(CapturedFrame::Cpu(make_raw_frame(config.width, config.height))).await.unwrap();
        let packet = handle.recv_packet().await.expect("expected a packet");
        assert!(!packet.is_keyframe, "keyframe request should not affect frames after the one it targeted");
    }

    /// Regression test for a startup race: a connecting client's viewport
    /// resize and the first frame rendered at that new size both reach the
    /// encoder thread before it ever observes the `resize_rx` notification
    /// for it (it can be parked in `blocking_recv` from before the resize
    /// was even sent). That used to make `encode_frame` compare an
    /// already-correctly-sized buffer against the *old* `EncoderConfig`
    /// dimensions and bail with a "too small" error. Exercise it directly by
    /// sending a frame sized for a different resolution without ever
    /// touching `resize_tx` -- the encoder should reinitialize to match the
    /// frame instead of erroring.
    #[tokio::test]
    async fn frame_size_mismatch_reinitializes_encoder() {
        let config = EncoderConfig {
            width: 1280,
            height: 720,
            framerate: 30,
            rate_control: RateControl::Bitrate(2_000_000),
            keyframe_interval: 1000,
            encoder_backend: EncoderBackend::X264,
            vaapi_device: "/dev/dri/renderD128".to_string(),
        };
        let (codec_tx, mut codec_rx) = watch::channel(String::new());
        let (mut handle, _buffer_return_rx, _join_handle) =
            spawn_encoder(config.clone(), codec_tx).expect("failed to spawn encoder");

        let frame_tx = handle.get_frame_sender();

        // Never sent on resize_tx -- the encoder thread only learns about
        // this resolution from the frame's own width/height.
        let (new_width, new_height) = (800, 592);
        frame_tx.send(CapturedFrame::Cpu(make_raw_frame(new_width, new_height))).await.unwrap();

        let packet = tokio::time::timeout(std::time::Duration::from_secs(5), handle.recv_packet())
            .await
            .expect("encoder should not hang on a mismatched frame size")
            .expect("expected a packet");
        assert!(packet.is_keyframe, "reinitializing for the new size should reset the GOP");

        // codec_tx should reflect the new resolution.
        codec_rx.changed().await.unwrap();
        let codec = codec_rx.borrow().clone();
        assert!(!codec.is_empty(), "codec string should be updated for the new resolution");

        // A second frame at the same (new) size should encode normally, with
        // no further reinitialization needed.
        frame_tx.send(CapturedFrame::Cpu(make_raw_frame(new_width, new_height))).await.unwrap();
        let packet = tokio::time::timeout(std::time::Duration::from_secs(5), handle.recv_packet())
            .await
            .expect("follow-up frame at the same size should encode without hanging")
            .expect("expected a packet");
        assert!(!packet.is_keyframe, "frame after the reinit should not force another IDR");
    }
}
