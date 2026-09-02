use crate::{
    player::PlayerBackend,
    state::{RendererState, TransportState},
};
use anyhow::{Context, Result};
use std::{
    collections::HashMap,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const DEVICE_DESCRIPTION: &str = include_str!("../resources/device.xml");
const SERVICE_CONNECTION: &str = include_str!("../resources/connection_manager.xml");
const SERVICE_AVTRANSPORT: &str = include_str!("../resources/av_transport.xml");
const SERVICE_RENDERING: &str = include_str!("../resources/rendering_control.xml");
const MAX_REQUEST_SIZE: usize = 1024 * 1024;
const MAX_CALLBACK_SIZE: usize = 512;
const MAX_SUBSCRIPTIONS: usize = 64;
/// Upper bound on concurrently running per-connection request threads.
/// Connections beyond this limit are closed instead of spawning another
/// OS thread.
const MAX_CONNECTION_THREADS: usize = 16;
const CONNECTION_READ_TIMEOUT: Duration = Duration::from_secs(3);
const CONNECTION_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

type SharedPlayer = Arc<Mutex<Box<dyn PlayerBackend>>>;
type SharedState = Arc<Mutex<RendererState>>;
type SharedSubscriptions = Arc<Subscriptions>;

pub struct UpnpServer {
    // Activity is bounded by the connection permit; the stream registry lets
    // shutdown interrupt requests that were accepted before Stop Cast.
    connections: Arc<Mutex<HashMap<u64, TcpStream>>>,
    connection_slots: Arc<AtomicUsize>,
    running: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
    port: u16,
}

struct ConnectionGuard {
    id: u64,
    connections: Arc<Mutex<HashMap<u64, TcpStream>>>,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        if let Ok(mut connections) = self.connections.lock() {
            connections.remove(&self.id);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EventService {
    AvTransport,
    RenderingControl,
    ConnectionManager,
}

struct Subscription {
    callback: String,
    service: EventService,
    expires_at: Instant,
    sequence: u32,
    notify_lock: Arc<Mutex<()>>,
}

/// Shared GENA subscription state. NOTIFY ordering for a single SID is
/// guaranteed by the per-subscription `notify_lock`; different SIDs notify
/// independently of each other.
struct Subscriptions {
    entries: Mutex<HashMap<String, Subscription>>,
}

/// RAII slot that bounds how many OS threads a server may spawn at once.
/// Released back to the shared counter when the owning thread finishes.
struct ThreadPermit {
    counter: Arc<AtomicUsize>,
}

impl ThreadPermit {
    fn try_acquire(counter: &Arc<AtomicUsize>, limit: usize) -> Option<Self> {
        loop {
            let current = counter.load(Ordering::Relaxed);
            if current >= limit {
                return None;
            }
            if counter
                .compare_exchange(current, current + 1, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return Some(Self {
                    counter: Arc::clone(counter),
                });
            }
        }
    }
}

impl Drop for ThreadPermit {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::AcqRel);
    }
}

struct HttpRequest {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: String,
}

struct HttpResponse {
    status: &'static str,
    content_type: &'static str,
    headers: Vec<(String, String)>,
    body: String,
}

impl UpnpServer {
    pub fn start(
        name: &str,
        udn: &str,
        player: SharedPlayer,
        state: SharedState,
        max_history: usize,
    ) -> Result<Self> {
        let listener = TcpListener::bind(("0.0.0.0", 0))?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let running = Arc::new(AtomicBool::new(true));
        let active = Arc::clone(&running);
        let subscriptions = Arc::new(Subscriptions {
            entries: Mutex::new(HashMap::new()),
        });
        let connection_slots = Arc::new(AtomicUsize::new(0));
        let connection_slots_out = Arc::clone(&connection_slots);
        let connections = Arc::new(Mutex::new(HashMap::new()));
        let connection_ids = Arc::new(AtomicU64::new(1));
        let name = name.to_owned();
        let udn = udn.to_owned();
        let tracked_connections = Arc::clone(&connections);
        let tracked_ids = Arc::clone(&connection_ids);
        let thread = thread::Builder::new()
            .name("upnp-http".into())
            .spawn(move || {
                while active.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            // Bounded concurrency: acquire the connection
                            // permit before anything is registered, so an
                            // over-limit or untrackable connection leaves no
                            // registry entry behind.
                            let Some(permit) = ThreadPermit::try_acquire(
                                &connection_slots,
                                MAX_CONNECTION_THREADS,
                            ) else {
                                crate::log_warn!(
                                    "UPnP connection limit ({MAX_CONNECTION_THREADS}) reached; closing new connection"
                                );
                                drop(stream);
                                continue;
                            };
                            let tracked = match stream.try_clone() {
                                Ok(tracked) => tracked,
                                Err(error) => {
                                    crate::log_warn!("cannot track UPnP connection: {error}");
                                    drop(permit);
                                    continue;
                                }
                            };
                            let connection_id = tracked_ids.fetch_add(1, Ordering::Relaxed);
                            match tracked_connections.lock() {
                                Ok(mut active_connections) => {
                                    active_connections.insert(connection_id, tracked);
                                }
                                Err(_) => {
                                    crate::log_warn!("UPnP connection registry is poisoned");
                                    drop(permit);
                                    drop(stream);
                                    continue;
                                }
                            }
                            if !active.load(Ordering::Relaxed) {
                                if let Ok(mut active_connections) = tracked_connections.lock() {
                                    active_connections.remove(&connection_id);
                                }
                                drop(stream);
                                drop(permit);
                                continue;
                            }
                            // Hand every accepted connection to its own thread
                            // so a slow or stalled client cannot block the
                            // accept loop for everyone else. Shared state is
                            // kept safe through the cloned Arc handles.
                            let name = name.clone();
                            let udn = udn.clone();
                            let player = Arc::clone(&player);
                            let state = Arc::clone(&state);
                            let subscriptions = Arc::clone(&subscriptions);
                            let connection_active = Arc::clone(&active);
                            let connection_registry = Arc::clone(&tracked_connections);
                            if let Err(error) = thread::Builder::new()
                                .name("upnp-conn".into())
                                .spawn(move || {
                                    let _permit = permit;
                                    let _connection = ConnectionGuard {
                                        id: connection_id,
                                        connections: connection_registry,
                                    };
                                    if let Err(error) = serve(
                                        stream,
                                        &name,
                                        &udn,
                                        &player,
                                        &state,
                                        &subscriptions,
                                        max_history,
                                        &connection_active,
                                    ) {
                                        crate::log_error!("UPnP request failed: {error:#}");
                                    }
                                })
                            {
                                if let Ok(mut active_connections) = tracked_connections.lock() {
                                    active_connections.remove(&connection_id);
                                }
                                crate::log_error!(
                                    "failed to spawn UPnP connection thread: {error}"
                                );
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(30));
                        }
                        Err(error) => {
                            crate::log_error!("UPnP listener failed: {error}");
                            break;
                        }
                    }
                    remove_expired_subscriptions(&subscriptions);
                }
            })?;
        crate::log_info!("UPnP HTTP server listening on port {}", address.port());
        Ok(Self {
            running,
            thread: Some(thread),
            port: address.port(),
            connections,
            connection_slots: connection_slots_out,
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

#[allow(clippy::too_many_arguments)]
fn serve(
    mut stream: TcpStream,
    name: &str,
    udn: &str,
    player: &SharedPlayer,
    state: &SharedState,
    subscriptions: &SharedSubscriptions,
    max_history: usize,
    running: &Arc<AtomicBool>,
) -> Result<()> {
    stream.set_read_timeout(Some(CONNECTION_READ_TIMEOUT))?;
    stream.set_write_timeout(Some(CONNECTION_WRITE_TIMEOUT))?;
    let request = match read_request(&mut stream) {
        Ok(req) => req,
        Err(error) => {
            // A client that connected but never sent a complete request only
            // tripped the read timeout; treat that as a quiet close.
            if error.downcast_ref::<std::io::Error>().is_some_and(|io| {
                matches!(
                    io.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                )
            }) {
                return Ok(());
            }
            let msg = error.to_string();
            if msg.contains("connection closed") || msg.contains("incomplete") {
                return Ok(());
            }
            return Err(error);
        }
    };
    if !running.load(Ordering::Relaxed) {
        return Ok(());
    }
    let response = route(
        &request,
        name,
        udn,
        player,
        state,
        subscriptions,
        max_history,
        running,
    );
    write_response(&mut stream, response)
}

#[allow(clippy::too_many_arguments)]
fn route(
    request: &HttpRequest,
    name: &str,
    udn: &str,
    player: &SharedPlayer,
    state: &SharedState,
    subscriptions: &SharedSubscriptions,
    max_history: usize,
    running: &AtomicBool,
) -> HttpResponse {
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/device.xml") => xml_response(device_xml(name, udn)),
        ("GET", "/connection_manager.xml") => xml_response(SERVICE_CONNECTION.into()),
        ("GET", "/av_transport.xml") => xml_response(SERVICE_AVTRANSPORT.into()),
        ("GET", "/rendering_control.xml") => xml_response(SERVICE_RENDERING.into()),
        ("POST", "/ConnectionManager/control") => soap_route(
            request,
            EventService::ConnectionManager,
            player,
            state,
            subscriptions,
            max_history,
            running,
        ),
        ("POST", "/AVTransport/control") => soap_route(
            request,
            EventService::AvTransport,
            player,
            state,
            subscriptions,
            max_history,
            running,
        ),
        ("POST", "/RenderingControl/control") => soap_route(
            request,
            EventService::RenderingControl,
            player,
            state,
            subscriptions,
            max_history,
            running,
        ),
        ("SUBSCRIBE", path) => match event_service(path) {
            Some(service) => subscribe(request, service, player, state, subscriptions),
            None => plain_response("404 Not Found", "Not found\n"),
        },
        ("UNSUBSCRIBE", path) => match event_service(path) {
            Some(service) => unsubscribe(request, service, subscriptions),
            None => plain_response("404 Not Found", "Not found\n"),
        },
        _ => plain_response("404 Not Found", "Not found\n"),
    }
}

fn soap_route(
    request: &HttpRequest,
    service: EventService,
    player: &SharedPlayer,
    state: &SharedState,
    subscriptions: &SharedSubscriptions,
    max_history: usize,
    running: &AtomicBool,
) -> HttpResponse {
    let Some(action) = request.headers.get("soapaction").and_then(|value| {
        value
            .trim_matches(|character| character == '"' || character == '\'')
            .rsplit_once('#')
            .map(|(_, action)| action)
    }) else {
        return soap_fault_response(401, "Invalid Action");
    };

    let result = execute_action(
        action,
        service,
        request,
        player,
        state,
        max_history,
        running,
    );
    match result {
        Ok((body, changed)) => {
            if changed {
                let protocol_info = player
                    .lock()
                    .map(|p| p.sink_protocol_info().to_owned())
                    .unwrap_or_default();
                notify_subscribers(subscriptions, state, service, &protocol_info);
            }
            HttpResponse {
                status: "200 OK",
                content_type: "text/xml; charset=utf-8",
                headers: vec![("EXT".into(), String::new())],
                body: soap_envelope(&body),
            }
        }
        Err(error) => {
            if error.code == 401 {
                crate::log_warn!("unknown SOAP action: {action}");
            }
            soap_fault_response(error.code, error.description)
        }
    }
}

fn execute_action(
    action: &str,
    service: EventService,
    request: &HttpRequest,
    player: &SharedPlayer,
    state: &SharedState,
    max_history: usize,
    running: &AtomicBool,
) -> std::result::Result<(String, bool), UpnpError> {
    // Every SOAP action funnels through here before touching the player, so
    // one check protects the old backend after Stop Cast has torn it down:
    // fault immediately instead of operating on a released backend.
    if !running.load(Ordering::Relaxed) {
        return Err(UpnpError::new(501, "Action Failed"));
    }
    match service {
        EventService::ConnectionManager => {
            execute_connection_manager(action, service, request, player)
        }
        EventService::AvTransport => {
            execute_av_transport(action, service, request, player, state, max_history)
        }
        EventService::RenderingControl => {
            execute_rendering_control(action, service, request, player, state)
        }
    }
}

fn execute_connection_manager(
    action: &str,
    service: EventService,
    _request: &HttpRequest,
    player: &SharedPlayer,
) -> std::result::Result<(String, bool), UpnpError> {
    match action {
        "GetProtocolInfo" => {
            let protocol_info = player
                .lock()
                .map(|p| p.sink_protocol_info().to_owned())
                .unwrap_or_default();
            Ok((
                action_response(
                    service,
                    action,
                    &format!(
                        "<Source></Source><Sink>{}</Sink>",
                        escape_xml(&protocol_info)
                    ),
                ),
                false,
            ))
        }
        "GetCurrentConnectionIDs" => Ok((
            action_response(service, action, "<ConnectionIDs>0</ConnectionIDs>"),
            false,
        )),
        "GetCurrentConnectionInfo" => Ok((
            action_response(
                service,
                action,
                "<RcsID>0</RcsID><AVTransportID>0</AVTransportID><ProtocolInfo></ProtocolInfo><PeerConnectionManager></PeerConnectionManager><PeerConnectionID>-1</PeerConnectionID><Direction>Input</Direction><Status>OK</Status>",
            ),
            false,
        )),
        _ => Err(UpnpError::new(401, "Invalid Action")),
    }
}

fn execute_av_transport(
    action: &str,
    service: EventService,
    request: &HttpRequest,
    player: &SharedPlayer,
    state: &SharedState,
    max_history: usize,
) -> std::result::Result<(String, bool), UpnpError> {
    match action {
        "SetAVTransportURI" => {
            require_instance_zero(request)?;
            let uri = decode_xml(&required_value(&request.body, "CurrentURI")?);
            let title = xml_value(&request.body, "CurrentURIMetaData")
                .as_deref()
                .map(decode_xml)
                .and_then(|metadata| xml_value(&metadata, "dc:title"))
                .map(|value| decode_xml(&value));
            if uri.is_empty() {
                // Per AVTransport spec, an empty CurrentURI stops playback
                // and removes the current media.
                lock_player(player)?.stop().map_err(player_error)?;
                let mut state = lock_state(state)?;
                state.transport = TransportState::NoMediaPresent;
                state.uri = None;
                state.title = None;
                state.position = Duration::ZERO;
                state.duration = None;
                return Ok((action_response(service, action, ""), true));
            }
            // Never hold the state lock while talking to the player backend:
            // load() performs external IPC that can block for seconds. Mark
            // TRANSITIONING briefly, release the lock, then roll back on
            // failure so a failed load never leaves the renderer stuck.
            let previous_transport = lock_state(state)?.transport;
            lock_state(state)?.transport = TransportState::Transitioning;
            // Scope the player guard so it is released before the state lock
            // is re-acquired for the rollback below.
            let load_result = {
                let mut player = lock_player(player)?;
                player.load(&uri, title.as_deref())
            };
            if let Err(error) = load_result {
                lock_state(state)?.transport = previous_transport;
                return Err(player_error(error));
            }
            crate::log_info!("loaded media: {}", title.as_deref().unwrap_or(&uri));
            // Persist history without holding any lock: history IO may block
            // on disk and must not stall other UPnP requests.
            if let Err(error) = crate::config::Config::append_history(
                crate::state::HistoryEntry::new(uri.clone(), title.clone()),
                max_history,
            ) {
                crate::log_error!("saving history: {error:#}");
            }
            let mut state = lock_state(state)?;
            state.uri = Some(uri);
            state.title = title;
            state.position = Duration::ZERO;
            state.duration = None;
            state.transport = TransportState::Stopped;
            Ok((action_response(service, action, ""), true))
        }
        "Play" => {
            require_instance_zero(request)?;
            // Snapshot the state first and release the lock: status()/load()/
            // play() below perform player IPC that must not run under the
            // state lock.
            let (uri, title) = {
                let state = lock_state(state)?;
                (state.uri.clone(), state.title.clone())
            };
            let Some(uri) = uri else {
                return Err(UpnpError::new(714, "No Such Resource"));
            };
            let mut player = lock_player(player)?;
            // Stop releases the player backend. If the backend no longer
            // holds any media, reload the stored URI so Play after Stop
            // restarts playback instead of silently doing nothing.
            let has_media = player
                .status()
                .is_ok_and(|status| status.playing || status.paused || status.duration.is_some());
            let reloaded = !has_media;
            if reloaded {
                player.load(&uri, title.as_deref()).map_err(player_error)?;
            }
            player.play().map_err(player_error)?;
            drop(player);
            let mut state = lock_state(state)?;
            if reloaded {
                state.position = Duration::ZERO;
                state.duration = None;
            }
            state.transport = TransportState::Playing;
            Ok((action_response(service, action, ""), true))
        }
        "Pause" => {
            require_instance_zero(request)?;
            let has_uri = lock_state(state)?.uri.is_some();
            if !has_uri {
                return Err(UpnpError::new(714, "No Such Resource"));
            }
            lock_player(player)?.pause().map_err(player_error)?;
            lock_state(state)?.transport = TransportState::PausedPlayback;
            Ok((action_response(service, action, ""), true))
        }
        "Stop" => {
            require_instance_zero(request)?;
            lock_player(player)?.stop().map_err(player_error)?;
            let mut state = lock_state(state)?;
            // Stop resets the position but must not clear the current URI:
            // the media stays loaded so Play can restart it (UPnP AVTransport).
            state.transport = if state.uri.is_some() {
                TransportState::Stopped
            } else {
                TransportState::NoMediaPresent
            };
            state.position = Duration::ZERO;
            Ok((action_response(service, action, ""), true))
        }
        "Seek" => {
            require_instance_zero(request)?;
            if required_value(&request.body, "Unit")? != "REL_TIME" {
                return Err(UpnpError::new(710, "Seek Mode Not Supported"));
            }
            let position = parse_upnp_time(&required_value(&request.body, "Target")?)?;
            let exceeds_duration = lock_state(state)?
                .duration
                .is_some_and(|duration| position > duration);
            if exceeds_duration {
                return Err(UpnpError::new(711, "Illegal seek target"));
            }
            lock_player(player)?.seek(position).map_err(player_error)?;
            lock_state(state)?.position = position;
            Ok((action_response(service, action, ""), true))
        }
        "GetTransportInfo" => {
            require_instance_zero(request)?;
            refresh_player_state(player, state);
            let state = lock_state(state)?;
            Ok((
                action_response(
                    service,
                    action,
                    &format!(
                        "<CurrentTransportState>{}</CurrentTransportState><CurrentTransportStatus>OK</CurrentTransportStatus><CurrentSpeed>1</CurrentSpeed>",
                        state.transport.upnp_value()
                    ),
                ),
                false,
            ))
        }
        "GetPositionInfo" => {
            require_instance_zero(request)?;
            refresh_player_state(player, state);
            let state = lock_state(state)?;
            let duration = format_upnp_time(state.duration.unwrap_or_default());
            let position = format_upnp_time(state.position);
            Ok((
                action_response(
                    service,
                    action,
                    &format!(
                        "<Track>1</Track><TrackDuration>{duration}</TrackDuration><TrackMetaData></TrackMetaData><TrackURI>{}</TrackURI><RelTime>{position}</RelTime><AbsTime>{position}</AbsTime><RelCount>2147483647</RelCount><AbsCount>2147483647</AbsCount>",
                        escape_xml(state.uri.as_deref().unwrap_or_default())
                    ),
                ),
                false,
            ))
        }
        "GetMediaInfo" => {
            require_instance_zero(request)?;
            refresh_player_state(player, state);
            let state = lock_state(state)?;
            Ok((
                action_response(
                    service,
                    action,
                    &format!(
                        "<NrTracks>1</NrTracks><MediaDuration>{}</MediaDuration><CurrentURI>{}</CurrentURI><CurrentURIMetaData></CurrentURIMetaData><NextURI></NextURI><NextURIMetaData></NextURIMetaData><PlayMedium>NETWORK</PlayMedium><RecordMedium>NOT_IMPLEMENTED</RecordMedium><WriteStatus>NOT_IMPLEMENTED</WriteStatus>",
                        format_upnp_time(state.duration.unwrap_or_default()),
                        escape_xml(state.uri.as_deref().unwrap_or_default())
                    ),
                ),
                false,
            ))
        }
        "GetTransportSettings" => {
            require_instance_zero(request)?;
            Ok((
                action_response(
                    service,
                    action,
                    "<PlayMode>NORMAL</PlayMode><RecQualityMode>NOT_IMPLEMENTED</RecQualityMode>",
                ),
                false,
            ))
        }
        _ => Err(UpnpError::new(401, "Invalid Action")),
    }
}

fn execute_rendering_control(
    action: &str,
    service: EventService,
    request: &HttpRequest,
    player: &SharedPlayer,
    state: &SharedState,
) -> std::result::Result<(String, bool), UpnpError> {
    match action {
        "GetVolume" => {
            require_instance_zero(request)?;
            require_master_channel(request)?;
            refresh_player_state(player, state);
            let volume = lock_state(state)?.volume;
            Ok((
                action_response(
                    service,
                    action,
                    &format!("<CurrentVolume>{volume}</CurrentVolume>"),
                ),
                false,
            ))
        }
        "SetVolume" => {
            require_instance_zero(request)?;
            require_master_channel(request)?;
            let volume = required_value(&request.body, "DesiredVolume")?
                .parse::<u16>()
                .map_err(|_| UpnpError::new(402, "Invalid Args"))?;
            if volume > 100 {
                return Err(UpnpError::new(402, "Invalid Args"));
            }
            let volume = volume as u8;
            let mut player = lock_player(player)?;
            player.set_volume(volume).map_err(player_error)?;
            drop(player);
            lock_state(state)?.volume = volume;
            Ok((action_response(service, action, ""), true))
        }
        "GetMute" => {
            require_instance_zero(request)?;
            require_master_channel(request)?;
            refresh_player_state(player, state);
            let muted = u8::from(lock_state(state)?.muted);
            Ok((
                action_response(
                    service,
                    action,
                    &format!("<CurrentMute>{muted}</CurrentMute>"),
                ),
                false,
            ))
        }
        "SetMute" => {
            require_instance_zero(request)?;
            require_master_channel(request)?;
            let muted = parse_bool(&required_value(&request.body, "DesiredMute")?)?;
            let mut player = lock_player(player)?;
            player.set_mute(muted).map_err(player_error)?;
            drop(player);
            lock_state(state)?.muted = muted;
            Ok((action_response(service, action, ""), true))
        }
        _ => Err(UpnpError::new(401, "Invalid Action")),
    }
}

fn subscribe(
    request: &HttpRequest,
    service: EventService,
    player: &SharedPlayer,
    state: &SharedState,
    subscriptions: &SharedSubscriptions,
) -> HttpResponse {
    let timeout = parse_timeout(request.headers.get("timeout").map(String::as_str));
    if let Some(sid) = request.headers.get("sid") {
        let mut guard = match subscriptions.entries.lock() {
            Ok(guard) => guard,
            Err(_) => {
                crate::log_warn!("failed to lock subscription state for renewal");
                return plain_response("500 Internal Server Error", "subscription state failed\n");
            }
        };
        let Some(subscription) = guard.get_mut(sid) else {
            return plain_response("412 Precondition Failed", "unknown SID\n");
        };
        if subscription.service != service {
            // A subscription may only be renewed on the eventSubURL of the
            // service it was created for.
            crate::log_warn!("GENA renewal SID={sid} used on a different service URL");
            return plain_response(
                "412 Precondition Failed",
                "SID does not match this service\n",
            );
        }
        subscription.expires_at = Instant::now() + timeout;
        crate::log_info!("GENA subscription renewed SID={sid}");
        return subscription_response(sid, timeout);
    }

    if request.headers.get("nt").map(String::as_str) != Some("upnp:event") {
        return plain_response("412 Precondition Failed", "NT must be upnp:event\n");
    }
    let Some(callback) = request
        .headers
        .get("callback")
        .and_then(|value| value.trim().strip_prefix('<'))
        .and_then(|value| value.strip_suffix('>'))
    else {
        return plain_response("412 Precondition Failed", "CALLBACK is required\n");
    };
    if !callback.starts_with("http://") {
        return plain_response(
            "412 Precondition Failed",
            "only HTTP callbacks are supported\n",
        );
    }
    if callback.len() > MAX_CALLBACK_SIZE {
        return plain_response("412 Precondition Failed", "CALLBACK is too long\n");
    }
    // Reuse the same URL policy as send_notify so we never create a
    // subscription that is guaranteed to fail: reject callbacks that do not
    // parse, do not resolve, or resolve only to loopback, unspecified, or
    // multicast addresses. RFC1918, ULA, and link-local destinations remain
    // accepted.
    if let Err(error) = parse_http_url(callback) {
        crate::log_warn!("rejecting unusable GENA CALLBACK {callback}: {error:#}");
        return plain_response("412 Precondition Failed", "CALLBACK is not usable\n");
    }
    let sid = new_sid();
    let subscription = Subscription {
        callback: callback.to_owned(),
        service,
        expires_at: Instant::now() + timeout,
        sequence: 0,
        notify_lock: Arc::new(Mutex::new(())),
    };
    if let Ok(mut guard) = subscriptions.entries.lock() {
        if guard.len() >= MAX_SUBSCRIPTIONS {
            return plain_response("503 Service Unavailable", "subscription limit reached\n");
        }
        guard.insert(sid.clone(), subscription);
        crate::log_info!("GENA subscription created SID={sid}");
    } else {
        return plain_response("500 Internal Server Error", "subscription state failed\n");
    }
    let sink_protocol_info = player
        .lock()
        .map(|p| p.sink_protocol_info().to_owned())
        .unwrap_or_default();
    notify_sid(subscriptions, state, &sid, &sink_protocol_info);
    subscription_response(&sid, timeout)
}

fn unsubscribe(
    request: &HttpRequest,
    service: EventService,
    subscriptions: &SharedSubscriptions,
) -> HttpResponse {
    let Some(sid) = request.headers.get("sid") else {
        return plain_response("412 Precondition Failed", "SID is required\n");
    };
    match subscriptions.entries.lock() {
        Ok(mut guard) => {
            if guard
                .get(sid)
                .is_some_and(|subscription| subscription.service == service)
            {
                guard.remove(sid);
                HttpResponse {
                    status: "200 OK",
                    content_type: "text/plain",
                    headers: Vec::new(),
                    body: String::new(),
                }
            } else {
                plain_response("412 Precondition Failed", "unknown SID\n")
            }
        }
        Err(_) => plain_response("500 Internal Server Error", "subscription state failed\n"),
    }
}

fn notify_subscribers(
    subscriptions: &SharedSubscriptions,
    state: &SharedState,
    service: EventService,
    sink_protocol_info: &str,
) {
    let sids = subscriptions
        .entries
        .lock()
        .map(|guard| {
            guard
                .iter()
                .filter(|(_, sub)| sub.service == service)
                .map(|(sid, _)| sid.clone())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for sid in sids {
        notify_sid(subscriptions, state, &sid, sink_protocol_info);
    }
}

fn notify_sid(
    subscriptions: &SharedSubscriptions,
    state: &SharedState,
    sid: &str,
    sink_protocol_info: &str,
) {
    // Serialize this SID: the per-subscription lock is held across sequence
    // allocation and the network send, so NOTIFYs to one subscriber arrive in
    // FIFO relative to notification dispatch. The state snapshot is taken
    // after acquiring this lock, so a later notification cannot capture a
    // newer state and then be followed by an older snapshot. The send is synchronous
    // and bounded by send_notify's 2s connect / 2s write timeouts, so a slow
    // subscriber delays its own notifications but cannot reorder them.
    let notify_lock = match subscriptions.entries.lock() {
        Ok(guard) => match guard.get(sid) {
            Some(subscription) => Arc::clone(&subscription.notify_lock),
            None => return,
        },
        Err(_) => {
            crate::log_warn!("failed to lock subscription state for GENA notification");
            return;
        }
    };
    let _notify_guard = match notify_lock.lock() {
        Ok(guard) => guard,
        Err(_) => {
            crate::log_warn!("subscription notify lock is poisoned");
            return;
        }
    };
    let state = match state.lock() {
        Ok(state) => state.clone(),
        Err(_) => {
            crate::log_warn!("failed to lock renderer state for GENA notification");
            return;
        }
    };
    let (callback, sequence, service) = match subscriptions.entries.lock() {
        Ok(mut guard) => match guard.get_mut(sid) {
            Some(subscription) => {
                let sequence = subscription.sequence;
                subscription.sequence = subscription.sequence.wrapping_add(1);
                (
                    subscription.callback.clone(),
                    sequence,
                    subscription.service,
                )
            }
            None => return,
        },
        Err(_) => {
            crate::log_warn!("failed to lock subscription state for GENA notification");
            return;
        }
    };
    let body = event_body(service, &state, sink_protocol_info);
    if let Err(error) = send_notify(&callback, sid, sequence, &body) {
        crate::log_error!("GENA notification to {callback} failed: {error:#}");
    }
}

fn send_notify(callback: &str, sid: &str, sequence: u32, body: &str) -> Result<()> {
    let target = parse_http_url(callback)?;
    let mut stream = TcpStream::connect_timeout(&target.address, Duration::from_secs(2))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    write!(
        stream,
        "NOTIFY {} HTTP/1.1\r\nHOST: {}\r\nCONTENT-TYPE: text/xml; charset=utf-8\r\nNT: upnp:event\r\nNTS: upnp:propchange\r\nSID: {sid}\r\nSEQ: {sequence}\r\nCONTENT-LENGTH: {}\r\nCONNECTION: close\r\n\r\n{body}",
        target.path,
        target.host_header,
        body.len()
    )?;
    Ok(())
}

struct HttpTarget {
    address: SocketAddr,
    host_header: String,
    path: String,
}

fn parse_http_url(url: &str) -> Result<HttpTarget> {
    let remainder = url
        .strip_prefix("http://")
        .context("callback is not HTTP")?;
    let (authority, path) = remainder
        .split_once('/')
        .map(|(authority, path)| (authority, format!("/{path}")))
        .unwrap_or((remainder, "/".into()));
    if authority.is_empty() {
        anyhow::bail!("callback host is empty");
    }
    let socket_authority = if authority
        .rsplit_once(':')
        .is_some_and(|(_, port)| port.parse::<u16>().is_ok())
    {
        authority.to_owned()
    } else {
        format!("{authority}:80")
    };
    let mut addresses = socket_authority
        .to_socket_addrs()
        .context("resolving callback host")?
        .filter(|address| {
            let ip = address.ip();
            !ip.is_loopback() && !ip.is_unspecified() && !ip.is_multicast()
        });
    let address = addresses
        .next()
        .context("callback host resolves only to local or multicast addresses")?;
    Ok(HttpTarget {
        address,
        host_header: authority.into(),
        path,
    })
}

fn event_body(service: EventService, state: &RendererState, sink_protocol_info: &str) -> String {
    let properties = match service {
        EventService::AvTransport => {
            let last_change = format!(
                "<Event xmlns=\"urn:schemas-upnp-org:metadata-1-0/AVT/\"><InstanceID val=\"0\"><TransportState val=\"{}\"/><CurrentTrackURI val=\"{}\"/><RelativeTimePosition val=\"{}\"/><CurrentTrackDuration val=\"{}\"/></InstanceID></Event>",
                state.transport.upnp_value(),
                escape_xml(state.uri.as_deref().unwrap_or_default()),
                format_upnp_time(state.position),
                format_upnp_time(state.duration.unwrap_or_default()),
            );
            format!(
                "<e:property><LastChange>{}</LastChange></e:property>",
                escape_xml(&last_change)
            )
        }
        EventService::RenderingControl => {
            let last_change = format!(
                "<Event xmlns=\"urn:schemas-upnp-org:metadata-1-0/RCS/\"><InstanceID val=\"0\"><Volume channel=\"Master\" val=\"{}\"/><Mute channel=\"Master\" val=\"{}\"/></InstanceID></Event>",
                state.volume,
                u8::from(state.muted),
            );
            format!(
                "<e:property><LastChange>{}</LastChange></e:property>",
                escape_xml(&last_change)
            )
        }
        EventService::ConnectionManager => format!(
            "<e:property><SourceProtocolInfo></SourceProtocolInfo></e:property><e:property><SinkProtocolInfo>{}</SinkProtocolInfo></e:property><e:property><CurrentConnectionIDs>0</CurrentConnectionIDs></e:property>",
            escape_xml(sink_protocol_info)
        ),
    };
    format!(
        "<?xml version=\"1.0\"?><e:propertyset xmlns:e=\"urn:schemas-upnp-org:event-1-0\">{properties}</e:propertyset>"
    )
}

fn read_request(stream: &mut TcpStream) -> Result<HttpRequest> {
    let mut data = Vec::new();
    let mut chunk = [0; 4096];
    let header_end = loop {
        let size = stream.read(&mut chunk)?;
        if size == 0 {
            anyhow::bail!("connection closed before headers complete");
        }
        data.extend_from_slice(&chunk[..size]);
        if data.len() > MAX_REQUEST_SIZE {
            anyhow::bail!("UPnP request exceeds 1 MiB");
        }
        if let Some(header_end) = crate::util::find_bytes(&data, b"\r\n\r\n") {
            break header_end;
        }
    };
    let headers = String::from_utf8_lossy(&data[..header_end]);
    let content_length = header_value(&headers, "content-length")
        .map(|value| value.parse::<usize>().context("invalid Content-Length"))
        .transpose()?
        .unwrap_or(0);
    let body_end = header_end
        .checked_add(4)
        .and_then(|value| value.checked_add(content_length))
        .context("invalid Content-Length")?;
    if body_end > MAX_REQUEST_SIZE {
        anyhow::bail!("UPnP request exceeds 1 MiB");
    }
    while data.len() < body_end {
        let size = stream.read(&mut chunk)?;
        if size == 0 {
            anyhow::bail!("connection closed before request body completed");
        }
        data.extend_from_slice(&chunk[..size]);
        if data.len() > MAX_REQUEST_SIZE {
            anyhow::bail!("UPnP request exceeds 1 MiB");
        }
    }
    let headers_text =
        std::str::from_utf8(&data[..header_end]).context("HTTP headers are not UTF-8")?;
    let mut lines = headers_text.lines();
    let mut request_line = lines.next().unwrap_or_default().split_whitespace();
    let method = request_line.next().unwrap_or_default().to_owned();
    let path = request_line
        .next()
        .unwrap_or_default()
        .split('?')
        .next()
        .unwrap_or_default()
        .to_owned();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let body = String::from_utf8(data[header_end + 4..body_end].to_vec())
        .context("HTTP body is not UTF-8")?;
    Ok(HttpRequest {
        method,
        path,
        headers,
        body,
    })
}

fn write_response(stream: &mut TcpStream, response: HttpResponse) -> Result<()> {
    write!(
        stream,
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        response.status,
        response.content_type,
        response.body.len()
    )?;
    for (name, value) in response.headers {
        write!(stream, "{name}: {value}\r\n")?;
    }
    write!(stream, "\r\n{}", response.body)?;
    Ok(())
}

fn xml_response(body: String) -> HttpResponse {
    HttpResponse {
        status: "200 OK",
        content_type: "text/xml; charset=utf-8",
        headers: Vec::new(),
        body,
    }
}
fn plain_response(status: &'static str, body: &str) -> HttpResponse {
    HttpResponse {
        status,
        content_type: "text/plain; charset=utf-8",
        headers: Vec::new(),
        body: body.into(),
    }
}
fn device_xml(name: &str, udn: &str) -> String {
    DEVICE_DESCRIPTION
        .replace("{{DEVICE_NAME}}", &escape_xml(name))
        .replace("{{UDN}}", &escape_xml(udn))
}

fn action_response(service: EventService, action: &str, values: &str) -> String {
    format!(
        "<u:{action}Response xmlns:u=\"{}\">{values}</u:{action}Response>",
        service.urn()
    )
}

fn soap_envelope(body: &str) -> String {
    format!(
        "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body>{body}</s:Body></s:Envelope>"
    )
}

fn soap_fault_response(code: u16, description: &str) -> HttpResponse {
    let fault = format!(
        "<s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring><detail><UPnPError xmlns=\"urn:schemas-upnp-org:control-1-0\"><errorCode>{code}</errorCode><errorDescription>{}</errorDescription></UPnPError></detail></s:Fault>",
        escape_xml(description)
    );
    HttpResponse {
        status: "500 Internal Server Error",
        content_type: "text/xml; charset=utf-8",
        headers: Vec::new(),
        body: soap_envelope(&fault),
    }
}

impl EventService {
    fn urn(self) -> &'static str {
        match self {
            Self::AvTransport => "urn:schemas-upnp-org:service:AVTransport:1",
            Self::RenderingControl => "urn:schemas-upnp-org:service:RenderingControl:1",
            Self::ConnectionManager => "urn:schemas-upnp-org:service:ConnectionManager:1",
        }
    }
}

fn event_service(path: &str) -> Option<EventService> {
    match path {
        "/AVTransport/event" => Some(EventService::AvTransport),
        "/RenderingControl/event" => Some(EventService::RenderingControl),
        "/ConnectionManager/event" => Some(EventService::ConnectionManager),
        _ => None,
    }
}

#[derive(Debug)]
struct UpnpError {
    code: u16,
    description: &'static str,
}
impl UpnpError {
    fn new(code: u16, description: &'static str) -> Self {
        Self { code, description }
    }
}
fn player_error(error: anyhow::Error) -> UpnpError {
    if !super::player::is_already_reported(&error) {
        crate::log_error!("player action failed: {error:#}");
    }
    UpnpError::new(501, "Action Failed")
}
fn lock_player(
    player: &SharedPlayer,
) -> std::result::Result<std::sync::MutexGuard<'_, Box<dyn PlayerBackend>>, UpnpError> {
    player
        .lock()
        .map_err(|_| UpnpError::new(501, "Action Failed"))
}
fn lock_state(
    state: &SharedState,
) -> std::result::Result<std::sync::MutexGuard<'_, RendererState>, UpnpError> {
    state
        .lock()
        .map_err(|_| UpnpError::new(501, "Action Failed"))
}

fn refresh_player_state(player: &SharedPlayer, state: &SharedState) {
    let status = player.lock().ok().and_then(|mut player| {
        player
            .status()
            .map_err(|error| crate::log_error!("reading player status: {error:#}"))
            .ok()
    });
    if let (Some(status), Ok(mut state)) = (status, state.lock()) {
        state.position = status.position;
        state.duration = status.duration;
        state.volume = status.volume;
        state.muted = status.muted;
        state.transport = if status.playing {
            TransportState::Playing
        } else if status.paused {
            // Some backends (mpv with keep-open) report "paused" when the
            // media has naturally reached the end; report that as Stopped.
            if status
                .duration
                .is_some_and(|duration| status.position >= duration)
            {
                TransportState::Stopped
            } else {
                TransportState::PausedPlayback
            }
        } else if state.uri.is_some() {
            TransportState::Stopped
        } else {
            TransportState::NoMediaPresent
        };
    }
}

fn require_instance_zero(request: &HttpRequest) -> std::result::Result<(), UpnpError> {
    if required_value(&request.body, "InstanceID")? == "0" {
        Ok(())
    } else {
        Err(UpnpError::new(718, "Invalid InstanceID"))
    }
}
fn require_master_channel(request: &HttpRequest) -> std::result::Result<(), UpnpError> {
    if required_value(&request.body, "Channel")? == "Master" {
        Ok(())
    } else {
        Err(UpnpError::new(402, "Invalid Args"))
    }
}
fn required_value(body: &str, tag: &str) -> std::result::Result<String, UpnpError> {
    xml_value(body, tag).ok_or_else(|| UpnpError::new(402, "Invalid Args"))
}

fn xml_value(body: &str, tag: &str) -> Option<String> {
    let opening = format!("<{tag}");
    let closing = format!("</{tag}>");
    let mut offset = 0;
    while let Some(position) = body[offset..].find(&opening) {
        let start = offset + position;
        let after_tag = start + opening.len();
        let boundary = body[after_tag..].chars().next()?;
        if !matches!(boundary, '>' | ' ' | '/' | '\t' | '\r' | '\n') {
            offset = after_tag;
            continue;
        }
        let content_start = body[after_tag..].find('>')? + after_tag + 1;
        let end = body[content_start..].find(&closing)? + content_start;
        return Some(body[content_start..end].trim().to_owned());
    }
    None
}

fn parse_bool(value: &str) -> std::result::Result<bool, UpnpError> {
    match value {
        "0" | "false" => Ok(false),
        "1" | "true" => Ok(true),
        _ => Err(UpnpError::new(402, "Invalid Args")),
    }
}

fn parse_upnp_time(value: &str) -> std::result::Result<Duration, UpnpError> {
    let parts = value.split(':').collect::<Vec<_>>();
    if parts.len() != 3 {
        return Err(UpnpError::new(402, "Invalid Args"));
    }
    let hours = parts[0]
        .parse::<u64>()
        .map_err(|_| UpnpError::new(402, "Invalid Args"))?;
    let minutes = parts[1]
        .parse::<u64>()
        .map_err(|_| UpnpError::new(402, "Invalid Args"))?;
    let seconds = parts[2]
        .parse::<f64>()
        .map_err(|_| UpnpError::new(402, "Invalid Args"))?;
    if minutes >= 60 || !(0.0..60.0).contains(&seconds) || !seconds.is_finite() {
        return Err(UpnpError::new(402, "Invalid Args"));
    }
    let whole = hours
        .checked_mul(3600)
        .and_then(|value| value.checked_add(minutes * 60))
        .ok_or_else(|| UpnpError::new(402, "Invalid Args"))?;
    Ok(Duration::from_secs(whole) + Duration::from_secs_f64(seconds))
}

fn format_upnp_time(value: Duration) -> String {
    let total = value.as_secs();
    format!(
        "{:02}:{:02}:{:02}",
        total / 3600,
        total % 3600 / 60,
        total % 60
    )
}
fn parse_timeout(value: Option<&str>) -> Duration {
    value
        .and_then(|value| value.strip_prefix("Second-"))
        .and_then(|value| value.parse::<u64>().ok())
        .map(|seconds| Duration::from_secs(seconds.clamp(60, 86400)))
        .unwrap_or(Duration::from_secs(1800))
}
fn subscription_response(sid: &str, timeout: Duration) -> HttpResponse {
    HttpResponse {
        status: "200 OK",
        content_type: "text/plain",
        headers: vec![
            ("SID".into(), sid.into()),
            ("TIMEOUT".into(), format!("Second-{}", timeout.as_secs())),
        ],
        body: String::new(),
    }
}
fn new_sid() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("uuid:mini-mdr-{}-{now}", std::process::id())
}
fn remove_expired_subscriptions(subscriptions: &SharedSubscriptions) {
    if let Ok(mut guard) = subscriptions.entries.lock() {
        let now = Instant::now();
        let before = guard.len();
        guard.retain(|_, subscription| subscription.expires_at > now);
        let removed = before - guard.len();
        if removed > 0 {
            crate::log_info!("removed {removed} expired GENA subscription(s)");
        }
    }
}
fn header_value<'a>(headers: &'a str, target: &str) -> Option<&'a str> {
    headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case(target).then(|| value.trim())
    })
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
fn decode_xml(value: &str) -> String {
    let result = value
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'");
    // Handle numeric character references: &#60; and &#x3C;
    let mut pos = 0;
    let mut decoded = String::new();
    while pos < result.len() {
        if let Some(start) = result[pos..].find("&#") {
            let abs_start = pos + start;
            // Copy everything before the reference
            decoded.push_str(&result[pos..abs_start]);
            let ref_start = abs_start + 2;
            if let Some(end) = result[ref_start..].find(';') {
                let ref_content = &result[ref_start..ref_start + end];
                let ch = if let Some(hex) = ref_content.strip_prefix('x') {
                    u32::from_str_radix(hex, 16).ok()
                } else {
                    ref_content.parse::<u32>().ok()
                }
                .and_then(std::char::from_u32);
                if let Some(ch) = ch {
                    decoded.push(ch);
                } else {
                    // Invalid reference, keep as-is
                    decoded.push_str("&#");
                    decoded.push_str(ref_content);
                    decoded.push(';');
                }
                pos = ref_start + end + 1;
            } else {
                // No closing semicolon, keep as-is
                decoded.push_str("&#");
                pos = ref_start;
            }
        } else {
            decoded.push_str(&result[pos..]);
            break;
        }
    }
    decoded
}

impl Drop for UpnpServer {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Ok(mut connections) = self.connections.lock() {
            for stream in connections.values() {
                if let Err(error) = stream.shutdown(std::net::Shutdown::Both)
                    && error.kind() != std::io::ErrorKind::NotConnected
                {
                    crate::log_warn!("shutting down UPnP connection: {error}");
                }
            }
            connections.clear();
        }
        if let Some(thread) = self.thread.take()
            && let Err(error) = thread.join()
        {
            crate::log_error!("UPnP thread panicked: {error:?}");
        }
        // Wait for in-flight connection threads to release their permits so
        // shutdown does not destroy shared state out from under them. Bounded
        // so a backend stuck in IPC cannot hang application exit forever.
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.connection_slots.load(Ordering::Relaxed) > 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        if self.connection_slots.load(Ordering::Relaxed) > 0 {
            crate::log_warn!("UPnP connection threads did not drain within 5 seconds of shutdown");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::player::PlayerStatus;

    #[test]
    fn parses_and_formats_upnp_time() {
        let value = parse_upnp_time("01:02:03.500").unwrap();
        assert_eq!(value, Duration::from_millis(3_723_500));
        assert_eq!(format_upnp_time(value), "01:02:03");
    }

    #[test]
    fn rejects_invalid_upnp_time() {
        assert!(parse_upnp_time("00:99:00").is_err());
    }

    #[test]
    fn escapes_device_name_and_udn() {
        let xml = device_xml("TV & <Room>", "uuid:test&mini");
        assert!(xml.contains("TV &amp; &lt;Room&gt;"));
        assert!(xml.contains("<UDN>uuid:test&amp;mini</UDN>"));
        assert!(!xml.contains("{{UDN}}"));
        assert!(!xml.contains("{{DEVICE_NAME}}"));
    }

    #[test]
    fn matches_exact_tag_boundaries() {
        let body = "<TrackDuration>00:01:00</TrackDuration><Track>1</Track>";
        assert_eq!(xml_value(body, "Track").as_deref(), Some("1"));
        assert_eq!(
            xml_value(body, "TrackDuration").as_deref(),
            Some("00:01:00")
        );
        assert_eq!(xml_value(body, "TrackURI"), None);
    }

    #[test]
    fn round_trips_xml_escaping() {
        let raw = "a<b>&\"'c";
        assert_eq!(decode_xml(&escape_xml(raw)), raw);
    }

    #[test]
    fn clamps_subscription_timeouts() {
        assert_eq!(parse_timeout(Some("Second-1")), Duration::from_secs(60));
        assert_eq!(
            parse_timeout(Some("Second-999999")),
            Duration::from_secs(86400)
        );
        assert_eq!(parse_timeout(None), Duration::from_secs(1800));
    }

    #[test]
    fn thread_permit_respects_limit() {
        let counter = Arc::new(AtomicUsize::new(0));
        let first = ThreadPermit::try_acquire(&counter, 1).unwrap();
        assert!(ThreadPermit::try_acquire(&counter, 1).is_none());
        drop(first);
        assert!(ThreadPermit::try_acquire(&counter, 1).is_some());
    }

    struct FakeBackend {
        fail_load: bool,
        load_flag: Option<Arc<AtomicBool>>,
        stop_flag: Option<Arc<AtomicBool>>,
        status: PlayerStatus,
    }

    impl FakeBackend {
        fn new() -> Self {
            Self {
                fail_load: false,
                load_flag: None,
                stop_flag: None,
                status: PlayerStatus::default(),
            }
        }
    }

    impl PlayerBackend for FakeBackend {
        fn load(&mut self, _uri: &str, _title: Option<&str>) -> Result<()> {
            if self.fail_load {
                anyhow::bail!("load failed");
            }
            if let Some(flag) = &self.load_flag {
                flag.store(true, Ordering::Relaxed);
            }
            Ok(())
        }
        fn play(&mut self) -> Result<()> {
            Ok(())
        }
        fn pause(&mut self) -> Result<()> {
            Ok(())
        }
        fn stop(&mut self) -> Result<()> {
            if let Some(flag) = &self.stop_flag {
                flag.store(true, Ordering::Relaxed);
            }
            Ok(())
        }
        fn seek(&mut self, _position: Duration) -> Result<()> {
            Ok(())
        }
        fn set_volume(&mut self, _volume: u8) -> Result<()> {
            Ok(())
        }
        fn set_mute(&mut self, _muted: bool) -> Result<()> {
            Ok(())
        }
        fn status(&mut self) -> Result<PlayerStatus> {
            Ok(self.status.clone())
        }
        fn sink_protocol_info(&self) -> &str {
            ""
        }
    }

    fn soap_request(body: &str) -> HttpRequest {
        HttpRequest {
            method: "POST".into(),
            path: "/AVTransport/control".into(),
            headers: HashMap::new(),
            body: body.into(),
        }
    }

    fn test_player(backend: FakeBackend) -> SharedPlayer {
        Arc::new(Mutex::new(Box::new(backend) as Box<dyn PlayerBackend>))
    }

    fn test_state(uri: Option<&str>, transport: TransportState) -> SharedState {
        Arc::new(Mutex::new(RendererState {
            uri: uri.map(str::to_owned),
            title: uri.map(|_| "title".to_owned()),
            transport,
            ..RendererState::default()
        }))
    }

    #[test]
    fn rejects_overflowing_upnp_time() {
        assert!(parse_upnp_time("18446744073709551615:00:00").is_err());
        assert!(parse_upnp_time("20000000000000000000:00:00").is_err());
    }

    #[test]
    fn rejects_seek_beyond_duration() {
        let player = test_player(FakeBackend::new());
        let state = test_state(Some("http://x/y"), TransportState::Playing);
        state.lock().unwrap().duration = Some(Duration::from_secs(60));
        let request = soap_request(
            "<InstanceID>0</InstanceID><Unit>REL_TIME</Unit><Target>01:00:01</Target>",
        );
        let result = execute_av_transport(
            "Seek",
            EventService::AvTransport,
            &request,
            &player,
            &state,
            200,
        );
        assert!(matches!(result, Err(UpnpError { code: 711, .. })));
    }

    #[test]
    fn rolls_back_transport_state_on_load_failure() {
        let mut backend = FakeBackend::new();
        backend.fail_load = true;
        let player = test_player(backend);
        let state = test_state(Some("http://old"), TransportState::Playing);
        let request = soap_request("<InstanceID>0</InstanceID><CurrentURI>http://new</CurrentURI>");
        let result = execute_av_transport(
            "SetAVTransportURI",
            EventService::AvTransport,
            &request,
            &player,
            &state,
            200,
        );
        assert!(result.is_err());
        let state = state.lock().unwrap();
        assert_eq!(state.transport, TransportState::Playing);
        assert_eq!(state.uri.as_deref(), Some("http://old"));
        assert_eq!(state.title.as_deref(), Some("title"));
    }

    #[test]
    fn empty_uri_stops_and_clears_media() {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let mut backend = FakeBackend::new();
        backend.stop_flag = Some(Arc::clone(&stop_flag));
        let player = test_player(backend);
        let state = test_state(Some("http://old"), TransportState::Playing);
        let request = soap_request("<InstanceID>0</InstanceID><CurrentURI></CurrentURI>");
        let result = execute_av_transport(
            "SetAVTransportURI",
            EventService::AvTransport,
            &request,
            &player,
            &state,
            200,
        );
        assert!(result.is_ok());
        assert!(stop_flag.load(Ordering::Relaxed));
        let state = state.lock().unwrap();
        assert_eq!(state.transport, TransportState::NoMediaPresent);
        assert!(state.uri.is_none());
    }

    #[test]
    fn stop_keeps_current_uri() {
        let player = test_player(FakeBackend::new());
        let state = test_state(Some("http://x/y"), TransportState::Playing);
        state.lock().unwrap().position = Duration::from_secs(5);
        let request = soap_request("<InstanceID>0</InstanceID>");
        let result = execute_av_transport(
            "Stop",
            EventService::AvTransport,
            &request,
            &player,
            &state,
            200,
        );
        assert!(result.is_ok());
        let state = state.lock().unwrap();
        assert_eq!(state.transport, TransportState::Stopped);
        assert_eq!(state.uri.as_deref(), Some("http://x/y"));
        assert_eq!(state.position, Duration::ZERO);
    }

    #[test]
    fn play_reloads_media_after_stop() {
        let load_flag = Arc::new(AtomicBool::new(false));
        let mut backend = FakeBackend::new();
        backend.load_flag = Some(Arc::clone(&load_flag));
        let player = test_player(backend);
        let state = test_state(Some("http://x/y"), TransportState::Stopped);
        let request = soap_request("<InstanceID>0</InstanceID>");
        let result = execute_av_transport(
            "Play",
            EventService::AvTransport,
            &request,
            &player,
            &state,
            200,
        );
        assert!(result.is_ok());
        assert!(load_flag.load(Ordering::Relaxed));
        assert_eq!(state.lock().unwrap().transport, TransportState::Playing);
    }

    #[test]
    fn rejects_out_of_range_volume() {
        let player = test_player(FakeBackend::new());
        let state = test_state(None, TransportState::NoMediaPresent);
        let request = soap_request(
            "<InstanceID>0</InstanceID><Channel>Master</Channel><DesiredVolume>101</DesiredVolume>",
        );
        let result = execute_rendering_control(
            "SetVolume",
            EventService::RenderingControl,
            &request,
            &player,
            &state,
        );
        assert!(matches!(result, Err(UpnpError { code: 402, .. })));
    }

    #[test]
    fn maps_natural_end_to_stopped() {
        let mut backend = FakeBackend::new();
        backend.status = PlayerStatus {
            playing: false,
            paused: true,
            position: Duration::from_secs(60),
            duration: Some(Duration::from_secs(60)),
            volume: 100,
            muted: false,
        };
        let player = test_player(backend);
        let state = test_state(Some("http://x/y"), TransportState::Playing);
        refresh_player_state(&player, &state);
        assert_eq!(state.lock().unwrap().transport, TransportState::Stopped);
    }

    #[test]
    fn maps_idle_player_to_no_media_present() {
        let player = test_player(FakeBackend::new());
        let state = test_state(None, TransportState::Stopped);
        refresh_player_state(&player, &state);
        assert_eq!(
            state.lock().unwrap().transport,
            TransportState::NoMediaPresent
        );
    }

    fn subscribe_request(headers: HashMap<String, String>) -> HttpRequest {
        HttpRequest {
            method: "SUBSCRIBE".into(),
            path: "/AVTransport/event".into(),
            headers,
            body: String::new(),
        }
    }

    fn test_subscriptions() -> SharedSubscriptions {
        Arc::new(Subscriptions {
            entries: Mutex::new(HashMap::new()),
        })
    }

    fn insert_subscription(subscriptions: &SharedSubscriptions, sid: &str, service: EventService) {
        subscriptions.entries.lock().unwrap().insert(
            sid.to_owned(),
            Subscription {
                callback: "http://127.0.0.1:9/".into(),
                service,
                expires_at: Instant::now() + Duration::from_secs(60),
                sequence: 0,
                notify_lock: Arc::new(Mutex::new(())),
            },
        );
    }

    #[test]
    fn rejects_renewal_on_wrong_service() {
        let player = test_player(FakeBackend::new());
        let state = test_state(None, TransportState::NoMediaPresent);
        let subscriptions = test_subscriptions();
        insert_subscription(&subscriptions, "uuid:test", EventService::RenderingControl);
        let request = subscribe_request(HashMap::from([("sid".into(), "uuid:test".into())]));
        let response = subscribe(
            &request,
            EventService::AvTransport,
            &player,
            &state,
            &subscriptions,
        );
        assert_eq!(response.status, "412 Precondition Failed");
    }

    #[test]
    fn rejects_callback_without_host() {
        let player = test_player(FakeBackend::new());
        let state = test_state(None, TransportState::NoMediaPresent);
        let subscriptions = test_subscriptions();
        let request = subscribe_request(HashMap::from([
            ("nt".into(), "upnp:event".into()),
            ("callback".into(), "<http://>".into()),
        ]));
        let response = subscribe(
            &request,
            EventService::AvTransport,
            &player,
            &state,
            &subscriptions,
        );
        assert_eq!(response.status, "412 Precondition Failed");
    }

    #[test]
    fn rejects_oversized_callback() {
        let player = test_player(FakeBackend::new());
        let state = test_state(None, TransportState::NoMediaPresent);
        let subscriptions = test_subscriptions();
        let callback = format!("<http://127.0.0.1:9/{}>", "a".repeat(MAX_CALLBACK_SIZE));
        let request = subscribe_request(HashMap::from([
            ("nt".into(), "upnp:event".into()),
            ("callback".into(), callback),
        ]));
        let response = subscribe(
            &request,
            EventService::AvTransport,
            &player,
            &state,
            &subscriptions,
        );
        assert_eq!(response.status, "412 Precondition Failed");
    }

    #[test]
    fn enforces_subscription_limit() {
        let player = test_player(FakeBackend::new());
        let state = test_state(None, TransportState::NoMediaPresent);
        let subscriptions = test_subscriptions();
        for index in 0..MAX_SUBSCRIPTIONS {
            insert_subscription(
                &subscriptions,
                &format!("uuid:test-{index}"),
                EventService::AvTransport,
            );
        }
        let request = subscribe_request(HashMap::from([
            ("nt".into(), "upnp:event".into()),
            ("callback".into(), "<http://192.168.1.100:9/>".into()),
        ]));
        let response = subscribe(
            &request,
            EventService::AvTransport,
            &player,
            &state,
            &subscriptions,
        );
        assert_eq!(response.status, "503 Service Unavailable");
    }

    #[test]
    fn rejects_loopback_or_unspecified_callback() {
        let player = test_player(FakeBackend::new());
        let state = test_state(None, TransportState::NoMediaPresent);
        for callback in ["<http://127.0.0.1:9/>", "<http://0.0.0.0:9/>"] {
            let subscriptions = test_subscriptions();
            let request = subscribe_request(HashMap::from([
                ("nt".into(), "upnp:event".into()),
                ("callback".into(), callback.into()),
            ]));
            let response = subscribe(
                &request,
                EventService::AvTransport,
                &player,
                &state,
                &subscriptions,
            );
            assert_eq!(response.status, "412 Precondition Failed", "{callback}");
            assert!(subscriptions.entries.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn stopped_server_faults_actions_without_touching_backend() {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let mut backend = FakeBackend::new();
        backend.stop_flag = Some(Arc::clone(&stop_flag));
        let player = test_player(backend);
        let state = test_state(Some("http://x/y"), TransportState::Playing);
        let request = soap_request("<InstanceID>0</InstanceID>");
        let running = Arc::new(AtomicBool::new(false));
        let result = execute_action(
            "Stop",
            EventService::AvTransport,
            &request,
            &player,
            &state,
            200,
            &running,
        );
        assert!(matches!(result, Err(UpnpError { code: 501, .. })));
        assert!(!stop_flag.load(Ordering::Relaxed));
        assert_eq!(state.lock().unwrap().transport, TransportState::Playing);
    }

    #[test]
    fn running_server_executes_actions() {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let mut backend = FakeBackend::new();
        backend.stop_flag = Some(Arc::clone(&stop_flag));
        let player = test_player(backend);
        let state = test_state(Some("http://x/y"), TransportState::Playing);
        let request = soap_request("<InstanceID>0</InstanceID>");
        let running = Arc::new(AtomicBool::new(true));
        let result = execute_action(
            "Stop",
            EventService::AvTransport,
            &request,
            &player,
            &state,
            200,
            &running,
        );
        assert!(result.is_ok());
        assert!(stop_flag.load(Ordering::Relaxed));
        assert_eq!(state.lock().unwrap().transport, TransportState::Stopped);
    }
}
