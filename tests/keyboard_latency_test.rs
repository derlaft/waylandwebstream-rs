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

/// End-to-end test: a real, OS-level browser key press (via Puppeteer's CDP
/// `page.keyboard` API) must travel through the WebSocket signaling channel,
/// get injected into Smithay's `wl_keyboard` seat capability
/// (`WaylandWebStreamState::key` in `src/compositor/state.rs`), reach the
/// `wayland-keyboard-client` test app, and come back out the other side as a
/// visibly different decoded video frame. Also reports the measured
/// glass-to-glass latency of that round trip (keydown/keyup to visual
/// black/white flip).
#[test]
fn test_keyboard_input_flips_compositor_output() {
    println!("Building compositor and test clients...");
    let build_status = Command::new("cargo")
        .args(&["build", "--release", "--workspace"])
        .status()
        .expect("Failed to build workspace");
    assert!(build_status.success(), "Build failed");

    let display_name = common::unique_display_name("wayland-keyboard-test");
    let port = common::unique_port();

    println!("Starting compositor...");
    let mut compositor = ChildGuard(start_compositor(&display_name, port));

    thread::sleep(Duration::from_secs(3));
    match compositor.0.try_wait() {
        Ok(Some(status)) => panic!("Compositor exited early with status: {status}"),
        Ok(None) => println!("Compositor is running"),
        Err(e) => panic!("Error checking compositor status: {e}"),
    }

    println!("Starting keyboard-reactive test client...");
    let _client = ChildGuard(start_keyboard_client(&display_name));
    thread::sleep(Duration::from_secs(2));

    println!("Running browser-driven keyboard latency capture...");
    let output = Command::new("node")
        .arg("tests/keyboard_latency_capture.js")
        .arg(port.to_string())
        .stderr(Stdio::inherit())
        .output()
        .expect("Failed to run keyboard latency capture script - ensure Node.js and tests/node_modules are installed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    print!("{stdout}");

    assert!(output.status.success(), "Keyboard latency capture failed");

    let (down_to_white_ms, up_to_black_ms) =
        parse_result_line(&stdout).expect("Capture script did not print a RESULT line");

    println!("keydown -> visual flip latency: {down_to_white_ms:.1} ms");
    println!("keyup   -> visual flip latency: {up_to_black_ms:.1} ms");

    // Generous bound: this is here to catch "input is wired but broken"
    // regressions (e.g. the flip never happens, or takes absurdly long),
    // not to enforce a strict perf budget. The pipeline involves software
    // H.264 encode/decode in a headless test environment.
    const MAX_LATENCY_MS: f64 = 6000.0;
    assert!(
        down_to_white_ms > 0.0 && down_to_white_ms < MAX_LATENCY_MS,
        "keydown-to-white latency {down_to_white_ms:.1}ms outside expected range"
    );
    assert!(
        up_to_black_ms > 0.0 && up_to_black_ms < MAX_LATENCY_MS,
        "keyup-to-black latency {up_to_black_ms:.1}ms outside expected range"
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

fn start_keyboard_client(display_name: &str) -> Child {
    let binary_path = PathBuf::from("./target/release/wayland-keyboard-client");
    Command::new(binary_path)
        .env("WAYLAND_DISPLAY", display_name)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("Failed to start wayland-keyboard-client - did `cargo build --release` build the workspace?")
}

/// Parses the `RESULT downToWhiteMs=<f64> upToBlackMs=<f64>` line
/// `keyboard_latency_capture.js` prints on success.
fn parse_result_line(stdout: &str) -> Option<(f64, f64)> {
    let rest = stdout.lines().find_map(|line| line.strip_prefix("RESULT "))?;

    let mut down_to_white = None;
    let mut up_to_black = None;
    for field in rest.split_whitespace() {
        if let Some(v) = field.strip_prefix("downToWhiteMs=") {
            down_to_white = v.parse().ok();
        } else if let Some(v) = field.strip_prefix("upToBlackMs=") {
            up_to_black = v.parse().ok();
        }
    }
    Some((down_to_white?, up_to_black?))
}
