mod app;
mod config;
mod player;
mod settings_server;
mod ssdp;
mod state;
mod tray;
mod upnp;

use anyhow::Result;
use app::App;
use config::Config;

fn main() -> Result<()> {
    let config = Config::load()?;
    App::new(config)?.run()
}
