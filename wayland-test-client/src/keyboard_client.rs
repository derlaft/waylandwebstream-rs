use std::os::unix::io::AsFd;
use wayland_client::{
    protocol::{wl_buffer, wl_compositor, wl_keyboard, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface},
    Connection, Dispatch, QueueHandle,
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

/// Keyboard-reactive Wayland test client used by the keyboard-latency
/// integration test. Renders solid black while idle and solid white while
/// the evdev `KEY_A` (30) key is held, so a remote observer (the browser,
/// watching the decoded video on a <canvas>) can detect "the compositor
/// received my keystroke" with a trivial brightness check that survives
/// H.264 compression.
const WIDTH: i32 = 64;
const HEIGHT: i32 = 64;

/// `linux/input-event-codes.h` KEY_A -- matches `src/input/keyboard.rs`'s
/// `evdev_keycode("KeyA")` on the server side.
const KEY_A: u32 = 30;

fn main() {
    println!("Starting Wayland keyboard-reactive test client...");

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
        black_buffer: None,
        white_buffer: None,
        is_white: false,
    };

    // Round trip to get globals.
    event_queue.roundtrip(&mut state).unwrap();

    assert!(state.compositor.is_some(), "No wl_compositor found");
    assert!(state.shm.is_some(), "No wl_shm found");
    assert!(state.wm_base.is_some(), "No xdg_wm_base found");
    assert!(state.seat.is_some(), "No wl_seat found");

    println!("Found required Wayland globals");

    let surface = state.compositor.as_ref().unwrap().create_surface(&qh, ());
    let xdg_surface = state.wm_base.as_ref().unwrap().get_xdg_surface(&surface, &qh, ());
    let _toplevel = xdg_surface.get_toplevel(&qh, ());
    let _keyboard = state.seat.as_ref().unwrap().get_keyboard(&qh, ());

    surface.commit();

    // Wait for configure.
    event_queue.roundtrip(&mut state).unwrap();

    // One SHM pool backing two fixed buffers (black, white) so reacting to a
    // keystroke is just an attach -- no per-frame pixel fill.
    let stride = WIDTH * 4;
    let buffer_size = (stride * HEIGHT) as usize;
    let pool_size = buffer_size * 2;

    let tmp_file = tempfile::tempfile().expect("Failed to create temp file");
    tmp_file.set_len(pool_size as u64).unwrap();

    let mut mmap = unsafe { memmap2::MmapMut::map_mut(&tmp_file).expect("Failed to mmap") };
    for pixel in mmap[..buffer_size].chunks_exact_mut(4) {
        pixel.copy_from_slice(&[0, 0, 0, 255]); // opaque black
    }
    for pixel in mmap[buffer_size..pool_size].chunks_exact_mut(4) {
        pixel.copy_from_slice(&[255, 255, 255, 255]); // opaque white
    }

    let shm = state.shm.as_ref().unwrap();
    let pool = shm.create_pool(tmp_file.as_fd(), pool_size as i32, &qh, ());
    let black_buffer = pool.create_buffer(0, WIDTH, HEIGHT, stride, wl_shm::Format::Argb8888, &qh, ());
    let white_buffer = pool.create_buffer(
        buffer_size as i32,
        WIDTH,
        HEIGHT,
        stride,
        wl_shm::Format::Argb8888,
        &qh,
        (),
    );

    surface.attach(Some(&black_buffer), 0, 0);
    surface.damage(0, 0, WIDTH, HEIGHT);
    surface.commit();

    state.surface = Some(surface);
    state.black_buffer = Some(black_buffer);
    state.white_buffer = Some(white_buffer);

    println!("Keyboard test client ready: black=idle, white=KEY_A held");

    let run_secs: u64 = std::env::var("KEYBOARD_CLIENT_RUN_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(120);
    let start = std::time::Instant::now();
    while state.running && start.elapsed().as_secs() < run_secs {
        event_queue.blocking_dispatch(&mut state).unwrap();
    }

    println!("Keyboard test client exiting");
}

struct AppState {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    seat: Option<wl_seat::WlSeat>,
    running: bool,
    surface: Option<wl_surface::WlSurface>,
    black_buffer: Option<wl_buffer::WlBuffer>,
    white_buffer: Option<wl_buffer::WlBuffer>,
    is_white: bool,
}

impl AppState {
    fn set_white(&mut self, want_white: bool) {
        if want_white == self.is_white {
            return;
        }
        self.is_white = want_white;
        let surface = self.surface.as_ref().unwrap();
        let buffer = if want_white {
            self.white_buffer.as_ref().unwrap()
        } else {
            self.black_buffer.as_ref().unwrap()
        };
        surface.attach(Some(buffer), 0, 0);
        surface.damage(0, 0, WIDTH, HEIGHT);
        surface.commit();
        println!("flip -> {}", if want_white { "white" } else { "black" });
    }
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
        if let wl_registry::Event::Global { name, interface, version } = event {
            match &interface[..] {
                "wl_compositor" => {
                    state.compositor = Some(registry.bind::<wl_compositor::WlCompositor, _, _>(name, version, qh, ()));
                }
                "wl_shm" => {
                    state.shm = Some(registry.bind::<wl_shm::WlShm, _, _>(name, version, qh, ()));
                }
                "xdg_wm_base" => {
                    state.wm_base = Some(registry.bind::<xdg_wm_base::XdgWmBase, _, _>(name, version, qh, ()));
                }
                "wl_seat" => {
                    state.seat = Some(registry.bind::<wl_seat::WlSeat, _, _>(name, version, qh, ()));
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

impl Dispatch<wl_seat::WlSeat, ()> for AppState {
    fn event(_: &mut Self, _: &wl_seat::WlSeat, _: wl_seat::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for AppState {
    fn event(_: &mut Self, wm_base: &xdg_wm_base::XdgWmBase, event: xdg_wm_base::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for AppState {
    fn event(_: &mut Self, xdg_surface: &xdg_surface::XdgSurface, event: xdg_surface::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg_surface.ack_configure(serial);
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for AppState {
    fn event(state: &mut Self, _: &xdg_toplevel::XdgToplevel, event: xdg_toplevel::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_toplevel::Event::Close = event {
            state.running = false;
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for AppState {
    fn event(state: &mut Self, _: &wl_keyboard::WlKeyboard, event: wl_keyboard::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            wl_keyboard::Event::Enter { .. } => {
                println!("keyboard enter");
            }
            wl_keyboard::Event::Leave { .. } => {
                println!("keyboard leave");
                // No `wl_keyboard.frame` batching exists, but losing focus
                // mid-press should still drop the held visual state.
                state.set_white(false);
            }
            wl_keyboard::Event::Key { key, state: key_state, .. } => {
                println!("key {} {:?}", key, key_state);
                if key == KEY_A {
                    let pressed = key_state == wayland_client::WEnum::Value(wl_keyboard::KeyState::Pressed);
                    state.set_white(pressed);
                }
            }
            _ => {}
        }
    }
}
