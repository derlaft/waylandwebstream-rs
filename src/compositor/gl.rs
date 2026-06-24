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
use std::fs::File;

use crate::encoder::{CapturedFrame, RawFrame};

use super::Compositor;
use super::state::WaylandWebStreamState;

/// GPU compositor (hardware-acceleration-plan.md Phase B, stage 1): renders
/// the `Space` with smithay's `GlesRenderer` into an offscreen GBM/dmabuf
/// target instead of the manual SHM memcpy `WaylandWebStreamState::render`
/// does, then reads the result back to the CPU so it can still be handed to
/// either encoder backend unchanged (`CapturedFrame::Gpu`/zero-copy VAAPI
/// import is Phase B.5, not built yet).
///
/// EGL/GBM setup happens once in `new` and outlives every resize; the dmabuf
/// target pool and damage tracker are size-dependent and built lazily on the
/// first `render()` call (and rebuilt whenever the output is resized), the
/// same "rebuild wholesale on resize" pattern `vaapi::build_pipeline` uses
/// for the VAAPI filtergraph/encoder.
pub struct GlCompositor {
    renderer: GlesRenderer,
    gbm: GbmAllocator<File>,
    sized: Option<SizedState>,
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
    /// context surfaceless, there's no separate flag to set.
    pub fn new(device_path: &str) -> Result<Self> {
        let file = File::options()
            .read(true)
            .write(true)
            .open(device_path)
            .with_context(|| format!("failed to open DRM render node {device_path:?}"))?;
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
            renderer,
            gbm,
            sized: None,
        })
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

        let mut framebuffer = self
            .renderer
            .bind(target)
            .context("failed to bind dmabuf render target")?;

        let elements = space_render_elements(&mut self.renderer, [&state.space], &state.output, 1.0)
            .context("failed to collect space render elements")?;

        sized
            .damage_tracker
            .render_output(&mut self.renderer, &mut framebuffer, 0, &elements, Color32F::BLACK)
            .map_err(|e| anyhow::anyhow!("render_output failed: {e:?}"))?;

        let region = Rectangle::<i32, BufferCoord>::from_size((width as i32, height as i32).into());
        let mapping = self
            .renderer
            .copy_framebuffer(&framebuffer, region, Fourcc::Argb8888)
            .context("failed to copy GL framebuffer to a CPU-readable mapping")?;
        let bytes = self
            .renderer
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
