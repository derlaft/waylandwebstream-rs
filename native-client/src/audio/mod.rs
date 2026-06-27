// Opus decode + PipeWire playback for Phase 6.
//
// `AudioPlayer` lives on the tokio recv task. `push_opus` decodes one
// Opus packet to F32LE stereo PCM and hands it to a dedicated PipeWire
// playback thread via a sync_channel. The PW process callback drains
// that channel into PW output buffers; if none are available it fills
// with silence rather than stalling.
//
// No A/V sync is attempted -- this is a diagnostic tool and the
// latency introduced here is not the focus (see plan §N7).

use anyhow::{Context, Result};
use opus::{Channels, Decoder as OpusDecoder};
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use pw::spa::pod::Pod;
use pw::stream::StreamFlags;
use spa::param::audio::{AudioFormat, AudioInfoRaw};
use spa::param::ParamType;
use spa::pod::{serialize::PodSerializer, Object, Value};
use spa::utils::{Direction, SpaTypes};
use std::sync::mpsc;
use tracing::{error, info, warn};

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u32 = 2;
// One Opus frame = 20 ms at 48 kHz.
const OPUS_FRAME_SAMPLES: usize = 960;

/// Owns the Opus decoder. Created on the tokio recv task; `push_opus`
/// decodes a packet and queues the PCM for the PipeWire thread.
pub struct AudioPlayer {
    tx: mpsc::SyncSender<Vec<f32>>,
    decoder: OpusDecoder,
}

impl AudioPlayer {
    /// Build an `AudioPlayer` with an externally-supplied channel sender,
    /// skipping the PipeWire thread. Only for unit tests.
    #[cfg(test)]
    pub(crate) fn new_with_sender(tx: mpsc::SyncSender<Vec<f32>>) -> Result<Self> {
        let decoder = OpusDecoder::new(SAMPLE_RATE, Channels::Stereo)
            .context("create Opus decoder")?;
        Ok(Self { tx, decoder })
    }

    /// Spawn the PipeWire playback thread and build the player.
    /// Returns `Err` if PipeWire is not available or the Opus decoder
    /// can't be initialized; the caller should log and continue without audio.
    pub fn spawn() -> Result<Self> {
        let (tx, rx) = mpsc::sync_channel::<Vec<f32>>(8);
        std::thread::Builder::new()
            .name("pw-audio-playback".into())
            .spawn(move || {
                if let Err(e) = run_playback_loop(rx) {
                    error!("audio playback thread exited: {e:#}");
                }
            })
            .context("failed to spawn audio playback thread")?;

        let decoder = OpusDecoder::new(SAMPLE_RATE, Channels::Stereo)
            .context("failed to create Opus decoder")?;

        Ok(Self { tx, decoder })
    }

    /// Decode one Opus packet and push the PCM to the playback thread.
    /// Drops silently if the playback thread is backlogged (prefers
    /// dropping over stalling the recv loop).
    pub fn push_opus(&mut self, data: &[u8]) -> Result<()> {
        // Worst-case Opus frame: OPUS_FRAME_SAMPLES × CHANNELS floats.
        let mut pcm = vec![0.0f32; OPUS_FRAME_SAMPLES * CHANNELS as usize];
        let n_per_ch = self
            .decoder
            .decode_float(data, &mut pcm, false)
            .context("Opus decode")?;
        pcm.truncate(n_per_ch * CHANNELS as usize);
        if self.tx.try_send(pcm).is_err() {
            warn!("audio backlog; dropping Opus frame");
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use opus::{Application, Encoder as OpusEncoder};

    fn make_player() -> (AudioPlayer, mpsc::Receiver<Vec<f32>>) {
        let (tx, rx) = mpsc::sync_channel(8);
        let player = AudioPlayer::new_with_sender(tx).unwrap();
        (player, rx)
    }

    /// Encode `pcm` (F32LE stereo) to a valid Opus packet.
    fn encode_pcm(pcm: &[f32]) -> Vec<u8> {
        let mut enc =
            OpusEncoder::new(SAMPLE_RATE, Channels::Stereo, Application::Audio).unwrap();
        let mut out = vec![0u8; 4096];
        let n = enc.encode_float(pcm, &mut out).unwrap();
        out.truncate(n);
        out
    }

    // --- decode path ---

    #[test]
    fn push_opus_decodes_silence_and_sends_pcm() {
        let (mut player, rx) = make_player();
        let pcm_in = vec![0.0f32; OPUS_FRAME_SAMPLES * CHANNELS as usize];
        let pkt = encode_pcm(&pcm_in);

        player.push_opus(&pkt).unwrap();

        let pcm_out = rx.try_recv().expect("PCM should arrive synchronously");
        assert_eq!(
            pcm_out.len(),
            OPUS_FRAME_SAMPLES * CHANNELS as usize,
            "decoded frame length"
        );
        // Opus may add a tiny bit of ringing but silence should decode near-zero.
        for (i, &s) in pcm_out.iter().enumerate() {
            assert!(
                s.abs() < 1e-3,
                "sample[{i}] = {s} is not near silence"
            );
        }
    }

    #[test]
    fn push_opus_decodes_sine_and_preserves_amplitude() {
        let (mut player, rx) = make_player();
        // 440 Hz sine, peak ±0.5, stereo interleaved.
        let pcm_in: Vec<f32> = (0..OPUS_FRAME_SAMPLES * CHANNELS as usize)
            .map(|i| {
                let t = (i / CHANNELS as usize) as f32 / SAMPLE_RATE as f32;
                0.5 * (2.0 * std::f32::consts::PI * 440.0 * t).sin()
            })
            .collect();
        let pkt = encode_pcm(&pcm_in);

        player.push_opus(&pkt).unwrap();

        let pcm_out = rx.try_recv().unwrap();
        // The decoded sine should have meaningful amplitude (not silence).
        let peak = pcm_out.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
        assert!(
            peak > 0.1,
            "decoded sine should have peak amplitude > 0.1, got {peak}"
        );
    }

    #[test]
    fn push_opus_rejects_obviously_invalid_packet() {
        let (mut player, _rx) = make_player();
        // 3 bytes with the first byte set to values that can't form a valid TOC.
        // libopus reliably errors on this.
        let result = player.push_opus(&[0xFF, 0xFF, 0xFF]);
        assert!(result.is_err(), "expected Err on invalid Opus packet");
    }

    // --- backlog / drop behaviour ---

    #[test]
    fn push_opus_drops_when_channel_is_full() {
        // Channel capacity 8; fill it then push one more — should not block or error.
        let (tx, _rx) = mpsc::sync_channel::<Vec<f32>>(2);
        let mut player = AudioPlayer::new_with_sender(tx).unwrap();
        let pcm_in = vec![0.0f32; OPUS_FRAME_SAMPLES * CHANNELS as usize];
        let pkt = encode_pcm(&pcm_in);

        for _ in 0..4 {
            // The first 2 fills the channel; the rest are try_send drops.
            // All calls must return Ok (drops are silent).
            assert!(player.push_opus(&pkt).is_ok());
        }
    }
}

// ---------------------------------------------------------------------------
// PipeWire playback thread
// ---------------------------------------------------------------------------

struct PlaybackState {
    rx: mpsc::Receiver<Vec<f32>>,
    // Accumulates decoded samples that span more than one PW quantum.
    buf: Vec<f32>,
}

fn run_playback_loop(rx: mpsc::Receiver<Vec<f32>>) -> Result<()> {
    pw::init();

    let main_loop =
        pw::main_loop::MainLoopBox::new(None).context("PipeWire main loop")?;
    let context = pw::context::ContextBox::new(main_loop.loop_(), None)
        .context("PipeWire context")?;
    let core = context
        .connect(None)
        .context("connect to PipeWire daemon")?;

    let state = PlaybackState {
        rx,
        buf: Vec::with_capacity(OPUS_FRAME_SAMPLES * CHANNELS as usize * 4),
    };

    let props = properties! {
        *pw::keys::MEDIA_TYPE     => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Playback",
        *pw::keys::MEDIA_ROLE     => "Music",
        "node.name"               => "wws-client-audio",
    };

    let stream =
        pw::stream::StreamBox::new(&core, "wws-client-audio", props)
            .context("create PipeWire stream")?;

    let _listener = stream
        .add_local_listener_with_user_data(state)
        .process(|stream, state| {
            // Pull all queued decoded frames into our local buffer first.
            while let Ok(pcm) = state.rx.try_recv() {
                state.buf.extend_from_slice(&pcm);
            }

            let Some(mut buf) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buf.datas_mut();
            if datas.is_empty() {
                return;
            }
            let data = &mut datas[0];
            let Some(raw_bytes) = data.data() else {
                return;
            };

            let capacity_f32 = raw_bytes.len() / std::mem::size_of::<f32>();
            let n_copy = capacity_f32.min(state.buf.len());

            // Write available samples to PW buffer.
            if n_copy > 0 {
                let n_bytes = n_copy * std::mem::size_of::<f32>();
                // SAFETY: f32 is Copy, the source and dest don't overlap,
                // and raw_bytes is large enough (checked via capacity_f32).
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        state.buf.as_ptr() as *const u8,
                        raw_bytes.as_mut_ptr(),
                        n_bytes,
                    );
                }
                state.buf.drain(..n_copy);
            }

            // Pad remainder with silence to avoid buffer underruns.
            let silence_start = n_copy * std::mem::size_of::<f32>();
            raw_bytes[silence_start..].fill(0);

            let total_bytes = capacity_f32 * std::mem::size_of::<f32>();
            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() =
                (CHANNELS as usize * std::mem::size_of::<f32>()) as i32;
            *chunk.size_mut() = total_bytes as u32;
        })
        .register()
        .context("register PipeWire stream listener")?;

    // Negotiate F32LE 48 kHz stereo output (mirrors the server's capture format).
    let mut audio_info = AudioInfoRaw::new();
    audio_info.set_format(AudioFormat::F32LE);
    audio_info.set_rate(SAMPLE_RATE);
    audio_info.set_channels(CHANNELS);

    let obj = Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: ParamType::EnumFormat.as_raw(),
        properties: audio_info.into(),
    };
    let values: Vec<u8> = PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &Value::Object(obj),
    )
    .context("serialize audio format pod")?
    .0
    .into_inner();

    let mut params =
        [Pod::from_bytes(&values).expect("audio format pod is well-formed")];

    stream
        .connect(
            Direction::Output,
            None,
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .context("connect PipeWire output stream")?;

    info!("PipeWire audio playback stream connected (F32LE 48 kHz stereo)");
    main_loop.run();
    Ok(())
}
