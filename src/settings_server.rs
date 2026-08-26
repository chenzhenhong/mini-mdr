use crate::{
    config::Config,
    i18n::{Language, lang, s},
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
        ("POST", "/settings") => {
            let body = request
                .split_once("\r\n\r\n")
                .map(|(_, body)| body)
                .unwrap_or_default();
            let fields = parse_form(body)?;
            let updated = update_config(config, &fields, lang);
            let message = match updated {
                Ok(()) => s!(
                    lang,
                    "设置已保存。设备名称和播放器设置将在下次开始 Cast 时生效。",
                    "Settings saved. Device name and player settings take effect on the next Start Cast."
                ),
                Err(error) => {
                    return respond(
                        stream,
                        "400 Bad Request",
                        "text/html; charset=utf-8",
                        &page(
                            config,
                            state,
                            lang,
                            Some(&format!("{}: {error}", s!(lang, "保存失败", "Save failed"))),
                        ),
                    );
                }
            };
            respond(
                stream,
                "200 OK",
                "text/html; charset=utf-8",
                &page(config, state, lang, Some(message)),
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
        if let Some(header_end) = find_bytes(&data, b"\r\n\r\n") {
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
    if name.is_empty() || name.len() > 128 {
        anyhow::bail!(s!(
            lang,
            "设备名称必须为 1 到 128 个字符",
            "Device name must be 1 to 128 characters"
        ));
    }
    if name.chars().any(char::is_control) {
        anyhow::bail!(s!(
            lang,
            "设备名称不能包含控制字符",
            "Device name must not contain control characters"
        ));
    }
    if backend != "mpv" {
        anyhow::bail!(s!(
            lang,
            "当前版本只提供 mpv 后端",
            "Only mpv backend is available in this version"
        ));
    }
    if mpv_path.is_empty() {
        anyhow::bail!(s!(lang, "mpv 路径不能为空", "mpv path must not be empty"));
    }
    let mut guard = config
        .lock()
        .map_err(|_| anyhow::anyhow!("configuration lock poisoned"))?;
    let mut next = guard.clone();
    next.device.name = name.to_owned();
    next.player.backend = backend.to_owned();
    next.player.mpv_path = mpv_path.to_owned();
    next.save()?;
    *guard = next;
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
    let title = s!(lang, "mini-mdr 设置", "mini-mdr Settings");
    let subtitle = s!(
        lang,
        "本地 DMR 设置与运行状态",
        "Local DMR settings and runtime status"
    );
    let section_status = s!(lang, "状态", "Status");
    let label_transport = s!(lang, "传输状态", "Transport");
    let label_volume = s!(lang, "音量", "Volume");
    let muted_label = s!(lang, "（静音）", " (muted)");
    let section_settings = s!(lang, "设置", "Settings");
    let label_name = s!(lang, "设备名称", "Device Name");
    let label_backend = s!(lang, "播放器后端", "Player Backend");
    let label_mpv_path = s!(lang, "mpv 可执行文件路径", "mpv Executable Path");
    let btn_save = s!(lang, "保存设置", "Save Settings");
    let muted_display = if state.muted { muted_label } else { "" };
    let html_lang = s!(lang, "zh-CN", "en");
    format!(
        r#"<!doctype html>
<html lang="{html_lang}">
<head>
<meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>{title}</title>
<style>
:root {{ color-scheme: dark; font-family: ui-sans-serif, system-ui, sans-serif; background:#10151c; color:#e8edf3; }}
body {{ max-width:720px; margin:0 auto; padding:48px 20px; }}
h1 {{ font-size:32px; margin:0 0 8px; }} .sub {{ color:#8fa2b7; margin-bottom:32px; }}
.card {{ background:#18212c; border:1px solid #2b3948; border-radius:12px; padding:24px; margin:16px 0; }}
label {{ display:block; margin:16px 0 6px; color:#b8c6d5; }} input,select {{ box-sizing:border-box; width:100%; padding:11px 12px; color:#eef4fa; background:#101720; border:1px solid #3a4b5d; border-radius:7px; }}
button {{ margin-top:22px; padding:11px 18px; border:0; border-radius:7px; background:#42a5f5; color:#07131d; font-weight:700; cursor:pointer; }}
.grid {{ display:grid; grid-template-columns:repeat(auto-fit,minmax(180px,1fr)); gap:12px; }} .metric {{ padding:14px; background:#101720; border-radius:8px; }} .metric b {{ display:block; margin-top:5px; }}
.notice {{ background:#143b2c; border:1px solid #287758; padding:12px 14px; border-radius:8px; }}
</style></head>
<body><h1>mini-mdr</h1><p class="sub">{subtitle}</p>{message}
<section class="card"><h2>{section_status}</h2><div class="grid">
<div class="metric">Cast<b>{cast}</b></div><div class="metric">{label_transport}<b>{transport}</b></div><div class="metric">{label_volume}<b>{volume}%{muted}</b></div>
</div></section>
<section class="card"><h2>{section_settings}</h2><form method="post" action="/settings">
<label for="device_name">{label_name}</label><input id="device_name" name="device_name" maxlength="128" required value="{name}">
<label for="backend">{label_backend}</label><select id="backend" name="backend"><option value="mpv" selected>mpv</option></select>
<label for="mpv_path">{label_mpv_path}</label><input id="mpv_path" name="mpv_path" required value="{mpv_path}">
<button type="submit">{btn_save}</button></form></section></body></html>"#,
        cast = escape_html(state.cast.as_str(lang)),
        transport = escape_html(state.transport.as_str(lang)),
        volume = state.volume,
        muted = muted_display,
        name = escape_html(&config.device.name),
        mpv_path = escape_html(&config.player.mpv_path),
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

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
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
