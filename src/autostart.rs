use anyhow::{Context, Result};
use std::path::PathBuf;

/// Registers the current executable to launch when the user logs in.
pub fn enable() -> Result<()> {
    #[cfg(windows)]
    {
        return windows::enable();
    }
    #[cfg(target_os = "macos")]
    {
        return macos::enable();
    }
    #[cfg(target_os = "linux")]
    {
        return linux::enable();
    }
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        anyhow::bail!("auto-start is not supported on this platform")
    }
}

/// Removes the auto-start registration, if present.
pub fn disable() -> Result<()> {
    #[cfg(windows)]
    {
        return windows::disable();
    }
    #[cfg(target_os = "macos")]
    {
        return macos::disable();
    }
    #[cfg(target_os = "linux")]
    {
        return linux::disable();
    }
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        Ok(())
    }
}

/// Returns whether the current executable is registered for auto-start.
pub fn is_enabled() -> bool {
    #[cfg(windows)]
    {
        return windows::is_enabled();
    }
    #[cfg(target_os = "macos")]
    {
        return macos::is_enabled();
    }
    #[cfg(target_os = "linux")]
    {
        return linux::is_enabled();
    }
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        false
    }
}

fn current_exe() -> Result<PathBuf> {
    std::env::current_exe().context("determining the current executable path")
}

#[cfg(windows)]
mod windows {
    use super::*;
    use winreg::enums::*;
    use winreg::RegKey;

    const REG_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const REG_VALUE: &str = "mini-mdr";

    fn expected_value() -> String {
        format!("\"{}\"", current_exe().map(|p| p.display().to_string()).unwrap_or_default())
    }

    pub fn enable() -> Result<()> {
        let path = current_exe()?;
        let value = format!("\"{}\"", path.display());
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let (key, _) = hkcu.create_subkey(REG_KEY)?;
        key.set_value(REG_VALUE, &value)?;
        Ok(())
    }

    pub fn disable() -> Result<()> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        if let Ok(key) = hkcu.open_subkey(REG_KEY) {
            let _ = key.delete_value(REG_VALUE);
        }
        Ok(())
    }

    pub fn is_enabled() -> bool {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        match hkcu
            .open_subkey(REG_KEY)
            .and_then(|k| k.get_value::<String, _>(REG_VALUE))
        {
            Ok(value) => value == expected_value(),
            Err(_) => false,
        }
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use std::fs;

    fn plist_path() -> Option<PathBuf> {
        let home = std::env::var("HOME").ok()?;
        Some(
            PathBuf::from(home)
                .join("Library")
                .join("LaunchAgents")
                .join("com.mini-mdr.app.plist"),
        )
    }

    fn plist_contents(exe: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.mini-mdr.app</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>ProcessType</key>
    <string>Interactive</string>
</dict>
</plist>
"#
        )
    }

    pub fn enable() -> Result<()> {
        let exe = current_exe()?.display().to_string();
        let path = plist_path().context("could not resolve the LaunchAgents directory")?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, plist_contents(&exe))?;
        Ok(())
    }

    pub fn disable() -> Result<()> {
        if let Some(path) = plist_path() {
            let _ = fs::remove_file(path);
        }
        Ok(())
    }

    pub fn is_enabled() -> bool {
        plist_path().map(|p| p.exists()).unwrap_or(false)
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn desktop_path() -> Option<PathBuf> {
        let config = directories::BaseDirs::new()?.config_dir().to_path_buf();
        Some(config.join("autostart").join("mini-mdr.desktop"))
    }

    fn desktop_contents(exe: &str) -> String {
        format!(
            "[Desktop Entry]\nType=Application\nName=mini-mdr\nExec=\"{exe}\"\nX-GNOME-Autostart-enabled=true\nHidden=false\n"
        )
    }

    pub fn enable() -> Result<()> {
        let exe = current_exe()?.display().to_string();
        let path = desktop_path().context("could not resolve the autostart directory")?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, desktop_contents(&exe))?;
        #[cfg(unix)]
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o755));
        Ok(())
    }

    pub fn disable() -> Result<()> {
        if let Some(path) = desktop_path() {
            let _ = fs::remove_file(path);
        }
        Ok(())
    }

    pub fn is_enabled() -> bool {
        desktop_path().map(|p| p.exists()).unwrap_or(false)
    }
}
