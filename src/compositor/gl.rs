use anyhow::{Context, Result};
use smithay::{
    backend::{
        allocator::{
            dmabuf::{AsDmabuf, Dmabuf},
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
            Allocator, Fourcc, Modifier,
        },
        egl::{EGLContext, EGLDisplay},
        renderer::{
            damage::OutputDamageTracker, gles::GlesRenderer, Bind, Color32F, ExportMem,
        },
    },
    desktop::space::space_render_elements,
    utils::{Buffer as BufferCoord, Rectangle},
};
use std::cell::RefCell;
use std::fs::File;
use std::os::unix::fs::MetadataExt;
use std::rc::Rc;

use crate::encoder::{CapturedFrame, RawFrame};

use super::Compositor;
use super::state::WaylandWebStreamState;

/// GPU compositor (hardware-acceleration-plan.md Phase B): renders the
/// `Space` with smithay's `GlesRenderer` into an offscreen GBM/dmabuf target
/// instead of the manual SHM memcpy `WaylandWebStreamState::render` does.
/// What happens to that target then depends on `produce_gpu_frames`: read
/// back to the CPU as `CapturedFrame::Cpu` (works with either encoder
/// backend), or handed straight on as `CapturedFrame::Gpu` for
/// `VaapiEncoder` to import with no CPU round-trip (Phase B.5, zero-copy --
/// proven on real hardware against this exact dmabuf shape, see
/// `vaapi::tests::dmabuf_can_be_mapped_into_a_vaapi_surface`).
///
/// EGL/GBM setup happens once in `new` and outlives every resize; the dmabuf
/// target pool and damage tracker are size-dependent and built lazily on the
/// first `render()` call (and rebuilt whenever the output is resized), the
/// same "rebuild wholesale on resize" pattern `vaapi::build_pipeline` uses
/// for the VAAPI filtergraph/encoder.
///
/// `renderer` is `Rc<RefCell<_>>`, not owned outright, so `WaylandWebStreamState`
/// can hold a clone of the same handle for `linux-dmabuf` import
/// (`DmabufHandler::dmabuf_imported`, hardware-acceleration-plan.md Phase
/// B.4) without this struct's render path and the dmabuf-import callback
/// fighting over two different `GlesRenderer`s. Single-threaded by
/// construction (the compositor render loop owns this on one thread), so
/// `Rc`/`RefCell` rather than `Arc`/`Mutex`.
pub struct GlCompositor {
    renderer: Rc<RefCell<GlesRenderer>>,
    gbm: GbmAllocator<File>,
    sized: Option<SizedState>,
    // `st_rdev` of the DRM render node, for the dmabuf feedback's
    // `main_device` (Phase B.4) -- without it, Mesa's wayland-egl platform
    // can't tell which device to open and falls back to no-op/zink (proven
    // on hardware: see the deviation note on `WaylandWebStreamState::
    // enable_dmabuf`).
    main_device: u64,
    // Whether `render_gl` hands the rendered dmabuf straight to the encoder
    // (`CapturedFrame::Gpu`, Phase B.5) instead of reading it back to the
    // CPU. Fixed at construction (mirrors `EncoderConfig::gpu_frames`, which
    // `main.rs` sets from the same decision) -- never toggled at runtime,
    // since the paired encoder backend doesn't change mid-run either.
    produce_gpu_frames: bool,
}

/// Everything that depends on the current output size, rebuilt on resize.
struct SizedState {
    size: (u32, u32),
    damage_tracker: OutputDamageTracker,
    // A small pool rather than one buffer: render + CPU readback is fully
    // synchronous in this design (no fence/async pipelining without
    // `backend_drm`), so one buffer would technically work, but a pool of a
    // few costs little and is defensive against that changing later.
    targets: Vec<Dmabuf>,
    next_target: usize,
}

impl GlCompositor {
    /// Opens `device_path` (a DRM render node, e.g. `/dev/dri/renderD128`)
    /// and brings up a headless, surfaceless GBM/EGL/GLES stack. No
    /// `EGLSurface` is ever created -- omitting one is what makes the
    /// context surfaceless, there's no separate flag to set. `produce_gpu_frames`
    /// is forwarded straight to the `produce_gpu_frames` field -- see its doc.
    pub fn new(device_path: &str, produce_gpu_frames: bool) -> Result<Self> {
        let file = File::options()
            .read(true)
            .write(true)
            .open(device_path)
            .with_context(|| format!("failed to open DRM render node {device_path:?}"))?;
        let main_device = file
            .metadata()
            .with_context(|| format!("failed to stat {device_path:?}"))?
            .rdev();
        // `GbmDevice<File>` isn't `Clone` (`File` isn't) but the allocator
        // and the EGL display each need their own owned device wrapping the
        // same underlying fd -- `try_clone` dup()s the fd instead.
        let file_for_egl = file
            .try_clone()
            .with_context(|| format!("failed to dup the fd for {device_path:?}"))?;
        let gbm_device = GbmDevice::new(file)
            .with_context(|| format!("failed to create GBM device for {device_path:?}"))?;
        let egl_gbm_device = GbmDevice::new(file_for_egl)
            .with_context(|| format!("failed to create GBM device for {device_path:?}"))?;

        // SAFETY: `egl_gbm_device` is a fresh device this struct owns
        // exclusively; smithay tracks `EGLDisplay`s by native-display
        // identity internally.
        let display = unsafe { EGLDisplay::new(egl_gbm_device) }
            .context("failed to create EGLDisplay from GBM device")?;
        let context = EGLContext::new(&display).context("failed to create configless EGLContext")?;
        // SAFETY: this context was just created above and is not current on
        // any other thread.
        let renderer = unsafe { GlesRenderer::new(context) }.context("failed to create GlesRenderer")?;

        let gbm = GbmAllocator::new(gbm_device, GbmBufferFlags::RENDERING);

        Ok(Self {
            renderer: Rc::new(RefCell::new(renderer)),
            gbm,
            sized: None,
            main_device,
            produce_gpu_frames,
        })
    }

    /// Clones the handle to this compositor's `GlesRenderer`, for
    /// `WaylandWebStreamState::enable_dmabuf` to import client dmabufs into
    /// (Phase B.4) -- the same renderer this struct renders frames with, not
    /// a second one.
    pub fn renderer_handle(&self) -> Rc<RefCell<GlesRenderer>> {
        self.renderer.clone()
    }

    /// `st_rdev` of the DRM render node this compositor was opened against,
    /// for `WaylandWebStreamState::enable_dmabuf`'s dmabuf feedback.
    pub fn main_device(&self) -> u64 {
        self.main_device
    }

    /// (Re)builds the size-dependent target pool and damage tracker if `size`
    /// differs from what they were last built for. Returns `()` rather than
    /// the rebuilt state -- `render_gl` re-borrows `self.sized` itself
    /// afterwards, as a field-disjoint borrow from `self.renderer`, which a
    /// `&mut self`-wide return from this method would otherwise prevent.
    fn ensure_sized(&mut self, size: (u32, u32)) -> Result<()> {
        let needs_rebuild = match &self.sized {
            Some(s) => s.size != size,
            None => true,
        };
        if needs_rebuild {
            let (width, height) = size;
            let mut targets = Vec::with_capacity(2);
            for _ in 0..2 {
                let buffer = self
                    .gbm
                    .create_buffer(width, height, Fourcc::Argb8888, &[Modifier::Invalid])
                    .context("failed to allocate GBM render target")?;
                let dmabuf = buffer.export().context("failed to export GBM buffer as a dmabuf")?;
                targets.push(dmabuf);
            }
            self.sized = Some(SizedState {
                size,
                damage_tracker: OutputDamageTracker::new(
                    (width as i32, height as i32),
                    1.0,
                    smithay::utils::Transform::Normal,
                ),
                targets,
                next_target: 0,
            });
        }
        Ok(())
    }
}

impl Compositor for GlCompositor {
    fn render(
        &mut self,
        state: &mut WaylandWebStreamState,
        reuse: Option<Vec<u8>>,
    ) -> Option<CapturedFrame> {
        let (width, height) = (state.width, state.height);
        match self.render_gl(state, reuse, (width, height)) {
            Ok(frame) => Some(frame),
            Err(e) => {
                tracing::warn!("GL compositor render failed: {e:#}");
                None
            }
        }
    }
}

impl GlCompositor {
    fn render_gl(
        &mut self,
        state: &mut WaylandWebStreamState,
        reuse: Option<Vec<u8>>,
        (width, height): (u32, u32),
    ) -> Result<CapturedFrame> {
        let capture_instant = std::time::Instant::now();

        self.ensure_sized((width, height))?;
        let sized = self.sized.as_mut().expect("ensure_sized just initialized this");
        let target_idx = sized.next_target;
        sized.next_target = (sized.next_target + 1) % sized.targets.len();
        let target = &mut sized.targets[target_idx];
        // Cloned upfront (a cheap Arc bump, not a copy) rather than after
        // binding: `Bind::bind`'s returned `Framebuffer<'_>` keeps `target`
        // mutably borrowed for as long as it's alive (it has a `Drop` impl,
        // so NLL can't shorten that), which would make a borrow taken after
        // `render_output` conflict with it.
        let dmabuf_for_gpu = self.produce_gpu_frames.then(|| target.clone());

        let mut renderer = self.renderer.borrow_mut();
        let renderer = &mut *renderer;

        let mut framebuffer = renderer
            .bind(target)
            .context("failed to bind dmabuf render target")?;

        let elements = space_render_elements(renderer, [&state.space], &state.output, 1.0)
            .context("failed to collect space render elements")?;

        let render_result = sized
            .damage_tracker
            .render_output(renderer, &mut framebuffer, 0, &elements, Color32F::BLACK)
            .map_err(|e| anyhow::anyhow!("render_output failed: {e:?}"))?;
        // Block until the GPU has actually finished writing `target` --
        // load-bearing for the `Gpu` arm below, which hands the same dmabuf
        // straight to a different API (VAAPI) that has no reason to respect
        // *this* GL context's own implicit ordering. `ExportMem` readback
        // (the `Cpu` arm) happened to work without this (a same-context
        // readback serializes naturally), but waiting unconditionally costs
        // nothing extra in this fully-synchronous render loop and removes
        // any doubt for either path.
        render_result.sync.wait().context("waiting on the GL render fence failed")?;

        if let Some(dmabuf) = dmabuf_for_gpu {
            return Ok(CapturedFrame::Gpu {
                dmabuf,
                width,
                height,
                capture_instant,
            });
        }

        let region = Rectangle::<i32, BufferCoord>::from_size((width as i32, height as i32).into());
        let mapping = renderer
            .copy_framebuffer(&framebuffer, region, Fourcc::Argb8888)
            .context("failed to copy GL framebuffer to a CPU-readable mapping")?;
        let bytes = renderer
            .map_texture(&mapping)
            .context("failed to map the copied framebuffer")?;

        let mut data = reuse.unwrap_or_default();
        data.clear();
        data.extend_from_slice(bytes);

        Ok(CapturedFrame::Cpu(RawFrame {
            data,
            width,
            height,
            capture_instant,
        }))
    }
}
