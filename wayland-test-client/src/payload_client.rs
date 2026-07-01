// Test-only payload client used by smoke_e2e.rs.
//
// Renders a solid-color wl_shm surface to the wws-server compositor.
// Polls a control file every 50ms; when the content changes ("white" ↔
// "black"), it recreates the buffer and commits. Also re-commits
// periodically to keep the server's compositor from going idle.
//
// CLI: payload-client [--color white|black] [--control-file PATH]

use std::os::fd::{AsFd, FromRawFd};
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

const DEFAULT_COLOR: &str = "white";
const DEFAULT_CONTROL_FILE: &str = "/tmp/wws-payload-color";
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const COMMIT_INTERVAL: Duration = Duration::from_millis(250);

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

    fn argb32(self) -> [u8; 4] {
        match self {
            Color::White => [0xFF, 0xFF, 0xFF, 0xFF],
            Color::Black => [0x00, 0x00, 0x00, 0xFF],
        }
    }
}

struct Args {
    color: Color,
    control_file: String,
}

fn parse_args() -> Result<Args> {
    let mut color_str = DEFAULT_COLOR.to_string();
    let mut control_file = DEFAULT_CONTROL_FILE.to_string();
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--color" => {
                color_str = iter
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--color requires a value"))?;
            }
            "--control-file" => {
                control_file = iter
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--control-file requires a value"))?;
            }
            "--help" | "-h" => {
                println!("payload-client [--color white|black] [--control-file PATH]");
                std::process::exit(0);
            }
            other => anyhow::bail!("unknown argument: {other}"),
        }
    }
    let color = Color::parse(&color_str)
        .ok_or_else(|| anyhow::anyhow!("invalid --color {color_str:?} (use white or black)"))?;
    Ok(Args {
        color,
        control_file,
    })
}

fn main() -> Result<()> {
    let args = parse_args()?;
    eprintln!(
        "payload-client starting: color={:?} control_file={:?}",
        args.color, args.control_file
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
        size: (320, 240),
        color: args.color,
    };

    event_queue
        .roundtrip(&mut state)
        .context("initial roundtrip")?;

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

    let surface = compositor.create_surface(&qh, ());
    let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg_surface.get_toplevel(&qh, ());
    toplevel.set_title("wws-payload-client".into());
    surface.commit();
    state.surface = Some(surface.clone());

    // Wait for xdg_surface::Configure so we know the compositor's chosen size.
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
        "payload-client: configured at {}x{}",
        state.size.0, state.size.1
    );

    let mut current_color = state.color;
    let mut committed_size = state.size;
    let mut buffer = create_colored_buffer(&shm, &qh, state.size, current_color)?;
    surface.attach(Some(&buffer), 0, 0);
    surface.damage_buffer(0, 0, state.size.0 as i32, state.size.1 as i32);
    surface.commit();
    event_queue.flush().context("flush after initial commit")?;
    eprintln!("payload-client: committed {:?}", current_color);

    let mut last_commit = Instant::now();
    loop {
        std::thread::sleep(POLL_INTERVAL);

        // Read from the socket so xdg_wm_base::Ping events are dispatched
        // (and ponged) -- without this, the compositor marks us non-responsive.
        if let Some(guard) = event_queue.prepare_read() {
            let _ = guard.read();
        }
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

        // Compositor sent a resize configure — submit a new correctly-sized
        // buffer immediately so the compositor can render the new size.
        // Without this the server's smithay compositor gets stale (it only
        // re-renders on new commits, not on a resize by itself).
        if state.size != committed_size {
            eprintln!(
                "payload-client: resizing buffer {}x{} → {}x{}",
                committed_size.0, committed_size.1, state.size.0, state.size.1
            );
            committed_size = state.size;
            buffer = create_colored_buffer(&shm, &qh, committed_size, current_color)?;
            surface.attach(Some(&buffer), 0, 0);
            surface.damage_buffer(0, 0, committed_size.0 as i32, committed_size.1 as i32);
            surface.commit();
            last_commit = Instant::now();
            event_queue.flush().ok();
            continue;
        }

        if let Some(requested) = read_control(&args.control_file) {
            if requested != current_color {
                eprintln!("payload-client: switching to {:?}", requested);
                buffer = create_colored_buffer(&shm, &qh, state.size, requested)?;
                surface.attach(Some(&buffer), 0, 0);
                surface.damage_buffer(0, 0, state.size.0 as i32, state.size.1 as i32);
                surface.commit();
                current_color = requested;
                last_commit = Instant::now();
                event_queue.flush().ok();
                continue;
            }
        }

        // Periodic re-commit to keep the compositor from going idle.
        // Re-attach the buffer explicitly: some compositors (smithay headless)
        // don't re-render on damage-only commits without a new attach.
        if last_commit.elapsed() >= COMMIT_INTERVAL {
            surface.attach(Some(&buffer), 0, 0);
            surface.damage_buffer(0, 0, state.size.0 as i32, state.size.1 as i32);
            surface.commit();
            last_commit = Instant::now();
            event_queue.flush().ok();
        }
    }

    Ok(())
}

fn read_control(path: &str) -> Option<Color> {
    let s = std::fs::read_to_string(path).ok()?;
    Color::parse(&s)
}

fn create_colored_buffer(
    shm: &wl_shm::WlShm,
    qh: &QueueHandle<AppState>,
    size: (u32, u32),
    color: Color,
) -> Result<wl_buffer::WlBuffer> {
    use std::io::Write;
    let (w, h) = size;
    let stride = w as usize * 4;
    let byte_len = stride * h as usize;

    let name = c"wws-payload-shm";
    let raw_fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if raw_fd < 0 {
        anyhow::bail!("memfd_create: {}", std::io::Error::last_os_error());
    }
    let mut f = unsafe { std::fs::File::from_raw_fd(raw_fd) };
    f.set_len(byte_len as u64).context("ftruncate")?;

    let row = color.argb32().repeat(w as usize);
    for _ in 0..h {
        f.write_all(&row).context("write pixels")?;
    }

    let pool = shm.create_pool(f.as_fd(), byte_len as i32, qh, ());
    Ok(pool.create_buffer(
        0,
        w as i32,
        h as i32,
        stride as i32,
        wl_shm::Format::Argb8888,
        qh,
        (),
    ))
}

// ----- Wayland dispatch state -----

struct AppState {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    wm_base: Option<xdg_wm_base_protocol::XdgWmBase>,
    surface: Option<wl_surface::WlSurface>,
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
                    state.compositor = Some(registry.bind::<wl_compositor::WlCompositor, _, _>(
                        name,
                        version,
                        qh,
                        (),
                    ));
                }
                "wl_shm" => {
                    state.shm = Some(registry.bind::<wl_shm::WlShm, _, _>(name, version, qh, ()));
                }
                "xdg_wm_base" => {
                    state.wm_base = Some(registry.bind::<xdg_wm_base_protocol::XdgWmBase, _, _>(
                        name,
                        version,
                        qh,
                        (),
                    ));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_compositor::WlCompositor, ()> for AppState {
    fn event(
        _: &mut Self,
        _: &wl_compositor::WlCompositor,
        _: wl_compositor::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for AppState {
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

impl Dispatch<wl_shm::WlShm, ()> for AppState {
    fn event(
        _: &mut Self,
        _: &wl_shm::WlShm,
        _: wl_shm::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for AppState {
    fn event(
        _: &mut Self,
        _: &wl_shm_pool::WlShmPool,
        _: wl_shm_pool::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_buffer::WlBuffer, ()> for AppState {
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
                if width > 0 && height > 0 {
                    state.size = (width as u32, height as u32);
                }
            }
            xdg_toplevel_protocol::Event::Close => {
                eprintln!("payload-client: Close received");
            }
            _ => {}
        }
    }
}
