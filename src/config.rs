use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "waylandwebstream")]
#[command(about = "A headless Wayland compositor streaming to browsers via WebRTC", long_about = None)]
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

    /// STUN server URL
    #[arg(long, default_value = "stun:stun.l.google.com:19302")]
    pub stun: String,

    /// UDP port for the embedded TURN relay
    #[arg(long, default_value = "3478")]
    pub turn_port: u16,

    /// Public IP address clients should use to reach the embedded TURN relay
    /// (e.g. your netbird IP). Auto-detected if not set.
    #[arg(long)]
    pub turn_public_ip: Option<String>,

    /// Target framerate
    #[arg(long, default_value = "60")]
    pub framerate: u32,

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

    pub fn get_max_resolution(&self) -> anyhow::Result<(u32, u32)> {
        Self::parse_resolution(&self.max_resolution)
    }
}
