// End-to-end visual smoke test for the wayland client + wws server
// pipeline.
//
// Drives the full stack and verifies the bytes the *user* would see
// on screen match the bytes the payload client drew:
//
//   payload-client  --(wl_shm)-->  wws-server compositor
//                          |
//                          v
//                       H.264 encode
//                          |
//                          v
//   <--- WebSocket /client --->  wws-client (the binary we ship)
//                          |
//                          v
//                       wl_shm blit
//                          |
//                          v
//   labwc (headless) --(wlr-screencopy)-->  grim --(PPM)---> this test
//
// Two checks, both of which must pass:
//
//   1. payload-client renders solid white  -> screenshot is mostly white
//   2. payload-client flips to solid black -> screenshot is mostly black
//
// If only (1) passes, the wws-client might be rendering the first
// frame correctly but failing to update when the payload changes --
// exactly the regression a screenshot-based check catches that
// wire-frame-counting does not.
//
// The test auto-spawns everything; no external setup beyond `labwc`
// + `grim` being on $PATH. The compositor runs headless via
// `WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1`; the wws-client
// connects to it as a Wayland client and gets a window on its
// virtual output. grim then captures that output via wlr-screencopy.
//
// The wws-client in the test runs *in-process* (under the same
// `cargo test` process) so the test can wait on its render counter
// directly. The display thread is forced to point at labwc's socket
// by overriding $WAYLAND_DISPLAY / $XDG_RUNTIME_DIR before spawning
// it; otherwise it would inherit the test process's environment
// (which on a developer workstation usually points at the real
// session, not the headless compositor we just spawned).
//
// Skip conditions (early return, not panic, so the rest of the test
// suite keeps running):
//
//   - `labwc` or `grim` not on $PATH -- headless compositor missing
//   - $WAYLAND_DISPLAY unset AND $WWS_FORCE_SMOKE_E2E unset --
//     most likely a CI runner with no display server at all
//
// The test does NOT skip when WAYLAND_DISPLAY is set on a workstation
// (because we override it for the display thread); only when the
// required binaries are absent.
//
// Requirements:
//
//   * A working headless compositor path. labwc with
//     `WLR_BACKENDS=headless` is the supported one; on machines
//     where the headless backend doesn't actually render client
//     content (some virtio-gpu setups show pure black regardless
//     of what the client committed), the screenshot assertion
//     fails even though the pipeline is healthy. The test still
//     exits with a clear message pointing at the saved PPM, so
//     the failure mode is obvious.
//   * The `payload-client` and `waylandwebstream` binaries in the
//     workspace `target/{debug,release}` dir. Override with
//     $WWS_PAYLOAD_BIN / $WWS_SERVER_BIN for out-of-tree builds.
//
// Run with:
//
//   cargo test -p wayland-test-client --test smoke_e2e -- --nocapture

use std::env;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use native_client::display::spawn_display_thread;
use native_client::transport::websocket::WsTransport;
use native_client::transport::{Frame, Transport};
use native_client::types::SignalingMessage;

/// How long to wait for the first rendered frame in the wws-client
/// before declaring the pipeline dead. The server takes ~500ms to
/// start broadcasting after a client connects; the H.264 decoder +
/// SHM blit adds a few hundred ms on top. 10s is generous.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(10);

/// How long to wait after signalling a payload color flip before
/// re-screenshotting. Includes: payload-client poll period (50ms),
/// compositor commit, capture-to-encode (≤ 1 keyframe interval =
/// ~2s default), WS transit, decode, render. 3s is enough on a
/// healthy local pipeline.
const PROPAGATION_DELAY: Duration = Duration::from_secs(3);

/// How long to wait for the second screenshot to *stabilize* on the
/// new color. The wws-client's renderer holds the *previous* buffer
/// until the compositor releases it; with 60fps capture, two frames
/// are enough. We give it a short window so a stale frame can't trick
/// the assertion.
const SECOND_SCREENSHOT_DELAY: Duration = Duration::from_millis(200);

/// Fraction of pixels that must match the expected color. The
/// rendered frame isn't *exactly* uniform after H.264 round-trip +
/// x264's `--tune zerolatency` quantization: macroblocks along edges
/// can pick up a few off-by-one values, and the compositor's scaling
/// at the window boundary can introduce a 1-2px band of sub-pixel
/// averaged values. 95% is comfortably above any of that noise.
const COLOR_MATCH_THRESHOLD: f64 = 0.95;

/// Path the payload client polls for color commands. The test writes
/// "white" or "black" here to drive the flip. Living in /tmp keeps it
/// out of the repo; the test cleans it up on drop.
const CONTROL_FILE: &str = "/tmp/wws-payload-color";

/// Temporary file names created by this test. All live under /tmp so
/// multiple parallel `cargo test` invocations on the same host would
/// step on each other -- but cargo test runs tests in a single
/// process by default, so we're fine.
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
    if env::var_os("WAYLAND_DISPLAY").is_none() {
        return true;
    }
    if which("labwc").is_none() {
        return true;
    }
    if which("grim").is_none() {
        return true;
    }
    false
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
    // Ensure the control file exists *before* the payload client
    // starts, so its very first poll sees a valid color. Default =
    // white; the test will write "black" later to flip.
    write_control("white")?;

let port = pick_free_port();
    let mut labwc = spawn_labwc().context("spawn labwc")?;
    // labwc creates its socket on startup; we wait briefly to make
    // sure it's there before anything connects to it.
    wait_for_wayland_socket(&labwc_socket_path(), Duration::from_secs(5))
        .with_context(|| format!("labwc wayland socket never appeared at {:?}", labwc_socket_path()))?;
    eprintln!("smoke_e2e: labwc ready");

    let mut server = spawn_server(port).context("spawn wws server")?;
    eprintln!("smoke_e2e: server spawned on 127.0.0.1:{port}");

    // Make sure the server's own socket is up before anything tries
    // to connect to it. The server-side SessionManager is lazy: it
    // doesn't spawn the payload client until the *first* /client
    // connection arrives, but the HTTP listener binds eagerly.
    wait_for_server("127.0.0.1", port, Duration::from_secs(15))
        .with_context(|| format!("wws server never bound its HTTP port on 127.0.0.1:{port}"))?;
    eprintln!("smoke_e2e: server ready");

    let result = std::panic::catch_unwind(|| run_visual_pipeline(port));

    // Cleanup happens unconditionally so a panic in the pipeline
    // doesn't leave labwc / the server running in the background.
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
    // Multi-threaded runtime: the decoder-pump task (recv loop on
    // the WebSocket) and the test's main loop both need to make
    // progress concurrently. A current_thread runtime would starve
    // one of them whenever the other holds the runtime. The wws
    // binary uses `#[tokio::main]` which is multi-thread by default;
    // mirror that here.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .build()
        .context("build tokio runtime")?;

    // Capture and override WAYLAND_DISPLAY + XDG_RUNTIME_DIR *before*
    // spawning the display thread (which inherits the env from this
    // process). The display thread calls `Connection::connect_to_env()`
    // which combines $WAYLAND_DISPLAY with $XDG_RUNTIME_DIR to find
    // the socket -- if either points at the user's actual session
    // rather than our headless labwc, the wws-client's window would
    // appear there, not in labwc.
    let previous_wayland_display = env::var_os("WAYLAND_DISPLAY");
    let previous_xdg_runtime_dir = env::var_os("XDG_RUNTIME_DIR");
    // SAFETY: labwc is up; the display thread we're about to spawn
    // will inherit this env. We restore before returning. (env::set_var
    // is unsafe on Rust 2024+ for thread-safety reasons; in a single-
    // threaded test context this is fine -- the runtime hasn't been
    // entered yet at this point.)
    env::set_var("WAYLAND_DISPLAY", "wayland-0");
    env::set_var("XDG_RUNTIME_DIR", LABWC_SOCKET_DIR);

    let result = rt.block_on(async {
        // Connect the wws-client to the server. The transport is
        // owned by the test; the actual display+decoder pipeline
        // runs on its own threads (spawn_display_thread + the
        // decoder thread inside the wws-client code).
        let server_url = format!("ws://127.0.0.1:{port}/client");
        let mut transport = WsTransport::connect(&server_url)
            .await
            .context("connect to /client")?;

        transport
            .send(
                &serde_json::to_string(&SignalingMessage::Ready)
                    .expect("Ready serializes"),
            )
            .await
            .context("send Ready")?;
        // Resize to match the wws-client's default window size; the
        // server reconfigures the payload client to fill its output
        // regardless, but this Resize is what unsticks the bitrate
        // probe on the server side (see main.rs).
        transport
            .send(
                &serde_json::to_string(&SignalingMessage::Resize {
                    width: 1280,
                    height: 720,
                })
                .expect("Resize serializes"),
            )
            .await
            .context("send Resize")?;

        // Spawn the wws-client's display thread (creates the
        // window in labwc, runs the SHM renderer).
        //
        // WAYLAND_DISPLAY was overridden before block_on to make
        // sure the spawned display thread connects to labwc, not
        // the user's actual session compositor (see outer comment).
        let (_packet_tx, packet_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(1);
        let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel(1);
        let display = spawn_display_thread((1280, 720), frame_rx)
            .context("spawn display thread")?;
        // Drop the packet sender's matching half to keep the channel
        // closed -- the decoder thread (if we spawn it) would only
        // need it, but we use the display thread's own render path
        // directly.
        drop(_packet_tx);
        drop(packet_rx);

        // Spawn a minimal decoder thread that just forwards H.264
        // packets to the SW decoder. We use the public spawn helper
        // so the test benefits from the same fixups the binary uses.
        // (Note: the binary wires the decoder->display channels
        // itself; here we do the same in-line.)
        let _decoder_handle = spawn_minimal_decoder(transport, frame_tx).await?;

        // === Phase 1: wait for first rendered frame ===
        let (_, first_elapsed) =
            wait_for_render_count(&display, 1, FIRST_FRAME_TIMEOUT).await?;
        eprintln!(
            "smoke_e2e: first rendered frame at t={first_elapsed:?} (render_count={})",
            display.render_counter.load(std::sync::atomic::Ordering::Relaxed),
        );

        // Brief settle: the SHM renderer's first blit happens before
        // the compositor has acknowledged + presented the buffer, so
        // a screencap taken immediately would see the prime buffer
        // (zeroed) instead of the actual content. Wait for at least
        // 2 vsyncs on a 60Hz compositor (~33ms each), and then add
        // a generous margin for slow CI machines: 500ms total.
        tokio::time::sleep(Duration::from_millis(500)).await;

        eprintln!(
            "smoke_e2e: after settle: render_count={} release_count={}",
            display.render_counter.load(std::sync::atomic::Ordering::Relaxed),
            display.release_counter.load(std::sync::atomic::Ordering::Relaxed),
        );

        // === Phase 2: screenshot the white frame ===
        grim_screenshot(SCREENSHOT_INITIAL).context("grim initial screenshot")?;
        let initial_match = ppm_white_fraction(SCREENSHOT_INITIAL)
            .context("parse initial PPM")?;
        eprintln!(
            "smoke_e2e: initial screenshot white fraction = {:.3}",
            initial_match
        );
        if initial_match < COLOR_MATCH_THRESHOLD {
            return Err(anyhow!(
                "initial screenshot: only {:.1}% of pixels are white \
                 (expected >= {:.0}%); the rendered frame isn't matching \
                 the payload. See {} for raw pixels.",
                initial_match * 100.0,
                COLOR_MATCH_THRESHOLD * 100.0,
                SCREENSHOT_INITIAL,
            ));
        }

        // === Phase 3: flip the payload to black ===
        write_control("black")?;
        eprintln!("smoke_e2e: signaled payload-client to switch to black");

        // Give the pipeline time to: poll the control file (≤50ms),
        // commit a new buffer, capture+encode a keyframe (≤1
        // keyframe interval ≈ 2s), decode + render. PROPAGATION_DELAY
        // is sized to be safely larger than the worst-case sum.
        tokio::time::sleep(PROPAGATION_DELAY).await;

        // Brief settle again so the renderer's *previous* buffer is
        // released and we're sampling the *current* one.
        tokio::time::sleep(SECOND_SCREENSHOT_DELAY).await;

        // === Phase 4: screenshot the black frame ===
        grim_screenshot(SCREENSHOT_FLIPPED).context("grim flipped screenshot")?;
        let flipped_black = ppm_black_fraction(SCREENSHOT_FLIPPED)
            .context("parse flipped PPM")?;
        eprintln!(
            "smoke_e2e: flipped screenshot black fraction = {:.3}",
            flipped_black
        );
        if flipped_black < COLOR_MATCH_THRESHOLD {
            return Err(anyhow!(
                "flipped screenshot: only {:.1}% of pixels are black \
                 (expected >= {:.0}%); the pipeline didn't propagate \
                 the payload change. See {} for raw pixels.",
                flipped_black * 100.0,
                COLOR_MATCH_THRESHOLD * 100.0,
                SCREENSHOT_FLIPPED,
            ));
        }

        eprintln!(
            "smoke_e2e: PASS -- initial {:.0}% white, flipped {:.0}% black",
            initial_match * 100.0,
            flipped_black * 100.0,
        );
        Ok::<(), anyhow::Error>(())
    });

    // Restore the original env so subsequent tests in the same
    // process see the same environment they would have without
    // this test having run.
    match previous_wayland_display {
        Some(v) => env::set_var("WAYLAND_DISPLAY", v),
        None => env::remove_var("WAYLAND_DISPLAY"),
    }
    match previous_xdg_runtime_dir {
        Some(v) => env::set_var("XDG_RUNTIME_DIR", v),
        None => env::remove_var("XDG_RUNTIME_DIR"),
    }
    result
}

/// Spawn a decoder thread that pulls H.264 packets from `transport`
/// and pushes decoded frames into `frame_tx`. Mirrors the
/// `spawn_decoder_thread` setup from the binary but inlines the
/// channel wiring so the test can own both ends.
async fn spawn_minimal_decoder(
    mut transport: WsTransport,
    frame_tx: std::sync::mpsc::SyncSender<native_client::decode::sw::DecodedFrame>,
) -> Result<tokio::task::JoinHandle<()>> {
    use native_client::decode::sw::spawn_decoder_thread;
    let (packet_tx, packet_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(4);
    let handle = tokio::spawn(async move {
        loop {
            // No timeout -- we want to keep pumping until the
            // transport closes. The test cleans up by killing the
            // server process, which closes the WebSocket, which
            // surfaces as a recv error and breaks the loop.
            match transport.recv().await {
                Ok(Frame::VideoFrame { data, .. }) => {
                    // Drop on full rather than block -- the render
                    // path uses try_send too (decoder->display is
                    // also bounded).
                    let _ = packet_tx.try_send(data);
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    });
    let (_join, _counter) = spawn_decoder_thread(packet_rx, frame_tx);
    Ok(handle)
}

/// Poll the display's render counter until it reaches `target`,
/// or until `timeout` elapses. Returns the `Instant` at which the
/// target was reached so the caller can compute its own elapsed
/// (the inline `start` would otherwise be lost across the await).
async fn wait_for_render_count(
    display: &native_client::display::DisplayHandle,
    target: u64,
    timeout: Duration,
) -> Result<(Instant, Duration)> {
    let start = Instant::now();
    loop {
        let now = display
            .render_counter
            .load(std::sync::atomic::Ordering::Relaxed);
        if now >= target {
            return Ok((start, start.elapsed()));
        }
        if start.elapsed() > timeout {
            return Err(anyhow!(
                "render_count never reached {} within {:?} (last seen: {})",
                target,
                timeout,
                now
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn grim_screenshot(path: &str) -> Result<()> {
    let grim = which("grim").ok_or_else(|| anyhow!("grim not on PATH"))?;
    let status = Command::new(grim)
        .args(["-t", "ppm", path])
        .status()
        .with_context(|| format!("failed to spawn grim at {}", path))?;
    if !status.success() {
        return Err(anyhow!("grim exited with {status}"));
    }
    Ok(())
}

/// Read a PPM and return the fraction of pixels that are "white"
/// (R > 240, G > 240, B > 240). PPM format: header "P6\n<W> <H>\n<max>\n"
/// followed by W*H*3 raw bytes (RGB, no padding). Returns 0.0..=1.0.
fn ppm_white_fraction(path: &str) -> Result<f64> {
    let fraction = ppm_color_fraction(path, |r, g, b| r > 240 && g > 240 && b > 240)?;
    Ok(fraction)
}

/// Read a PPM and return the fraction of pixels that are "black"
/// (R < 15, G < 15, B < 15). See `ppm_white_fraction` for the format.
fn ppm_black_fraction(path: &str) -> Result<f64> {
    let fraction = ppm_color_fraction(path, |r, g, b| r < 15 && g < 15 && b < 15)?;
    Ok(fraction)
}

fn ppm_color_fraction(path: &str, predicate: impl Fn(u8, u8, u8) -> bool) -> Result<f64> {
    let bytes = std::fs::read(path).with_context(|| format!("read PPM {path}"))?;
    // Find the end of the PPM header: third newline terminates
    // "P6\n<width> <height>\n<maxval>\n".
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
    let mut matched = 0usize;
    // Sample every 4th pixel -- PPM at 1280x720 is 2.7MB, scanning
    // every pixel would dominate the test runtime. Sampling at
    // stride 4 is ~170k samples, plenty of statistical power, and
    // brings the scan under 50ms.
    let stride = 4;
    let mut i = 0;
    while i < pixels.len() {
        if predicate(pixels[i], pixels[i + 1], pixels[i + 2]) {
            matched += 1;
        }
        i += 3 * stride;
    }
    let sampled = (total + stride - 1) / stride;
    Ok(matched as f64 / sampled as f64)
}

fn write_control(color: &str) -> Result<()> {
    std::fs::write(CONTROL_FILE, color)
        .with_context(|| format!("write control file {CONTROL_FILE}"))?;
    Ok(())
}

fn labwc_socket_path() -> PathBuf {
    PathBuf::from(LABWC_SOCKET_DIR).join("wayland-0")
}

fn spawn_labwc() -> Result<Child> {
    // Fresh, dedicated XDG_RUNTIME_DIR for the test compositor so
    // we don't share sockets with whatever the user has running.
    // Permissions: 0700, like a normal XDG_RUNTIME_DIR.
    let _ = std::fs::remove_dir_all(LABWC_SOCKET_DIR);
    std::fs::create_dir_all(LABWC_SOCKET_DIR)
        .context("create XDG_RUNTIME_DIR for labwc")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(LABWC_SOCKET_DIR, std::fs::Permissions::from_mode(0o700))
            .context("chmod 0700 labwc XDG_RUNTIME_DIR")?;
    }

    let labwc_bin = which("labwc").ok_or_else(|| anyhow!("labwc not on PATH"))?;
    let log = std::fs::File::create(LABWC_LOG).context("create labwc log file")?;
    let err_log = std::fs::File::create("/tmp/wws-smoke-e2e-labwc.err")
        .context("create labwc err log file")?;
    Command::new(labwc_bin)
        .arg("-V") // verbose: log configure/ping/xdg_surface errors
        // WLR_BACKENDS=headless: render to a virtual output with no
        // physical display attached, so the test runs on a headless
        // CI / dev box.
        // WLR_LIBINPUT_NO_DEVICES=1: skip libinput device discovery
        // -- otherwise labwc will try to grab /dev/input/event* on
        // the host, which may not be permitted inside a container.
        .env("WLR_BACKENDS", "headless")
        .env("WLR_LIBINPUT_NO_DEVICES", "1")
        .env_remove("WAYLAND_DISPLAY")
        .env("XDG_RUNTIME_DIR", LABWC_SOCKET_DIR)
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(err_log))
        .spawn()
        .context("spawn labwc")
}

fn spawn_server(port: u16) -> Result<Child> {
    let server_bin = locate_server_binary();
    let payload_bin = locate_payload_binary();

    // The server's --display-name controls which socket name *its*
    // compositor publishes for the payload client to attach to. We
    // use a name unique to this test so we don't collide with any
    // other wws-server the user might be running.
    //
    // The session manager (src/session.rs) is lazy: it spawns the
    // -- command only after the first /client connection arrives.
    // That's the wws-client, which we connect below. So the
    // payload-client launches ~immediately after we spawn the server.
    //
    // --no-audio keeps PipeWire out of the picture: the test host
    // doesn't necessarily have a working PipeWire daemon, and a
    // failed PipeWire init floods the server log without affecting
    // the H.264 path we're verifying.
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
            "--color",
            "white",
            "--control-file",
            CONTROL_FILE,
        ])
        .env("WAYLAND_DISPLAY", "wayland-0")
        // The wws server's compositor is internal (Smithay); the
        // env var only matters for child processes it spawns.
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .context("spawn wws server")
}

fn locate_server_binary() -> PathBuf {
    if let Ok(p) = env::var("WWS_SERVER_BIN") {
        return PathBuf::from(p);
    }
    let manifest_dir =
        env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set in cargo test");
    let workspace = std::path::Path::new(&manifest_dir)
        .parent()
        .expect("CARGO_MANIFEST_DIR has no parent (not in a workspace?)");
    for profile in &["debug", "release"] {
        let candidate = workspace.join("target").join(profile).join("waylandwebstream");
        if candidate.exists() {
            return candidate;
        }
    }
    panic!(
        "could not find `waylandwebstream` binary under {}",
        workspace.join("target").display()
    );
}

fn locate_payload_binary() -> PathBuf {
    if let Ok(p) = env::var("WWS_PAYLOAD_BIN") {
        return PathBuf::from(p);
    }
    let manifest_dir =
        env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set in cargo test");
    let workspace = std::path::Path::new(&manifest_dir)
        .parent()
        .expect("CARGO_MANIFEST_DIR has no parent (not in a workspace?)");
    for profile in &["debug", "release"] {
        let candidate = workspace.join("target").join(profile).join("payload-client");
        if candidate.exists() {
            return candidate;
        }
    }
    panic!(
        "could not find `payload-client` binary under {}; \
         run `cargo build -p wayland-test-client --bin payload-client` first",
        workspace.join("target").display()
    );
}

fn pick_free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind :0");
    let port = listener.local_addr().expect("local_addr").port();
    drop(listener);
    port
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

// ----- end of test -----