// PipeWire audio capture with Opus encoding.
//
// Architecture:
//   1. A PipeWire capture stream with STREAM_CAPTURE_SINK connects to the default sink's
//      monitor port.  In a container with no physical audio hardware, WirePlumber
//      automatically creates a "Dummy Output" null sink; apps that play audio are routed
//      there.  If a pw-loopback sink was set up externally (e.g. in the systemd service or
//      via `pactl load-module module-null-sink`), the capture stream monitors that instead.
//   2. Incoming F32LE PCM samples are buffered into 20 ms / 960-sample Opus frames and
//      encoded to Opus at 96 kbps stereo.
//   3. Encoded AudioPackets are broadcast over a tokio broadcast channel consumed by the
//      /client WebSocket.
//
// The PipeWire main loop runs on a dedicated OS thread.  Process exit kills it cleanly.

use anyhow::{Context, Result};
use opus::{Application, Channels, Encoder as OpusEncoder};
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use pw::spa::pod::Pod;
use pw::stream::StreamFlags;
use spa::param::audio::{AudioFormat, AudioInfoRaw};
use spa::param::ParamType;
use spa::pod::{serialize::PodSerializer, Object, Value};
use spa::utils::{Direction, SpaTypes};
use tokio::sync::broadcast;
use tracing::{error, info, warn};

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u32 = 2;
// 20 ms frame — standard Opus frame size.
const OPUS_FRAME_SAMPLES: usize = 960;
// Opus worst-case per packet; 4 kB is ample.
const OPUS_MAX_PACKET: usize = 4096;

#[derive(Clone)]
pub struct AudioPacket {
    /// Raw Opus-encoded bytes for one 20 ms frame.
    pub data: Vec<u8>,
    /// Presentation timestamp in microseconds, monotonically from 0.
    pub pts_us: u64,
}

/// Starts PipeWire audio capture on a dedicated OS thread.
/// Returns the broadcast sender; subscribe to receive encoded Opus packets.
pub fn spawn_audio_capture() -> Result<broadcast::Sender<AudioPacket>> {
    let (audio_tx, _) = broadcast::channel::<AudioPacket>(32);
    let tx = audio_tx.clone();

    std::thread::Builder::new()
        .name("pw-audio-capture".into())
        .spawn(move || {
            if let Err(e) = run_capture_loop(tx) {
                error!("Audio capture thread exited with error: {e:#}");
            }
        })
        .context("Failed to spawn audio capture thread")?;

    Ok(audio_tx)
}

fn run_capture_loop(audio_tx: broadcast::Sender<AudioPacket>) -> Result<()> {
    pw::init();

    let main_loop =
        pw::main_loop::MainLoopBox::new(None).context("Failed to create PipeWire main loop")?;
    let context = pw::context::ContextBox::new(main_loop.loop_(), None)
        .context("Failed to create PipeWire context")?;
    let core = context
        .connect(None)
        .context("Failed to connect to PipeWire")?;

    // State owned by the stream process callback.
    struct State {
        encoder: OpusEncoder,
        pcm_buf: Vec<f32>,
        pts_us: u64,
        audio_tx: broadcast::Sender<AudioPacket>,
    }

    let mut encoder = OpusEncoder::new(SAMPLE_RATE, Channels::Stereo, Application::Audio)
        .context("Failed to create Opus encoder")?;
    encoder
        .set_bitrate(opus::Bitrate::Bits(96_000))
        .context("Failed to set Opus bitrate")?;

    let state = State {
        encoder,
        pcm_buf: Vec::with_capacity(OPUS_FRAME_SAMPLES * CHANNELS as usize * 4),
        pts_us: 0,
        audio_tx,
    };

    // STREAM_CAPTURE_SINK: monitor the default sink's output rather than reading
    // from a microphone source.  In a no-hardware container, the default sink is
    // the WirePlumber fallback null sink, so this captures whatever apps play.
    let props = properties! {
        *pw::keys::MEDIA_TYPE     => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE     => "Music",
        *pw::keys::STREAM_CAPTURE_SINK => "true",
        "node.name"               => "wws-audio-capture",
    };

    let stream = pw::stream::StreamBox::new(&core, "wws-audio-capture", props)
        .context("Failed to create PipeWire stream")?;

    let _listener = stream
        .add_local_listener_with_user_data(state)
        .process(|stream, state| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                return;
            }

            let data = &mut datas[0];
            let n_bytes = data.chunk().size() as usize;
            if n_bytes == 0 {
                return;
            }

            let Some(raw) = data.data() else { return };
            let n_samples = n_bytes / std::mem::size_of::<f32>();
            // SAFETY: we negotiated F32LE in stream.connect(), so PipeWire fills
            // this buffer with `n_samples` valid, natively-aligned f32 PCM samples
            // (audio DMA buffers are at least sample-aligned). `raw` stays valid
            // and unaliased for the duration of this borrow.
            let samples =
                unsafe { std::slice::from_raw_parts(raw.as_ptr() as *const f32, n_samples) };

            state.pcm_buf.extend_from_slice(samples);

            // Drain in 20 ms chunks.  The PW quantum may be larger than 960 samples, so
            // this may encode more than one frame per process() call.
            let frame_len = OPUS_FRAME_SAMPLES * CHANNELS as usize;
            while state.pcm_buf.len() >= frame_len {
                let pcm: Vec<f32> = state.pcm_buf.drain(..frame_len).collect();
                let mut pkt = vec![0u8; OPUS_MAX_PACKET];
                match state.encoder.encode_float(&pcm, &mut pkt) {
                    Ok(n) => {
                        pkt.truncate(n);
                        // Fire-and-forget broadcast: no subscribers when no
                        // client is connected, and audio is real-time so a
                        // dropped packet is simply skipped (not retransmitted).
                        let _ = state.audio_tx.send(AudioPacket {
                            data: pkt,
                            pts_us: state.pts_us,
                        });
                    }
                    Err(e) => warn!("Opus encode error: {e}"),
                }
                state.pts_us += (OPUS_FRAME_SAMPLES as u64 * 1_000_000) / SAMPLE_RATE as u64;
            }
        })
        .register()
        .context("Failed to register stream listener")?;

    // Negotiate F32LE 48 kHz stereo.
    let mut audio_info = AudioInfoRaw::new();
    audio_info.set_format(AudioFormat::F32LE);
    audio_info.set_rate(SAMPLE_RATE);
    audio_info.set_channels(CHANNELS);

    let obj = Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: ParamType::EnumFormat.as_raw(),
        properties: audio_info.into(),
    };
    let values: Vec<u8> =
        PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(obj))
            .context("Failed to serialize audio format pod")?
            .0
            .into_inner();

    let mut params = [Pod::from_bytes(&values).expect("audio format pod is well-formed")];

    stream
        .connect(
            Direction::Input,
            None,
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .context("Failed to connect PipeWire capture stream")?;

    info!("PipeWire audio capture stream connected (F32LE 48 kHz stereo → Opus 96 kbps)");
    main_loop.run();

    info!("PipeWire audio main loop exited");
    Ok(())
}
