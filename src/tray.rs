use crate::i18n::{Language, t};
use anyhow::Result;
use ldtray::{Event, Icon, Menu, MenuItem, Notification, Tray, TrayConfig, TrayHandle};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

pub const TOGGLE_CAST: u32 = 1;
pub const OPEN_SETTINGS: u32 = 2;
pub const QUIT: u32 = 4;
pub const OPEN_LOG_DIR: u32 = 8;

const TRAY_ICON_SIZE: u32 = 32;
const TRAY_ICON_RGBA: &[u8; (TRAY_ICON_SIZE * TRAY_ICON_SIZE * 4) as usize] =
    include_bytes!("../resources/icon.rgba");

static TRAY_HANDLE: OnceLock<TrayHandle> = OnceLock::new();
static CASTING: AtomicBool = AtomicBool::new(true);

pub fn refresh_menu() {
    if let Some(handle) = TRAY_HANDLE.get() {
        let lang = crate::i18n::lang();
        let casting = CASTING.load(Ordering::Relaxed);
        if let Err(error) = handle.set_menu(menu(casting, lang)) {
            crate::log_error!("refreshing tray menu: {error}");
        }
    }
}

/// Records the authoritative cast state reported by the application and
/// refreshes the tray menu so the Start/Stop Cast checkbox cannot desync from
/// the actual DMR state after a failed toggle or auto-start.
pub fn set_casting(casting: bool) {
    CASTING.store(casting, Ordering::Relaxed);
    refresh_menu();
}

fn menu(casting: bool, lang: Language) -> Menu {
    let toggle = t(lang, "tray-cast");
    let quit = t(lang, "tray-quit");
    let more = t(lang, "tray-more");
    let open_settings = t(lang, "tray-open-settings");
    let open_log_dir = t(lang, "tray-open-log-dir");
    Menu::new()
        .item(MenuItem::checkbox(TOGGLE_CAST, toggle, casting))
        .item(MenuItem::separator())
        .item(MenuItem::submenu(
            more,
            [
                MenuItem::button(OPEN_SETTINGS, open_settings),
                MenuItem::button(OPEN_LOG_DIR, open_log_dir),
            ],
        ))
        .item(MenuItem::button(QUIT, quit))
}

pub fn run(sender: mpsc::Sender<crate::app::Command>) -> Result<()> {
    let icon = tray_icon()?;
    let lang = crate::i18n::lang();
    // Honor any cast state the application has already published (for example
    // when auto-start failed before the tray finished initializing) instead of
    // forcing the checkbox on.
    let casting = CASTING.load(Ordering::Relaxed);
    let tray = match Tray::new(
        TrayConfig::new(icon)
            .tooltip("mini-mdr")
            .menu(menu(casting, lang)),
    ) {
        Ok(tray) => tray,
        Err(error) => {
            crate::log_error!("tray unavailable: {error}");
            return Ok(());
        }
    };
    let handle = tray.handle();
    if TRAY_HANDLE.set(handle.clone()).is_err() {
        crate::log_warn!("tray handle was already registered");
    }

    let quit_flag = Arc::new(AtomicBool::new(false));
    if signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&quit_flag)).is_err() {
        crate::log_warn!("failed to register SIGINT handler");
    }
    #[cfg(unix)]
    if signal_hook::flag::register(signal_hook::consts::SIGHUP, Arc::clone(&quit_flag)).is_err() {
        crate::log_warn!("failed to register SIGHUP handler");
    }
    #[cfg(unix)]
    if signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&quit_flag)).is_err() {
        crate::log_warn!("failed to register SIGTERM handler");
    }
    crate::log_info!("tray initialized");

    // The tray event loop only invokes the callback on tray events, so a
    // signal that only flips `quit_flag` would never be noticed. A watcher
    // thread reacts to the flag and breaks the loop via `handle.quit()`.
    let watcher_handle = handle.clone();
    let watcher_sender = sender.clone();
    let watcher_flag = Arc::clone(&quit_flag);
    std::thread::spawn(move || {
        while !watcher_flag.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(100));
        }
        if let Err(error) = watcher_sender.send(crate::app::Command::Quit) {
            crate::log_error!("sending quit command from signal watcher: {error}");
        }
        if let Err(error) = watcher_handle.quit() {
            crate::log_error!("stopping tray event loop from signal watcher: {error}");
        }
    });

    tray.run(move |event| {
        if quit_flag.load(Ordering::Relaxed) {
            let _ = sender.send(crate::app::Command::Quit);
            let _ = handle.quit();
            return;
        }
        if let Event::Menu(id) = event {
            let command = match id.0 {
                TOGGLE_CAST => Some(crate::app::Command::ToggleCast),
                OPEN_SETTINGS => Some(crate::app::Command::OpenSettings),
                OPEN_LOG_DIR => {
                    match crate::config::Config::config_dir() {
                        Ok(dir) => {
                            if let Err(error) = open::that(&dir) {
                                crate::log_error!(
                                    "opening config directory '{}': {error}",
                                    dir.display()
                                );
                            }
                        }
                        Err(error) => {
                            crate::log_error!("resolving config directory: {error}");
                        }
                    }
                    None
                }
                QUIT => Some(crate::app::Command::Quit),
                _ => None,
            };
            if let Some(cmd) = command
                && let Err(error) = sender.send(cmd)
            {
                crate::log_error!("sending tray command: {error}");
            }
            if id.0 == QUIT
                && let Err(error) = handle.quit()
            {
                crate::log_error!("stopping tray event loop: {error}");
            }
        }
    })?;
    Ok(())
}

fn tray_icon() -> Result<Icon> {
    Ok(Icon::from_rgba(
        TRAY_ICON_SIZE,
        TRAY_ICON_SIZE,
        TRAY_ICON_RGBA.to_vec(),
    )?)
}

static LAST_NOTIFY: OnceLock<Mutex<Instant>> = OnceLock::new();

pub fn notify_error(message: &str) {
    let now = Instant::now();
    let last = LAST_NOTIFY
        .get_or_init(|| Mutex::new(now))
        .lock()
        .map(|mut guard| {
            if now.duration_since(*guard) < Duration::from_secs(1) {
                return true;
            }
            *guard = now;
            false
        })
        .unwrap_or(false);
    if last {
        return;
    }
    if let Some(handle) = TRAY_HANDLE.get() {
        let icon = match Icon::from_rgba(TRAY_ICON_SIZE, TRAY_ICON_SIZE, TRAY_ICON_RGBA.to_vec()) {
            Ok(icon) => icon,
            Err(error) => {
                // log_warn, not log_error: ERROR-level logging re-enters
                // notify_error and would recurse.
                crate::log_warn!("building tray notification icon: {error}");
                return;
            }
        };
        if let Err(error) = handle.notify(Notification::new("mini-mdr", message).with_icon(icon)) {
            crate::log_warn!("showing tray notification: {error}");
        }
    }
}
