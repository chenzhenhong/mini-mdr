use crate::{
    config::Config,
    i18n::{Language, t},
    state::RendererState,
};
use anyhow::{Context, Result};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use std::{
    collections::HashMap,
    io::{Read, Write},
    net::{Shutdown, SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{Receiver, RecvTimeoutError, Sender, channel},
    },
    thread,
    time::Duration,
};

enum SettingsEvent {
    Status {
        running: bool,
        address: Option<String>,
    },
    History(String),
}

struct Subscriber {
    id: u64,
    sender: Sender<SettingsEvent>,
}

static EVENTS: OnceLock<Mutex<Vec<Subscriber>>> = OnceLock::new();
static NEXT_SUBSCRIBER_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_CONNECTION_ID: AtomicU64 = AtomicU64::new(1);
static WATCHER: OnceLock<Mutex<RecommendedWatcher>> = OnceLock::new();
/// Serializes the whole config read-modify-save-commit cycle so that two
/// concurrent `POST /settings` requests cannot lose each other's updates.
static CONFIG_SAVE_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();

fn events() -> &'static Mutex<Vec<Subscriber>> {
    EVENTS.get_or_init(|| Mutex::new(Vec::new()))
}

struct Subscription {
    id: u64,
    receiver: Receiver<SettingsEvent>,
}

fn subscribe() -> Subscription {
    let (sender, receiver) = channel();
    let id = NEXT_SUBSCRIBER_ID.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut subscribers) = events().lock() {
        subscribers.push(Subscriber { id, sender });
    }
    Subscription { id, receiver }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        if let Ok(mut subscribers) = events().lock() {
            subscribers.retain(|subscriber| subscriber.id != self.id);
        }
    }
}

pub fn publish_status(running: bool, address: Option<String>) {
    if let Ok(mut subscribers) = events().lock() {
        subscribers.retain(|subscriber| {
            subscriber
                .sender
                .send(SettingsEvent::Status {
                    running,
                    address: address.clone(),
                })
                .is_ok()
        });
    }
}

pub fn publish_history() {
    let limit = crate::config::Config::max_history_from_disk();
    let payload = history_payload(limit);
    if let Ok(mut subscribers) = events().lock() {
        subscribers.retain(|subscriber| {
            subscriber
                .sender
                .send(SettingsEvent::History(payload.clone()))
                .is_ok()
        });
    }
}

pub fn init_watcher() {
    if WATCHER.get().is_some() {
        return;
    }
    let dir = match crate::config::Config::config_dir() {
        Ok(dir) => dir,
        Err(error) => {
            crate::log_warn!("cannot determine config directory for file watcher: {error:#}");
            return;
        }
    };
    if let Err(error) = std::fs::create_dir_all(&dir) {
        crate::log_warn!("cannot create config directory for file watcher: {error:#}");
        return;
    }
    let handler = move |res: std::result::Result<Event, notify::Error>| {
        if let Ok(event) = res
            && event
                .paths
                .iter()
                .any(|p| p.file_name().map(|n| n == "history.json").unwrap_or(false))
        {
            publish_history();
        }
    };
    let mut watcher = match notify::recommended_watcher(handler) {
        Ok(watcher) => watcher,
        Err(error) => {
            crate::log_warn!("failed to start config file watcher: {error:#}");
            return;
        }
    };
    if let Err(error) = watcher.watch(&dir, RecursiveMode::NonRecursive) {
        crate::log_warn!("cannot watch config directory for changes: {error:#}");
        return;
    }
    if WATCHER.set(Mutex::new(watcher)).is_err() {
        crate::log_warn!("config file watcher already initialized");
    }
}

/// RAII guard that unregisters a connection clone when the request thread
/// finishes, so `SettingsServer::drop` can shut down only live connections.
struct TrackedConnection {
    id: u64,
    connections: Arc<Mutex<HashMap<u64, TcpStream>>>,
}

impl Drop for TrackedConnection {
    fn drop(&mut self) {
        if let Ok(mut connections) = self.connections.lock() {
            connections.remove(&self.id);
        }
    }
}

/// Registers a duplicate handle to `stream` so the server can `shutdown` it to
/// unblock a request thread stuck in a read or write. Returns `None` when the
/// connection could not be registered; the caller must then close the stream
/// instead of serving it untracked.
fn track_connection(
    connections: &Arc<Mutex<HashMap<u64, TcpStream>>>,
    stream: &TcpStream,
) -> Option<TrackedConnection> {
    let clone = match stream.try_clone() {
        Ok(clone) => clone,
        Err(error) => {
            crate::log_warn!("cannot track settings connection: {error}");
            return None;
        }
    };
    let id = NEXT_CONNECTION_ID.fetch_add(1, Ordering::Relaxed);
    match connections.lock() {
        Ok(mut tracked) => {
            tracked.insert(id, clone);
        }
        Err(_) => {
            crate::log_warn!("settings connection registry unavailable; refusing request");
            return None;
        }
    }
    Some(TrackedConnection {
        id,
        connections: Arc::clone(connections),
    })
}

pub struct SettingsServer {
    pub address: SocketAddr,
    running: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
    connections: Arc<Mutex<HashMap<u64, TcpStream>>>,
    requests: Arc<Mutex<Vec<thread::JoinHandle<()>>>>,
}

impl SettingsServer {
    pub fn start(
        preferred_port: u16,
        config: Arc<Mutex<Config>>,
        state: Arc<Mutex<RendererState>>,
    ) -> Result<Self> {
        init_watcher();
        let listener = TcpListener::bind(("127.0.0.1", preferred_port)).or_else(|_| {
            crate::log_warn!(
                "preferred settings port {preferred_port} unavailable, using ephemeral port"
            );
            TcpListener::bind(("127.0.0.1", 0))
        })?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let running = Arc::new(AtomicBool::new(true));
        let connections: Arc<Mutex<HashMap<u64, TcpStream>>> = Arc::new(Mutex::new(HashMap::new()));
        let requests: Arc<Mutex<Vec<thread::JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));
        let active = Arc::clone(&running);
        let tracked = Arc::clone(&connections);
        let handles = Arc::clone(&requests);
        let thread = thread::Builder::new()
            .name("settings-server".into())
            .spawn(move || {
                while active.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let config = Arc::clone(&config);
                            let state = Arc::clone(&state);
                            let active = Arc::clone(&active);
                            let tracked = Arc::clone(&tracked);
                            let handles = Arc::clone(&handles);
                            // Register the connection before spawning the
                            // request thread. A concurrent `SettingsServer::drop`
                            // can otherwise clear the registry and join the
                            // accept thread while the request thread has not
                            // registered itself yet, leaving a live socket that
                            // is never shut down.
                            let Some(guard) = track_connection(&tracked, &stream) else {
                                crate::log_warn!("cannot track settings connection; closing it");
                                let _ = stream.shutdown(Shutdown::Both);
                                continue;
                            };
                            let handle = thread::spawn(move || {
                                // The guard unregisters the connection when the
                                // request thread finishes, matching the thread's
                                // own lifetime.
                                let _guard = guard;
                                // Bail out early when the server is shutting
                                // down: `drop` may have already cleared the
                                // connection registry, so serving could
                                // otherwise block until the read timeout.
                                if !active.load(Ordering::Relaxed) {
                                    return;
                                }
                                if let Err(error) =
                                    serve(&mut stream, address, &config, &state, &active)
                                {
                                    crate::log_error!("settings request failed: {error:#}");
                                }
                            });
                            if let Ok(mut pending) = handles.lock() {
                                pending.retain(|pending| !pending.is_finished());
                                pending.push(handle);
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
        crate::log_info!("settings server listening on http://{address}");
        Ok(Self {
            address,
            running,
            thread: Some(thread),
            connections,
            requests,
        })
    }
}

fn serve(
    stream: &mut TcpStream,
    address: SocketAddr,
    config: &Arc<Mutex<Config>>,
    state: &Arc<Mutex<RendererState>>,
    running: &Arc<AtomicBool>,
) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;
    let request = match read_request(stream) {
        Ok(req) => req,
        Err(_) => {
            let _ = respond(
                stream,
                "400 Bad Request",
                "text/plain; charset=utf-8",
                "400 Bad Request\n",
            );
            return Ok(());
        }
    };
    if !running.load(Ordering::Relaxed) {
        return Ok(());
    }
    let first_line = request.lines().next().unwrap_or_default();
    let mut parts = first_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let path = parts.next().unwrap_or_default();
    let headers = parse_headers(&request);
    let cfg_language = config
        .lock()
        .map(|c| c.settings.language.clone())
        .unwrap_or_default();
    let lang = crate::i18n::resolve_language(&cfg_language);
    let history_limit = config
        .lock()
        .map(|c| c.settings.max_history)
        .unwrap_or(crate::config::DEFAULT_MAX_HISTORY);

    if !same_origin(&headers, address) {
        crate::log_warn!(
            "rejected cross-origin settings request from {}",
            headers
                .get("origin")
                .map(String::as_str)
                .unwrap_or("unknown")
        );
        return respond(
            stream,
            "403 Forbidden",
            "text/plain; charset=utf-8",
            &format!("{}\n", t(lang, "error-cross-origin")),
        );
    }

    match (method, path) {
        ("GET", "/") => {
            let config = config.lock().map(|v| v.clone()).unwrap_or_default();
            let state = state.lock().map(|v| v.clone()).unwrap_or_default();
            let running = state.cast == crate::state::CastState::Running;
            let indicator_color = if running { "#22c55e" } else { "#ef4444" };
            let indicator_text = if running {
                t(lang, "cast-running")
            } else {
                t(lang, "cast-stopped")
            };
            let max_history = config.settings.max_history.to_string();
            let html_lang = match lang {
                Language::Zh => "zh-CN",
                Language::En => "en",
            };
            let title = t(lang, "settings-title");
            let tab_settings = t(lang, "tab-settings");
            let tab_history = t(lang, "tab-history");
            let tab_about = t(lang, "tab-about");
            let subtitle = t(lang, "settings-subtitle");
            let status_label = t(lang, "settings-status");
            let status_running = t(lang, "cast-running");
            let status_stopped = t(lang, "cast-stopped");
            let settings_heading = t(lang, "settings-section-settings");
            let label_name = t(lang, "settings-device-name");
            let label_backend = t(lang, "settings-player-backend");
            let label_mpv_path = t(lang, "settings-mpv-path");
            let label_vlc_path = t(lang, "settings-vlc-path");
            let label_max_history = t(lang, "settings-max-history");
            let save_btn = t(lang, "settings-save");
            let history_heading = t(lang, "settings-section-history");
            let history_empty = t(lang, "settings-history-empty");
            let history_empty_sub = t(lang, "settings-history-empty-sub");
            let history_subtitle = t(lang, "settings-history-subtitle");
            let settings_hint = t(lang, "settings-hint");
            let time_col = t(lang, "settings-history-time");
            let title_col = t(lang, "settings-history-title");
            let index_col = t(lang, "settings-history-index");
            let copy_link = t(lang, "settings-history-copy-link");
            let open_in_browser = t(lang, "settings-history-open-in-browser");
            let copied_msg = t(lang, "settings-history-copied");
            let saved_msg = t(lang, "settings-saved");
            let save_err_msg = t(lang, "error-save-failed");
            let about_title = t(lang, "about-title");
            let about_subtitle = t(lang, "about-subtitle");
            let about_desc = t(lang, "about-description");
            let label_language = t(lang, "settings-language");
            let language_options_str = language_options(lang, &config.settings.language);
            let tab_guide = t(lang, "tab-guide");
            let guide_title = t(lang, "guide-title");
            let guide_intro = t(lang, "guide-intro");
            let guide_player_heading = t(lang, "guide-player-heading");
            let guide_player_text = t(lang, "guide-player-text");
            let guide_config_heading = t(lang, "guide-config-heading");
            let guide_config_text = t(lang, "guide-config-text");
            let guide_usage_heading = t(lang, "guide-usage-heading");
            let guide_usage_text = t(lang, "guide-usage-text");
            let status_upnp = t(lang, "status-upnp");
            let about_version = t(lang, "about-version");
            let about_repository = t(lang, "about-repository");
            let about_license = t(lang, "about-license");
            let config_dir = crate::config::Config::config_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            let vars: Vec<(&str, &str)> = vec![
                ("HTML_LANG", html_lang),
                ("TITLE", title.as_str()),
                ("TAB_SETTINGS", tab_settings.as_str()),
                ("TAB_HISTORY", tab_history.as_str()),
                ("TAB_GUIDE", tab_guide.as_str()),
                ("TAB_ABOUT", tab_about.as_str()),
                ("SUBTITLE", subtitle.as_str()),
                ("STATUS_LABEL", status_label.as_str()),
                ("LABEL_LANGUAGE", label_language.as_str()),
                ("INDICATOR_COLOR", indicator_color),
                ("INDICATOR_TEXT", indicator_text.as_str()),
                ("STATUS_RUNNING", status_running.as_str()),
                ("STATUS_STOPPED", status_stopped.as_str()),
                ("SETTINGS_HEADING", settings_heading.as_str()),
                ("SETTINGS_HINT", settings_hint.as_str()),
                ("LABEL_NAME", label_name.as_str()),
                ("LABEL_BACKEND", label_backend.as_str()),
                ("LABEL_MPV_PATH", label_mpv_path.as_str()),
                ("LABEL_VLC_PATH", label_vlc_path.as_str()),
                ("LABEL_MAX_HISTORY", label_max_history.as_str()),
                ("DEVICE_NAME", config.device.name.as_str()),
                ("BACKEND_VALUE", config.player.backend.as_str()),
                (
                    "MPV_SELECTED",
                    if config.player.backend == "mpv" {
                        "selected"
                    } else {
                        ""
                    },
                ),
                (
                    "VLC_SELECTED",
                    if config.player.backend == "vlc" {
                        "selected"
                    } else {
                        ""
                    },
                ),
                ("MPV_PATH", config.player.mpv_path.as_str()),
                ("VLC_PATH", config.player.vlc_path.as_str()),
                ("MAX_HISTORY", max_history.as_str()),
                ("SAVE_BTN", save_btn.as_str()),
                ("HISTORY_HEADING", history_heading.as_str()),
                ("HISTORY_EMPTY", history_empty.as_str()),
                ("HISTORY_EMPTY_SUB", history_empty_sub.as_str()),
                ("HISTORY_SUBTITLE", history_subtitle.as_str()),
                ("TIME_COL", time_col.as_str()),
                ("TITLE_COL", title_col.as_str()),
                ("INDEX_COL", index_col.as_str()),
                ("COPY_LINK", copy_link.as_str()),
                ("OPEN_IN_BROWSER", open_in_browser.as_str()),
                ("COPIED_MSG", copied_msg.as_str()),
                ("SAVED_MSG", saved_msg.as_str()),
                ("SAVE_ERR_MSG", save_err_msg.as_str()),
                ("ABOUT_TITLE", about_title.as_str()),
                ("ABOUT_SUBTITLE", about_subtitle.as_str()),
                ("ABOUT_DESC", about_desc.as_str()),
                ("ABOUT_VERSION", about_version.as_str()),
                ("ABOUT_REPOSITORY", about_repository.as_str()),
                ("ABOUT_LICENSE", about_license.as_str()),
                ("STATUS_UPNP", status_upnp.as_str()),
                ("GUIDE_TITLE", guide_title.as_str()),
                ("GUIDE_INTRO", guide_intro.as_str()),
                ("GUIDE_PLAYER_HEADING", guide_player_heading.as_str()),
                ("GUIDE_PLAYER_TEXT", guide_player_text.as_str()),
                ("GUIDE_CONFIG_HEADING", guide_config_heading.as_str()),
                ("GUIDE_CONFIG_TEXT", guide_config_text.as_str()),
                ("GUIDE_USAGE_HEADING", guide_usage_heading.as_str()),
                ("GUIDE_USAGE_TEXT", guide_usage_text.as_str()),
                ("CONFIG_DIR", config_dir.as_str()),
                ("VERSION", env!("CARGO_PKG_VERSION")),
            ];
            let body = render(&load_template(), &vars)
                .replace("{{LANGUAGE_OPTIONS}}", &language_options_str);
            respond(stream, "200 OK", "text/html; charset=utf-8", &body)
        }
        ("GET", "/history") => {
            let body = history_payload(history_limit);
            respond(stream, "200 OK", "application/json; charset=utf-8", &body)
        }
        ("GET", "/events") => sse_stream(stream, state, running, history_limit),
        ("GET", "/icon.png") => serve_asset(
            stream,
            "icon.png",
            "image/png",
            include_bytes!("../resources/icon.png"),
        ),
        ("GET", "/favicon.ico") => serve_asset(
            stream,
            "icon.ico",
            "image/x-icon",
            include_bytes!("../resources/icon.ico"),
        ),
        ("POST", "/settings") => {
            let body = request
                .split_once("\r\n\r\n")
                .map(|(_, body)| body)
                .unwrap_or_default();
            let fields = parse_form(body)?;
            let (ok, message) = match update_config(config, &fields, lang) {
                Ok(()) => {
                    crate::log_info!("settings updated");
                    (true, t(lang, "settings-saved"))
                }
                Err(error) => (false, format!("{}: {error}", t(lang, "error-save-failed"))),
            };
            let body = serde_json::json!({ "ok": ok, "message": message }).to_string();
            respond(stream, "200 OK", "application/json; charset=utf-8", &body)
        }
        ("GET", "/health") => respond(stream, "200 OK", "text/plain; charset=utf-8", "ok\n"),
        _ => respond(
            stream,
            "404 Not Found",
            "text/plain; charset=utf-8",
            &format!("{}\n", t(lang, "error-not-found")),
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
    let localhost = format!("http://localhost:{}", address.port());
    if let Some(origin) = headers.get("origin")
        && !origin.eq_ignore_ascii_case(&expected)
        && !origin.eq_ignore_ascii_case(&localhost)
    {
        return false;
    }
    if headers
        .get("sec-fetch-site")
        .is_some_and(|value| value.eq_ignore_ascii_case("cross-site"))
    {
        return false;
    }
    true
}

fn config_save_lock() -> Result<std::sync::MutexGuard<'static, ()>> {
    CONFIG_SAVE_MUTEX
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| anyhow::anyhow!("config save lock poisoned"))
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
    let vlc_path = fields
        .get("vlc_path")
        .map(|value| value.trim())
        .unwrap_or_default();
    let max_history = fields
        .get("max_history")
        .and_then(|value| value.trim().parse::<usize>().ok())
        .map(crate::config::normalize_max_history)
        .unwrap_or(crate::config::DEFAULT_MAX_HISTORY);
    let language = fields
        .get("language")
        .map(|value| value.trim())
        .unwrap_or_default();
    if !language.is_empty() && crate::i18n::language_from_code(language).is_none() {
        anyhow::bail!(t(lang, "error-language-invalid"));
    }
    if name.is_empty() || name.len() > 128 {
        anyhow::bail!(t(lang, "error-name-length"));
    }
    if name.chars().any(char::is_control) {
        anyhow::bail!(t(lang, "error-name-control"));
    }
    if backend != "mpv" && backend != "vlc" {
        anyhow::bail!(t(lang, "error-unsupported-backend"));
    }
    let active_path = if backend == "mpv" {
        &mpv_path
    } else {
        &vlc_path
    };
    if active_path.is_empty() {
        anyhow::bail!(t(lang, "error-player-path-empty"));
    }
    // Serialize the whole read-modify-save-commit cycle so concurrent saves
    // cannot overwrite each other. The config mutex is only held while
    // cloning and committing the in-memory value; disk IO happens in between
    // without holding the config lock.
    let _save_guard = config_save_lock()?;
    let updated = {
        let guard = config
            .lock()
            .map_err(|_| anyhow::anyhow!("configuration lock poisoned"))?;
        let mut updated = guard.clone();
        updated.device.name = name.to_owned();
        updated.player.backend = backend.to_owned();
        updated.player.mpv_path = mpv_path.to_owned();
        updated.player.vlc_path = vlc_path.to_owned();
        updated.settings.max_history = max_history;
        updated.settings.language = language.to_owned();
        updated
    };
    updated.save()?;
    let mut guard = config
        .lock()
        .map_err(|_| anyhow::anyhow!("configuration lock poisoned"))?;
    *guard = updated;
    crate::i18n::set_lang(crate::i18n::resolve_language(language));
    crate::tray::refresh_menu();
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

fn render(template: &str, vars: &[(&str, &str)]) -> String {
    let mut output = template.to_string();
    for (key, value) in vars {
        let token = ["{{", key, "}}"].concat();
        output = output.replace(&token, &escape_html(value));
    }
    output
}

fn load_template() -> String {
    std::fs::read_to_string("resources/index.html").unwrap_or_else(|_err| {
        crate::log_info!("index.html not on disk, using embedded copy");
        include_str!("../resources/index.html").to_string()
    })
}

fn language_options(lang: Language, current: &str) -> String {
    let system_label = t(lang, "settings-language-system");
    let mut out = String::new();
    out.push_str(&format!(
        "<option value=\"\"{}>{}</option>",
        if current.is_empty() { " selected" } else { "" },
        escape_html(&system_label),
    ));
    for info in crate::i18n::LANGUAGES {
        out.push_str(&format!(
            "<option value=\"{}\"{}>{}</option>",
            escape_html(info.code),
            if info.code == current {
                " selected"
            } else {
                ""
            },
            escape_html(info.name),
        ));
    }
    out
}

fn read_history_file() -> Option<Vec<crate::state::HistoryEntry>> {
    let dir = crate::config::Config::config_dir().ok()?;
    let path = dir.join("history.json");
    let text = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&text).ok()
}

fn history_payload_from(entries: &[crate::state::HistoryEntry]) -> String {
    let items: Vec<_> = entries
        .iter()
        .rev()
        .map(|e| {
            serde_json::json!({
                "time": e.time_str(),
                "title": e.title.clone().unwrap_or_else(|| e.uri.clone()),
                "uri": e.uri,
            })
        })
        .collect();
    serde_json::json!({ "entries": items }).to_string()
}

fn history_payload(limit: usize) -> String {
    let entries = read_history_file().unwrap_or_default();
    let limit = limit.max(1);
    let start = entries.len().saturating_sub(limit);
    history_payload_from(&entries[start..])
}

fn send_event(stream: &mut TcpStream, name: &str, data: &str) -> bool {
    let msg = format!("event: {name}\ndata: {data}\n\n");
    match stream.write_all(msg.as_bytes()) {
        Ok(()) => stream.flush().is_ok(),
        Err(_) => false,
    }
}

fn send_comment(stream: &mut TcpStream) -> bool {
    stream
        .write_all(b": ping\n\n")
        .and_then(|()| stream.flush())
        .is_ok()
}

fn sse_stream(
    stream: &mut TcpStream,
    state: &Arc<Mutex<RendererState>>,
    running: &Arc<AtomicBool>,
    history_limit: usize,
) -> Result<()> {
    stream.write_all(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream; charset=utf-8\r\nCache-Control: no-store\r\nConnection: keep-alive\r\n\r\n",
    )?;
    stream.flush()?;

    let (running_now, address_now) = state
        .lock()
        .map(|s| {
            (
                s.cast == crate::state::CastState::Running,
                s.upnp_address.clone(),
            )
        })
        .unwrap_or((false, None));
    if !send_event(
        stream,
        "status",
        &serde_json::json!({ "running": running_now, "address": address_now }).to_string(),
    ) {
        return Ok(());
    }
    if !send_event(stream, "history", &history_payload(history_limit)) {
        return Ok(());
    }

    let subscription = subscribe();
    let mut ticks = 0u32;
    loop {
        if !running.load(Ordering::Relaxed) {
            break;
        }
        match subscription
            .receiver
            .recv_timeout(Duration::from_millis(500))
        {
            Ok(SettingsEvent::Status { running, address }) => {
                if !send_event(
                    stream,
                    "status",
                    &serde_json::json!({ "running": running, "address": address }).to_string(),
                ) {
                    break;
                }
            }
            Ok(SettingsEvent::History(payload)) => {
                if !send_event(stream, "history", &payload) {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                ticks += 1;
                if ticks.is_multiple_of(30) && !send_comment(stream) {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    Ok(())
}

fn respond(stream: &mut TcpStream, status: &str, content_type: &str, body: &str) -> Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    Ok(())
}

fn serve_asset(
    stream: &mut TcpStream,
    name: &str,
    content_type: &str,
    embedded: &'static [u8],
) -> Result<()> {
    let body = std::fs::read(format!("resources/{name}")).unwrap_or_else(|_| embedded.to_vec());
    respond_bytes(stream, content_type, &body)
}

fn respond_bytes(stream: &mut TcpStream, content_type: &str, body: &[u8]) -> Result<()> {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
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
        // Shut down every tracked connection so request threads blocked in a
        // read or write return immediately instead of waiting for a timeout.
        if let Ok(mut connections) = self.connections.lock() {
            for stream in connections.values() {
                if let Err(error) = stream.shutdown(Shutdown::Both)
                    && error.kind() != std::io::ErrorKind::NotConnected
                {
                    crate::log_warn!("shutting down settings connection: {error}");
                }
            }
            connections.clear();
        }
        // Stop the accept loop.
        if let Some(thread) = self.thread.take()
            && let Err(error) = thread.join()
        {
            crate::log_error!("settings thread panicked: {error:?}");
        }
        // Reap the request threads. Their sockets are already shut down and
        // `running` is false, so give them a short grace period, then join the
        // finished ones and detach any stragglers (they still exit on their
        // own instead of lingering forever).
        let pending: Vec<thread::JoinHandle<()>> = self
            .requests
            .lock()
            .map(|mut handles| handles.drain(..).collect())
            .unwrap_or_default();
        if !pending.is_empty() {
            for _ in 0..100 {
                if pending.iter().all(|handle| handle.is_finished()) {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
        for handle in pending {
            if handle.is_finished() {
                if let Err(error) = handle.join() {
                    crate::log_error!("settings request thread panicked: {error:?}");
                }
            } else {
                crate::log_warn!("settings request thread still busy at shutdown; detaching");
            }
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
        let localhost = format!("http://localhost:{}", address.port());
        let mut headers = HashMap::new();
        assert!(same_origin(&headers, address));
        headers.insert("origin".into(), expected.clone());
        assert!(same_origin(&headers, address));
        headers.insert("origin".into(), localhost.clone());
        assert!(same_origin(&headers, address));
        headers.insert("origin".into(), "http://evil.example".into());
        assert!(!same_origin(&headers, address));
        headers.remove("origin");
        headers.insert("sec-fetch-site".into(), "cross-site".into());
        assert!(!same_origin(&headers, address));
        headers.insert("sec-fetch-site".into(), "same-origin".into());
        assert!(same_origin(&headers, address));
        headers.insert("sec-fetch-site".into(), "none".into());
        assert!(same_origin(&headers, address));
    }

    #[test]
    fn subscription_removed_on_drop() {
        let subscription = subscribe();
        assert_eq!(events().lock().unwrap().len(), 1);
        drop(subscription);
        assert_eq!(events().lock().unwrap().len(), 0);
    }

    #[test]
    fn render_replaces_and_escapes() {
        let out = render("a={{X}} b={{Y}}", &[("X", "<i>"), ("Y", "ok")]);
        assert_eq!(out, "a=&lt;i&gt; b=ok");
    }
}
