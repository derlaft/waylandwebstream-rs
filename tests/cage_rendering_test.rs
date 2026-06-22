//! Regression test for: running `cage` (or any nested wlroots compositor)
//! inside waylandwebstream renders as a solid black screen even though the
//! nested client is actively drawing.
//!
//! This drives `WaylandWebStreamState` directly (rather than through the
//! `waylandwebstream` binary + WebRTC pipeline) so the captured framebuffer
//! from `render()` can be inspected pixel-by-pixel in-process.

use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use smithay::reexports::calloop::EventLoop;
use smithay::reexports::wayland_server::Display;
use smithay::wayland::compositor::CompositorClientState;
use smithay::wayland::socket::ListeningSocketSource;

use waylandwebstream::compositor::state::{ClientState, WaylandWebStreamState};

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;

#[test]
fn cage_window_renders_visible_content_not_black() {
    if which("cage").is_none() || which("weston-simple-shm").is_none() {
        eprintln!("skipping: `cage` and/or `weston-simple-shm` not found in PATH");
        return;
    }

    let display_name = format!("wayland-cage-regress-{}", std::process::id());

    let mut event_loop: EventLoop<WaylandWebStreamState> =
        EventLoop::try_new().expect("failed to create event loop");
    let mut display: Display<WaylandWebStreamState> =
        Display::new().expect("failed to create display");

    let mut state = WaylandWebStreamState::new(&mut event_loop, &mut display, WIDTH, HEIGHT);

    let socket_source = ListeningSocketSource::with_name(&display_name)
        .expect("failed to create wayland listening socket");

    let mut display_handle = display.handle();
    event_loop
        .handle()
        .insert_source(socket_source, move |client_stream, _, _state| {
            let client_state = ClientState {
                compositor_state: CompositorClientState::default(),
            };
            let _ = display_handle.insert_client(client_stream, Arc::new(client_state));
        })
        .expect("failed to insert listening socket into event loop");

    let mut cage = spawn_cage(&display_name);

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut window_seen = false;
    let mut captured: Option<Vec<u8>> = None;

    while Instant::now() < deadline {
        event_loop
            .dispatch(Duration::from_millis(16), &mut state)
            .expect("event loop dispatch failed");
        display
            .dispatch_clients(&mut state)
            .expect("failed to dispatch wayland clients");
        state.send_frames();
        display.flush_clients().expect("failed to flush clients");

        if state.space.elements().count() > 0 {
            window_seen = true;
            // Give the client a few frames to actually paint after mapping
            // before we sample, then capture and stop.
            if captured.is_none() {
                for _ in 0..30 {
                    event_loop
                        .dispatch(Duration::from_millis(16), &mut state)
                        .expect("event loop dispatch failed");
                    display
                        .dispatch_clients(&mut state)
                        .expect("failed to dispatch wayland clients");
                    state.send_frames();
                    display.flush_clients().expect("failed to flush clients");
                }
                captured = state.render(None);
                break;
            }
        }
    }

    let _ = cage.kill();
    let _ = cage.wait();

    assert!(window_seen, "cage never mapped a toplevel window in time");
    let framebuffer = captured.expect("render() returned no framebuffer");

    // Ignore the alpha channel (byte 3 of every pixel) since the background
    // clear unconditionally sets it to 255 regardless of window content.
    let nonzero_color_bytes = framebuffer
        .chunks_exact(4)
        .filter(|px| px[0] != 0 || px[1] != 0 || px[2] != 0)
        .count();

    assert!(
        nonzero_color_bytes > 0,
        "framebuffer is entirely black even though cage has a window with a painting client; \
         expected at least some non-black pixels from the nested client's content"
    );
}

fn spawn_cage(display_name: &str) -> Child {
    Command::new("cage")
        .arg("-D")
        .arg("--")
        .arg("weston-simple-shm")
        .env("WAYLAND_DISPLAY", display_name)
        .env("WLR_BACKENDS", "wayland")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to start cage")
}

fn which(bin: &str) -> Option<std::path::PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(bin))
            .find(|candidate| candidate.is_file())
    })
}
