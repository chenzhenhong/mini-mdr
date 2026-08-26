use crate::i18n::{Language, s};
use anyhow::Result;
use ldtray::{Event, Icon, Menu, MenuItem, Tray, TrayConfig};
use std::sync::mpsc::Sender;

pub const TOGGLE_CAST: u32 = 1;
pub const OPEN_SETTINGS: u32 = 2;
pub const QUIT: u32 = 3;

const TRAY_ICON_SIZE: u32 = 32;
const TRAY_ICON_RGBA: &[u8; (TRAY_ICON_SIZE * TRAY_ICON_SIZE * 4) as usize] =
    include_bytes!("../resources/icon.rgba");

fn menu(casting: bool, lang: Language) -> Menu {
    let toggle = s!(
        lang,
        if casting {
            "停止 Cast"
        } else {
            "开始 Cast"
        },
        if casting { "Stop Cast" } else { "Start Cast" }
    );
    let settings = s!(lang, "打开设置", "Open Settings");
    let quit = s!(lang, "退出程序", "Quit");
    Menu::new()
        .item(MenuItem::button(TOGGLE_CAST, toggle))
        .item(MenuItem::button(OPEN_SETTINGS, settings))
        .item(MenuItem::button(QUIT, quit))
}

pub fn run(sender: Sender<crate::app::Command>) -> Result<()> {
    let icon = tray_icon()?;
    let lang = crate::i18n::lang();
    let tray = match Tray::new(
        TrayConfig::new(icon)
            .tooltip("mini-mdr")
            .menu(menu(false, lang)),
    ) {
        Ok(tray) => tray,
        Err(error) => {
            crate::log_error!("tray unavailable: {error}");
            return Ok(());
        }
    };
    let handle = tray.handle();
    let mut casting = false;
    tray.run(move |event| {
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
                casting = !casting;
                if let Err(error) = handle.set_menu(menu(casting, lang)) {
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
