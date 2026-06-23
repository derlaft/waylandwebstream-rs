use std::process::{Command, Child, Stdio};
use std::time::Duration;
use std::thread;
use std::path::PathBuf;

/// Integration test that validates the entire compositor pipeline:
/// 1. Start compositor
/// 2. Launch a Wayland client
/// 3. Connect via the WebSocket/WebCodecs stream and capture frames
/// 4. Validate rendering works correctly
#[test]
fn test_compositor_pipeline() {
    // Build the compositor first
    println!("Building compositor...");
    let build_status = Command::new("cargo")
        .args(&["build", "--release", "--workspace"])
        .status()
        .expect("Failed to build compositor");
    
    assert!(build_status.success(), "Compositor build failed");
    
    // Start the compositor
    println!("Starting compositor...");
    let mut compositor = start_compositor();
    
    // Use a closure to ensure cleanup happens even on panic
    let test_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // Give it time to initialize
        thread::sleep(Duration::from_secs(3));
        
        // Check if compositor is still running
        match compositor.try_wait() {
            Ok(Some(status)) => panic!("Compositor exited early with status: {}", status),
            Ok(None) => println!("Compositor is running"),
            Err(e) => panic!("Error checking compositor status: {}", e),
        }
        
        // Launch a simple Wayland client
        println!("Launching test Wayland client...");
        let mut client = start_test_client();
        
        // Give the client time to connect and render
        thread::sleep(Duration::from_secs(2));
        
        // Connect a stream client and capture a frame
        println!("Connecting stream client...");
        let screenshot_path = capture_stream_frame();
        
        // Validate the screenshot
        println!("Validating screenshot...");
        validate_screenshot(&screenshot_path);
        
        // Cleanup client
        let _ = client.kill();
        
        println!("Test passed!");
    }));
    
    // Always cleanup compositor
    println!("Cleaning up compositor...");
    let _ = compositor.kill();
    let _ = compositor.wait();
    
    // Re-panic if the test failed
    if let Err(e) = test_result {
        std::panic::resume_unwind(e);
    }
}

fn start_compositor() -> Child {
    let binary_path = PathBuf::from("./target/release/waylandwebstream");
    
    // Use a custom display name for testing
    Command::new(binary_path)
        .arg("--display-name")
        .arg("wayland-test-0")
        .env("RUST_LOG", "info")
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("Failed to start compositor")
}

fn start_test_client() -> Child {
    // Try to use our built-in test client first, fall back to weston-terminal
    let test_client = PathBuf::from("./target/release/wayland-test-client");
    
    if test_client.exists() {
        Command::new(test_client)
            .env("WAYLAND_DISPLAY", "wayland-test-0")
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("Failed to start test client")
    } else {
        // Fall back to weston-terminal if available
        Command::new("weston-terminal")
            .env("WAYLAND_DISPLAY", "wayland-test-0")
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("Failed to start weston-terminal - please install weston or build wayland-test-client")
    }
}

fn capture_stream_frame() -> PathBuf {
    // Use Node.js + Puppeteer to connect and capture a frame
    let screenshot_path = PathBuf::from("/tmp/compositor_test_screenshot.png");

    let status = Command::new("node")
        .arg("tests/stream_capture.js")
        .arg(&screenshot_path)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .expect("Failed to run stream capture script - ensure Node.js and dependencies are installed");

    assert!(status.success(), "Stream capture failed");
    assert!(screenshot_path.exists(), "Screenshot was not created");

    screenshot_path
}

fn validate_screenshot(screenshot_path: &PathBuf) {
    // Basic validation: check that the image is not blank and has expected dimensions
    let status = Command::new("node")
        .arg("tests/validate_screenshot.js")
        .arg(screenshot_path)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .expect("Failed to run screenshot validation");
    
    assert!(status.success(), "Screenshot validation failed");
}

#[test]
fn test_compositor_startup() {
    println!("Testing compositor can start and bind socket...");
    
    let mut compositor = start_compositor();
    
    let test_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        thread::sleep(Duration::from_secs(2));
        
        // Check if compositor is running
        match compositor.try_wait() {
            Ok(Some(status)) => panic!("Compositor exited with status: {}", status),
            Ok(None) => println!("Compositor started successfully"),
            Err(e) => panic!("Error checking compositor: {}", e),
        }
        
        // Check if the Wayland socket was created
        let socket_path = format!("/run/user/{}/wayland-test-0", users::get_current_uid());
        let socket_exists = std::path::Path::new(&socket_path).exists();
        
        assert!(socket_exists, "Wayland socket was not created at {}", socket_path);
        println!("Test passed!");
    }));
    
    // Always cleanup
    let _ = compositor.kill();
    let _ = compositor.wait();
    
    // Re-panic if the test failed
    if let Err(e) = test_result {
        std::panic::resume_unwind(e);
    }
}
