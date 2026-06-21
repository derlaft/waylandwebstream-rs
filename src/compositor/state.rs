// Complete compositor state implementation with full Wayland protocol support

use smithay::{
    backend::{
        input::TouchSlot,
        renderer::{
            pixman::PixmanRenderer,
            utils::with_renderer_surface_state,
        },
    },
    delegate_compositor, delegate_output, delegate_seat, delegate_shm,
    delegate_xdg_shell,
    desktop::{Space, Window},
    input::{
        Seat, SeatState,
        pointer::CursorImageStatus,
        touch::{DownEvent, MotionEvent, UpEvent},
    },
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::{
        calloop::EventLoop,
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::{wl_seat, wl_surface::WlSurface},
            Display,
        },
    },
    utils::{Clock, Logical, Monotonic, Point, SERIAL_COUNTER},
    wayland::{
        buffer::BufferHandler,
        compositor::{
            CompositorClientState, CompositorState as SmithayCompositorState,
        },
        output::{OutputManagerState, OutputHandler},
        shell::xdg::{
            XdgShellState, ToplevelSurface,
        },
        shm::{ShmState, ShmHandler},
        seat::WaylandFocus,
    },
};
use tracing::info;

pub struct WaylandWebStreamState {
    // Core Smithay states
    pub compositor_state: SmithayCompositorState,
    pub xdg_shell_state: XdgShellState,
    pub shm_state: ShmState,
    pub seat_state: SeatState<Self>,
    pub output_manager_state: OutputManagerState,
    
    // Desktop management
    pub space: Space<Window>,
    pub seat: Seat<Self>,
    
    // Output and rendering
    pub output: Output,
    pub renderer: Option<PixmanRenderer>,
    pub width: u32,
    pub height: u32,
    
    // Clock for timing
    pub clock: Clock<Monotonic>,
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
        
        // Initialize all Wayland protocol states
        let compositor_state = SmithayCompositorState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);
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

        // Create pixman renderer
        let renderer = PixmanRenderer::new().ok();

        Self {
            compositor_state,
            xdg_shell_state,
            shm_state,
            seat_state,
            output_manager_state,
            space,
            seat,
            output,
            renderer,
            width,
            height,
            clock: Clock::new(),
        }
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

    pub fn render(&mut self) -> Option<Vec<u8>> {
        let buffer_size = (self.width * self.height * 4) as usize;
        let mut render_buffer = vec![0u8; buffer_size];
        
        // Clear to black background
        for pixel in render_buffer.chunks_exact_mut(4) {
            pixel[0] = 0;   // B
            pixel[1] = 0;   // G
            pixel[2] = 0;   // R
            pixel[3] = 255; // A
        }
        
        let window_count = self.space.elements().count();
        
        // Log every 30 frames (once per second at 30fps)
        static mut FRAME_COUNTER: u32 = 0;
        unsafe {
            FRAME_COUNTER += 1;
            if FRAME_COUNTER % 30 == 0 {
                info!("Rendering {} windows", window_count);
            }
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
                        let _result = smithay::wayland::shm::with_buffer_contents(
                            &*buffer,
                        |ptr, len, buffer_data| {
                            let buffer_width = buffer_data.width as u32;
                            let buffer_height = buffer_data.height as u32;
                            let buffer_stride = buffer_data.stride as u32;
                            let buffer_offset = buffer_data.offset as isize;
                            
                            unsafe {
                                if FRAME_COUNTER % 120 == 0 {
                                    info!("Rendering buffer: {}x{}", buffer_width, buffer_height);
                                }
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
                    );
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

    pub fn get_framebuffer(&self) -> Vec<u8> {
        let buffer_size = (self.width * self.height * 4) as usize;
        vec![0u8; buffer_size]
    }
    
    pub fn surface_under_pointer(&self, position: (f64, f64)) -> Option<(WlSurface, (f64, f64))> {
        self.space.element_under(position).and_then(|(window, pos)| {
            window.wl_surface().map(|surface| (surface.into_owned(), (pos.x as f64, pos.y as f64)))
        })
    }

    /// Resolves a touch point given in output-pixel coordinates to the
    /// topmost window plus that point translated into the window's own
    /// buffer-pixel space.
    ///
    /// Every window is configured to occupy the entire output (see
    /// `configure_toplevel_fullscreen`), and `render()` scales whatever
    /// buffer a client has actually attached to fill the output regardless
    /// of the buffer's real pixel size -- a client can lag a viewport
    /// resize by a frame or more, or simply never resize at all (see the
    /// `wayland-touch-client` test client). `Space::element_under` hit-tests
    /// against the literal, possibly-stale buffer bbox, which would make
    /// most of a touch test client's window untouchable. So for touch
    /// targeting, any point within the output belongs to the topmost
    /// window, scaled into its buffer space the same way `render()` scales
    /// the other direction.
    fn touch_target_at(&self, location: Point<f64, Logical>) -> Option<(WlSurface, Point<f64, Logical>)> {
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
        let target = self.touch_target_at(location);
        let time = self.clock.now().as_millis();
        // The location handed to `TouchHandle::down` is delivered to the
        // client as-is, minus the focus origin we pass alongside it. We've
        // already done that translation ourselves in `touch_target_at`, so
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
        let target = self.touch_target_at(location);
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
delegate_seat!(WaylandWebStreamState);
delegate_output!(WaylandWebStreamState);

// XDG Shell handler for window management
impl smithay::wayland::shell::xdg::XdgShellHandler for WaylandWebStreamState {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }
    
    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        info!("New toplevel surface created");

        // Send initial configure to the client, sized to fill the output
        self.configure_toplevel_fullscreen(&surface);
        surface.send_configure();
        
        #[allow(deprecated)]
        let window = Window::new(surface);
        self.space.map_element(window, (0, 0), false);
        
        info!("Window mapped to space. Total windows: {}", self.space.elements().count());
    }
    
    fn new_popup(&mut self, _surface: smithay::wayland::shell::xdg::PopupSurface, _positioner: smithay::wayland::shell::xdg::PositionerState) {
        info!("New popup surface created");
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let window = self.space.elements()
            .find(|w| w.toplevel() == Some(&surface))
            .cloned();

        if let Some(window) = window {
            self.space.unmap_elem(&window);
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
        
        let is_window_surface = self.space.elements().any(|w| {
            w.wl_surface().map(|s| &*s == surface).unwrap_or(false)
        });
        
        if is_window_surface {
            info!("Window surface committed");
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
