use anyhow::Result;
use image::{ImageFormat, imageops::FilterType};
use ldtray::{Event, Icon, Menu, MenuItem, Tray, TrayConfig};
use std::sync::mpsc::Sender;

pub const TOGGLE_CAST: u32 = 1;
pub const OPEN_SETTINGS: u32 = 2;
pub const QUIT: u32 = 3;

const TRAY_ICON: &[u8] = include_bytes!("../resources/icon.png");
const TRAY_ICON_SIZE: u32 = 32;

fn menu(casting: bool) -> Menu {
    Menu::new()
        .item(MenuItem::button(
            TOGGLE_CAST,
            if casting {
                "停止 Cast"
            } else {
                "开始 Cast"
            },
        ))
        .item(MenuItem::button(OPEN_SETTINGS, "打开设置"))
        .item(MenuItem::button(QUIT, "退出程序"))
}

pub fn run(sender: Sender<crate::app::Command>) -> Result<()> {
    let icon = tray_icon()?;
    let tray = match Tray::new(TrayConfig::new(icon).tooltip("mini-mdr").menu(menu(false))) {
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
                if let Err(error) = handle.set_menu(menu(casting)) {
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
    let source = image::load_from_memory_with_format(TRAY_ICON, ImageFormat::Png)?.to_rgba8();
    let rgba = image::imageops::resize(
        &source,
        TRAY_ICON_SIZE,
        TRAY_ICON_SIZE,
        FilterType::Lanczos3,
    );
    Ok(Icon::from_rgba(
        TRAY_ICON_SIZE,
        TRAY_ICON_SIZE,
        rgba.into_raw(),
    )?)
}
