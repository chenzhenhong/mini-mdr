use anyhow::{Context, Result};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

const MULTICAST_ADDRESS: SocketAddr =
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(239, 255, 255, 250)), 1900);
const DEVICE_TYPE: &str = "urn:schemas-upnp-org:device:MediaRenderer:1";
const UDN: &str = "uuid:mini-mdr";
const SERVICE_AVTRANSPORT: &str = "urn:schemas-upnp-org:service:AVTransport:1";
const SERVICE_RENDERING_CONTROL: &str = "urn:schemas-upnp-org:service:RenderingControl:1";
const SERVICE_CONNECTION_MANAGER: &str = "urn:schemas-upnp-org:service:ConnectionManager:1";
const ADVERTISED_TARGETS: [&str; 6] = [
    "upnp:rootdevice",
    UDN,
    DEVICE_TYPE,
    SERVICE_AVTRANSPORT,
    SERVICE_RENDERING_CONTROL,
    SERVICE_CONNECTION_MANAGER,
];

pub struct SsdpServer {
    running: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl SsdpServer {
    pub fn start(http_port: u16, device_name: &str) -> Result<Self> {
        let socket =
            UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 1900)).context("binding SSDP UDP port 1900")?;
        socket.set_read_timeout(Some(Duration::from_millis(300)))?;
        socket.set_multicast_loop_v4(true)?;
        socket.join_multicast_v4(&Ipv4Addr::new(239, 255, 255, 250), &Ipv4Addr::UNSPECIFIED)?;
        let running = Arc::new(AtomicBool::new(true));
        let active = Arc::clone(&running);
        let name = sanitize_header_value(device_name);
        let thread = thread::Builder::new().name("ssdp".into()).spawn(move || {
            let location = format!(
                "http://{}:{http_port}/device.xml",
                local_ip().unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST))
            );
            announce(&socket, &location, &name, "ssdp:alive");
            let mut last_announce = Instant::now();
            let mut buffer = [0; 4096];
            while active.load(Ordering::Relaxed) {
                match socket.recv_from(&mut buffer) {
                    Ok((size, peer)) => {
                        respond_to_search(&socket, &buffer[..size], peer, &location, &name)
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) => {}
                    Err(error) => {
                        crate::log_error!("SSDP receive failed: {error}");
                        break;
                    }
                }
                if last_announce.elapsed() >= Duration::from_secs(900) {
                    announce(&socket, &location, &name, "ssdp:alive");
                    last_announce = Instant::now();
                }
            }
            announce(&socket, &location, &name, "ssdp:byebye");
        })?;
        Ok(Self {
            running,
            thread: Some(thread),
        })
    }
}

fn respond_to_search(
    socket: &UdpSocket,
    data: &[u8],
    peer: SocketAddr,
    location: &str,
    name: &str,
) {
    let request = String::from_utf8_lossy(data);
    if !request
        .lines()
        .next()
        .is_some_and(|line| line.eq_ignore_ascii_case("M-SEARCH * HTTP/1.1"))
    {
        return;
    }
    let search_target = header(&request, "ST").unwrap_or_default();
    let targets: Vec<&str> = if search_target.eq_ignore_ascii_case("ssdp:all") {
        ADVERTISED_TARGETS.to_vec()
    } else if search_target.eq_ignore_ascii_case("upnp:rootdevice") {
        vec!["upnp:rootdevice"]
    } else if search_target.eq_ignore_ascii_case(UDN) {
        vec![UDN]
    } else if search_target.eq_ignore_ascii_case(DEVICE_TYPE)
        || search_target.eq_ignore_ascii_case("urn:schemas-upnp-org:device:MediaRenderer:3")
    {
        vec![DEVICE_TYPE]
    } else {
        match ADVERTISED_TARGETS
            .iter()
            .find(|target| search_target.eq_ignore_ascii_case(target))
        {
            Some(target) => vec![*target],
            None => return,
        }
    };
    for target in targets {
        let usn = usn(target);
        let response = format!(
            "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=1800\r\nEXT:\r\nLOCATION: {location}\r\nSERVER: {name}/1.0 UPnP/1.1 mini-mdr/0.1\r\nST: {target}\r\nUSN: {usn}\r\n\r\n"
        );
        if let Err(error) = socket.send_to(response.as_bytes(), peer) {
            crate::log_error!("sending SSDP response: {error}");
        }
    }
}

fn announce(socket: &UdpSocket, location: &str, name: &str, nts: &str) {
    for target in ADVERTISED_TARGETS {
        let mut message = format!(
            "NOTIFY * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nNT: {target}\r\nNTS: {nts}\r\nUSN: {}\r\nSERVER: {name}/1.0 UPnP/1.1 mini-mdr/0.1\r\n",
            usn(target)
        );
        if nts == "ssdp:alive" {
            message.push_str(&format!(
                "CACHE-CONTROL: max-age=1800\r\nLOCATION: {location}\r\n"
            ));
        }
        message.push_str("\r\n");
        if let Err(error) = socket.send_to(message.as_bytes(), MULTICAST_ADDRESS) {
            crate::log_error!("sending SSDP {nts}: {error}");
        }
    }
}

fn usn(target: &str) -> String {
    if target == UDN {
        UDN.into()
    } else {
        format!("{UDN}::{target}")
    }
}

fn sanitize_header_value(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

fn header<'a>(request: &'a str, target: &str) -> Option<&'a str> {
    request.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case(target).then(|| value.trim())
    })
}

pub fn local_ip() -> Option<IpAddr> {
    UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        .ok()
        .and_then(|socket| {
            socket.connect((Ipv4Addr::new(192, 0, 2, 1), 80)).ok()?;
            Some(socket.local_addr().ok()?.ip())
        })
}

impl Drop for SsdpServer {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(thread) = self.thread.take()
            && let Err(error) = thread.join()
        {
            crate::log_error!("SSDP thread panicked: {error:?}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_correct_usn() {
        assert_eq!(usn(UDN), UDN);
        assert_eq!(usn(DEVICE_TYPE), format!("{UDN}::{DEVICE_TYPE}"));
    }

    #[test]
    fn sanitizes_control_characters_in_header_values() {
        assert_eq!(
            sanitize_header_value("TV\r\nX-Injected: 1"),
            "TV  X-Injected: 1"
        );
        assert_eq!(sanitize_header_value("客厅"), "客厅");
    }
}
