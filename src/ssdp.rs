use anyhow::{Context, Result};
use local_ip_address::list_afinet_netifas;
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
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
        set_reuse_addr(&socket)?;
        socket.join_multicast_v4(&Ipv4Addr::new(239, 255, 255, 250), &Ipv4Addr::UNSPECIFIED)?;
        let running = Arc::new(AtomicBool::new(true));
        let active = Arc::clone(&running);
        let name = sanitize_header_value(device_name);
        let thread = thread::Builder::new().name("ssdp".into()).spawn(move || {
            let local_ips = list_local_ipv4();
            let best_ip = find_best_local_ip(&local_ips);
            let location = format!("http://{best_ip}:{http_port}/device.xml");
            if best_ip == Ipv4Addr::LOCALHOST {
                crate::log_warn!("could not detect local IP for SSDP LOCATION, using 127.0.0.1");
            }
            let senders = create_multicast_senders(&local_ips);
            crate::log_info!("SSDP server started, location={location}");
            for _ in 0..3 {
                announce(&senders, &location, &name, "ssdp:alive");
                thread::sleep(Duration::from_millis(200));
            }
            let mut last_announce = Instant::now();
            let mut buffer = [0; 4096];
            while active.load(Ordering::Relaxed) {
                match socket.recv_from(&mut buffer) {
                    Ok((size, peer)) => {
                        respond_to_search(
                            &senders,
                            &buffer[..size],
                            peer,
                            &location,
                            &name,
                            &local_ips,
                            http_port,
                        );
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
                    announce(&senders, &location, &name, "ssdp:alive");
                    last_announce = Instant::now();
                }
            }
            announce(&senders, &location, &name, "ssdp:byebye");
        })?;
        Ok(Self {
            running,
            thread: Some(thread),
        })
    }
}

fn respond_to_search(
    senders: &[MulticastSender],
    data: &[u8],
    peer: SocketAddr,
    location: &str,
    name: &str,
    local_ips: &[Ipv4Addr],
    http_port: u16,
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
    let peer_ip = peer.ip();
    let matched_sender = senders
        .iter()
        .find(|s| find_matching_interface(local_ips, peer_ip) == Some(s.ip));
    let matched_location = if let Some(sender) = matched_sender {
        format!("http://{}:{http_port}/device.xml", sender.ip)
    } else {
        location.to_owned()
    };
    let response_socket = matched_sender
        .map(|s| &s.socket)
        .or_else(|| senders.first().map(|s| &s.socket));
    for target in targets {
        let usn = usn(target);
        let response = format!(
            "HTTP/1.1 200 OK\r\n\
             CACHE-CONTROL: max-age=1800\r\n\
             EXT:\r\n\
             LOCATION: {matched_location}\r\n\
             SERVER: {name}\r\n\
             ST: {target}\r\n\
             USN: {usn}\r\n\
             DATE: {}\r\n\
             \r\n",
            timestamp_rfc1123()
        );
        if let Some(sock) = response_socket {
            if let Err(error) = sock.send_to(response.as_bytes(), peer) {
                crate::log_error!("sending SSDP response: {error}");
            }
        } else if let Err(error) = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
            .and_then(|s| s.send_to(response.as_bytes(), peer).map(|_| ()))
        {
            crate::log_error!("sending SSDP response: {error}");
        }
    }
}

fn announce(senders: &[MulticastSender], location: &str, name: &str, nts: &str) {
    for target in ADVERTISED_TARGETS {
        let date = timestamp_rfc1123();
        let mut message = format!(
            "NOTIFY * HTTP/1.1\r\n\
             HOST: 239.255.255.250:1900\r\n\
             NT: {target}\r\n\
             NTS: {nts}\r\n\
             USN: {}\r\n\
             SERVER: {name}\r\n\
             DATE: {date}\r\n",
            usn(target)
        );
        if nts == "ssdp:alive" {
            message.push_str(&format!(
                "CACHE-CONTROL: max-age=1800\r\n\
                 LOCATION: {location}\r\n"
            ));
        }
        message.push_str("\r\n");
        for sender in senders {
            if let Err(error) = sender.send_to(message.as_bytes(), MULTICAST_ADDRESS) {
                crate::log_error!("sending SSDP {nts}: {error}");
            }
            if nts == "ssdp:alive" {
                let _ = sender.send_to(message.as_bytes(), MULTICAST_ADDRESS);
            }
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

fn list_local_ipv4() -> Vec<Ipv4Addr> {
    list_afinet_netifas()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(_name, ip)| match ip {
            IpAddr::V4(v4) if !v4.is_loopback() => Some(v4),
            _ => None,
        })
        .collect()
}

fn find_best_local_ip(ips: &[Ipv4Addr]) -> Ipv4Addr {
    ips.iter()
        .copied()
        .find(|ip| is_rfc1918(*ip))
        .or_else(|| ips.first().copied())
        .unwrap_or(Ipv4Addr::LOCALHOST)
}

fn find_matching_interface(local_ips: &[Ipv4Addr], peer: IpAddr) -> Option<Ipv4Addr> {
    let peer_v4 = match peer {
        IpAddr::V4(v4) => v4,
        _ => return None,
    };
    local_ips
        .iter()
        .copied()
        .find(|local| is_same_subnet(*local, peer_v4))
        .or_else(|| local_ips.first().copied())
}

fn is_same_subnet(a: Ipv4Addr, b: Ipv4Addr) -> bool {
    let ao = a.octets();
    let bo = b.octets();
    match (ao[0], ao[1]) {
        (10, _) => ao[0] == bo[0],
        (172, 16..=31) => ao[0] == bo[0] && ao[1] == bo[1],
        (192, 168) => ao[0] == bo[0] && ao[1] == bo[1] && ao[2] == bo[2],
        _ => ao[0] == bo[0] && ao[1] == bo[1] && ao[2] == bo[2],
    }
}

fn is_rfc1918(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 10 || (o[0] == 172 && (16..=31).contains(&o[1])) || (o[0] == 192 && o[1] == 168)
}

struct MulticastSender {
    socket: UdpSocket,
    ip: Ipv4Addr,
}

impl MulticastSender {
    fn new(ip: Ipv4Addr) -> Result<Self> {
        let socket = UdpSocket::bind((ip, 0))?;
        socket.set_multicast_loop_v4(true)?;
        set_multicast_if(&socket, ip)?;
        Ok(Self { socket, ip })
    }

    fn send_to(&self, data: &[u8], dest: SocketAddr) -> Result<()> {
        self.socket.send_to(data, dest)?;
        Ok(())
    }
}

fn set_reuse_addr(socket: &UdpSocket) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawSocket;
        unsafe extern "system" {
            fn setsockopt(
                s: libc::SOCKET,
                level: libc::c_int,
                optname: libc::c_int,
                optval: *const u8,
                optlen: libc::c_int,
            ) -> libc::c_int;
        }
        const SOL_SOCKET: libc::c_int = 1;
        const SO_REUSEADDR: libc::c_int = 4;
        let val: i32 = 1;
        let raw = socket.as_raw_socket();
        let ret = unsafe {
            setsockopt(
                raw as libc::SOCKET,
                SOL_SOCKET,
                SO_REUSEADDR,
                &val as *const i32 as *const u8,
                std::mem::size_of::<i32>() as libc::c_int,
            )
        };
        if ret != 0 {
            crate::log_warn!("setsockopt SO_REUSEADDR failed");
        }
    }
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let fd = socket.as_raw_fd();
        let val: libc::c_int = 1;
        let ret = unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_REUSEADDR,
                &val as *const libc::c_int as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if ret != 0 {
            crate::log_warn!("setsockopt SO_REUSEADDR failed");
        }
    }
    Ok(())
}

fn set_multicast_if(socket: &UdpSocket, interface: Ipv4Addr) -> Result<()> {
    let addr = interface.octets();
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawSocket;
        unsafe extern "system" {
            fn setsockopt(
                s: libc::SOCKET,
                level: libc::c_int,
                optname: libc::c_int,
                optval: *const u8,
                optlen: libc::c_int,
            ) -> libc::c_int;
        }
        const IPPROTO_IP: libc::c_int = 0;
        const IP_MULTICAST_IF: libc::c_int = 19;
        let raw = socket.as_raw_socket();
        let ret = unsafe {
            setsockopt(
                raw as libc::SOCKET,
                IPPROTO_IP,
                IP_MULTICAST_IF,
                addr.as_ptr(),
                addr.len() as libc::c_int,
            )
        };
        if ret != 0 {
            anyhow::bail!("setsockopt IP_MULTICAST_IF failed for {interface}");
        }
    }
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let fd = socket.as_raw_fd();
        let ret = unsafe {
            libc::setsockopt(
                fd,
                libc::IPPROTO_IP,
                libc::IP_MULTICAST_IF,
                addr.as_ptr() as *const libc::c_void,
                addr.len() as libc::socklen_t,
            )
        };
        if ret != 0 {
            anyhow::bail!("setsockopt IP_MULTICAST_IF failed for {interface}");
        }
    }
    Ok(())
}

fn create_multicast_senders(ips: &[Ipv4Addr]) -> Vec<MulticastSender> {
    let mut senders = Vec::new();
    for &ip in ips {
        match MulticastSender::new(ip) {
            Ok(sender) => {
                crate::log_info!("SSDP multicast sender created for {ip}");
                senders.push(sender);
            }
            Err(error) => {
                crate::log_warn!("creating SSDP sender for {ip}: {error}");
            }
        }
    }
    if senders.is_empty() {
        crate::log_warn!("no SSDP multicast senders created, alive notifications disabled");
    }
    senders
}

fn timestamp_rfc1123() -> String {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = secs / 86400;
    let time_of_day = secs % 86400;
    let mut y = 1970u32;
    let mut remaining = days;
    loop {
        let days_in_year = if is_leap(y) { 366 } else { 365 };
        if remaining < days_in_year as u64 {
            break;
        }
        remaining -= days_in_year as u64;
        y += 1;
    }
    let leap = is_leap(y);
    let month_days: [u32; 12] = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut m = 0usize;
    let mut rem = remaining as u32;
    for (i, &d) in month_days.iter().enumerate() {
        if rem < d {
            m = i;
            break;
        }
        rem -= d;
    }
    let day = rem + 1;
    let weekday = ((days + 3) % 7) as usize;
    let hour = (time_of_day / 3600) as u32;
    let min = ((time_of_day % 3600) / 60) as u32;
    let sec = (time_of_day % 60) as u32;
    format!(
        "{}, {day:02} {} {y} {hour:02}:{min:02}:{sec:02} GMT",
        DAYS[weekday], MONTHS[m]
    )
}

fn is_leap(y: u32) -> bool {
    (y.is_multiple_of(4) && !y.is_multiple_of(100)) || y.is_multiple_of(400)
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

    #[test]
    fn subnet_matching_covers_common_private_ranges() {
        let local = Ipv4Addr::new(192, 168, 1, 100);
        assert!(is_same_subnet(local, Ipv4Addr::new(192, 168, 1, 50)));
        assert!(!is_same_subnet(local, Ipv4Addr::new(192, 168, 2, 50)));

        let local10 = Ipv4Addr::new(10, 0, 0, 1);
        assert!(is_same_subnet(local10, Ipv4Addr::new(10, 0, 0, 50)));
        assert!(!is_same_subnet(local10, Ipv4Addr::new(10, 1, 0, 50)));

        let local172 = Ipv4Addr::new(172, 16, 5, 1);
        assert!(is_same_subnet(local172, Ipv4Addr::new(172, 16, 5, 50)));
        assert!(!is_same_subnet(local172, Ipv4Addr::new(172, 17, 5, 50)));
    }

    #[test]
    fn finds_best_local_ip_prefers_rfc1918() {
        let ips = vec![Ipv4Addr::new(172, 217, 22, 14)];
        assert_eq!(find_best_local_ip(&ips), Ipv4Addr::new(172, 217, 22, 14));

        let ips = vec![
            Ipv4Addr::new(172, 217, 22, 14),
            Ipv4Addr::new(192, 168, 1, 100),
        ];
        assert_eq!(find_best_local_ip(&ips), Ipv4Addr::new(192, 168, 1, 100));
    }

    #[test]
    fn rfc1918_detection() {
        assert!(is_rfc1918(Ipv4Addr::new(10, 0, 0, 1)));
        assert!(is_rfc1918(Ipv4Addr::new(172, 16, 0, 1)));
        assert!(is_rfc1918(Ipv4Addr::new(192, 168, 0, 1)));
        assert!(!is_rfc1918(Ipv4Addr::new(8, 8, 8, 8)));
        assert!(!is_rfc1918(Ipv4Addr::new(172, 32, 0, 1)));
    }

    #[test]
    fn timestamp_rfc1123_format() {
        let ts = timestamp_rfc1123();
        assert!(ts.ends_with(" GMT"));
        assert!(ts.contains("2026"));
    }
}
