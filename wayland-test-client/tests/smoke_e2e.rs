// End-to-end visual smoke test: payload-client -> wws-server -> H.264 encode
// -> WebSocket -> native-client decoder -> wl_shm blit -> labwc (headless)
// -> grim screenshot -> pixel color check.
//
// Run: cargo test -p wayland-test-client --test smoke_e2e -- --nocapture
// Skip unless: labwc + grim on PATH AND (WAYLAND_DISPLAY set OR WWS_FORCE_SMOKE_E2E=1).

use std::env;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use native_client::display::{spawn_display_thread, RendererKind};
use native_client::transport::websocket::WsTransport;
use native_client::transport::{Frame, Transport};
use native_client::types::SignalingMessage;

const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(10);
// Sized to cover: payload poll (50ms) + compositor commit + one keyframe
// interval (~2s for x264 default) + WS transit + decode + render.
const PROPAGATION_DELAY: Duration = Duration::from_secs(3);
const COLOR_MATCH_THRESHOLD: f64 = 0.95;
const CONTROL_FILE: &str = "/tmp/wws-payload-color";
const LABWC_SOCKET_DIR: &str = "/tmp/wws-smoke-e2e-xdg";
const LABWC_LOG: &str = "/tmp/wws-smoke-e2e-labwc.log";
const SERVER_LOG: &str = "/tmp/wws-smoke-e2e-server.log";
const SERVER_ERR: &str = "/tmp/wws-smoke-e2e-server.err";
const SCREENSHOT_INITIAL: &str = "/tmp/wws-smoke-e2e-initial.ppm";
const SCREENSHOT_FLIPPED: &str = "/tmp/wws-smoke-e2e-flipped.ppm";

#[test]
fn end_to_end_visual_smoke() {
    if should_skip() {
        eprintln!(
            "smoke_e2e: skipping (need labwc + grim + WAYLAND_DISPLAY; \
             set WWS_FORCE_SMOKE_E2E=1 to override)"
        );
        return;
    }
    match run_test() {
        Ok(()) => {}
        Err(e) => panic!("smoke_e2e failed: {e:#}"),
    }
}

fn should_skip() -> bool {
    if env::var_os("WWS_FORCE_SMOKE_E2E").is_some() {
        return false;
    }
    env::var_os("WAYLAND_DISPLAY").is_none() || which("labwc").is_none() || which("grim").is_none()
}

fn which(bin: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    for dir in env::split_paths(&path) {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn run_test() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
    write_control("white")?;
    let port = pick_free_port();

    let mut labwc = spawn_labwc().context("spawn labwc")?;
    wait_for_wayland_socket(&labwc_socket_path(), Duration::from_secs(5))
        .with_context(|| format!("labwc socket never appeared at {:?}", labwc_socket_path()))?;
    eprintln!("smoke_e2e: labwc ready");

    let mut server = spawn_server(port).context("spawn wws server")?;
    eprintln!("smoke_e2e: server spawned on 127.0.0.1:{port}");
    wait_for_server("127.0.0.1", port, Duration::from_secs(15))
        .with_context(|| format!("wws server never bound on 127.0.0.1:{port}"))?;
    eprintln!("smoke_e2e: server ready");

    let result = std::panic::catch_unwind(|| run_visual_pipeline(port));

    let _ = server.kill();
    let _ = server.wait();
    let _ = labwc.kill();
    let _ = labwc.wait();
    let _ = std::fs::remove_file(CONTROL_FILE);
    let _ = std::fs::remove_dir_all(LABWC_SOCKET_DIR);

    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e),
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

fn run_visual_pipeline(port: u16) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .build()
        .context("build tokio runtime")?;

    // Override the env so the display thread connects to labwc, not the
    // user's own session. Restored before returning.
    let prev_wayland = env::var_os("WAYLAND_DISPLAY");
    let prev_xdg = env::var_os("XDG_RUNTIME_DIR");
    env::set_var("WAYLAND_DISPLAY", "wayland-0");
    env::set_var("XDG_RUNTIME_DIR", LABWC_SOCKET_DIR);

    let result = rt.block_on(async {
        let server_url = format!("ws://127.0.0.1:{port}/client");
        let mut transport = WsTransport::connect(&server_url)
            .await
            .context("connect to /client")?;
        transport
            .send(&serde_json::to_string(&SignalingMessage::Ready).expect("Ready serializes"))
            .await
            .context("send Ready")?;

        let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel(1);
        let display = spawn_display_thread((1280, 720), frame_rx, RendererKind::Shm)
            .context("spawn display thread")?;
        eprintln!("smoke_e2e: display thread spawned");

        // Wait for the display thread to receive the actual window size from
        // labwc (labwc may differ from the initial 1280×720 due to server-side
        // decorations). Using that size for Resize keeps the server in sync.
        tokio::time::timeout(Duration::from_secs(5), display.size_rx.clone().changed())
            .await
            .context("display thread never received initial configure")??;
        // Settle — labwc often sends a second configure shortly after the first.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let (actual_w, actual_h) = *display.size_rx.borrow();
        eprintln!("smoke_e2e: labwc configured window at {actual_w}x{actual_h}");
        transport
            .send(
                &serde_json::to_string(&SignalingMessage::Resize {
                    width: actual_w,
                    height: actual_h,
                })
                .expect("Resize serializes"),
            )
            .await
            .context("send Resize")?;

        let (_decoder, decoded_count) = spawn_decoder(transport, frame_tx).await?;
        eprintln!("smoke_e2e: decoder spawned");

        let elapsed =
            wait_for_render_count(&display, 1, FIRST_FRAME_TIMEOUT, &decoded_count).await?;
        eprintln!("smoke_e2e: first rendered frame at t={elapsed:?}");
        // Wait for at least a couple of vsyncs + margin so the screenshot
        // sees the actual frame content, not the prime buffer.
        tokio::time::sleep(Duration::from_millis(500)).await;
        eprintln!(
            "smoke_e2e: after settle: render_count={} release_count={}",
            display
                .render_counter
                .load(std::sync::atomic::Ordering::Relaxed),
            display
                .release_counter
                .load(std::sync::atomic::Ordering::Relaxed),
        );

        grim_screenshot(SCREENSHOT_INITIAL).context("grim initial screenshot")?;
        let initial_white =
            ppm_color_fraction(SCREENSHOT_INITIAL, |r, g, b| r > 240 && g > 240 && b > 240)?;
        eprintln!("smoke_e2e: initial screenshot white fraction = {initial_white:.3}");
        if initial_white < COLOR_MATCH_THRESHOLD {
            return Err(anyhow!(
                "initial screenshot: {:.1}% white (expected >= {:.0}%); see {SCREENSHOT_INITIAL}",
                initial_white * 100.0,
                COLOR_MATCH_THRESHOLD * 100.0,
            ));
        }

        write_control("black")?;
        eprintln!("smoke_e2e: signaled payload-client to switch to black");
        tokio::time::sleep(PROPAGATION_DELAY).await;

        grim_screenshot(SCREENSHOT_FLIPPED).context("grim flipped screenshot")?;
        let flipped_black =
            ppm_color_fraction(SCREENSHOT_FLIPPED, |r, g, b| r < 15 && g < 15 && b < 15)?;
        eprintln!("smoke_e2e: flipped screenshot black fraction = {flipped_black:.3}");
        if flipped_black < COLOR_MATCH_THRESHOLD {
            return Err(anyhow!(
                "flipped screenshot: {:.1}% black (expected >= {:.0}%); see {SCREENSHOT_FLIPPED}",
                flipped_black * 100.0,
                COLOR_MATCH_THRESHOLD * 100.0,
            ));
        }

        eprintln!(
            "smoke_e2e: PASS -- initial {:.0}% white, flipped {:.0}% black",
            initial_white * 100.0,
            flipped_black * 100.0,
        );
        Ok::<(), anyhow::Error>(())
    });

    match prev_wayland {
        Some(v) => env::set_var("WAYLAND_DISPLAY", v),
        None => env::remove_var("WAYLAND_DISPLAY"),
    }
    match prev_xdg {
        Some(v) => env::set_var("XDG_RUNTIME_DIR", v),
        None => env::remove_var("XDG_RUNTIME_DIR"),
    }
    result
}

/// Pull H.264 packets from `transport` and feed them to a decoder thread
/// that sends `DecodedFrame`s to `frame_tx`.
async fn spawn_decoder(
    mut transport: WsTransport,
    frame_tx: std::sync::mpsc::SyncSender<native_client::decode::sw::DecodedFrame>,
) -> Result<(
    tokio::task::JoinHandle<()>,
    std::sync::Arc<std::sync::atomic::AtomicU64>,
)> {
    use native_client::decode::sw::spawn_decoder_thread;
    let (packet_tx, packet_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(4);
    let ws_frame_count = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let ws_frame_count2 = ws_frame_count.clone();
    let handle = tokio::spawn(async move {
        let mut ctrl = 0u64;
        loop {
            match transport.recv().await {
                Ok(Frame::VideoFrame { data, .. }) => {
                    let n = ws_frame_count2.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if n < 3 {
                        eprintln!("smoke_e2e: ws video frame #{n} len={}", data.len());
                    }
                    let _ = packet_tx.try_send(data);
                }
                Ok(_) => {
                    ctrl += 1;
                    eprintln!("smoke_e2e: ws ctrl frame #{ctrl}");
                }
                Err(e) => {
                    eprintln!("smoke_e2e: ws recv error: {e:#}");
                    break;
                }
            }
        }
        eprintln!("smoke_e2e: ws recv loop exited");
    });
    let (_, decoded_count) = spawn_decoder_thread(packet_rx, frame_tx);
    Ok((handle, decoded_count))
}

async fn wait_for_render_count(
    display: &native_client::display::DisplayHandle,
    target: u64,
    timeout: Duration,
    decoded_count: &std::sync::Arc<std::sync::atomic::AtomicU64>,
) -> Result<Duration> {
    let start = Instant::now();
    let mut last_log = Instant::now();
    loop {
        let now = display
            .render_counter
            .load(std::sync::atomic::Ordering::Relaxed);
        if now >= target {
            return Ok(start.elapsed());
        }
        if start.elapsed() > timeout {
            let decoded = decoded_count.load(std::sync::atomic::Ordering::Relaxed);
            return Err(anyhow!(
                "render_count never reached {target} within {timeout:?} (render={now}, decoded={decoded})"
            ));
        }
        if last_log.elapsed() > Duration::from_secs(2) {
            let decoded = decoded_count.load(std::sync::atomic::Ordering::Relaxed);
            eprintln!(
                "smoke_e2e: waiting... render={now} decoded={decoded} elapsed={:?}",
                start.elapsed()
            );
            last_log = Instant::now();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn grim_screenshot(path: &str) -> Result<()> {
    let grim = which("grim").ok_or_else(|| anyhow!("grim not on PATH"))?;
    let status = Command::new(grim)
        .args(["-t", "ppm", path])
        .status()
        .with_context(|| format!("spawn grim for {path}"))?;
    if !status.success() {
        return Err(anyhow!("grim exited with {status}"));
    }
    Ok(())
}

fn ppm_color_fraction(path: &str, predicate: impl Fn(u8, u8, u8) -> bool) -> Result<f64> {
    let bytes = std::fs::read(path).with_context(|| format!("read PPM {path}"))?;
    // Skip "P6\n<W> <H>\n<maxval>\n" header (three newlines).
    let mut pos = 0usize;
    let mut newlines = 0;
    while newlines < 3 && pos < bytes.len() {
        if bytes[pos] == b'\n' {
            newlines += 1;
        }
        pos += 1;
    }
    if newlines < 3 {
        return Err(anyhow!("PPM {path} has malformed header"));
    }
    let pixels = &bytes[pos..];
    if pixels.len() % 3 != 0 {
        return Err(anyhow!("PPM {path} pixel data not a multiple of 3"));
    }
    let total = pixels.len() / 3;
    let stride = 4; // sample every 4th pixel; ~170k samples at 1280x720
    let mut matched = 0usize;
    let mut i = 0;
    while i < pixels.len() {
        if predicate(pixels[i], pixels[i + 1], pixels[i + 2]) {
            matched += 1;
        }
        i += 3 * stride;
    }
    let sampled = total.div_ceil(stride);
    Ok(matched as f64 / sampled as f64)
}

fn write_control(color: &str) -> Result<()> {
    std::fs::write(CONTROL_FILE, color).with_context(|| format!("write {CONTROL_FILE}"))
}

fn labwc_socket_path() -> PathBuf {
    PathBuf::from(LABWC_SOCKET_DIR).join("wayland-0")
}

fn spawn_labwc() -> Result<Child> {
    let _ = std::fs::remove_dir_all(LABWC_SOCKET_DIR);
    std::fs::create_dir_all(LABWC_SOCKET_DIR).context("create XDG_RUNTIME_DIR for labwc")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(LABWC_SOCKET_DIR, std::fs::Permissions::from_mode(0o700))
            .context("chmod 0700 labwc XDG_RUNTIME_DIR")?;
    }
    let labwc_bin = which("labwc").ok_or_else(|| anyhow!("labwc not on PATH"))?;
    let log = std::fs::File::create(LABWC_LOG).context("create labwc log")?;
    let err =
        std::fs::File::create("/tmp/wws-smoke-e2e-labwc.err").context("create labwc err log")?;
    Command::new(labwc_bin)
        .arg("-V")
        .env("WLR_BACKENDS", "headless")
        .env("WLR_LIBINPUT_NO_DEVICES", "1")
        .env_remove("WAYLAND_DISPLAY")
        .env("XDG_RUNTIME_DIR", LABWC_SOCKET_DIR)
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(err))
        .spawn()
        .context("spawn labwc")
}

fn spawn_server(port: u16) -> Result<Child> {
    let server_bin = locate_binary("waylandwebstream", "WWS_SERVER_BIN");
    let payload_bin = locate_binary("payload-client", "WWS_PAYLOAD_BIN");
    let stdout = std::fs::File::create(SERVER_LOG).context("create server log")?;
    let stderr = std::fs::File::create(SERVER_ERR).context("create server err log")?;
    Command::new(server_bin)
        .args([
            "--display-name",
            "wayland-wws-smoke-e2e",
            "--port",
            &port.to_string(),
            "--listen-addr",
            "127.0.0.1",
            "--encoder",
            "x264",
            "--no-audio",
            "--",
            payload_bin.to_str().unwrap(),
            "--control-file",
            CONTROL_FILE,
        ])
        .env("WAYLAND_DISPLAY", "wayland-0")
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .context("spawn wws server")
}

fn locate_binary(name: &str, env_override: &str) -> PathBuf {
    if let Ok(p) = env::var(env_override) {
        return PathBuf::from(p);
    }
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
    let workspace = std::path::Path::new(&manifest_dir)
        .parent()
        .expect("CARGO_MANIFEST_DIR has no parent");
    for profile in &["debug", "release"] {
        let candidate = workspace.join("target").join(profile).join(name);
        if candidate.exists() {
            return candidate;
        }
    }
    panic!(
        "could not find `{name}` under {}",
        workspace.join("target").display()
    );
}

fn pick_free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind :0");
    listener.local_addr().expect("local_addr").port()
}

fn wait_for_server(host: &str, port: u16, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if std::net::TcpStream::connect((host, port)).is_ok() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(anyhow!(
        "no TCP listener on {host}:{port} within {timeout:?}"
    ))
}

fn wait_for_wayland_socket(path: &std::path::Path, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(anyhow!(
        "Wayland socket at {} did not appear within {timeout:?}",
        path.display()
    ))
}
