use crate::i18n::{Language, t};
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CastState {
    Stopped,
    Running,
}

impl CastState {
    pub fn as_str(self, lang: Language) -> String {
        let key = match self {
            Self::Stopped => "cast-stopped",
            Self::Running => "cast-running",
        };
        t(lang, key)
    }
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
    pub fn as_str(self, lang: Language) -> String {
        let key = match self {
            Self::NoMediaPresent => "transport-no-media",
            Self::Stopped => "transport-stopped",
            Self::Playing => "transport-playing",
            Self::PausedPlayback => "transport-paused",
            Self::Transitioning => "transport-loading",
        };
        t(lang, key)
    }

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
        }
    }
}
