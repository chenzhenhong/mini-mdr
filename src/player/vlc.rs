use super::{PlayerBackend, PlayerStatus};
use anyhow::{Context, Result};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const VLC_HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const VLC_START_TIMEOUT: Duration = Duration::from_secs(5);

pub struct VlcBackend {
    executable: String,
    session: Option<VlcSession>,
    failed_permanently: bool,
    pre_mute_volume: u8,
    muted: bool,
}

struct VlcSession {
    child: Child,
    port: u16,
    password: String,
}

impl VlcBackend {
    pub fn new(path: &str) -> Result<Self> {
        if path.trim().is_empty() {
            anyhow::bail!("vlc executable path cannot be empty");
        }
        Ok(Self {
            executable: path.to_owned(),
            session: None,
            failed_permanently: false,
            pre_mute_volume: 100,
            muted: false,
        })
    }

    fn session(&mut self) -> Result<&mut VlcSession> {
        if self.failed_permanently {
            return Err(super::already_reported(anyhow::anyhow!(
                "vlc not found, check player.vlc_path in config"
            )));
        }
        if self.session.is_none() {
            match VlcSession::start(&self.executable) {
                Ok(session) => {
                    self.session = Some(session);
                }
                Err(error) => {
                    if super::is_program_not_found(&error) {
                        self.failed_permanently = true;
                        crate::log_error!("vlc not found at '{}', will not retry", self.executable);
                    }
                    return Err(super::already_reported(error));
                }
            }
        }
        self.session.as_mut().context("vlc session did not start")
    }

    fn http_request(&mut self, method: &str, path: &str, body: &str) -> Result<String> {
        let session = self.session()?;
        let addr: std::net::SocketAddr = format!("127.0.0.1:{}", session.port)
            .parse()
            .context("parsing VLC HTTP address")?;
        let mut stream = TcpStream::connect_timeout(&addr, VLC_HTTP_TIMEOUT)
            .context("connecting to VLC HTTP interface")?;
        stream.set_read_timeout(Some(VLC_HTTP_TIMEOUT))?;
        stream.set_write_timeout(Some(VLC_HTTP_TIMEOUT))?;

        let auth = base64_encode(&format!(":{}", session.password));
        let content_length = body.len();
        let request = if body.is_empty() {
            format!(
                "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Basic {auth}\r\nConnection: close\r\n\r\n",
                port = session.port,
            )
        } else {
            format!(
                "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Basic {auth}\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {content_length}\r\nConnection: close\r\n\r\n{body}",
                port = session.port,
            )
        };
        stream.write_all(request.as_bytes())?;

        let mut reader = BufReader::new(stream);
        let mut status_line = String::new();
        reader.read_line(&mut status_line)?;

        let mut headers_done = false;
        let mut header_line = String::new();
        while !headers_done {
            header_line.clear();
            reader.read_line(&mut header_line)?;
            if header_line.trim().is_empty() {
                headers_done = true;
            }
        }

        let mut body = String::new();
        reader.read_to_string(&mut body)?;

        if !status_line.contains("200") {
            anyhow::bail!("VLC HTTP error: {}", status_line.trim());
        }
        Ok(body)
    }

    fn post(&mut self, command: &str) -> Result<String> {
        let path = format!("/requests/status.xml?command={command}");
        self.http_request("POST", &path, "")
    }

    fn get_status_xml(&mut self) -> Result<String> {
        self.http_request("GET", "/requests/status.xml", "")
    }

    fn xml_tag<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
        let open = format!("<{tag}>");
        let close = format!("</{tag}>");
        let start = xml.find(&open)? + open.len();
        let end = xml.find(&close)?;
        Some(&xml[start..end])
    }
}

impl PlayerBackend for VlcBackend {
    fn load(&mut self, uri: &str, _title: Option<&str>) -> Result<()> {
        self.post(&format!("in_play&input={}", percent_encode(uri)))?;
        if self.muted {
            self.post("volume&val=0")?;
        }
        Ok(())
    }

    fn play(&mut self) -> Result<()> {
        if self.session.is_none() {
            return Ok(());
        }
        let status = self.get_status_xml()?;
        if Self::xml_tag(&status, "state").unwrap_or("") == "paused" {
            self.post("pl_pause")?;
        } else {
            self.post("pl_play")?;
        }
        Ok(())
    }

    fn pause(&mut self) -> Result<()> {
        if self.session.is_none() {
            return Ok(());
        }
        self.post("pl_pause")?;
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        if let Some(session) = self.session.take() {
            drop(session);
        }
        // Mute is renderer state, not media-session state. Keep it so a
        // subsequent lazy session starts muted as the control point expects.
        Ok(())
    }

    fn seek(&mut self, position: Duration) -> Result<()> {
        if self.session.is_none() {
            return Ok(());
        }
        self.post(&format!("seek&val={}", position.as_secs()))?;
        Ok(())
    }

    fn set_volume(&mut self, volume: u8) -> Result<()> {
        if self.session.is_none() {
            if self.muted {
                self.pre_mute_volume = volume.min(100);
            }
            return Ok(());
        }
        if self.muted {
            self.pre_mute_volume = volume.min(100);
            return Ok(());
        }
        let vlc_volume = (volume.min(100) as u32) * 256 / 100;
        self.post(&format!("volume&val={vlc_volume}"))?;
        Ok(())
    }

    fn set_mute(&mut self, muted: bool) -> Result<()> {
        if self.session.is_none() {
            self.muted = muted;
            return Ok(());
        }
        if self.muted == muted {
            return Ok(());
        }
        if muted {
            let status = self.get_status_xml()?;
            self.pre_mute_volume = Self::xml_tag(&status, "volume")
                .and_then(|v| v.parse::<u32>().ok())
                .map(|v| (v * 100 / 256).min(100) as u8)
                .unwrap_or(100);
            self.post("volume&val=0")?;
        } else {
            let vlc_volume = (self.pre_mute_volume.min(100) as u32) * 256 / 100;
            self.post(&format!("volume&val={vlc_volume}"))?;
        }
        self.muted = muted;
        Ok(())
    }

    fn status(&mut self) -> Result<PlayerStatus> {
        if self.session.is_none() {
            return Ok(PlayerStatus::default());
        }
        let xml = self.get_status_xml()?;
        let state = Self::xml_tag(&xml, "state").unwrap_or("");
        let time = Self::xml_tag(&xml, "time")
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.0);
        let length = Self::xml_tag(&xml, "length")
            .and_then(|v| v.parse::<f64>().ok())
            .map(Duration::from_secs_f64);
        let volume = Self::xml_tag(&xml, "volume")
            .and_then(|v| v.parse::<u32>().ok())
            .map(|v| (v * 100 / 256).min(100) as u8)
            .unwrap_or(100);
        Ok(PlayerStatus {
            playing: state == "playing",
            paused: state == "paused",
            position: Duration::from_secs_f64(time),
            duration: length,
            volume,
            muted: self.muted,
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
         http-get:*:video/quicktime:*,\
         http-get:*:video/x-ms-wmv:*"
    }
}

impl VlcSession {
    fn start(executable: &str) -> Result<Self> {
        let port = find_free_port().context("finding free port for VLC HTTP")?;
        let password = format!("mini-mdr-{}", std::process::id());

        let mut child = Command::new(executable)
            .args([
                "--intf",
                "http",
                "--http-host",
                "127.0.0.1",
                "--http-port",
                &port.to_string(),
                "--http-password",
                &password,
                "--no-video-title-show",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("starting vlc from {executable}"))?;

        // Verify within a bounded window that VLC is still running and its HTTP
        // interface is reachable; otherwise clean up the child process.
        let deadline = Instant::now() + VLC_START_TIMEOUT;
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        loop {
            if let Some(status) = child.try_wait().context("checking vlc process status")? {
                anyhow::bail!("vlc exited before its HTTP interface was ready (status: {status})");
            }
            match TcpStream::connect_timeout(&addr, Duration::from_millis(200)) {
                Ok(_) => break,
                Err(_) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        anyhow::bail!(
                            "timed out waiting for VLC HTTP interface on 127.0.0.1:{port}"
                        );
                    }
                    thread::sleep(Duration::from_millis(50));
                }
            }
        }

        Ok(Self {
            child,
            port,
            password,
        })
    }
}

impl Drop for VlcSession {
    fn drop(&mut self) {
        if let Err(error) = self.child.kill()
            && error.kind() != std::io::ErrorKind::InvalidInput
        {
            crate::log_error!("stopping vlc: {error}");
        }
        if let Err(error) = self.child.wait() {
            crate::log_error!("waiting for vlc: {error}");
        }
    }
}

// NOTE: There is a small TOCTOU window between dropping the listener and VLC
// binding to the port. This is acceptable for a single-user desktop application.
fn find_free_port() -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

/// Percent-encodes every byte that is not an RFC 3986 unreserved character so
/// the URI survives being embedded in an HTTP query string unmodified.
fn percent_encode(input: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut output = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                output.push(byte as char);
            }
            _ => {
                output.push('%');
                output.push(HEX[(byte >> 4) as usize] as char);
                output.push(HEX[(byte & 0x0F) as usize] as char);
            }
        }
    }
    output
}

fn base64_encode(input: &str) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = input.as_bytes();
    let mut result = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let triple = (b0 << 16) | (b1 << 8) | b2;
        result.push(CHARS[((triple >> 18) & 0x3F) as usize] as char);
        result.push(CHARS[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            result.push(CHARS[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }
        if chunk.len() > 2 {
            result.push(CHARS[(triple & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::percent_encode;

    #[test]
    fn percent_encodes_uri_specials() {
        assert_eq!(
            percent_encode("http://192.168.1.5:8200/MediaItems/1.mp3?x=1&y=2"),
            "http%3A%2F%2F192.168.1.5%3A8200%2FMediaItems%2F1.mp3%3Fx%3D1%26y%3D2"
        );
        assert_eq!(
            percent_encode("A song with spaces.flac"),
            "A%20song%20with%20spaces.flac"
        );
        assert_eq!(percent_encode("abc-._~09"), "abc-._~09");
    }
}
