use anyhow::Result;
use std::sync::{Arc, Mutex, mpsc};

fn lock<T>(mutex: &Mutex<T>) -> Result<std::sync::MutexGuard<'_, T>> {
    mutex.lock().map_err(|_| anyhow::anyhow!("lock poisoned"))
}

pub enum Command {
    ToggleCast,
    OpenSettings,
    Quit,
}

pub struct App {
    config: Arc<Mutex<crate::config::Config>>,
    cast: Option<(crate::upnp::UpnpServer, crate::ssdp::SsdpServer)>,
    settings: Option<crate::web::SettingsServer>,
    state: Arc<Mutex<crate::state::RendererState>>,
    player: Option<Arc<Mutex<Box<dyn crate::player::PlayerBackend>>>>,
}

impl App {
    pub fn new(config: crate::config::Config) -> Result<Self> {
        let state = crate::state::RendererState {
            ..Default::default()
        };
        Ok(Self {
            config: Arc::new(Mutex::new(config)),
            cast: None,
            settings: None,
            state: Arc::new(Mutex::new(state)),
            player: None,
        })
    }

    pub fn run(self) -> Result<()> {
        let (sender, receiver) = mpsc::channel();
        let app_thread = std::thread::Builder::new()
            .name("mini-mdr-app".into())
            .spawn(move || self.command_loop(receiver))?;
        let quit_sender = sender.clone();

        let tray_result = crate::tray::run(sender);
        if let Err(error) = &tray_result {
            crate::log_error!("tray event loop failed: {error:#}");
        }
        // The tray loop owns the primary sender. Guarantee the application
        // thread observes a quit even when the tray exits without sending one
        // (for example after an error), so joining it cannot hang.
        let _ = quit_sender.send(Command::Quit);
        let app_result = app_thread
            .join()
            .map_err(|_| anyhow::anyhow!("application thread panicked"))?;
        tray_result?;
        app_result
    }

    fn command_loop(mut self, receiver: mpsc::Receiver<Command>) -> Result<()> {
        match self.toggle_cast() {
            Ok(()) => crate::log_info!("auto-started cast"),
            Err(error) => crate::log_error!("auto-starting cast: {error:#}"),
        }
        crate::tray::set_casting(self.cast.is_some());
        match self.ensure_settings() {
            Ok(()) => crate::log_info!("auto-started settings server"),
            Err(error) => crate::log_error!("auto-starting settings server: {error:#}"),
        }
        while let Ok(command) = receiver.recv() {
            let result = match command {
                Command::ToggleCast => self.toggle_cast(),
                Command::OpenSettings => self.open_settings(),
                Command::Quit => break,
            };
            if let Err(error) = result {
                crate::log_error!("application command failed: {error:#}");
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
        let config = lock(&self.config)?.clone();
        let player: Arc<Mutex<Box<dyn crate::player::PlayerBackend>>> =
            Arc::new(Mutex::new(crate::player::create_backend(
                &config.player.backend,
                &config.player.mpv_path,
                &config.player.vlc_path,
            )?));
        let upnp = crate::upnp::UpnpServer::start(
            &config.device.name,
            &config.device.udn,
            Arc::clone(&player),
            Arc::clone(&self.state),
            config.settings.max_history,
        )?;
        let ssdp = match crate::ssdp::SsdpServer::start(
            upnp.port(),
            &config.device.name,
            &config.device.udn,
        ) {
            Ok(server) => server,
            Err(error) => {
                drop(upnp);
                crate::tray::set_casting(false);
                return Err(error);
            }
        };
        self.player = Some(player);
        self.cast = Some((upnp, ssdp));
        let upnp_port = self.cast.as_ref().map(|(u, _)| u.port()).unwrap_or(0);
        let upnp_addr = crate::ssdp::local_ip().map(|ip| format!("http://{ip}:{upnp_port}"));
        {
            let mut state = lock(&self.state)?;
            state.cast = crate::state::CastState::Running;
            state.upnp_address = upnp_addr.clone();
        }
        crate::web::publish_status(true, upnp_addr);
        crate::tray::set_casting(true);
        crate::log_info!("cast started on port {upnp_port}");
        Ok(())
    }

    fn stop_cast(&mut self) -> Result<()> {
        // Close the protocol server before releasing the player so requests
        // already accepted by the server cannot outlive the Cast lifecycle.
        self.cast = None;
        if let Some(player) = &self.player
            && let Ok(mut player) = player.lock()
            && let Err(error) = player.stop()
        {
            crate::log_error!("stopping current media: {error:#}");
        }
        self.player = None;
        let mut state = lock(&self.state)?;
        state.cast = crate::state::CastState::Stopped;
        state.upnp_address = None;
        crate::web::publish_status(false, None);
        state.transport = crate::state::TransportState::Stopped;
        state.position = std::time::Duration::ZERO;
        state.uri = None;
        state.title = None;
        state.muted = false;
        crate::log_info!("cast stopped");
        crate::tray::set_casting(false);
        Ok(())
    }

    fn ensure_settings(&mut self) -> Result<()> {
        if self.settings.is_some() {
            return Ok(());
        }
        let config = Arc::clone(&self.config);
        let port = lock(&config)?.settings.port;
        self.settings = Some(crate::web::SettingsServer::start(
            port,
            config,
            Arc::clone(&self.state),
        )?);
        Ok(())
    }

    fn open_settings(&mut self) -> Result<()> {
        self.ensure_settings()?;
        let address = self
            .settings
            .as_ref()
            .map(|server| server.address)
            .ok_or_else(|| anyhow::anyhow!("settings server did not start"))?;
        open::that(format!("http://{address}/"))?;
        crate::log_info!("opened settings in browser");
        Ok(())
    }
}
