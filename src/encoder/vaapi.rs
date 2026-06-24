//! Phase A (hardware-acceleration-plan.md) -- VAAPI hardware encode, the
//! "A-filtergraph" variant: `hwupload,scale_vaapi=format=nv12` does the
//! BGRA->NV12 colour conversion on the GPU, so the CPU only ever touches the
//! raw captured frame, never converts pixels itself. `h264_vaapi` then
//! encodes the resulting NV12 surfaces.
//!
//! ffmpeg-next's safe wrapper has no VAAPI-specific accessors (see
//! docs/hardware-acceleration-plan.md's feasibility findings), so device
//! setup, the filtergraph's `hw_device_ctx` wiring, and pulling the
//! filter-derived `hw_frames_ctx` back out all go through `ffmpeg_next::ffi`
//! directly.

use anyhow::{Context as _, Result};
use ffmpeg_next as ffmpeg;
use ffmpeg_next::ffi;
use tracing::{error, info, warn};

use super::{
    create_input_frame, h264_codec_string, h264_level_option, select_h264_level, CapturedFrame,
    EncodedPacket, EncoderConfig, RateControl, RawFrame, VideoEncoder,
};

/// VAAPI backend. Owns the device once; `graph` + `encoder` are rebuilt
/// together on every resolution change (and bitrate change, since CBR's
/// target is baked into the encoder at open time) -- see `build_pipeline`
/// for why they have to be rebuilt as a pair.
pub struct VaapiEncoder {
    config: EncoderConfig,
    /// `av_hwdevice_ctx_create`'s output. Outlives every `graph`/`encoder`
    /// rebuild (the device doesn't depend on resolution); unreffed in
    /// `Drop`.
    device_ref: *mut ffi::AVBufferRef,
    graph: ffmpeg::filter::Graph,
    encoder: ffmpeg::encoder::Video,
    /// Reused across `submit` calls the same way `X264Encoder::input_frame`
    /// is: never owns a buffer, just points at whichever `RawFrame` is being
    /// encoded right now. `av_buffersrc_add_frame` copies it synchronously
    /// (this frame isn't refcounted, so ffmpeg can't just take a reference),
    /// so repointing it next call is safe once this call returns.
    bgra_frame: ffmpeg::frame::Video,
    frame_count: i64,
    next_frame_id: u32,
    buffer_return_tx: std::sync::mpsc::Sender<Vec<u8>>,
}

impl VaapiEncoder {
    pub fn new(config: EncoderConfig, buffer_return_tx: std::sync::mpsc::Sender<Vec<u8>>) -> Result<Self> {
        let device_ref = create_device(&config.vaapi_device)?;
        let (graph, encoder) = build_pipeline(&config, device_ref)?;
        let bgra_frame = create_input_frame(config.width, config.height);
        Ok(Self {
            config,
            device_ref,
            graph,
            encoder,
            bgra_frame,
            frame_count: 0,
            next_frame_id: 0,
            buffer_return_tx,
        })
    }

    /// Feeds one BGRA frame through the filtergraph and drains whatever NV12
    /// hardware frame(s) it produces into the encoder. Almost always exactly
    /// one frame in, one out, but the buffersink drain loop doesn't assume
    /// that -- see the comment in `build_pipeline` on why frames aren't
    /// reused (filter pool owns surface lifetime here, unlike `X264Encoder`'s
    /// `yuv_frame`).
    fn encode_frame(&mut self, raw_frame: &RawFrame, force_keyframe: bool, capture_to_encode_ms: f64) -> Result<Vec<EncodedPacket>> {
        let expected_len = (self.bgra_frame.width() * self.bgra_frame.height() * 4) as usize;
        if raw_frame.data.len() < expected_len {
            anyhow::bail!(
                "raw frame buffer ({} bytes) too small for {}x{} BGRA ({} bytes expected)",
                raw_frame.data.len(),
                self.bgra_frame.width(),
                self.bgra_frame.height(),
                expected_len
            );
        }
        unsafe {
            let ptr = self.bgra_frame.as_mut_ptr();
            (*ptr).data[0] = raw_frame.data.as_ptr() as *mut u8;
            (*ptr).linesize[0] = (self.bgra_frame.width() * 4) as i32;
        }
        self.bgra_frame.set_pts(Some(self.frame_count));

        let mut in_ctx = self
            .graph
            .get("in")
            .context("vaapi filtergraph missing its \"in\" buffer source")?;
        in_ctx
            .source()
            .add(&self.bgra_frame)
            .context("failed to feed frame into vaapi filtergraph")?;

        let mut out_ctx = self
            .graph
            .get("out")
            .context("vaapi filtergraph missing its \"out\" buffersink")?;

        let mut packets = Vec::new();
        loop {
            let mut hw_frame = ffmpeg::frame::Video::empty();
            match out_ctx.sink().frame(&mut hw_frame) {
                Ok(()) => {
                    // Forcing a keyframe out of h264_vaapi: this build's
                    // h264_vaapi has no `forced_idr` AVOption (checked via
                    // `ffmpeg -h encoder=h264_vaapi` on the target hardware --
                    // it only exists on some other hwaccel encoders), so
                    // tagging the frame is the only mechanism available.
                    // Verified by the keyframe regression test ported to
                    // this backend (hardware-acceleration-plan.md A.4).
                    hw_frame.set_kind(if force_keyframe {
                        ffmpeg::picture::Type::I
                    } else {
                        ffmpeg::picture::Type::None
                    });
                    self.encoder.send_frame(&hw_frame)?;
                    packets.extend(super::drain_packets(&mut self.encoder, &mut self.next_frame_id, capture_to_encode_ms)?);
                }
                Err(ffmpeg::Error::Other { errno: ffmpeg::error::EAGAIN }) => break,
                Err(e) => return Err(e.into()),
            }
        }
        Ok(packets)
    }
}

impl VideoEncoder for VaapiEncoder {
    fn submit(&mut self, frame: CapturedFrame, capture_to_encode_ms: f64, force_keyframe: bool) -> Result<Vec<EncodedPacket>> {
        let raw_frame = match frame {
            CapturedFrame::Cpu(raw_frame) => raw_frame,
            CapturedFrame::Gpu { .. } => {
                anyhow::bail!(
                    "vaapi backend doesn't accept GPU frames yet; zero-copy dmabuf import \
                     lands in hardware-acceleration-plan.md Phase B"
                );
            }
        };

        let result = self.encode_frame(&raw_frame, force_keyframe, capture_to_encode_ms);

        // Mirrors X264Encoder::submit: encode_frame only borrows raw_frame,
        // so hand the buffer back regardless of outcome.
        let _ = self.buffer_return_tx.send(raw_frame.data);

        let packets = result?;
        self.frame_count += 1;
        Ok(packets)
    }

    fn reinitialize(&mut self, width: u32, height: u32) -> Result<()> {
        self.config.width = width;
        self.config.height = height;

        let (graph, encoder) = build_pipeline(&self.config, self.device_ref)?;
        self.graph = graph;
        self.encoder = encoder;
        self.bgra_frame = create_input_frame(width, height);
        self.frame_count = 0;

        Ok(())
    }

    fn change_bitrate(&mut self, bitrate: usize) -> bool {
        if self.config.rate_control == RateControl::Bitrate(bitrate) {
            return false;
        }
        if !matches!(self.config.rate_control, RateControl::Bitrate(_)) {
            warn!("Ignoring bitrate change request: VAAPI encoder is in constant-quality mode");
            return false;
        }
        info!("Changing VAAPI bitrate from {:?} to {} bps", self.config.rate_control, bitrate);
        self.config.rate_control = RateControl::Bitrate(bitrate);

        match build_pipeline(&self.config, self.device_ref) {
            Ok((graph, encoder)) => {
                self.graph = graph;
                self.encoder = encoder;
                self.frame_count = 0; // Reset frame count to force IDR
                info!("VAAPI encoder reinitialized with new bitrate");
                true
            }
            Err(e) => {
                error!("Failed to reinitialize VAAPI encoder with new bitrate: {}", e);
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

impl Drop for VaapiEncoder {
    fn drop(&mut self) {
        // `graph`/`encoder` free themselves (and the device refs they hold)
        // via their own Drop impls when this struct is dropped -- only the
        // device ref this struct created directly needs unreffing here.
        unsafe {
            ffi::av_buffer_unref(&mut self.device_ref);
        }
    }
}

/// Opens the VAAPI render node and returns an owned device reference
/// (`av_hwdevice_ctx_create`'s output buffer). Lives for the lifetime of the
/// `VaapiEncoder`; every `graph`/`encoder` rebuild takes a fresh
/// `av_buffer_ref` of it rather than reopening the device.
fn create_device(path: &str) -> Result<*mut ffi::AVBufferRef> {
    let device_path = std::ffi::CString::new(path).with_context(|| format!("invalid VAAPI device path {path:?}"))?;
    let mut device_ref: *mut ffi::AVBufferRef = std::ptr::null_mut();
    let ret = unsafe {
        ffi::av_hwdevice_ctx_create(
            &mut device_ref,
            ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
            device_path.as_ptr(),
            std::ptr::null_mut(),
            0,
        )
    };
    if ret < 0 {
        anyhow::bail!("av_hwdevice_ctx_create({}) failed: {}", path, ffmpeg::Error::from(ret));
    }
    Ok(device_ref)
}

/// Builds the `hwupload,scale_vaapi=format=nv12` filtergraph and the
/// `h264_vaapi` encoder that consumes its output, for one resolution/rate
/// control. They're built together (not as two independent steps) because
/// the encoder's `hw_frames_ctx` has to be the exact frames context
/// `scale_vaapi` derives for its output -- see the call to
/// `av_buffersink_get_hw_frames_ctx` below. Rebuilt wholesale on every resize
/// and bitrate change, same as `X264Encoder::reinitialize`/`change_bitrate`
/// rebuild `encoder`/`scaler`.
fn build_pipeline(config: &EncoderConfig, device_ref: *mut ffi::AVBufferRef) -> Result<(ffmpeg::filter::Graph, ffmpeg::encoder::Video)> {
    let mut graph = ffmpeg::filter::Graph::new();

    let buffer_filter = ffmpeg::filter::find("buffer").context("\"buffer\" filter not registered")?;
    let hwupload_filter = ffmpeg::filter::find("hwupload").context("\"hwupload\" filter not registered")?;
    let scale_vaapi_filter = ffmpeg::filter::find("scale_vaapi").context("\"scale_vaapi\" filter not registered")?;
    let buffersink_filter = ffmpeg::filter::find("buffersink").context("\"buffersink\" filter not registered")?;

    let args = format!(
        "video_size={}x{}:pix_fmt=bgra:time_base=1/{}:pixel_aspect=1/1",
        config.width, config.height, config.framerate
    );
    let mut in_ctx = graph
        .add(&buffer_filter, "in", &args)
        .context("failed to add buffer source to vaapi filtergraph")?;
    let mut out_ctx = graph
        .add(&buffersink_filter, "out", "")
        .context("failed to add buffersink to vaapi filtergraph")?;

    // hwupload/scale_vaapi each check `hw_device_ctx` inside their own
    // init() callback (confirmed on real hardware: omitting this makes
    // hwupload fail immediately with "A hardware device reference is
    // required to upload frames to"), so it has to be set *before* they're
    // initialized. `Graph::add` (avfilter_graph_create_filter) initializes
    // immediately on return, which is too late. Per
    // avfilter_graph_create_filter's own doc comment, the fix is to split
    // alloc and init by hand -- see `alloc_hw_filter`.
    let mut hwupload_ctx = alloc_hw_filter(&mut graph, &hwupload_filter, "hwupload", device_ref, None)
        .context("failed to initialize hwupload filter")?;
    let mut scale_ctx = alloc_hw_filter(&mut graph, &scale_vaapi_filter, "scale_vaapi", device_ref, Some(("format", "nv12")))
        .context("failed to initialize scale_vaapi filter")?;

    in_ctx.link(0, &mut hwupload_ctx, 0);
    hwupload_ctx.link(0, &mut scale_ctx, 0);
    scale_ctx.link(0, &mut out_ctx, 0);

    graph.validate().context("failed to configure vaapi filtergraph")?;

    let out_ctx = graph
        .get("out")
        .context("vaapi filtergraph missing its \"out\" buffersink after validation")?;
    // scale_vaapi derives its own output hw_frames_ctx from hw_device_ctx
    // during graph.validate() above; this is the *only* public way to get
    // it back out (AVFilterLink::hw_frames_ctx isn't part of the public ABI
    // in this ffmpeg version -- it moved to a private internal struct).
    // Borrowed, not owned -- see the comment in create_vaapi_encoder on why
    // this must never be unreffed directly.
    let frames_ref = unsafe { ffi::av_buffersink_get_hw_frames_ctx(out_ctx.as_ptr()) };
    if frames_ref.is_null() {
        anyhow::bail!("scale_vaapi did not produce a hardware frames context");
    }

    let encoder = create_vaapi_encoder(config, frames_ref)?;

    Ok((graph, encoder))
}

/// Allocates a filter instance in `graph` without initializing it (unlike
/// `Graph::add`), sets `hw_device_ctx` on it, then initializes it -- the
/// ordering `avfilter_graph_create_filter`'s own doc comment recommends for
/// filters flagged `AVFILTER_FLAG_HWDEVICE`. `option` is the single
/// `key=value` the filter needs (e.g. scale_vaapi's `format=nv12`); `None`
/// for filters that take no options (hwupload).
fn alloc_hw_filter(
    graph: &mut ffmpeg::filter::Graph,
    filter: &ffmpeg::filter::Filter,
    name: &str,
    device_ref: *mut ffi::AVBufferRef,
    option: Option<(&str, &str)>,
) -> Result<ffmpeg::filter::Context> {
    let cname = std::ffi::CString::new(name).unwrap();
    let ctx_ptr = unsafe { ffi::avfilter_graph_alloc_filter(graph.as_mut_ptr(), filter.as_ptr(), cname.as_ptr()) };
    if ctx_ptr.is_null() {
        anyhow::bail!("avfilter_graph_alloc_filter({name:?}) failed");
    }
    unsafe {
        (*ctx_ptr).hw_device_ctx = ffi::av_buffer_ref(device_ref);
    }

    let ret = match option {
        None => unsafe { ffi::avfilter_init_dict(ctx_ptr, std::ptr::null_mut()) },
        Some((key, value)) => {
            let mut dict = ffmpeg::Dictionary::new();
            dict.set(key, value);
            let mut raw = unsafe { dict.disown() };
            let ret = unsafe { ffi::avfilter_init_dict(ctx_ptr, &mut raw) };
            // Frees any options avfilter_init_dict left unconsumed, mirroring
            // create_encoder/open_with's Dictionary::own/disown round trip.
            unsafe { ffmpeg::Dictionary::own(raw) };
            ret
        }
    };
    if ret < 0 {
        anyhow::bail!("avfilter_init_dict({name:?}) failed: {}", ffmpeg::Error::from(ret));
    }

    Ok(unsafe { ffmpeg::filter::Context::wrap(ctx_ptr) })
}

/// Creates and opens the `h264_vaapi` encoder against `frames_ref` (the
/// filtergraph's derived NV12 hardware frames context -- see `build_pipeline`
/// for why it must be that exact context and not a separately allocated
/// one).
fn create_vaapi_encoder(config: &EncoderConfig, frames_ref: *mut ffi::AVBufferRef) -> Result<ffmpeg::encoder::Video> {
    let codec = ffmpeg::encoder::find_by_name("h264_vaapi").context("h264_vaapi encoder not found")?;
    let mut encoder = ffmpeg::codec::context::Context::new_with_codec(codec).encoder().video()?;

    encoder.set_width(config.width);
    encoder.set_height(config.height);
    encoder.set_format(ffmpeg::format::Pixel::VAAPI);
    encoder.set_frame_rate(Some(ffmpeg::Rational::new(config.framerate as i32, 1)));
    encoder.set_time_base(ffmpeg::Rational::new(1, config.framerate as i32));
    encoder.set_gop(config.keyframe_interval);
    encoder.set_max_b_frames(0); // No B-frames for low latency, same as x264

    // av_buffersink_get_hw_frames_ctx (unlike its similarly-named sibling
    // avfilter_link_get_hw_frames_ctx, which does av_buffer_ref internally
    // and is genuinely owned) has no doc comment in upstream FFmpeg at all,
    // and its actual implementation (libavfilter/buffersink.c) just returns
    // the link's hw_frames_ctx pointer directly -- no ref taken. Treating it
    // as owned and unreffing it (verified on target hardware: see
    // vaapi::tests::graph_plus_frames_ref_no_encoder) corrupts whatever the
    // filtergraph still tracks internally for that link. So: take our own
    // fresh av_buffer_ref of it for the encoder (whose avcodec_free_context
    // will correctly unref *that* copy on drop) and never touch frames_ref
    // itself -- it stays borrowed, owned by the graph/link.
    unsafe {
        (*encoder.as_mut_ptr()).hw_frames_ctx = ffi::av_buffer_ref(frames_ref);
    }

    let mut opts = ffmpeg::Dictionary::new();
    opts.set("profile", "constrained_baseline");
    opts.set("level", &h264_level_option(select_h264_level(config.width, config.height, config.framerate)));
    opts.set("async_depth", "1");

    match config.rate_control {
        RateControl::Bitrate(bitrate) => {
            encoder.set_bit_rate(bitrate);
            opts.set("rc_mode", "CBR");
        }
        RateControl::Quality(crf) => {
            // VAAPI's "qp" is a constant QP, not x264's RD-optimized CRF --
            // similar intent (skip bitrate targeting, let scene complexity
            // drive frame size) but not the same scale or algorithm.
            opts.set("rc_mode", "CQP");
            opts.set("qp", &crf.to_string());
        }
    }

    let encoder = encoder.open_with(opts)?;

    info!(
        "VAAPI encoder initialized: {}x{} @ {}fps, {:?}",
        config.width, config.height, config.framerate, config.rate_control
    );

    Ok(encoder)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Some VAAPI drivers refuse to encode below a minimum size (e.g. one
    // tested GPU rejected 64x64 with "Hardware does not support encoding at
    // size 64x64 (constraints: width 128-4096 height 128-4096)") -- 256 is
    // comfortably above the minimums seen so far.
    const TEST_SIZE: u32 = 256;

    fn make_encoder(tx: std::sync::mpsc::Sender<Vec<u8>>, keyframe_interval: u32) -> VaapiEncoder {
        let config = EncoderConfig {
            width: TEST_SIZE,
            height: TEST_SIZE,
            framerate: 30,
            rate_control: RateControl::Bitrate(500_000),
            keyframe_interval,
            encoder_backend: super::super::EncoderBackend::Vaapi,
            vaapi_device: "/dev/dri/renderD128".to_string(),
        };
        VaapiEncoder::new(config, tx).expect("failed to construct VaapiEncoder")
    }

    fn make_raw_frame_sized(width: u32, height: u32) -> RawFrame {
        RawFrame {
            data: vec![0u8; (width * height * 4) as usize],
            width,
            height,
            capture_instant: std::time::Instant::now(),
        }
    }

    fn make_raw_frame() -> RawFrame {
        make_raw_frame_sized(TEST_SIZE, TEST_SIZE)
    }

    fn submit(encoder: &mut VaapiEncoder, force_keyframe: bool) -> Vec<EncodedPacket> {
        encoder
            .submit(CapturedFrame::Cpu(make_raw_frame()), 0.0, force_keyframe)
            .expect("submit failed")
    }

    fn submit_sized(encoder: &mut VaapiEncoder, width: u32, height: u32, force_keyframe: bool) -> Vec<EncodedPacket> {
        encoder
            .submit(CapturedFrame::Cpu(make_raw_frame_sized(width, height)), 0.0, force_keyframe)
            .expect("submit failed")
    }

    /// Regression test for a real heap-corruption bug found while validating
    /// Phase A on real hardware (no GPU locally, so this only runs on a
    /// machine with a VAAPI-capable render node):
    /// `av_buffersink_get_hw_frames_ctx` has no doc comment in upstream
    /// FFmpeg and, per its actual implementation
    /// (libavfilter/buffersink.c), returns the link's `hw_frames_ctx`
    /// pointer directly -- borrowed, not ref-counted, despite looking just
    /// like its documented sibling `avfilter_link_get_hw_frames_ctx` (which
    /// *does* `av_buffer_ref` and is genuinely owned). Treating its return
    /// value as owned (handing it straight to the encoder's `hw_frames_ctx`,
    /// which `avcodec_free_context` then unrefs on drop) corrupts whatever
    /// the filtergraph still tracks internally for that link, surfacing as a
    /// double-free/heap corruption abort later (sometimes not until process
    /// exit, since glibc detects it lazily). Fixed in `create_vaapi_encoder`
    /// by taking a fresh `av_buffer_ref` for the encoder instead of
    /// transferring the original.
    /// `#[ignore]`d by default since it needs a real VAAPI-capable render
    /// node (`vainfo` showing H264 EncSlice) -- run with
    /// `cargo test --lib vaapi:: -- --ignored` on hardware that has one.
    #[test]
    #[ignore = "needs a real VAAPI render node with H264 encode support"]
    fn construct_submit_drop_does_not_corrupt_heap() {
        ffmpeg::init().unwrap();
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut encoder = make_encoder(tx, 30);

        for _ in 0..3 {
            submit(&mut encoder, false);
        }

        drop(encoder);
        println!("dropped cleanly");
    }

    /// Mirrors `encoder::tests::force_keyframe_actually_forces_an_idr` (the
    /// x264 backend's test) for the VAAPI backend. Notable because the fix
    /// isn't the same: this build's `h264_vaapi` has no `forced_idr`
    /// AVOption at all (checked via `ffmpeg -h encoder=h264_vaapi` on this
    /// hardware -- it exists on some other hwaccel encoders but not this
    /// one), so tagging the frame `AV_PICTURE_TYPE_I` is the *only*
    /// mechanism available, not a belt-and-suspenders addition to it. This
    /// proves that alone is sufficient.
    #[test]
    #[ignore = "needs a real VAAPI render node with H264 encode support"]
    fn force_keyframe_actually_forces_an_idr() {
        ffmpeg::init().unwrap();
        let (tx, _rx) = std::sync::mpsc::channel();
        // Large enough that nothing here crosses a natural GOP boundary on
        // its own -- every keyframe observed must come from ForceKeyframe.
        let mut encoder = make_encoder(tx, 1000);

        let packets = submit(&mut encoder, false);
        assert!(
            packets.iter().any(|p| p.is_keyframe),
            "first frame of a GOP should be a keyframe"
        );

        for _ in 0..3 {
            let packets = submit(&mut encoder, false);
            assert!(
                packets.iter().all(|p| !p.is_keyframe),
                "frame without a keyframe request should not be an IDR"
            );
        }

        let packets = submit(&mut encoder, true);
        assert!(
            packets.iter().any(|p| p.is_keyframe),
            "force_keyframe should make this frame an IDR even with no forced_idr AVOption"
        );

        let packets = submit(&mut encoder, false);
        assert!(
            packets.iter().all(|p| !p.is_keyframe),
            "keyframe request should not affect frames after the one it targeted"
        );

        drop(encoder);
    }

    /// Mirrors `encoder::tests::frame_size_mismatch_reinitializes_encoder`
    /// (the x264 backend's test) for the VAAPI backend. `reinitialize` tears
    /// down and rebuilds both `graph` and `encoder` -- exactly the path the
    /// heap-corruption bug above (`construct_submit_drop_does_not_corrupt_heap`)
    /// lived in, just triggered by a resize instead of by construction+drop.
    /// A resize that doesn't also corrupt anything, followed by a normal
    /// frame at the new size, is the real regression coverage for that fix.
    #[test]
    #[ignore = "needs a real VAAPI render node with H264 encode support"]
    fn resize_reinitializes_encoder_without_corruption() {
        ffmpeg::init().unwrap();
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut encoder = make_encoder(tx, 1000);

        let packets = submit(&mut encoder, false);
        assert!(packets.iter().any(|p| p.is_keyframe), "first frame of a GOP should be a keyframe");

        // Different aspect ratio too, not just a different area -- mirrors
        // the x264 test's 1280x720 -> 800x592 resize.
        let (new_width, new_height) = (TEST_SIZE * 2, TEST_SIZE / 2);
        encoder.reinitialize(new_width, new_height).expect("reinitialize failed");
        assert_eq!(encoder.width(), new_width);
        assert_eq!(encoder.height(), new_height);

        let packets = submit_sized(&mut encoder, new_width, new_height, false);
        assert!(
            packets.iter().any(|p| p.is_keyframe),
            "reinitializing should reset the GOP, so the first frame at the new size should be a keyframe"
        );

        let packets = submit_sized(&mut encoder, new_width, new_height, false);
        assert!(
            packets.iter().all(|p| !p.is_keyframe),
            "a second frame at the same (new) size should encode normally, with no further reinitialization needed"
        );

        drop(encoder);
        println!("resized, re-encoded, and dropped cleanly");
    }
}
