// End-to-end smoke test for the native client.
//
// Verifies:
//   1. input/output is working -- the handshake (Ready + Resize) is
//      accepted by the server, and we get back the Codec / Bitrate
//      control messages every connected client receives.
//   2. video playback is "somewhat working" -- at least N decoded
//      frames reach the Wayland renderer's `render_count`, proving
//      the full pipeline (transport -> decoder -> swscale ->
//      ShmRenderer -> wl_surface::commit) actually completes end to
//      end.
//
// Skipped when `WAYLAND_DISPLAY` is unset (i.e. there's no Wayland
// compositor for the native client to attach to), or when
// `WWS_TEST_SERVER_URL` is unset (i.e. the user hasn't pointed us
// at a running server).
//
// To run on a workstation with both a Wayland compositor and a
// running wayland-webstream server:
//
//   WAYLAND_DISPLAY=wayland-0 \
//   WWS_TEST_SERVER_URL=ws://127.0.0.1:8765/client \
//   cargo test -p native-client --test smoke_e2e -- --nocapture
//
// The `--nocapture` lets the diagnostic `eprintln!`s (frame counts,
// sizes) show up in the test output. Without it the test passes
// silently.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use native_client::decode::sw::{spawn_decoder_thread, DecodedFrame};
use native_client::display::spawn_display_thread;
use native_client::transport::websocket::WsTransport;
use native_client::transport::{Frame, Transport};
use native_client::types::SignalingMessage;

/// How long to receive frames before asserting. Long enough that a
/// 60fps stream lands ~60 frames for the "many" assertion, short
/// enough that the test doesn't dominate `cargo test` runtime.
const RUN_DURATION: Duration = Duration::from_secs(3);

/// After the recv loop ends, give the decoder + display thread time
/// to drain the channels. The decoder is sync on a dedicated
/// thread; the display thread is too. 500ms is generous for
/// ~5-10 in-flight frames on the cap-1 channels.
const DRAIN_SETTLE: Duration = Duration::from_millis(500);

/// Minimum number of decoded frames that must reach the renderer's
/// commit path. Picked so a single short keyframe gap (server
/// silence for the first ~2s of GOP boundary) can't fail the test
/// but a totally dead decoder or renderer can.
const MIN_RENDERED: u64 = 1;

/// Skip the test cleanly when the env vars it needs aren't set.
fn skip_if_unavailable() -> bool {
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        eprintln!(
            "WAYLAND_DISPLAY not set -- skipping e2e smoke test \
             (run with a Wayland compositor attached to test live \
             video playback)"
        );
        return true;
    }
    if std::env::var_os("WWS_TEST_SERVER_URL").is_none() {
        eprintln!(
            "WWS_TEST_SERVER_URL not set -- skipping e2e smoke test \
             (point it at a running server, e.g. \
             WWS_TEST_SERVER_URL=ws://127.0.0.1:8765/client)"
        );
        return true;
    }
    false
}

#[test]
fn end_to_end_smoke() {
    if skip_if_unavailable() {
        return;
    }
    let server_url =
        std::env::var("WWS_TEST_SERVER_URL").expect("checked in skip_if_unavailable");

    // Single tokio runtime for both the transport and the test's own
    // sleeps. The decoder + display threads are pure `std::thread`
    // -- no runtime on those sides.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    rt.block_on(async {
        // 1. Connect transport and do the handshake. Connecting alone
        //    already proves the WS layer + the server's
        //    `unified_client_handler` accept our endpoint, which
        //    counts as "input/output is working" at the wire level.
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

        // 2. Wire up the full decode + render pipeline. We mirror
        //    main.rs so the test exercises the same code paths the
        //    binary uses.
        let (packet_tx, packet_rx) = mpsc::sync_channel::<Vec<u8>>(1);
        let (frame_tx, frame_rx) = mpsc::sync_channel::<DecodedFrame>(1);
        let display =
            spawn_display_thread((1280, 720), frame_rx).expect("spawn display thread");
        let _decoder_join = spawn_decoder_thread(packet_rx, frame_tx);

        // 3. Receive packets for `RUN_DURATION`, forwarding video
        //    packets to the decoder. Anything else (audio, control)
        //    is just counted.
        let mut wire_frames = 0u32;
        let mut wire_keyframes = 0u32;
        let mut wire_audio = 0u32;
        let mut wire_control = 0u32;
        let mut first_control_kind: Option<String> = None;
        let deadline = Instant::now() + RUN_DURATION;

        while Instant::now() < deadline {
            // 200ms poll cadence: short enough that we don't miss
            // a server-sent close, long enough that idle time
            // doesn't burn CPU.
            match tokio::time::timeout(Duration::from_millis(200), transport.recv()).await {
                Ok(Ok(Frame::VideoFrame { is_keyframe, data, .. })) => {
                    wire_frames += 1;
                    if is_keyframe {
                        wire_keyframes += 1;
                    }
                    // try_send: a full decoder is fine -- we'll drop
                    // and the renderer will catch up on the keyframe
                    // cadence.
                    let _ = packet_tx.try_send(data);
                }
                Ok(Ok(Frame::AudioFrame { .. })) => wire_audio += 1,
                Ok(Ok(Frame::Control(msg))) => {
                    if first_control_kind.is_none() {
                        first_control_kind = Some(format!("{msg:?}"));
                    }
                    wire_control += 1;
                }
                Ok(Err(e)) => {
                    eprintln!("transport closed: {e:#}");
                    break;
                }
                Err(_) => continue, // poll timeout, keep waiting
            }
        }

        // 4. Give the decoder + display thread time to drain. The
        //    channels are bounded to 1 so a small constant is fine.
        tokio::time::sleep(DRAIN_SETTLE).await;

        let rendered = display
            .render_counter
            .load(std::sync::atomic::Ordering::Relaxed);

        eprintln!(
            "smoke_e2e: wire_frames={wire_frames} (keyframes={wire_keyframes}) \
             audio={wire_audio} control={wire_control} rendered={rendered} \
             first_control={:?}",
            first_control_kind
        );

        // 5. Assertions.

        // (a) I/O working: the server sent us control messages
        //     (Codec + Bitrate) on connect. Their absence would mean
        //     the unified endpoint isn't wired up correctly.
        assert!(
            wire_control >= 2,
            "expected at least 2 server control messages (Codec + Bitrate), \
             got {wire_control}; is the server's /client endpoint running?"
        );

        // (b) Video frames arrived over the wire. Zero means the
        //     server isn't broadcasting (no compositor damage, or
        //     the wire format is wrong).
        assert!(
            wire_frames > 0,
            "no video frames received in {RUN_DURATION:?}; is the server's \
             compositor producing damage? (try running wayland-test-client \
             against WAYLAND_DISPLAY=<display-name>)"
        );

        // (c) At least one frame went all the way through to a
        //     surface commit. This is the "video playback is somewhat
        //     working" assertion.
        assert!(
            rendered >= MIN_RENDERED,
            "only {rendered} frame(s) attached to the wl_surface in \
             {RUN_DURATION:?}; renderer is stuck (likely a \
             wl_buffer::Release dispatch issue)"
        );
    });
}