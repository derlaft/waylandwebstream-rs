/// End-to-end cursor rendering test.
///
/// Starts the compositor and the `wayland-cursor-client` test binary, then
/// runs `tests/cursor_capture.js` (a Puppeteer script) which moves the mouse
/// over the streamed canvas and verifies that the compositor pushed a custom
/// cursor surface to the browser, which applied it as a CSS `url(...)` cursor
/// on the `<canvas>` element.
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;

mod common;

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn test_cursor_surface_reaches_browser() {
    println!("Building compositor and test clients...");
    let build_status = Command::new("cargo")
        .args(["build", "--release", "--workspace"])
        .status()
        .expect("Failed to build workspace");
    assert!(build_status.success(), "Build failed");

    let display_name = common::unique_display_name("wayland-cursor-test");
    let port = common::unique_port();

    println!("Starting compositor on display {display_name}, port {port}...");
    let mut _compositor = ChildGuard(start_compositor(&display_name, port));

    thread::sleep(Duration::from_secs(3));
    match _compositor.0.try_wait() {
        Ok(Some(status)) => panic!("Compositor exited early with status: {status}"),
        Ok(None) => println!("Compositor is running"),
        Err(e) => panic!("Error checking compositor status: {e}"),
    }

    println!("Starting cursor test client...");
    let _client = ChildGuard(start_cursor_client(&display_name));
    thread::sleep(Duration::from_secs(2));

    println!("Running cursor capture script...");
    let output = Command::new("node")
        .arg("tests/cursor_capture.js")
        .arg(port.to_string())
        .stderr(Stdio::inherit())
        .output()
        .expect("Failed to run cursor capture script — ensure Node.js and tests/node_modules are installed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    print!("{stdout}");

    assert!(output.status.success(), "Cursor capture script failed");
    assert!(
        stdout
            .lines()
            .any(|l| l.starts_with("RESULT cursor_set=true")),
        "Cursor capture did not print the expected RESULT line",
    );

    println!("Cursor rendering test passed!");
}

fn start_compositor(display_name: &str, port: u16) -> Child {
    Command::new(PathBuf::from("./target/release/waylandwebstream"))
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

fn start_cursor_client(display_name: &str) -> Child {
    Command::new(PathBuf::from("./target/release/wayland-cursor-client"))
        .env("WAYLAND_DISPLAY", display_name)
        .env("CURSOR_CLIENT_RUN_SECS", "120")
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("Failed to start wayland-cursor-client — did `cargo build --release` build the workspace?")
}
