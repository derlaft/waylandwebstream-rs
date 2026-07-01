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

/// Regression test: a key held down when the browser window loses OS-level
/// focus (the same thing that happens during a real Alt+Tab, since the OS
/// intercepts the combo and switches focus before the page ever sees a
/// keyup) must still be released compositor-side -- otherwise it's held
/// forever and silently modifies every keystroke after it. See
/// `releaseAllKeys` in web/src/lib/input.ts.
#[test]
fn test_keyboard_releases_held_keys_on_focus_loss() {
    println!("Building compositor and test clients...");
    let build_status = Command::new("cargo")
        .args(["build", "--release", "--workspace"])
        .status()
        .expect("Failed to build workspace");
    assert!(build_status.success(), "Build failed");

    let display_name = common::unique_display_name("wayland-keyboard-focus-test");
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

    println!("Running browser-driven keyboard focus-loss capture...");
    let output = Command::new("node")
        .arg("tests/keyboard_focus_loss_capture.js")
        .arg(port.to_string())
        .stderr(Stdio::inherit())
        .output()
        .expect("Failed to run keyboard focus-loss capture script - ensure Node.js and tests/node_modules are installed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    print!("{stdout}");

    assert!(
        output.status.success(),
        "Keyboard focus-loss capture failed"
    );
    assert!(
        stdout.lines().any(|line| line == "RESULT ok"),
        "Capture script did not report success"
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
