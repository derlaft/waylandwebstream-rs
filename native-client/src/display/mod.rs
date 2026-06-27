// Wayland display wiring for the native client.
//
// Owns the synchronous Wayland event loop and the wl_surface/xdg_toplevel
// plumbing, plus (Phase 5+) the SHM renderer that blits decoded frames
// into wl_shm buffers. The display loop runs on a dedicated OS thread
// because wayland-client 0.31 is synchronous (no async). The tokio
// side feeds it via `frame_rx` (decoded H.264 frames) and reads back
// window size + close state through `tokio::sync::watch` channels in
// `DisplayHandle`. That keeps `event_queue.dispatch_pending` off the
// async executor entirely -- see AGENTS.md "two execution domains".

use anyhow::{Context, Result};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tokio::sync::watch;
use tracing::{debug, info, warn};
use wayland_client::{
    protocol::{wl_buffer, wl_compositor, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface},
    Connection, Dispatch, QueueHandle,
};
use wayland_protocols::xdg::shell::client::xdg_surface as xdg_surface_protocol;
use wayland_protocols::xdg::shell::client::xdg_toplevel as xdg_toplevel_protocol;
use wayland_protocols::xdg::shell::client::xdg_wm_base as xdg_wm_base_protocol;

use crate::decode::sw::DecodedFrame;
use crate::render::shm::{ShmRenderer, SlotId};

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

    let counters = DisplayCounters {
        render: render_counter.clone(),
        release: release_counter.clone(),
    };
    thread::Builder::new()
        .name("wws-display".into())
        .spawn(move || {
            if let Err(e) =
                run_display_loop(initial_size, size_tx, close_tx, frame_rx, counters)
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
) -> Result<()> {
    let conn = Connection::connect_to_env()
        .context("could not connect to Wayland compositor (is $WAYLAND_DISPLAY set?)")?;
    let display = conn.display();
    let mut event_queue = conn.new_event_queue();
    let qh = event_queue.handle();

    let mut state = DisplayState::new(initial_size, size_tx, close_tx.clone(), counters.clone());

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

    let mut renderer = ShmRenderer::new(shm, surface.clone(), &qh, state.width, state.height)
        .context("create SHM renderer")?;
    // Replace the renderer's internal counter with the shared one
    // so the DisplayHandle returned to the caller observes the same
    // increments. Cheap: just an Arc swap.
    renderer.install_render_counter(counters.render.clone());
    if !renderer.prime() {
        warn!("renderer could not prime initial buffer");
    } else {
        info!(
            "window created: title=\"waylandwebstream\" size={}x{}",
            state.width, state.height
        );
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

        // Drain decoded frames first (fast, non-blocking). This is
        // what makes the window *show* new pictures -- if no frame is
        // available, we fall through to blocking_dispatch to wait for
        // compositor events.
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
            // Reset the "no frames" timer whenever we successfully
            // render. The stale-frame warning below uses this to
            // distinguish "server is producing frames but renderer is
            // stuck" (which would still hit frames_drained > 0 here)
            // from "server is silent" (which never does).
            state.last_render = Some(std::time::Instant::now());
        }

        if let Some((w, h)) = state.pending_resize.take() {
            debug!("applying pending resize to {w}x{h}");
            if let Some(r) = state.renderer.as_mut() {
                r.resize(w, h);
            }
        }

        // Periodically warn if no frames have been rendered for >2s.
        // This catches the "server has nothing to send" case
        // (compositor idle, no Wayland clients attached) which
        // otherwise looks indistinguishable from a renderer bug: the
        // user sees a static picture and assumes the renderer is
        // stuck, when actually the upstream is silent.
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
                state.last_render = Some(std::time::Instant::now()); // suppress repeat
            }
        }

        // Non-blocking read from the Wayland socket, then dispatch
        // any newly-buffered events (wl_buffer::Release, frame
        // callbacks, xdg_wm_base::Ping, etc.).
        //
        // IMPORTANT: in wayland-client 0.31, `dispatch_pending` alone
        // does NOT read from the socket — it only dispatches events
        // that are already in the queue's internal buffer. Without the
        // explicit prepare_read + read step, Release events sent by the
        // compositor never land in the buffer and both SHM slots stay
        // "held" indefinitely, starving the renderer after the very
        // first frame. `blocking_dispatch` reads from the socket but
        // blocks until an event arrives, which would stall frame_rx
        // polling. The prepare_read + non-blocking read is the correct
        // idiom for a tight event loop; see the docs on
        // EventQueue::prepare_read.
        if let Some(guard) = event_queue.prepare_read() {
            let _ = guard.read(); // non-blocking: returns Ok(0) if no data
        }
        let _ = event_queue.dispatch_pending(&mut state);
        // Brief sleep so we don't spin at 100% CPU.
        std::thread::sleep(Duration::from_millis(1));
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
    #[allow(dead_code)]
    seat: Option<wl_seat::WlSeat>, // bound for Phase 7 input forwarding

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
}

impl DisplayState {
    fn new(
        initial_size: (u32, u32),
        size_tx: watch::Sender<(u32, u32)>,
        close_tx: watch::Sender<bool>,
        counters: DisplayCounters,
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
            surface: None,
            renderer: None,
            pending_resize: None,
            last_render: None,
            counters,
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

/// xdg_surface::Configure carries a serial we MUST ack before committing,
/// or the compositor treats the next commit as out-of-sync. The size
/// itself comes from the xdg_toplevel Configure event (xdg_surface's
/// Configure is per-surface, xdg_toplevel's is per-role).
///
/// After the ack we also commit the surface. The xdg-shell spec requires
/// a commit to "apply" each configure event; without it the compositor
/// may keep sending configures or consider the client non-responsive. The
/// commit here re-commits the current surface state (last attached buffer
/// unchanged) — it is not a new frame, just a protocol acknowledgement.
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

/// The compositor's authoritative surface size (per the xdg-shell spec).
/// A width/height of 0 means "pick something" -- we fall back to the
/// `initial_size` so a misbehaving compositor can't leave us at 0x0.
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
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Phase 4 binds the seat but doesn't yet request its input
        // capabilities. Phase 7 fills this in.
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
        // wl_shm::Format advertises supported pixel formats. We use
        // Argb8888 unconditionally; ignoring means we don't react if
        // a compositor claims it doesn't support it (then we'd
        // negotiate). Phase 5 keeps it simple.
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

/// `wl_buffer::release` for the *placeholder* user-data type. Phase 5
/// no longer uses the placeholder -- the renderer's slots carry
/// `SlotId` as their user data (see the next impl). Kept around as a
/// no-op so any stray placeholder buffer (left over from an aborted
/// startup, for example) doesn't panic the event loop.
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

/// `wl_buffer::release` for the *renderer* buffers. The user data is
/// the slot index (`SlotId`), so we route the release back to the
/// renderer's slot table.
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
