use crate::state::HistoryEntry;
use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::PathBuf,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub device: DeviceConfig,
    pub player: PlayerConfig,
    pub settings: SettingsConfig,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct DeviceConfig {
    pub name: String,
    #[serde(default)]
    pub udn: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct PlayerConfig {
    pub backend: String,
    pub mpv_path: String,
    pub vlc_path: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct SettingsConfig {
    pub port: u16,
    pub max_history: usize,
    pub language: String,
}

impl Default for DeviceConfig {
    fn default() -> Self {
        let hostname = std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("HOST"))
            .unwrap_or_else(|_| "localhost".into());
        Self {
            name: format!("mini-mdr({hostname})"),
            udn: new_udn(),
        }
    }
}

impl Default for PlayerConfig {
    fn default() -> Self {
        Self {
            backend: "mpv".into(),
            mpv_path: "mpv".into(),
            vlc_path: "vlc".into(),
        }
    }
}

impl Default for SettingsConfig {
    fn default() -> Self {
        Self {
            port: 7878,
            max_history: DEFAULT_MAX_HISTORY,
            language: String::new(),
        }
    }
}

static HISTORY_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();

/// Default number of history entries kept when `settings.max_history` is
/// missing from the configuration file.
pub const DEFAULT_MAX_HISTORY: usize = 200;
/// Upper bound for `settings.max_history`; larger values are clamped.
pub const MAX_HISTORY_LIMIT: usize = 10_000;

/// Clamps `max_history` to the supported range so that values from old or
/// hand-edited configuration files behave the same everywhere.
pub fn normalize_max_history(value: usize) -> usize {
    value.clamp(1, MAX_HISTORY_LIMIT)
}

impl Config {
    pub fn path() -> Result<PathBuf> {
        ProjectDirs::from("com", "mini-mdr", "mini-mdr")
            .map(|dirs| dirs.config_dir().join("config.toml"))
            .context("could not determine the configuration directory")
    }

    pub fn config_dir() -> Result<PathBuf> {
        ProjectDirs::from("com", "mini-mdr", "mini-mdr")
            .map(|dirs| dirs.config_dir().to_path_buf())
            .context("could not determine the configuration directory")
    }

    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        if !path.exists() {
            crate::log_warn!("no configuration file found, using defaults");
            let config = Self::default();
            if let Err(error) = config.save() {
                crate::log_warn!("could not save default configuration: {error:#}");
            }
            return Ok(config);
        }
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) => {
                crate::log_warn!(
                    "could not read configuration from {} ({error}), using defaults",
                    path.display()
                );
                return Ok(Self::default());
            }
        };
        let has_udn = text
            .parse::<toml::Value>()
            .ok()
            .and_then(|value| value.get("device").and_then(|device| device.get("udn")))
            .is_some();
        match toml::from_str::<Self>(&text) {
            Ok(mut config) => {
                // Normalize values that older or hand-edited files may carry,
                // then persist them once so disk and memory never disagree.
                let mut changed = false;
                if !has_udn || !valid_udn(&config.device.udn) {
                    config.device.udn = new_udn();
                    changed = true;
                }
                let normalized = normalize_max_history(config.settings.max_history);
                if config.settings.max_history != normalized {
                    crate::log_warn!(
                        "configuration max_history {} is out of range, clamping to {normalized}",
                        config.settings.max_history
                    );
                    config.settings.max_history = normalized;
                    changed = true;
                }
                if changed {
                    if let Err(error) = config.save() {
                        crate::log_warn!("could not persist normalized configuration: {error:#}");
                    }
                }
                crate::log_info!("configuration loaded from {}", path.display());
                Ok(config)
            }
            Err(error) => {
                crate::log_warn!("invalid configuration file ({error}), using defaults");
                let config = Self::default();
                if let Err(save_error) = config.save() {
                    crate::log_warn!("could not save fallback configuration: {save_error:#}");
                }
                Ok(config)
            }
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self).context("serializing configuration")?;
        replace_file(&path, text.as_bytes()).context("writing configuration")?;
        crate::log_info!("configuration saved");
        Ok(())
    }

    pub fn load_history() -> Vec<HistoryEntry> {
        let path = match Self::config_dir() {
            Ok(dir) => dir.join("history.json"),
            Err(_) => {
                crate::log_warn!("cannot determine config directory for history");
                return Vec::new();
            }
        };
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(_) => {
                crate::log_warn!("could not read history file");
                return Vec::new();
            }
        };
        match serde_json::from_str(&text) {
            Ok(entries) => entries,
            Err(_) => {
                crate::log_warn!("history file is corrupted, starting fresh");
                Vec::new()
            }
        }
    }

    pub fn save_history(history: &[HistoryEntry]) -> Result<()> {
        let dir = Self::config_dir()?;
        fs::create_dir_all(&dir)?;
        let path = dir.join("history.json");
        let text = serde_json::to_string_pretty(history).context("serializing history")?;
        replace_file(&path, text.as_bytes()).context("writing history")
    }

    pub fn append_history(entry: HistoryEntry, max: usize) -> Result<()> {
        let mutex = HISTORY_MUTEX.get_or_init(|| Mutex::new(()));
        let _guard = mutex
            .lock()
            .map_err(|_| anyhow::anyhow!("history lock poisoned"))?;
        let mut entries = Self::load_history();
        if max == 0 {
            return Self::save_history(&[]);
        }
        entries.push(entry);
        if entries.len() > max {
            let drop = entries.len() - max;
            entries.drain(0..drop);
        }
        Self::save_history(&entries)
    }

    /// Reads `settings.max_history` straight from the config file. Used by the
    /// settings SSE path where the in-memory `Config` is not available. Falls
    /// back to the default when the file is missing or invalid and normalizes
    /// the value exactly like `load` does, so it always matches the in-memory
    /// configuration.
    pub fn max_history_from_disk() -> usize {
        let Ok(path) = Self::path() else {
            return DEFAULT_MAX_HISTORY;
        };
        let Ok(text) = fs::read_to_string(path) else {
            return DEFAULT_MAX_HISTORY;
        };
        toml::from_str::<Self>(&text)
            .map(|config| normalize_max_history(config.settings.max_history))
            .unwrap_or(DEFAULT_MAX_HISTORY)
    }
}

fn new_udn() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("uuid:mini-mdr-{}-{nanos}", std::process::id())
}

fn valid_udn(value: &str) -> bool {
    value.starts_with("uuid:")
        && value.len() > 5
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'-'))
}

static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Atomically replaces `path` with `contents`.
///
/// The new content is written to a uniquely named temporary file in the same
/// directory and flushed to disk, then renamed over the target. `fs::rename`
/// replaces an existing destination on every supported platform (`rename` on
/// Unix, `MoveFileEx` with `MOVEFILE_REPLACE_EXISTING` on Windows), so the
/// original file is never removed before its replacement is fully written.
/// The temporary file is removed on every failure path.
fn replace_file(path: &std::path::Path, contents: &[u8]) -> Result<()> {
    let counter = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("tmp-{}-{counter}", std::process::id()));
    let result = write_and_sync(&tmp, contents).and_then(|()| fs::rename(&tmp, path));
    if let Err(error) = result {
        let _ = fs::remove_file(&tmp);
        return Err(error.into());
    }
    Ok(())
}

fn write_and_sync(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    let mut file = fs::File::create(path)?;
    file.write_all(contents)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_max_history_range() {
        assert_eq!(normalize_max_history(0), 1);
        assert_eq!(normalize_max_history(1), 1);
        assert_eq!(
            normalize_max_history(DEFAULT_MAX_HISTORY),
            DEFAULT_MAX_HISTORY
        );
        assert_eq!(normalize_max_history(MAX_HISTORY_LIMIT), MAX_HISTORY_LIMIT);
        assert_eq!(
            normalize_max_history(MAX_HISTORY_LIMIT + 1),
            MAX_HISTORY_LIMIT
        );
    }
}
