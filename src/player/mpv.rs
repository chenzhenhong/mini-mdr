use super::{PlayerBackend, PlayerStatus};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver, RecvTimeoutError},
    thread,
    time::{Duration, Instant},
};

const IPC_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
const EXIT_WAIT_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(windows)]
type PlatformStream = std::fs::File;
#[cfg(unix)]
type PlatformStream = std::os::unix::net::UnixStream;

pub struct MpvBackend {
    executable: String,
    session: Option<MpvSession>,
    failed_permanently: bool,
}

struct MpvSession {
    child: Child,
    writer: PlatformStream,
    receiver: Receiver<Value>,
    next_request: u64,
    #[cfg(unix)]
    socket_path: std::path::PathBuf,
}

impl MpvBackend {
    pub fn new(path: &str) -> Result<Self> {
        if path.trim().is_empty() {
            anyhow::bail!("mpv executable path cannot be empty");
        }
        Ok(Self {
            executable: path.to_owned(),
            session: None,
            failed_permanently: false,
        })
    }

    fn session(&mut self) -> Result<&mut MpvSession> {
        if self.failed_permanently {
            return Err(super::already_reported(anyhow::anyhow!(
                "mpv not found, check player.mpv_path in config"
            )));
        }
        if self.session.is_none() {
            match MpvSession::start(&self.executable) {
                Ok(session) => {
                    self.session = Some(session);
                    crate::log_info!("mpv process started");
                }
                Err(error) => {
                    if super::is_program_not_found(&error) {
                        self.failed_permanently = true;
                        crate::log_error!("mpv not found at '{}', will not retry", self.executable);
                    }
                    return Err(super::already_reported(error));
                }
            }
        }
        self.session.as_mut().context("mpv session did not start")
    }

    fn command(&mut self, command: Value) -> Result<Value> {
        match self.session()?.command(command.clone()) {
            Ok(value) => Ok(value),
            Err(error) => {
                let msg = error.to_string();
                let is_pipe_error = msg.contains("Broken pipe")
                    || msg.contains("os error 32")
                    || msg.contains("os error 232")
                    || msg.contains("IPC closed");
                if is_pipe_error {
                    crate::log_warn!("mpv process lost, restarting...");
                    self.session = None;
                    self.session()?.command(command)
                } else {
                    Err(error)
                }
            }
        }
    }
}

impl MpvSession {
    fn start(executable: &str) -> Result<Self> {
        let endpoint = ipc_endpoint();
        let mut child = Command::new(executable)
            .args([
                "--idle=yes",
                "--force-window=immediate",
                "--no-terminal",
                "--keep-open=yes",
                "--fullscreen",
                &format!("--input-ipc-server={}", endpoint.to_string_lossy()),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("starting mpv from {executable}"))?;

        let started = Instant::now();
        let writer = loop {
            match connect_ipc(&endpoint) {
                Ok(stream) => break stream,
                Err(error) => {
                    if let Some(status) = child.try_wait()? {
                        anyhow::bail!("mpv exited before opening IPC (status: {status})");
                    }
                    if started.elapsed() >= Duration::from_secs(5) {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(error).context("timed out waiting for mpv JSON IPC");
                    }
                    thread::sleep(Duration::from_millis(40));
                }
            }
        };
        let reader_stream = clone_stream(&writer)?;
        let (sender, receiver) = mpsc::channel();
        thread::Builder::new()
            .name("mpv-ipc-reader".into())
            .spawn(move || {
                let mut reader = BufReader::new(reader_stream);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => match serde_json::from_str::<Value>(&line) {
                            Ok(value) => {
                                if sender.send(value).is_err() {
                                    break;
                                }
                            }
                            Err(_) => continue,
                        },
                    }
                }
            })?;
        Ok(Self {
            child,
            writer,
            receiver,
            next_request: 1,
            #[cfg(unix)]
            socket_path: endpoint,
        })
    }

    fn command(&mut self, command: Value) -> Result<Value> {
        let value = self.raw_command(command)?;
        if value
            .get("error")
            .and_then(Value::as_str)
            .is_some_and(|error| error != "success")
        {
            anyhow::bail!("mpv command failed: {}", value["error"]);
        }
        Ok(value)
    }

    fn raw_command(&mut self, command: Value) -> Result<Value> {
        self.raw_command_until(Instant::now() + IPC_RESPONSE_TIMEOUT, command)
    }

    fn raw_command_until(&mut self, deadline: Instant, mut command: Value) -> Result<Value> {
        let id = self.next_request;
        self.next_request += 1;
        command["request_id"] = json!(id);
        serde_json::to_writer(&mut self.writer, &command)?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()?;

        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                anyhow::bail!("timed out waiting for mpv IPC response");
            }
            match self.receiver.recv_timeout(remaining) {
                Ok(value) => {
                    if value.get("request_id") == Some(&json!(id)) {
                        return Ok(value);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    anyhow::bail!("timed out waiting for mpv IPC response");
                }
                Err(RecvTimeoutError::Disconnected) => {
                    anyhow::bail!("mpv IPC closed while waiting for response");
                }
            }
        }
    }
}

impl PlayerBackend for MpvBackend {
    fn load(&mut self, uri: &str, title: Option<&str>) -> Result<()> {
        let mut command = json!({"command": ["loadfile", uri, "replace"]});
        if let Some(title) = title {
            command["options"] = json!({"force-media-title": title});
        }
        self.command(command)?;
        if let Some(title) = title {
            if let Err(error) = self.command(json!({"command": ["set_property", "title", title]})) {
                crate::log_warn!("setting mpv window title failed: {:#}", error);
            }
        }
        Ok(())
    }

    fn play(&mut self) -> Result<()> {
        if self.session.is_none() {
            return Ok(());
        }
        self.command(json!({"command": ["set_property", "pause", false]}))?;
        Ok(())
    }

    fn pause(&mut self) -> Result<()> {
        if self.session.is_none() {
            return Ok(());
        }
        self.command(json!({"command": ["set_property", "pause", true]}))?;
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        if let Some(session) = self.session.take() {
            drop(session);
        }
        Ok(())
    }

    fn seek(&mut self, position: Duration) -> Result<()> {
        if self.session.is_none() {
            return Ok(());
        }
        self.command(json!({"command": ["seek", position.as_secs_f64(), "absolute", "exact"]}))?;
        Ok(())
    }

    fn set_volume(&mut self, volume: u8) -> Result<()> {
        if self.session.is_none() {
            return Ok(());
        }
        self.command(json!({"command": ["set_property", "volume", volume.min(100)]}))?;
        Ok(())
    }

    fn set_mute(&mut self, muted: bool) -> Result<()> {
        if self.session.is_none() {
            return Ok(());
        }
        self.command(json!({"command": ["set_property", "mute", muted]}))?;
        Ok(())
    }

    fn status(&mut self) -> Result<PlayerStatus> {
        if self.session.is_none() {
            return Ok(PlayerStatus::default());
        }
        // Share one deadline across all six property reads so a hung mpv can
        // stall the caller for at most IPC_RESPONSE_TIMEOUT, not 6x that.
        let deadline = Instant::now() + IPC_RESPONSE_TIMEOUT;
        let idle = self.get_bool("idle-active", true, deadline);
        let paused = self.get_bool("pause", false, deadline);
        let position = self.get_f64("time-pos", 0.0, deadline).max(0.0);
        let duration = self
            .get_optional_f64("duration", deadline)
            .map(Duration::from_secs_f64);
        let volume = self.get_f64("volume", 100.0, deadline).clamp(0.0, 100.0) as u8;
        let muted = self.get_bool("mute", false, deadline);
        Ok(PlayerStatus {
            playing: !idle && !paused,
            paused: !idle && paused,
            position: Duration::from_secs_f64(position),
            duration,
            volume,
            muted,
        })
    }

    fn sink_protocol_info(&self) -> &str {
        "http-get:*:audio/mpeg:*,\
         http-get:*:audio/mp4:*,\
         http-get:*:audio/flac:*,\
         http-get:*:audio/ogg:*,\
         http-get:*:audio/x-flac:*,\
         http-get:*:audio/wav:*,\
         http-get:*:audio/aac:*,\
         http-get:*:video/mp4:*,\
         http-get:*:video/webm:*,\
         http-get:*:video/x-matroska:*,\
         http-get:*:video/mpeg:*,\
         http-get:*:video/quicktime:*"
    }
}

impl MpvBackend {
    fn property(&mut self, name: &str, deadline: Instant) -> Value {
        let response = self
            .session()
            .and_then(|session| {
                session.raw_command_until(deadline, json!({"command": ["get_property", name]}))
            })
            .ok();
        response
            .and_then(|value| value.get("data").cloned())
            .unwrap_or(Value::Null)
    }

    fn get_bool(&mut self, name: &str, default: bool, deadline: Instant) -> bool {
        self.property(name, deadline).as_bool().unwrap_or(default)
    }

    fn get_f64(&mut self, name: &str, default: f64, deadline: Instant) -> f64 {
        self.property(name, deadline).as_f64().unwrap_or(default)
    }

    fn get_optional_f64(&mut self, name: &str, deadline: Instant) -> Option<f64> {
        self.property(name, deadline).as_f64()
    }
}

impl Drop for MpvSession {
    fn drop(&mut self) {
        // Ask mpv to quit on its own so it closes its window gracefully
        // instead of being force-killed (which causes a visible flash).
        let quit = json!({ "command": ["quit"] });
        if serde_json::to_writer(&mut self.writer, &quit).is_ok()
            && self.writer.write_all(b"\n").is_ok()
        {
            let _ = self.writer.flush();
        }
        std::thread::sleep(Duration::from_millis(300));
        if self.child.try_wait().ok().flatten().is_none()
            && let Err(error) = self.child.kill()
            && error.kind() != std::io::ErrorKind::InvalidInput
        {
            crate::log_error!("stopping mpv: {error}");
        }
        // Reap the child within a bounded window so a stuck mpv cannot stall
        // application shutdown indefinitely.
        let deadline = Instant::now() + EXIT_WAIT_TIMEOUT;
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        crate::log_warn!(
                            "mpv did not exit within {EXIT_WAIT_TIMEOUT:?} of shutdown"
                        );
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => {
                    crate::log_error!("waiting for mpv: {error}");
                    break;
                }
            }
        }
        #[cfg(unix)]
        if let Err(error) = std::fs::remove_file(&self.socket_path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            crate::log_error!("removing mpv IPC socket: {error}");
        }
    }
}

#[cfg(unix)]
fn ipc_endpoint() -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "mini-mdr-{}-{}.sock",
        std::process::id(),
        unique_suffix()
    ))
}

#[cfg(windows)]
fn ipc_endpoint() -> std::path::PathBuf {
    std::path::PathBuf::from(format!(
        r"\\.\pipe\mini-mdr-{}-{}",
        std::process::id(),
        unique_suffix()
    ))
}

fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

#[cfg(unix)]
fn connect_ipc(endpoint: &std::path::Path) -> std::io::Result<PlatformStream> {
    let stream = PlatformStream::connect(endpoint)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    Ok(stream)
}

#[cfg(windows)]
fn connect_ipc(endpoint: &std::path::Path) -> std::io::Result<PlatformStream> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(endpoint)
}

fn clone_stream(stream: &PlatformStream) -> std::io::Result<PlatformStream> {
    stream.try_clone()
}
