// End-to-end smoke test for the native client.
//
// Verifies the full pipeline from wire to surface:
//
//   transport -> decoder -> swscale -> ShmRenderer -> wl_surface::commit
//
// The test auto-spawns a `waylandwebstream` server on a free port
// (no external setup needed) and runs against it. It is skipped
// when `WAYLAND_DISPLAY` is unset (i.e. there's no Wayland
// compositor for the native client to attach to) -- a workstation
// running this test must have a Wayland session.
//
// Counters distinguish failure points:
//
//   wire_frames      H.264 packets seen on the wire
//   decoded_frames   frames the SW H.264 decoder produced
//   rendered         frames committed to wl_surface (ShmRenderer::render Ok)
//   released         wl_buffer::Release events dispatched
//   wire_control     JSON control messages from the server
//
//   decoded == 0 && wire_frames > 0  -> ffmpeg issue / wrong format
//   decoded > 0 && rendered == 0     -> renderer stuck on wl_buffer::Release
//   wire_control < 2                 -> /client endpoint broken
//   wire_frames == 0                 -> server not broadcasting
//
// The server takes ~500ms to start broadcasting frames after spawn
// (compositor init + encoder init). To avoid running the test
// against a still-starting server, the test waits until at least
// one decoded frame is produced before timing the run window -- so
// the run window is guaranteed to overlap with steady-state frame
// production rather than startup latency.
//
// Run with:
//
//   WAYLAND_DISPLAY=wayland-0 \
//   cargo test -p native-client --test smoke_e2e -- --nocapture

use std::io::Write;
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use native_client::decode::sw::{spawn_decoder_thread, DecodedFrame};
use native_client::display::spawn_display_thread;
use native_client::transport::websocket::WsTransport;
use native_client::transport::{Frame, Transport};
use native_client::types::SignalingMessage;

/// How long to receive frames after the first decoded frame arrives.
/// Once the pipeline is producing frames we only need a short window
/// to confirm steady-state; longer would just make the test slower
/// without adding signal.
const RUN_WINDOW: Duration = Duration::from_secs(2);

/// Hard ceiling on the test duration -- even if the server never
/// produces a frame (worst case), we exit this fast so `cargo test`
/// doesn't hang.
const HARD_TIMEOUT: Duration = Duration::from_secs(15);

/// How long to wait for the server's TCP port to accept a
/// connection after spawning it. Server startup (compositor init,
/// encoder init, bind) typically lands in 1-3s.
const SERVER_READY_TIMEOUT: Duration = Duration::from_secs(15);

/// How long to wait for the first decoded frame before declaring
/// the pipeline dead. The server can take 1-2s after accepting
/// connections to produce its first frame (encoder warmup); 10s
/// is generous.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(10);

const MIN_RENDERED: u64 = 1;
const MIN_DECODED: u64 = 1;

fn skip_if_unavailable() -> bool {
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        eprintln!(
            "WAYLAND_DISPLAY not set -- skipping e2e smoke test \
             (set it to a Wayland session to run)"
        );
        return true;
    }
    false
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_test_writer()
        .try_init();
}

fn pick_free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind :0");
    let port = listener.local_addr().expect("local_addr").port();
    drop(listener);
    port
}

fn wait_for_server(host: &str, port: u16, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if TcpStream::connect((host, port)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

fn locate_server_binary() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("WWS_SERVER_BIN") {
        return std::path::PathBuf::from(p);
    }
    let manifest_dir =
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
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
        "could not find `waylandwebstream` binary; tried {}",
        workspace.join("target").display()
    );
}

fn spawn_server(port: u16, wayland_display: &str) -> Child {
    let bin = locate_server_binary();
    Command::new(bin)
        .args([
            "--display-name",
            wayland_display,
            "--port",
            &port.to_string(),
            "--listen-addr",
            "127.0.0.1",
            "--encoder",
            "x264",
        ])
        .env("WAYLAND_DISPLAY", wayland_display)
        .env("RUST_LOG", "info")
        .stdout(Stdio::from(std::fs::File::create("/tmp/wws-smoke-server.log").unwrap()))
        .stderr(Stdio::from(std::fs::File::create("/tmp/wws-smoke-server.err").unwrap()))
        .spawn()
        .expect("failed to spawn `waylandwebstream` -- binary should exist")
}

#[test]
fn end_to_end_smoke() {
    if skip_if_unavailable() {
        return;
    }
    init_tracing();
    let wayland_display =
        std::env::var("WAYLAND_DISPLAY").expect("checked in skip_if_unavailable");

    let port = pick_free_port();
    eprintln!("smoke_e2e: spawning server on 127.0.0.1:{port} (display={wayland_display})");
    let mut server = spawn_server(port, &wayland_display);
    if !wait_for_server("127.0.0.1", port, SERVER_READY_TIMEOUT) {
        let _ = server.kill();
        let _ = server.wait();
        panic!(
            "server did not start listening on 127.0.0.1:{port} within {SERVER_READY_TIMEOUT:?}; \
             see /tmp/wws-smoke-server.{{log,err}} for details"
        );
    }
    eprintln!("smoke_e2e: server ready");

    let result = std::panic::catch_unwind(|| run_test(port));
    let _ = server.kill();
    let _ = server.wait();
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

fn run_test(port: u16) {
    let server_url = format!("ws://127.0.0.1:{port}/client");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    rt.block_on(async {
        let mut transport = WsTransport::connect(&server_url)
            .await
            .expect("connect to /client");
        transport
            .send(
                &serde_json::to_string(&SignalingMessage::Ready)
                    .expect("serialize Ready"),
            )
            .await
            .expect("send Ready");
        transport
            .send(
                &serde_json::to_string(&SignalingMessage::Resize {
                    width: 1280,
                    height: 720,
                })
                .expect("serialize Resize"),
            )
            .await
            .expect("send Resize");

        let (packet_tx, packet_rx) = mpsc::sync_channel::<Vec<u8>>(1);
        let (frame_tx, frame_rx) = mpsc::sync_channel::<DecodedFrame>(1);
        let display =
            spawn_display_thread((1280, 720), frame_rx).expect("spawn display thread");
        let (_decoder_join, decoded_count) = spawn_decoder_thread(packet_rx, frame_tx);

        // Counters accumulated over the test.
        let mut wire_frames: u64 = 0;
        let mut wire_keyframes: u64 = 0;
        let mut wire_audio: u64 = 0;
        let mut wire_control: u64 = 0;
        let mut first_control_kind: Option<String> = None;

        // Phase 1: wait until the first decoded frame is observed.
        // This is the "pipeline is alive" gate. If it doesn't fire
        // within FIRST_FRAME_TIMEOUT the server isn't producing
        // frames (or our decode is broken); fail fast.
        let phase1_start = Instant::now();
        loop {
            if phase1_start.elapsed() > FIRST_FRAME_TIMEOUT {
                let _ = std::io::stderr().write_all(
                    format!(
                        "smoke_e2e: FAILED -- no decoded frame in {FIRST_FRAME_TIMEOUT:?}; \
                         wire_frames={wire_frames} decoded=0\n"
                    )
                    .as_bytes(),
                );
                panic!(
                    "no decoded frame produced within {FIRST_FRAME_TIMEOUT:?}; \
                     wire_frames={wire_frames}, server output in /tmp/wws-smoke-server.log"
                );
            }
            if decoded_count.load(Ordering::Relaxed) >= 1 {
                break;
            }
            pump_one_frame(
                &mut transport,
                &packet_tx,
                &mut wire_frames,
                &mut wire_keyframes,
                &mut wire_audio,
                &mut wire_control,
                &mut first_control_kind,
            )
            .await;
        }
        eprintln!("smoke_e2e: first decoded frame at t={:?}", phase1_start.elapsed());

        // Phase 2: timed run window. Just keep pumping until
        // RUN_WINDOW has elapsed, then read the final counters.
        let window_deadline = Instant::now() + RUN_WINDOW;
        let hard_deadline = Instant::now() + HARD_TIMEOUT;
        while Instant::now() < window_deadline && Instant::now() < hard_deadline {
            pump_one_frame(
                &mut transport,
                &packet_tx,
                &mut wire_frames,
                &mut wire_keyframes,
                &mut wire_audio,
                &mut wire_control,
                &mut first_control_kind,
            )
            .await;
        }

        // Brief drain settle so the decoder + display thread finish
        // anything still in flight.
        tokio::time::sleep(Duration::from_millis(300)).await;

        let decoded = decoded_count.load(Ordering::Relaxed);
        let rendered = display.render_counter.load(Ordering::Relaxed);
        let released = display.release_counter.load(Ordering::Relaxed);

        let _ = std::io::stderr().write_all(
            format!(
                "smoke_e2e: wire_frames={wire_frames} (keyframes={wire_keyframes}) \
                 audio={wire_audio} control={wire_control} \
                 decoded={decoded} rendered={rendered} released={released} \
                 first_control={first_control_kind:?}\n"
            )
            .as_bytes(),
        );

        // Assertions, each naming the failure mode it covers.
        assert!(
            wire_control >= 2,
            "expected >= 2 server control messages (Codec + Bitrate), got {wire_control}; \
             the server's /client endpoint may be broken. \
             (See /tmp/wws-smoke-server.log for server output.)"
        );
        assert!(
            wire_frames > 0,
            "no video frames received during the run window; \
             the server isn't broadcasting. \
             (See /tmp/wws-smoke-server.log for server output.)"
        );
        assert!(
            decoded >= MIN_DECODED,
            "decoder produced {decoded} frames from {wire_frames} H.264 packets; \
             expected >= {MIN_DECODED}. Likely an ffmpeg decode failure or a \
             wire-format mismatch between the server's H.264 output and the \
             client's H.264 decoder."
        );
        assert!(
            released >= 1,
            "no wl_buffer::Release events dispatched in the run window; \
             the compositor isn't releasing our wl_shm buffers, so the \
             renderer can't acquire a free slot. (Previous bug: \
             dispatch_pending vs blocking_dispatch + the Dispatch trait \
             binding for WlBuffer, SlotId.)"
        );
        assert!(
            rendered >= MIN_RENDERED,
            "decoder produced {decoded} frames and compositor released {released} \
             buffers, but renderer committed only {rendered} to the wl_surface; \
             expected >= {MIN_RENDERED}. Some path between Release dispatch \
             and ShmRenderer::render is broken."
        );
    });
}

/// Pull one frame from the transport (or time out after 200ms)
/// and update the running counters. Forwards H.264 packets to the
/// decoder via `packet_tx`. Audio and control frames are counted
/// but not forwarded (Phase 6 plays audio).
#[allow(clippy::too_many_arguments)]
async fn pump_one_frame(
    transport: &mut WsTransport,
    packet_tx: &mpsc::SyncSender<Vec<u8>>,
    wire_frames: &mut u64,
    wire_keyframes: &mut u64,
    wire_audio: &mut u64,
    wire_control: &mut u64,
    first_control_kind: &mut Option<String>,
) {
    match tokio::time::timeout(Duration::from_millis(200), transport.recv()).await {
        Ok(Ok(Frame::VideoFrame { is_keyframe, data, .. })) => {
            *wire_frames += 1;
            if is_keyframe {
                *wire_keyframes += 1;
            }
            let _ = packet_tx.try_send(data);
        }
        Ok(Ok(Frame::AudioFrame { .. })) => *wire_audio += 1,
        Ok(Ok(Frame::Control(msg))) => {
            if first_control_kind.is_none() {
                *first_control_kind = Some(format!("{msg:?}"));
            }
            *wire_control += 1;
        }
        Ok(Err(_)) | Err(_) => {} // timeout or transient error -- just retry
    }
}

/// Compile-time sanity check: the decoder's counter type matches
/// the shape we use here. If `spawn_decoder_thread`'s return type
/// ever changes this trips the build.
#[allow(dead_code)]
fn _counter_type_check(c: Arc<AtomicU64>) -> Arc<AtomicU64> {
    c
}
use std::sync::Arc;