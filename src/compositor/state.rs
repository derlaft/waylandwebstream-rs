// Complete compositor state implementation with full Wayland protocol support

use anyhow::{Context, Result};
use smithay::{
    backend::{
        allocator::{
            dmabuf::{Dmabuf, DmabufMappingMode, DmabufSyncFlags},
            Buffer as _, Fourcc,
        },
        input::{Axis, AxisSource, ButtonState, KeyState, TouchSlot},
        renderer::{gles::GlesRenderer, utils::with_renderer_surface_state, ImportDma},
    },
    delegate_compositor, delegate_cursor_shape, delegate_dmabuf, delegate_fractional_scale,
    delegate_keyboard_shortcuts_inhibit, delegate_output, delegate_pointer_constraints,
    delegate_relative_pointer, delegate_seat, delegate_shm, delegate_single_pixel_buffer,
    delegate_viewporter, delegate_xdg_shell, delegate_xdg_toplevel_icon,
    desktop::{Space, Window},
    input::{
        keyboard::FilterResult,
        pointer::{
            AxisFrame, ButtonEvent, CursorImageStatus, CursorImageSurfaceData,
            MotionEvent as PointerMotionEvent, RelativeMotionEvent,
        },
        touch::{DownEvent, MotionEvent, UpEvent},
        Seat, SeatState,
    },
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::{
        calloop::EventLoop,
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{
            backend::{ClientData, ClientId, DisconnectReason, ObjectId},
            protocol::{wl_buffer::WlBuffer, wl_seat, wl_shm, wl_surface::WlSurface},
            Display, DisplayHandle, Resource,
        },
    },
    utils::{Clock, Logical, Monotonic, Point, Rectangle, Transform, SERIAL_COUNTER},
    wayland::{
        buffer::BufferHandler,
        compositor::{
            send_surface_state, with_states, CompositorClientState,
            CompositorState as SmithayCompositorState, RectangleKind, RegionAttributes,
        },
        cursor_shape::CursorShapeManagerState,
        dmabuf::{
            get_dmabuf, DmabufFeedbackBuilder, DmabufGlobal, DmabufHandler, DmabufState,
            ImportNotifier,
        },
        fractional_scale::{
            with_fractional_scale, FractionalScaleHandler, FractionalScaleManagerState,
        },
        keyboard_shortcuts_inhibit::{
            KeyboardShortcutsInhibitHandler, KeyboardShortcutsInhibitState,
            KeyboardShortcutsInhibitor,
        },
        output::{OutputHandler, OutputManagerState},
        pointer_constraints::{
            with_pointer_constraint, PointerConstraint, PointerConstraintsHandler,
            PointerConstraintsState,
        },
        relative_pointer::RelativePointerManagerState,
        seat::WaylandFocus,
        shell::xdg::{ToplevelSurface, XdgShellState, XdgToplevelSurfaceData},
        shm::{with_buffer_contents, ShmHandler, ShmState},
        single_pixel_buffer::SinglePixelBufferState,
        tablet_manager::TabletSeatHandler,
        viewporter::ViewporterState,
        xdg_toplevel_icon::{
            ToplevelIconCachedState, XdgToplevelIconHandler, XdgToplevelIconManager,
        },
    },
};
use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use tracing::{debug, info, trace, warn};

use crate::encoder::DamageRect;

/// Raw top-down RGBA favicon image: `(width, height, bytes)`.
type Favicon = (u32, u32, Vec<u8>);

/// Snapshot of the topmost window's app metadata: `(title, app_id, favicon)`.
type AppMeta = (String, String, Option<Favicon>);

/// Name reported for the single headless `wl_output` we expose.
const OUTPUT_NAME: &str = "HEADLESS-1";

/// Keyboard auto-repeat: delay before the first repeat, in milliseconds.
const KEYBOARD_REPEAT_DELAY_MS: i32 = 200;

/// Keyboard auto-repeat: repeats per second once repeating begins.
const KEYBOARD_REPEAT_RATE_HZ: i32 = 25;

/// Cursor update extracted from a Wayland client's `wl_pointer.set_cursor` call.
/// Pixels in the `Surface` variant are already RGBA (Canvas-ready); the
/// BGRA↔RGBA swap happens inside `read_cursor_pixels`.
#[derive(Debug, Clone)]
pub enum CursorPending {
    Hidden,
    Named(String),
    Surface {
        width: u32,
        height: u32,
        hotspot_x: i32,
        hotspot_y: i32,
        /// Raw RGBA bytes (not base64), width × height × 4.
        rgba: Vec<u8>,
    },
}

impl CursorPending {
    pub fn kind_name(&self) -> &'static str {
        match self {
            CursorPending::Hidden => "Hidden",
            CursorPending::Named(_) => "Named",
            CursorPending::Surface { .. } => "Surface",
        }
    }
}

pub struct WaylandWebStreamState {
    // Core Smithay states
    pub compositor_state: SmithayCompositorState,
    pub xdg_shell_state: XdgShellState,
    pub shm_state: ShmState,
    // Holds protocol globals alive; never read directly — delegate macros wire Dispatch impls.
    #[allow(dead_code)]
    pub single_pixel_buffer_state: SinglePixelBufferState,
    #[allow(dead_code)]
    pub viewporter_state: ViewporterState,
    #[allow(dead_code)]
    pub pointer_constraints_state: PointerConstraintsState,
    #[allow(dead_code)]
    pub relative_pointer_manager_state: RelativePointerManagerState,
    #[allow(dead_code)]
    pub fractional_scale_manager_state: FractionalScaleManagerState,
    #[allow(dead_code)]
    pub keyboard_shortcuts_inhibit_state: KeyboardShortcutsInhibitState,
    #[allow(dead_code)]
    pub xdg_toplevel_icon_manager: XdgToplevelIconManager,
    #[allow(dead_code)]
    pub cursor_shape_state: CursorShapeManagerState,
    pub seat_state: SeatState<Self>,

    // Desktop management
    pub space: Space<Window>,
    pub seat: Seat<Self>,

    // Output and rendering
    pub output: Output,
    pub width: u32,
    pub height: u32,

    // Clock for timing
    pub clock: Clock<Monotonic>,

    // Accumulated logical-space damage since the last `take_dirty()`: a small
    // *set* of rectangles, not one merged bounding box, so two disjoint
    // damages -- a cursor in one corner and a caret in another -- don't blow up
    // into a near-fullscreen repaint. Unioned across every surface commit,
    // window map/unmap, and resize that may have changed the picture; bounded
    // to `MAX_DAMAGE_RECTS` (collapsing to a single bbox past that, and a
    // full-output rect supersedes the rest). Empty means provably nothing
    // changed -- the main loop skips render()+encode() then.
    damage: Vec<Rectangle<i32, Logical>>,

    // The damage rectangles `take_dirty()` last consumed, stashed for the SW
    // `render()` to clip each composite pass to. Empty means "repaint
    // everything" (a forced render with no tracked damage). The GL backend
    // ignores it.
    repaint_region: Vec<Rectangle<i32, Logical>>,

    // The pixel rectangles the last `render()` actually repainted, handed to
    // the encoder (via `take_repaint_rects`) so it converts only those rows
    // BGRA->YUV. Mirrors `repaint_region` but as output-pixel `DamageRect`s and
    // *after* the full-repaint fallback is resolved (so a forced/first/resized
    // frame reports the whole output, matching what was composited).
    last_repaint_rects: Vec<DamageRect>,

    // Persistent last fully-composited frame (BGRA, `width*height*4` bytes).
    // `render()` updates only the damaged sub-rect each frame and hands the
    // encoder a full copy, so unchanged regions aren't recomposited. Empty
    // until the first render and whenever the output resizes (size mismatch
    // forces a full repaint). Unused by the GL backend.
    canvas: Vec<u8>,

    // Counts calls to `render()`, used to throttle its debug/trace logging.
    frame_counter: u32,

    // `linux-dmabuf` (AGENTS.md). Both `None`
    // until `enable_dmabuf` registers the global -- only meaningful with the
    // `gl` compositor backend, since `SwCompositor`'s SHM-only render path
    // has no renderer to import a client's dmabuf into. `dmabuf_renderer` is
    // a clone of the same handle `GlCompositor` renders with (see
    // `GlCompositor::renderer_handle`), not a second renderer.
    dmabuf_state: Option<DmabufState>,
    dmabuf_renderer: Option<Rc<RefCell<GlesRenderer>>>,

    // Toplevels that have already received their post-first-commit "kick"
    // configure -- see `commit`'s call to `configure_toplevel_fullscreen`
    // below. Cleared per-surface in `toplevel_destroyed`.
    kicked_toplevels: HashSet<ObjectId>,

    // Current cursor surface + hotspot set by the focused client via
    // `wl_pointer.set_cursor`. Updated in `cursor_image()` and re-extracted
    // on every commit to the cursor surface (for animated cursors).
    cursor_surface: Option<(WlSurface, Point<i32, Logical>)>,
    // Pending cursor update for the main loop to pick up and forward to
    // WebSocket clients. Set by `try_extract_cursor` and consumed by
    // `take_cursor_pending`.
    cursor_pending: Option<CursorPending>,

    // Topmost (focused) toplevel's title/app_id, surfaced to the browser tab.
    // Recomputed by `refresh_app_meta` on title/app_id changes and on focus
    // changes (map/unmap); `app_meta_dirty` gates the per-tick forward in the
    // main loop (see `take_app_meta_pending`), mirroring the cursor pattern.
    // In nested mode these reflect the inner compositor's generic toplevel
    // (e.g. "sway"), not the app running inside it.
    current_title: String,
    current_app_id: String,
    // Topmost window's favicon as raw top-down RGBA `(width, height, bytes)`,
    // or `None` if it set no `xdg_toplevel_icon`. Carried alongside title on the
    // app-metadata channel (main loop base64-encodes it for the wire).
    current_favicon: Option<(u32, u32, Vec<u8>)>,
    // A toplevel surface whose pending icon must be (re-)read after its next
    // commit -- `xdg_toplevel_icon` is double-buffered with the toplevel commit,
    // so `set_icon` only flags it and `commit()` does the extraction.
    icon_dirty_surface: Option<WlSurface>,
    app_meta_dirty: bool,

    // Whether the focused surface currently holds an *active* pointer lock
    // (`zwp_locked_pointer`). Tracked so `take_pointer_lock_pending` can tell
    // the browser when to enter/leave Pointer Lock (relative-motion) mode.
    // Polled each tick rather than event-driven: the constraints protocol has
    // no "deactivated" callback, and a client can drop the lock at any time.
    pointer_locked: bool,

    // The fractional scale most recently requested by the browser (its
    // devicePixelRatio when the HiDPI/native-resolution toggle is on, else
    // 1.0). Advertised to clients via `wp_fractional_scale_v1` -- but only when
    // the GL backend is active, since the SW blit can't downscale a larger
    // fractional buffer (see `effective_scale`). Default 1.0.
    preferred_scale: f64,
}

/// Half-open pixel rectangle `[x0, x1) × [y0, y1)` used to restrict
/// compositing to a damaged sub-region of the output. Coordinates are clamped
/// to the output bounds on construction, so a `Clip` never addresses outside
/// the framebuffer.
/// Upper bound on tracked damage rectangles before they collapse to a single
/// bounding box. Small: a handful of disjoint damages (cursor, caret, a couple
/// of updating widgets) is the case worth keeping separate; beyond that the
/// per-rect compositing overhead outweighs the saving over one bbox repaint.
const MAX_DAMAGE_RECTS: usize = 8;

/// Accumulates `rect` into a damage `set`, keeping disjoint regions separate.
/// Separated from `CompositorState::add_damage` so the set logic (dedup,
/// full-output supersede, bounded collapse) is unit-testable without building a
/// whole compositor. `full` is the output-covering rectangle.
fn accumulate_damage(
    set: &mut Vec<Rectangle<i32, Logical>>,
    rect: Rectangle<i32, Logical>,
    full: Rectangle<i32, Logical>,
) {
    // Already fully damaged -> nothing finer worth tracking.
    if set.first() == Some(&full) {
        return;
    }
    if rect == full {
        set.clear();
        set.push(full);
        return;
    }
    // Skip exact duplicates (the same surface re-damaged within a frame).
    if set.contains(&rect) {
        return;
    }
    if set.len() >= MAX_DAMAGE_RECTS {
        // Collapse to the bounding box of everything plus the newcomer.
        let merged = set.iter().copied().fold(rect, |a, r| a.merge(r));
        set.clear();
        set.push(merged);
    } else {
        set.push(rect);
    }
}

#[derive(Clone, Copy, Debug)]
struct Clip {
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
}

impl Clip {
    /// The whole `width × height` output.
    fn full(width: u32, height: u32) -> Self {
        Self {
            x0: 0,
            y0: 0,
            x1: width,
            y1: height,
        }
    }

    /// Converts a logical-space damage rectangle to a pixel clip, clamped to
    /// the output. The compositor blits buffers 1:1 (scale 1, no transform),
    /// so logical coordinates are output pixels.
    fn from_logical(rect: Rectangle<i32, Logical>, width: u32, height: u32) -> Self {
        let x0 = rect.loc.x.clamp(0, width as i32) as u32;
        let y0 = rect.loc.y.clamp(0, height as i32) as u32;
        let x1 = (rect.loc.x + rect.size.w).clamp(0, width as i32) as u32;
        let y1 = (rect.loc.y + rect.size.h).clamp(0, height as i32) as u32;
        Self { x0, y0, x1, y1 }
    }

    fn is_empty(&self) -> bool {
        self.x0 >= self.x1 || self.y0 >= self.y1
    }
}

/// Zeroes the `clip` region of `dst` (a `width`-wide BGRA buffer), one row
/// span at a time. Clears only the area that's about to be repainted.
fn clear_region(dst: &mut [u8], width: u32, clip: Clip) {
    for y in clip.y0..clip.y1 {
        let start = ((y * width + clip.x0) * 4) as usize;
        let end = ((y * width + clip.x1) * 4) as usize;
        if end <= dst.len() {
            dst[start..end].fill(0);
        }
    }
}

/// Copies a window's BGRA buffer into the output framebuffer `dst` at
/// `(pos_x, pos_y)`, 1:1, clipping to the output bounds and to `clip` (the
/// damaged sub-region being repainted this frame). `src` holds `src_h` rows of
/// `src_stride` bytes; only the first `src_w * 4` bytes of each row are
/// candidates. Rows/bytes that would fall outside either buffer or outside
/// `clip` are skipped.
#[allow(clippy::too_many_arguments)] // a pixel blit is clearest with explicit dims
fn blit_bgra(
    dst: &mut [u8],
    dst_width: u32,
    dst_height: u32,
    pos_x: u32,
    pos_y: u32,
    src: &[u8],
    src_w: u32,
    src_h: u32,
    src_stride: u32,
    clip: Clip,
) {
    let target_width = dst_width.saturating_sub(pos_x);
    let target_height = dst_height.saturating_sub(pos_y);
    if src_w == 0 || src_h == 0 || target_width == 0 || target_height == 0 {
        return;
    }
    let copy_w = src_w.min(target_width);
    let copy_h = src_h.min(target_height);
    // Intersect the blit's destination extent with the clip region.
    let dx0 = pos_x.max(clip.x0);
    let dx1 = (pos_x + copy_w).min(clip.x1);
    let dy0 = pos_y.max(clip.y0);
    let dy1 = (pos_y + copy_h).min(clip.y1);
    if dx0 >= dx1 || dy0 >= dy1 {
        return;
    }
    let row_bytes = ((dx1 - dx0) * 4) as usize;
    for dest_y in dy0..dy1 {
        // Source pixel for output (dx0, dest_y): offset back by the blit origin.
        let src_idx = ((dest_y - pos_y) * src_stride + (dx0 - pos_x) * 4) as usize;
        let dest_idx = ((dest_y * dst_width + dx0) * 4) as usize;
        if src_idx + row_bytes <= src.len() && dest_idx + row_bytes <= dst.len() {
            dst[dest_idx..dest_idx + row_bytes].copy_from_slice(&src[src_idx..src_idx + row_bytes]);
        }
    }
}

/// Fills the output region covered by a single-pixel buffer with its solid
/// colour, restricted to `clip`. `rgba` is the source colour; the output is
/// BGRA-packed.
fn fill_solid(
    dst: &mut [u8],
    dst_width: u32,
    dst_height: u32,
    pos_x: u32,
    pos_y: u32,
    rgba: [u8; 4],
    clip: Clip,
) {
    let [r, g, b, a] = rgba;
    let x0 = pos_x.max(clip.x0);
    let x1 = dst_width.min(clip.x1);
    let y0 = pos_y.max(clip.y0);
    let y1 = dst_height.min(clip.y1);
    for dest_y in y0..y1 {
        for dest_x in x0..x1 {
            let dest_idx = ((dest_y * dst_width + dest_x) * 4) as usize;
            if let Some(px) = dst.get_mut(dest_idx..dest_idx + 4) {
                px.copy_from_slice(&[b, g, r, a]);
            }
        }
    }
}

/// Renders the classic X11 "root weave" stipple (a 4x4 basket-weave bitmap in
/// black and white) into `dst`, used when no window is mapped.
fn render_root_weave(dst: &mut [u8], width: u32, height: u32, clip: Clip) {
    const ROOT_WEAVE_BITS: [u8; 4] = [0b0110, 0b1001, 0b1001, 0b0110];
    let y1 = height.min(clip.y1);
    let x1 = width.min(clip.x1);
    // The pattern is keyed off absolute (x, y), so clipping only limits which
    // pixels are touched -- the weave stays aligned across partial repaints.
    for y in clip.y0..y1 {
        let row = ROOT_WEAVE_BITS[(y % 4) as usize];
        for x in clip.x0..x1 {
            let bit = (row >> (x % 4)) & 1;
            let color = if bit == 1 { 255 } else { 0 };
            let idx = ((y * width + x) * 4) as usize;
            if let Some(px) = dst.get_mut(idx..idx + 4) {
                px.copy_from_slice(&[color, color, color, 255]);
            }
        }
    }
}

/// Clamps `p` (surface-local logical coords) into the bounding box of a pointer
/// constraint region's additive rectangles. Best-effort: non-rectangular
/// regions (those using subtract rects) are approximated by the additive
/// bounding box, which is sufficient for the common single-rect confine.
fn clamp_point_to_region(p: Point<f64, Logical>, region: &RegionAttributes) -> Point<f64, Logical> {
    let mut bbox: Option<Rectangle<i32, Logical>> = None;
    for (kind, rect) in &region.rects {
        if matches!(kind, RectangleKind::Add) {
            bbox = Some(match bbox {
                Some(b) => b.merge(*rect),
                None => *rect,
            });
        }
    }
    let Some(b) = bbox else { return p };
    let x = p.x.clamp(b.loc.x as f64, (b.loc.x + b.size.w) as f64);
    let y = p.y.clamp(b.loc.y as f64, (b.loc.y + b.size.h) as f64);
    Point::from((x, y))
}

impl WaylandWebStreamState {
    pub fn new(
        _event_loop: &mut EventLoop<Self>,
        display: &mut Display<Self>,
        width: u32,
        height: u32,
    ) -> Self {
        info!(
            "Initializing full compositor with resolution {}x{}",
            width, height
        );

        let dh = display.handle();

        // Initialize all Wayland protocol states.
        // Hyprland's Aquamarine backend requires wl_compositor >= 6; new() only
        // advertises version 5, which makes it reject the bind and abort.
        let compositor_state = SmithayCompositorState::new_v6::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let single_pixel_buffer_state = SinglePixelBufferState::new::<Self>(&dh);
        let viewporter_state = ViewporterState::new::<Self>(&dh);
        let pointer_constraints_state = PointerConstraintsState::new::<Self>(&dh);
        let relative_pointer_manager_state = RelativePointerManagerState::new::<Self>(&dh);
        let fractional_scale_manager_state = FractionalScaleManagerState::new::<Self>(&dh);
        // NOTE: wp_presentation is intentionally NOT advertised. render()
        // bypasses Smithay's renderer and never records a scan-out, so we have
        // no presentation feedback to send; advertising the global while never
        // emitting `presented`/`discarded` left timing-sensitive clients (GTK4
        // frame clock, mpv) waiting on feedback that never came. TODO: wire
        // real feedback from the capture loop's capture/encode timestamps and
        // re-advertise.
        let keyboard_shortcuts_inhibit_state = KeyboardShortcutsInhibitState::new::<Self>(&dh);
        let xdg_toplevel_icon_manager = XdgToplevelIconManager::new::<Self>(&dh);
        let cursor_shape_state = CursorShapeManagerState::new::<Self>(&dh);
        // Registers the wl_output/xdg-output globals as a side effect; the
        // returned handle itself is never read afterwards.
        OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let mut seat_state = SeatState::new();

        // Create output with specified dimensions
        let mode = Mode {
            size: (width as i32, height as i32).into(),
            refresh: 60_000, // 60 Hz in mHz
        };

        let physical_properties = PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "WaylandWebStream".into(),
            model: "Virtual".into(),
        };

        let output = Output::new(OUTPUT_NAME.to_string(), physical_properties);

        output.change_current_state(Some(mode), None, None, Some((0, 0).into()));
        output.set_preferred(mode);
        output.create_global::<Self>(&dh);

        // Create seat (input device manager)
        let mut seat = seat_state.new_wl_seat(&dh, "seat-0");
        seat.add_keyboard(
            Default::default(),
            KEYBOARD_REPEAT_DELAY_MS,
            KEYBOARD_REPEAT_RATE_HZ,
        )
        .unwrap();
        seat.add_pointer();
        seat.add_touch();

        // Create space for window management
        let mut space = Space::default();
        space.map_output(&output, (0, 0));

        Self {
            compositor_state,
            xdg_shell_state,
            shm_state,
            single_pixel_buffer_state,
            viewporter_state,
            pointer_constraints_state,
            relative_pointer_manager_state,
            fractional_scale_manager_state,
            keyboard_shortcuts_inhibit_state,
            xdg_toplevel_icon_manager,
            cursor_shape_state,
            seat_state,
            space,
            seat,
            output,
            width,
            height,
            clock: Clock::new(),
            damage: vec![Rectangle::new(
                (0, 0).into(),
                (width as i32, height as i32).into(),
            )],
            repaint_region: Vec::new(),
            last_repaint_rects: Vec::new(),
            canvas: Vec::new(),
            frame_counter: 0,
            dmabuf_state: None,
            dmabuf_renderer: None,
            kicked_toplevels: HashSet::new(),
            cursor_surface: None,
            cursor_pending: None,
            current_title: String::new(),
            current_app_id: String::new(),
            current_favicon: None,
            icon_dirty_surface: None,
            app_meta_dirty: false,
            pointer_locked: false,
            preferred_scale: 1.0,
        }
    }

    /// Registers the `zwp_linux_dmabuf_v1` global, advertising `renderer`'s
    /// supported dmabuf formats, and remembers `renderer` so
    /// `DmabufHandler::dmabuf_imported` can actually import client buffers
    /// into it. `renderer` is the same handle `GlCompositor` renders with
    /// (`GlCompositor::renderer_handle`); `main_device` is its DRM render
    /// node's `st_rdev` (`GlCompositor::main_device`). Only called when
    /// `--compositor gl` initializes successfully; the `sw` backend has no
    /// renderer to import into, so no global is advertised and SHM-only
    /// clients are unaffected either way.
    ///
    /// **Deviation from the plan's literal checklist** (which named the
    /// formats-only v3 global, `DmabufState::create_global`): verified on
    /// real hardware that v3 doesn't actually work for a GL client. Mesa's
    /// wayland-egl platform needs the dmabuf feedback's `main_device` event
    /// to know which DRM device to open -- v3 has no feedback mechanism at
    /// all, so without it Mesa can't find a device (`failed to get driver
    /// name for fd -1`, falls back to zink/software, which then also fails
    /// with no usable Vulkan ICD). Reproduced with `weston-simple-egl`
    /// against this server; switching to the feedback-based v4/v5 global
    /// (`create_global_with_default_feedback`) fixed it. A single render
    /// node and no scan-out planes means there's nothing to put in a
    /// preference tranche, so the feedback carries just the main tranche.
    pub fn enable_dmabuf(
        &mut self,
        display: &DisplayHandle,
        renderer: Rc<RefCell<GlesRenderer>>,
        main_device: u64,
    ) -> Result<()> {
        let formats = renderer.borrow().dmabuf_formats();
        let feedback = DmabufFeedbackBuilder::new(main_device, formats)
            .build()
            .context("failed to build dmabuf feedback")?;
        let mut dmabuf_state = DmabufState::new();
        dmabuf_state.create_global_with_default_feedback::<Self>(display, &feedback);
        self.dmabuf_state = Some(dmabuf_state);
        self.dmabuf_renderer = Some(renderer);
        Ok(())
    }

    /// Returns whether the rendered picture may have changed since the last
    /// call, and clears the accumulated damage. Conservative where real
    /// per-surface damage can't be determined (e.g. a surface commit that
    /// doesn't map to a positioned window): such commits mark the whole
    /// output damaged rather than risk missing a real change.
    ///
    /// The consumed damage rectangle is stashed in `repaint_region` so the
    /// SW `render()` (called moments later in the same loop tick) can repaint
    /// only that sub-region. A `true` return is always followed by a
    /// `render()` call, so the stash never goes stale across frames.
    pub fn take_dirty(&mut self) -> bool {
        self.repaint_region = std::mem::take(&mut self.damage);
        !self.repaint_region.is_empty()
    }

    /// Takes the pixel rectangles the most recent `render()` repainted, for the
    /// encoder to convert damage-only (see `RawFrame::damage`). Empty if the
    /// last render repainted nothing (it never hands the encoder a frame then).
    pub fn take_repaint_rects(&mut self) -> Vec<DamageRect> {
        std::mem::take(&mut self.last_repaint_rects)
    }

    /// Re-marks `rects` as damage. Used when a frame was rendered but dropped
    /// before it reached the encoder (the capture loop's bounded send queue was
    /// full): without this the encoder's persistent YUV frame keeps the dropped
    /// frame's stale rows, since the *next* frame's damage no longer covers
    /// them. Pixel coords are output-local, i.e. 1:1 with logical space.
    pub fn readd_damage(&mut self, rects: &[DamageRect]) {
        let width = self.width as i32;
        for r in rects {
            // Row bands are full-width (the encoder is row-based); re-mark the
            // whole rows so the next render re-composites and re-converts them.
            self.add_damage(Rectangle::new(
                (0, r.y as i32).into(),
                (width, r.height as i32).into(),
            ));
        }
    }

    /// Whether there is accumulated damage, *without* consuming it. The capture
    /// loop uses this to decide whether to render early (as soon as a frame
    /// interval has elapsed since the last capture) instead of waiting for the
    /// next periodic frame deadline -- damage often lands mid-interval on an
    /// otherwise idle screen, and waiting for the grid point adds up to a full
    /// frame of latency. The actual consume still happens in `take_dirty()`
    /// when the frame is rendered, so the `repaint_region` stash stays correct.
    pub fn is_dirty(&self) -> bool {
        !self.damage.is_empty()
    }

    /// Adds `rect` to the accumulated damage for the current frame. Tracked as
    /// a small set of rectangles rather than one merged bounding box (see the
    /// `damage` field) so `render()` only recomposites the regions that
    /// actually changed. A full-output rect supersedes everything (and
    /// short-circuits further adds); past `MAX_DAMAGE_RECTS` the set collapses
    /// to its bounding box so pathological churn can't make per-frame
    /// compositing unbounded.
    fn add_damage(&mut self, rect: Rectangle<i32, Logical>) {
        let full = self.full_output_damage();
        accumulate_damage(&mut self.damage, rect, full);
    }

    /// Returns the rectangle covering the entire output, in logical space.
    fn full_output_damage(&self) -> Rectangle<i32, Logical> {
        Rectangle::new(
            (0, 0).into(),
            (self.width as i32, self.height as i32).into(),
        )
    }

    /// Computes the logical-space rectangle damaged by `surface`'s most
    /// recent buffer commit, if it carried any new damage, and advances the
    /// per-surface damage cursor so the same damage isn't reported twice.
    /// `location` is the surface's position in output space. Returns `None`
    /// if the commit carried no buffer (yet) or no new damage -- including a
    /// commit that detaches a previously-attached buffer without destroying
    /// the surface. That's indistinguishable here from "nothing to report"
    /// and isn't a pattern any client this project targets uses; `toplevel_destroyed`
    /// covers the actual window-going-away case with full-output damage.
    fn surface_damage(
        surface: &WlSurface,
        location: Point<i32, Logical>,
    ) -> Option<Rectangle<i32, Logical>> {
        use smithay::backend::renderer::utils::{CommitCounter, RendererSurfaceStateUserData};
        use smithay::wayland::compositor::with_states;
        use std::cell::Cell;

        with_states(surface, |states| {
            let rstate = states
                .data_map
                .get::<RendererSurfaceStateUserData>()?
                .lock()
                .unwrap();
            let buffer_size = rstate.buffer_size()?;

            let counter_cell = states
                .data_map
                .get_or_insert(Cell::<CommitCounter>::default);
            let last_seen = counter_cell.get();
            let buffer_damage = rstate.damage_since(Some(last_seen));
            counter_cell.set(rstate.current_commit());

            if buffer_damage.is_empty() {
                return None;
            }

            if rstate.buffer_scale() == 1 && rstate.buffer_transform() == Transform::Normal {
                let union = buffer_damage.iter().copied().reduce(|a, b| a.merge(b))?;
                let buffer_dims = buffer_size.to_buffer(1, Transform::Normal);
                let logical = union.to_logical(1, Transform::Normal, &buffer_dims);
                Some(Rectangle::new(logical.loc + location, logical.size))
            } else {
                // Scaled/transformed buffers don't occur in practice in this
                // headless compositor; fall back to the whole surface rather
                // than risk getting the scale/transform math wrong.
                Some(Rectangle::new(location, buffer_size))
            }
        })
    }

    pub fn resize_output(&mut self, width: u32, height: u32) {
        info!("Resizing output to {}x{}", width, height);

        let mode = Mode {
            size: (width as i32, height as i32).into(),
            refresh: 60_000,
        };

        self.output
            .change_current_state(Some(mode), None, None, None);
        self.output.set_preferred(mode);
        self.width = width;
        self.height = height;
        let full_damage = self.full_output_damage();
        self.add_damage(full_damage);

        // Tell every mapped client window about the new viewport size so it
        // redraws to fill the screen instead of staying at its old size.
        let toplevels: Vec<ToplevelSurface> = self
            .space
            .elements()
            .filter_map(|window| window.toplevel().cloned())
            .collect();
        for toplevel in toplevels {
            self.configure_toplevel_fullscreen(&toplevel);
            toplevel.send_configure();
        }
    }

    /// Configures a toplevel's pending state to occupy the entire output,
    /// borderless. Used both for newly created windows and on viewport resize.
    fn configure_toplevel_fullscreen(&self, surface: &ToplevelSurface) {
        surface.with_pending_state(|state| {
            state.size = Some((self.width as i32, self.height as i32).into());
            state.states.set(xdg_toplevel::State::Maximized);
            state.states.set(xdg_toplevel::State::Activated);
        });
    }

    /// Renders the current frame into the persistent `canvas`, repainting only
    /// the region damaged since the last frame, then hands the encoder a full
    /// copy. `reuse_buffer`, if given, is an already-allocated buffer (handed
    /// back by the encoder once it's done with a previous frame) reused as the
    /// output instead of allocating a fresh ~8MB buffer each frame.
    ///
    /// `take_dirty()` stashes the frame's damage in `repaint_region`;
    /// everything outside it is carried over from the previous frame still
    /// sitting in `canvas`, so unchanged windows aren't recomposited. A size
    /// mismatch (first frame or just-resized output) or a missing region (a
    /// forced render with no tracked damage) falls back to a full repaint. The
    /// encoder always receives a complete frame, so its BGRA->YUV conversion
    /// and keyframe logic are unaffected.
    pub fn render(&mut self, reuse_buffer: Option<Vec<u8>>) -> Option<Vec<u8>> {
        let buffer_size = (self.width * self.height * 4) as usize;

        // Pull the canvas out so the window loop can borrow `self.space` while
        // compositing into it; it's restored before returning. A size mismatch
        // (first frame or post-resize) means last frame's pixels can't be
        // trusted, so the whole output is repainted.
        let mut canvas = std::mem::take(&mut self.canvas);
        let size_changed = canvas.len() != buffer_size;
        if size_changed {
            canvas.clear();
            canvas.resize(buffer_size, 0);
        }

        // Repaint clips: the damaged sub-rects, or the whole output when the
        // canvas was just (re)sized or no damage was recorded. Each is
        // composited independently so disjoint damage doesn't repaint the
        // bounding box between the regions.
        let clips: Vec<Clip> = if size_changed || self.repaint_region.is_empty() {
            self.repaint_region.clear();
            vec![Clip::full(self.width, self.height)]
        } else {
            std::mem::take(&mut self.repaint_region)
                .into_iter()
                .map(|rect| Clip::from_logical(rect, self.width, self.height))
                .filter(|clip| !clip.is_empty())
                .collect()
        };

        // Hand these exact regions to the encoder so it converts only these rows
        // BGRA->YUV. (The handoff below copies the whole frame -- see the note
        // there on why a partial copy is unsafe under frame skipping.)
        self.last_repaint_rects = clips
            .iter()
            .map(|c| DamageRect {
                y: c.y0,
                height: c.y1 - c.y0,
            })
            .collect();

        let window_count = self.space.elements().count();

        // Log every 30 frames (once per second at 30fps)
        self.frame_counter = self.frame_counter.wrapping_add(1);
        let frame_counter = self.frame_counter;
        if frame_counter.is_multiple_of(30) {
            trace!("Rendering {} windows", window_count);
        }

        // Clear only the area being repainted; the rest stays as last frame.
        // Alpha is irrelevant here -- the buffer only ever feeds the BGRA->
        // YUV420P conversion in the encoder, which doesn't read it.
        for &clip in &clips {
            clear_region(&mut canvas, self.width, clip);

            // Render each window
            for window in self.space.elements() {
                let location = self.space.element_location(window).unwrap_or((0, 0).into());
                let window_pos_x = location.x.max(0) as u32;
                let window_pos_y = location.y.max(0) as u32;

                // Get the window's surface
                if let Some(surface) = window.wl_surface() {
                    // Access the surface buffer using renderer surface state
                    // on_commit_buffer_handler stores buffers in RendererSurfaceState, not SurfaceAttributes
                    with_renderer_surface_state(&surface, |state| {
                        if let Some(buffer) = state.buffer() {
                            // Buffer derefs to WlBuffer, so we can use it directly with with_buffer_contents
                            // Access SHM buffer contents
                            let shm_result = smithay::wayland::shm::with_buffer_contents(
                                buffer,
                                |ptr, len, buffer_data| {
                                    let buffer_width = buffer_data.width as u32;
                                    let buffer_height = buffer_data.height as u32;
                                    let buffer_stride = buffer_data.stride as u32;
                                    let buffer_offset = buffer_data.offset as isize;

                                    if frame_counter.is_multiple_of(120) {
                                        trace!(
                                            "Rendering buffer: {}x{}",
                                            buffer_width,
                                            buffer_height
                                        );
                                    }

                                    // Access pixel data safely
                                    let expected_len = (buffer_stride * buffer_height) as usize;
                                    if buffer_offset as usize + expected_len <= len {
                                        // SAFETY: the bounds check above guarantees
                                        // `[offset, offset+expected_len)` lies within the
                                        // mapped SHM pool of `len` bytes, so `ptr+offset`
                                        // is valid for `expected_len` reads for the life
                                        // of this borrow (the pool stays mapped).
                                        let pixel_data = unsafe {
                                            std::slice::from_raw_parts(
                                                ptr.offset(buffer_offset),
                                                expected_len,
                                            )
                                        };

                                        // Copy the buffer into the output at 1:1 scale (see
                                        // `blit_bgra`). Both SW and GL renderers blit at 1:1 so
                                        // `surface_at` can share one coordinate formula; the
                                        // zero-cleared canvas leaves uncovered pixels black.
                                        blit_bgra(
                                            &mut canvas,
                                            self.width,
                                            self.height,
                                            window_pos_x,
                                            window_pos_y,
                                            pixel_data,
                                            buffer_width,
                                            buffer_height,
                                            buffer_stride,
                                            clip,
                                        );
                                    }
                                },
                            );

                            if matches!(
                                shm_result,
                                Err(smithay::wayland::shm::BufferAccessError::NotManaged)
                            ) {
                                if let Ok(spb) =
                                    smithay::wayland::single_pixel_buffer::get_single_pixel_buffer(
                                        buffer,
                                    )
                                {
                                    fill_solid(
                                        &mut canvas,
                                        self.width,
                                        self.height,
                                        window_pos_x,
                                        window_pos_y,
                                        spb.rgba8888(),
                                        clip,
                                    );
                                }
                            }
                        }
                    });
                }
            }

            // If no windows, show the classic Xorg "root weave" stipple: a 4x4
            // basket-weave bitmap (X11's default root window pattern before any
            // window manager or client connects), rendered in black and white.
            if window_count == 0 {
                render_root_weave(&mut canvas, self.width, self.height, clip);
            }
        } // end `for clip in clips`

        // Hand the canvas to the encoder, reusing the recycled buffer's
        // allocation, and keep the canvas for next frame's update.
        // Always a full copy. A partial copy (only the damaged rows) is unsafe:
        // when the encoder falls behind it skips frames and unions their damage
        // onto the *newest* frame's buffer (see `skip_to_newest_frame`), so that
        // buffer must hold current pixels for every row -- a partial copy leaves
        // the skipped frames' rows stale and the encoder converts garbage. The
        // damage-only saving is on the BGRA->YUV conversion (encoder) and the
        // compositing (above), not this memcpy.
        let mut output = reuse_buffer.unwrap_or_default();
        output.clear();
        output.extend_from_slice(&canvas);
        self.canvas = canvas;
        Some(output)
    }

    /// Resolves a point given in output-pixel coordinates to the topmost
    /// window plus that point translated into the window's own buffer-pixel
    /// space. Used by both touch and pointer injection.
    ///
    /// Every window is configured to occupy the entire output (see
    /// `configure_toplevel_fullscreen`). Both SW and GL renderers copy the
    /// buffer at 1:1 scale (clipping if the buffer is larger than the output,
    /// leaving black fill if it is smaller). `Space::element_under` hit-tests
    /// against the literal, possibly-stale buffer bbox, which would make
    /// most of a touch test client's window untouchable, so for hit testing
    /// any point within the output belongs to the topmost window.
    fn surface_at(
        &self,
        location: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        if location.x < 0.0
            || location.y < 0.0
            || location.x >= self.width as f64
            || location.y >= self.height as f64
        {
            return None;
        }

        let window = self.space.elements().last()?;
        let surface = window.wl_surface()?.into_owned();
        let render_location = self.space.element_location(window).unwrap_or((0, 0).into());

        let origin_x = render_location.x.max(0) as f64;
        let origin_y = render_location.y.max(0) as f64;
        let target_w = (self.width as f64 - origin_x).max(1.0);
        let target_h = (self.height as f64 - origin_y).max(1.0);
        let rel_x = (location.x - origin_x).clamp(0.0, target_w);
        let rel_y = (location.y - origin_y).clamp(0.0, target_h);

        // 1:1 mapping: compositor coordinates ARE surface-local coordinates.
        // Both SW and GL renderers now copy the buffer at 1:1 (clipping when
        // the buffer is larger than the output), so no bbox-based scale factor
        // is needed here. Using bbox dimensions as a scale multiplier was
        // consistent only with the old SW scale-to-fill path; it produced
        // wrong coordinates under GL (which always clips, never scales).
        let surface_local = Point::<f64, Logical>::from((rel_x, rel_y));
        Some((surface, surface_local))
    }

    /// Inject a new touch point at the given output-pixel coordinates.
    pub fn touch_down(&mut self, slot: i32, x: f64, y: f64) {
        let Some(touch) = self.seat.get_touch() else {
            return;
        };
        let location = Point::<f64, Logical>::from((x, y));
        let target = self.surface_at(location);
        tracing::debug!(
            "touch_down ({x:.1},{y:.1}): windows={}, surface={}",
            self.space.elements().count(),
            target.is_some()
        );
        let time = self.clock.now().as_millis();
        // The location handed to `TouchHandle::down` is delivered to the
        // client as-is, minus the focus origin we pass alongside it. We've
        // already done that translation ourselves in `surface_at`, so
        // pass a zero origin and let `event.location` be the final,
        // already-surface-local coordinate.
        let (focus, event_location) = match target {
            Some((surface, surface_local)) => {
                (Some((surface, Point::from((0.0, 0.0)))), surface_local)
            }
            None => (None, location),
        };
        touch.down(
            self,
            focus,
            &DownEvent {
                slot: TouchSlot::from(Some(slot as u32)),
                location: event_location,
                serial: SERIAL_COUNTER.next_serial(),
                time,
            },
        );
    }

    /// Update the position of an in-progress touch point.
    pub fn touch_motion(&mut self, slot: i32, x: f64, y: f64) {
        let Some(touch) = self.seat.get_touch() else {
            return;
        };
        let location = Point::<f64, Logical>::from((x, y));
        let target = self.surface_at(location);
        let time = self.clock.now().as_millis();
        let (focus, event_location) = match target {
            Some((surface, surface_local)) => {
                (Some((surface, Point::from((0.0, 0.0)))), surface_local)
            }
            None => (None, location),
        };
        touch.motion(
            self,
            focus,
            &MotionEvent {
                slot: TouchSlot::from(Some(slot as u32)),
                location: event_location,
                time,
            },
        );
    }

    /// End a touch point (finger lifted).
    pub fn touch_up(&mut self, slot: i32) {
        let Some(touch) = self.seat.get_touch() else {
            return;
        };
        let time = self.clock.now().as_millis();
        touch.up(
            self,
            &UpEvent {
                slot: TouchSlot::from(Some(slot as u32)),
                serial: SERIAL_COUNTER.next_serial(),
                time,
            },
        );
    }

    /// Marks the end of a batch of touch down/motion/up calls that logically
    /// belong together (e.g. all the touches in one browser `touchmove`
    /// event).
    pub fn touch_frame(&mut self) {
        if let Some(touch) = self.seat.get_touch() {
            touch.frame(self);
        }
    }

    /// Cancels the entire touch sequence. `wl_touch.cancel` has no per-slot
    /// variant -- it always ends every active touch point at once.
    pub fn touch_cancel(&mut self) {
        if let Some(touch) = self.seat.get_touch() {
            touch.cancel(self);
        }
    }

    /// Move the pointer to the given output-pixel coordinates.
    pub fn pointer_motion(&mut self, x: f64, y: f64) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let mut location = Point::<f64, Logical>::from((x, y));

        // Honor an active pointer constraint on the focused (topmost) surface: a
        // lock freezes the pointer (the browser switches to relative deltas via
        // `pointer_relative_motion`), a confine clamps it to the region.
        if let Some(surface) = self
            .space
            .elements()
            .last()
            .and_then(|w| w.wl_surface())
            .map(|s| s.into_owned())
        {
            enum Action {
                Pass,
                Suppress,
                Clamp(Option<RegionAttributes>),
            }
            let action = with_pointer_constraint(&surface, &pointer, |c| match c {
                Some(c) if c.is_active() => match &*c {
                    PointerConstraint::Locked(_) => Action::Suppress,
                    PointerConstraint::Confined(confined) => {
                        Action::Clamp(confined.region().cloned())
                    }
                },
                _ => Action::Pass,
            });
            match action {
                // Locked: the pointer must stay put; relative deltas carry motion.
                Action::Suppress => return,
                Action::Clamp(region) => {
                    if let Some(region) = region {
                        location = clamp_point_to_region(location, &region);
                    }
                }
                Action::Pass => {}
            }
        }

        let target = self.surface_at(location);
        tracing::debug!(
            "pointer_motion ({x:.1},{y:.1}): surface={}",
            target.is_some()
        );
        let time = self.clock.now().as_millis();
        let (focus, event_location) = match target {
            Some((surface, surface_local)) => {
                (Some((surface, Point::from((0.0, 0.0)))), surface_local)
            }
            None => (None, location),
        };
        pointer.motion(
            self,
            focus,
            &PointerMotionEvent {
                location: event_location,
                serial: SERIAL_COUNTER.next_serial(),
                time,
            },
        );
    }

    /// Press or release a pointer button (Linux button code, e.g. `BTN_LEFT`).
    pub fn pointer_button(&mut self, button: u32, pressed: bool) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let time = self.clock.now().as_millis();
        pointer.button(
            self,
            &ButtonEvent {
                serial: SERIAL_COUNTER.next_serial(),
                time,
                button,
                state: if pressed {
                    ButtonState::Pressed
                } else {
                    ButtonState::Released
                },
            },
        );
    }

    /// Scroll by the given amount (wheel or trackpad delta, in surface-local pixels).
    pub fn pointer_axis(&mut self, delta_x: f64, delta_y: f64) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let time = self.clock.now().as_millis();
        // `Continuous` rather than `Wheel`: the browser can't tell us whether
        // the delta came from a touchpad or a notched wheel, and tagging it
        // `Wheel` makes clients like GTK accumulate deltas up to a discrete
        // click threshold (~10px) before scrolling -- exactly the "have to
        // scroll far before anything happens" behavior on a touchpad, whose
        // per-event deltas are only a few pixels. `Continuous` is applied
        // immediately with no notch quantization, which also scrolls fine
        // for real wheel deltas (they're just applied smoothly instead of
        // as separate clicks).
        let frame = AxisFrame::new(time)
            .source(AxisSource::Continuous)
            .value(Axis::Horizontal, delta_x)
            .value(Axis::Vertical, delta_y);
        pointer.axis(self, frame);
    }

    /// Marks the end of a batch of pointer motion/button/axis calls that
    /// logically belong together (e.g. a motion plus the button event it's
    /// paired with), mirroring `touch_frame`.
    pub fn pointer_frame(&mut self) {
        if let Some(pointer) = self.seat.get_pointer() {
            pointer.frame(self);
        }
    }

    /// Injects relative pointer motion (browser Pointer Lock `movementX/Y`)
    /// while a client holds a pointer lock. Smithay only delivers relative
    /// events to the *focused* pointer surface -- the same topmost window the
    /// lock is on -- so focus is already established by the absolute motion that
    /// preceded the lock.
    pub fn pointer_relative_motion(&mut self, dx: f64, dy: f64) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let utime = self.clock.now().as_micros();
        let focus = self
            .space
            .elements()
            .last()
            .and_then(|w| w.wl_surface())
            .map(|s| (s.into_owned(), Point::from((0.0, 0.0))));
        let delta = Point::<f64, Logical>::from((dx, dy));
        pointer.relative_motion(
            self,
            focus,
            &RelativeMotionEvent {
                delta,
                delta_unaccel: delta,
                utime,
            },
        );
        pointer.frame(self);
    }

    /// Recomputes whether the focused surface holds an active pointer *lock*,
    /// activating a not-yet-active constraint on it as a side effect (single
    /// fullscreen window -> always granted), and returns `Some(locked)` when the
    /// state changed since the last call. The main loop forwards the change to
    /// the browser as `ServerMessage::PointerLock` so it can enter/leave Pointer
    /// Lock. Polled because the constraints protocol has no deactivation
    /// callback -- a client can drop its lock at any time.
    pub fn take_pointer_lock_pending(&mut self) -> Option<bool> {
        let locked = self.refresh_pointer_lock();
        if locked != self.pointer_locked {
            self.pointer_locked = locked;
            Some(locked)
        } else {
            None
        }
    }

    fn refresh_pointer_lock(&mut self) -> bool {
        let Some(pointer) = self.seat.get_pointer() else {
            return false;
        };
        let Some(surface) = self
            .space
            .elements()
            .last()
            .and_then(|w| w.wl_surface())
            .map(|s| s.into_owned())
        else {
            return false;
        };
        with_pointer_constraint(&surface, &pointer, |c| {
            let Some(c) = c else { return false };
            // Grant a still-pending constraint (lock or confine).
            if !c.is_active() {
                c.activate();
            }
            // Only a lock drives the browser into relative-motion mode; a
            // confine is handled entirely server-side by clamping in
            // `pointer_motion`.
            matches!(&*c, PointerConstraint::Locked(_))
        })
    }

    /// The fractional scale actually advertised to clients: the browser's
    /// requested scale on the GL backend (which composites scaled buffers
    /// correctly via the renderer), or 1.0 on the SW backend, whose 1:1 blit
    /// would crop a larger fractional buffer. `dmabuf_renderer` is `Some`
    /// exactly when the GL backend is active (see `enable_dmabuf`).
    fn effective_scale(&self) -> f64 {
        if self.dmabuf_renderer.is_some() {
            self.preferred_scale
        } else {
            1.0
        }
    }

    /// Updates the browser-requested fractional scale (its devicePixelRatio when
    /// the native-resolution toggle is on, else 1.0) and re-advertises it to
    /// every mapped surface: both the `wp_fractional_scale_v1` value and the
    /// integer `wl_surface.preferred_buffer_scale` (v6) fallback for clients
    /// that don't implement fractional scaling. Called from the resize path.
    pub fn set_preferred_scale(&mut self, scale: f64) {
        if scale <= 0.0 || scale == self.preferred_scale {
            return;
        }
        self.preferred_scale = scale;
        let effective = self.effective_scale();
        let int_scale = (effective.round() as i32).max(1);
        let surfaces: Vec<WlSurface> = self
            .space
            .elements()
            .filter_map(|w| w.wl_surface().map(|s| s.into_owned()))
            .collect();
        for surface in surfaces {
            with_states(&surface, |states| {
                with_fractional_scale(states, |fs| fs.set_preferred_scale(effective));
                send_surface_state(&surface, states, int_scale, Transform::Normal);
            });
        }
    }

    /// Press or release a key (Linux evdev keycode, e.g. `KEY_A`).
    pub fn key(&mut self, keycode: u32, pressed: bool) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        let time = self.clock.now().as_millis();
        // xkbcommon's `Keycode` uses XKB/X11 numbering, which is evdev + 8.
        keyboard.input::<(), _>(
            self,
            smithay::input::keyboard::Keycode::new(keycode + 8),
            if pressed {
                KeyState::Pressed
            } else {
                KeyState::Released
            },
            SERIAL_COUNTER.next_serial(),
            time,
            |_, _, _| FilterResult::Forward,
        );
    }

    /// Sets keyboard focus to the topmost mapped window, mirroring
    /// `surface_at`'s "topmost window always wins" hit-testing model --
    /// this compositor only ever has one full-screen-configured topmost
    /// window at a time, so there's no separate focus-follows-click policy
    /// to track.
    fn update_keyboard_focus(&mut self) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        let surface = self
            .space
            .elements()
            .last()
            .and_then(|w| w.wl_surface())
            .map(|s| s.into_owned());
        let serial = SERIAL_COUNTER.next_serial();
        keyboard.set_focus(self, surface, serial);
        // Focus moved to a different topmost window -> its title/app_id is now
        // the one to show in the browser tab.
        self.refresh_app_meta();
    }

    /// Recomputes the title/app_id of the topmost (focused) toplevel and, if
    /// either changed, stages them for the main loop to forward to clients
    /// (see `take_app_meta_pending`). Reading only the topmost window means a
    /// title change on a background window is naturally ignored.
    fn refresh_app_meta(&mut self) {
        let (title, app_id) = self
            .space
            .elements()
            .last()
            .and_then(|w| w.toplevel().cloned())
            .map(|tl| {
                with_states(tl.wl_surface(), |states| {
                    match states.data_map.get::<XdgToplevelSurfaceData>() {
                        Some(data) => {
                            let attrs = data.lock().unwrap();
                            (
                                attrs.title.clone().unwrap_or_default(),
                                attrs.app_id.clone().unwrap_or_default(),
                            )
                        }
                        None => (String::new(), String::new()),
                    }
                })
            })
            .unwrap_or_default();

        if title != self.current_title || app_id != self.current_app_id {
            self.current_title = title;
            self.current_app_id = app_id;
            self.app_meta_dirty = true;
        }
    }

    /// Consumes and returns the topmost window's `(title, app_id, favicon)` if
    /// any changed since the last call (`favicon` is raw top-down RGBA
    /// `(w, h, bytes)`). `None` when unchanged. The main loop pushes the result
    /// onto the app-metadata watch channel (see `ServerMessage::Title`/`Favicon`).
    pub fn take_app_meta_pending(&mut self) -> Option<AppMeta> {
        if self.app_meta_dirty {
            self.app_meta_dirty = false;
            Some((
                self.current_title.clone(),
                self.current_app_id.clone(),
                self.current_favicon.clone(),
            ))
        } else {
            None
        }
    }

    /// Reads the topmost toplevel's committed `xdg_toplevel_icon` (if any) and,
    /// if it changed, stages it as the new favicon. Called from `commit()` once
    /// a toplevel flagged by `set_icon` commits (the icon is double-buffered
    /// with that commit). Named icons can't be resolved in the browser, so only
    /// pixel buffers are surfaced; a name-only icon leaves the favicon cleared.
    fn extract_toplevel_icon(&mut self, surface: &WlSurface) {
        // Only the topmost (focused) window's icon is shown.
        let is_topmost = self
            .space
            .elements()
            .last()
            .and_then(|w| w.wl_surface())
            .map(|s| &*s == surface)
            .unwrap_or(false);
        if !is_topmost {
            return;
        }

        let renderer = self.dmabuf_renderer.clone();
        let favicon = with_states(surface, |states| {
            let mut icon = states.cached_state.get::<ToplevelIconCachedState>();
            let cur = icon.current();
            // Pick the largest readable pixel buffer (capped at 256px/side) so
            // the browser gets the crispest favicon without us reading a huge
            // surface. Most clients provide a single buffer.
            let mut best: Option<(u32, u32, Vec<u8>)> = None;
            for (buf, _scale) in cur.buffers() {
                let Some(cand) = Self::read_wl_buffer_rgba(buf, renderer.as_ref()) else {
                    continue;
                };
                let replace = match &best {
                    None => true,
                    Some((bw, bh, _)) => {
                        cand.0 <= 256 && cand.1 <= 256 && cand.0 * cand.1 > bw * bh
                    }
                };
                if replace {
                    best = Some(cand);
                }
            }
            best
        });

        if favicon != self.current_favicon {
            self.current_favicon = favicon;
            self.app_meta_dirty = true;
        }
    }

    /// Consumes and returns any pending cursor update from the last
    /// `cursor_image()` or cursor-surface commit. `None` when the cursor
    /// hasn't changed since the last call.
    pub fn take_cursor_pending(&mut self) -> Option<CursorPending> {
        self.cursor_pending.take()
    }

    /// Reads the pixel data from the current cursor surface (if any) and
    /// stores it in `cursor_pending`. Safe to call both from `cursor_image`
    /// (when a new cursor is set) and from `commit` (for animated cursors).
    fn try_extract_cursor(&mut self) {
        let Some((wl_surface, hotspot)) = self.cursor_surface.clone() else {
            return;
        };
        let renderer = self.dmabuf_renderer.clone();
        match Self::read_cursor_pixels(&wl_surface, hotspot, renderer.as_ref()) {
            Some(pending) => {
                debug!("try_extract_cursor: extracted {:?}", pending.kind_name());
                self.cursor_pending = Some(pending);
            }
            None => {
                debug!("try_extract_cursor: no buffer yet, will retry on commit");
            }
        }
    }

    /// Reads `wl_surface`'s committed cursor buffer and wraps it as a
    /// `CursorPending::Surface` with the given hotspot.
    fn read_cursor_pixels(
        wl_surface: &WlSurface,
        hotspot: Point<i32, Logical>,
        renderer: Option<&Rc<RefCell<GlesRenderer>>>,
    ) -> Option<CursorPending> {
        let (width, height, rgba) = Self::read_buffer_rgba(wl_surface, renderer)?;
        Some(CursorPending::Surface {
            width,
            height,
            hotspot_x: hotspot.x,
            hotspot_y: hotspot.y,
            rgba,
        })
    }

    /// Reads `wl_surface`'s committed buffer (SHM or dmabuf) as tightly-packed,
    /// top-down RGBA `(width, height, bytes)`. `None` if no buffer is committed
    /// yet or the format is unsupported. Shared by the cursor and toplevel-icon
    /// (favicon) extraction paths.
    fn read_buffer_rgba(
        wl_surface: &WlSurface,
        renderer: Option<&Rc<RefCell<GlesRenderer>>>,
    ) -> Option<(u32, u32, Vec<u8>)> {
        let mut result: Option<(u32, u32, Vec<u8>)> = None;

        let had_renderer_state = with_renderer_surface_state(wl_surface, |rstate| {
            let Some(buffer) = rstate.buffer() else {
                debug!("read_buffer_rgba: RendererSurfaceState exists but buffer is None");
                return;
            };
            result = Self::read_wl_buffer_rgba(buffer, renderer);
        });

        if had_renderer_state.is_none() {
            debug!("read_buffer_rgba: no RendererSurfaceState yet");
        }

        result
    }

    /// Reads a committed `wl_buffer` (SHM or dmabuf) as tightly-packed,
    /// top-down RGBA `(width, height, bytes)`. `None` if the format is
    /// unsupported. The buffer-level core shared by the cursor-surface path
    /// (`read_buffer_rgba`) and the toplevel-icon path, whose buffers come
    /// straight from `ToplevelIconCachedState` and aren't attached to a
    /// surface, so they can't go through the renderer surface state.
    fn read_wl_buffer_rgba(
        buffer: &WlBuffer,
        renderer: Option<&Rc<RefCell<GlesRenderer>>>,
    ) -> Option<(u32, u32, Vec<u8>)> {
        let mut result: Option<(u32, u32, Vec<u8>)> = None;

        // Try SHM first.
        let shm_result = with_buffer_contents(buffer, |ptr, len, data| {
            let w = data.width as u32;
            let h = data.height as u32;
            let stride = data.stride as u32;
            let offset = data.offset as isize;

            let is_bgra = matches!(
                data.format,
                wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888
            );
            if !is_bgra {
                return;
            }

            let expected = (stride * h) as usize;
            if (offset as usize).saturating_add(expected) > len {
                return;
            }

            // SAFETY: the (saturating) bounds check above guarantees
            // `[offset, offset+expected)` lies within the mapped SHM pool of
            // `len` bytes, so this read is in-bounds for the borrow's life.
            let pixels = unsafe { std::slice::from_raw_parts(ptr.offset(offset), expected) };
            let mut rgba = vec![0u8; (w * h * 4) as usize];
            for y in 0..h {
                for x in 0..w {
                    let src = (y * stride + x * 4) as usize;
                    let dst = (y * w + x) as usize * 4;
                    rgba[dst] = pixels[src + 2];
                    rgba[dst + 1] = pixels[src + 1];
                    rgba[dst + 2] = pixels[src];
                    rgba[dst + 3] = if data.format == wl_shm::Format::Xrgb8888 {
                        255
                    } else {
                        pixels[src + 3]
                    };
                }
            }
            result = Some((w, h, rgba));
        });

        if matches!(
            shm_result,
            Err(smithay::wayland::shm::BufferAccessError::NotManaged)
        ) {
            // SHM failed — try dmabuf (GPU clients back surfaces with dmabuf).
            if let Ok(dmabuf) = get_dmabuf(buffer) {
                result = Self::read_buffer_rgba_dmabuf(dmabuf, renderer);
            }
        }

        result
    }

    /// Reads RGBA pixels from a dmabuf cursor surface.
    /// Tries linear mmap first (fast, works if modifier is LINEAR/Invalid), then falls back
    /// to GL texture import + readback (works for tiled/vendor-modified dmabufs).
    fn read_buffer_rgba_dmabuf(
        dmabuf: &Dmabuf,
        renderer: Option<&Rc<RefCell<GlesRenderer>>>,
    ) -> Option<(u32, u32, Vec<u8>)> {
        use smithay::backend::allocator::Buffer as AllocBuffer;
        use smithay::backend::renderer::{ExportMem, ImportDma};
        use smithay::utils::Rectangle;

        let size = dmabuf.size();
        let w = size.w as u32;
        let h = size.h as u32;
        let fourcc = dmabuf.format().code;

        let is_argb = fourcc == Fourcc::Argb8888;
        let is_xrgb = fourcc == Fourcc::Xrgb8888;
        if !is_argb && !is_xrgb {
            debug!("read_buffer_rgba_dmabuf: unsupported format {fourcc:?}");
            return None;
        }

        // Fast path: linear dmabuf — mmap plane 0 and read stride-based pixels.
        if !dmabuf.has_modifier() {
            if let Some(stride) = dmabuf.strides().next() {
                let _ = dmabuf.sync_plane(0, DmabufSyncFlags::START | DmabufSyncFlags::READ);
                let map_result = dmabuf.map_plane(0, DmabufMappingMode::READ);
                let _ = dmabuf.sync_plane(0, DmabufSyncFlags::END | DmabufSyncFlags::READ);
                if let Ok(mapping) = map_result {
                    let expected = (stride * h) as usize;
                    if mapping.length() >= expected {
                        // SAFETY: the dmabuf plane is mapped READ for this scope
                        // (sync START/END bracket it) and `mapping.length()` is
                        // confirmed >= `expected`, so `mapping.ptr()` is valid for
                        // `expected` bytes while `mapping` is alive.
                        let pixels = unsafe {
                            std::slice::from_raw_parts(mapping.ptr() as *const u8, expected)
                        };
                        let mut rgba = vec![0u8; (w * h * 4) as usize];
                        for y in 0..h {
                            for x in 0..w {
                                let src = (y * stride + x * 4) as usize;
                                let dst = (y * w + x) as usize * 4;
                                rgba[dst] = pixels[src + 2];
                                rgba[dst + 1] = pixels[src + 1];
                                rgba[dst + 2] = pixels[src];
                                rgba[dst + 3] = if is_xrgb { 255 } else { pixels[src + 3] };
                            }
                        }
                        return Some((w, h, rgba));
                    }
                }
                debug!("read_buffer_rgba_dmabuf: linear mmap failed, trying GL readback");
            }
        }

        // GL readback path: import the dmabuf as an EGL image → GL texture → readback.
        // Works for tiled modifiers that can't be stride-mmap'd.
        let Some(renderer) = renderer else {
            debug!("read_buffer_rgba_dmabuf: no GL renderer available for tiled dmabuf");
            return None;
        };

        let mut rend = match renderer.try_borrow_mut() {
            Ok(r) => r,
            Err(_) => {
                debug!("read_buffer_rgba_dmabuf: renderer already borrowed");
                return None;
            }
        };

        let texture = match rend.import_dmabuf(dmabuf, None) {
            Ok(t) => t,
            Err(e) => {
                debug!("read_buffer_rgba_dmabuf: import_dmabuf failed: {e:?}");
                return None;
            }
        };

        // Check if this texture can be attached to an FBO for readback
        // (external GL textures from OES_EGL_image_external cannot).
        match rend.can_read_texture(&texture) {
            Ok(false) | Err(_) => {
                debug!(
                    "read_buffer_rgba_dmabuf: texture not readable (likely external OES texture)"
                );
                return None;
            }
            Ok(true) => {}
        }

        let region = Rectangle::from_size((w as i32, h as i32).into());
        // Abgr8888 = GL_RGBA/UNSIGNED_BYTE = R,G,B,A bytes — no post-swap needed.
        let mapping = match rend.copy_texture(&texture, region, Fourcc::Abgr8888) {
            Ok(m) => m,
            Err(e) => {
                debug!("read_buffer_rgba_dmabuf: copy_texture failed: {e:?}");
                return None;
            }
        };

        let raw = match rend.map_texture(&mapping) {
            Ok(b) => b,
            Err(e) => {
                debug!("read_buffer_rgba_dmabuf: map_texture failed: {e:?}");
                return None;
            }
        };

        // The dmabuf has top-down pixel order (Wayland convention). GL imports
        // it with y-flip baked in (row 0 → t=0 = FBO bottom), so glReadPixels
        // starting at y=0 returns dmabuf row 0 = visual top. Data is already
        // top-down — no flip needed.
        let rgba = raw.to_vec();

        Some((w, h, rgba))
    }

    pub fn send_frames(&mut self) {
        // Send frame callbacks to all surfaces so they know when to render.
        //
        // `render()` copies surface buffers directly rather than going through
        // Smithay's renderer-based damage tracking, so no surface ever gets a
        // primary scan-out output recorded. With throttle = None, Smithay's
        // frame-callback helper treats every surface as never-overdue and
        // never sends a callback at all (see `SurfaceFrameThrottlingState::update`),
        // so clients that wait for `frame.done` before repainting (e.g. cage)
        // stall forever on their first, often-blank, buffer. Duration::ZERO
        // makes every surface "overdue" so a callback fires every time this is
        // called. Callers must therefore call this at the rate they actually
        // want clients to redraw at (e.g. once per render(), not once per
        // event-loop tick) or clients will repaint far faster than necessary.
        let time = self.clock.now();

        for window in self.space.elements() {
            window.send_frame(
                &self.output,
                time,
                Some(std::time::Duration::ZERO),
                |_, _| None,
            );
        }
    }
}

// Implement Smithay delegates for protocol handling
delegate_compositor!(WaylandWebStreamState);
delegate_xdg_shell!(WaylandWebStreamState);
delegate_shm!(WaylandWebStreamState);
delegate_single_pixel_buffer!(WaylandWebStreamState);
delegate_viewporter!(WaylandWebStreamState);
delegate_seat!(WaylandWebStreamState);
delegate_output!(WaylandWebStreamState);
delegate_dmabuf!(WaylandWebStreamState);
delegate_pointer_constraints!(WaylandWebStreamState);
delegate_relative_pointer!(WaylandWebStreamState);
delegate_fractional_scale!(WaylandWebStreamState);
delegate_keyboard_shortcuts_inhibit!(WaylandWebStreamState);
delegate_xdg_toplevel_icon!(WaylandWebStreamState);
delegate_cursor_shape!(WaylandWebStreamState);

// XDG Shell handler for window management
impl smithay::wayland::shell::xdg::XdgShellHandler for WaylandWebStreamState {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        debug!("New toplevel surface created");

        // Stage the fullscreen size/state, but don't send the configure yet:
        // `commit()` sends it on the client's first commit instead. Some
        // clients (e.g. wlroots' nested Wayland backend, used by labwc/sway
        // running nested) move their xdg_surface/xdg_toplevel proxies onto a
        // private queue only right before their own initial commit, then
        // busy-wait on just that queue for this configure. Sending it earlier
        // risks the bytes being read and demultiplexed into the client's
        // default queue before that swap happens, where they'd never be
        // dispatched -- a permanent, silent hang on the client side.
        self.configure_toplevel_fullscreen(&surface);

        let window = Window::new_wayland_window(surface);
        self.space.map_element(window, (0, 0), false);
        let full_damage = self.full_output_damage();
        self.add_damage(full_damage);
        self.update_keyboard_focus();

        debug!(
            "Window mapped to space. Total windows: {}",
            self.space.elements().count()
        );
    }

    fn new_popup(
        &mut self,
        _surface: smithay::wayland::shell::xdg::PopupSurface,
        _positioner: smithay::wayland::shell::xdg::PositionerState,
    ) {
        debug!("New popup surface created");
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        self.kicked_toplevels.remove(&surface.wl_surface().id());

        let window = self
            .space
            .elements()
            .find(|w| w.toplevel() == Some(&surface))
            .cloned();

        if let Some(window) = window {
            self.space.unmap_elem(&window);
            let full_damage = self.full_output_damage();
            self.add_damage(full_damage);
            self.update_keyboard_focus();
        }

        debug!(
            "Window unmapped from space. Total windows: {}",
            self.space.elements().count()
        );
    }

    fn title_changed(&mut self, _surface: ToplevelSurface) {
        // `refresh_app_meta` only reads the topmost window, so a title change on
        // a background window is a no-op; no need to filter on `_surface` here.
        self.refresh_app_meta();
    }

    fn app_id_changed(&mut self, _surface: ToplevelSurface) {
        self.refresh_app_meta();
    }

    fn grab(
        &mut self,
        _surface: smithay::wayland::shell::xdg::PopupSurface,
        _seat: wl_seat::WlSeat,
        _serial: smithay::utils::Serial,
    ) {
        // Handle popup grabs
    }

    fn reposition_request(
        &mut self,
        _surface: smithay::wayland::shell::xdg::PopupSurface,
        _positioner: smithay::wayland::shell::xdg::PositionerState,
        _token: u32,
    ) {
        // Handle reposition requests
    }
}

// Compositor handler
impl smithay::wayland::compositor::CompositorHandler for WaylandWebStreamState {
    fn compositor_state(&mut self) -> &mut SmithayCompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(
        &self,
        client: &'a smithay::reexports::wayland_server::Client,
    ) -> &'a CompositorClientState {
        client.get_data::<ClientState>().unwrap().compositor_state()
    }

    fn commit(&mut self, surface: &WlSurface) {
        // Handle surface commits - apply pending state
        use smithay::backend::renderer::utils::on_commit_buffer_handler;
        on_commit_buffer_handler::<Self>(surface);

        // If this is the current cursor surface being re-committed (animated
        // cursor or the client committed its buffer after calling set_cursor),
        // re-extract and forward the new pixels.
        let is_cursor_surface = self
            .cursor_surface
            .as_ref()
            .map(|(s, _)| s == surface)
            .unwrap_or(false);
        if is_cursor_surface {
            debug!("commit: retrying cursor extraction for cursor surface");
            self.try_extract_cursor();
        }

        // A toplevel that set an `xdg_toplevel_icon` commits the icon with its
        // surface; extract the new favicon now that the cached state is current.
        if self.icon_dirty_surface.as_ref() == Some(surface) {
            self.icon_dirty_surface = None;
            self.extract_toplevel_icon(surface);
        }

        // `Window::bbox()` is a cache that only `Window::on_commit()` refreshes;
        // without this, it stays at its initial (0,0) forever and `surface_at`'s
        // `.max(1)` fallback collapses every touch/pointer hit-test target to a
        // 1x1 box, regardless of where the client's buffer actually is.
        let window = self
            .space
            .elements()
            .find(|w| w.wl_surface().map(|s| &*s == surface).unwrap_or(false))
            .cloned();

        match &window {
            // Known, positioned window: compute the real damage this commit
            // carried and union just that into the accumulator.
            Some(window) => {
                let location = self.space.element_location(window).unwrap_or((0, 0).into());
                if let Some(rect) = Self::surface_damage(surface, location) {
                    self.add_damage(rect);
                }
            }
            // A surface we don't have a position for (e.g. not yet mapped) --
            // conservatively mark the whole output dirty rather than risk
            // missing a real change.
            None => {
                let full_damage = self.full_output_damage();
                self.add_damage(full_damage);
            }
        }

        if let Some(window) = window {
            window.on_commit();
            trace!("Window surface committed");

            // Nested wlroots compositors (e.g. sway run as this compositor's
            // client) don't size their own emulated output from the very
            // first xdg_toplevel configure -- that one only unblocks their
            // first commit, since nothing has been displayed yet for them to
            // resize *from*. They only actually adopt a suggested size from
            // a configure that arrives once they're already mapped and have
            // committed at least once, same as a real interactive window
            // resize would deliver. Manually proved out: resizing the
            // browser window after such a client has mapped (sending it
            // another, otherwise-identical configure) fixes it; restarting
            // the client without ever touching the browser reproduces the
            // undersized render every time. So immediately after a
            // toplevel's first-ever commit, send it a second configure
            // identical to the first -- this reproduces that fix
            // automatically instead of requiring the user to nudge the
            // browser window.
            if let Some(toplevel) = window.toplevel() {
                let surface_id = toplevel.wl_surface().id();
                if self.kicked_toplevels.insert(surface_id) {
                    self.configure_toplevel_fullscreen(toplevel);
                    toplevel.send_configure();
                }
            }
        }

        // Surface state is updated, frame callbacks will be sent in main loop
    }
}

// SHM buffer handler
impl ShmHandler for WaylandWebStreamState {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

// Buffer handler
impl BufferHandler for WaylandWebStreamState {
    fn buffer_destroyed(
        &mut self,
        _buffer: &smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer,
    ) {
        // Handle buffer destruction
    }
}

// Output handler
impl OutputHandler for WaylandWebStreamState {}

// `linux-dmabuf` handler (AGENTS.md). Only
// reachable once `enable_dmabuf` has run (`gl` compositor backend); the
// global itself isn't advertised otherwise, so `dmabuf_imported` only fires
// when `dmabuf_renderer` is actually `Some`.
impl DmabufHandler for WaylandWebStreamState {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        self.dmabuf_state
            .as_mut()
            .expect("dmabuf_imported fired without a registered global")
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        debug!(
            "dmabuf_imported called: format={:?} num_planes={} size={:?}",
            dmabuf.format(),
            dmabuf.num_planes(),
            (dmabuf.width(), dmabuf.height())
        );
        let imported = self
            .dmabuf_renderer
            .as_ref()
            .map(|renderer| renderer.borrow_mut().import_dmabuf(&dmabuf, None).is_ok())
            .unwrap_or(false);
        debug!("dmabuf_imported result: imported={imported}");

        if imported {
            if let Err(e) = notifier.successful::<Self>() {
                warn!("Failed to create wl_buffer for imported dmabuf: {e}");
            }
        } else {
            notifier.failed();
        }
    }
}

// Seat handler for input
impl smithay::input::SeatHandler for WaylandWebStreamState {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, _seat: &Seat<Self>, _focused: Option<&WlSurface>) {
        // Handle focus changes
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        debug!("cursor_image: {:?}", image);
        match image {
            CursorImageStatus::Hidden => {
                self.cursor_surface = None;
                self.cursor_pending = Some(CursorPending::Hidden);
            }
            CursorImageStatus::Named(icon) => {
                self.cursor_surface = None;
                self.cursor_pending =
                    Some(CursorPending::Named(cursor_icon_to_css(icon).to_owned()));
            }
            CursorImageStatus::Surface(wl_surface) => {
                let hotspot = with_states(&wl_surface, |states| {
                    states
                        .data_map
                        .get::<CursorImageSurfaceData>()
                        .map(|d| d.lock().unwrap().hotspot)
                        .unwrap_or_default()
                });
                debug!("cursor_image: Surface hotspot={:?}", hotspot);
                self.cursor_surface = Some((wl_surface, hotspot));
                self.try_extract_cursor();
            }
        }
    }
}

impl PointerConstraintsHandler for WaylandWebStreamState {
    fn new_constraint(
        &mut self,
        surface: &WlSurface,
        pointer: &smithay::input::pointer::PointerHandle<Self>,
    ) {
        // This compositor only ever has one fullscreen window, so a constraint
        // on the focused surface is always granted. Activate it immediately if
        // it's the focused surface; the per-tick `take_pointer_lock_pending`
        // poll then tells the browser to enter Pointer Lock.
        let focused = self
            .space
            .elements()
            .last()
            .and_then(|w| w.wl_surface())
            .map(|s| &*s == surface)
            .unwrap_or(false);
        if focused {
            with_pointer_constraint(surface, pointer, |c| {
                if let Some(c) = c {
                    c.activate();
                }
            });
        }
    }

    fn cursor_position_hint(
        &mut self,
        _surface: &WlSurface,
        _pointer: &smithay::input::pointer::PointerHandle<Self>,
        _location: smithay::utils::Point<f64, smithay::utils::Logical>,
    ) {
        // The locked client renders its own cursor at this hint, but the visible
        // cursor here is the browser's, which the Pointer Lock API restores to
        // its pre-lock position on exit -- so there's nothing for us to warp.
    }
}

impl FractionalScaleHandler for WaylandWebStreamState {
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        let scale = self.effective_scale();
        with_states(&surface, |states| {
            with_fractional_scale(states, |fs| fs.set_preferred_scale(scale));
        });
    }
}

impl KeyboardShortcutsInhibitHandler for WaylandWebStreamState {
    fn keyboard_shortcuts_inhibit_state(&mut self) -> &mut KeyboardShortcutsInhibitState {
        &mut self.keyboard_shortcuts_inhibit_state
    }

    fn new_inhibitor(&mut self, inhibitor: KeyboardShortcutsInhibitor) {
        inhibitor.activate();
    }
}

impl XdgToplevelIconHandler for WaylandWebStreamState {
    fn set_icon(&mut self, _toplevel: xdg_toplevel::XdgToplevel, wl_surface: WlSurface) {
        // The icon is now pending but is committed with the toplevel's next
        // commit (double-buffered); flag the surface so `commit()` reads it.
        self.icon_dirty_surface = Some(wl_surface);
    }
}

impl TabletSeatHandler for WaylandWebStreamState {}

// Client state to store per-client data
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientState {
    pub fn compositor_state(&self) -> &CompositorClientState {
        &self.compositor_state
    }
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

/// Maps smithay's `CursorIcon` (from `wp_cursor_shape_v1`) to the corresponding
/// CSS cursor name understood by browsers.
fn cursor_icon_to_css(icon: smithay::input::pointer::CursorIcon) -> &'static str {
    use smithay::input::pointer::CursorIcon;
    match icon {
        CursorIcon::Default => "default",
        CursorIcon::ContextMenu => "context-menu",
        CursorIcon::Help => "help",
        CursorIcon::Pointer => "pointer",
        CursorIcon::Progress => "progress",
        CursorIcon::Wait => "wait",
        CursorIcon::Cell => "cell",
        CursorIcon::Crosshair => "crosshair",
        CursorIcon::Text => "text",
        CursorIcon::VerticalText => "vertical-text",
        CursorIcon::Alias => "alias",
        CursorIcon::Copy => "copy",
        CursorIcon::Move => "move",
        CursorIcon::NoDrop => "no-drop",
        CursorIcon::NotAllowed => "not-allowed",
        CursorIcon::Grab => "grab",
        CursorIcon::Grabbing => "grabbing",
        CursorIcon::EResize => "e-resize",
        CursorIcon::NResize => "n-resize",
        CursorIcon::NeResize => "ne-resize",
        CursorIcon::NwResize => "nw-resize",
        CursorIcon::SResize => "s-resize",
        CursorIcon::SeResize => "se-resize",
        CursorIcon::SwResize => "sw-resize",
        CursorIcon::WResize => "w-resize",
        CursorIcon::EwResize => "ew-resize",
        CursorIcon::NsResize => "ns-resize",
        CursorIcon::NeswResize => "nesw-resize",
        CursorIcon::NwseResize => "nwse-resize",
        CursorIcon::ColResize => "col-resize",
        CursorIcon::RowResize => "row-resize",
        CursorIcon::AllScroll => "all-scroll",
        CursorIcon::ZoomIn => "zoom-in",
        CursorIcon::ZoomOut => "zoom-out",
        _ => "default",
    }
}

// Re-export as CompositorState for compatibility
pub type CompositorState = WaylandWebStreamState;

#[cfg(test)]
mod render_tests {
    use super::*;

    #[test]
    fn blit_bgra_copies_at_offset_and_clips() {
        // 4x4 output, blit a 2x2 source (stride 8 = 2px*4) at (1,1).
        let mut dst = vec![0u8; 4 * 4 * 4];
        let src = vec![0xAAu8; 2 * 2 * 4];
        blit_bgra(&mut dst, 4, 4, 1, 1, &src, 2, 2, 8, Clip::full(4, 4));
        let idx = (4 + 1) * 4; // pixel (1,1) in a 4-wide buffer, 4 bytes/px
        assert_eq!(&dst[idx..idx + 4], &[0xAA, 0xAA, 0xAA, 0xAA]);
        assert_eq!(&dst[0..4], &[0, 0, 0, 0]); // (0,0) untouched
    }

    #[test]
    fn blit_bgra_clips_oversized_source_without_panicking() {
        let mut dst = vec![0u8; 2 * 2 * 4];
        let src = vec![0x11u8; 4 * 4 * 4];
        blit_bgra(&mut dst, 2, 2, 1, 1, &src, 4, 4, 16, Clip::full(2, 2));
        let idx = (2 + 1) * 4; // pixel (1,1) in a 2-wide buffer, 4 bytes/px
        assert_eq!(&dst[idx..idx + 4], &[0x11, 0x11, 0x11, 0x11]);
    }

    #[test]
    fn blit_bgra_only_writes_inside_clip() {
        // 4x4 output, blit a full-cover 4x4 source but clip to the single
        // pixel (2,2). Only that pixel should change; the rest stays zero.
        let mut dst = vec![0u8; 4 * 4 * 4];
        let src = vec![0xCCu8; 4 * 4 * 4];
        let clip = Clip {
            x0: 2,
            y0: 2,
            x1: 3,
            y1: 3,
        };
        blit_bgra(&mut dst, 4, 4, 0, 0, &src, 4, 4, 16, clip);
        let inside = ((2 * 4) + 2) * 4; // pixel (2,2)
        assert_eq!(&dst[inside..inside + 4], &[0xCC, 0xCC, 0xCC, 0xCC]);
        // A pixel just outside the clip (1,2) is untouched.
        let outside = ((2 * 4) + 1) * 4;
        assert_eq!(&dst[outside..outside + 4], &[0, 0, 0, 0]);
        // Exactly one pixel was written.
        assert_eq!(dst.iter().filter(|&&b| b == 0xCC).count(), 4);
    }

    #[test]
    fn clear_region_zeroes_only_the_clip() {
        let mut dst = vec![0xFFu8; 4 * 4 * 4];
        clear_region(
            &mut dst,
            4,
            Clip {
                x0: 1,
                y0: 1,
                x1: 3,
                y1: 3,
            },
        );
        // 2x2 cleared region = 4 pixels = 16 bytes zeroed.
        assert_eq!(dst.iter().filter(|&&b| b == 0).count(), 16);
        let inside = (4 + 1) * 4; // pixel (1,1) in a 4-wide buffer: (1*4 + 1)*4
        assert_eq!(&dst[inside..inside + 4], &[0, 0, 0, 0]);
        assert_eq!(&dst[0..4], &[0xFF, 0xFF, 0xFF, 0xFF]); // (0,0) untouched
    }

    #[test]
    fn fill_solid_swaps_rgba_to_bgra() {
        let mut dst = vec![0u8; 4];
        fill_solid(&mut dst, 1, 1, 0, 0, [10, 20, 30, 40], Clip::full(1, 1)); // r,g,b,a
        assert_eq!(&dst[0..4], &[30, 20, 10, 40]); // b,g,r,a
    }

    #[test]
    fn render_root_weave_fills_every_pixel_opaque() {
        let (w, h) = (8u32, 8u32);
        let mut dst = vec![0u8; (w * h * 4) as usize];
        render_root_weave(&mut dst, w, h, Clip::full(w, h));
        for px in dst.chunks_exact(4) {
            assert_eq!(px[3], 255);
            assert!(px[0] == px[1] && px[1] == px[2]);
            assert!(px[0] == 0 || px[0] == 255);
        }
    }

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, Logical> {
        Rectangle::new((x, y).into(), (w, h).into())
    }

    #[test]
    fn accumulate_damage_keeps_disjoint_rects_separate() {
        let full = rect(0, 0, 1920, 1080);
        let (a, b) = (rect(0, 0, 16, 16), rect(1900, 1060, 16, 16));
        let mut set = Vec::new();
        accumulate_damage(&mut set, a, full);
        accumulate_damage(&mut set, b, full);
        // The whole point of #11: two corner damages stay two small rects,
        // not one near-fullscreen bounding box.
        assert_eq!(set, vec![a, b]);
    }

    #[test]
    fn accumulate_damage_dedups_exact_repeats() {
        let full = rect(0, 0, 1920, 1080);
        let a = rect(10, 10, 20, 20);
        let mut set = Vec::new();
        accumulate_damage(&mut set, a, full);
        accumulate_damage(&mut set, a, full);
        assert_eq!(
            set,
            vec![a],
            "an identical rect should not be tracked twice"
        );
    }

    #[test]
    fn accumulate_damage_full_output_supersedes_and_short_circuits() {
        let full = rect(0, 0, 1920, 1080);
        let mut set = Vec::new();
        accumulate_damage(&mut set, rect(10, 10, 20, 20), full);
        accumulate_damage(&mut set, full, full);
        assert_eq!(
            set,
            vec![full],
            "a full-output rect collapses the set to itself"
        );
        // Further adds are no-ops while fully damaged.
        accumulate_damage(&mut set, rect(5, 5, 5, 5), full);
        assert_eq!(set, vec![full]);
    }

    #[test]
    fn accumulate_damage_collapses_past_the_cap() {
        let full = rect(0, 0, 10000, 10000);
        let mut set = Vec::new();
        for i in 0..MAX_DAMAGE_RECTS as i32 {
            accumulate_damage(&mut set, rect(i * 100, 0, 10, 10), full);
        }
        assert_eq!(set.len(), MAX_DAMAGE_RECTS);
        accumulate_damage(&mut set, rect(9000, 9000, 10, 10), full);
        assert_eq!(set.len(), 1, "past the cap the set collapses to one bbox");
        let b = set[0];
        // The bbox must enclose both the first rect's origin and the last's far corner.
        assert!(b.loc.x <= 0 && b.loc.y <= 0);
        assert!(b.loc.x + b.size.w >= 9010 && b.loc.y + b.size.h >= 9010);
    }
}

#[cfg(test)]
mod cursor_tests {
    use super::*;
    use smithay::backend::allocator::{dmabuf::DmabufFlags, Modifier};
    use smithay::utils::{Buffer as BufSpace, Physical, Size};
    use std::io::Write;
    use std::os::unix::io::OwnedFd;

    /// Builds a `Dmabuf` backed by a temp file (no real GPU needed).
    /// Filled with `bgra_pixels` in row-major order. `Modifier::Linear`
    /// ensures `read_buffer_rgba_dmabuf` uses the mmap fast path.
    fn make_linear_dmabuf(w: u32, h: u32, fourcc: Fourcc, bgra_pixels: &[[u8; 4]]) -> Dmabuf {
        assert_eq!(bgra_pixels.len(), (w * h) as usize);
        let stride = w * 4;
        let mut tmp = tempfile::tempfile().expect("tempfile");
        for p in bgra_pixels {
            tmp.write_all(p).unwrap();
        }
        let fd: OwnedFd = tmp.into();
        let size = Size::<i32, BufSpace>::from((w as i32, h as i32));
        let mut builder = Dmabuf::builder(size, fourcc, Modifier::Linear, DmabufFlags::empty());
        builder.add_plane(fd, 0, 0, stride);
        builder.build().expect("Dmabuf::build")
    }

    #[test]
    fn sw_cursor_bgra_to_rgba_conversion() {
        // 2×2 ARGB8888 LE in memory = [B, G, R, A] per pixel.
        let bgra: [[u8; 4]; 4] = [
            [255, 0, 0, 255],   // blue pixel  → RGBA [  0,  0,255,255]
            [0, 255, 0, 255],   // green pixel → RGBA [  0,255,  0,255]
            [0, 0, 255, 255],   // red pixel   → RGBA [255,  0,  0,255]
            [255, 255, 0, 128], // cyan+alpha  → RGBA [  0,255,255,128]
        ];
        let dmabuf = make_linear_dmabuf(2, 2, Fourcc::Argb8888, &bgra);

        let (width, height, rgba) = WaylandWebStreamState::read_buffer_rgba_dmabuf(&dmabuf, None)
            .expect("expected Some((w, h, rgba))");

        assert_eq!((width, height), (2, 2));
        assert_eq!(rgba.len(), 16);
        assert_eq!(&rgba[0..4], &[0, 0, 255, 255], "pixel 0: blue   BGRA→RGBA");
        assert_eq!(&rgba[4..8], &[0, 255, 0, 255], "pixel 1: green  BGRA→RGBA");
        assert_eq!(&rgba[8..12], &[255, 0, 0, 255], "pixel 2: red    BGRA→RGBA");
        assert_eq!(
            &rgba[12..16],
            &[0, 255, 255, 128],
            "pixel 3: cyan   BGRA→RGBA"
        );
    }

    #[test]
    fn sw_cursor_xrgb_alpha_forced_opaque() {
        // XRGB8888: the alpha byte in the buffer is ignored; output must be 255.
        let bgra: [[u8; 4]; 1] = [[100, 150, 200, 42]]; // A=42 in buffer
        let dmabuf = make_linear_dmabuf(1, 1, Fourcc::Xrgb8888, &bgra);

        let (_, _, rgba) =
            WaylandWebStreamState::read_buffer_rgba_dmabuf(&dmabuf, None).expect("Some");
        assert_eq!(
            rgba[3], 255,
            "Xrgb8888 must produce alpha=255 regardless of buffer byte"
        );
    }

    /// Opens the first available DRM render node or returns `None`.
    fn open_drm_render_node() -> Option<std::fs::File> {
        for path in [
            "/dev/dri/renderD128",
            "/dev/dri/renderD129",
            "/dev/dri/renderD130",
        ] {
            if let Ok(f) = std::fs::File::options().read(true).write(true).open(path) {
                return Some(f);
            }
        }
        None
    }

    /// GL readback test: allocates a GBM dmabuf (driver picks the modifier, which on
    /// real GPU hardware is tiled), fills it with solid red via a GL clear, then calls
    /// `read_buffer_rgba_dmabuf` to verify the GL texture-import + readback path.
    ///
    /// Skipped gracefully when no DRM render node or EGL/GL init fails.
    /// A `None` result is also accepted: some drivers return an OES external texture
    /// that can't be attached to an FBO; the test verifies the graceful fallback.
    #[test]
    fn hw_cursor_gl_readback() {
        use smithay::backend::allocator::{
            dmabuf::AsDmabuf as _,
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
            Allocator,
        };
        use smithay::backend::egl::{EGLContext, EGLDisplay};
        use smithay::backend::renderer::{Bind, Color32F, Frame as RendererFrame, Renderer};

        macro_rules! skip {
            ($msg:literal, $e:expr) => {{
                eprintln!(
                    concat!("hw_cursor_gl_readback: ", $msg, " ({e}), skipping"),
                    e = $e
                );
                return;
            }};
            ($msg:literal) => {{
                eprintln!(concat!("hw_cursor_gl_readback: ", $msg, ", skipping"));
                return;
            }};
        }

        // ── prerequisites ──────────────────────────────────────────────────
        let drm_file = match open_drm_render_node() {
            Some(f) => f,
            None => {
                skip!("no DRM render node found")
            }
        };
        let drm_for_egl = drm_file.try_clone().unwrap();
        let gbm = match GbmDevice::new(drm_file) {
            Ok(d) => d,
            Err(e) => {
                skip!("GBM init failed", e)
            }
        };
        let egl_gbm = GbmDevice::new(drm_for_egl).unwrap();
        // SAFETY: `egl_gbm` is a fresh GBM device owned exclusively by this test;
        // smithay tracks EGLDisplays by native-display identity internally.
        let display = match unsafe { EGLDisplay::new(egl_gbm) } {
            Ok(d) => d,
            Err(e) => {
                skip!("EGL display failed", e)
            }
        };
        let ctx = match EGLContext::new(&display) {
            Ok(c) => c,
            Err(e) => {
                skip!("EGL context failed", e)
            }
        };
        // SAFETY: `ctx` was just created above and is not current on any other
        // thread, which is what GlesRenderer::new requires.
        let renderer = match unsafe { GlesRenderer::new(ctx) } {
            Ok(r) => r,
            Err(e) => {
                skip!("GlesRenderer init failed", e)
            }
        };

        // ── allocate a cursor-sized dmabuf (driver picks modifier) ─────────
        let (w, h) = (16u32, 16u32);
        let mut gbm_alloc = GbmAllocator::new(gbm, GbmBufferFlags::RENDERING);
        let gbm_buf = match gbm_alloc.create_buffer(w, h, Fourcc::Argb8888, &[Modifier::Invalid]) {
            Ok(b) => b,
            Err(e) => {
                skip!("GBM alloc failed", e)
            }
        };
        let mut dmabuf = match gbm_buf.export() {
            Ok(d) => d,
            Err(e) => {
                skip!("dmabuf export failed", e)
            }
        };

        // ── fill the dmabuf with solid red via GL clear ────────────────────
        let renderer_rc = Rc::new(RefCell::new(renderer));
        {
            let mut rend = renderer_rc.borrow_mut();
            let mut target = match rend.bind(&mut dmabuf) {
                Ok(t) => t,
                Err(e) => {
                    skip!("bind failed", e)
                }
            };
            let output_size = Size::<i32, Physical>::from((w as i32, h as i32));
            let mut frame = rend
                .render(&mut target, output_size, Transform::Normal)
                .expect("render");
            frame
                .clear(
                    Color32F::new(1.0, 0.0, 0.0, 1.0),
                    &[Rectangle::from_size(output_size)],
                )
                .expect("clear");
            let _ = frame.finish().expect("finish");
        }

        // ── read pixels back via read_buffer_rgba_dmabuf ───────────────────
        let result = WaylandWebStreamState::read_buffer_rgba_dmabuf(&dmabuf, Some(&renderer_rc));

        match result {
            Some((width, height, rgba)) => {
                assert_eq!((width, height), (w, h));
                assert_eq!(rgba.len(), (w * h * 4) as usize);
                // GL clear to red should yield RGBA ≈ [255, 0, 0, 255] per pixel.
                for (i, pixel) in rgba.chunks_exact(4).enumerate() {
                    assert!(pixel[0] > 200, "pixel {i}: R={} expected ≈255", pixel[0]);
                    assert!(pixel[1] < 50, "pixel {i}: G={} expected ≈0", pixel[1]);
                    assert!(pixel[2] < 50, "pixel {i}: B={} expected ≈0", pixel[2]);
                    assert!(pixel[3] > 200, "pixel {i}: A={} expected ≈255", pixel[3]);
                }
                eprintln!("hw_cursor_gl_readback: GL readback succeeded on {w}×{h} dmabuf");
            }
            None => {
                // Some drivers return an OES external texture that can't bind to an FBO.
                // The function returning None is the correct graceful fallback.
                eprintln!("hw_cursor_gl_readback: returned None (OES external texture or unsupported format — expected on some drivers)");
            }
        }
    }
}
