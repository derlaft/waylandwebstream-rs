use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;

mod common;

/// Kills (and reaps) its child on drop, including during a panic unwind, so
/// a failed assertion never leaves the compositor or test client running.
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// End-to-end test: a synthetic browser touch event must travel through the
/// WebSocket signaling channel, get injected into Smithay's `wl_touch` seat
/// capability (`WaylandWebStreamState::touch_down` in
/// `src/compositor/state.rs`), reach the `wayland-touch-client` test app,
/// and come back out the other side as a visibly different decoded video
/// frame. Also reports the measured glass-to-glass latency of that round
/// trip (touch down/up to visual black/white flip).
#[test]
fn test_touch_input_flips_compositor_output() {
    println!("Building compositor and test clients...");
    let build_status = Command::new("cargo")
        .args(&["build", "--release", "--workspace"])
        .status()
        .expect("Failed to build workspace");
    assert!(build_status.success(), "Build failed");

    let display_name = common::unique_display_name("wayland-touch-test");
    let port = common::unique_port();

    println!("Starting compositor...");
    let mut compositor = ChildGuard(start_compositor(&display_name, port));

    thread::sleep(Duration::from_secs(3));
    match compositor.0.try_wait() {
        Ok(Some(status)) => panic!("Compositor exited early with status: {status}"),
        Ok(None) => println!("Compositor is running"),
        Err(e) => panic!("Error checking compositor status: {e}"),
    }

    println!("Starting touch-reactive test client...");
    let _client = ChildGuard(start_touch_client(&display_name));
    thread::sleep(Duration::from_secs(2));

    println!("Running browser-driven touch latency capture...");
    let output = Command::new("node")
        .arg("tests/touch_latency_capture.js")
        .arg(port.to_string())
        .stderr(Stdio::inherit())
        .output()
        .expect("Failed to run touch latency capture script - ensure Node.js and tests/node_modules are installed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    print!("{stdout}");

    assert!(output.status.success(), "Touch latency capture failed");

    let (press_to_white_ms, release_to_black_ms) =
        parse_result_line(&stdout).expect("Capture script did not print a RESULT line");

    println!("touch-down -> visual flip latency: {press_to_white_ms:.1} ms");
    println!("touch-up   -> visual flip latency: {release_to_black_ms:.1} ms");

    // Generous bound: this is here to catch "input is wired but broken"
    // regressions (e.g. the flip never happens, or takes absurdly long),
    // not to enforce a strict perf budget. The pipeline involves software
    // H.264 encode/decode in a headless test environment.
    const MAX_LATENCY_MS: f64 = 6000.0;
    assert!(
        press_to_white_ms > 0.0 && press_to_white_ms < MAX_LATENCY_MS,
        "touch-down-to-white latency {press_to_white_ms:.1}ms outside expected range"
    );
    assert!(
        release_to_black_ms > 0.0 && release_to_black_ms < MAX_LATENCY_MS,
        "touch-up-to-black latency {release_to_black_ms:.1}ms outside expected range"
    );

    println!("Test passed!");
}

fn start_compositor(display_name: &str, port: u16) -> Child {
    let binary_path = PathBuf::from("./target/release/waylandwebstream");
    Command::new(binary_path)
        .arg("--display-name")
        .arg(display_name)
        .arg("--port")
        .arg(port.to_string())
        .env("RUST_LOG", "info")
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("Failed to start compositor")
}

fn start_touch_client(display_name: &str) -> Child {
    let binary_path = PathBuf::from("./target/release/wayland-touch-client");
    Command::new(binary_path)
        .env("WAYLAND_DISPLAY", display_name)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("Failed to start wayland-touch-client - did `cargo build --release` build the workspace?")
}

/// Parses the `RESULT pressToWhiteMs=<f64> releaseToBlackMs=<f64>` line
/// `touch_latency_capture.js` prints on success.
fn parse_result_line(stdout: &str) -> Option<(f64, f64)> {
    let rest = stdout.lines().find_map(|line| line.strip_prefix("RESULT "))?;

    let mut press_to_white = None;
    let mut release_to_black = None;
    for field in rest.split_whitespace() {
        if let Some(v) = field.strip_prefix("pressToWhiteMs=") {
            press_to_white = v.parse().ok();
        } else if let Some(v) = field.strip_prefix("releaseToBlackMs=") {
            release_to_black = v.parse().ok();
        }
    }
    Some((press_to_white?, release_to_black?))
}
