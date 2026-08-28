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
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const DEVICE_DESCRIPTION: &str = include_str!("../resources/device.xml");
const SERVICE_CONNECTION: &str = include_str!("../resources/connection_manager.xml");
const SERVICE_AVTRANSPORT: &str = include_str!("../resources/av_transport.xml");
const SERVICE_RENDERING: &str = include_str!("../resources/rendering_control.xml");
const MAX_REQUEST_SIZE: usize = 1024 * 1024;

type SharedPlayer = Arc<Mutex<Box<dyn PlayerBackend>>>;
type SharedState = Arc<Mutex<RendererState>>;
type SharedSubscriptions = Arc<Mutex<HashMap<String, Subscription>>>;

pub struct UpnpServer {
    running: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
    port: u16,
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
        player: SharedPlayer,
        state: SharedState,
        max_history: usize,
    ) -> Result<Self> {
        let listener = TcpListener::bind(("0.0.0.0", 0))?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let running = Arc::new(AtomicBool::new(true));
        let active = Arc::clone(&running);
        let subscriptions = Arc::new(Mutex::new(HashMap::new()));
        let name = name.to_owned();
        let thread = thread::Builder::new()
            .name("upnp-http".into())
            .spawn(move || {
                while active.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            if let Err(error) = serve(
                                &mut stream,
                                &name,
                                &player,
                                &state,
                                &subscriptions,
                                max_history,
                            ) {
                                crate::log_error!("UPnP request failed: {error:#}");
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
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

fn serve(
    stream: &mut TcpStream,
    name: &str,
    player: &SharedPlayer,
    state: &SharedState,
    subscriptions: &SharedSubscriptions,
    max_history: usize,
) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    let request = match read_request(stream) {
        Ok(req) => req,
        Err(error) => {
            let msg = error.to_string();
            if msg.contains("connection closed") || msg.contains("incomplete") {
                return Ok(());
            }
            return Err(error);
        }
    };
    let response = route(&request, name, player, state, subscriptions, max_history);
    write_response(stream, response)
}

fn route(
    request: &HttpRequest,
    name: &str,
    player: &SharedPlayer,
    state: &SharedState,
    subscriptions: &SharedSubscriptions,
    max_history: usize,
) -> HttpResponse {
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/device.xml") => xml_response(device_xml(name)),
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
        ),
        ("POST", "/AVTransport/control") => soap_route(
            request,
            EventService::AvTransport,
            player,
            state,
            subscriptions,
            max_history,
        ),
        ("POST", "/RenderingControl/control") => soap_route(
            request,
            EventService::RenderingControl,
            player,
            state,
            subscriptions,
            max_history,
        ),
        ("SUBSCRIBE", path) => match event_service(path) {
            Some(service) => subscribe(request, service, state, subscriptions),
            None => plain_response("404 Not Found", "Not found\n"),
        },
        ("UNSUBSCRIBE", path) => match event_service(path) {
            Some(_) => unsubscribe(request, subscriptions),
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
) -> HttpResponse {
    let Some(action) = request.headers.get("soapaction").and_then(|value| {
        value
            .trim_matches(|character| character == '"' || character == '\'')
            .rsplit_once('#')
            .map(|(_, action)| action)
    }) else {
        return soap_fault_response(401, "Invalid Action");
    };

    let result = execute_action(action, service, request, player, state, max_history);
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
) -> std::result::Result<(String, bool), UpnpError> {
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
            let uri = required_value(&request.body, "CurrentURI")?;
            let metadata = xml_value(&request.body, "CurrentURIMetaData");
            let title = metadata
                .as_deref()
                .map(decode_xml)
                .and_then(|metadata| xml_value(&metadata, "dc:title"));
            let mut player = lock_player(player)?;
            let mut state = lock_state(state)?;
            state.transport = TransportState::Transitioning;
            let uri = decode_xml(&uri);
            let display_title = title.map(|value| decode_xml(&value));
            player
                .load(&uri, display_title.as_deref())
                .map_err(player_error)?;
            crate::log_info!("loaded media: {}", display_title.as_deref().unwrap_or(&uri));
            state.uri = Some(uri.clone());
            state.title = display_title.clone();
            state.position = Duration::ZERO;
            state.duration = None;
            state.transport = TransportState::Stopped;
            if let Err(error) = crate::config::Config::append_history(
                crate::state::HistoryEntry::new(uri, display_title),
                max_history,
            ) {
                crate::log_error!("saving history: {error:#}");
            }
            Ok((action_response(service, action, ""), true))
        }
        "Play" => {
            require_instance_zero(request)?;
            let mut player = lock_player(player)?;
            let mut state = lock_state(state)?;
            if state.uri.is_none() {
                return Err(UpnpError::new(714, "No Such Resource"));
            }
            player.play().map_err(player_error)?;
            state.transport = TransportState::Playing;
            Ok((action_response(service, action, ""), true))
        }
        "Pause" => {
            require_instance_zero(request)?;
            if lock_state(state)?.uri.is_none() {
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
            state.transport = TransportState::Stopped;
            state.position = Duration::ZERO;
            Ok((action_response(service, action, ""), true))
        }
        "Seek" => {
            require_instance_zero(request)?;
            if required_value(&request.body, "Unit")? != "REL_TIME" {
                return Err(UpnpError::new(710, "Seek Mode Not Supported"));
            }
            let position = parse_upnp_time(&required_value(&request.body, "Target")?)?;
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
                .parse::<u8>()
                .map_err(|_| UpnpError::new(402, "Invalid Args"))?
                .min(100);
            lock_player(player)?
                .set_volume(volume)
                .map_err(player_error)?;
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
            lock_player(player)?.set_mute(muted).map_err(player_error)?;
            lock_state(state)?.muted = muted;
            Ok((action_response(service, action, ""), true))
        }
        _ => Err(UpnpError::new(401, "Invalid Action")),
    }
}

fn subscribe(
    request: &HttpRequest,
    service: EventService,
    state: &SharedState,
    subscriptions: &SharedSubscriptions,
) -> HttpResponse {
    let timeout = parse_timeout(request.headers.get("timeout").map(String::as_str));
    if let Some(sid) = request.headers.get("sid") {
        let mut guard = match subscriptions.lock() {
            Ok(guard) => guard,
            Err(_) => {
                crate::log_warn!("failed to lock subscription state for renewal");
                return plain_response("500 Internal Server Error", "subscription state failed\n");
            }
        };
        let Some(subscription) = guard.get_mut(sid) else {
            return plain_response("412 Precondition Failed", "unknown SID\n");
        };
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
    let sid = new_sid();
    let subscription = Subscription {
        callback: callback.to_owned(),
        service,
        expires_at: Instant::now() + timeout,
        sequence: 0,
    };
    if let Ok(mut guard) = subscriptions.lock() {
        guard.insert(sid.clone(), subscription);
        crate::log_info!("GENA subscription created SID={sid}");
    } else {
        return plain_response("500 Internal Server Error", "subscription state failed\n");
    }
    notify_sid(subscriptions, state, &sid);
    subscription_response(&sid, timeout)
}

fn unsubscribe(request: &HttpRequest, subscriptions: &SharedSubscriptions) -> HttpResponse {
    let Some(sid) = request.headers.get("sid") else {
        return plain_response("412 Precondition Failed", "SID is required\n");
    };
    match subscriptions.lock() {
        Ok(mut guard) => {
            if guard.remove(sid).is_some() {
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
    let state = match state.lock() {
        Ok(state) => state.clone(),
        Err(_) => {
            crate::log_warn!("failed to lock renderer state for GENA notification");
            return;
        }
    };
    let (callback, sequence, service) = match subscriptions.lock() {
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
    let socket_authority = if authority
        .rsplit_once(':')
        .is_some_and(|(_, port)| port.parse::<u16>().is_ok())
    {
        authority.to_owned()
    } else {
        format!("{authority}:80")
    };
    let address = socket_authority
        .to_socket_addrs()
        .context("resolving callback host")?
        .next()
        .context("callback host has no address")?;
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
    let mut content_length = None;
    loop {
        let size = stream.read(&mut chunk)?;
        if size == 0 {
            anyhow::bail!("connection closed before headers complete");
        }
        data.extend_from_slice(&chunk[..size]);
        if data.len() > MAX_REQUEST_SIZE {
            anyhow::bail!("UPnP request exceeds 1 MiB");
        }
        if let Some(header_end) = crate::util::find_bytes(&data, b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&data[..header_end]);
            content_length.get_or_insert_with(|| {
                header_value(&headers, "content-length")
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(0)
            });
            if data.len() >= header_end + 4 + content_length.unwrap_or(0) {
                break;
            }
        }
    }
    let header_end =
        crate::util::find_bytes(&data, b"\r\n\r\n").context("incomplete HTTP headers")?;
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
    let body =
        String::from_utf8(data[header_end + 4..].to_vec()).context("HTTP body is not UTF-8")?;
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
fn device_xml(name: &str) -> String {
    DEVICE_DESCRIPTION.replace("{{DEVICE_NAME}}", &escape_xml(name))
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
    crate::log_error!("player action failed: {error:#}");
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
        if status.playing {
            state.transport = TransportState::Playing;
        } else if status.paused {
            state.transport = TransportState::PausedPlayback;
        }
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
    if minutes >= 60 || !(0.0..60.0).contains(&seconds) {
        return Err(UpnpError::new(402, "Invalid Args"));
    }
    Ok(Duration::from_secs(hours * 3600 + minutes * 60) + Duration::from_secs_f64(seconds))
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
    if let Ok(mut guard) = subscriptions.lock() {
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
    value
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

impl Drop for UpnpServer {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(thread) = self.thread.take()
            && let Err(error) = thread.join()
        {
            crate::log_error!("UPnP thread panicked: {error:?}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn escapes_device_name() {
        assert!(device_xml("TV & <Room>").contains("TV &amp; &lt;Room&gt;"));
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
}
