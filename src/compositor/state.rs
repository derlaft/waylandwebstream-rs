// Complete compositor state implementation with full Wayland protocol support

use smithay::{
    backend::renderer::{
        pixman::PixmanRenderer,
        utils::with_renderer_surface_state,
    },
    delegate_compositor, delegate_output, delegate_seat, delegate_shm,
    delegate_xdg_shell,
    desktop::{Space, Window},
    input::{Seat, SeatState, pointer::CursorImageStatus},
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::{
        calloop::EventLoop,
        wayland_server::{
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::{wl_seat, wl_surface::WlSurface},
            Display,
        },
    },
    utils::{Clock, Monotonic},
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
                                
                                // Copy pixels from client buffer to output framebuffer
                                for y in 0..buffer_height.min(self.height - window_pos_y) {
                                    for x in 0..buffer_width.min(self.width - window_pos_x) {
                                        let dest_y = window_pos_y + y;
                                        let dest_x = window_pos_x + x;
                                        
                                        let src_idx = (y * buffer_stride + x * 4) as usize;
                                        let dest_idx = ((dest_y * self.width + dest_x) * 4) as usize;
                                        
                                        if src_idx + 3 < pixel_data.len() && dest_idx + 3 < render_buffer.len() {
                                            // Copy ARGB8888/XRGB8888 pixel
                                            render_buffer[dest_idx] = pixel_data[src_idx];
                                            render_buffer[dest_idx + 1] = pixel_data[src_idx + 1];
                                            render_buffer[dest_idx + 2] = pixel_data[src_idx + 2];
                                            render_buffer[dest_idx + 3] = pixel_data[src_idx + 3];
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
        
        // If no windows, show test pattern
        if window_count == 0 {
            for y in 0..self.height {
                for x in 0..self.width {
                    let idx = ((y * self.width + x) * 4) as usize;
                    render_buffer[idx] = (x % 256) as u8;     // B
                    render_buffer[idx + 1] = (y % 256) as u8; // G
                    render_buffer[idx + 2] = 128;              // R
                    render_buffer[idx + 3] = 255;              // A
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
    
    pub fn send_frames(&mut self) {
        // Send frame callbacks to all surfaces so they know when to render
        let time = self.clock.now();
        
        for window in self.space.elements() {
            window.send_frame(&self.output, time, None, |_, _| None);
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
        
        // Send initial configure to the client
        surface.with_pending_state(|state| {
            state.size = Some((self.width as i32, self.height as i32).into());
        });
        surface.send_configure();
        
        #[allow(deprecated)]
        let window = Window::new(surface);
        self.space.map_element(window, (0, 0), false);
        
        info!("Window mapped to space. Total windows: {}", self.space.elements().count());
    }
    
    fn new_popup(&mut self, _surface: smithay::wayland::shell::xdg::PopupSurface, _positioner: smithay::wayland::shell::xdg::PositionerState) {
        info!("New popup surface created");
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
