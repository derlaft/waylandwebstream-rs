use std::os::unix::io::AsFd;
use wayland_client::{
    protocol::{wl_compositor, wl_shm, wl_shm_pool, wl_surface, wl_registry},
    Connection, Dispatch, QueueHandle,
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

/// Simple Wayland test client that creates a window with a red rectangle
/// This is used for automated testing of the compositor
fn main() {
    println!("Starting Wayland test client...");
    
    let conn = Connection::connect_to_env().expect("Failed to connect to Wayland display");
    let display = conn.display();
    
    let mut event_queue = conn.new_event_queue();
    let qh = event_queue.handle();
    
    let _registry = display.get_registry(&qh, ());
    
    let mut state = AppState {
        compositor: None,
        shm: None,
        wm_base: None,
        running: true,
    };
    
    // Round trip to get globals
    event_queue.roundtrip(&mut state).unwrap();
    
    assert!(state.compositor.is_some(), "No wl_compositor found");
    assert!(state.shm.is_some(), "No wl_shm found");
    assert!(state.wm_base.is_some(), "No xdg_wm_base found");
    
    println!("Found required Wayland globals");
    
    // Create a surface
    let surface = state.compositor.as_ref().unwrap().create_surface(&qh, ());
    let xdg_surface = state.wm_base.as_ref().unwrap().get_xdg_surface(&surface, &qh, ());
    let _toplevel = xdg_surface.get_toplevel(&qh, ());
    
    // Commit the surface
    surface.commit();
    
    // Wait for configure
    event_queue.roundtrip(&mut state).unwrap();
    
    // Create a shared memory buffer with a red rectangle
    let width = 800;
    let height = 600;
    let stride = width * 4;
    let size = stride * height;
    
    let tmp_file = tempfile::tempfile().expect("Failed to create temp file");
    tmp_file.set_len(size as u64).unwrap();
    
    let mut mmap = unsafe {
        memmap2::MmapMut::map_mut(&tmp_file).expect("Failed to mmap")
    };
    
    // Fill with bright red (0xFFFF0000 in ARGB format)
    for y in 0..height {
        for x in 0..width {
            let offset = (y * stride + x * 4) as usize;
            mmap[offset] = 0;      // B
            mmap[offset + 1] = 0;  // G
            mmap[offset + 2] = 255; // R
            mmap[offset + 3] = 255; // A
        }
    }
    
    // Create SHM pool and buffer
    let shm = state.shm.as_ref().unwrap();
    let pool = shm.create_pool(tmp_file.as_fd(), size as i32, &qh, ());
    let buffer = pool.create_buffer(
        0,
        width as i32,
        height as i32,
        stride as i32,
        wl_shm::Format::Argb8888,
        &qh,
        (),
    );
    
    // Attach buffer and commit
    surface.attach(Some(&buffer), 0, 0);
    surface.damage(0, 0, width as i32, height as i32);
    surface.commit();
    
    println!("Window created with red content, running for 30 seconds...");

    // The stream capture script (tests/stream_capture.js) budgets up to ~10s
    // for page navigation, 15s waiting for the first decoded frame, plus
    // a 2s settle -- comfortably under 30s, but 5s (the old value) raced
    // that budget and made the integration test flaky under load.
    let start = std::time::Instant::now();
    while state.running && start.elapsed().as_secs() < 30 {
        // A graceful compositor shutdown closes our connection cleanly --
        // treat that the same as `running` going false rather than
        // panicking, so stopping the compositor while this client is
        // attached doesn't look like a test client crash.
        if let Err(e) = event_queue.blocking_dispatch(&mut state) {
            println!("Wayland connection closed ({e}), exiting");
            break;
        }
    }
    
    println!("Test client exiting");
}

struct AppState {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    running: bool,
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

impl Dispatch<wayland_client::protocol::wl_buffer::WlBuffer, ()> for AppState {
    fn event(
        _: &mut Self,
        _: &wayland_client::protocol::wl_buffer::WlBuffer,
        _: wayland_client::protocol::wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for AppState {
    fn event(
        _: &mut Self,
        wm_base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for AppState {
    fn event(
        _: &mut Self,
        xdg_surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg_surface.ack_configure(serial);
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
