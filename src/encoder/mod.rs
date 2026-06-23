use anyhow::{Context, Result};
use ffmpeg_next as ffmpeg;
use tokio::sync::{mpsc, watch};
use tracing::{error, info, warn};

/// Raw frame data to be encoded
#[derive(Clone)]
pub struct RawFrame {
    pub data: Vec<u8>,
    /// When the render loop captured this frame -- the start of the
    /// server-side latency pipeline (see `capture_to_encode_ms` below).
    pub capture_instant: std::time::Instant,
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
}

impl Default for EncoderConfig {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 720,
            framerate: 30,
            rate_control: RateControl::Bitrate(2_000_000), // 2 Mbps
            keyframe_interval: 60, // 2 seconds at 30fps
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
    frame_tx: mpsc::Sender<RawFrame>,
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
    pub fn get_frame_sender(&self) -> mpsc::Sender<RawFrame> {
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

/// Spawn the encoder thread
pub fn spawn_encoder(config: EncoderConfig) -> Result<(EncoderHandle, BufferReturnReceiver)> {
    // Initialize FFmpeg
    ffmpeg::init().context("Failed to initialize FFmpeg")?;

    let (frame_tx, frame_rx) = mpsc::channel::<RawFrame>(4); // Bounded channel with small buffer
    let (packet_tx, packet_rx) = mpsc::channel::<EncodedPacket>(16);
    let (resize_tx, resize_rx) = watch::channel::<Option<ResolutionChange>>(None);
    let (control_tx, control_rx) = mpsc::channel::<EncoderControl>(8);
    let (buffer_return_tx, buffer_return_rx) = std::sync::mpsc::channel::<Vec<u8>>();

    // Spawn encoder thread
    std::thread::spawn(move || {
        if let Err(e) = encoder_thread(config, frame_rx, packet_tx, resize_rx, control_rx, buffer_return_tx) {
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
    ))
}

/// Drain and apply any pending control messages without blocking. Split out
/// so it can be called both before and after waiting for the next raw
/// frame -- see the call site after `blocking_recv` for why that second
/// call matters.
fn drain_control_messages(
    control_rx: &mut mpsc::Receiver<EncoderControl>,
    config: &mut EncoderConfig,
    encoder: &mut ffmpeg::encoder::Video,
    frame_count: &mut i64,
    force_keyframe: &mut bool,
) {
    while let Ok(control) = control_rx.try_recv() {
        match control {
            EncoderControl::ForceKeyframe => {
                info!("Keyframe requested");
                *force_keyframe = true;
            }
            EncoderControl::ChangeBitrate(new_bitrate) => {
                if config.rate_control == RateControl::Bitrate(new_bitrate) {
                    continue;
                }
                if !matches!(config.rate_control, RateControl::Bitrate(_)) {
                    warn!("Ignoring bitrate change request: encoder is in constant-quality mode");
                    continue;
                }
                info!("Changing bitrate from {:?} to {} bps", config.rate_control, new_bitrate);
                config.rate_control = RateControl::Bitrate(new_bitrate);

                // Reinitialize encoder with new bitrate
                match create_encoder(config) {
                    Ok(new_encoder) => {
                        *encoder = new_encoder;
                        *frame_count = 0; // Reset frame count to force IDR
                        info!("Encoder reinitialized with new bitrate");
                    }
                    Err(e) => {
                        error!("Failed to reinitialize encoder with new bitrate: {}", e);
                    }
                }
            }
        }
    }
}

/// Encoder thread main loop
fn encoder_thread(
    mut config: EncoderConfig,
    mut frame_rx: mpsc::Receiver<RawFrame>,
    packet_tx: mpsc::Sender<EncodedPacket>,
    mut resize_rx: watch::Receiver<Option<ResolutionChange>>,
    mut control_rx: mpsc::Receiver<EncoderControl>,
    buffer_return_tx: std::sync::mpsc::Sender<Vec<u8>>,
) -> Result<()> {
    info!("Encoder thread started with config: {:?}", config);

    // Create initial encoder context
    let mut encoder = create_encoder(&config)?;
    let mut scaler = create_scaler(&config)?;
    let mut input_frame = create_input_frame(config.width, config.height);
    // Unlike `input_frame`, this owns a real (refcounted) buffer, so
    // `avcodec_send_frame` is allowed to keep a reference to it instead of
    // copying -- see the safety note in `encode_frame` on why reusing it
    // across calls is still fine here.
    let mut yuv_frame = ffmpeg::frame::Video::new(ffmpeg::format::Pixel::YUV420P, config.width, config.height);
    let mut frame_count = 0i64;
    let mut force_keyframe = false;
    let mut next_frame_id = 0u32;

    loop {
        // Check for control messages (non-blocking)
        drain_control_messages(&mut control_rx, &mut config, &mut encoder, &mut frame_count, &mut force_keyframe);

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
                        input_frame = create_input_frame(config.width, config.height);
                        yuv_frame = ffmpeg::frame::Video::new(ffmpeg::format::Pixel::YUV420P, config.width, config.height);
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

        // Drain again now that we actually have a frame in hand: a
        // ForceKeyframe sent right before this exact frame was produced
        // (the common case -- a new client's connect handler requests one
        // and the capture loop renders+sends a frame for it moments later)
        // would otherwise sit unseen until the *next* frame, since the
        // check above can run before the request even arrives if the
        // thread was already parked in `blocking_recv`.
        drain_control_messages(&mut control_rx, &mut config, &mut encoder, &mut frame_count, &mut force_keyframe);

        // Resetting `frame_count` here did nothing useful: libx264 places
        // IDRs based on its own internal frame counter against `g`/
        // `keyint_min`, not on the PTS values we assign, and rewinding our
        // PTS counter on a live encoder context just makes it non-monotonic.
        // The actual way to force an IDR out of libx264 via ffmpeg is to tag
        // the frame `AV_PICTURE_TYPE_I` before sending it -- see
        // `encode_frame`.
        let force_this_frame = force_keyframe;
        if force_this_frame {
            info!("Forcing keyframe");
            force_keyframe = false;
        }

        let capture_to_encode_ms = raw_frame.capture_instant.elapsed().as_secs_f64() * 1000.0;
        let encode_start = std::time::Instant::now();

        // Encode the frame
        let encode_result = encode_frame(
            &mut encoder,
            &mut scaler,
            &mut input_frame,
            &mut yuv_frame,
            &raw_frame,
            frame_count,
            &mut next_frame_id,
            force_this_frame,
            capture_to_encode_ms,
        );

        let encoding_ms = encode_start.elapsed().as_secs_f64() * 1000.0;
        let encode_complete = std::time::Instant::now();

        // The encoder has already copied everything it needs out of
        // raw_frame.data (encode_frame only borrows it) -- hand the buffer
        // back to the render loop so it can reuse it instead of allocating a
        // fresh one next frame. Ignore failure: it just means the render
        // loop has dropped the receiver, in which case the buffer is freed
        // normally.
        let _ = buffer_return_tx.send(raw_frame.data);

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
fn create_input_frame(width: u32, height: u32) -> ffmpeg::frame::Video {
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
        };
        let (mut handle, _buffer_return_rx) =
            spawn_encoder(config.clone()).expect("failed to spawn encoder");

        let frame_tx = handle.get_frame_sender();
        let control_tx = handle.get_control_sender();

        // The first frame of a fresh GOP is always a keyframe -- baseline
        // sanity check, not the thing under test.
        frame_tx.send(make_raw_frame(config.width, config.height)).await.unwrap();
        let packet = handle.recv_packet().await.expect("expected a packet");
        assert!(packet.is_keyframe, "first frame of a GOP should be a keyframe");

        // Ordinary frames with nothing requested should be P-frames.
        for _ in 0..3 {
            frame_tx.send(make_raw_frame(config.width, config.height)).await.unwrap();
            let packet = handle.recv_packet().await.expect("expected a packet");
            assert!(!packet.is_keyframe, "frame without a keyframe request should not be an IDR");
        }

        // Request a keyframe, then immediately send the next frame with no
        // delay -- exercises the post-`blocking_recv` drain.
        control_tx.send(EncoderControl::ForceKeyframe).await.unwrap();
        frame_tx.send(make_raw_frame(config.width, config.height)).await.unwrap();
        let packet = handle.recv_packet().await.expect("expected a packet");
        assert!(packet.is_keyframe, "ForceKeyframe should make the next frame an IDR");

        // The request should not stick beyond that one frame.
        frame_tx.send(make_raw_frame(config.width, config.height)).await.unwrap();
        let packet = handle.recv_packet().await.expect("expected a packet");
        assert!(!packet.is_keyframe, "keyframe request should not affect frames after the one it targeted");
    }
}
