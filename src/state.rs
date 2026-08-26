use serde::{Deserialize, Serialize};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CastState {
    Stopped,
    Running,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportState {
    NoMediaPresent,
    Stopped,
    Playing,
    PausedPlayback,
    Transitioning,
}

impl TransportState {
    pub fn upnp_value(self) -> &'static str {
        match self {
            Self::NoMediaPresent => "NO_MEDIA_PRESENT",
            Self::Stopped => "STOPPED",
            Self::Playing => "PLAYING",
            Self::PausedPlayback => "PAUSED_PLAYBACK",
            Self::Transitioning => "TRANSITIONING",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub timestamp: u64,
    pub uri: String,
    pub title: Option<String>,
}

impl HistoryEntry {
    pub fn new(uri: String, title: Option<String>) -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Self {
            timestamp,
            uri,
            title,
        }
    }

    pub fn time_str(&self) -> String {
        let datetime = UNIX_EPOCH + Duration::from_secs(self.timestamp);
        let secs_since_midnight = datetime
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            % 86400;
        let hours = secs_since_midnight / 3600;
        let minutes = (secs_since_midnight % 3600) / 60;
        format!("{hours:02}:{minutes:02}")
    }
}

#[derive(Clone, Debug)]
pub struct RendererState {
    pub cast: CastState,
    pub uri: Option<String>,
    pub title: Option<String>,
    pub transport: TransportState,
    pub duration: Option<Duration>,
    pub position: Duration,
    pub volume: u8,
    pub muted: bool,
    pub history: Vec<HistoryEntry>,
}

impl Default for RendererState {
    fn default() -> Self {
        Self {
            cast: CastState::Stopped,
            uri: None,
            title: None,
            transport: TransportState::NoMediaPresent,
            duration: None,
            position: Duration::ZERO,
            volume: 100,
            muted: false,
            history: Vec::new(),
        }
    }
}
