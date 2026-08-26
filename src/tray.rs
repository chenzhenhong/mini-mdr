use anyhow::Result;
use ldtray::{Event, Icon, Menu, MenuItem, Tray, TrayConfig};
use std::sync::mpsc::Sender;

pub const TOGGLE_CAST: u32 = 1;
pub const OPEN_SETTINGS: u32 = 2;
pub const QUIT: u32 = 3;

fn menu(casting: bool) -> Menu {
    Menu::new()
        .item(MenuItem::button(
            TOGGLE_CAST,
            if casting { "停止 Cast" } else { "开始 Cast" },
        ))
        .item(MenuItem::button(OPEN_SETTINGS, "打开设置"))
        .item(MenuItem::button(QUIT, "退出程序"))
}

pub fn run(sender: Sender<crate::app::Command>) -> Result<()> {
    let icon = Icon::from_rgba(16, 16, vec![40, 140, 220, 255].repeat(256))?;
    let tray = match Tray::new(TrayConfig::new(icon).tooltip("mini-mdr").menu(menu(false))) {
        Ok(tray) => tray,
        Err(error) => {
            eprintln!("tray unavailable: {error}");
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
                eprintln!("sending tray command: {error}");
            }
            if id.0 == TOGGLE_CAST {
                casting = !casting;
                if let Err(error) = handle.set_menu(menu(casting)) {
                    eprintln!("updating tray menu: {error}");
                }
            }
            if id.0 == QUIT {
                if let Err(error) = handle.quit() {
                    eprintln!("stopping tray event loop: {error}");
                }
            }
        }
    })?;
    Ok(())
}
