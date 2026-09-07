use macmapd::{
    clients::Clients,
    config::Config,
    dhcp::{Packet, reply},
};
use std::{collections::BTreeMap, net::Ipv4Addr};

const SERVER: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 10);
const CLIENT_IP: [u8; 4] = [1, 2, 3, 4];

fn config() -> Config {
    toml::from_str(include_str!("../examples/server.toml")).unwrap()
}

fn wire(message: u8, options: &[(u8, &[u8])]) -> Vec<u8> {
    let mut bytes = vec![0; 240];
    bytes[..3].copy_from_slice(&[1, 1, 6]);
    bytes[4..8].copy_from_slice(&[0x12, 0x34, 0x56, 0x78]);
    bytes[28..34].copy_from_slice(&[0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]);
    bytes[236..240].copy_from_slice(&[99, 130, 83, 99]);
    for (code, value) in std::iter::once((53, &[message][..])).chain(options.iter().copied()) {
        bytes.extend([code, value.len() as u8]);
        bytes.extend_from_slice(value);
    }
    bytes.push(255);
    bytes
}

fn answer(packet: &Packet, config: &Config) -> Option<macmapd::dhcp::Reply> {
    let clients = Clients::parse(include_str!("../examples/clients.csv")).unwrap();
    reply(
        packet,
        clients.records.get(&packet.mac).unwrap(),
        config,
        SERVER,
    )
    .unwrap()
}

fn response_options(bytes: &[u8]) -> BTreeMap<u8, Vec<u8>> {
    let mut options: BTreeMap<u8, Vec<u8>> = BTreeMap::new();
    let mut pos = 240;
    while pos < bytes.len() {
        let code = bytes[pos];
        pos += 1;
        if code == 255 {
            break;
        }
        if code == 0 {
            continue;
        }
        let length = bytes[pos] as usize;
        pos += 1;
        options
            .entry(code)
            .or_default()
            .extend_from_slice(&bytes[pos..pos + length]);
        pos += length;
    }
    options
}

#[test]
fn route_matrix_and_architectures() {
    for (arch, user, vendor, stage, architecture) in [
        (&[0, 0][..], &b""[..], &b"PXEClient"[..], "bios", "x86_64"),
        (&[0, 7][..], &b""[..], &b"PXEClient"[..], "uefi", "x86_64"),
        (&[0, 9][..], &b""[..], &b"PXEClient"[..], "uefi", "x86_64"),
        (&[0, 11][..], &b""[..], &b"PXEClient"[..], "uefi", "arm64"),
        (
            &[0, 11][..],
            &b"\x04iPXE"[..],
            &b"PXEClient"[..],
            "ipxe",
            "arm64",
        ),
        (
            &[0, 9][..],
            &b"iPXE"[..],
            &b"PXEClient"[..],
            "ipxe",
            "x86_64",
        ),
        (&[][..], &b""[..], &b"ordinary-os"[..], "os", "unknown"),
    ] {
        for requested in [false, true] {
            let prl: &[u8] = if requested {
                &[1, 3, 6, 121]
            } else {
                &[1, 3, 6]
            };
            let mut input_options = vec![(77, user), (60, vendor), (55, prl)];
            if !arch.is_empty() {
                input_options.push((93, arch));
            }
            let packet = Packet::parse(&wire(1, &input_options)).unwrap();
            assert_eq!(packet.stage(), (stage, architecture));
            let response = answer(&packet, &config()).unwrap();
            let options = response_options(&response.bytes);
            let classless = requested && !matches!(stage, "bios" | "uefi");
            assert_eq!(
                options.contains_key(&121),
                classless,
                "{stage} requested={requested}"
            );
            assert_eq!(options.contains_key(&3), !classless);
            if classless {
                assert_eq!(options[&121], [8, 10, 1, 2, 3, 1, 12, 172, 16, 1, 2, 3, 1]);
            }
            if stage == "os" {
                assert!(response.boot_file.is_none());
            }
            if stage == "ipxe" {
                assert!(response.boot_file.unwrap().ends_with("boot.ipxe"));
            }
        }
    }
}

#[test]
fn request_states_and_relay_echo() {
    for selecting in [false, true] {
        let mut options = vec![(50, &CLIENT_IP[..])];
        let identifier = SERVER.octets();
        if selecting {
            options.push((54, &identifier));
        }
        let packet = Packet::parse(&wire(3, &options)).unwrap();
        assert_eq!(answer(&packet, &config()).unwrap().message, 5);
    }
    let mut bytes = wire(3, &[(82, &[1, 3, b'a', b'b', b'c'])]);
    bytes[12..16].copy_from_slice(&CLIENT_IP);
    let packet = Packet::parse(&bytes).unwrap();
    let response = answer(&packet, &config()).unwrap();
    assert_eq!(response.destination.ip().octets(), CLIENT_IP);
    assert!(!response_options(&response.bytes).contains_key(&82));
    bytes[24..28].copy_from_slice(&[198, 51, 100, 77]);
    let packet = Packet::parse(&bytes).unwrap();
    let response = answer(&packet, &config()).unwrap();
    assert_eq!(response.destination.to_string(), "198.51.100.77:67");
    assert_eq!(
        response_options(&response.bytes)[&82],
        [1, 3, b'a', b'b', b'c']
    );
}

#[test]
fn malformed_request_state_combinations_do_not_acknowledge() {
    // RFC 2131 table 4: renewals have neither 50 nor 54; selecting has
    // both options and zero ciaddr; INIT-REBOOT has 50 and zero ciaddr.
    let identifier = SERVER.octets();
    for (ciaddr, options) in [
        (CLIENT_IP, vec![(50, &CLIENT_IP[..])]),
        (CLIENT_IP, vec![(54, &identifier[..])]),
        (CLIENT_IP, vec![(50, &CLIENT_IP[..]), (54, &identifier[..])]),
        ([0; 4], vec![(54, &identifier[..])]),
        ([0; 4], vec![]),
    ] {
        let mut bytes = wire(3, &options);
        bytes[12..16].copy_from_slice(&ciaddr);
        if let Ok(packet) = Packet::parse(&bytes) {
            let clients = Clients::parse(include_str!("../examples/clients.csv")).unwrap();
            let result = reply(&packet, &clients.records[&packet.mac], &config(), SERVER);
            assert!(
                !matches!(result, Ok(Some(_))),
                "malformed REQUEST was answered"
            );
        }
    }
}

#[test]
fn inform_nak_and_other_server() {
    let mut bytes = wire(8, &[]);
    bytes[12..16].copy_from_slice(&CLIENT_IP);
    let response = answer(&Packet::parse(&bytes).unwrap(), &config()).unwrap();
    assert_eq!(&response.bytes[16..20], &[0; 4]);
    let options = response_options(&response.bytes);
    for code in [51, 58, 59] {
        assert!(!options.contains_key(&code));
    }
    let mut bytes = wire(3, &[(50, &[1, 2, 3, 99])]);
    bytes[24..28].copy_from_slice(&[198, 51, 100, 2]);
    let response = answer(&Packet::parse(&bytes).unwrap(), &config()).unwrap();
    assert_eq!(response.message, 6);
    assert_ne!(response.bytes[10] & 0x80, 0);
    assert!(!response_options(&response.bytes).contains_key(&1));
    let packet = Packet::parse(&wire(3, &[(50, &CLIENT_IP), (54, &[192, 0, 2, 99])])).unwrap();
    assert!(answer(&packet, &config()).is_none());
    for message in [4, 7] {
        assert!(answer(&Packet::parse(&wire(message, &[])).unwrap(), &config()).is_none());
    }
}

#[test]
fn malformed_options_and_long_routes() {
    for options in [
        vec![(53, &b"\x01"[..])],
        vec![(93, &b"\x00"[..])],
        vec![(54, &b"\x01\x02"[..])],
    ] {
        assert!(Packet::parse(&wire(1, &options)).is_err());
    }
    let mut truncated = wire(1, &[]);
    truncated.pop();
    truncated.extend([200, 4, 1]);
    assert!(Packet::parse(&truncated).is_err());
    let mut cfg = config();
    cfg.client_options.classless_routes = (1..=40).map(|i| format!("10.0.{i}.0/24")).collect();
    let packet = Packet::parse(&wire(1, &[(55, &[121]), (57, &[5, 220])])).unwrap();
    let response = answer(&packet, &cfg).unwrap();
    assert_eq!(response_options(&response.bytes)[&121].len(), 320);
    cfg.client_options.classless_routes = (1..=100).map(|i| format!("10.0.{i}.0/24")).collect();
    let packet = Packet::parse(&wire(1, &[(55, &[121])])).unwrap();
    let clients = Clients::parse(include_str!("../examples/clients.csv")).unwrap();
    assert!(reply(&packet, &clients.records[&packet.mac], &cfg, SERVER).is_err());
}

#[test]
fn relay_option_is_omitted_only_when_it_does_not_fit() {
    let mut relay = vec![1, 253];
    relay.extend([b'a'; 253]);
    for (maximum, omitted) in [(576u16, true), (1500, false)] {
        let mut bytes = wire(
            1,
            &[(93, &[0, 9]), (82, &relay), (57, &maximum.to_be_bytes())],
        );
        bytes[24..28].copy_from_slice(&[192, 0, 2, 1]);
        let response = answer(&Packet::parse(&bytes).unwrap(), &config()).unwrap();
        assert_eq!(response.message, 2);
        assert_eq!(response.relay_option_omitted, omitted);
        assert!(response.bytes.len() <= usize::from(maximum));
        let options = response_options(&response.bytes);
        if omitted {
            assert!(!options.contains_key(&82));
        } else {
            assert_eq!(options[&82], relay);
        }
    }
}

#[test]
fn ipxe_uses_vendor_architecture_fallback_and_prefers_option_93() {
    for (architecture, expected) in [(9u16, "x86_64"), (11, "arm64")] {
        let vendor = format!("PXEClient:Arch:{architecture:05}:UNDI:003016");
        let packet = Packet::parse(&wire(1, &[(77, b"iPXE"), (60, vendor.as_bytes())])).unwrap();
        assert_eq!(packet.stage(), ("ipxe", expected));
        let response = answer(&packet, &config()).unwrap();
        assert_eq!(
            response.boot_file,
            config().boot.architectures[expected].ipxe_file
        );
    }
    let packet = Packet::parse(&wire(
        1,
        &[(77, b"iPXE"), (60, b"PXEClient:Arch:00009"), (93, &[0, 11])],
    ))
    .unwrap();
    assert_eq!(packet.stage(), ("ipxe", "arm64"));
}
