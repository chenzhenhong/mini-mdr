// Hide the console window in release builds so launching the executable does
// not open a terminal; debug builds keep the console for development output.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod config;
mod i18n;
mod log;
mod player;
mod settings_server;
mod ssdp;
mod state;
mod tray;
mod upnp;
mod util;

use anyhow::Result;
use app::App;
use config::Config;

fn main() -> Result<()> {
    i18n::detect();
    let config = Config::load()?;
    i18n::set_lang(i18n::resolve_language(&config.settings.language));
    App::new(config)?.run()
}
