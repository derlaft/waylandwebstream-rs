// Software H.264 -> ARGB32 decoder.
//
// Mirrors the server's encoder structure: a single stateful `H264Decoder`
// runs on a dedicated OS thread (see `run_decoder_thread`); packets
// arrive on a `sync_channel(1)`, decoded frames leave on another
// `sync_channel(1)`. The thread is the sole owner of the ffmpeg
// decoder and swscale context -- both are not `Send` and trying to
// smuggle them through tokio's blocking pool requires an `unsafe impl
// Send`. A dedicated thread sidesteps the question entirely and
// matches the server's "ffmpeg blocks, give it its own thread" rule
// from AGENTS.md.
//
// The decode output is `wl_shm::Format::Argb8888` packed as little-
// endian u32 (i.e. B,G,R,A byte order on x86_64). stride = width*4,
// no row padding -- the renderer relies on this so it can memcpy
// straight into a wl_shm pool without per-row handling.

use anyhow::{anyhow, Context, Result};
use ffmpeg_next as ffmpeg;

/// ARGB8888 pixel data ready to be blitted into a wl_shm pool.
///
/// `pixels` length is exactly `width * height * 4` bytes, no padding.
/// The renderer is allowed to memcpy it straight into a wl_shm buffer
/// at the same stride.
#[derive(Debug)]
pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// Spawn the SW H.264 decoder thread.
///
/// Reads H.264 packet payloads from `packet_rx` (each `Vec<u8>` is one
/// Annex-B NAL unit -- one `MSG_VIDEO_FRAME` body from the server's
/// `/client` endpoint, with the 14-byte unified header already
/// stripped by the transport). Pushes at most one `DecodedFrame` per
/// packet onto `frame_tx`. The sender side uses `try_send`, so when
/// the renderer is slow we drop new frames rather than build up a
/// backlog of stale P-frames (mirrors the server's tiny broadcast
/// channel -- see "The video broadcast channel is intentionally tiny
/// (cap 3)" in AGENTS.md).
///
/// Returns `(JoinHandle, Arc<AtomicU64>)`. The atomic is a counter
/// of `DecodedFrame`s the decoder successfully produced (i.e.
/// `Ok(Some(_))` returns). Tests use this to localize "no frames
/// reaching the renderer" failures: if `decoded_count == 0` the
/// decoder never produced output (ffmpeg issue or wrong format);
/// if `decoded_count > 0 && rendered == 0` the chain breaks
/// between decoder output and surface commit (display/render
/// side). `main` ignores the counter.
pub fn spawn_decoder_thread(
    packet_rx: std::sync::mpsc::Receiver<Vec<u8>>,
    frame_tx: std::sync::mpsc::SyncSender<DecodedFrame>,
) -> (
    std::thread::JoinHandle<()>,
    std::sync::Arc<std::sync::atomic::AtomicU64>,
) {
    let decoded_count = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let decoded_for_thread = decoded_count.clone();
    let join = std::thread::Builder::new()
        .name("wws-h264-decoder".into())
        .spawn(move || run_decoder_thread(packet_rx, frame_tx, decoded_for_thread))
        .expect("failed to spawn decoder thread");
    (join, decoded_count)
}

/// Owns the ffmpeg decoder + scaler + a reusable destination frame.
/// Held only on the decoder thread; not `Send`/`Sync`.
struct H264Decoder {
    decoder: ffmpeg::decoder::Video,
    /// `scaling::Context` is created for one specific (src_fmt, w, h) ->
    /// (dst_fmt, w, h) tuple. On resolution change we throw it away and
    /// build a new one (same pattern the server's encoder uses --
    /// `frame_size_mismatch_reinitializes_encoder`).
    scaler: Option<(u32, u32, ffmpeg::software::scaling::Context)>,
    /// Owned ARGB8888 frame that swscale writes into each call. `alloc`
    /// gives it a real backing buffer (unlike `Video::empty()`).
    rgb: ffmpeg::frame::Video,
}

impl H264Decoder {
    fn new() -> Result<Self> {
        ffmpeg::init().context("ffmpeg init")?;

        let codec = ffmpeg::decoder::find(ffmpeg::codec::Id::H264)
            .ok_or_else(|| anyhow!("H.264 decoder not found"))?;
        let decoder = ffmpeg::codec::context::Context::new_with_codec(codec)
            .decoder()
            .video()
            .context("open H.264 decoder as video")?;

        let mut rgb = ffmpeg::frame::Video::empty();
        rgb.set_format(ffmpeg::format::Pixel::BGRA);

        Ok(Self {
            decoder,
            scaler: None,
            rgb,
        })
    }

    /// Feed one H.264 packet (Annex-B) to the decoder. Returns the next
    /// decoded frame if one was produced; `Ok(None)` means "the decoder
    /// consumed the packet but didn't emit a frame yet -- send more".
    /// Errors bubble up so the caller can log and decide whether to
    /// continue or bail.
    fn decode(&mut self, packet_data: &[u8]) -> Result<Option<DecodedFrame>> {
        // `Borrow::new` doesn't copy the packet data; it just records
        // a pointer + length and `av_packet_unref`s on drop.
        let packet = ffmpeg::codec::packet::Borrow::new(packet_data);
        self.decoder
            .send_packet(&packet)
            .context("avcodec_send_packet")?;

        let mut raw = ffmpeg::frame::Video::empty();
        match self.decoder.receive_frame(&mut raw) {
            Ok(()) => {}
            Err(ffmpeg::Error::Other {
                errno: ffmpeg::error::EAGAIN,
            }) => return Ok(None),
            Err(e) => return Err(anyhow!("avcodec_receive_frame: {e}")),
        }

        let width = raw.width();
        let height = raw.height();
        if width == 0 || height == 0 {
            anyhow::bail!("decoder emitted a zero-dimension frame: {width}x{height}");
        }

        // (Re)create the swscale context if the source resolution or
        // format changed. We rebuild -- not `cached()` -- because the
        // cached variant's exact freshness semantics (when the cache
        // is consulted vs. when a fresh context is built) differ
        // across ffmpeg versions and the rebuild cost is negligible
        // at our framerate.
        let scaler_recreated = match &self.scaler {
            Some((sw, sh, _)) => *sw != width || *sh != height,
            None => true,
        };
        if scaler_recreated {
            let scaler = ffmpeg::software::scaling::Context::get(
                raw.format(),
                width,
                height,
                ffmpeg::format::Pixel::BGRA,
                width,
                height,
                ffmpeg::software::scaling::Flags::POINT,
            )
            .context("create swscale context")?;
            // Match the rgb frame's allocation to the new size so swscale
            // has a destination buffer to write into.
            unsafe {
                self.rgb.alloc(ffmpeg::format::Pixel::BGRA, width, height);
            }
            self.scaler = Some((width, height, scaler));
        }

        let (_, _, scaler) = self.scaler.as_mut().expect("just set");
        scaler.run(&raw, &mut self.rgb).context("swscale run")?;

        let pixels = self.rgb.data(0).to_vec();
        Ok(Some(DecodedFrame {
            width,
            height,
            pixels,
        }))
    }
}

/// Body of the decoder thread. Loops pulling packets, decoding them,
/// and pushing `DecodedFrame`s to the renderer. Exits cleanly when
/// the packet channel closes (the recv_task has shut down).
fn run_decoder_thread(
    packet_rx: std::sync::mpsc::Receiver<Vec<u8>>,
    frame_tx: std::sync::mpsc::SyncSender<DecodedFrame>,
    decoded_count: std::sync::Arc<std::sync::atomic::AtomicU64>,
) {
    let mut decoder = match H264Decoder::new() {
        Ok(d) => d,
        Err(e) => {
            tracing::error!("decoder init failed: {e:#}");
            return;
        }
    };
    loop {
        let packet = match packet_rx.recv() {
            Ok(p) => p,
            Err(std::sync::mpsc::RecvError) => break,
        };
        if packet.is_empty() {
            continue;
        }
        let frame = match decoder.decode(&packet) {
            Ok(Some(f)) => f,
            Ok(None) => continue, // EAGAIN, need more packets
            Err(e) => {
                tracing::warn!("decode error: {e:#}");
                continue;
            }
        };
        decoded_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // try_send: if the renderer is slow, drop the new frame. The
        // next keyframe (~1s away) will resync the picture cheaply.
        if frame_tx.try_send(frame).is_err() {
            tracing::debug!("renderer behind; dropping frame");
        }
    }
}
