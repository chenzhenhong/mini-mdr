use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
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
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct PlayerConfig {
    pub backend: String,
    pub mpv_path: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct SettingsConfig {
    pub port: u16,
    pub max_history: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            device: DeviceConfig::default(),
            player: PlayerConfig {
                backend: "mpv".into(),
                mpv_path: "mpv".into(),
            },
            settings: SettingsConfig::default(),
        }
    }
}

impl Default for DeviceConfig {
    fn default() -> Self {
        let hostname = std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("HOST"))
            .unwrap_or_else(|_| "localhost".into());
        Self {
            name: format!("mini-mdr({hostname})"),
        }
    }
}

impl Default for PlayerConfig {
    fn default() -> Self {
        Self {
            backend: "mpv".into(),
            mpv_path: "mpv".into(),
        }
    }
}

impl Default for SettingsConfig {
    fn default() -> Self {
        Self {
            port: 7878,
            max_history: 200,
        }
    }
}

impl Config {
    pub fn path() -> Result<PathBuf> {
        ProjectDirs::from("com", "mini-mdr", "mini-mdr")
            .map(|dirs| dirs.config_dir().join("config.toml"))
            .context("could not determine the configuration directory")
    }

    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).context("parsing configuration")
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self).context("serializing configuration")?;
        fs::write(path, text).context("writing configuration")
    }
}
