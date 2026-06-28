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

    /// Address the HTTP signaling server binds to. Defaults to all
    /// interfaces; set to 127.0.0.1 to only accept connections from a local
    /// reverse proxy (e.g. one that adds authentication) instead of exposing
    /// the server directly.
    #[arg(long, default_value = "0.0.0.0")]
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
    fn trailing_command_with_hyphenated_args_is_captured_verbatim() {
        let config = Config::parse_from([
            "waylandwebstream", "--port", "9999", "--", "foot", "-e", "--some-flag", "value",
        ]);
        assert_eq!(config.port, 9999);
        assert_eq!(
            config.command,
            vec!["foot", "-e", "--some-flag", "value"]
        );
    }
}
