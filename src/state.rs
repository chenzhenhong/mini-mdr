use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CastState {
    Stopped,
    Running,
}

impl CastState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stopped => "已停止",
            Self::Running => "运行中",
        }
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
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoMediaPresent => "无媒体",
            Self::Stopped => "已停止",
            Self::Playing => "播放中",
            Self::PausedPlayback => "已暂停",
            Self::Transitioning => "加载中",
        }
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
