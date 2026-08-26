mod mpv;

use anyhow::{anyhow, Result};
use std::time::Duration;

pub use mpv::MpvBackend;

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
    fn load(&mut self, uri: &str) -> Result<()>;
    fn play(&mut self) -> Result<()>;
    fn pause(&mut self) -> Result<()>;
    fn stop(&mut self) -> Result<()>;
    fn seek(&mut self, position: Duration) -> Result<()>;
    fn set_volume(&mut self, volume: u8) -> Result<()>;
    fn set_mute(&mut self, muted: bool) -> Result<()>;
    fn status(&mut self) -> Result<PlayerStatus>;
}

pub fn create_backend(name: &str, mpv_path: &str) -> Result<Box<dyn PlayerBackend>> {
    match name {
        "mpv" => Ok(Box::new(MpvBackend::new(mpv_path)?)),
        other => Err(anyhow!("unsupported player backend: {other}")),
    }
}
