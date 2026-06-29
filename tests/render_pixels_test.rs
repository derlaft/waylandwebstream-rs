//! Hermetic, in-process pixel check for `WaylandWebStreamState::render()`.
//!
//! Drives the compositor directly (no streaming pipeline, no browser) with a
//! real Wayland client -- `weston-simple-shm` connecting straight to our
//! `xdg_shell`/`wl_shm` rather than through a nested wlroots compositor -- so
//! the composited framebuffer can be inspected pixel-by-pixel. This is the
//! GPU-free counterpart to `cage_rendering_test`: `cage`/`labwc` need a
//! wlroots renderer that can't come up on a headless box with no `/dev/dri`,
//! whereas `weston-simple-shm` only needs SHM and so runs anywhere.
//!
//! It guards the damage-driven partial-repaint render path against the obvious
//! catastrophic regressions -- a black screen, or a frame that loses its
//! carried-over content -- across two consecutive renders (the first a full
//! repaint, the second going through the partial-repaint/canvas-carryover
//! path after the client paints again).

use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use smithay::reexports::calloop::EventLoop;
use smithay::reexports::wayland_server::Display;
use smithay::wayland::compositor::CompositorClientState;
use smithay::wayland::socket::ListeningSocketSource;

use waylandwebstream::compositor::state::{ClientState, WaylandWebStreamState};

mod common;

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;

/// Counts opaque, non-black pixels (alpha is always 255 from the clear, so it
/// is ignored).
fn nonblack_pixels(framebuffer: &[u8]) -> usize {
    framebuffer
        .chunks_exact(4)
        .filter(|px| px[0] != 0 || px[1] != 0 || px[2] != 0)
        .count()
}

#[test]
fn render_composites_client_content_across_two_frames() {
    if which("weston-simple-shm").is_none() {
        eprintln!("skipping: `weston-simple-shm` not found in PATH");
        return;
    }

    let display_name = common::unique_display_name("wayland-render-pixels");

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

    let mut client = spawn_simple_shm(&display_name);

    // Pump the loop until the client maps a toplevel, then give it a few more
    // frames to actually paint before sampling.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut window_seen = false;
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
            break;
        }
    }
    assert!(window_seen, "client never mapped a toplevel window in time");

    // First render: a full repaint (the canvas starts empty). Must show the
    // client's content, not a black screen.
    let frame1 = state.render(None).expect("render() returned no framebuffer");
    assert_eq!(frame1.len(), (WIDTH * HEIGHT * 4) as usize, "frame1 wrong size");
    let frame1_nonblack = nonblack_pixels(&frame1);
    assert!(
        frame1_nonblack > 0,
        "first frame is entirely black even though a painting client is mapped"
    );

    // Let the animated client paint again, then render once more. This second
    // render goes through the partial-repaint path (take_dirty() stashes the
    // new surface damage; everything else is carried over from the canvas).
    // A broken carryover would drop the content -> a (near-)black frame.
    for _ in 0..15 {
        event_loop
            .dispatch(Duration::from_millis(16), &mut state)
            .expect("event loop dispatch failed");
        display
            .dispatch_clients(&mut state)
            .expect("failed to dispatch wayland clients");
        state.send_frames();
        display.flush_clients().expect("failed to flush clients");
    }
    let _ = state.take_dirty(); // stash this frame's damage for render()
    let frame2 = state.render(None).expect("render() returned no framebuffer");
    let frame2_nonblack = nonblack_pixels(&frame2);
    assert!(
        frame2_nonblack > 0,
        "second (partial-repaint) frame lost all content -> canvas carryover is broken"
    );

    let _ = client.kill();
    let _ = client.wait();
}

fn spawn_simple_shm(display_name: &str) -> Child {
    Command::new("weston-simple-shm")
        .env("WAYLAND_DISPLAY", display_name)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to start weston-simple-shm")
}

fn which(bin: &str) -> Option<std::path::PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(bin))
            .find(|candidate| candidate.is_file())
    })
}
