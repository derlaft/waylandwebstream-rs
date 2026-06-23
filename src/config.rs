use clap::Parser;

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

    /// Video bitrate in bits per second (constant bitrate mode). Ignored if
    /// --crf is set.
    #[arg(long, default_value = "2000000")]
    pub bitrate: usize,

    /// Enable constant quality mode using this x264 CRF value (0-51, lower
    /// is higher quality/larger frames; 18-28 is a typical range) instead of
    /// targeting a constant bitrate. Frame size will vary with scene
    /// complexity since there is no bitrate cap in this mode.
    #[arg(long, value_parser = clap::value_parser!(u8).range(0..=51))]
    pub crf: Option<u8>,

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
