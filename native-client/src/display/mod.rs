// Wayland display thread — smithay-client-toolkit 0.18 rewrite.
//
// SCTK takes over the boilerplate that was previously implemented by hand:
//   • wl_registry global binding
//   • xdg_wm_base ping / pong
//   • xdg_surface::Configure → ack_configure
//   • xdg-decoration negotiation (server-side preferred, client-side fallback)
//   • wl_seat capability binding → wl_keyboard / wl_pointer
//   • keyboard via libxkbcommon (dead keys, modifiers, compose)
//   • wl_output tracking (scale factor, transform)
//
// What we keep:
//   • Dedicated OS thread with a 1 ms tight-poll loop (no calloop) for
//     minimum render latency — each tick non-blocking-reads Wayland events,
//     drains the decoder's frame channel, and renders.
//   • ActiveRenderer (Shm / Egl) dispatch unchanged.
//   • Dispatch impls for our own protocol objects (WlShmPool + WlBuffer
//     with SlotId user data; WlBuffer with () for the EGL path).
//   • watch channels for size + close state; mpsc for input forwarding.

use anyhow::{Context, Result};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tokio::sync::watch;
use tracing::{debug, info, warn};

use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_keyboard, delegate_output, delegate_pointer,
    delegate_registry, delegate_seat, delegate_shm, delegate_xdg_shell, delegate_xdg_window,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers},
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
        Capability, SeatHandler, SeatState,
    },
    shell::{
        xdg::{
            window::{Window, WindowConfigure, WindowDecorations, WindowHandler},
            XdgShell,
        },
        WaylandSurface,
    },
    shm::{Shm, ShmHandler},
};

use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_buffer, wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm_pool, wl_surface},
    Connection, Dispatch, Proxy, QueueHandle,
};

use crate::decode::sw::DecodedFrame;
use crate::input::keymap::evdev_to_code;
use crate::render::egl::EglRenderer;
use crate::render::shm::{ShmRenderer, SlotId};
use crate::types::{KeyboardEvent, MouseEvent, PointerPoint, SignalingMessage};

// ── Public types ──────────────────────────────────────────────────────────────

/// Which rendering backend to use (passed from CLI).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RendererKind {
    /// CPU blit into shared memory via wl_shm (default, no GPU required).
    Shm,
    /// OpenGL ES 2.0 via EGL (GPU-composited fullscreen quad).
    Egl,
}

/// Handle returned by [`spawn_display_thread`].
pub struct DisplayHandle {
    pub size_rx: watch::Receiver<(u32, u32)>,
    pub close_rx: watch::Receiver<bool>,
    #[allow(dead_code)]
    pub render_counter: std::sync::Arc<std::sync::atomic::AtomicU64>,
    #[allow(dead_code)]
    pub release_counter: std::sync::Arc<std::sync::atomic::AtomicU64>,
    pub input_rx: tokio::sync::mpsc::Receiver<SignalingMessage>,
    /// Inject a synthetic compositor configure event for integration tests.
    /// Sending `(w, h)` causes the display thread to resize its renderer as
    /// if the compositor had sent an xdg_toplevel::configure at that size,
    /// without needing a real Wayland compositor to trigger it.
    #[allow(dead_code)]
    pub synthetic_resize_tx: std::sync::mpsc::SyncSender<(u32, u32)>,
}

/// Spawn the Wayland display thread.
pub fn spawn_display_thread(
    initial_size: (u32, u32),
    frame_rx: mpsc::Receiver<DecodedFrame>,
    renderer_kind: RendererKind,
) -> Result<DisplayHandle> {
    let (size_tx, size_rx) = watch::channel(initial_size);
    let (close_tx, close_rx) = watch::channel(false);
    let render_counter = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let release_counter = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let (input_tx, input_rx) = tokio::sync::mpsc::channel::<SignalingMessage>(64);
    let (synthetic_resize_tx, synthetic_resize_rx) = std::sync::mpsc::sync_channel::<(u32, u32)>(4);

    let counters = DisplayCounters {
        render: render_counter.clone(),
        release: release_counter.clone(),
    };

    thread::Builder::new()
        .name("wws-display".into())
        .spawn(move || {
            if let Err(e) = run_display_loop(
                initial_size,
                size_tx,
                close_tx,
                frame_rx,
                counters,
                input_tx,
                renderer_kind,
                synthetic_resize_rx,
            ) {
                warn!("display thread exited: {e:#}");
            }
        })
        .context("failed to spawn display thread")?;

    Ok(DisplayHandle {
        size_rx,
        close_rx,
        render_counter,
        release_counter,
        input_rx,
        synthetic_resize_tx,
    })
}

// ── Private types ─────────────────────────────────────────────────────────────

#[derive(Clone)]
struct DisplayCounters {
    render: std::sync::Arc<std::sync::atomic::AtomicU64>,
    release: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

/// Unified renderer: either SHM or EGL.
enum ActiveRenderer {
    Shm(ShmRenderer),
    Egl(EglRenderer),
}

impl ActiveRenderer {
    fn try_drain(
        &mut self,
        qh: &QueueHandle<DisplayState>,
        frame_rx: &mpsc::Receiver<DecodedFrame>,
    ) -> anyhow::Result<usize> {
        match self {
            Self::Shm(r) => r.try_drain(qh, frame_rx),
            Self::Egl(r) => r.drain_frames(frame_rx),
        }
    }

    /// Resize the renderer and immediately commit a frame at the new size so
    /// the compositor maps the window correctly without waiting for the next
    /// decoded video frame.  Returns false only in degenerate edge cases.
    fn resize_and_prime(&mut self, qh: &QueueHandle<DisplayState>, w: u32, h: u32) -> bool {
        match self {
            Self::Shm(r) => r.resize_and_prime(qh, w, h),
            Self::Egl(r) => {
                r.resize(w, h);
                match r.prime() {
                    Ok(()) => true,
                    Err(e) => {
                        warn!("EGL prime after resize: {e:#}");
                        false
                    }
                }
            }
        }
    }

    fn slot_state(&self) -> String {
        match self {
            Self::Shm(r) => r.slot_state(),
            Self::Egl(_) => "egl".into(),
        }
    }

    fn prime(&mut self) -> bool {
        match self {
            Self::Shm(r) => r.prime(),
            Self::Egl(r) => match r.prime() {
                Ok(()) => true,
                Err(e) => {
                    warn!("EGL prime failed: {e:#}");
                    false
                }
            },
        }
    }

    fn release_slot(&mut self, idx: SlotId) {
        if let Self::Shm(r) = self {
            r.release_slot(idx);
        }
    }
}

// ── Display loop ──────────────────────────────────────────────────────────────

fn run_display_loop(
    initial_size: (u32, u32),
    size_tx: watch::Sender<(u32, u32)>,
    close_tx: watch::Sender<bool>,
    frame_rx: mpsc::Receiver<DecodedFrame>,
    counters: DisplayCounters,
    input_tx: tokio::sync::mpsc::Sender<SignalingMessage>,
    renderer_kind: RendererKind,
    synthetic_resize_rx: std::sync::mpsc::Receiver<(u32, u32)>,
) -> Result<()> {
    let conn = Connection::connect_to_env()
        .context("could not connect to Wayland compositor (is $WAYLAND_DISPLAY set?)")?;

    // Raw wl_display* pointer for EGL initialisation (system backend).
    let wl_display_ptr = conn.backend().display_ptr() as *mut std::ffi::c_void;

    // Enumerate globals and create the event queue in one shot.
    let (globals, mut event_queue) = registry_queue_init(&conn).context("registry_queue_init")?;
    let qh = event_queue.handle();

    // Bind SCTK-managed globals.
    let compositor_state =
        CompositorState::bind(&globals, &qh).context("wl_compositor not available")?;
    let xdg_shell_state =
        XdgShell::bind(&globals, &qh).context("xdg_wm_base not available")?;
    let shm_state = Shm::bind(&globals, &qh).context("wl_shm not available")?;

    // Surface + window (no buffer yet; initial empty commit triggers configure).
    let surface = compositor_state.create_surface(&qh);
    let window =
        xdg_shell_state.create_window(surface, WindowDecorations::RequestServer, &qh);
    window.set_title("waylandwebstream");
    window.set_app_id("rs.waylandwebstream.client");
    window.set_min_size(Some((16, 16)));
    window.commit();

    let mut state = DisplayState {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        compositor_state,
        xdg_shell_state,
        shm_state,
        window,
        keyboard: None,
        pointer: None,
        pointer_pos: (0.5, 0.5),
        width: initial_size.0,
        height: initial_size.1,
        configured: false,
        size_tx,
        close_tx,
        input_tx,
        counters,
        renderer: None,
        pending_resize: None,
        last_render: None,
    };

    // First roundtrip: compositor delivers the initial Configure + seat capabilities.
    event_queue.roundtrip(&mut state).context("initial roundtrip")?;

    if !state.configured {
        warn!("no initial configure; using {initial_size:?}");
    }

    // Build renderer from the configured size.
    let wl_shm = state.shm_state.wl_shm().clone();
    let wl_surface = state.window.wl_surface().clone();

    let mut renderer = match renderer_kind {
        RendererKind::Shm => {
            let r = ShmRenderer::new(
                wl_shm,
                wl_surface,
                &qh,
                state.width,
                state.height,
                state.counters.render.clone(),
            )
            .context("create SHM renderer")?;
            ActiveRenderer::Shm(r)
        }
        RendererKind::Egl => {
            let surface_id = state.window.wl_surface().id();
            let r = EglRenderer::new(
                wl_display_ptr,
                surface_id,
                state.width,
                state.height,
                state.counters.render.clone(),
            )
            .context("create EGL renderer")?;
            ActiveRenderer::Egl(r)
        }
    };

    info!(
        "window ready: {}x{} renderer={:?}",
        state.width, state.height, renderer_kind
    );

    // Initial commit — makes the compositor map the window before the first frame.
    if !renderer.prime() {
        warn!("renderer: prime failed");
    }
    state.renderer = Some(renderer);

    // ── Main event loop ─────────────────────────────────────────────────────
    // Non-blocking: read Wayland socket, dispatch pending events, drain the
    // decoded-frame channel, render, sleep 1 ms.  No calloop overhead.
    loop {
        if *state.close_tx.borrow() {
            info!("window closed");
            break;
        }

        tracing::trace!(
            "tick: slot_state={}",
            state.renderer.as_ref().map(|r| r.slot_state()).unwrap_or_default()
        );

        // Non-blocking socket read then dispatch.
        if let Some(guard) = event_queue.prepare_read() {
            let _ = guard.read();
        }
        event_queue.dispatch_pending(&mut state).context("dispatch_pending")?;
        event_queue.flush().ok();

        // Synthetic resize injection (used by integration tests to simulate a
        // compositor configure without needing to resize an actual Wayland output).
        if let Ok((w, h)) = synthetic_resize_rx.try_recv() {
            if (w, h) != (state.width, state.height) {
                debug!("synthetic resize inject: {}x{} → {w}x{h}", state.width, state.height);
                state.width = w;
                state.height = h;
                let _ = state.size_tx.send((w, h));
                state.pending_resize = Some((w, h));
            }
        }

        // Apply any resize that arrived in this tick's configure event BEFORE
        // draining frames.  After ack_configure the next committed buffer must
        // have the new size; draining frames first could commit an old-sized
        // buffer between ack_configure and resize_and_prime (protocol violation).
        if let Some((w, h)) = state.pending_resize.take() {
            debug!("resize → {w}x{h}");
            if let Some(r) = state.renderer.as_mut() {
                if !r.resize_and_prime(&qh, w, h) {
                    warn!("resize_and_prime failed (all slots held by compositor?)");
                }
            }
            event_queue.flush().ok();
        }

        // Render the latest decoded frame (drops older ones).
        let frames_drained = match state.renderer.as_mut().unwrap().try_drain(&qh, &frame_rx) {
            Ok(n) => n,
            Err(e) => {
                warn!("renderer error: {e:#}");
                break;
            }
        };
        if frames_drained > 0 {
            state.last_render = Some(std::time::Instant::now());
        }

        if let Some(last) = state.last_render {
            if last.elapsed() > Duration::from_secs(2) {
                warn!(
                    "no frame rendered in {:.1}s — is the server compositor producing frames?",
                    last.elapsed().as_secs_f32()
                );
                state.last_render = Some(std::time::Instant::now());
            }
        }

        std::thread::sleep(Duration::from_millis(1));
        event_queue.flush().ok();
    }

    Ok(())
}

// ── State struct ──────────────────────────────────────────────────────────────

struct DisplayState {
    // SCTK protocol state (these own the underlying wayland objects).
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    // Kept alive so the WlCompositor / XdgWmBase proxies aren't destroyed;
    // SCTK's dispatch impls receive them via the proxy argument, not field access.
    #[allow(dead_code)]
    compositor_state: CompositorState,
    #[allow(dead_code)]
    xdg_shell_state: XdgShell,
    shm_state: Shm,

    // Top-level window (owns xdg_surface + xdg_toplevel + optional decoration).
    window: Window,

    // Input devices — present once the seat advertises the capability.
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    /// Last pointer position in 0..1 surface-local coordinates.
    pointer_pos: (f64, f64),

    width: u32,
    height: u32,
    /// Set on first WindowHandler::configure; used to gate renderer creation.
    configured: bool,

    size_tx: watch::Sender<(u32, u32)>,
    close_tx: watch::Sender<bool>,
    input_tx: tokio::sync::mpsc::Sender<SignalingMessage>,
    counters: DisplayCounters,

    renderer: Option<ActiveRenderer>,
    /// Pending resize from the most recent configure; applied on the next tick.
    pending_resize: Option<(u32, u32)>,
    last_render: Option<std::time::Instant>,
}

impl DisplayState {
    fn send_input(&self, msg: SignalingMessage) {
        if self.input_tx.try_send(msg).is_err() {
            debug!("input channel full; dropping event");
        }
    }
}

// ── SCTK handler implementations ──────────────────────────────────────────────

impl CompositorHandler for DisplayState {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        new_factor: i32,
    ) {
        debug!("scale factor → {new_factor}");
        // TODO: forward to server for HiDPI cursor / content scaling
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        // We poll every 1 ms rather than using frame callbacks; no-op here.
    }
}

impl OutputHandler for DisplayState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_output::WlOutput,
    ) {
    }
}

impl WindowHandler for DisplayState {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) {
        info!("xdg_toplevel::Close");
        let _ = self.close_tx.send(true);
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _window: &Window,
        configure: WindowConfigure,
        _serial: u32,
    ) {
        // SCTK has already sent ack_configure before calling us.
        // Fall back to the current window dimensions when the compositor sends
        // None for a dimension (meaning "client chooses") — NOT the original
        // initial_size, which would revert back to 1280×720 on e.g. tiling
        // configure events.
        let w = configure.new_size.0.map(|v| v.get()).unwrap_or(self.width);
        let h = configure.new_size.1.map(|v| v.get()).unwrap_or(self.height);

        if !self.configured {
            self.width = w;
            self.height = h;
            self.configured = true;
            // Signal main.rs so it sends the real compositor-assigned size as
            // the initial Resize, not the hard-coded INITIAL_WINDOW_SIZE.
            let _ = self.size_tx.send((w, h));
            debug!("initial configure: {w}x{h} decoration={:?}", configure.decoration_mode);
        } else if (w, h) != (self.width, self.height) {
            debug!("resize configure: {}x{} → {w}x{h}", self.width, self.height);
            self.width = w;
            self.height = h;
            let _ = self.size_tx.send((w, h));
            self.pending_resize = Some((w, h));
        }
    }
}

impl SeatHandler for DisplayState {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            match self.seat_state.get_keyboard(qh, &seat, None) {
                Ok(kb) => {
                    debug!("keyboard ready");
                    self.keyboard = Some(kb);
                }
                Err(e) => warn!("get_keyboard: {e}"),
            }
        }
        if capability == Capability::Pointer && self.pointer.is_none() {
            match self.seat_state.get_pointer(qh, &seat) {
                Ok(ptr) => {
                    debug!("pointer ready");
                    self.pointer = Some(ptr);
                }
                Err(e) => warn!("get_pointer: {e}"),
            }
        }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard {
            if let Some(kb) = self.keyboard.take() {
                kb.release();
            }
        }
        if capability == Capability::Pointer {
            if let Some(ptr) = self.pointer.take() {
                ptr.release();
            }
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl KeyboardHandler for DisplayState {
    fn enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _serial: u32,
        _raw: &[u32],
        _keysyms: &[Keysym],
    ) {
    }

    fn leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _serial: u32,
    ) {
    }

    fn press_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        event: KeyEvent,
    ) {
        // raw_code is the wayland/evdev scancode — identical to what the old
        // wl_keyboard::Event::Key { key } field carried, so evdev_to_code works as-is.
        match evdev_to_code(event.raw_code) {
            Some(code) => self.send_input(SignalingMessage::Key {
                event: KeyboardEvent::Down { code: code.to_string() },
            }),
            None => debug!(
                "unknown evdev scancode {} (keysym={:?})",
                event.raw_code, event.keysym
            ),
        }
    }

    fn release_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        event: KeyEvent,
    ) {
        if let Some(code) = evdev_to_code(event.raw_code) {
            self.send_input(SignalingMessage::Key {
                event: KeyboardEvent::Up { code: code.to_string() },
            });
        }
    }

    fn update_modifiers(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        _modifiers: Modifiers,
    ) {
        // TODO: encode modifier state in SignalingMessage for Ctrl+C, etc.
    }
}

impl PointerHandler for DisplayState {
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        let win_surface = self.window.wl_surface().clone();
        let w = self.width as f64;
        let h = self.height as f64;

        for event in events {
            if event.surface != win_surface {
                continue;
            }
            match event.kind {
                PointerEventKind::Enter { serial } => {
                    // Hide the local OS cursor — the remote app's cursor is
                    // composited into the video stream.
                    pointer.set_cursor(serial, None, 0, 0);
                    self.pointer_pos = norm(event.position, w, h);
                }
                PointerEventKind::Leave { .. } => {}
                PointerEventKind::Motion { .. } => {
                    self.pointer_pos = norm(event.position, w, h);
                    let (x, y) = self.pointer_pos;
                    self.send_input(SignalingMessage::Pointer {
                        event: MouseEvent::Move {
                            pointer: PointerPoint {
                                x,
                                y,
                                button: 0,
                                pointer_type: "mouse".into(),
                                pressure: 0.0,
                            },
                        },
                    });
                }
                PointerEventKind::Press { button, .. } => {
                    let (x, y) = self.pointer_pos;
                    self.send_input(SignalingMessage::Pointer {
                        event: MouseEvent::Down {
                            pointer: PointerPoint {
                                x,
                                y,
                                button: linux_to_browser_button(button) as i32,
                                pointer_type: "mouse".into(),
                                pressure: 0.0,
                            },
                        },
                    });
                }
                PointerEventKind::Release { button, .. } => {
                    let (x, y) = self.pointer_pos;
                    self.send_input(SignalingMessage::Pointer {
                        event: MouseEvent::Up {
                            pointer: PointerPoint {
                                x,
                                y,
                                button: linux_to_browser_button(button) as i32,
                                pointer_type: "mouse".into(),
                                pressure: 0.0,
                            },
                        },
                    });
                }
                PointerEventKind::Axis { horizontal, vertical, .. } => {
                    let (x, y) = self.pointer_pos;
                    self.send_input(SignalingMessage::Pointer {
                        event: MouseEvent::Wheel {
                            x,
                            y,
                            delta_x: horizontal.absolute,
                            delta_y: vertical.absolute,
                        },
                    });
                }
            }
        }
    }
}

impl ShmHandler for DisplayState {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm_state
    }
}

impl ProvidesRegistryState for DisplayState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

// ── Dispatch for our own protocol objects ─────────────────────────────────────

// wl_shm_pool with SlotId user data — created by ShmRenderer.
// SCTK's delegate_shm! only dispatches WlShm; WlShmPool is ours.
impl Dispatch<wl_shm_pool::WlShmPool, SlotId> for DisplayState {
    fn event(
        _: &mut Self,
        _: &wl_shm_pool::WlShmPool,
        _: wl_shm_pool::Event,
        _: &SlotId,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // wl_shm_pool has no events.
    }
}

// wl_buffer with SlotId — ShmRenderer's double-buffered slots.
// Release marks the slot free so the renderer can reuse it.
impl Dispatch<wl_buffer::WlBuffer, SlotId> for DisplayState {
    fn event(
        state: &mut Self,
        _: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        slot_id: &SlotId,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            state.counters.release.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            debug!("wl_buffer::Release slot={slot_id}");
            if let Some(r) = state.renderer.as_mut() {
                r.release_slot(*slot_id);
            }
        }
    }
}

// wl_buffer with () — safety net for any anonymous buffers (EGL path).
impl Dispatch<wl_buffer::WlBuffer, ()> for DisplayState {
    fn event(
        _: &mut Self,
        _: &wl_buffer::WlBuffer,
        _: wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

// ── Delegate macros ───────────────────────────────────────────────────────────

delegate_compositor!(DisplayState);
delegate_output!(DisplayState);
delegate_shm!(DisplayState);
delegate_seat!(DisplayState);
delegate_keyboard!(DisplayState);
delegate_pointer!(DisplayState);
delegate_xdg_shell!(DisplayState);
delegate_xdg_window!(DisplayState);
delegate_registry!(DisplayState);

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Normalise surface-local pixel position to 0..1.
#[inline]
fn norm(pos: (f64, f64), w: f64, h: f64) -> (f64, f64) {
    (pos.0 / w, pos.1 / h)
}

/// Map a Linux BTN_* evdev code to a browser button index.
#[inline]
fn linux_to_browser_button(button: u32) -> u32 {
    match button {
        0x110 => 0, // BTN_LEFT   → primary
        0x111 => 2, // BTN_RIGHT  → secondary
        0x112 => 1, // BTN_MIDDLE → auxiliary
        _ => 0,
    }
}
