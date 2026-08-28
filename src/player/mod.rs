mod mpv;
mod vlc;

use anyhow::{Result, anyhow};
use std::time::Duration;

pub use mpv::MpvBackend;
pub use vlc::VlcBackend;

#[derive(Clone, Debug)]
pub struct PlayerStatus {
    pub playing: bool,
    pub paused: bool,
    pub position: Duration,
    pub duration: Option<Duration>,
    pub volume: u8,
    pub muted: bool,
}

impl Default for PlayerStatus {
    fn default() -> Self {
        Self {
            playing: false,
            paused: false,
            position: Duration::ZERO,
            duration: None,
            volume: 100,
            muted: false,
        }
    }
}

pub trait PlayerBackend: Send {
    fn load(&mut self, uri: &str, title: Option<&str>) -> Result<()>;
    fn play(&mut self) -> Result<()>;
    fn pause(&mut self) -> Result<()>;
    fn stop(&mut self) -> Result<()>;
    fn seek(&mut self, position: Duration) -> Result<()>;
    fn set_volume(&mut self, volume: u8) -> Result<()>;
    fn set_mute(&mut self, muted: bool) -> Result<()>;
    fn status(&mut self) -> Result<PlayerStatus>;
    fn sink_protocol_info(&self) -> &str;
}

pub fn create_backend(
    name: &str,
    mpv_path: &str,
    vlc_path: &str,
) -> Result<Box<dyn PlayerBackend>> {
    match name {
        "mpv" => Ok(Box::new(MpvBackend::new(mpv_path)?)),
        "vlc" => Ok(Box::new(VlcBackend::new(vlc_path)?)),
        other => Err(anyhow!("unsupported player backend: {other}")),
    }
}

pub fn is_program_not_found(error: &anyhow::Error) -> bool {
    for cause in error.chain() {
        if let Some(io_err) = cause.downcast_ref::<std::io::Error>() {
            if io_err.kind() == std::io::ErrorKind::NotFound {
                return true;
            }
            #[cfg(windows)]
            if io_err.raw_os_error() == Some(2) {
                return true;
            }
        }
    }
    false
}
