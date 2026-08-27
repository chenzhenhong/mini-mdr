use crate::i18n::{Language, t};
use anyhow::Result;
use ldtray::{Event, Icon, Menu, MenuItem, Tray, TrayConfig, TrayHandle};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, mpsc};

pub const TOGGLE_CAST: u32 = 1;
pub const OPEN_SETTINGS: u32 = 2;
pub const QUIT: u32 = 4;

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

fn menu(casting: bool, lang: Language) -> Menu {
    let toggle = t(lang, "tray-cast");
    let settings = t(lang, "tray-more");
    let quit = t(lang, "tray-quit");
    Menu::new()
        .item(MenuItem::checkbox(TOGGLE_CAST, toggle, casting))
        .item(MenuItem::separator())
        .item(MenuItem::button(OPEN_SETTINGS, settings))
        .item(MenuItem::button(QUIT, quit))
}

pub fn run(sender: mpsc::Sender<crate::app::Command>) -> Result<()> {
    let icon = tray_icon()?;
    let lang = crate::i18n::lang();
    CASTING.store(true, Ordering::Relaxed);
    let tray = match Tray::new(
        TrayConfig::new(icon)
            .tooltip("mini-mdr")
            .menu(menu(true, lang)),
    ) {
        Ok(tray) => tray,
        Err(error) => {
            crate::log_error!("tray unavailable: {error}");
            return Ok(());
        }
    };
    let handle = tray.handle();
    let _ = TRAY_HANDLE.set(handle.clone());

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

    tray.run(move |event| {
        if quit_flag.load(Ordering::Relaxed) {
            let _ = sender.send(crate::app::Command::Quit);
            let _ = handle.quit();
            return;
        }
        if let Event::Menu(id) = event {
            let command = match id.0 {
                TOGGLE_CAST => crate::app::Command::ToggleCast,
                OPEN_SETTINGS => crate::app::Command::OpenSettings,
                QUIT => crate::app::Command::Quit,
                _ => return,
            };
            if let Err(error) = sender.send(command) {
                crate::log_error!("sending tray command: {error}");
            }
            if id.0 == TOGGLE_CAST {
                let casting = !CASTING.load(Ordering::Relaxed);
                CASTING.store(casting, Ordering::Relaxed);
                if let Err(error) = handle.set_menu(menu(casting, crate::i18n::lang())) {
                    crate::log_error!("updating tray menu: {error}");
                }
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
