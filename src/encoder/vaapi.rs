//! Phase A (AGENTS.md) -- VAAPI hardware encode, the
//! "A-filtergraph" variant: `hwupload,scale_vaapi=format=nv12` does the
//! BGRA->NV12 colour conversion on the GPU, so the CPU only ever touches the
//! raw captured frame, never converts pixels itself. `h264_vaapi` then
//! encodes the resulting NV12 surfaces.
//!
//! Phase B.5 adds a second, zero-copy path for `CapturedFrame::Gpu`: instead
//! of uploading a CPU-side BGRA buffer, a client's dmabuf is mapped directly
//! into a VAAPI surface (`av_hwframe_map`, no pixel copy -- proven against
//! this exact dmabuf shape on real hardware, see
//! `tests::dmabuf_can_be_mapped_into_a_vaapi_surface`) and fed into the same
//! `scale_vaapi=format=nv12` GPU colour conversion `hwupload` would
//! otherwise feed. `EncoderConfig::gpu_frames` decides which of the two
//! pipelines this encoder builds -- never both, see `VaapiPipeline`.
//!
//! ffmpeg-next's safe wrapper has no VAAPI-specific accessors (see
//! AGENTS.md feasibility findings), so device
//! setup, the filtergraph's `hw_device_ctx` wiring, and pulling the
//! filter-derived `hw_frames_ctx` back out all go through `ffmpeg_next::ffi`
//! directly.

use anyhow::{Context as _, Result};
use ffmpeg_next as ffmpeg;
use ffmpeg_next::ffi;
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::allocator::Buffer as _;
use std::os::fd::AsRawFd;
use tracing::{error, info, warn};

use super::{
    create_input_frame, h264_codec_string, h264_level_option, select_h264_level, CapturedFrame,
    EncodedPacket, EncoderConfig, RateControl, RawFrame, VideoEncoder,
};

/// The two encode pipelines `VaapiEncoder` can drive, mutually exclusive for
/// the lifetime of one encoder (`EncoderConfig::gpu_frames` decides which at
/// construction, see `VaapiEncoder::new`) -- never both at once, since that
/// would mean two concurrent `h264_vaapi` hardware encode sessions for what
/// is logically a single output stream.
enum VaapiPipeline {
    /// Phase A: `buffer(bgra) -> hwupload -> scale_vaapi=nv12 -> buffersink`.
    /// `bgra_frame` is reused across `submit` calls the same way
    /// `X264Encoder::input_frame` is: never owns a buffer, just points at
    /// whichever `RawFrame` is being encoded right now. `av_buffersrc_add_frame`
    /// copies it synchronously (this frame isn't refcounted, so ffmpeg can't
    /// just take a reference), so repointing it next call is safe once this
    /// call returns.
    Cpu {
        graph: ffmpeg::filter::Graph,
        encoder: ffmpeg::encoder::Video,
        bgra_frame: ffmpeg::frame::Video,
    },
    /// Phase B.5: `buffer(vaapi, hw_frames_ctx=frames_ref) -> scale_vaapi=nv12
    /// -> buffersink` -- no `hwupload`, the frame is already on the GPU.
    /// `frames_ref` describes the *shape* every per-frame zero-copy mapped
    /// surface conforms to (format/sw_format/size); it isn't a real surface
    /// pool -- `av_hwframe_map` (see `encode_gpu_frame`) creates one actual
    /// VA surface per dmabuf import, wrapping that specific buffer, every
    /// time. Rebuilt alongside `graph`/`encoder` on resize/bitrate change,
    /// same as the `Cpu` variant.
    Gpu {
        frames_ref: *mut ffi::AVBufferRef,
        graph: ffmpeg::filter::Graph,
        encoder: ffmpeg::encoder::Video,
    },
}

impl Drop for VaapiPipeline {
    fn drop(&mut self) {
        // `graph`/`encoder` free themselves (and any refs they hold) via
        // their own Drop impls -- only `Gpu`'s `frames_ref` is a raw ref this
        // type owns directly.
        if let VaapiPipeline::Gpu { frames_ref, .. } = self {
            // SAFETY: `frames_ref` is the owned ref this variant holds (from
            // `av_hwframe_ctx_alloc`); it's still live here, freed exactly once
            // on drop, and null is tolerated by av_buffer_unref.
            unsafe { ffi::av_buffer_unref(frames_ref) };
        }
    }
}

/// VAAPI backend. Owns the device once; `pipeline` is rebuilt wholesale on
/// every resolution change (and bitrate change, since CBR's target is baked
/// into the encoder at open time) -- see `build_pipeline`/`build_gpu_pipeline`
/// for why `graph` and `encoder` have to be rebuilt as a pair.
pub struct VaapiEncoder {
    config: EncoderConfig,
    /// `av_hwdevice_ctx_create`'s output. Outlives every `pipeline` rebuild
    /// (the device doesn't depend on resolution); unreffed in `Drop`.
    device_ref: *mut ffi::AVBufferRef,
    pipeline: VaapiPipeline,
    frame_count: i64,
    next_frame_id: u32,
    buffer_return_tx: std::sync::mpsc::Sender<Vec<u8>>,
}

impl VaapiEncoder {
    pub fn new(config: EncoderConfig, buffer_return_tx: std::sync::mpsc::Sender<Vec<u8>>) -> Result<Self> {
        let device_ref = create_device(&config.vaapi_device)?;
        let pipeline = build_pipeline_for(&config, device_ref)?;
        Ok(Self {
            config,
            device_ref,
            pipeline,
            frame_count: 0,
            next_frame_id: 0,
            buffer_return_tx,
        })
    }

    /// Feeds one BGRA frame through the `Cpu` filtergraph and drains
    /// whatever NV12 hardware frame(s) it produces into the encoder. Almost
    /// always exactly one frame in, one out, but the buffersink drain loop
    /// doesn't assume that -- see the comment in `build_pipeline` on why
    /// frames aren't reused (filter pool owns surface lifetime here, unlike
    /// `X264Encoder`'s `yuv_frame`).
    #[allow(clippy::too_many_arguments)] // threads the filtergraph + per-frame encode state
    fn encode_cpu_frame(
        graph: &mut ffmpeg::filter::Graph,
        encoder: &mut ffmpeg::encoder::Video,
        bgra_frame: &mut ffmpeg::frame::Video,
        raw_frame: &RawFrame,
        frame_count: i64,
        next_frame_id: &mut u32,
        force_keyframe: bool,
        capture_to_encode_ms: f64,
    ) -> Result<Vec<EncodedPacket>> {
        let expected_len = (bgra_frame.width() * bgra_frame.height() * 4) as usize;
        if raw_frame.data.len() < expected_len {
            anyhow::bail!(
                "raw frame buffer ({} bytes) too small for {}x{} BGRA ({} bytes expected)",
                raw_frame.data.len(),
                bgra_frame.width(),
                bgra_frame.height(),
                expected_len
            );
        }
        // SAFETY: `as_mut_ptr` returns the live, non-null AVFrame this owned
        // `bgra_frame` wraps; `raw_frame.data` was just length-checked above to
        // be at least `expected_len`, so pointing data[0] at it with the
        // matching stride stays in-bounds, and it outlives the synchronous
        // `av_buffersrc_add_frame` copy below.
        unsafe {
            let ptr = bgra_frame.as_mut_ptr();
            (*ptr).data[0] = raw_frame.data.as_ptr() as *mut u8;
            (*ptr).linesize[0] = (bgra_frame.width() * 4) as i32;
        }
        bgra_frame.set_pts(Some(frame_count));

        let mut in_ctx = graph
            .get("in")
            .context("vaapi filtergraph missing its \"in\" buffer source")?;
        in_ctx
            .source()
            .add(bgra_frame)
            .context("failed to feed frame into vaapi filtergraph")?;

        drain_filtergraph(graph, encoder, next_frame_id, force_keyframe, capture_to_encode_ms)
    }
}

impl VideoEncoder for VaapiEncoder {
    fn submit(&mut self, frame: CapturedFrame, capture_to_encode_ms: f64, force_keyframe: bool) -> Result<Vec<EncodedPacket>> {
        match (&mut self.pipeline, frame) {
            (VaapiPipeline::Cpu { graph, encoder, bgra_frame }, CapturedFrame::Cpu(raw_frame)) => {
                let result = Self::encode_cpu_frame(
                    graph,
                    encoder,
                    bgra_frame,
                    &raw_frame,
                    self.frame_count,
                    &mut self.next_frame_id,
                    force_keyframe,
                    capture_to_encode_ms,
                );
                // encode_cpu_frame only borrows raw_frame, so hand the
                // buffer back regardless of outcome (mirrors X264Encoder).
                let _ = self.buffer_return_tx.send(raw_frame.data);
                let packets = result?;
                self.frame_count += 1;
                Ok(packets)
            }
            (VaapiPipeline::Gpu { frames_ref, graph, encoder }, CapturedFrame::Gpu { dmabuf, width, height, .. }) => {
                let packets = encode_gpu_frame(
                    *frames_ref,
                    graph,
                    encoder,
                    &dmabuf,
                    width,
                    height,
                    &mut self.next_frame_id,
                    force_keyframe,
                    capture_to_encode_ms,
                )?;
                self.frame_count += 1;
                Ok(packets)
            }
            (VaapiPipeline::Cpu { .. }, CapturedFrame::Gpu { .. }) => anyhow::bail!(
                "vaapi encoder is configured for CPU frames (gl compositor not selected, or it \
                 fell back to sw) but received a GPU frame -- this is a bug in how the \
                 compositor/encoder backends were paired at startup"
            ),
            (VaapiPipeline::Gpu { .. }, CapturedFrame::Cpu(raw_frame)) => {
                // Hand the buffer back even though this is an error: the
                // render loop still needs it returned to reuse, regardless
                // of why this frame can't be encoded.
                let _ = self.buffer_return_tx.send(raw_frame.data);
                anyhow::bail!(
                    "vaapi encoder is configured for zero-copy GPU frames but received a CPU \
                     frame -- this is a bug in how the compositor/encoder backends were paired \
                     at startup"
                )
            }
        }
    }

    fn reinitialize(&mut self, width: u32, height: u32) -> Result<()> {
        self.config.width = width;
        self.config.height = height;

        self.pipeline = build_pipeline_for(&self.config, self.device_ref)?;
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

        match build_pipeline_for(&self.config, self.device_ref) {
            Ok(pipeline) => {
                self.pipeline = pipeline;
                // Restart the PTS clock for the fresh pipeline. The rebuilt
                // h264_vaapi session emits an IDR on its first frame regardless;
                // the counter doesn't drive IDR placement. h264_vaapi has no
                // in-place rate-control reconfig here either, so ChangeBitrate is
                // coalesced upstream (adaptive_bitrate.rs) to keep rebuilds rare.
                self.frame_count = 0;
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
        // `pipeline` frees itself (including the refs it holds) via its own
        // Drop impl when this struct is dropped -- only the device ref this
        // struct created directly needs unreffing here.
        // SAFETY: `device_ref` is the owned ref `create_device` produced; it's
        // still live, freed exactly once here on drop, no aliasing.
        unsafe {
            ffi::av_buffer_unref(&mut self.device_ref);
        }
    }
}

/// Builds whichever pipeline `config.gpu_frames` selects. Building only the
/// one actually needed (rather than both) matters beyond avoiding wasted
/// setup: each pipeline opens its own `h264_vaapi` hardware encode session,
/// and some VAAPI drivers cap how many of those can run concurrently.
fn build_pipeline_for(config: &EncoderConfig, device_ref: *mut ffi::AVBufferRef) -> Result<VaapiPipeline> {
    if config.gpu_frames {
        let (frames_ref, graph, encoder) = build_gpu_pipeline(config, device_ref)?;
        Ok(VaapiPipeline::Gpu { frames_ref, graph, encoder })
    } else {
        let (graph, encoder) = build_pipeline(config, device_ref)?;
        let bgra_frame = create_input_frame(config.width, config.height);
        Ok(VaapiPipeline::Cpu { graph, encoder, bgra_frame })
    }
}

/// Drains whatever NV12 hardware frame(s) `graph`'s buffersink has ready
/// into `encoder`, tagging the first/only one as a keyframe if requested.
/// Shared by both pipelines -- the only difference between them is how the
/// frame got *into* the graph (`hwupload` vs. a zero-copy `av_hwframe_map`),
/// not how packets come back out.
fn drain_filtergraph(
    graph: &mut ffmpeg::filter::Graph,
    encoder: &mut ffmpeg::encoder::Video,
    next_frame_id: &mut u32,
    force_keyframe: bool,
    capture_to_encode_ms: f64,
) -> Result<Vec<EncodedPacket>> {
    let mut out_ctx = graph
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
                // this backend (AGENTS.md).
                hw_frame.set_kind(if force_keyframe {
                    ffmpeg::picture::Type::I
                } else {
                    ffmpeg::picture::Type::None
                });
                encoder.send_frame(&hw_frame)?;
                packets.extend(super::drain_packets(encoder, next_frame_id, capture_to_encode_ms)?);
            }
            Err(ffmpeg::Error::Other { errno: ffmpeg::error::EAGAIN }) => break,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(packets)
}

/// Maps `dmabuf` directly into a VAAPI surface conforming to `frames_ref`
/// (`av_hwframe_map`, zero-copy -- proven on real hardware against this
/// exact dmabuf shape, see `tests::dmabuf_can_be_mapped_into_a_vaapi_surface`),
/// feeds the result through `graph`'s `scale_vaapi=format=nv12` GPU colour
/// conversion, and drains the encoder. No CPU pixel copy happens anywhere in
/// this path.
#[allow(clippy::too_many_arguments)] // threads the filtergraph + per-frame encode state
fn encode_gpu_frame(
    frames_ref: *mut ffi::AVBufferRef,
    graph: &mut ffmpeg::filter::Graph,
    encoder: &mut ffmpeg::encoder::Video,
    dmabuf: &Dmabuf,
    width: u32,
    height: u32,
    next_frame_id: &mut u32,
    force_keyframe: bool,
    capture_to_encode_ms: f64,
) -> Result<Vec<EncodedPacket>> {
    let src = drm_prime_frame_from_dmabuf(dmabuf, width, height)?;

    // SAFETY: av_frame_alloc has no preconditions and returns null on OOM,
    // which is checked immediately below.
    let mut mapped = unsafe { ffi::av_frame_alloc() };
    if mapped.is_null() {
        anyhow::bail!("av_frame_alloc failed for the mapped VAAPI frame");
    }
    // SAFETY: `mapped` was just null-checked, so these field writes are
    // in-bounds and exclusive; `frames_ref` is a live ref this encoder owns,
    // and the fresh av_buffer_ref of it is owned by `mapped` from here on.
    unsafe {
        (*mapped).hw_frames_ctx = ffi::av_buffer_ref(frames_ref);
        (*mapped).format = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI as i32;
    }

    // SAFETY: `mapped` is the just-allocated VAAPI dst frame; `src` is the
    // live DRM_PRIME frame `drm_prime_frame_from_dmabuf` returned, valid for
    // this borrow; both stay alive across the call.
    let map_ret = unsafe {
        ffi::av_hwframe_map(
            mapped,
            src.as_ptr() as *mut ffi::AVFrame,
            (ffi::AV_HWFRAME_MAP_READ as i32) | (ffi::AV_HWFRAME_MAP_DIRECT as i32),
        )
    };
    if map_ret < 0 {
        // SAFETY: `mapped` is the owned frame allocated above; av_hwframe_map
        // failed, so freeing it here (exactly once) is the right cleanup.
        unsafe { ffi::av_frame_free(&mut mapped) };
        anyhow::bail!("av_hwframe_map(DRM_PRIME -> VAAPI) failed: {}", ffmpeg::Error::from(map_ret));
    }
    // Safe wrapper takes ownership of `mapped` (frees it via Drop) -- same
    // type the Cpu path's `hw_frame` uses, so it can go through the
    // identical `Source::add`/buffersink drain code below.
    // SAFETY: `mapped` is a valid, mapped, non-null AVFrame that nothing else
    // owns; the wrapper takes sole ownership of it.
    let mapped_frame = unsafe { ffmpeg::frame::Video::wrap(mapped) };

    let mut in_ctx = graph
        .get("in_gpu")
        .context("vaapi gpu filtergraph missing its \"in_gpu\" buffer source")?;
    // `mapped_frame` is refcounted (av_hwframe_map populated its buf[]), so
    // av_buffersrc_add_frame takes ownership of the reference instead of
    // copying -- mirrors how the Cpu path's `hw_frame`s flow into the
    // encoder, just one filter stage earlier.
    in_ctx
        .source()
        .add(&mapped_frame)
        .context("failed to feed mapped VAAPI frame into vaapi gpu filtergraph")?;

    drain_filtergraph(graph, encoder, next_frame_id, force_keyframe, capture_to_encode_ms)
}

/// Owns everything a `AV_PIX_FMT_DRM_PRIME` source `AVFrame` needs to stay
/// valid for as long as ffmpeg might reference it: the descriptor itself
/// (`AVDRMFrameDescriptor`, pointed at by the frame's `data[0]`) and a clone
/// of `dmabuf` (keeping its plane fd open -- the descriptor only carries the
/// fd's numeric value, not a reference that would keep it alive on its own).
/// Freed via `av_buffer_create`'s callback (`free_drm_frame_owner`) once the
/// frame's last reference drops.
struct DrmFrameOwner {
    descriptor: Box<ffi::AVDRMFrameDescriptor>,
    _dmabuf: Dmabuf,
}

unsafe extern "C" fn free_drm_frame_owner(opaque: *mut std::ffi::c_void, _data: *mut u8) {
    // SAFETY: ffmpeg runs this callback exactly once, with the original
    // `Box<DrmFrameOwner>` pointer passed to av_buffer_create, so reclaiming
    // and dropping that box here frees it exactly once.
    drop(unsafe { Box::from_raw(opaque as *mut DrmFrameOwner) });
}

/// Builds an `AV_PIX_FMT_DRM_PRIME` frame describing `dmabuf` as-is -- no
/// pixel copy, just metadata (fd, stride, offset, modifier) wrapped the way
/// `av_hwframe_map` expects to find it. Single-plane only (every dmabuf
/// `GlCompositor` produces is Argb8888, never subsampled/multi-plane).
fn drm_prime_frame_from_dmabuf(dmabuf: &Dmabuf, width: u32, height: u32) -> Result<ffmpeg::frame::Video> {
    if dmabuf.num_planes() != 1 {
        anyhow::bail!(
            "expected a single-plane dmabuf, got {} planes -- GlCompositor should only ever \
             produce single-plane Argb8888 targets",
            dmabuf.num_planes()
        );
    }
    let format = dmabuf.format();
    let fd = dmabuf.handles().next().context("dmabuf has no plane fds")?;
    let offset = dmabuf.offsets().next().context("dmabuf has no plane offsets")?;
    let stride = dmabuf.strides().next().context("dmabuf has no plane strides")?;
    // dma_buf fds report their real backing size via fstat (the kernel sets
    // the file's size at creation) -- dup the fd (we don't own the original,
    // `dmabuf` does) just to query it without touching the dmabuf's actual
    // handle.
    let object_size = std::fs::File::from(
        fd.try_clone_to_owned()
            .context("failed to dup dmabuf fd to query its size")?,
    )
    .metadata()
    .map(|m| m.len() as usize)
    .unwrap_or(0);

    // Every `std::mem::zeroed()` below fills an unused trailing slot of a C
    // AVDRM* descriptor; see the per-block SAFETY notes.
    let mut owner = Box::new(DrmFrameOwner {
        descriptor: Box::new(ffi::AVDRMFrameDescriptor {
            nb_objects: 1,
            objects: [
                ffi::AVDRMObjectDescriptor {
                    fd: fd.as_raw_fd(),
                    size: object_size,
                    format_modifier: u64::from(format.modifier),
                },
                // SAFETY: AVDRMObjectDescriptor is C POD; all-zeroes is a valid
                // initial state, and nb_objects=1 so ffmpeg never reads this slot.
                unsafe { std::mem::zeroed() },
                // SAFETY: AVDRMObjectDescriptor is C POD; all-zeroes is a valid
                // initial state, and nb_objects=1 so ffmpeg never reads this slot.
                unsafe { std::mem::zeroed() },
                // SAFETY: AVDRMObjectDescriptor is C POD; all-zeroes is a valid
                // initial state, and nb_objects=1 so ffmpeg never reads this slot.
                unsafe { std::mem::zeroed() },
            ],
            nb_layers: 1,
            layers: [
                ffi::AVDRMLayerDescriptor {
                    format: format.code as u32,
                    nb_planes: 1,
                    planes: [
                        ffi::AVDRMPlaneDescriptor {
                            object_index: 0,
                            offset: offset as isize,
                            pitch: stride as isize,
                        },
                        // SAFETY: AVDRMPlaneDescriptor is C POD; all-zeroes is a
                        // valid initial state, and nb_planes=1 so ffmpeg never
                        // reads this slot.
                        unsafe { std::mem::zeroed() },
                        // SAFETY: AVDRMPlaneDescriptor is C POD; all-zeroes is a
                        // valid initial state, and nb_planes=1 so ffmpeg never
                        // reads this slot.
                        unsafe { std::mem::zeroed() },
                        // SAFETY: AVDRMPlaneDescriptor is C POD; all-zeroes is a
                        // valid initial state, and nb_planes=1 so ffmpeg never
                        // reads this slot.
                        unsafe { std::mem::zeroed() },
                    ],
                },
                // SAFETY: AVDRMLayerDescriptor is C POD; all-zeroes is a valid
                // initial state, and nb_layers=1 so ffmpeg never reads this slot.
                unsafe { std::mem::zeroed() },
                // SAFETY: AVDRMLayerDescriptor is C POD; all-zeroes is a valid
                // initial state, and nb_layers=1 so ffmpeg never reads this slot.
                unsafe { std::mem::zeroed() },
                // SAFETY: AVDRMLayerDescriptor is C POD; all-zeroes is a valid
                // initial state, and nb_layers=1 so ffmpeg never reads this slot.
                unsafe { std::mem::zeroed() },
            ],
        }),
        _dmabuf: dmabuf.clone(),
    });
    let descriptor_ptr: *mut ffi::AVDRMFrameDescriptor = &mut *owner.descriptor;

    // SAFETY: av_frame_alloc has no preconditions and returns null on OOM,
    // which is checked immediately below.
    let raw = unsafe { ffi::av_frame_alloc() };
    if raw.is_null() {
        anyhow::bail!("av_frame_alloc failed for the DRM_PRIME source frame");
    }
    // SAFETY: `raw` was just null-checked so the field writes are in-bounds and
    // exclusive; `descriptor_ptr` points into `owner`'s heap-boxed descriptor,
    // and av_buffer_create takes ownership of the `Box::into_raw(owner)` pointer
    // it'll later hand to free_drm_frame_owner.
    unsafe {
        (*raw).format = ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
        (*raw).width = width as i32;
        (*raw).height = height as i32;
        (*raw).data[0] = descriptor_ptr as *mut u8;
        // Hold the owner box pointer separately: `data[0]` points at the inner
        // descriptor allocation, but ffmpeg's opaque (and our failure reclaim)
        // must be the *outer* DrmFrameOwner box, so we can't recover it from
        // `data[0]` later.
        let owner_raw = Box::into_raw(owner);
        (*raw).buf[0] = ffi::av_buffer_create(
            descriptor_ptr as *mut u8,
            std::mem::size_of::<ffi::AVDRMFrameDescriptor>(),
            Some(free_drm_frame_owner),
            owner_raw as *mut std::ffi::c_void,
            0,
        );
        if (*raw).buf[0].is_null() {
            // av_buffer_create failed (allocation failure) before taking
            // ownership of `owner` -- reclaim and drop it ourselves so it
            // doesn't leak, since its callback will now never run.
            // SAFETY: `owner_raw` came from Box::into_raw just above and was not
            // consumed (av_buffer_create returned null), so from_raw reclaims
            // unique ownership exactly once. `raw` is a valid owned frame, freed
            // once here.
            drop(Box::from_raw(owner_raw));
            let mut raw = raw;
            ffi::av_frame_free(&mut raw);
            anyhow::bail!("av_buffer_create failed for the DRM_PRIME frame descriptor");
        }
    }

    // SAFETY: `raw` is a fully-populated, non-null DRM_PRIME AVFrame with a
    // valid buf[0]; the wrapper takes sole ownership and frees it via Drop.
    Ok(unsafe { ffmpeg::frame::Video::wrap(raw) })
}

/// Opens the VAAPI render node and returns an owned device reference
/// (`av_hwdevice_ctx_create`'s output buffer). Lives for the lifetime of the
/// `VaapiEncoder`; every `graph`/`encoder` rebuild takes a fresh
/// `av_buffer_ref` of it rather than reopening the device.
fn create_device(path: &str) -> Result<*mut ffi::AVBufferRef> {
    let device_path = std::ffi::CString::new(path).with_context(|| format!("invalid VAAPI device path {path:?}"))?;
    let mut device_ref: *mut ffi::AVBufferRef = std::ptr::null_mut();
    // SAFETY: `&mut device_ref` and the null-terminated `device_path` C string
    // are valid for the call; av_hwdevice_ctx_create writes the owned device
    // ref into `device_ref` on success (ret >= 0), checked below.
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

    finalize_vaapi_graph(graph, config, "")
}

/// Shared tail of `build_pipeline`/`build_gpu_pipeline`: validate the linked
/// graph, recover scale_vaapi's output hw_frames_ctx, and build the encoder
/// from it. `label` distinguishes the two graphs in error messages (`""` for
/// the CPU-upload graph, `"gpu "` for the zero-copy one). Split out so the
/// delicate "borrowed, never-unref" frames_ctx handling lives in one place.
fn finalize_vaapi_graph(
    mut graph: ffmpeg::filter::Graph,
    config: &EncoderConfig,
    label: &str,
) -> Result<(ffmpeg::filter::Graph, ffmpeg::encoder::Video)> {
    graph
        .validate()
        .with_context(|| format!("failed to configure vaapi {label}filtergraph"))?;

    let out_ctx = graph.get("out").with_context(|| {
        format!("vaapi {label}filtergraph missing its \"out\" buffersink after validation")
    })?;
    // scale_vaapi derives its own output hw_frames_ctx from hw_device_ctx
    // during graph.validate() above; this is the *only* public way to get
    // it back out (AVFilterLink::hw_frames_ctx isn't part of the public ABI
    // in this ffmpeg version -- it moved to a private internal struct).
    // Borrowed, not owned -- see the comment in create_vaapi_encoder on why
    // this must never be unreffed directly.
    // SAFETY: `out_ctx.as_ptr()` is the live, validated buffersink filter
    // context; the call only reads its link's hw_frames_ctx and returns a
    // borrowed (not owned) pointer, null-checked below.
    let frames_ref = unsafe { ffi::av_buffersink_get_hw_frames_ctx(out_ctx.as_ptr()) };
    if frames_ref.is_null() {
        anyhow::bail!("scale_vaapi did not produce a hardware frames context");
    }

    let encoder = create_vaapi_encoder(config, frames_ref)?;

    Ok((graph, encoder))
}

/// Builds the zero-copy input side (Phase B.5): a `scale_vaapi=format=nv12`
/// filtergraph fed by VAAPI surfaces mapped directly from a client dmabuf --
/// `buffer(format=vaapi)` in place of `build_pipeline`'s `buffer(format=bgra)
/// -> hwupload` pair, since the frame is already on the GPU and needs
/// importing, not uploading. Returns the input-side frames context alongside
/// the graph/encoder so `VaapiPipeline::Gpu` can keep it alive (every mapped
/// frame references it, see `encode_gpu_frame`) and free it on drop.
fn build_gpu_pipeline(
    config: &EncoderConfig,
    device_ref: *mut ffi::AVBufferRef,
) -> Result<(*mut ffi::AVBufferRef, ffmpeg::filter::Graph, ffmpeg::encoder::Video)> {
    // Describes the *shape* every per-frame zero-copy mapped surface
    // conforms to -- not a pool. `av_hwframe_map` creates one real VA
    // surface per dmabuf import (see `encode_gpu_frame`); every mapped
    // frame just references this same frames context as its declared type.
    // SAFETY: `device_ref` is a live owned device ref; av_hwframe_ctx_alloc
    // returns a new owned ref or null (checked below).
    let input_frames_ref = unsafe { ffi::av_hwframe_ctx_alloc(device_ref) };
    if input_frames_ref.is_null() {
        anyhow::bail!("av_hwframe_ctx_alloc failed for the GPU input frames context");
    }
    // SAFETY: `input_frames_ref` was just null-checked; its `data` points at a
    // freshly-allocated, not-yet-initialized AVHWFramesContext, so these field
    // writes (before av_hwframe_ctx_init) are in-bounds and exclusive.
    unsafe {
        let ctx = (*input_frames_ref).data as *mut ffi::AVHWFramesContext;
        (*ctx).format = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
        // Matches the byte order GlCompositor's Argb8888 GBM targets are
        // already treated as elsewhere (create_input_frame, B.3's real-
        // hardware byte-order check).
        (*ctx).sw_format = ffi::AVPixelFormat::AV_PIX_FMT_BGRA;
        (*ctx).width = config.width as i32;
        (*ctx).height = config.height as i32;
    }
    // SAFETY: `input_frames_ref` is the live, now-configured frames ctx ref.
    let init_ret = unsafe { ffi::av_hwframe_ctx_init(input_frames_ref) };
    if init_ret < 0 {
        let mut input_frames_ref = input_frames_ref;
        // SAFETY: `input_frames_ref` is the owned ref allocated above; init
        // failed, so free it exactly once here before bailing.
        unsafe { ffi::av_buffer_unref(&mut input_frames_ref) };
        anyhow::bail!("av_hwframe_ctx_init failed: {}", ffmpeg::Error::from(init_ret));
    }

    let mut graph = ffmpeg::filter::Graph::new();
    let buffer_filter = ffmpeg::filter::find("buffer").context("\"buffer\" filter not registered")?;
    let scale_vaapi_filter = ffmpeg::filter::find("scale_vaapi").context("\"scale_vaapi\" filter not registered")?;
    let buffersink_filter = ffmpeg::filter::find("buffersink").context("\"buffersink\" filter not registered")?;

    // **Real bug found on hardware, not just theoretical**: `buffer`'s own
    // init() callback checks `hw_frames_ctx` immediately when `pix_fmt` is a
    // hw format ("Setting BufferSourceContext.pix_fmt to a HW format
    // requires hw_frames_ctx to be non-NULL!"), so going through
    // `Graph::add` with a `pix_fmt=vaapi` args string (which initializes on
    // return) fails before `av_buffersrc_parameters_set` ever runs -- the
    // exact same "alloc before init" problem `hwupload`/`scale_vaapi` have
    // (see `alloc_hw_filter`), just solved differently here since the field
    // that needs setting pre-init is `hw_frames_ctx`, set via
    // `AVBufferSrcParameters`, not `hw_device_ctx` set directly on the
    // context. `av_buffersrc_parameters_set` itself works before init
    // (that's its documented purpose: configure a buffersrc instead of an
    // options string) and takes its own `av_buffer_ref` of what we pass, so
    // `input_frames_ref` itself is only borrowed here.
    let cname = std::ffi::CString::new("in_gpu").unwrap();
    // SAFETY: `graph.as_mut_ptr()` and `buffer_filter.as_ptr()` are live, and
    // `cname` is a valid null-terminated C string outliving the call; returns
    // a filter context owned by the graph, or null (checked below).
    let in_ctx_ptr = unsafe { ffi::avfilter_graph_alloc_filter(graph.as_mut_ptr(), buffer_filter.as_ptr(), cname.as_ptr()) };
    if in_ctx_ptr.is_null() {
        anyhow::bail!("avfilter_graph_alloc_filter(\"in_gpu\") failed");
    }

    // SAFETY: av_buffersrc_parameters_alloc has no preconditions; returns an
    // owned, av_malloc'd struct or null (checked below).
    let params = unsafe { ffi::av_buffersrc_parameters_alloc() };
    if params.is_null() {
        anyhow::bail!("av_buffersrc_parameters_alloc failed");
    }
    // SAFETY: `params` was just null-checked, so the field writes are in-bounds
    // and exclusive; `input_frames_ref` is a live ref that av_buffersrc_parameters_set
    // below takes its own av_buffer_ref of (so it stays borrowed here).
    unsafe {
        (*params).format = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI as i32;
        (*params).width = config.width as i32;
        (*params).height = config.height as i32;
        (*params).time_base = ffi::AVRational { num: 1, den: config.framerate as i32 };
        (*params).hw_frames_ctx = input_frames_ref;
    }
    // SAFETY: `in_ctx_ptr` is the live, not-yet-initialized buffer filter
    // context; `params` is the valid struct populated just above.
    let params_ret = unsafe { ffi::av_buffersrc_parameters_set(in_ctx_ptr, params) };
    // SAFETY: `params` was allocated by av_buffersrc_parameters_alloc and is no
    // longer needed after the set call copied what it needs; freed once here.
    unsafe { ffi::av_free(params as *mut std::ffi::c_void) };
    if params_ret < 0 {
        anyhow::bail!("av_buffersrc_parameters_set failed: {}", ffmpeg::Error::from(params_ret));
    }

    // SAFETY: `in_ctx_ptr` is the live filter context, configured via params
    // above; initializing it with no options dict is valid here.
    let init_ret = unsafe { ffi::avfilter_init_dict(in_ctx_ptr, std::ptr::null_mut()) };
    if init_ret < 0 {
        anyhow::bail!("avfilter_init_dict(\"in_gpu\") failed: {}", ffmpeg::Error::from(init_ret));
    }
    // SAFETY: `in_ctx_ptr` is a live, now-initialized filter context owned by
    // `graph`; wrapping borrows it for the graph's lifetime.
    let mut in_ctx = unsafe { ffmpeg::filter::Context::wrap(in_ctx_ptr) };

    let mut out_ctx = graph
        .add(&buffersink_filter, "out", "")
        .context("failed to add buffersink to vaapi gpu filtergraph")?;
    // scale_vaapi still needs hw_device_ctx set before its own init() --
    // same requirement and same split-alloc/init fix as build_pipeline.
    let mut scale_ctx = alloc_hw_filter(&mut graph, &scale_vaapi_filter, "scale_vaapi", device_ref, Some(("format", "nv12")))
        .context("failed to initialize scale_vaapi filter")?;

    in_ctx.link(0, &mut scale_ctx, 0);
    scale_ctx.link(0, &mut out_ctx, 0);

    let (graph, encoder) = finalize_vaapi_graph(graph, config, "gpu ")?;
    Ok((input_frames_ref, graph, encoder))
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
    // SAFETY: `graph.as_mut_ptr()` and `filter.as_ptr()` are live, and `cname`
    // is a valid null-terminated C string outliving the call; returns a filter
    // context owned by the graph, or null (checked below).
    let ctx_ptr = unsafe { ffi::avfilter_graph_alloc_filter(graph.as_mut_ptr(), filter.as_ptr(), cname.as_ptr()) };
    if ctx_ptr.is_null() {
        anyhow::bail!("avfilter_graph_alloc_filter({name:?}) failed");
    }
    // SAFETY: `ctx_ptr` was just null-checked (live, not-yet-initialized filter
    // context); `device_ref` is a live owned ref, and the fresh av_buffer_ref
    // of it becomes owned by the filter context.
    unsafe {
        (*ctx_ptr).hw_device_ctx = ffi::av_buffer_ref(device_ref);
    }

    let ret = match option {
        // SAFETY: `ctx_ptr` is the live filter context; initializing with no
        // options dict is valid.
        None => unsafe { ffi::avfilter_init_dict(ctx_ptr, std::ptr::null_mut()) },
        Some((key, value)) => {
            let mut dict = ffmpeg::Dictionary::new();
            dict.set(key, value);
            // SAFETY: hands the dict's owned raw AVDictionary pointer out so it
            // can be passed by `&mut` to avfilter_init_dict below; ownership is
            // reclaimed via Dictionary::own afterwards.
            let mut raw = unsafe { dict.disown() };
            // SAFETY: `ctx_ptr` is the live filter context; `&mut raw` is a
            // valid AVDictionary pointer that the call may consume/modify.
            let ret = unsafe { ffi::avfilter_init_dict(ctx_ptr, &mut raw) };
            // Frees any options avfilter_init_dict left unconsumed, mirroring
            // create_encoder/open_with's Dictionary::own/disown round trip.
            // SAFETY: `raw` is the (possibly reduced) AVDictionary the init
            // call left behind; re-owning it frees any unconsumed options once.
            unsafe { ffmpeg::Dictionary::own(raw) };
            ret
        }
    };
    if ret < 0 {
        anyhow::bail!("avfilter_init_dict({name:?}) failed: {}", ffmpeg::Error::from(ret));
    }

    // SAFETY: `ctx_ptr` is a live, now-initialized filter context owned by
    // `graph`; wrapping borrows it for the graph's lifetime.
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
    // SAFETY: `encoder.as_mut_ptr()` is the live, not-yet-opened encoder
    // context; `frames_ref` is the live (borrowed) frames ctx, and the fresh
    // av_buffer_ref of it becomes owned by the encoder (unreffed on its drop).
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

    fn vaapi_available() -> bool {
        if std::fs::File::options()
            .read(true)
            .write(true)
            .open("/dev/dri/renderD128")
            .is_err()
        {
            return false;
        }
        ffmpeg::init().ok();
        // Opening the device is necessary but not sufficient: a render node
        // can be perfectly valid for GL/compositing yet have no H.264 encode
        // profile exposed to VAAPI, in which case `h264_vaapi` opens fine
        // and then immediately fails with "No usable encoding profile found"
        // (the actual symptom on hardware that can composite but not HW-
        // encode). The skip probe has to drive the full encoder-open path,
        // not just av_hwdevice_ctx_create, or the tests that need encode
        // support bail out at `VaapiEncoder::new` instead of being skipped.
        let mut device_ref = match create_device("/dev/dri/renderD128") {
            Ok(dev) => dev,
            Err(_) => return false,
        };

        // SAFETY: `device_ref` is a live owned device ref; returns a new owned
        // frames ctx ref or null (checked below).
        let frames_ref = unsafe { ffi::av_hwframe_ctx_alloc(device_ref) };
        if frames_ref.is_null() {
            // SAFETY: `device_ref` is the owned ref from create_device; free it
            // once before returning.
            unsafe { ffi::av_buffer_unref(&mut device_ref) };
            return false;
        }
        // SAFETY: `frames_ref` was just null-checked; its `data` points at a
        // freshly-allocated, not-yet-initialized AVHWFramesContext, so these
        // writes (before init) are in-bounds and exclusive.
        unsafe {
            let ctx = (*frames_ref).data as *mut ffi::AVHWFramesContext;
            (*ctx).format = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
            (*ctx).sw_format = ffi::AVPixelFormat::AV_PIX_FMT_NV12;
            (*ctx).width = TEST_SIZE as i32;
            (*ctx).height = TEST_SIZE as i32;
        }
        // SAFETY: `frames_ref` is the live, now-configured frames ctx ref.
        let init_ret = unsafe { ffi::av_hwframe_ctx_init(frames_ref) };
        if init_ret < 0 {
            let mut frames_ref = frames_ref;
            // SAFETY: `frames_ref` is the owned ref allocated above; free once.
            unsafe { ffi::av_buffer_unref(&mut frames_ref) };
            // SAFETY: `device_ref` is the owned ref from create_device; free once.
            unsafe { ffi::av_buffer_unref(&mut device_ref) };
            return false;
        }

        let probe_config = EncoderConfig {
            width: TEST_SIZE,
            height: TEST_SIZE,
            framerate: 30,
            rate_control: RateControl::Bitrate(500_000),
            keyframe_interval: 60,
            encoder_backend: super::super::EncoderBackend::Vaapi,
            vaapi_device: "/dev/dri/renderD128".to_string(),
            gpu_frames: false,
        };
        let result = create_vaapi_encoder(&probe_config, frames_ref).is_ok();

        let mut frames_ref = frames_ref;
        // SAFETY: `frames_ref` is an owned ref; the encoder (if created) took
        // its own ref of it, so freeing this one once here is correct.
        unsafe { ffi::av_buffer_unref(&mut frames_ref) };
        // SAFETY: `device_ref` is the owned ref from create_device; free once.
        unsafe { ffi::av_buffer_unref(&mut device_ref) };
        result
    }

    macro_rules! require_vaapi {
        () => {
            if !vaapi_available() {
                eprintln!("skipping: no VAAPI-capable render node with H264 encode support at /dev/dri/renderD128");
                return;
            }
        };
    }

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
            gpu_frames: false,
        };
        VaapiEncoder::new(config, tx).expect("failed to construct VaapiEncoder")
    }

    fn make_raw_frame_sized(width: u32, height: u32) -> RawFrame {
        RawFrame {
            data: vec![0u8; (width * height * 4) as usize],
            width,
            height,
            capture_instant: std::time::Instant::now(),
            damage: Vec::new(),
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

    fn make_gpu_encoder(tx: std::sync::mpsc::Sender<Vec<u8>>, keyframe_interval: u32) -> VaapiEncoder {
        let config = EncoderConfig {
            width: TEST_SIZE,
            height: TEST_SIZE,
            framerate: 30,
            rate_control: RateControl::Bitrate(500_000),
            keyframe_interval,
            encoder_backend: super::super::EncoderBackend::Vaapi,
            vaapi_device: "/dev/dri/renderD128".to_string(),
            gpu_frames: true,
        };
        VaapiEncoder::new(config, tx).expect("failed to construct VaapiEncoder")
    }

    /// Allocates a dmabuf the same way `GlCompositor`'s GBM target pool does
    /// (Argb8888, `Modifier::Invalid`, see `GlCompositor::ensure_sized`).
    fn make_gpu_dmabuf(width: u32, height: u32) -> Dmabuf {
        use smithay::backend::allocator::{
            dmabuf::AsDmabuf,
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
            Allocator, Fourcc, Modifier,
        };

        let file = std::fs::File::options()
            .read(true)
            .write(true)
            .open("/dev/dri/renderD128")
            .expect("failed to open render node");
        let gbm = GbmDevice::new(file).expect("failed to create GBM device");
        let mut allocator = GbmAllocator::new(gbm, GbmBufferFlags::RENDERING);
        let buffer = allocator
            .create_buffer(width, height, Fourcc::Argb8888, &[Modifier::Invalid])
            .expect("failed to allocate a GBM buffer");
        buffer.export().expect("failed to export GBM buffer as a dmabuf")
    }

    fn submit_gpu(encoder: &mut VaapiEncoder, dmabuf: Dmabuf, force_keyframe: bool) -> Vec<EncodedPacket> {
        encoder
            .submit(
                CapturedFrame::Gpu {
                    dmabuf,
                    width: TEST_SIZE,
                    height: TEST_SIZE,
                    capture_instant: std::time::Instant::now(),
                },
                0.0,
                force_keyframe,
            )
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
    /// Skipped gracefully when no VAAPI-capable render node is available
    /// (requires `vainfo` showing H264 EncSlice).
    #[test]
    fn construct_submit_drop_does_not_corrupt_heap() {
        require_vaapi!();
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
    fn force_keyframe_actually_forces_an_idr() {
        require_vaapi!();
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
    fn resize_reinitializes_encoder_without_corruption() {
        require_vaapi!();
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

    /// **Feasibility spike for AGENTS.md**
    /// (zero-copy dmabuf -> VAAPI import). Deliberately bypasses
    /// `GlCompositor`/`CapturedFrame::Gpu` entirely -- allocates a dmabuf the
    /// same way `GlCompositor`'s GBM target pool does (Argb8888,
    /// `Modifier::Invalid`, see `GlCompositor::ensure_sized`) and asks this
    /// driver to derive a VAAPI surface directly from it via
    /// `av_hwframe_map`. Isolates "does this GPU/driver accept the dmabuf
    /// shape the GL compositor actually produces" from every other moving
    /// part (shared fallback flag, `GlCompositor` producing `Gpu` frames,
    /// `scale_vaapi` wiring) before investing in the full integration.
    #[test]
    fn dmabuf_can_be_mapped_into_a_vaapi_surface() {
        require_vaapi!();
        use smithay::backend::allocator::{
            dmabuf::AsDmabuf,
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
            Allocator, Buffer, Fourcc, Modifier,
        };
        use std::fs::File;
        use std::os::fd::AsRawFd;

        ffmpeg::init().unwrap();

        let device_path = "/dev/dri/renderD128";
        let file = File::options()
            .read(true)
            .write(true)
            .open(device_path)
            .expect("failed to open render node");
        let gbm = GbmDevice::new(file).expect("failed to create GBM device");
        let mut allocator = GbmAllocator::new(gbm, GbmBufferFlags::RENDERING);
        let buffer = allocator
            .create_buffer(TEST_SIZE, TEST_SIZE, Fourcc::Argb8888, &[Modifier::Invalid])
            .expect("failed to allocate a GBM buffer");
        let dmabuf = buffer.export().expect("failed to export GBM buffer as a dmabuf");
        assert_eq!(dmabuf.num_planes(), 1, "single-plane Argb8888 should have exactly one plane");

        let mut device_ref = create_device(device_path).expect("failed to create VAAPI device");

        // --- Describe the dmabuf as an AV_PIX_FMT_DRM_PRIME source frame ---
        let format = dmabuf.format();
        let fd = dmabuf.handles().next().expect("dmabuf has no plane fds");
        let offset = dmabuf.offsets().next().unwrap();
        let stride = dmabuf.strides().next().unwrap();
        // dma_buf fds report their real backing size via fstat (the kernel
        // sets the file's size at creation) -- dup the fd (we don't own the
        // original, `dmabuf` does) just to query it without touching the
        // dmabuf's actual handle.
        let object_size = File::from(fd.try_clone_to_owned().expect("failed to dup dmabuf fd"))
            .metadata()
            .map(|m| m.len() as usize)
            .unwrap_or(0);

        // Every `std::mem::zeroed()` below fills an unused trailing slot of a C
        // AVDRM* descriptor; see the per-block SAFETY notes.
        let descriptor = Box::into_raw(Box::new(ffi::AVDRMFrameDescriptor {
            nb_objects: 1,
            objects: [
                ffi::AVDRMObjectDescriptor {
                    fd: fd.as_raw_fd(),
                    size: object_size,
                    format_modifier: u64::from(format.modifier),
                },
                // SAFETY: AVDRMObjectDescriptor is C POD; all-zeroes is valid and
                // nb_objects=1 means ffmpeg never reads this slot.
                unsafe { std::mem::zeroed() },
                // SAFETY: AVDRMObjectDescriptor is C POD; all-zeroes is valid and
                // nb_objects=1 means ffmpeg never reads this slot.
                unsafe { std::mem::zeroed() },
                // SAFETY: AVDRMObjectDescriptor is C POD; all-zeroes is valid and
                // nb_objects=1 means ffmpeg never reads this slot.
                unsafe { std::mem::zeroed() },
            ],
            nb_layers: 1,
            layers: [
                ffi::AVDRMLayerDescriptor {
                    format: format.code as u32,
                    nb_planes: 1,
                    planes: [
                        ffi::AVDRMPlaneDescriptor {
                            object_index: 0,
                            offset: offset as isize,
                            pitch: stride as isize,
                        },
                        // SAFETY: AVDRMPlaneDescriptor is C POD; all-zeroes is
                        // valid and nb_planes=1 means ffmpeg never reads this slot.
                        unsafe { std::mem::zeroed() },
                        // SAFETY: AVDRMPlaneDescriptor is C POD; all-zeroes is
                        // valid and nb_planes=1 means ffmpeg never reads this slot.
                        unsafe { std::mem::zeroed() },
                        // SAFETY: AVDRMPlaneDescriptor is C POD; all-zeroes is
                        // valid and nb_planes=1 means ffmpeg never reads this slot.
                        unsafe { std::mem::zeroed() },
                    ],
                },
                // SAFETY: AVDRMLayerDescriptor is C POD; all-zeroes is valid and
                // nb_layers=1 means ffmpeg never reads this slot.
                unsafe { std::mem::zeroed() },
                // SAFETY: AVDRMLayerDescriptor is C POD; all-zeroes is valid and
                // nb_layers=1 means ffmpeg never reads this slot.
                unsafe { std::mem::zeroed() },
                // SAFETY: AVDRMLayerDescriptor is C POD; all-zeroes is valid and
                // nb_layers=1 means ffmpeg never reads this slot.
                unsafe { std::mem::zeroed() },
            ],
        }));

        // Keeps `dmabuf` (and so the plane fd the descriptor above points at)
        // alive for as long as ffmpeg might still reference it; freed via
        // av_buffer_create's callback below, alongside the descriptor box.
        struct DrmFrameOwner {
            descriptor: *mut ffi::AVDRMFrameDescriptor,
            _dmabuf: smithay::backend::allocator::dmabuf::Dmabuf,
        }
        unsafe extern "C" fn free_drm_frame_owner(opaque: *mut std::ffi::c_void, _data: *mut u8) {
            // SAFETY: ffmpeg runs this callback exactly once with the original
            // `Box<DrmFrameOwner>` pointer passed to av_buffer_create, so
            // reclaiming that box here frees it exactly once.
            let owner = unsafe { Box::from_raw(opaque as *mut DrmFrameOwner) };
            // SAFETY: `owner.descriptor` is the raw pointer from Box::into_raw
            // (the descriptor box) stored when the owner was built; reclaim and
            // drop it exactly once here.
            unsafe { drop(Box::from_raw(owner.descriptor)) };
        }
        let owner_ptr = Box::into_raw(Box::new(DrmFrameOwner {
            descriptor,
            _dmabuf: dmabuf,
        })) as *mut std::ffi::c_void;

        // SAFETY: av_frame_alloc has no preconditions; returns null on OOM,
        // asserted non-null below.
        let mut src = unsafe { ffi::av_frame_alloc() };
        assert!(!src.is_null(), "av_frame_alloc returned null");
        // SAFETY: `src` was just asserted non-null, so the field writes are
        // in-bounds and exclusive; `descriptor` points at the leaked descriptor
        // box, and av_buffer_create takes ownership of `owner_ptr` (the leaked
        // DrmFrameOwner box) for free_drm_frame_owner.
        unsafe {
            (*src).format = ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
            (*src).width = TEST_SIZE as i32;
            (*src).height = TEST_SIZE as i32;
            (*src).data[0] = descriptor as *mut u8;
            (*src).buf[0] = ffi::av_buffer_create(
                descriptor as *mut u8,
                std::mem::size_of::<ffi::AVDRMFrameDescriptor>(),
                Some(free_drm_frame_owner),
                owner_ptr,
                0,
            );
            assert!(!(*src).buf[0].is_null(), "av_buffer_create returned null");
        }

        // --- Build the destination VAAPI-format frame ---
        // SAFETY: `device_ref` is a live owned device ref; returns a new owned
        // frames ctx ref or null, asserted below.
        let mut vaapi_frames_ref = unsafe { ffi::av_hwframe_ctx_alloc(device_ref) };
        assert!(!vaapi_frames_ref.is_null(), "av_hwframe_ctx_alloc returned null");
        // SAFETY: `vaapi_frames_ref` was just asserted non-null; its `data`
        // points at a fresh, not-yet-initialized AVHWFramesContext, so these
        // pre-init writes are in-bounds and exclusive.
        unsafe {
            let ctx = (*vaapi_frames_ref).data as *mut ffi::AVHWFramesContext;
            (*ctx).format = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
            // Matches the byte order `GlCompositor`'s Argb8888 GBM targets
            // are already treated as elsewhere in this codebase (see
            // `create_input_frame`/B.3's real-hardware byte-order check).
            (*ctx).sw_format = ffi::AVPixelFormat::AV_PIX_FMT_BGRA;
            (*ctx).width = TEST_SIZE as i32;
            (*ctx).height = TEST_SIZE as i32;
        }
        // SAFETY: `vaapi_frames_ref` is the live, now-configured frames ctx ref.
        let init_ret = unsafe { ffi::av_hwframe_ctx_init(vaapi_frames_ref) };
        assert!(
            init_ret >= 0,
            "av_hwframe_ctx_init failed: {}",
            ffmpeg::Error::from(init_ret)
        );

        // SAFETY: av_frame_alloc has no preconditions; returns null on OOM,
        // asserted non-null below.
        let mut dst = unsafe { ffi::av_frame_alloc() };
        assert!(!dst.is_null(), "av_frame_alloc returned null");
        // SAFETY: `dst` was just asserted non-null; `vaapi_frames_ref` is a live
        // ref, and the fresh av_buffer_ref of it becomes owned by `dst`.
        unsafe {
            (*dst).hw_frames_ctx = ffi::av_buffer_ref(vaapi_frames_ref);
            (*dst).format = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI as i32;
        }

        // SAFETY: `dst` is the just-built VAAPI dst frame and `src` the live
        // DRM_PRIME source frame; both stay alive across the call.
        let map_ret = unsafe {
            ffi::av_hwframe_map(
                dst,
                src,
                (ffi::AV_HWFRAME_MAP_READ as i32) | (ffi::AV_HWFRAME_MAP_DIRECT as i32),
            )
        };

        // SAFETY: `dst`/`src` are the owned frames allocated above and
        // `vaapi_frames_ref`/`device_ref` the owned refs; each is freed exactly
        // once here, no aliasing.
        unsafe {
            ffi::av_frame_free(&mut dst);
            ffi::av_frame_free(&mut src);
            ffi::av_buffer_unref(&mut vaapi_frames_ref);
            ffi::av_buffer_unref(&mut device_ref);
        }

        assert!(
            map_ret >= 0,
            "av_hwframe_map(DRM_PRIME -> VAAPI) failed: {} -- this driver does not support \
             zero-copy import of the dmabuf shape GlCompositor produces; Phase B.5 should ship \
             CPU-readback only",
            ffmpeg::Error::from(map_ret)
        );
        println!("dmabuf -> VAAPI surface zero-copy mapping succeeded");
    }

    /// End-to-end regression test for the Phase B.5 zero-copy path (mirrors
    /// `force_keyframe_actually_forces_an_idr`, just submitting
    /// `CapturedFrame::Gpu` instead of `Cpu`): every dmabuf goes through
    /// `encode_gpu_frame`'s `av_hwframe_map` -> `scale_vaapi=nv12` ->
    /// `h264_vaapi` pipeline with no CPU pixel copy, and forced keyframes
    /// still work the same way they do on the `Cpu` path (tagging the frame,
    /// not a `forced_idr` AVOption -- see that test's doc comment).
    #[test]
    fn gpu_frame_zero_copy_path_encodes_and_forces_keyframe() {
        require_vaapi!();
        ffmpeg::init().unwrap();
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut encoder = make_gpu_encoder(tx, 1000);

        let packets = submit_gpu(&mut encoder, make_gpu_dmabuf(TEST_SIZE, TEST_SIZE), false);
        assert!(
            packets.iter().any(|p| p.is_keyframe),
            "first frame of a GOP should be a keyframe"
        );

        for _ in 0..3 {
            let packets = submit_gpu(&mut encoder, make_gpu_dmabuf(TEST_SIZE, TEST_SIZE), false);
            assert!(
                packets.iter().all(|p| !p.is_keyframe),
                "frame without a keyframe request should not be an IDR"
            );
        }

        let packets = submit_gpu(&mut encoder, make_gpu_dmabuf(TEST_SIZE, TEST_SIZE), true);
        assert!(
            packets.iter().any(|p| p.is_keyframe),
            "force_keyframe should make this frame an IDR"
        );

        drop(encoder);
        println!("GPU zero-copy path encoded and dropped cleanly");
    }

    /// `GlCompositor`'s GBM target pool round-robins just 2 buffers (see
    /// `SizedState::targets`), so in real use the *same* dmabuf comes back
    /// around and gets mapped into a fresh VAAPI surface again every other
    /// frame. Confirms repeatedly re-mapping one dmabuf doesn't leak or
    /// crash (each `av_hwframe_map` call creates a new VA surface wrapping
    /// the same underlying buffer -- this is exercising that churn, not
    /// surface reuse).
    #[test]
    fn same_dmabuf_can_be_mapped_repeatedly() {
        require_vaapi!();
        ffmpeg::init().unwrap();
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut encoder = make_gpu_encoder(tx, 1000);

        let dmabuf = make_gpu_dmabuf(TEST_SIZE, TEST_SIZE);
        for i in 0..5 {
            let packets = submit_gpu(&mut encoder, dmabuf.clone(), false);
            assert!(
                i != 0 || packets.iter().any(|p| p.is_keyframe),
                "first frame of a GOP should be a keyframe"
            );
        }

        drop(encoder);
        println!("same dmabuf mapped repeatedly without leaking or crashing");
    }

    /// Mirrors `resize_reinitializes_encoder_without_corruption` for the
    /// Phase B.5 zero-copy path. `reinitialize` tears down and rebuilds the
    /// `Gpu` pipeline (including the input-side `frames_ref`) -- a resize
    /// that doesn't corrupt anything, followed by a normal frame at the new
    /// size, is the regression coverage for that rebuild path.
    #[test]
    fn gpu_path_resize_reinitializes_encoder_without_corruption() {
        require_vaapi!();
        ffmpeg::init().unwrap();
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut encoder = make_gpu_encoder(tx, 1000);

        let packets = submit_gpu(&mut encoder, make_gpu_dmabuf(TEST_SIZE, TEST_SIZE), false);
        assert!(packets.iter().any(|p| p.is_keyframe), "first frame of a GOP should be a keyframe");

        let (new_width, new_height) = (TEST_SIZE * 2, TEST_SIZE / 2);
        encoder.reinitialize(new_width, new_height).expect("reinitialize failed");
        assert_eq!(encoder.width(), new_width);
        assert_eq!(encoder.height(), new_height);

        let packets = encoder
            .submit(
                CapturedFrame::Gpu {
                    dmabuf: make_gpu_dmabuf(new_width, new_height),
                    width: new_width,
                    height: new_height,
                    capture_instant: std::time::Instant::now(),
                },
                0.0,
                false,
            )
            .expect("submit failed");
        assert!(
            packets.iter().any(|p| p.is_keyframe),
            "reinitializing should reset the GOP, so the first frame at the new size should be a keyframe"
        );

        drop(encoder);
        println!("GPU path resized, re-encoded, and dropped cleanly");
    }
}
