use anyhow::Result;
use std::sync::{Arc, Mutex, mpsc};

pub enum Command {
    ToggleCast,
    OpenSettings,
    Quit,
}

pub struct App {
    config: Arc<Mutex<crate::config::Config>>,
    cast: Option<(crate::upnp::UpnpServer, crate::ssdp::SsdpServer)>,
    settings: Option<crate::settings_server::SettingsServer>,
    state: Arc<Mutex<crate::state::RendererState>>,
    player: Option<Arc<Mutex<Box<dyn crate::player::PlayerBackend>>>>,
}

impl App {
    pub fn new(config: crate::config::Config) -> Result<Self> {
        Ok(Self {
            config: Arc::new(Mutex::new(config)),
            cast: None,
            settings: None,
            state: Arc::new(Mutex::new(crate::state::RendererState::default())),
            player: None,
        })
    }

    pub fn run(self) -> Result<()> {
        let (sender, receiver) = mpsc::channel();
        let app_thread = std::thread::Builder::new()
            .name("mini-mdr-app".into())
            .spawn(move || self.command_loop(receiver))?;

        let tray_result = crate::tray::run(sender);
        let app_result = app_thread
            .join()
            .map_err(|_| anyhow::anyhow!("application thread panicked"))?;
        tray_result?;
        app_result
    }

    fn command_loop(mut self, receiver: mpsc::Receiver<Command>) -> Result<()> {
        while let Ok(command) = receiver.recv() {
            let result = match command {
                Command::ToggleCast => self.toggle_cast(),
                Command::OpenSettings => self.open_settings(),
                Command::Quit => break,
            };
            if let Err(error) = result {
                eprintln!("application command failed: {error:#}");
            }
        }
        self.stop_cast()?;
        drop(self.settings.take());
        Ok(())
    }

    fn toggle_cast(&mut self) -> Result<()> {
        if self.cast.is_some() {
            return self.stop_cast();
        }
        let config = self
            .config
            .lock()
            .map_err(|_| anyhow::anyhow!("configuration lock poisoned"))?
            .clone();
        let player: Arc<Mutex<Box<dyn crate::player::PlayerBackend>>> = Arc::new(Mutex::new(crate::player::create_backend(
            &config.player.backend,
            &config.player.mpv_path,
        )?));
        let upnp = crate::upnp::UpnpServer::start(
            &config.device.name,
            Arc::clone(&player),
            Arc::clone(&self.state),
        )?;
        let ssdp = match crate::ssdp::SsdpServer::start(upnp.port(), &config.device.name) {
            Ok(server) => server,
            Err(error) => {
                drop(upnp);
                return Err(error);
            }
        };
        self.player = Some(player);
        self.cast = Some((upnp, ssdp));
        self.state
            .lock()
            .map_err(|_| anyhow::anyhow!("renderer state lock poisoned"))?
            .cast = crate::state::CastState::Running;
        Ok(())
    }

    fn stop_cast(&mut self) -> Result<()> {
        if let Some(player) = &self.player {
            if let Ok(mut player) = player.lock() {
                if let Err(error) = player.stop() {
                    eprintln!("stopping current media: {error:#}");
                }
            }
        }
        self.cast = None;
        self.player = None;
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("renderer state lock poisoned"))?;
        state.cast = crate::state::CastState::Stopped;
        state.transport = crate::state::TransportState::Stopped;
        state.position = std::time::Duration::ZERO;
        Ok(())
    }

    fn open_settings(&mut self) -> Result<()> {
        if self.settings.is_none() {
            let config = Arc::clone(&self.config);
            let port = config
                .lock()
                .map_err(|_| anyhow::anyhow!("configuration lock poisoned"))?
                .settings
                .port;
            self.settings = Some(crate::settings_server::SettingsServer::start(
                port,
                config,
                Arc::clone(&self.state),
            )?);
        }
        let address = self
            .settings
            .as_ref()
            .map(|server| server.address)
            .ok_or_else(|| anyhow::anyhow!("settings server did not start"))?;
        open::that(format!("http://{address}/"))?;
        Ok(())
    }
}
