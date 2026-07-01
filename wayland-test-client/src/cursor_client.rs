/// Cursor-reactive Wayland test client used by the cursor rendering e2e test.
///
/// Creates a small toplevel window (opaque black) so the compositor maps it
/// and starts delivering pointer events to it.  On `wl_pointer::Enter` it
/// sets a solid magenta 16×16 custom cursor via `wl_pointer::set_cursor`,
/// which causes the compositor to call its `cursor_image()` callback and
/// forward the pixel data to connected browser clients.  On `Leave` it resets
/// the cursor to the default (no surface).
use std::os::unix::io::AsFd;
use wayland_client::{
    protocol::{
        wl_buffer, wl_compositor, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface,
    },
    Connection, Dispatch, QueueHandle,
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

const WIN_W: i32 = 64;
const WIN_H: i32 = 64;
const CUR_W: i32 = 16;
const CUR_H: i32 = 16;

fn main() {
    println!("Starting Wayland cursor test client...");

    let conn = Connection::connect_to_env().expect("Failed to connect to Wayland display");
    let display = conn.display();
    let mut event_queue = conn.new_event_queue();
    let qh = event_queue.handle();
    let _registry = display.get_registry(&qh, ());

    let mut state = AppState {
        compositor: None,
        shm: None,
        wm_base: None,
        seat: None,
        running: true,
        surface: None,
        window_buffer: None,
        cursor_surface: None,
        cursor_buffer: None,
        pointer: None,
        serial: 0,
    };

    event_queue.roundtrip(&mut state).unwrap();
    assert!(state.compositor.is_some(), "No wl_compositor found");
    assert!(state.shm.is_some(), "No wl_shm found");
    assert!(state.wm_base.is_some(), "No xdg_wm_base found");
    assert!(state.seat.is_some(), "No wl_seat found");

    println!("Found required Wayland globals");

    // ── main application window ──────────────────────────────────────────
    let win_stride = WIN_W * 4;
    let win_size = (win_stride * WIN_H) as usize;

    // ── cursor surface (plain wl_surface; not an xdg_surface) ───────────
    let cur_stride = CUR_W * 4;
    let cur_size = (cur_stride * CUR_H) as usize;

    let pool_size = win_size + cur_size;

    let tmp = tempfile::tempfile().expect("Failed to create temp file");
    tmp.set_len(pool_size as u64).unwrap();

    let mut mmap = unsafe { memmap2::MmapMut::map_mut(&tmp).expect("Failed to mmap") };

    // Window: opaque black (BGRA = 0,0,0,255)
    for pixel in mmap[..win_size].chunks_exact_mut(4) {
        pixel.copy_from_slice(&[0, 0, 0, 255]);
    }
    // Cursor: solid magenta (BGRA: B=255, G=0, R=255, A=255)
    for pixel in mmap[win_size..pool_size].chunks_exact_mut(4) {
        pixel.copy_from_slice(&[255, 0, 255, 255]);
    }

    let shm = state.shm.as_ref().unwrap();
    let pool = shm.create_pool(tmp.as_fd(), pool_size as i32, &qh, ());

    let window_buffer = pool.create_buffer(
        0,
        WIN_W,
        WIN_H,
        win_stride,
        wl_shm::Format::Argb8888,
        &qh,
        (),
    );
    let cursor_buffer = pool.create_buffer(
        win_size as i32,
        CUR_W,
        CUR_H,
        cur_stride,
        wl_shm::Format::Argb8888,
        &qh,
        (),
    );

    // Create and commit the cursor surface (no xdg role; role is set by
    // wl_pointer.set_cursor implicitly).
    let cursor_surface = state.compositor.as_ref().unwrap().create_surface(&qh, ());
    cursor_surface.attach(Some(&cursor_buffer), 0, 0);
    cursor_surface.damage(0, 0, CUR_W, CUR_H);
    cursor_surface.commit();

    // Create and map the main window.
    let surface = state.compositor.as_ref().unwrap().create_surface(&qh, ());
    let xdg_surface = state
        .wm_base
        .as_ref()
        .unwrap()
        .get_xdg_surface(&surface, &qh, ());
    let _toplevel = xdg_surface.get_toplevel(&qh, ());
    let _pointer = state.seat.as_ref().unwrap().get_pointer(&qh, ());
    surface.commit();

    // Wait for configure.
    event_queue.roundtrip(&mut state).unwrap();

    surface.attach(Some(&window_buffer), 0, 0);
    surface.damage(0, 0, WIN_W, WIN_H);
    surface.commit();

    state.surface = Some(surface);
    state.window_buffer = Some(window_buffer);
    state.cursor_surface = Some(cursor_surface);
    state.cursor_buffer = Some(cursor_buffer);

    println!("Cursor client ready: will set magenta cursor on pointer enter");

    let run_secs: u64 = std::env::var("CURSOR_CLIENT_RUN_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(120);
    let start = std::time::Instant::now();
    while state.running && start.elapsed().as_secs() < run_secs {
        event_queue.blocking_dispatch(&mut state).unwrap();
    }
    println!("Cursor client exiting");
}

struct AppState {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    seat: Option<wl_seat::WlSeat>,
    running: bool,
    surface: Option<wl_surface::WlSurface>,
    window_buffer: Option<wl_buffer::WlBuffer>,
    cursor_surface: Option<wl_surface::WlSurface>,
    cursor_buffer: Option<wl_buffer::WlBuffer>,
    pointer: Option<wl_pointer::WlPointer>,
    /// Serial from the most recent pointer enter event, needed for set_cursor.
    serial: u32,
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
                    state.compositor = Some(registry.bind(name, version, qh, ()));
                }
                "wl_shm" => {
                    state.shm = Some(registry.bind(name, version, qh, ()));
                }
                "xdg_wm_base" => {
                    state.wm_base = Some(registry.bind(name, version, qh, ()));
                }
                "wl_seat" => {
                    state.seat = Some(registry.bind(name, version, qh, ()));
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
impl Dispatch<wl_seat::WlSeat, ()> for AppState {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities { capabilities } = event {
            use wayland_client::WEnum;
            if let WEnum::Value(caps) = capabilities {
                if caps.contains(wl_seat::Capability::Pointer) && state.pointer.is_none() {
                    state.pointer = Some(seat.get_pointer(qh, ()));
                }
            }
        }
    }
}
impl Dispatch<xdg_wm_base::XdgWmBase, ()> for AppState {
    fn event(
        _: &mut Self,
        base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            base.pong(serial);
        }
    }
}
impl Dispatch<xdg_surface::XdgSurface, ()> for AppState {
    fn event(
        _: &mut Self,
        xdg: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg.ack_configure(serial);
        }
    }
}
impl Dispatch<xdg_toplevel::XdgToplevel, ()> for AppState {
    fn event(
        state: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_toplevel::Event::Close = event {
            state.running = false;
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for AppState {
    fn event(
        state: &mut Self,
        pointer: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter { serial, .. } => {
                println!("pointer enter (serial {serial}), setting magenta cursor");
                state.serial = serial;
                if let (Some(cursor_surface), Some(ptr)) = (&state.cursor_surface, &state.pointer) {
                    // Hotspot at (0,0): top-left corner of the cursor image.
                    ptr.set_cursor(serial, Some(cursor_surface), 0, 0);
                }
                let _ = pointer;
            }
            wl_pointer::Event::Leave { .. } => {
                println!("pointer leave, resetting cursor");
                if let Some(ptr) = &state.pointer {
                    ptr.set_cursor(state.serial, None, 0, 0);
                }
            }
            _ => {}
        }
    }
}
