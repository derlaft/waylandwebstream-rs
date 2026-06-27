// Wayland display wiring for the native client.
//
// Owns the synchronous Wayland event loop, wl_surface/xdg_toplevel plumbing,
// and the SHM renderer that blits decoded frames into wl_shm buffers. Runs on
// a dedicated OS thread (wayland-client 0.31 is synchronous). The tokio side
// feeds it decoded frames via `frame_rx` and reads back window size + close
// state through `tokio::sync::watch` channels -- keeping Wayland dispatch off
// the async executor entirely (see AGENTS.md "two execution domains").

use anyhow::{Context, Result};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tokio::sync::watch;
use tracing::{debug, info, warn};
use wayland_client::{
    protocol::{
        wl_buffer, wl_compositor, wl_keyboard, wl_pointer, wl_registry, wl_seat, wl_shm,
        wl_shm_pool, wl_surface,
    },
    Connection, Dispatch, QueueHandle, WEnum,
};
use wayland_protocols::xdg::shell::client::xdg_surface as xdg_surface_protocol;
use wayland_protocols::xdg::shell::client::xdg_toplevel as xdg_toplevel_protocol;
use wayland_protocols::xdg::shell::client::xdg_wm_base as xdg_wm_base_protocol;

use crate::decode::sw::DecodedFrame;
use crate::input::keymap::evdev_to_code;
use crate::render::shm::{ShmRenderer, SlotId};
use crate::types::{KeyboardEvent, MouseEvent, PointerPoint, SignalingMessage};

/// Handle to the Wayland display thread. Returned by
/// [`spawn_display_thread`]; the rest of the client reads window size and
/// close state through the two `watch` receivers. The matching frame
/// receiver (`frame_rx`) is the *second* tuple element of
/// `spawn_display_thread` -- it's intentionally separate so the
/// decoder thread (not the main loop) can own it.
pub struct DisplayHandle {
    /// Current window size in surface-local pixels, updated whenever
    /// `xdg_toplevel::Configure` fires (or set to `initial_size` until the
    /// first configure arrives).
    pub size_rx: watch::Receiver<(u32, u32)>,
    /// Flips to `true` when `xdg_toplevel::Close` fires. The display thread
    /// then breaks out of its loop.
    pub close_rx: watch::Receiver<bool>,
    /// Counter of successful `ShmRenderer::render` calls (i.e. a
    /// decoded frame was actually attached + committed to the
    /// surface). Smoke tests use this to assert "frames are
    /// reaching the window" without driving the compositor directly.
    /// Set up after `spawn_display_thread` returns, so read it
    /// lazily in tests that need it.
    #[allow(dead_code)] // read by integration tests, not by the main binary
    pub render_counter: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Counter of `wl_buffer::Release` events dispatched (i.e. the
    /// compositor told us a buffer is free to reuse). Should be
    /// `>= render_counter - 1` after steady state: every render
    /// after the first produces a Release for the previous buffer.
    /// If `release_count` is much lower, the renderer is starved
    /// of free slots and `pick_next_released` keeps returning None.
    #[allow(dead_code)] // read by integration tests, not by the main binary
    pub release_counter: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Input events (pointer, keyboard) translated to SignalingMessage and
    /// ready to forward to the server. Phase 7.
    pub input_rx: tokio::sync::mpsc::Receiver<SignalingMessage>,
}

/// Spawn the Wayland display thread. `frame_rx` is consumed by the
/// display thread (it's the source of decoded H.264 frames).
/// `DisplayHandle` is returned for `main` to read window size + close
/// state. The thread:
///   1. Connects to `$WAYLAND_DISPLAY`, binds the globals we need
///      (`wl_compositor`, `xdg_wm_base`, `wl_shm`).
///   2. Creates a `wl_surface` + `xdg_toplevel` titled "waylandwebstream".
///   3. Constructs a `ShmRenderer` (two wl_shm buffers) and commits
///      an initial buffer so the compositor maps the surface even
///      before the first decoded frame arrives.
///   4. Runs forever: polls `frame_rx` for decoded frames, polls
///      Wayland events, exits on `xdg_toplevel::Close`.
pub fn spawn_display_thread(
    initial_size: (u32, u32),
    frame_rx: mpsc::Receiver<DecodedFrame>,
) -> Result<DisplayHandle> {
    let (size_tx, size_rx) = watch::channel(initial_size);
    let (close_tx, close_rx) = watch::channel(false);
    // Shared with the renderer so observers (smoke tests, the main
    // loop's debug log) can read "frames attached to the surface"
    // without borrowing the renderer directly. See
    // `DisplayHandle::render_counter`.
    let render_counter = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let release_counter = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));

    // Input events from the Wayland event loop (pointer, keyboard) go
    // here; the tokio recv task forwards them to the server. Capacity
    // 64: typing a burst of keys or a fast pointer drag should never
    // fill it before the send loop drains it.
    let (input_tx, input_rx) = tokio::sync::mpsc::channel::<SignalingMessage>(64);

    let counters = DisplayCounters {
        render: render_counter.clone(),
        release: release_counter.clone(),
    };
    thread::Builder::new()
        .name("wws-display".into())
        .spawn(move || {
            if let Err(e) =
                run_display_loop(initial_size, size_tx, close_tx, frame_rx, counters, input_tx)
            {
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
    })
}

/// Counters shared between the display thread (writer) and the
/// outside world (reader). Bundled so we can pass them to
/// `run_display_loop` in one move instead of two.
#[derive(Clone)]
struct DisplayCounters {
    render: std::sync::Arc<std::sync::atomic::AtomicU64>,
    release: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

fn run_display_loop(
    initial_size: (u32, u32),
    size_tx: watch::Sender<(u32, u32)>,
    close_tx: watch::Sender<bool>,
    frame_rx: mpsc::Receiver<DecodedFrame>,
    counters: DisplayCounters,
    input_tx: tokio::sync::mpsc::Sender<SignalingMessage>,
) -> Result<()> {
    let conn = Connection::connect_to_env()
        .context("could not connect to Wayland compositor (is $WAYLAND_DISPLAY set?)")?;
    let display = conn.display();
    let mut event_queue = conn.new_event_queue();
    let qh = event_queue.handle();

    let mut state = DisplayState::new(initial_size, size_tx, close_tx.clone(), counters.clone(), input_tx);

    // First roundtrip: bind wl_compositor, xdg_wm_base, wl_seat, wl_shm.
    display.get_registry(&qh, ());
    event_queue
        .roundtrip(&mut state)
        .context("initial Wayland roundtrip")?;

    let compositor = state
        .compositor
        .take()
        .context("wl_compositor global missing")?;
    let wm_base = state
        .wm_base
        .take()
        .context("xdg_wm_base global missing")?;
    if state.seat.is_none() {
        warn!("wl_seat global missing -- input forwarding will not work");
    }
    let shm = state
        .shm
        .take()
        .context("wl_shm global missing -- cannot render")?;

    let surface = compositor.create_surface(&qh, ());
    let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg_surface.get_toplevel(&qh, ());
    toplevel.set_title("waylandwebstream".to_string());
    toplevel.set_app_id("wws-client".to_string());

    // Store the surface in state so the xdg_surface::Configure handler
    // can commit after ack_configure (required by the xdg-shell spec).
    state.surface = Some(surface.clone());

    // Initial commit to trigger the first Configure. xdg_surface::Configure
    // arrives in the next roundtrip; until then the compositor hasn't
    // allocated us a size, so we keep `state.width/height` at initial.
    surface.commit();
    event_queue
        .roundtrip(&mut state)
        .context("configure roundtrip")?;

    let mut renderer =
        ShmRenderer::new(shm, surface.clone(), &qh, state.width, state.height, counters.render.clone())
            .context("create SHM renderer")?;
    info!("window created: {}x{}", state.width, state.height);
    if !renderer.prime() {
        warn!("renderer: no free slot for initial commit");
    }
    state.renderer = Some(renderer);

    loop {
        if *close_tx.borrow() {
            info!("window closed by compositor");
            break;
        }
        // Verbose loop logging (debug level) so a stuck loop is
        // visible in --nocapture output. Disabled by default to
        // avoid log spam in production.
        tracing::trace!(
            "display tick: slot_state={}",
            state
                .renderer
                .as_ref()
                .map(|r| r.slot_state())
                .unwrap_or_else(|| "<no renderer>".into()),
        );

        // Read and dispatch Wayland events FIRST so wl_buffer::Release
        // events are processed before try_drain tries to pick a slot.
        // With only 2 SHM slots and a 60 Hz compositor (Release latency
        // ~16-32 ms vs. 33 ms frame interval), the Release for the just-
        // displayed slot can arrive in the same 1 ms tick as the next
        // decoded frame. If we rendered first, both slots would still
        // appear held and the frame would be dropped needlessly.
        if let Some(guard) = event_queue.prepare_read() {
            let _ = guard.read(); // non-blocking: returns Ok(0) if no data
        }
        let _ = event_queue.dispatch_pending(&mut state);
        event_queue.flush().ok();

        let frames_drained = match state
            .renderer
            .as_mut()
            .unwrap()
            .try_drain(&qh, &frame_rx)
        {
            Ok(0) => 0usize,
            Ok(n) => n,
            Err(e) => {
                warn!("renderer error: {e:#}");
                break;
            }
        };
        if frames_drained > 0 {
            state.last_render = Some(std::time::Instant::now());
        }

        if let Some((w, h)) = state.pending_resize.take() {
            debug!("applying pending resize to {w}x{h}");
            if let Some(r) = state.renderer.as_mut() {
                r.resize(w, h);
            }
        }

        // Periodically warn if no frames have been rendered for >2s.
        if let Some(last) = state.last_render {
            let elapsed = last.elapsed();
            if elapsed > std::time::Duration::from_secs(2) {
                tracing::warn!(
                    "no frame rendered in {:.1}s; is the server's compositor idle? \
                     (attach a Wayland client -- e.g. wayland-test-client -- to \
                     WAYLAND_DISPLAY={} to generate frames)",
                    elapsed.as_secs_f32(),
                    std::env::var("WAYLAND_DISPLAY").unwrap_or_default(),
                );
                state.last_render = Some(std::time::Instant::now());
            }
        }

        // Brief sleep so we don't spin at 100% CPU.
        std::thread::sleep(Duration::from_millis(1));
        // Flush the commit from try_drain's render() call.
        event_queue.flush().ok();
    }

    Ok(())
}

struct DisplayState {
    initial_size: (u32, u32),
    width: u32,
    height: u32,
    size_tx: watch::Sender<(u32, u32)>,
    close_tx: watch::Sender<bool>,

    compositor: Option<wl_compositor::WlCompositor>,
    wm_base: Option<xdg_wm_base_protocol::XdgWmBase>,
    shm: Option<wl_shm::WlShm>,
    seat: Option<wl_seat::WlSeat>,

    /// Active wl_pointer object; present once the seat advertises
    /// Capability::Pointer and we call seat.get_pointer().
    pointer: Option<wl_pointer::WlPointer>,
    /// Active wl_keyboard object; present once the seat advertises
    /// Capability::Keyboard and we call seat.get_keyboard().
    keyboard: Option<wl_keyboard::WlKeyboard>,

    /// Last pointer position (normalized 0..1 in surface coordinates).
    /// Updated on Enter and Motion events.
    pointer_x: f64,
    pointer_y: f64,

    /// The wl_surface for the main window. Set before the configure
    /// roundtrip so the xdg_surface::Configure handler can call
    /// `surface.commit()` after `ack_configure` — the spec requires a
    /// commit to apply each configure.
    surface: Option<wl_surface::WlSurface>,

    renderer: Option<ShmRenderer>,
    /// Resize requests from xdg_toplevel::Configure are applied on the
    /// next loop tick (after any in-flight render) so we never resize
    /// mid-blit. Stored here, drained at the top of each tick.
    pending_resize: Option<(u32, u32)>,
    /// Wall-clock time of the last successful render. Used by the
    /// "no frame in N seconds" warning below to distinguish a stuck
    /// renderer from a silent upstream.
    last_render: Option<std::time::Instant>,
    /// Counters shared with `DisplayHandle` for observability (smoke
    /// tests, future metrics). Cloned from the Arc the caller sees.
    counters: DisplayCounters,
    /// Sends translated input events to the tokio send loop.
    input_tx: tokio::sync::mpsc::Sender<SignalingMessage>,
}

impl DisplayState {
    fn new(
        initial_size: (u32, u32),
        size_tx: watch::Sender<(u32, u32)>,
        close_tx: watch::Sender<bool>,
        counters: DisplayCounters,
        input_tx: tokio::sync::mpsc::Sender<SignalingMessage>,
    ) -> Self {
        Self {
            initial_size,
            width: initial_size.0,
            height: initial_size.1,
            size_tx,
            close_tx,
            compositor: None,
            wm_base: None,
            shm: None,
            seat: None,
            pointer: None,
            keyboard: None,
            pointer_x: 0.5,
            pointer_y: 0.5,
            surface: None,
            renderer: None,
            pending_resize: None,
            last_render: None,
            counters,
            input_tx,
        }
    }

    /// Send an input event to the tokio send loop. Uses `try_send`
    /// to avoid blocking the Wayland event loop thread; drops silently
    /// if the channel is full (64 slots, very unlikely during normal
    /// pointer/keyboard use).
    fn send_input(&self, msg: SignalingMessage) {
        if self.input_tx.try_send(msg).is_err() {
            debug!("input channel full; dropping event");
        }
    }
}

// --- Wayland Dispatch impls ---------------------------------------------

/// Bind required globals on registry advertisements. Version is pinned to
/// the highest version the compositor advertises (wayland-client does that
/// automatically via the `bind::<Interface, _, _>(name, version, ...)`
/// call when we pass the advertised `version`).
impl Dispatch<wl_registry::WlRegistry, ()> for DisplayState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
            ..
        } = event
        {
            debug!("registry global: {interface} v{version}");
            match interface.as_str() {
                "wl_compositor" => {
                    state.compositor = Some(
                        registry.bind::<wl_compositor::WlCompositor, _, _>(name, version, qh, ()),
                    );
                }
                "xdg_wm_base" => {
                    state.wm_base = Some(
                        registry.bind::<xdg_wm_base_protocol::XdgWmBase, _, _>(
                            name, version, qh, (),
                        ),
                    );
                }
                "wl_seat" => {
                    state.seat =
                        Some(registry.bind::<wl_seat::WlSeat, _, _>(name, version, qh, ()));
                }
                "wl_shm" => {
                    state.shm =
                        Some(registry.bind::<wl_shm::WlShm, _, _>(name, version, qh, ()));
                }
                _ => {} // We don't need any of the others (yet).
            }
        }
    }
}

impl Dispatch<wl_compositor::WlCompositor, ()> for DisplayState {
    fn event(
        _: &mut Self,
        _: &wl_compositor::WlCompositor,
        _: wl_compositor::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // wl_compositor has no events.
    }
}

impl Dispatch<xdg_wm_base_protocol::XdgWmBase, ()> for DisplayState {
    fn event(
        _: &mut Self,
        wm_base: &xdg_wm_base_protocol::XdgWmBase,
        event: xdg_wm_base_protocol::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // The xdg-shell spec requires us to respond to every ping
        // with pong(serial). Compositors use this to detect clients
        // that have stopped responding to events -- if we ignore
        // pings, the WM flags our surface as "Not Responding" (and
        // some compositors will eventually kill the connection,
        // which is why the user reported "first and only picture
        // appears" -- after the connection dies, blocking_dispatch
        // wakes up on the disconnect and the display thread exits).
        if let xdg_wm_base_protocol::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface_protocol::XdgSurface, ()> for DisplayState {
    fn event(
        state: &mut Self,
        xdg_surface: &xdg_surface_protocol::XdgSurface,
        event: xdg_surface_protocol::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface_protocol::Event::Configure { serial } = event {
            debug!("xdg_surface::Configure serial={serial} -> ack_configure");
            xdg_surface.ack_configure(serial);
            // Commit so the compositor considers this configure applied.
            if let Some(s) = &state.surface {
                s.commit();
            }
        }
    }
}

impl Dispatch<xdg_toplevel_protocol::XdgToplevel, ()> for DisplayState {
    fn event(
        state: &mut Self,
        _: &xdg_toplevel_protocol::XdgToplevel,
        event: xdg_toplevel_protocol::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            xdg_toplevel_protocol::Event::Configure {
                width,
                height,
                states,
                ..
            } => {
                let w = if width == 0 {
                    state.initial_size.0
                } else {
                    width.try_into().unwrap()
                };
                let h = if height == 0 {
                    state.initial_size.1
                } else {
                    height.try_into().unwrap()
                };
                if (w, h) != (state.width, state.height) {
                    debug!("xdg_toplevel configure: {w}x{h} states={states:?}");
                    let _ = state.size_tx.send((w, h));
                    state.pending_resize = Some((w, h));
                }
                state.width = w;
                state.height = h;
            }
            xdg_toplevel_protocol::Event::Close => {
                let _ = state.close_tx.send(true);
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for DisplayState {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities { capabilities } = event {
            let caps = match capabilities {
                WEnum::Value(c) => c,
                WEnum::Unknown(_) => return,
            };
            if caps.contains(wl_seat::Capability::Pointer) && state.pointer.is_none() {
                debug!("seat: pointer capability -> get_pointer");
                state.pointer = Some(seat.get_pointer(qh, ()));
            }
            if caps.contains(wl_seat::Capability::Keyboard) && state.keyboard.is_none() {
                debug!("seat: keyboard capability -> get_keyboard");
                state.keyboard = Some(seat.get_keyboard(qh, ()));
            }
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for DisplayState {
    fn event(
        state: &mut Self,
        pointer: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                serial,
                surface_x,
                surface_y,
                ..
            } => {
                // Hide the native cursor so only the remote app's cursor shows.
                pointer.set_cursor(serial, None, 0, 0);
                state.pointer_x = surface_x / state.width as f64;
                state.pointer_y = surface_y / state.height as f64;
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                state.pointer_x = surface_x / state.width as f64;
                state.pointer_y = surface_y / state.height as f64;
                let x = state.pointer_x;
                let y = state.pointer_y;
                state.send_input(SignalingMessage::Pointer {
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
            wl_pointer::Event::Button {
                button,
                state: btn_state,
                ..
            } => {
                // Linux button codes → browser button index
                let browser_button = match button {
                    0x110 => 0, // BTN_LEFT
                    0x111 => 2, // BTN_RIGHT
                    0x112 => 1, // BTN_MIDDLE
                    _ => 0,
                };
                let x = state.pointer_x;
                let y = state.pointer_y;
                let pp = PointerPoint {
                    x,
                    y,
                    button: browser_button,
                    pointer_type: "mouse".into(),
                    pressure: 0.0,
                };
                let mouse_event = match btn_state {
                    WEnum::Value(wl_pointer::ButtonState::Pressed) => {
                        MouseEvent::Down { pointer: pp }
                    }
                    WEnum::Value(wl_pointer::ButtonState::Released) => {
                        MouseEvent::Up { pointer: pp }
                    }
                    _ => return,
                };
                state.send_input(SignalingMessage::Pointer { event: mouse_event });
            }
            wl_pointer::Event::Axis { axis, value, .. } => {
                let (delta_x, delta_y) = match axis {
                    WEnum::Value(wl_pointer::Axis::VerticalScroll) => (0.0, value),
                    WEnum::Value(wl_pointer::Axis::HorizontalScroll) => (value, 0.0),
                    _ => return,
                };
                let x = state.pointer_x;
                let y = state.pointer_y;
                state.send_input(SignalingMessage::Pointer {
                    event: MouseEvent::Wheel {
                        x,
                        y,
                        delta_x,
                        delta_y,
                    },
                });
            }
            _ => {} // Leave, Frame, AxisSource, AxisStop, AxisDiscrete, etc.
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for DisplayState {
    fn event(
        state: &mut Self,
        _keyboard: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Key { key, state: key_state, .. } = event {
            let code = match evdev_to_code(key) {
                Some(c) => c,
                None => {
                    debug!("unknown evdev keycode {key}; skipping");
                    return;
                }
            };
            let kb_event = match key_state {
                WEnum::Value(wl_keyboard::KeyState::Pressed) => {
                    KeyboardEvent::Down { code: code.to_string() }
                }
                WEnum::Value(wl_keyboard::KeyState::Released) => {
                    KeyboardEvent::Up { code: code.to_string() }
                }
                _ => return,
            };
            state.send_input(SignalingMessage::Key { event: kb_event });
        }
        // Keymap, Enter, Leave, Modifiers, RepeatInfo: no-ops.
        // We use a static reverse table (input/keymap.rs), not xkbcommon.
    }
}

impl Dispatch<wl_shm::WlShm, ()> for DisplayState {
    fn event(
        _: &mut Self,
        _: &wl_shm::WlShm,
        _: wl_shm::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Format advertisements; we use Argb8888 unconditionally.
    }
}

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

// No-op: keeps the event loop from panicking if a placeholder buffer
// gets a Release (e.g. from a stale startup buffer).
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
            state.counters.release.fetch_add(
                1,
                std::sync::atomic::Ordering::Relaxed,
            );
            tracing::debug!("wl_buffer::Release for slot={slot_id}");
            if let Some(renderer) = state.renderer.as_mut() {
                renderer.release_slot(*slot_id);
            }
        }
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for DisplayState {
    fn event(
        _: &mut Self,
        _: &wl_surface::WlSurface,
        _: wl_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
