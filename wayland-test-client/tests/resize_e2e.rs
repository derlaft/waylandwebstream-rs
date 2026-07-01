// End-to-end resize regression test.
//
// The pipeline under test:
//
//   payload-client (white surface) -> wws-server -> H.264 encode
//     -> WebSocket -> native-client decoder -> SHM renderer -> labwc (headless)
//     -> grim screenshot -> pixel color check.
//
// The resize is driven by `wlr-randr --custom-mode`, which changes the labwc
// headless output resolution.  Labwc propagates it as an xdg-toplevel configure
// event that the display thread handles exactly as a real user resize would.
//
// The initial `--display-name` socket is chosen to be NON-STANDARD (850x500 is
// not a multiple of 16) to exercise the old ÷16 rounding bug where the server
// would encode at 848x496 instead of 850x500, causing a permanent frame-size
// mismatch.  With the fix (÷2 even rounding), 850x500 encodes correctly.
//
// Run (requires labwc + grim + wlr-randr on PATH and WAYLAND_DISPLAY set, or
// set WWS_FORCE_RESIZE_E2E=1):
//
//   cargo test -p wayland-test-client --test resize_e2e -- --nocapture

use std::env;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use native_client::display::{spawn_display_thread, RendererKind};
use native_client::transport::websocket::WsTransport;
use native_client::transport::{Frame, Transport};
use native_client::types::SignalingMessage;

// ── Constants ──────────────────────────────────────────────────────────────────

const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(12);
const POST_RESIZE_FRAME_TIMEOUT: Duration = Duration::from_secs(15);
const COLOR_THRESHOLD: f64 = 0.90;
const CONTROL_FILE: &str = "/tmp/wws-resize-e2e-color";
const LABWC_SOCKET_DIR: &str = "/tmp/wws-resize-e2e-xdg";
const LABWC_CFG_DIR: &str = "/tmp/wws-resize-e2e-labwc-cfg";
const LABWC_ERR_LOG: &str = "/tmp/wws-resize-e2e-labwc.err";
const SERVER_LOG: &str = "/tmp/wws-resize-e2e-server.log";
const SERVER_ERR: &str = "/tmp/wws-resize-e2e-server.err";
const SCREENSHOT_INITIAL: &str = "/tmp/wws-resize-e2e-initial.ppm";
const SCREENSHOT_RESIZED: &str = "/tmp/wws-resize-e2e-resized.ppm";

// labwc headless creates "HEADLESS-1" by default.
const HEADLESS_OUTPUT: &str = "HEADLESS-1";
// Initial output size (labwc headless default).
const INITIAL_OUTPUT_W: u32 = 1280;
const INITIAL_OUTPUT_H: u32 = 720;
// Resize target — deliberately NOT a multiple of 16 to exercise the old ÷16
// rounding bug.
const RESIZE_OUTPUT_W: u32 = 850;
const RESIZE_OUTPUT_H: u32 = 500;

// ── Test entry point ───────────────────────────────────────────────────────────

#[test]
fn resize_preserves_video_no_black_screen() {
    if should_skip() {
        eprintln!(
            "resize_e2e: skipping — need labwc + grim + wlr-randr on PATH and WAYLAND_DISPLAY \
             set. Override with WWS_FORCE_RESIZE_E2E=1."
        );
        return;
    }
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
    match run_test() {
        Ok(()) => {}
        Err(e) => panic!("resize_e2e FAILED: {e:#}"),
    }
}

fn should_skip() -> bool {
    if env::var_os("WWS_FORCE_RESIZE_E2E").is_some() {
        return false;
    }
    env::var_os("WAYLAND_DISPLAY").is_none()
        || which("labwc").is_none()
        || which("grim").is_none()
        || which("wlr-randr").is_none()
}

// ── Orchestration ──────────────────────────────────────────────────────────────

fn run_test() -> Result<()> {
    std::fs::write(CONTROL_FILE, "white").context("write control file")?;
    let port = pick_free_port();

    create_labwc_config().context("create labwc config")?;
    let mut labwc = spawn_labwc().context("spawn labwc")?;
    wait_for_wayland_socket(&labwc_socket_path(), Duration::from_secs(5))
        .with_context(|| format!("labwc socket never appeared at {:?}", labwc_socket_path()))?;
    eprintln!("resize_e2e: labwc ready");

    let mut server = spawn_server(port).context("spawn wws server")?;
    wait_for_server("127.0.0.1", port, Duration::from_secs(15))
        .with_context(|| format!("wws server never bound on 127.0.0.1:{port}"))?;
    eprintln!("resize_e2e: server ready on :{port}");

    let result = std::panic::catch_unwind(|| run_visual_pipeline(port));

    let _ = server.kill();
    let _ = server.wait();
    let _ = labwc.kill();
    let _ = labwc.wait();
    let _ = std::fs::remove_file(CONTROL_FILE);
    let _ = std::fs::remove_dir_all(LABWC_SOCKET_DIR);
    let _ = std::fs::remove_dir_all(LABWC_CFG_DIR);

    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e),
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

// ── Pipeline ───────────────────────────────────────────────────────────────────

fn run_visual_pipeline(port: u16) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .build()
        .context("build tokio runtime")?;

    let prev_display = env::var_os("WAYLAND_DISPLAY");
    let prev_xdg = env::var_os("XDG_RUNTIME_DIR");
    env::set_var("WAYLAND_DISPLAY", "wayland-0");
    env::set_var("XDG_RUNTIME_DIR", LABWC_SOCKET_DIR);

    let result = rt.block_on(run_pipeline_async(port));

    match prev_display {
        Some(v) => env::set_var("WAYLAND_DISPLAY", v),
        None => env::remove_var("WAYLAND_DISPLAY"),
    }
    match prev_xdg {
        Some(v) => env::set_var("XDG_RUNTIME_DIR", v),
        None => env::remove_var("XDG_RUNTIME_DIR"),
    }
    result
}

async fn run_pipeline_async(port: u16) -> Result<()> {
    let url = format!("ws://127.0.0.1:{port}/client");
    let transport = WsTransport::connect(&url)
        .await
        .context("connect to /client")?;

    // Split send/recv so we can send Resize mid-test without fighting the
    // decoder task for ownership.
    let (send_tx, send_rx) = tokio::sync::mpsc::channel::<String>(8);
    let (packet_tx, packet_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(4);
    let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel(1);

    tokio::spawn(transport_relay(transport, send_rx, packet_tx));

    let mut display = spawn_display_thread(
        (INITIAL_OUTPUT_W, INITIAL_OUTPUT_H),
        frame_rx,
        RendererKind::Shm,
    )
    .context("spawn display thread")?;
    let (_, decoded_count) = native_client::decode::sw::spawn_decoder_thread(packet_rx, frame_tx);

    // Handshake: send Ready, then wait for the display thread to receive the
    // actual window size from labwc before sending Resize.
    // IMPORTANT: call changed() directly (not on a clone) so the receiver's
    // internal version is updated.  If we call clone().changed(), the clone
    // inherits the stale pre-configure version and the next changed() call
    // resolves immediately with the same value instead of waiting for the
    // real wlr-randr resize configure.
    send(&send_tx, &SignalingMessage::Ready).await?;
    tokio::time::timeout(Duration::from_secs(5), display.size_rx.changed())
        .await
        .context("display thread never received initial configure from labwc")??;
    // Settle — labwc may send a second configure after the initial one.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let (initial_w, initial_h) = *display.size_rx.borrow();
    eprintln!("resize_e2e: labwc initial configure: {initial_w}×{initial_h}");
    send(
        &send_tx,
        &SignalingMessage::Resize {
            width: initial_w,
            height: initial_h,
        },
    )
    .await?;

    // ── Phase 1: verify initial rendering ─────────────────────────────────────
    eprintln!("resize_e2e: waiting for first rendered frame…");
    let elapsed = wait_for_render_count(&display, 1, FIRST_FRAME_TIMEOUT, &decoded_count)
        .await
        .context("first frame never rendered")?;
    eprintln!("resize_e2e: first frame at {elapsed:.1?}");
    tokio::time::sleep(Duration::from_millis(500)).await;

    grim_screenshot(SCREENSHOT_INITIAL)?;
    let initial_white = ppm_white_fraction(SCREENSHOT_INITIAL)?;
    eprintln!(
        "resize_e2e: initial screenshot — {:.1}% white",
        initial_white * 100.0
    );
    if initial_white < COLOR_THRESHOLD {
        return Err(anyhow!(
            "initial screenshot only {:.1}% white (expected ≥{:.0}%). \
             Pipeline broken before resize. See {SCREENSHOT_INITIAL}",
            initial_white * 100.0,
            COLOR_THRESHOLD * 100.0,
        ));
    }

    // ── Phase 2: resize via wlr-randr ─────────────────────────────────────────
    let pre_resize_count = display.render_counter.load(Ordering::Relaxed);
    eprintln!(
        "resize_e2e: changing output to {RESIZE_OUTPUT_W}×{RESIZE_OUTPUT_H} via wlr-randr \
         (render_count before={pre_resize_count})"
    );

    // Change the headless output resolution.  Labwc sends an xdg_toplevel
    // configure event to the window at the new output size, which the display
    // thread handles as a normal compositor-driven resize.
    wlr_randr_set_mode(RESIZE_OUTPUT_W, RESIZE_OUTPUT_H)?;

    // Wait for the display thread to receive and process the resize configure.
    // Direct changed() (not on a clone) so the version advances past this
    // point; Phase 3 wait_for_render_count does not call changed() again.
    tokio::time::timeout(Duration::from_secs(5), display.size_rx.changed())
        .await
        .context("size_rx never changed after wlr-randr resize")??;
    // Settle in case labwc sends a follow-up configure.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (resized_w, resized_h) = *display.size_rx.borrow();
    eprintln!("resize_e2e: display thread configured at {resized_w}×{resized_h}");

    // Tell the server to encode at the new size.
    send(
        &send_tx,
        &SignalingMessage::Resize {
            width: resized_w,
            height: resized_h,
        },
    )
    .await?;
    eprintln!(
        "resize_e2e: Resize({resized_w},{resized_h}) sent to server; waiting for new frames…"
    );

    // ── Phase 3: confirm pipeline works at new size (still white) ─────────────
    wait_for_render_count(
        &display,
        pre_resize_count + 2,
        POST_RESIZE_FRAME_TIMEOUT,
        &decoded_count,
    )
    .await
    .map_err(|e| {
        let after = display.render_counter.load(Ordering::Relaxed);
        let decoded = decoded_count.load(Ordering::Relaxed);
        anyhow!(
            "{e} (render_count before={pre_resize_count} after={after} decoded={decoded}). \
             No frames after resize: renderer not accepting frames at {resized_w}×{resized_h}. \
             Check {SERVER_LOG} / {SERVER_ERR}"
        )
    })?;
    eprintln!(
        "resize_e2e: {} frame(s) rendered at new size",
        display.render_counter.load(Ordering::Relaxed) - pre_resize_count,
    );

    // ── Phase 4: switch payload to black — catch stale/wrong-size frames ──────
    // Writing "black" makes payload-client commit a black buffer at the
    // already-resized compositor size.  If the server never received our Resize
    // (or is encoding at the wrong dimensions), the renderer will reject
    // old-size frames and the screenshot will NOT be mostly dark → test fails.
    let post_resize_count = display.render_counter.load(Ordering::Relaxed);
    std::fs::write(CONTROL_FILE, "black").context("switch payload to black")?;
    eprintln!("resize_e2e: payload switched to black; waiting for dark frames…");

    wait_for_render_count(
        &display,
        post_resize_count + 3,
        POST_RESIZE_FRAME_TIMEOUT,
        &decoded_count,
    )
    .await
    .map_err(|e| {
        let after = display.render_counter.load(Ordering::Relaxed);
        let decoded = decoded_count.load(Ordering::Relaxed);
        anyhow!(
            "{e} (render={after} decoded={decoded}). \
             Dark frames never arrived after payload color switch. \
             Check {SERVER_LOG} / {SERVER_ERR}"
        )
    })?;
    // Extra settle time — pipeline needs to flush the color-switch through
    // encode → transport → decode → render before the screenshot.
    tokio::time::sleep(Duration::from_millis(500)).await;

    grim_screenshot(SCREENSHOT_RESIZED)?;
    let post_dark = ppm_black_fraction(SCREENSHOT_RESIZED)?;
    eprintln!(
        "resize_e2e: post-switch screenshot — {:.1}% dark",
        post_dark * 100.0
    );
    if post_dark < COLOR_THRESHOLD {
        return Err(anyhow!(
            "post-resize screenshot only {:.1}% dark after switching payload to black \
             (expected ≥{:.0}%). Stale or wrong-size buffer still displayed. \
             See {SCREENSHOT_RESIZED}",
            post_dark * 100.0,
            COLOR_THRESHOLD * 100.0,
        ));
    }

    // ── Phase 5: verify server logged the resize at the correct dimensions ─────
    // The server's tracing-subscriber writes to stdout → SERVER_LOG.
    let resize_entry = format!("Resize complete: {resized_w}x{resized_h}");
    let server_log = std::fs::read_to_string(SERVER_LOG).unwrap_or_default();
    if !server_log.contains(&resize_entry) {
        let tail: Vec<&str> = server_log.lines().rev().take(20).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        return Err(anyhow!(
            "server log ({SERVER_LOG}) never contained \"{resize_entry}\".\n\
             Server did not process the resize to {resized_w}×{resized_h}.\n\
             Server log tail:\n{}",
            tail.join("\n")
        ));
    }
    eprintln!("resize_e2e: server confirmed resize to {resized_w}×{resized_h}");

    eprintln!(
        "resize_e2e: PASS — initial {:.0}% white, post-resize {:.0}% dark, \
         server confirmed resize to {resized_w}×{resized_h}",
        initial_white * 100.0,
        post_dark * 100.0,
    );
    Ok(())
}

// ── Helpers ────────────────────────────────────────────────────────────────────

async fn transport_relay(
    mut transport: WsTransport,
    mut send_rx: tokio::sync::mpsc::Receiver<String>,
    packet_tx: std::sync::mpsc::SyncSender<Vec<u8>>,
) {
    loop {
        tokio::select! {
            msg = send_rx.recv() => {
                match msg {
                    Some(m) => { let _ = transport.send(&m).await; }
                    None => return,
                }
            }
            frame = transport.recv() => {
                match frame {
                    Ok(Frame::VideoFrame { data, .. }) => { let _ = packet_tx.try_send(data); }
                    Ok(_) => {}
                    Err(_) => return,
                }
            }
        }
    }
}

async fn send(tx: &tokio::sync::mpsc::Sender<String>, msg: &SignalingMessage) -> Result<()> {
    tx.send(serde_json::to_string(msg).expect("serialize"))
        .await
        .context("send to transport relay")
}

async fn wait_for_render_count(
    display: &native_client::display::DisplayHandle,
    target: u64,
    timeout: Duration,
    decoded_count: &std::sync::Arc<std::sync::atomic::AtomicU64>,
) -> Result<Duration> {
    let start = Instant::now();
    let mut last_log = start;
    loop {
        let now = display.render_counter.load(Ordering::Relaxed);
        if now >= target {
            return Ok(start.elapsed());
        }
        if start.elapsed() > timeout {
            return Err(anyhow!(
                "render_count never reached {target} within {timeout:.0?} (stuck at {now})"
            ));
        }
        if last_log.elapsed() > Duration::from_secs(2) {
            let decoded = decoded_count.load(Ordering::Relaxed);
            eprintln!(
                "resize_e2e: waiting for render_count={target} (render={now} decoded={decoded}) elapsed={:.1?}",
                start.elapsed()
            );
            last_log = Instant::now();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn grim_screenshot(path: &str) -> Result<()> {
    let status = Command::new("grim")
        .args(["-t", "ppm", path])
        .env("WAYLAND_DISPLAY", "wayland-0")
        .env("XDG_RUNTIME_DIR", LABWC_SOCKET_DIR)
        .status()
        .with_context(|| format!("spawn grim → {path}"))?;
    if !status.success() {
        return Err(anyhow!("grim exited with {status} for {path}"));
    }
    Ok(())
}

fn wlr_randr_set_mode(w: u32, h: u32) -> Result<()> {
    let status = Command::new("wlr-randr")
        .args([
            "--output",
            HEADLESS_OUTPUT,
            "--custom-mode",
            &format!("{w}x{h}@60Hz"),
        ])
        .env("WAYLAND_DISPLAY", "wayland-0")
        .env("XDG_RUNTIME_DIR", LABWC_SOCKET_DIR)
        .status()
        .context("spawn wlr-randr")?;
    if !status.success() {
        return Err(anyhow!("wlr-randr exited with {status}"));
    }
    Ok(())
}

fn ppm_white_fraction(path: &str) -> Result<f64> {
    ppm_color_fraction(path, |r, g, b| r > 240 && g > 240 && b > 240)
}

fn ppm_black_fraction(path: &str) -> Result<f64> {
    ppm_color_fraction(path, |r, g, b| r < 15 && g < 15 && b < 15)
}

fn ppm_color_fraction(path: &str, pred: impl Fn(u8, u8, u8) -> bool) -> Result<f64> {
    let bytes = std::fs::read(path).with_context(|| format!("read {path}"))?;
    let mut pos = 0;
    let mut nl = 0;
    while nl < 3 && pos < bytes.len() {
        if bytes[pos] == b'\n' {
            nl += 1;
        }
        pos += 1;
    }
    if nl < 3 {
        return Err(anyhow!("{path}: malformed PPM header"));
    }
    let pixels = &bytes[pos..];
    if pixels.len() % 3 != 0 {
        return Err(anyhow!("{path}: pixel data length not divisible by 3"));
    }
    let stride = 4usize;
    let mut matched = 0usize;
    let mut sampled = 0usize;
    let mut i = 0;
    while i + 2 < pixels.len() {
        if pred(pixels[i], pixels[i + 1], pixels[i + 2]) {
            matched += 1;
        }
        sampled += 1;
        i += 3 * stride;
    }
    if sampled == 0 {
        return Ok(0.0);
    }
    Ok(matched as f64 / sampled as f64)
}

// ── Process management ─────────────────────────────────────────────────────────

fn create_labwc_config() -> Result<()> {
    let _ = std::fs::remove_dir_all(LABWC_CFG_DIR);
    std::fs::create_dir_all(LABWC_CFG_DIR).context("create labwc cfg dir")?;
    // Disable decorations + auto-maximize every window so it fills the full
    // output.  This has two benefits:
    //   1. The window size equals the output size (no decoration subtraction).
    //   2. When wlr-randr changes the output resolution, labwc reconfigures
    //      the maximized window at the new size, giving the display thread a
    //      real compositor-driven resize event.  Floating (non-maximized)
    //      windows are NOT resized when the output changes.
    std::fs::write(
        format!("{LABWC_CFG_DIR}/rc.xml"),
        r#"<?xml version="1.0"?>
<labwc_config>
  <windowRules>
    <windowRule identifier="*" serverDecoration="no">
      <action name="Maximize" />
    </windowRule>
  </windowRules>
</labwc_config>
"#,
    )
    .context("write labwc rc.xml")
}

fn spawn_labwc() -> Result<Child> {
    let _ = std::fs::remove_dir_all(LABWC_SOCKET_DIR);
    std::fs::create_dir_all(LABWC_SOCKET_DIR).context("mkdir labwc XDG_RUNTIME_DIR")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(LABWC_SOCKET_DIR, std::fs::Permissions::from_mode(0o700))
            .context("chmod labwc socket dir")?;
    }
    let err = std::fs::File::create(LABWC_ERR_LOG).context("create labwc err log")?;
    Command::new("labwc")
        .args(["-V", "-C", LABWC_CFG_DIR])
        .env("WLR_BACKENDS", "headless")
        .env("WLR_LIBINPUT_NO_DEVICES", "1")
        .env_remove("WAYLAND_DISPLAY")
        .env("XDG_RUNTIME_DIR", LABWC_SOCKET_DIR)
        .stdout(Stdio::null())
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
            "wayland-wws-resize-e2e",
            "--port",
            &port.to_string(),
            "--listen-addr",
            "127.0.0.1",
            "--encoder",
            "x264",
            "--no-audio",
            "--",
            payload_bin.to_str().unwrap(),
            "--color",
            "white",
            "--control-file",
            CONTROL_FILE,
        ])
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .context("spawn wws server")
}

fn locate_binary(name: &str, env_var: &str) -> PathBuf {
    if let Ok(p) = env::var(env_var) {
        return PathBuf::from(p);
    }
    let manifest = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let ws = std::path::Path::new(&manifest)
        .parent()
        .expect("workspace root");
    for profile in &["debug", "release"] {
        let c = ws.join("target").join(profile).join(name);
        if c.exists() {
            return c;
        }
    }
    panic!("could not find `{name}` in target/debug or target/release");
}

fn pick_free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind :0")
        .local_addr()
        .expect("local_addr")
        .port()
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
        "no TCP listener on {host}:{port} within {timeout:.0?}"
    ))
}

fn labwc_socket_path() -> PathBuf {
    PathBuf::from(LABWC_SOCKET_DIR).join("wayland-0")
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
        "Wayland socket {:?} never appeared within {timeout:.0?}",
        path
    ))
}

fn which(bin: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    for dir in env::split_paths(&path) {
        let c = dir.join(bin);
        if c.is_file() {
            return Some(c);
        }
    }
    None
}
