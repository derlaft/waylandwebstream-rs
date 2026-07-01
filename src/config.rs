use clap::{Parser, ValueEnum};

/// Which `Compositor` backend renders the output. `Gl`
/// (AGENTS.md) isn't implemented yet -- selecting
/// it falls back to `Sw` with a warning instead of failing to start.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lower")]
pub enum CompositorBackendArg {
    Sw,
    Gl,
}

/// Which `VideoEncoder` backend encodes captured frames. `Vaapi`
/// (AGENTS.md) does H.264 encode on the GPU via
/// `hwupload,scale_vaapi=format=nv12` + `h264_vaapi`; needs `--vaapi-device`
/// to point at a render node that actually has VAAPI H.264 encode support.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lower")]
pub enum EncoderBackendArg {
    X264,
    Vaapi,
}

#[derive(Debug, Parser)]
#[command(name = "waylandwebstream")]
#[command(version)] // `--version` reports the crate version (from Cargo.toml)
#[command(about = "A headless Wayland compositor streaming to browsers over WebSocket/WebCodecs", long_about = None)]
pub struct Config {
    /// Initial resolution (width x height)
    #[arg(long, default_value = "1280x720")]
    pub initial_resolution: String,

    /// Maximum resolution (width x height)
    #[arg(long, default_value = "3840x2160")]
    pub max_resolution: String,

    /// HTTP signaling server port
    #[arg(long, default_value = "8080")]
    pub port: u16,

    /// Address the HTTP signaling server binds to. Defaults to loopback only
    /// (127.0.0.1) because the server has no authentication of its own and a
    /// reachable port grants full keyboard/pointer/touch/clipboard injection
    /// into the session. Put an authenticating reverse proxy in front and only
    /// then widen this (e.g. 0.0.0.0) to expose it beyond localhost.
    #[arg(long, default_value = "127.0.0.1")]
    pub listen_addr: String,

    /// Target framerate
    #[arg(long, default_value = "60")]
    pub framerate: u32,

    /// Video bitrate in bits per second. In adaptive mode (the default,
    /// unless --crf is set) this is the starting point the controller probes
    /// up or down from; with --no-adaptive-bitrate it's used as a fixed
    /// constant bitrate instead.
    #[arg(long, default_value = "2000000")]
    pub bitrate: usize,

    /// Enable constant quality mode using this x264 CRF value (0-51, lower
    /// is higher quality/larger frames; 18-28 is a typical range) instead of
    /// targeting a bitrate at all. Frame size will vary with scene
    /// complexity since there is no bitrate cap in this mode. Disables
    /// adaptive bitrate (there is no bitrate to adapt).
    #[arg(long, value_parser = clap::value_parser!(u8).range(0..=51))]
    pub crf: Option<u8>,

    /// Disable adaptive bitrate control, which is otherwise on by default:
    /// it adjusts the encoder's bitrate between --min-bitrate and
    /// --max-bitrate based on client keyframe-resync requests (the signal
    /// that a client's decoder is falling behind) and reported decode
    /// latency. Has no effect when --crf is set.
    #[arg(long)]
    pub no_adaptive_bitrate: bool,

    /// Lower bound for adaptive bitrate.
    #[arg(long, default_value = "500000")]
    pub min_bitrate: usize,

    /// Upper bound for adaptive bitrate.
    #[arg(long, default_value = "12000000")]
    pub max_bitrate: usize,

    /// Keyframe interval in frames (GOP size). Lower values mean smaller,
    /// more frequent keyframes instead of large periodic bursts, which
    /// reduces the receive-side jitter buffer needed to absorb them - at
    /// the cost of a bit more average bitrate. Defaults to 2 seconds worth
    /// of frames at the configured framerate.
    #[arg(long)]
    pub keyframe_interval: Option<u32>,

    /// Wayland display name
    #[arg(long, default_value = "wayland-wws-0")]
    pub display_name: String,

    /// Rendering backend. `gl` (GPU compositing via smithay's GlesRenderer,
    /// reading the result back to the CPU -- no zero-copy GPU encode path
    /// yet) falls back to `sw` with a warning if GL/EGL/GBM initialization
    /// fails. See AGENTS.md
    #[arg(long, value_enum, default_value = "sw")]
    pub compositor: CompositorBackendArg,

    /// Video encoder backend. `vaapi` does hardware H.264 encode --
    /// requires a render node with VAAPI H.264 EncSlice support (check with
    /// `vainfo`). See AGENTS.md
    #[arg(long, value_enum, default_value = "x264")]
    pub encoder: EncoderBackendArg,

    /// DRM render node used by `--encoder vaapi` (and, once implemented,
    /// `--compositor gl`, which will default to the same node so both share
    /// a GPU).
    #[arg(long, default_value = "/dev/dri/renderD128")]
    pub vaapi_device: String,

    /// Disable PipeWire audio capture. The `/client` WebSocket endpoint will
    /// close immediately with "audio capture not available" when this is set.
    #[arg(long)]
    pub no_audio: bool,

    /// Command to run as the session's client app, e.g.
    /// `waylandwebstream -- foot -e vim`. Everything after `--` is passed
    /// through verbatim as the program and its arguments. The session is
    /// lazy: this command isn't started until the first browser connection
    /// arrives, so an idle server with nobody watching never runs it. If
    /// omitted, no child process is spawned -- Wayland clients can still be
    /// launched manually against `--display-name` as before.
    #[arg(last = true, value_name = "COMMAND")]
    pub command: Vec<String>,
}

impl Config {
    pub fn parse_resolution(res: &str) -> anyhow::Result<(u32, u32)> {
        let parts: Vec<&str> = res.split('x').collect();
        if parts.len() != 2 {
            anyhow::bail!("Invalid resolution format. Expected WIDTHxHEIGHT");
        }
        let width = parts[0].parse()?;
        let height = parts[1].parse()?;
        Ok((width, height))
    }

    pub fn get_initial_resolution(&self) -> anyhow::Result<(u32, u32)> {
        Self::parse_resolution(&self.initial_resolution)
    }
}

/// Sanitizes a client-requested output resolution before it drives encoder and
/// framebuffer allocation. Clamps each axis to `max` (the server is the
/// authority — the request arrives over the untrusted `/client` socket), rounds
/// down to even dimensions (YUV 4:2:0 requires them; ÷16 macroblock alignment
/// is not needed, x264 pads internally and signals the crop in the SPS), and
/// rejects anything below the 16×16 minimum. Returns `None` for a request that
/// is too small to use.
pub fn sanitize_resolution(req: (u32, u32), max: (u32, u32)) -> Option<(u32, u32)> {
    let width = req.0.min(max.0) & !1u32;
    let height = req.1.min(max.1) & !1u32;
    if width < 16 || height < 16 {
        return None;
    }
    Some((width, height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_trailing_command_leaves_it_empty() {
        let config = Config::parse_from(["waylandwebstream", "--port", "9999"]);
        assert!(config.command.is_empty());
        assert_eq!(config.port, 9999);
    }

    #[test]
    fn sanitize_resolution_clamps_to_max_and_rounds_even() {
        // Oversized request is clamped to max; odd max rounds down to even.
        assert_eq!(
            sanitize_resolution((9999, 9999), (1920, 1080)),
            Some((1920, 1080))
        );
        assert_eq!(
            sanitize_resolution((3841, 2161), (3840, 2160)),
            Some((3840, 2160))
        );
        // Within bounds but odd → rounded down.
        assert_eq!(
            sanitize_resolution((1281, 721), (3840, 2160)),
            Some((1280, 720))
        );
        // Below the 16×16 floor → rejected.
        assert_eq!(sanitize_resolution((8, 8), (3840, 2160)), None);
        // A tiny max can itself force the result below the floor → rejected.
        assert_eq!(sanitize_resolution((100, 100), (10, 10)), None);
    }

    #[test]
    fn trailing_command_with_hyphenated_args_is_captured_verbatim() {
        let config = Config::parse_from([
            "waylandwebstream",
            "--port",
            "9999",
            "--",
            "foot",
            "-e",
            "--some-flag",
            "value",
        ]);
        assert_eq!(config.port, 9999);
        assert_eq!(config.command, vec!["foot", "-e", "--some-flag", "value"]);
    }
}
