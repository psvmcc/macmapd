use crate::{clients::Client, config::Config, runtime::Shared};
use anyhow::{Context, Result, ensure};
use std::{
    collections::BTreeMap,
    net::{Ipv4Addr, SocketAddrV4},
    sync::{Arc, atomic::Ordering},
    time::Instant,
};
use tokio::net::UdpSocket;

fn format_mac(mac: &[u8; 6]) -> String {
    mac.iter()
        .map(|v| format!("{v:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

#[derive(Debug)]
pub struct Packet {
    pub header: Vec<u8>,
    pub options: BTreeMap<u8, Vec<u8>>,
    pub mac: [u8; 6],
    pub message: u8,
    pub ciaddr: Ipv4Addr,
    pub giaddr: Ipv4Addr,
}

fn read_options(bytes: &[u8], options: &mut BTreeMap<u8, Vec<u8>>) -> Result<()> {
    let mut pos = 0;
    while pos < bytes.len() {
        let code = bytes[pos];
        pos += 1;
        if code == 255 {
            return Ok(());
        }
        if code == 0 {
            continue;
        }
        ensure!(pos < bytes.len(), "missing option length");
        let len = bytes[pos] as usize;
        pos += 1;
        ensure!(pos + len <= bytes.len(), "truncated option {code}");
        options
            .entry(code)
            .or_default()
            .extend_from_slice(&bytes[pos..pos + len]);
        pos += len;
    }
    Ok(())
}

impl Packet {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        ensure!(bytes.len() >= 240, "short DHCP packet");
        ensure!(
            bytes[0] == 1 && bytes[1] == 1 && bytes[2] == 6,
            "expected Ethernet BOOTREQUEST"
        );
        ensure!(bytes[236..240] == [99, 130, 83, 99], "invalid DHCP cookie");
        let mut options = BTreeMap::new();
        read_options(&bytes[240..], &mut options)?;
        let overload = options.get(&52).cloned();
        if let Some(overload) = overload {
            ensure!(
                overload.len() == 1 && (1..=3).contains(&overload[0]),
                "invalid option overload"
            );
            if overload[0] & 1 != 0 {
                read_options(&bytes[108..236], &mut options)?;
            }
            if overload[0] & 2 != 0 {
                read_options(&bytes[44..108], &mut options)?;
            }
        }
        for (code, len) in [(53, 1), (50, 4), (54, 4), (57, 2)] {
            if let Some(value) = options.get(&code) {
                ensure!(value.len() == len, "invalid option {code} length");
            }
        }
        if let Some(arch) = options.get(&93) {
            ensure!(
                !arch.is_empty() && arch.len() % 2 == 0,
                "invalid architecture list"
            );
        }
        let message = *options
            .get(&53)
            .and_then(|v| v.first())
            .context("missing message type")?;
        ensure!(
            [1, 3, 4, 7, 8].contains(&message),
            "unsupported client message type"
        );
        Ok(Self {
            header: bytes[..236].to_vec(),
            options,
            mac: bytes[28..34].try_into()?,
            message,
            ciaddr: Ipv4Addr::new(bytes[12], bytes[13], bytes[14], bytes[15]),
            giaddr: Ipv4Addr::new(bytes[24], bytes[25], bytes[26], bytes[27]),
        })
    }
    pub fn ip_option(&self, code: u8) -> Option<Ipv4Addr> {
        self.options
            .get(&code)
            .and_then(|v| <[u8; 4]>::try_from(v.as_slice()).ok())
            .map(Ipv4Addr::from)
    }
    pub fn stage(&self) -> (&'static str, &'static str) {
        let user = self.options.get(&77).map(Vec::as_slice).unwrap_or_default();
        let mut classes = user;
        let mut ipxe = user == b"iPXE";
        while let Some((&len, rest)) = classes.split_first() {
            if rest.len() < len as usize {
                break;
            }
            ipxe |= &rest[..len as usize] == b"iPXE";
            classes = &rest[len as usize..];
        }
        let arch =
            self.options.get(&93).and_then(|values| {
                values.as_chunks::<2>().0.iter().find_map(|v| {
                    match u16::from_be_bytes([v[0], v[1]]) {
                        0 => Some(("bios", "x86_64")),
                        7 | 9 => Some(("uefi", "x86_64")),
                        11 => Some(("uefi", "arm64")),
                        _ => None,
                    }
                })
            });
        let vendor = self.options.get(&60).map(Vec::as_slice).unwrap_or_default();
        let pxe = vendor.starts_with(b"PXEClient");
        if ipxe || pxe || arch.is_some() {
            let fallback = std::str::from_utf8(vendor)
                .ok()
                .and_then(|s| s.split("Arch:").nth(1))
                .and_then(|s| s.get(..5))
                .and_then(|s| s.parse::<u16>().ok())
                .and_then(|v| match v {
                    0 => Some(("bios", "x86_64")),
                    7 | 9 => Some(("uefi", "x86_64")),
                    11 => Some(("uefi", "arm64")),
                    _ => None,
                });
            let detected = arch.or(fallback).unwrap_or(("unknown", "unknown"));
            return if ipxe { ("ipxe", detected.1) } else { detected };
        }
        ("os", arch.map(|a| a.1).unwrap_or("unknown"))
    }
    pub fn route_mode(&self) -> &'static str {
        let stage = self.stage().0;
        if !matches!(stage, "bios" | "uefi")
            && self.options.get(&55).is_some_and(|v| v.contains(&121))
        {
            "classless"
        } else {
            "default"
        }
    }
}

pub struct Reply {
    pub bytes: Vec<u8>,
    pub destination: SocketAddrV4,
    pub message: u8,
    pub boot_file: Option<String>,
    pub relay_option_omitted: bool,
}

fn option(out: &mut Vec<u8>, code: u8, value: &[u8]) {
    for chunk in value.chunks(255) {
        out.extend([code, chunk.len() as u8]);
        out.extend(chunk);
    }
}

pub fn reply(
    packet: &Packet,
    client: &Client,
    config: &Config,
    server: Ipv4Addr,
) -> Result<Option<Reply>> {
    ensure!(
        !server.is_unspecified() && !server.is_broadcast() && !server.is_multicast(),
        "invalid server identifier"
    );
    if packet.ip_option(54).is_some_and(|ip| ip != server) {
        return Ok(None);
    }
    let message = match packet.message {
        1 => 2,
        3 => {
            if (!packet.ciaddr.is_unspecified()
                && (packet.options.contains_key(&50) || packet.options.contains_key(&54)))
                || (packet.options.contains_key(&54) && !packet.options.contains_key(&50))
            {
                return Ok(None);
            }
            let requested = packet
                .ip_option(50)
                .or_else(|| (!packet.ciaddr.is_unspecified()).then_some(packet.ciaddr));
            let Some(requested) = requested else {
                return Ok(None);
            };
            if requested == client.ip { 5 } else { 6 }
        }
        8 if !packet.ciaddr.is_unspecified() => 5,
        _ => return Ok(None),
    };
    let mut bytes = vec![0u8; 240];
    bytes[0..3].copy_from_slice(&[2, 1, 6]);
    bytes[4..8].copy_from_slice(&packet.header[4..8]);
    bytes[10..16].copy_from_slice(&packet.header[10..16]);
    bytes[24..44].copy_from_slice(&packet.header[24..44]);
    bytes[236..240].copy_from_slice(&[99, 130, 83, 99]);
    option(&mut bytes, 53, &[message]);
    option(&mut bytes, 54, &server.octets());
    if let Some(id) = packet.options.get(&61) {
        option(&mut bytes, 61, id);
    }
    let mut boot_file = None;
    if message != 6 {
        if packet.message != 8 {
            bytes[16..20].copy_from_slice(&client.ip.octets());
            option(&mut bytes, 51, &config.dhcp.lease_seconds.to_be_bytes());
            if config.dhcp.lease_seconds >= 8 {
                option(
                    &mut bytes,
                    58,
                    &(config.dhcp.lease_seconds / 2).to_be_bytes(),
                );
                option(
                    &mut bytes,
                    59,
                    &((u64::from(config.dhcp.lease_seconds) * 7 / 8) as u32).to_be_bytes(),
                );
            }
        }
        let mask = u32::MAX
            .checked_shl(32 - u32::from(client.prefix_length))
            .unwrap_or(0);
        option(&mut bytes, 1, &mask.to_be_bytes());
        option(&mut bytes, 12, client.hostname.as_bytes());
        option(&mut bytes, 15, config.client_options.domain.as_bytes());
        for (code, ips) in [
            (6, &config.client_options.dns),
            (42, &config.client_options.ntp),
        ] {
            option(
                &mut bytes,
                code,
                &ips.iter().flat_map(|ip| ip.octets()).collect::<Vec<_>>(),
            );
        }
        if packet.route_mode() == "classless" {
            let mut routes = Vec::new();
            for route in &config.client_options.classless_routes {
                let (net, prefix) = route.split_once('/').context("route requires prefix")?;
                let prefix: u8 = prefix.parse()?;
                ensure!((1..=32).contains(&prefix), "invalid route prefix");
                let net: Ipv4Addr = net.parse()?;
                routes.push(prefix);
                routes.extend_from_slice(&net.octets()[..usize::from(prefix).div_ceil(8)]);
                routes.extend_from_slice(&client.gateway.octets());
            }
            option(&mut bytes, 121, &routes);
        } else {
            option(&mut bytes, 3, &client.gateway.octets());
        }
        let (stage, architecture) = packet.stage();
        if matches!(stage, "bios" | "uefi" | "ipxe") {
            if stage != "ipxe" && stage != client.boot_type {
                tracing::warn!(mac=%format_mac(&client.mac),location=%client.location,hostname=%client.hostname,boot_stage=stage,architecture,detected=stage, configured=%client.boot_type, "boot mode mismatch");
            }
            if let Some(files) = config.boot.architectures.get(architecture) {
                boot_file = if stage == "ipxe" {
                    files.ipxe_file.clone()
                } else if client.boot_type == "bios" {
                    files.bios_file.clone()
                } else {
                    files.uefi_file.clone()
                };
            }
            if let Some(file) = &boot_file {
                bytes[20..24].copy_from_slice(&config.boot.tftp_server.octets());
                if file.len() < 128 {
                    bytes[108..108 + file.len()].copy_from_slice(file.as_bytes());
                }
                option(
                    &mut bytes,
                    66,
                    config.boot.tftp_server.to_string().as_bytes(),
                );
                option(&mut bytes, 67, file.as_bytes());
            } else {
                tracing::warn!(mac=%format_mac(&client.mac),location=%client.location,hostname=%client.hostname,boot_stage=stage, architecture, "no matching boot file");
            }
        }
    } else {
        bytes[12..16].fill(0);
        if !packet.giaddr.is_unspecified() {
            bytes[10] |= 0x80;
        }
    }
    let maximum = packet
        .options
        .get(&57)
        .map(|v| usize::from(u16::from_be_bytes([v[0], v[1]])).max(576))
        .unwrap_or(576);
    let mut relay_option_omitted = false;
    if !packet.giaddr.is_unspecified()
        && let Some(relay) = packet.options.get(&82)
    {
        let encoded_size = relay.len() + relay.len().div_ceil(255) * 2;
        // RFC 3046 section 2.2: omit the entire relay option if it does not fit.
        if bytes.len() + encoded_size < maximum {
            option(&mut bytes, 82, relay);
        } else {
            relay_option_omitted = true;
        }
    }
    bytes.push(255);
    ensure!(
        bytes.len() <= maximum,
        "response exceeds client maximum DHCP size ({maximum})"
    );
    bytes.resize(bytes.len().max(300), 0);
    let destination = if !packet.giaddr.is_unspecified() {
        SocketAddrV4::new(packet.giaddr, 67)
    } else if message == 6 {
        SocketAddrV4::new(Ipv4Addr::BROADCAST, 68)
    } else if !packet.ciaddr.is_unspecified() {
        SocketAddrV4::new(packet.ciaddr, 68)
    } else {
        SocketAddrV4::new(Ipv4Addr::BROADCAST, 68)
    };
    Ok(Some(Reply {
        bytes,
        destination,
        message,
        boot_file,
        relay_option_omitted,
    }))
}

pub async fn serve(config: Arc<Config>, shared: Arc<Shared>) -> Result<()> {
    let socket = UdpSocket::bind((config.dhcp.listen_ip, 67))
        .await
        .context("bind UDP/67")?;
    socket.set_broadcast(true)?;
    let explicit = config
        .dhcp
        .server_identifier
        .or_else(|| (!config.dhcp.listen_ip.is_unspecified()).then_some(config.dhcp.listen_ip));
    #[cfg(target_os = "linux")]
    if explicit.is_none() {
        enable_pktinfo(&socket)?;
    }
    #[cfg(not(target_os = "linux"))]
    if explicit.is_none() {
        anyhow::bail!("wildcard requires server_identifier on non-Linux platforms");
    }
    shared.dhcp_ready.store(true, Ordering::Relaxed);
    tracing::info!(listen=%socket.local_addr()?,"DHCP listening");
    let mut buffer = vec![0u8; 65535];
    loop {
        let (len, server) = if let Some(server) = explicit {
            let (len, _) = socket.recv_from(&mut buffer).await?;
            (len, server)
        } else {
            #[cfg(target_os = "linux")]
            {
                receive_pktinfo(&socket, &mut buffer).await?
            }
            #[cfg(not(target_os = "linux"))]
            {
                unreachable!()
            }
        };
        let start = Instant::now();
        let packet = match Packet::parse(&buffer[..len]) {
            Ok(packet) => packet,
            Err(error) => {
                shared.record_request("unknown", "unknown", "unknown", "unknown", "invalid");
                shared.errors.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(%error,boot_stage="unknown",result="malformed","DHCP request");
                continue;
            }
        };
        let snapshot = shared.snapshot.read().expect("snapshot lock").clone();
        let client = snapshot.as_ref().and_then(|s| s.records.get(&packet.mac));
        let location = client.map(|c| c.location.as_str()).unwrap_or("unknown");
        let hostname = client.map(|c| c.hostname.as_str()).unwrap_or("unknown");
        let (stage, architecture) = packet.stage();
        shared.record_request(
            location,
            hostname,
            stage,
            packet.route_mode(),
            message_name(packet.message),
        );
        let mac = format_mac(&packet.mac);
        // Put context on each event: an INFO span is disabled at WARN/ERROR levels.
        macro_rules! request_log {
            ($level:ident, $($fields:tt)*) => {
                tracing::$level!(%mac,location,hostname,dhcp_message=message_name(packet.message),xid=u32::from_be_bytes(packet.header[4..8].try_into().unwrap()),relay_ip=%packet.giaddr,boot_stage=stage,architecture,route_mode=packet.route_mode(),$($fields)*,"DHCP request")
            };
        }
        let outcome = if let Some(client) = client {
            reply(&packet, client, &config, server)
        } else {
            shared.unknown.fetch_add(1, Ordering::Relaxed);
            Ok(None)
        };
        match outcome {
            Ok(Some(response)) => {
                if response.relay_option_omitted {
                    shared.errors.fetch_add(1, Ordering::Relaxed);
                    request_log!(warn, result = "relay_option_omitted");
                }
                match socket.send_to(&response.bytes, response.destination).await {
                    Ok(_) => {
                        shared.record_response(message_name(response.message));
                        request_log!(info,assigned_ip=%client.unwrap().ip,boot_file=?response.boot_file,result=message_name(response.message));
                    }
                    Err(error) => {
                        shared.errors.fetch_add(1, Ordering::Relaxed);
                        request_log!(error,%error,result="send_error");
                    }
                }
            }
            Ok(None) => {
                request_log!(
                    info,
                    result = if client.is_none() {
                        "unknown_or_unavailable"
                    } else {
                        "no_response"
                    }
                )
            }
            Err(error) => {
                shared.errors.fetch_add(1, Ordering::Relaxed);
                request_log!(warn,%error,result="error");
            }
        }
        shared.record_duration(start.elapsed().as_secs_f64());
    }
}

pub fn message_name(value: u8) -> &'static str {
    match value {
        1 => "discover",
        2 => "offer",
        3 => "request",
        4 => "decline",
        5 => "ack",
        6 => "nak",
        7 => "release",
        8 => "inform",
        _ => "unknown",
    }
}

#[cfg(target_os = "linux")]
fn enable_pktinfo(socket: &UdpSocket) -> Result<()> {
    use std::os::fd::AsRawFd;
    let enabled: libc::c_int = 1;
    // SAFETY: valid socket descriptor and correctly sized integer pointer.
    let result = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::IPPROTO_IP,
            libc::IP_PKTINFO,
            (&enabled as *const libc::c_int).cast(),
            std::mem::size_of_val(&enabled) as libc::socklen_t,
        )
    };
    if result < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
async fn receive_pktinfo(socket: &UdpSocket, buffer: &mut [u8]) -> Result<(usize, Ipv4Addr)> {
    use std::os::fd::AsRawFd;
    loop {
        socket.readable().await?;
        let result = socket.try_io(tokio::io::Interest::READABLE, || {
            // SAFETY: all recvmsg buffers are live, writable and sized; ancillary
            // buffer is aligned as usize and CMSG helpers are used within msg_controllen.
            unsafe {
                let mut control = [0usize; 32];
                let mut iov = libc::iovec {
                    iov_base: buffer.as_mut_ptr().cast(),
                    iov_len: buffer.len(),
                };
                let mut msg: libc::msghdr = std::mem::zeroed();
                msg.msg_iov = &mut iov;
                msg.msg_iovlen = 1;
                msg.msg_control = control.as_mut_ptr().cast();
                msg.msg_controllen = std::mem::size_of_val(&control);
                let len = libc::recvmsg(socket.as_raw_fd(), &mut msg, 0);
                if len < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let mut header = libc::CMSG_FIRSTHDR(&msg);
                while !header.is_null() {
                    if (*header).cmsg_level == libc::IPPROTO_IP
                        && (*header).cmsg_type == libc::IP_PKTINFO
                    {
                        let info = std::ptr::read_unaligned(
                            libc::CMSG_DATA(header).cast::<libc::in_pktinfo>(),
                        );
                        return Ok((
                            len as usize,
                            Ipv4Addr::from(info.ipi_spec_dst.s_addr.to_ne_bytes()),
                        ));
                    }
                    header = libc::CMSG_NXTHDR(&msg, header);
                }
                Err(std::io::Error::other("missing destination IP metadata"))
            }
        });
        match result {
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            other => return Ok(other?),
        }
    }
}
