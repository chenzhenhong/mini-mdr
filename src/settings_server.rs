use crate::{
    config::Config,
    i18n::{Language, lang, t},
    state::RendererState,
};
use anyhow::{Context, Result};
use std::{
    collections::HashMap,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

pub struct SettingsServer {
    pub address: SocketAddr,
    running: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl SettingsServer {
    pub fn start(
        preferred_port: u16,
        config: Arc<Mutex<Config>>,
        state: Arc<Mutex<RendererState>>,
    ) -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", preferred_port))
            .or_else(|_| TcpListener::bind(("127.0.0.1", 0)))?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let running = Arc::new(AtomicBool::new(true));
        let active = Arc::clone(&running);
        let thread = thread::Builder::new()
            .name("settings-server".into())
            .spawn(move || {
                while active.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            if let Err(error) = serve(&mut stream, address, &config, &state) {
                                crate::log_error!("settings request failed: {error:#}");
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(50));
                        }
                        Err(error) => {
                            crate::log_error!("settings listener failed: {error}");
                            break;
                        }
                    }
                }
            })?;
        Ok(Self {
            address,
            running,
            thread: Some(thread),
        })
    }
}

fn serve(
    stream: &mut TcpStream,
    address: SocketAddr,
    config: &Arc<Mutex<Config>>,
    state: &Arc<Mutex<RendererState>>,
) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let request = read_request(stream)?;
    let first_line = request.lines().next().unwrap_or_default();
    let mut parts = first_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let path = parts.next().unwrap_or_default();
    let headers = parse_headers(&request);
    let lang = lang();

    if !same_origin(&headers, address) {
        return respond(
            stream,
            "403 Forbidden",
            "text/plain; charset=utf-8",
            "cross-origin requests are not allowed\n",
        );
    }

    match (method, path) {
        ("GET", "/") => respond(
            stream,
            "200 OK",
            "text/html; charset=utf-8",
            &page(config, state, lang, None),
        ),
        ("GET", "/history") => respond(
            stream,
            "200 OK",
            "text/html; charset=utf-8",
            &history_section(&state.lock().map(|v| v.clone()).unwrap_or_default(), lang),
        ),
        ("POST", "/settings") => {
            let body = request
                .split_once("\r\n\r\n")
                .map(|(_, body)| body)
                .unwrap_or_default();
            let fields = parse_form(body)?;
            let updated = update_config(config, &fields, lang);
            let message = match updated {
                Ok(()) => t(lang, "settings-saved"),
                Err(error) => {
                    return respond(
                        stream,
                        "400 Bad Request",
                        "text/html; charset=utf-8",
                        &page(
                            config,
                            state,
                            lang,
                            Some(&format!("{}: {error}", t(lang, "error-save-failed"))),
                        ),
                    );
                }
            };
            respond(
                stream,
                "200 OK",
                "text/html; charset=utf-8",
                &page(config, state, lang, Some(&message)),
            )
        }
        ("GET", "/health") => respond(stream, "200 OK", "text/plain; charset=utf-8", "ok\n"),
        _ => respond(
            stream,
            "404 Not Found",
            "text/plain; charset=utf-8",
            "Not found\n",
        ),
    }
}

fn read_request(stream: &mut TcpStream) -> Result<String> {
    let mut data = Vec::new();
    let mut chunk = [0; 2048];
    let mut content_length = None;
    loop {
        let size = stream.read(&mut chunk)?;
        if size == 0 {
            break;
        }
        data.extend_from_slice(&chunk[..size]);
        if data.len() > 64 * 1024 {
            anyhow::bail!("settings request exceeds 64 KiB");
        }
        if let Some(header_end) = crate::util::find_bytes(&data, b"\r\n\r\n") {
            if content_length.is_none() {
                let headers = String::from_utf8_lossy(&data[..header_end]);
                content_length = headers.lines().find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                });
            }
            if data.len() >= header_end + 4 + content_length.unwrap_or(0) {
                break;
            }
        }
    }
    String::from_utf8(data).context("settings request is not UTF-8")
}

fn parse_headers(request: &str) -> HashMap<String, String> {
    request
        .lines()
        .skip(1)
        .take_while(|line| !line.is_empty())
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect()
}

fn same_origin(headers: &HashMap<String, String>, address: SocketAddr) -> bool {
    let expected = format!("http://{address}");
    if let Some(origin) = headers.get("origin")
        && !origin.eq_ignore_ascii_case(&expected)
    {
        return false;
    }
    if let Some(site) = headers.get("sec-fetch-site")
        && site.eq_ignore_ascii_case("cross-site")
    {
        return false;
    }
    true
}

fn update_config(
    config: &Arc<Mutex<Config>>,
    fields: &HashMap<String, String>,
    lang: Language,
) -> Result<()> {
    let name = fields
        .get("device_name")
        .map(|value| value.trim())
        .unwrap_or_default();
    let backend = fields
        .get("backend")
        .map(|value| value.trim())
        .unwrap_or_default();
    let mpv_path = fields
        .get("mpv_path")
        .map(|value| value.trim())
        .unwrap_or_default();
    let max_history = fields
        .get("max_history")
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(200)
        .min(10000);
    if name.is_empty() || name.len() > 128 {
        anyhow::bail!(t(lang, "error-name-length"));
    }
    if name.chars().any(char::is_control) {
        anyhow::bail!(t(lang, "error-name-control"));
    }
    if backend != "mpv" {
        anyhow::bail!(t(lang, "error-only-mpv"));
    }
    if mpv_path.is_empty() {
        anyhow::bail!(t(lang, "error-mpv-path-empty"));
    }
    let mut guard = config
        .lock()
        .map_err(|_| anyhow::anyhow!("configuration lock poisoned"))?;
    guard.device.name = name.to_owned();
    guard.player.backend = backend.to_owned();
    guard.player.mpv_path = mpv_path.to_owned();
    guard.settings.max_history = max_history;
    guard.save()?;
    Ok(())
}

fn parse_form(body: &str) -> Result<HashMap<String, String>> {
    body.split('&')
        .filter(|field| !field.is_empty())
        .map(|field| {
            let (name, value) = field.split_once('=').unwrap_or((field, ""));
            Ok((url_decode(name)?, url_decode(value)?))
        })
        .collect()
}

fn url_decode(value: &str) -> Result<String> {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => output.push(b' '),
            b'%' if index + 2 < bytes.len() => {
                let encoded = std::str::from_utf8(&bytes[index + 1..index + 3])?;
                output.push(u8::from_str_radix(encoded, 16).context("invalid URL encoding")?);
                index += 2;
            }
            b'%' => anyhow::bail!("incomplete URL encoding"),
            byte => output.push(byte),
        }
        index += 1;
    }
    String::from_utf8(output).context("form value is not UTF-8")
}

fn page(
    config: &Arc<Mutex<Config>>,
    state: &Arc<Mutex<RendererState>>,
    lang: Language,
    message: Option<&str>,
) -> String {
    let config = config.lock().map(|value| value.clone()).unwrap_or_default();
    let state = state.lock().map(|value| value.clone()).unwrap_or_default();
    let message = message
        .map(|value| format!("<div class=\"notice\">{}</div>", escape_html(value)))
        .unwrap_or_default();
    let indicator_color = if state.cast == crate::state::CastState::Running {
        "#4caf50"
    } else {
        "#f44336"
    };
    let html_lang = match lang {
        Language::Zh => "zh-CN",
        Language::En => "en",
    };
    format!(
        r#"<!doctype html>
<html lang="{html_lang}">
<head>
<meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>{title}</title>
<style>{CSS}</style></head>
<body><h1><span class="indicator" style="background:{indicator_color}"></span>mini-mdr</h1><p class="sub">{subtitle}</p>{message}
{settings}
<div id="history">{history}</div>
<script>
setInterval(function(){{
  fetch("/history").then(function(r){{return r.text()}}).then(function(h){{
    document.getElementById("history").innerHTML=h;
  }})
}},3000);
</script></body></html>"#,
        title = t(lang, "settings-title"),
        subtitle = t(lang, "settings-subtitle"),
        settings = settings_form(&config, lang),
        history = history_section(&state, lang),
    )
}

const CSS: &str = "\
:root { color-scheme: dark; font-family: ui-sans-serif, system-ui, sans-serif; background:#10151c; color:#e8edf3; }
body { max-width:720px; margin:0 auto; padding:48px 20px; }
h1 { font-size:32px; margin:0 0 8px; } .sub { color:#8fa2b7; margin-bottom:32px; }
.indicator { display:inline-block; width:12px; height:12px; border-radius:50%; margin-right:8px; vertical-align:middle; }
.card { background:#18212c; border:1px solid #2b3948; border-radius:12px; padding:24px; margin:16px 0; }
label { display:block; margin:16px 0 6px; color:#b8c6d5; } input,select { box-sizing:border-box; width:100%; padding:11px 12px; color:#eef4fa; background:#101720; border:1px solid #3a4b5d; border-radius:7px; }
button { margin-top:22px; padding:11px 18px; border:0; border-radius:7px; background:#42a5f5; color:#07131d; font-weight:700; cursor:pointer; }
.notice { background:#143b2c; border:1px solid #287758; padding:12px 14px; border-radius:8px; }
.history { width:100%; border-collapse:collapse; margin-top:12px; }
.history th,.history td { padding:10px 12px; text-align:left; border-bottom:1px solid #2b3948; }
.history th { color:#8fa2b7; font-weight:600; }
.history td { color:#e8edf3; }
.time { color:#8fa2b7; white-space:nowrap; width:60px; }
.empty { color:#8fa2b7; margin:12px 0; }";

fn settings_form(config: &crate::config::Config, lang: Language) -> String {
    let name = escape_html(&config.device.name);
    let mpv_path = escape_html(&config.player.mpv_path);
    let max_history = config.settings.max_history;
    format!(
        r#"<section class="card"><h2>{section}</h2><form method="post" action="/settings">
<label for="device_name">{label_name}</label><input id="device_name" name="device_name" maxlength="128" required value="{name}">
<label for="backend">{label_backend}</label><select id="backend" name="backend"><option value="mpv" selected>mpv</option></select>
<label for="mpv_path">{label_mpv_path}</label><input id="mpv_path" name="mpv_path" required value="{mpv_path}">
<label for="max_history">{label_max_history}</label><input id="max_history" name="max_history" type="number" min="1" max="10000" required value="{max_history}">
<button type="submit">{btn_save}</button></form></section>"#,
        section = t(lang, "settings-section-settings"),
        label_name = t(lang, "settings-device-name"),
        label_backend = t(lang, "settings-player-backend"),
        label_mpv_path = t(lang, "settings-mpv-path"),
        label_max_history = t(lang, "settings-max-history"),
        btn_save = t(lang, "settings-save"),
    )
}

fn history_section(state: &crate::state::RendererState, lang: Language) -> String {
    let content = if state.history.is_empty() {
        format!(
            "<p class=\"empty\">{}</p>",
            t(lang, "settings-history-empty")
        )
    } else {
        let mut rows = String::new();
        for entry in state.history.iter().rev() {
            let display_title = entry.title.as_deref().unwrap_or(&entry.uri);
            let time = &entry.time_str();
            rows.push_str(&format!(
                "<tr><td class=\"time\">{time}</td><td title=\"{}\">{}</td></tr>",
                escape_html(&entry.uri),
                escape_html(display_title),
            ));
        }
        format!(
            "<table class=\"history\"><thead><tr><th>{}</th><th>{}</th></tr></thead><tbody>{rows}</tbody></table>",
            t(lang, "settings-history-time"),
            t(lang, "settings-history-title"),
        )
    };
    format!(
        r#"<section class="card"><h2>{}</h2>{content}</section>"#,
        t(lang, "settings-section-history"),
    )
}

fn respond(stream: &mut TcpStream, status: &str, content_type: &str, body: &str) -> Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    Ok(())
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}



impl Drop for SettingsServer {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(thread) = self.thread.take()
            && let Err(error) = thread.join()
        {
            crate::log_error!("settings thread panicked: {error:?}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_form_values() {
        let form = parse_form("device_name=Living+Room%26TV&backend=mpv").unwrap();
        assert_eq!(form["device_name"], "Living Room&TV");
    }

    #[test]
    fn escapes_html_values() {
        assert_eq!(escape_html("<a&\"'>"), "&lt;a&amp;&quot;&#39;&gt;");
    }

    #[test]
    fn rejects_cross_origin_requests() {
        let address: SocketAddr = "127.0.0.1:7878".parse().unwrap();
        let expected = format!("http://{address}");
        let mut headers = HashMap::new();
        assert!(same_origin(&headers, address));
        headers.insert("origin".into(), expected.clone());
        assert!(same_origin(&headers, address));
        headers.insert("origin".into(), "http://evil.example".into());
        assert!(!same_origin(&headers, address));
        headers.remove("origin");
        headers.insert("sec-fetch-site".into(), "cross-site".into());
        assert!(!same_origin(&headers, address));
    }
}
