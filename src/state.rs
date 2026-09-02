use serde::{Deserialize, Serialize};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

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

#[derive(Clone, Copy)]
struct LocalTime {
    year: i64,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
}

#[cfg(unix)]
fn local_time(timestamp: libc::time_t) -> Option<LocalTime> {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let result = unsafe { libc::localtime_r(&timestamp, &mut tm) };
    if result.is_null() {
        return None;
    }
    Some(LocalTime {
        year: i64::from(tm.tm_year) + 1900,
        month: (tm.tm_mon + 1) as u32,
        day: tm.tm_mday as u32,
        hour: tm.tm_hour as u32,
        minute: tm.tm_min as u32,
        second: tm.tm_sec as u32,
    })
}

#[cfg(windows)]
fn local_time(timestamp: libc::time_t) -> Option<LocalTime> {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_s(&mut tm, &timestamp) } != 0 {
        return None;
    }
    Some(LocalTime {
        year: i64::from(tm.tm_year) + 1900,
        month: (tm.tm_mon + 1) as u32,
        day: tm.tm_mday as u32,
        hour: tm.tm_hour as u32,
        minute: tm.tm_min as u32,
        second: tm.tm_sec as u32,
    })
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
        if let Ok(timestamp) = libc::time_t::try_from(self.timestamp)
            && let Some(time) = local_time(timestamp)
        {
            return format!(
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                time.year, time.month, time.day, time.hour, time.minute, time.second
            );
        }
        if self.timestamp > 253_402_300_799 {
            return self.timestamp.to_string();
        }
        // Fall back to UTC when the local-time conversion fails.
        let days = self.timestamp / 86400;
        let secs = self.timestamp % 86400;
        let hours = secs / 3600;
        let minutes = (secs % 3600) / 60;
        let seconds = secs % 60;
        let mut y = 1970i64;
        let mut remaining = days as i64;
        loop {
            let days_in_year = if is_leap(y) { 366 } else { 365 };
            if remaining < days_in_year {
                break;
            }
            remaining -= days_in_year;
            y += 1;
        }
        let leap = is_leap(y);
        let month_days: [i64; 12] = [
            31,
            if leap { 29 } else { 28 },
            31,
            30,
            31,
            30,
            31,
            31,
            30,
            31,
            30,
            31,
        ];
        let mut m = 1u32;
        for &md in &month_days {
            if remaining < md {
                break;
            }
            remaining -= md;
            m += 1;
        }
        let d = remaining + 1;
        format!("{y:04}-{m:02}-{d:02} {hours:02}:{minutes:02}:{seconds:02}")
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
    pub upnp_address: Option<String>,
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
            upnp_address: None,
        }
    }
}
