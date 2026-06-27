// Test-only payload client for the native-client smoke test.
//
// Renders a single solid-color wl_shm buffer fullscreen. The color is
// read from a control file at startup and is polled for changes; when
// the file's content changes (e.g. "black"), the client recreates its
// wl_shm buffer with the new color and re-attaches + commits it. This
// drives the whole pipeline end-to-end:
//
//   payload-client -> wws-server compositor -> H.264 encode ->
//     wws-client (native-client) -> labwc (headless) -> grim screenshot
//
// The test reads the screenshot back and verifies that the majority
// of pixels are the expected color. A second screenshot after a color
// flip verifies the pipeline reacts to new client commits within the
// test's run window.
//
// CLI:
//
//   payload-client [--color white|black] [--size WxH]
//                  [--control-file PATH]
//
// Defaults: --color white, --size 320x240, --control-file
// /tmp/wws-payload-color. The control file's content is read on every
// poll tick; if it differs from the current color, the client switches.
// The test writes "black" to the file to flip; "white" to flip back.

use std::fs;
use std::io::Write;
use std::os::fd::{AsFd, FromRawFd, RawFd};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use wayland_client::{
    protocol::{wl_buffer, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface},
    Connection, Dispatch, QueueHandle,
};
use wayland_protocols::xdg::shell::client::{
    xdg_surface as xdg_surface_protocol, xdg_toplevel as xdg_toplevel_protocol,
    xdg_wm_base as xdg_wm_base_protocol,
};

const DEFAULT_SIZE: (u32, u32) = (320, 240);
const DEFAULT_COLOR: &str = "white";
const DEFAULT_CONTROL_FILE: &str = "/tmp/wws-payload-color";
const POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Color {
    White,
    Black,
}

impl Color {
    fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "white" => Some(Color::White),
            "black" => Some(Color::Black),
            _ => None,
        }
    }

    /// ARGB8888 byte values, packed little-endian as BGRA in memory
    /// (the wl_shm XRGB8888 / ARGB8888 wire layout is host-endian
    /// 32-bit pixels, BGRA on little-endian systems).
    fn argb32(self) -> [u8; 4] {
        match self {
            // 0xFFRRGGBB; for ARGB8888 wl_shm, alpha byte must be 0xFF.
            Color::White => [0xFF, 0xFF, 0xFF, 0xFF],
            Color::Black => [0x00, 0x00, 0x00, 0xFF],
        }
    }
}

struct Args {
    color: Color,
    size: (u32, u32),
    control_file: String,
}

fn parse_args() -> Result<Args> {
    let mut color_str = DEFAULT_COLOR.to_string();
    let mut size = DEFAULT_SIZE;
    let mut control_file = DEFAULT_CONTROL_FILE.to_string();

    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--color" => {
                color_str = iter
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--color requires a value"))?;
            }
            "--size" => {
                let s = iter
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--size requires a value"))?;
                let (w, h) = s
                    .split_once('x')
                    .ok_or_else(|| anyhow::anyhow!("--size expects WxH, got {s:?}"))?;
                size = (
                    w.parse().context("--size width")?,
                    h.parse().context("--size height")?,
                );
            }
            "--control-file" => {
                control_file = iter
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--control-file requires a value"))?;
            }
            "--help" | "-h" => {
                println!(
                    "payload-client [--color white|black] [--size WxH] \
                     [--control-file PATH]"
                );
                std::process::exit(0);
            }
            other => anyhow::bail!("unknown argument: {other}"),
        }
    }

    let color = Color::parse(&color_str)
        .ok_or_else(|| anyhow::anyhow!("invalid --color {color_str:?} (use white or black)"))?;
    Ok(Args {
        color,
        size,
        control_file,
    })
}

fn main() -> Result<()> {
    let args = parse_args()?;
    eprintln!(
        "payload-client starting: color={:?} size={:?} control_file={:?}",
        args.color, args.size, args.control_file
    );

    let conn = Connection::connect_to_env().context("WAYLAND_DISPLAY connect")?;
    let display = conn.display();
    let mut event_queue = conn.new_event_queue();
    let qh = event_queue.handle();

    let _registry = display.get_registry(&qh, ());

    let mut state = AppState {
        compositor: None,
        shm: None,
        wm_base: None,
        surface: None,
        configured: false,
        size: args.size,
        color: args.color,
    };

    // Round-trip to bind globals we need: wl_compositor + wl_shm +
    // xdg_wm_base. The wws compositor publishes all three.
    event_queue
        .roundtrip(&mut state)
        .context("initial roundtrip (globals)")?;

    let compositor = state
        .compositor
        .clone()
        .ok_or_else(|| anyhow::anyhow!("compositor global not advertised"))?;
    let shm = state
        .shm
        .clone()
        .ok_or_else(|| anyhow::anyhow!("wl_shm global not advertised"))?;
    let wm_base = state
        .wm_base
        .clone()
        .ok_or_else(|| anyhow::anyhow!("xdg_wm_base global not advertised"))?;

    // Create the surface + xdg_toplevel and commit so the compositor
    // sends us a Configure back with the actual size.
    let surface = compositor.create_surface(&qh, ());
    let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg_surface.get_toplevel(&qh, ());
    toplevel.set_title("wws-payload-client".into());
    surface.commit();
    state.surface = Some(surface.clone());

    // Drain Configure events until ack_configure lets us proceed. The
    // wws server resizes our surface to fill its output (fullscreen
    // config); we wait for the first non-zero Configure before we
    // know the real size. We use `roundtrip` here (not
    // `dispatch_pending`): dispatch_pending only drains the queue's
    // local buffer, while the Configure we're waiting for arrives
    // over the socket after our commit. roundtrip blocks until both
    // ends have drained everything pending. We loop until
    // `state.configured` flips true because on a fast local socket
    // roundtrip can return once the queue is balanced but before the
    // server's Configure has been dispatched into our state.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !state.configured && Instant::now() < deadline {
        let _ = event_queue.roundtrip(&mut state);
        let _ = event_queue.flush();
        std::thread::sleep(Duration::from_millis(10));
    }
    if !state.configured {
        anyhow::bail!("timed out waiting for xdg_surface::Configure");
    }
    eprintln!(
        "payload-client: surface configured at {}x{}",
        state.size.0, state.size.1
    );

    // Initial commit: solid-color buffer at the configured size.
    let initial_size = state.size;
    let mut current_color = state.color;
    let mut buffer = create_colored_buffer(&shm, &qh, initial_size, current_color)?;
    surface.attach(Some(&buffer), 0, 0);
    surface.damage_buffer(0, 0, initial_size.0 as i32, initial_size.1 as i32);
    surface.commit();
    event_queue
        .dispatch_pending(&mut state)
        .context("dispatch after initial commit")?;
    event_queue.flush().context("flush after initial commit")?;
    eprintln!(
        "payload-client: initial color {:?} committed",
        current_color
    );

    // Steady-state loop: poll the control file; on change, recreate
    // the buffer and re-attach. Re-commit the buffer periodically
    // (even when the color hasn't changed) so the wws server keeps
    // seeing surface damage -- without this, the compositor would
    // consider the screen idle and stop producing frames, leaving
    // the wws-client with a stale picture and the test unable to
    // observe any further updates.
    let mut last_seen = read_control(&args.control_file).unwrap_or(current_color);
    // Re-commit cadence: 250ms. Slow enough not to spam the
    // compositor with redundant damage events; fast enough that the
    // server's frame loop (60Hz / 16ms tick) sees a new commit on
    // most ticks and stays in steady-state "producing frames"
    // rather than idle-detecting.
    let mut last_commit = Instant::now();
    let commit_interval = Duration::from_millis(250);
    loop {
        std::thread::sleep(POLL_INTERVAL);

        // Drain Wayland events first so Close can exit us promptly.
        match event_queue.dispatch_pending(&mut state) {
            Ok(_) => {}
            Err(e) => {
                eprintln!("payload-client: dispatch error: {e}; exiting");
                break;
            }
        }
        if let Err(e) = event_queue.flush() {
            eprintln!("payload-client: flush error: {e}; exiting");
            break;
        }

        let mut needs_commit = false;
        if let Some(requested) = read_control(&args.control_file) {
            if requested != current_color && requested != last_seen {
                eprintln!(
                    "payload-client: control-file requested {:?} (was {:?})",
                    requested, current_color
                );
                // Replace the buffer with the new color and re-attach.
                buffer = create_colored_buffer(&shm, &qh, state.size, requested)?;
                surface.attach(Some(&buffer), 0, 0);
                surface.damage_buffer(0, 0, state.size.0 as i32, state.size.1 as i32);
                surface.commit();
                current_color = requested;
                last_commit = Instant::now();
                needs_commit = true;
                eprintln!("payload-client: switched to {:?}", current_color);
            }
            last_seen = requested;
        }

        // Periodic re-commit to keep the wws server's compositor
        // from going idle. We just re-damage the surface; the
        // buffer content is unchanged so the server encodes the
        // same bytes, but its damage tracker fires and the encoder
        // loop continues to be exercised end-to-end.
        if !needs_commit && last_commit.elapsed() >= commit_interval {
            surface.damage_buffer(0, 0, state.size.0 as i32, state.size.1 as i32);
            surface.commit();
            last_commit = Instant::now();
        }
        event_queue.dispatch_pending(&mut state).ok();
        event_queue.flush().ok();
    }

    Ok(())
}

/// Read the control file and parse its content as a color name.
/// Returns `None` if the file is missing/unreadable or the content
/// doesn't name a known color (so a partial write doesn't cause a
/// spurious "switch to <garbage>" attempt).
fn read_control(path: &str) -> Option<Color> {
    let s = fs::read_to_string(path).ok()?;
    Color::parse(&s)
}

/// Allocate a fresh wl_shm buffer of the requested size, filled with
/// the requested color, and return the `wl_buffer` proxy. The buffer
/// is created from a fresh memfd-backed wl_shm_pool so we don't have
/// to worry about recycling the previous buffer (the compositor will
/// Release it as we attach the new one).
fn create_colored_buffer(
    shm: &wl_shm::WlShm,
    qh: &QueueHandle<AppState>,
    size: (u32, u32),
    color: Color,
) -> Result<wl_buffer::WlBuffer> {
    let (w, h) = size;
    let stride = w * 4;
    let byte_len = (stride * h) as usize;

    let pixel = color.argb32();
    let mut pixels = vec![0u8; byte_len];
    // Fill with the solid color. ARGB8888 wl_shm wants 32-bit pixels
    // in little-endian BGRA order on x86; we already laid out the
    // pixel as [B, G, R, A].
    for chunk in pixels.chunks_exact_mut(4) {
        chunk.copy_from_slice(&pixel);
    }

    // memfd_create + ftruncate + write, all in one. We use std::fs
    // for portability; the memfd is just a regular file the wl_shm
    // pool can mmap.
    let memfd_name = c"wws-payload-shm";
    let fd = memfd_create(memfd_name).context("memfd_create")?;
    let mut f = unsafe { std::fs::File::from_raw_fd(fd as RawFd) };
    f.set_len(byte_len as u64).context("ftruncate memfd")?;
    f.write_all(&pixels).context("write pixels to memfd")?;

    let pool = shm.create_pool(f.as_fd(), byte_len as i32, qh, ());
    let buffer = pool.create_buffer(
        0,
        w as i32,
        h as i32,
        stride as i32,
        wl_shm::Format::Argb8888,
        qh,
        (),
    );
    Ok(buffer)
}

fn memfd_create(name: &std::ffi::CStr) -> Result<i32> {
    // memfd_create(2) -- SYS_memfd_create on Linux. Returns a new fd
    // referring to an anonymous file backed by RAM. The flag 0 means
    // "no close-on-exec, no hugetlb, no sealing"; the test never
    // forks this process so we don't need sealing.
    const SYS_MEMFD_CREATE: i64 = 319;
    let ret = unsafe {
        libc::syscall(
            SYS_MEMFD_CREATE,
            name.as_ptr() as *const std::ffi::c_char,
            0u32,
        )
    };
    if ret < 0 {
        let err = std::io::Error::last_os_error();
        Err(anyhow::anyhow!("memfd_create failed: {err}"))
    } else {
        Ok(ret as i32)
    }
}

// ----- Wayland dispatch state -----

struct AppState {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    wm_base: Option<xdg_wm_base_protocol::XdgWmBase>,
    surface: Option<wl_surface::WlSurface>,
    /// True after the first xdg_surface::Configure arrives. Until then
    /// we don't know the size the compositor wants us to render at.
    configured: bool,
    size: (u32, u32),
    color: Color,
}

impl Dispatch<wl_registry::WlRegistry, ()> for AppState {
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
        } = event
        {
            match &interface[..] {
                "wl_compositor" => {
                    state.compositor = Some(
                        registry.bind::<wl_compositor::WlCompositor, _, _>(name, version, qh, ()),
                    );
                }
                "wl_shm" => {
                    state.shm =
                        Some(registry.bind::<wl_shm::WlShm, _, _>(name, version, qh, ()));
                }
                "xdg_wm_base" => {
                    state.wm_base = Some(
                        registry.bind::<xdg_wm_base_protocol::XdgWmBase, _, _>(
                            name, version, qh, (),
                        ),
                    );
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_compositor::WlCompositor, ()> for AppState {
    fn event(_: &mut Self, _: &wl_compositor::WlCompositor, _: wl_compositor::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<wl_surface::WlSurface, ()> for AppState {
    fn event(_: &mut Self, _: &wl_surface::WlSurface, _: wl_surface::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<wl_shm::WlShm, ()> for AppState {
    fn event(_: &mut Self, _: &wl_shm::WlShm, _: wl_shm::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for AppState {
    fn event(_: &mut Self, _: &wl_shm_pool::WlShmPool, _: wl_shm_pool::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<wl_buffer::WlBuffer, ()> for AppState {
    fn event(_: &mut Self, _: &wl_buffer::WlBuffer, _: wl_buffer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<xdg_wm_base_protocol::XdgWmBase, ()> for AppState {
    fn event(
        _: &mut Self,
        wm_base: &xdg_wm_base_protocol::XdgWmBase,
        event: xdg_wm_base_protocol::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base_protocol::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface_protocol::XdgSurface, ()> for AppState {
    fn event(
        state: &mut Self,
        xdg_surface: &xdg_surface_protocol::XdgSurface,
        event: xdg_surface_protocol::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface_protocol::Event::Configure { serial } = event {
            xdg_surface.ack_configure(serial);
            state.configured = true;
        }
    }
}

impl Dispatch<xdg_toplevel_protocol::XdgToplevel, ()> for AppState {
    fn event(
        state: &mut Self,
        _: &xdg_toplevel_protocol::XdgToplevel,
        event: xdg_toplevel_protocol::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            xdg_toplevel_protocol::Event::Configure { width, height, .. } => {
                // 0x0 means "compositor decides"; the wws server picks
                // the output size in that case. We accept whatever
                // we're given and render at that size.
                if width > 0 && height > 0 {
                    state.size = (width as u32, height as u32);
                }
            }
            xdg_toplevel_protocol::Event::Close => {
                eprintln!("payload-client: xdg_toplevel::Close received");
                // The smoke test sends SIGTERM to clean up; we don't
                // unwind on Close because the test owns the lifecycle.
            }
            _ => {}
        }
    }
}