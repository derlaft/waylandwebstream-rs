// Complete compositor state implementation with full Wayland protocol support

use anyhow::{Context, Result};
use smithay::{
    backend::{
        allocator::{dmabuf::Dmabuf, Buffer as _},
        input::{Axis, AxisSource, ButtonState, KeyState, TouchSlot},
        renderer::{gles::GlesRenderer, utils::with_renderer_surface_state, ImportDma},
    },
    delegate_compositor, delegate_dmabuf, delegate_keyboard_shortcuts_inhibit, delegate_output,
    delegate_pointer_constraints, delegate_presentation, delegate_seat, delegate_shm,
    delegate_single_pixel_buffer, delegate_viewporter, delegate_xdg_shell,
    delegate_xdg_toplevel_icon,
    desktop::{Space, Window},
    input::{
        Seat, SeatState,
        keyboard::FilterResult,
        pointer::{AxisFrame, ButtonEvent, CursorImageStatus, MotionEvent as PointerMotionEvent},
        touch::{DownEvent, MotionEvent, UpEvent},
    },
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::{
        calloop::EventLoop,
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{
            backend::{ClientData, ClientId, DisconnectReason, ObjectId},
            protocol::{wl_seat, wl_surface::WlSurface},
            Display, DisplayHandle, Resource,
        },
    },
    utils::{Clock, Logical, Monotonic, Point, Rectangle, Transform, SERIAL_COUNTER},
    wayland::{
        buffer::BufferHandler,
        compositor::{
            CompositorClientState, CompositorState as SmithayCompositorState,
        },
        dmabuf::{DmabufFeedbackBuilder, DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier},
        output::{OutputManagerState, OutputHandler},
        shell::xdg::{
            XdgShellState, ToplevelSurface,
        },
        shm::{ShmState, ShmHandler},
        single_pixel_buffer::SinglePixelBufferState,
        viewporter::ViewporterState,
        seat::WaylandFocus,
        pointer_constraints::{PointerConstraintsHandler, PointerConstraintsState},
        presentation::PresentationState,
        keyboard_shortcuts_inhibit::{
            KeyboardShortcutsInhibitHandler, KeyboardShortcutsInhibitState,
            KeyboardShortcutsInhibitor,
        },
        xdg_toplevel_icon::{XdgToplevelIconHandler, XdgToplevelIconManager},
    },
};
use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use tracing::{info, trace, warn};

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
    pub presentation_state: PresentationState,
    #[allow(dead_code)]
    pub keyboard_shortcuts_inhibit_state: KeyboardShortcutsInhibitState,
    #[allow(dead_code)]
    pub xdg_toplevel_icon_manager: XdgToplevelIconManager,
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

    // Accumulated logical-space damage since the last `take_dirty()`, unioned
    // across every surface commit, window map/unmap, and resize that may
    // have changed the rendered picture. `None` means provably nothing
    // changed. Lets the main loop skip render()+encode() on frames where the
    // screen provably hasn't changed.
    damage: Option<Rectangle<i32, Logical>>,

    // Counts calls to `render()`, used to throttle its debug/trace logging.
    frame_counter: u32,

    // `linux-dmabuf` (hardware-acceleration-plan.md Phase B.4). Both `None`
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
}

impl WaylandWebStreamState {
    pub fn new(
        _event_loop: &mut EventLoop<Self>,
        display: &mut Display<Self>,
        width: u32,
        height: u32,
    ) -> Self {
        info!("Initializing full compositor with resolution {}x{}", width, height);

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
        let presentation_state = PresentationState::new::<Self>(&dh, 1 /* CLOCK_MONOTONIC */);
        let keyboard_shortcuts_inhibit_state = KeyboardShortcutsInhibitState::new::<Self>(&dh);
        let xdg_toplevel_icon_manager = XdgToplevelIconManager::new::<Self>(&dh);
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

        let output = Output::new(
            "HEADLESS-1".to_string(),
            physical_properties,
        );

        output.change_current_state(Some(mode), None, None, Some((0, 0).into()));
        output.set_preferred(mode);
        output.create_global::<Self>(&dh);

        // Create seat (input device manager)
        let mut seat = seat_state.new_wl_seat(&dh, "seat-0");
        seat.add_keyboard(Default::default(), 200, 25).unwrap();
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
            presentation_state,
            keyboard_shortcuts_inhibit_state,
            xdg_toplevel_icon_manager,
            seat_state,
            space,
            seat,
            output,
            width,
            height,
            clock: Clock::new(),
            damage: Some(Rectangle::new((0, 0).into(), (width as i32, height as i32).into())),
            frame_counter: 0,
            dmabuf_state: None,
            dmabuf_renderer: None,
            kicked_toplevels: HashSet::new(),
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
    pub fn take_dirty(&mut self) -> bool {
        self.damage.take().is_some()
    }

    /// Unions `rect` into the accumulated damage for the current frame.
    fn add_damage(&mut self, rect: Rectangle<i32, Logical>) {
        self.damage = Some(match self.damage {
            Some(existing) => existing.merge(rect),
            None => rect,
        });
    }

    /// Returns the rectangle covering the entire output, in logical space.
    fn full_output_damage(&self) -> Rectangle<i32, Logical> {
        Rectangle::new((0, 0).into(), (self.width as i32, self.height as i32).into())
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
            let rstate = states.data_map.get::<RendererSurfaceStateUserData>()?.lock().unwrap();
            let buffer_size = rstate.buffer_size()?;

            let counter_cell = states.data_map.get_or_insert(Cell::<CommitCounter>::default);
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

        self.output.change_current_state(Some(mode), None, None, None);
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

    /// Renders the current frame. `reuse_buffer`, if given, is an
    /// already-allocated buffer (typically handed back by the encoder once
    /// it's done with a previous frame) that gets cleared and rendered into
    /// instead of allocating a fresh ~8MB buffer every frame.
    pub fn render(&mut self, reuse_buffer: Option<Vec<u8>>) -> Option<Vec<u8>> {
        let buffer_size = (self.width * self.height * 4) as usize;
        let mut render_buffer = reuse_buffer.unwrap_or_default();
        render_buffer.resize(buffer_size, 0);
        // Alpha is irrelevant here -- this buffer only ever feeds the BGRA->
        // YUV420P conversion in the encoder, which doesn't read it -- so a
        // plain memset clear (vs. a per-pixel store loop) is safe.
        render_buffer.fill(0);

        let window_count = self.space.elements().count();

        // Log every 30 frames (once per second at 30fps)
        self.frame_counter = self.frame_counter.wrapping_add(1);
        let frame_counter = self.frame_counter;
        if frame_counter % 30 == 0 {
            trace!("Rendering {} windows", window_count);
        }

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
                            &*buffer,
                        |ptr, len, buffer_data| {
                            let buffer_width = buffer_data.width as u32;
                            let buffer_height = buffer_data.height as u32;
                            let buffer_stride = buffer_data.stride as u32;
                            let buffer_offset = buffer_data.offset as isize;
                            
                            if frame_counter % 120 == 0 {
                                trace!("Rendering buffer: {}x{}", buffer_width, buffer_height);
                            }

                            // Access pixel data safely
                            let expected_len = (buffer_stride * buffer_height) as usize;
                            if buffer_offset as usize + expected_len <= len {
                                let pixel_data = unsafe {
                                    std::slice::from_raw_parts(ptr.offset(buffer_offset), expected_len)
                                };
                                
                                // Scale the client buffer to fill the space available to it
                                // on the output. Windows are configured to match the output
                                // size, but we scale rather than copy 1:1 so the picture still
                                // fills the screen for a frame or two while a client catches up
                                // to a viewport resize (or doesn't honor it exactly).
                                let target_width = self.width.saturating_sub(window_pos_x);
                                let target_height = self.height.saturating_sub(window_pos_y);

                                if buffer_width > 0 && buffer_height > 0 && target_width > 0 && target_height > 0 {
                                    if buffer_width == target_width && buffer_height == target_height {
                                        // Steady-state case: the client buffer already matches
                                        // its target 1:1 (the common case, since windows are
                                        // configured fullscreen). Scaling only matters for the
                                        // frame or two a client lags a viewport resize by, so
                                        // there's no need to fast-path that path too -- just
                                        // copy row-by-row (respecting stride) instead of running
                                        // the per-pixel scaling loop below.
                                        let row_bytes = (buffer_width * 4) as usize;
                                        for y in 0..buffer_height {
                                            let src_idx = (y * buffer_stride) as usize;
                                            let dest_idx = (((window_pos_y + y) * self.width + window_pos_x) * 4) as usize;
                                            if src_idx + row_bytes <= pixel_data.len() && dest_idx + row_bytes <= render_buffer.len() {
                                                render_buffer[dest_idx..dest_idx + row_bytes]
                                                    .copy_from_slice(&pixel_data[src_idx..src_idx + row_bytes]);
                                            }
                                        }
                                    } else {
                                        for dest_y in 0..target_height {
                                            let src_y = (dest_y as u64 * buffer_height as u64 / target_height as u64) as u32;
                                            for dest_x in 0..target_width {
                                                let src_x = (dest_x as u64 * buffer_width as u64 / target_width as u64) as u32;

                                                let src_idx = (src_y * buffer_stride + src_x * 4) as usize;
                                                let dest_idx = (((window_pos_y + dest_y) * self.width + (window_pos_x + dest_x)) * 4) as usize;

                                                if src_idx + 3 < pixel_data.len() && dest_idx + 3 < render_buffer.len() {
                                                    // Copy ARGB8888/XRGB8888 pixel
                                                    render_buffer[dest_idx..dest_idx + 4]
                                                        .copy_from_slice(&pixel_data[src_idx..src_idx + 4]);
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    );

                        if matches!(shm_result, Err(smithay::wayland::shm::BufferAccessError::NotManaged)) {
                            if let Ok(spb) = smithay::wayland::single_pixel_buffer::get_single_pixel_buffer(&*buffer) {
                                let [r, g, b, a] = spb.rgba8888();
                                let target_width = self.width.saturating_sub(window_pos_x);
                                let target_height = self.height.saturating_sub(window_pos_y);
                                for dest_y in 0..target_height {
                                    for dest_x in 0..target_width {
                                        let dest_idx = (((window_pos_y + dest_y) * self.width + (window_pos_x + dest_x)) * 4) as usize;
                                        if dest_idx + 3 < render_buffer.len() {
                                            render_buffer[dest_idx]     = b;
                                            render_buffer[dest_idx + 1] = g;
                                            render_buffer[dest_idx + 2] = r;
                                            render_buffer[dest_idx + 3] = a;
                                        }
                                    }
                                }
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
            const ROOT_WEAVE_BITS: [u8; 4] = [0b0110, 0b1001, 0b1001, 0b0110];
            for y in 0..self.height {
                let row = ROOT_WEAVE_BITS[(y % 4) as usize];
                for x in 0..self.width {
                    let bit = (row >> (x % 4)) & 1;
                    let color = if bit == 1 { 255 } else { 0 };
                    let idx = ((y * self.width + x) * 4) as usize;
                    render_buffer[idx] = color;     // B
                    render_buffer[idx + 1] = color; // G
                    render_buffer[idx + 2] = color; // R
                    render_buffer[idx + 3] = 255;    // A
                }
            }
        }
        
        Some(render_buffer)
    }

    /// Resolves a point given in output-pixel coordinates to the topmost
    /// window plus that point translated into the window's own buffer-pixel
    /// space. Used by both touch and pointer injection.
    ///
    /// Every window is configured to occupy the entire output (see
    /// `configure_toplevel_fullscreen`), and `render()` scales whatever
    /// buffer a client has actually attached to fill the output regardless
    /// of the buffer's real pixel size -- a client can lag a viewport
    /// resize by a frame or more, or simply never resize at all (see the
    /// `wayland-touch-client` test client). `Space::element_under` hit-tests
    /// against the literal, possibly-stale buffer bbox, which would make
    /// most of a touch test client's window untouchable. So for hit
    /// testing, any point within the output belongs to the topmost window,
    /// scaled into its buffer space the same way `render()` scales the
    /// other direction.
    fn surface_at(&self, location: Point<f64, Logical>) -> Option<(WlSurface, Point<f64, Logical>)> {
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

        let bbox = window.bbox();
        let buffer_w = (bbox.size.w.max(1)) as f64;
        let buffer_h = (bbox.size.h.max(1)) as f64;

        let surface_local = Point::<f64, Logical>::from((rel_x * buffer_w / target_w, rel_y * buffer_h / target_h));
        Some((surface, surface_local))
    }

    /// Inject a new touch point at the given output-pixel coordinates.
    pub fn touch_down(&mut self, slot: i32, x: f64, y: f64) {
        let Some(touch) = self.seat.get_touch() else { return };
        let location = Point::<f64, Logical>::from((x, y));
        let target = self.surface_at(location);
        let time = self.clock.now().as_millis();
        // The location handed to `TouchHandle::down` is delivered to the
        // client as-is, minus the focus origin we pass alongside it. We've
        // already done that translation ourselves in `surface_at`, so
        // pass a zero origin and let `event.location` be the final,
        // already-surface-local coordinate.
        let (focus, event_location) = match target {
            Some((surface, surface_local)) => (Some((surface, Point::from((0.0, 0.0)))), surface_local),
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
        let Some(touch) = self.seat.get_touch() else { return };
        let location = Point::<f64, Logical>::from((x, y));
        let target = self.surface_at(location);
        let time = self.clock.now().as_millis();
        let (focus, event_location) = match target {
            Some((surface, surface_local)) => (Some((surface, Point::from((0.0, 0.0)))), surface_local),
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
        let Some(touch) = self.seat.get_touch() else { return };
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
        let Some(pointer) = self.seat.get_pointer() else { return };
        let location = Point::<f64, Logical>::from((x, y));
        let target = self.surface_at(location);
        let time = self.clock.now().as_millis();
        let (focus, event_location) = match target {
            Some((surface, surface_local)) => (Some((surface, Point::from((0.0, 0.0)))), surface_local),
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
        let Some(pointer) = self.seat.get_pointer() else { return };
        let time = self.clock.now().as_millis();
        pointer.button(
            self,
            &ButtonEvent {
                serial: SERIAL_COUNTER.next_serial(),
                time,
                button,
                state: if pressed { ButtonState::Pressed } else { ButtonState::Released },
            },
        );
    }

    /// Scroll by the given amount (wheel or trackpad delta, in surface-local pixels).
    pub fn pointer_axis(&mut self, delta_x: f64, delta_y: f64) {
        let Some(pointer) = self.seat.get_pointer() else { return };
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

    /// Press or release a key (Linux evdev keycode, e.g. `KEY_A`).
    pub fn key(&mut self, keycode: u32, pressed: bool) {
        let Some(keyboard) = self.seat.get_keyboard() else { return };
        let time = self.clock.now().as_millis();
        // xkbcommon's `Keycode` uses XKB/X11 numbering, which is evdev + 8.
        keyboard.input::<(), _>(
            self,
            smithay::input::keyboard::Keycode::new(keycode + 8),
            if pressed { KeyState::Pressed } else { KeyState::Released },
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
        let Some(keyboard) = self.seat.get_keyboard() else { return };
        let surface = self.space.elements().last().and_then(|w| w.wl_surface()).map(|s| s.into_owned());
        let serial = SERIAL_COUNTER.next_serial();
        keyboard.set_focus(self, surface, serial);
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
            window.send_frame(&self.output, time, Some(std::time::Duration::ZERO), |_, _| None);
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
delegate_presentation!(WaylandWebStreamState);
delegate_keyboard_shortcuts_inhibit!(WaylandWebStreamState);
delegate_xdg_toplevel_icon!(WaylandWebStreamState);

// XDG Shell handler for window management
impl smithay::wayland::shell::xdg::XdgShellHandler for WaylandWebStreamState {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }
    
    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        info!("New toplevel surface created");

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

        #[allow(deprecated)]
        let window = Window::new(surface);
        self.space.map_element(window, (0, 0), false);
        let full_damage = self.full_output_damage();
        self.add_damage(full_damage);
        self.update_keyboard_focus();

        info!("Window mapped to space. Total windows: {}", self.space.elements().count());
    }
    
    fn new_popup(&mut self, _surface: smithay::wayland::shell::xdg::PopupSurface, _positioner: smithay::wayland::shell::xdg::PositionerState) {
        info!("New popup surface created");
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        self.kicked_toplevels.remove(&surface.wl_surface().id());

        let window = self.space.elements()
            .find(|w| w.toplevel() == Some(&surface))
            .cloned();

        if let Some(window) = window {
            self.space.unmap_elem(&window);
            let full_damage = self.full_output_damage();
            self.add_damage(full_damage);
            self.update_keyboard_focus();
        }

        info!("Window unmapped from space. Total windows: {}", self.space.elements().count());
    }
    
    fn grab(&mut self, _surface: smithay::wayland::shell::xdg::PopupSurface, _seat: wl_seat::WlSeat, _serial: smithay::utils::Serial) {
        // Handle popup grabs
    }
    
    fn reposition_request(&mut self, _surface: smithay::wayland::shell::xdg::PopupSurface, _positioner: smithay::wayland::shell::xdg::PositionerState, _token: u32) {
        // Handle reposition requests
    }
}

// Compositor handler
impl smithay::wayland::compositor::CompositorHandler for WaylandWebStreamState {
    fn compositor_state(&mut self) -> &mut SmithayCompositorState {
        &mut self.compositor_state
    }
    
    fn client_compositor_state<'a>(&self, client: &'a smithay::reexports::wayland_server::Client) -> &'a CompositorClientState {
        client.get_data::<ClientState>().unwrap().compositor_state()
    }
    
    fn commit(&mut self, surface: &WlSurface) {
        // Handle surface commits - apply pending state
        use smithay::backend::renderer::utils::on_commit_buffer_handler;
        on_commit_buffer_handler::<Self>(surface);

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
    fn buffer_destroyed(&mut self, _buffer: &smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer) {
        // Handle buffer destruction
    }
}

// Output handler
impl OutputHandler for WaylandWebStreamState {}

// `linux-dmabuf` handler (hardware-acceleration-plan.md Phase B.4). Only
// reachable once `enable_dmabuf` has run (`gl` compositor backend); the
// global itself isn't advertised otherwise, so `dmabuf_imported` only fires
// when `dmabuf_renderer` is actually `Some`.
impl DmabufHandler for WaylandWebStreamState {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        self.dmabuf_state.as_mut().expect("dmabuf_imported fired without a registered global")
    }

    fn dmabuf_imported(&mut self, _global: &DmabufGlobal, dmabuf: Dmabuf, notifier: ImportNotifier) {
        info!(
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
        info!("dmabuf_imported result: imported={imported}");

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
    
    fn cursor_image(&mut self, _seat: &Seat<Self>, _image: CursorImageStatus) {
        // Handle cursor image changes
    }
}

impl PointerConstraintsHandler for WaylandWebStreamState {
    fn new_constraint(&mut self, _surface: &WlSurface, _pointer: &smithay::input::pointer::PointerHandle<Self>) {}
    fn cursor_position_hint(
        &mut self,
        _surface: &WlSurface,
        _pointer: &smithay::input::pointer::PointerHandle<Self>,
        _location: smithay::utils::Point<f64, smithay::utils::Logical>,
    ) {}
}

impl KeyboardShortcutsInhibitHandler for WaylandWebStreamState {
    fn keyboard_shortcuts_inhibit_state(&mut self) -> &mut KeyboardShortcutsInhibitState {
        &mut self.keyboard_shortcuts_inhibit_state
    }

    fn new_inhibitor(&mut self, inhibitor: KeyboardShortcutsInhibitor) {
        inhibitor.activate();
    }
}

impl XdgToplevelIconHandler for WaylandWebStreamState {}

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

// Re-export as CompositorState for compatibility
pub type CompositorState = WaylandWebStreamState;
