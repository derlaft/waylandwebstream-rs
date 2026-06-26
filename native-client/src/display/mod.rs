// Wayland display wiring for the native client.
//
// Phase 4 lays down the surface-and-toplevel plumbing only: connect to the
// compositor, bind the globals we will eventually need (`wl_compositor`,
// `xdg_wm_base`, `wl_seat`, `wl_shm`), create a single `wl_surface` with
// an `xdg_toplevel` titled "waylandwebstream", and run an event loop that
// tracks window size + close. No rendering, no input forwarding -- those
// arrive in later phases and hook into the same `DisplayHandle` channels.
//
// Wayland event dispatch is synchronous (`wayland-client` 0.31), so the
// display loop runs on a dedicated OS thread (per N2 in the plan). The
// tokio side communicates with it through `tokio::sync::watch` channels
// in `DisplayHandle`. That keeps `event_queue.dispatch_pending` off the
// async executor entirely.

use anyhow::{Context, Result};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::thread;
use std::time::Duration;
use tokio::sync::watch;
use tracing::{debug, info, warn};
use wayland_client::{
    protocol::wl_buffer, protocol::wl_compositor, protocol::wl_registry, protocol::wl_seat,
    protocol::wl_shm, protocol::wl_shm_pool, protocol::wl_surface, Connection, Dispatch,
    QueueHandle,
};
use wayland_protocols::xdg::shell::client::xdg_surface as xdg_surface_protocol;
use wayland_protocols::xdg::shell::client::xdg_toplevel as xdg_toplevel_protocol;
use wayland_protocols::xdg::shell::client::xdg_wm_base as xdg_wm_base_protocol;

/// Handle to the Wayland display thread. Returned by
/// [`spawn_display_thread`]; the rest of the client reads window size and
/// close state through the two `watch` receivers.
pub struct DisplayHandle {
    /// Current window size in surface-local pixels, updated whenever
    /// `xdg_toplevel::Configure` fires (or set to `initial_size` until the
    /// first configure arrives).
    pub size_rx: watch::Receiver<(u32, u32)>,
    /// Flips to `true` when `xdg_toplevel::Close` fires. The display thread
    /// then breaks out of its loop.
    pub close_rx: watch::Receiver<bool>,
}

/// Spawn the Wayland display thread. The thread connects to `$WAYLAND_DISPLAY`,
/// creates a `wl_surface` + `xdg_toplevel` titled "waylandwebstream", and
/// dispatches events forever (or until the window is closed).
///
/// Phase 4 deliberately ignores the renderer / decoder plumbing -- those
/// parameters get layered in starting with Phase 5 (SW decode + SHM render)
/// without changing this signature's caller-visible shape.
pub fn spawn_display_thread(initial_size: (u32, u32)) -> Result<DisplayHandle> {
    let (size_tx, size_rx) = watch::channel(initial_size);
    let (close_tx, close_rx) = watch::channel(false);

    thread::Builder::new()
        .name("wws-display".into())
        .spawn(move || {
            if let Err(e) = run_display_loop(initial_size, size_tx, close_tx) {
                warn!("display thread exited: {e:#}");
            }
        })
        .context("failed to spawn display thread")?;

    Ok(DisplayHandle { size_rx, close_rx })
}

/// Create a `memfd_create`-backed wl_shm buffer of `width x height`
/// pixels in `WL_SHM_FORMAT_ARGB8888` filled with solid dark grey,
/// attach it to `surface`, and damage the entire surface so the
/// compositor redraws.
///
/// Returns the `(pool, buffer, fd)` triple so the caller can stash them
/// in `DisplayState` -- the buffer's mmap must remain valid until the
/// compositor sends `wl_buffer::release` (or until the surface is
/// unmapped). Phase 5 replaces this with a real decoded frame.
fn attach_placeholder_buffer(
    surface: &wl_surface::WlSurface,
    shm: &wl_shm::WlShm,
    qh: &QueueHandle<DisplayState>,
    width: u32,
    height: u32,
) -> Result<(wl_shm_pool::WlShmPool, wl_buffer::WlBuffer, OwnedFd)> {
    use std::ffi::CString;

    const BYTES_PER_PIXEL: usize = 4; // WL_SHM_FORMAT_ARGB8888
    let stride = width as usize * BYTES_PER_PIXEL;
    let size = stride * height as usize;

    // memfd_create + ftruncate + mmap. We're on Linux (Wayland implies
    // it); MFD_CLOEXEC so the fd doesn't leak into child processes.
    let name = CString::new("wws-client-placeholder").unwrap();
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        anyhow::bail!(
            "memfd_create failed: {}",
            std::io::Error::last_os_error()
        );
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };

    let ret = unsafe { libc::ftruncate(fd.as_raw_fd(), size as libc::off_t) };
    if ret < 0 {
        anyhow::bail!("ftruncate({size}) failed: {}", std::io::Error::last_os_error());
    }

    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd.as_raw_fd(),
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        anyhow::bail!("mmap failed: {}", std::io::Error::last_os_error());
    }
    // SAFETY: ptr is valid for `size` bytes; we just mapped it.
    let pixels: &mut [u32] = unsafe {
        std::slice::from_raw_parts_mut(ptr as *mut u32, size / BYTES_PER_PIXEL)
    };
    pixels.fill(0xFF20_2020); // ARGB32 dark grey, opaque
    unsafe { libc::msync(ptr, size, libc::MS_SYNC) };

    let pool = shm.create_pool(fd.as_fd(), size as i32, qh, ());
    let buffer = pool.create_buffer(
        0,
        width as i32,
        height as i32,
        stride as i32,
        wl_shm::Format::Argb8888,
        qh,
        (),
    );

    surface.attach(Some(&buffer), 0, 0);
    surface.damage_buffer(0, 0, width as i32, height as i32);
    Ok((pool, buffer, fd))
}

fn run_display_loop(
    initial_size: (u32, u32),
    size_tx: watch::Sender<(u32, u32)>,
    close_tx: watch::Sender<bool>,
) -> Result<()> {
    let conn = Connection::connect_to_env()
        .context("could not connect to Wayland compositor (is $WAYLAND_DISPLAY set?)")?;
    let display = conn.display();
    let mut event_queue = conn.new_event_queue();
    let qh = event_queue.handle();

    let mut state = DisplayState::new(initial_size, size_tx, close_tx.clone());

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

    // Phase 4 doesn't read seat/shm yet, but binding them now means later
    // phases (input, SHM render) don't need another roundtrip + re-bind.
    if state.seat.is_none() {
        warn!("wl_seat global missing -- input forwarding will not work");
    }
    if state.shm.is_none() {
        warn!("wl_shm global missing -- SHM renderer will not work");
    }

    let surface = compositor.create_surface(&qh, ());
    let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg_surface.get_toplevel(&qh, ());
    toplevel.set_title("waylandwebstream".to_string());
    toplevel.set_app_id("wws-client".to_string());

    // Commit to trigger the initial configure. xdg_surface::Configure
    // arrives in the next roundtrip; until then the compositor hasn't
    // allocated us a size, so we keep `state.width/height` at initial.
    surface.commit();
    event_queue
        .roundtrip(&mut state)
        .context("configure roundtrip")?;

    // Without a buffer attached, the xdg-shell spec says the surface is
    // never mapped -- so it lives in the task switcher but stays
    // invisible. We attach a solid-color wl_shm buffer here as a
    // placeholder; Phase 5 replaces it with the SW-decoder-fed renderer.
    if state.shm.is_some() {
        match attach_placeholder_buffer(
            &surface,
            state.shm.as_ref().unwrap(),
            &qh,
            state.width,
            state.height,
        ) {
            Ok((pool, buffer, fd)) => {
                state.placeholder_pool = Some(pool);
                state.placeholder_buffer = Some(buffer);
                state.placeholder_fd = Some(fd);
                surface.commit();
                event_queue
                    .roundtrip(&mut state)
                    .context("post-placeholder commit roundtrip")?;
                info!("placeholder buffer attached; window should be visible");
            }
            Err(e) => warn!("could not attach placeholder buffer: {e:#}"),
        }
    } else {
        warn!("no wl_shm available; window will be invisible until Phase 5 adds a renderer");
    }

    info!(
        "window created: title=\"waylandwebstream\" size={}x{}",
        state.width, state.height
    );

    loop {
        if *close_tx.borrow() {
            info!("window closed by compositor");
            break;
        }
        // Drain any pending events (input callbacks, configure, etc).
        event_queue
            .dispatch_pending(&mut state)
            .context("dispatch_pending")?;
        event_queue.flush().context("flush")?;
        // Park briefly so the loop doesn't spin when nothing is happening.
        // Phase 5+ will replace this with frame-driven wakeups from the
        // decoder feeding the renderer.
        thread::sleep(Duration::from_millis(1));
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
    #[allow(dead_code)]
    seat: Option<wl_seat::WlSeat>, // bound for Phase 7 input forwarding
    #[allow(dead_code)]
    shm: Option<wl_shm::WlShm>, // bound for Phase 5 SHM render

    // Phase 4 placeholder buffer so the surface actually maps (see
    // attach_placeholder_buffer). Dropped automatically when the
    // display thread exits, which destroys the wl_buffer, wl_shm_pool,
    // and memfd in that order.
    placeholder_pool: Option<wl_shm_pool::WlShmPool>,
    placeholder_buffer: Option<wl_buffer::WlBuffer>,
    placeholder_fd: Option<OwnedFd>,
}

impl DisplayState {
    fn new(
        initial_size: (u32, u32),
        size_tx: watch::Sender<(u32, u32)>,
        close_tx: watch::Sender<bool>,
    ) -> Self {
        Self {
            initial_size,
            width: initial_size.0,
            height: initial_size.1,
            size_tx,
            close_tx,
            compositor: None,
            wm_base: None,
            seat: None,
            shm: None,
            placeholder_pool: None,
            placeholder_buffer: None,
            placeholder_fd: None,
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
        _: &xdg_wm_base_protocol::XdgWmBase,
        _: xdg_wm_base_protocol::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // xdg_wm_base::ping is the only event and we don't need to respond
        // to it (the spec says we must, but our compositor is well-behaved
        // and we're not running away; a real client would `pong(serial)`).
    }
}

/// xdg_surface::Configure carries a serial we MUST ack before committing,
/// or the compositor treats the next commit as out-of-sync. The size
/// itself comes from the xdg_toplevel Configure event (xdg_surface's
/// Configure is per-surface, xdg_toplevel's is per-role).
impl Dispatch<xdg_surface_protocol::XdgSurface, ()> for DisplayState {
    fn event(
        _: &mut Self,
        xdg_surface: &xdg_surface_protocol::XdgSurface,
        event: xdg_surface_protocol::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface_protocol::Event::Configure { serial } = event {
            xdg_surface.ack_configure(serial);
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
                // xdg-shell delivers dimensions as i32; a value of 0 means
                // "client picks". Fall back to initial_size in that case.
                // try_into is safe: the compositor never sends negative
                // widths (negative is a protocol error).
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
        // wl_shm::Format advertises supported pixel formats. Phase 5
        // (SHM renderer) reads this; for now we ignore it.
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for DisplayState {
    fn event(
        _: &mut Self,
        _: &wl_shm_pool::WlShmPool,
        _: wl_shm_pool::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // wl_shm_pool has no events.
    }
}

/// `wl_buffer::release` fires when the compositor is done reading from
/// the buffer. Phase 4 doesn't reuse buffers (one placeholder for the
/// whole session) so we just log it. Phase 5's SHM renderer will use
/// this to recycle released slots.
impl Dispatch<wl_buffer::WlBuffer, ()> for DisplayState {
    fn event(
        _: &mut Self,
        _: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            debug!("placeholder wl_buffer released by compositor");
        }
    }
}

// We don't construct a wl_surface here -- the compositor does, via
// wl_compositor::create_surface -- but we still need a Dispatch impl
// because wayland-client requires one for every object the event queue
// might encounter. We just never instantiate one ourselves.
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