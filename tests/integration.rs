use macmapd::{
    clients::Clients,
    config::Config,
    dhcp::{Packet, reply},
};
use std::net::Ipv4Addr;

// Wire-level exchange. The container smoke test exercises dhcp::serve itself
// and receives replies on the actual relay port 67.
#[test]
fn relay_discover_request_packets() {
    let config: Config = toml::from_str(include_str!("../examples/server.toml")).unwrap();
    let clients = Clients::parse(include_str!("../examples/clients.csv")).unwrap();
    let client = clients.records.values().next().unwrap();
    let mut bytes = vec![0u8; 240];
    bytes[..3].copy_from_slice(&[1, 1, 6]);
    bytes[4..8].copy_from_slice(&1234u32.to_be_bytes());
    bytes[24..28].copy_from_slice(&[127, 0, 0, 1]);
    bytes[28..34].copy_from_slice(&client.mac);
    bytes[236..240].copy_from_slice(&[99, 130, 83, 99]);
    bytes.extend([53, 1, 1, 55, 1, 121, 255]);
    let request = Packet::parse(&bytes).unwrap();
    let offer = reply(&request, client, &config, Ipv4Addr::LOCALHOST)
        .unwrap()
        .unwrap();
    assert_eq!(offer.destination.ip(), &Ipv4Addr::LOCALHOST);
    assert_eq!(offer.destination.port(), 67);
    assert_eq!(&offer.bytes[16..20], &client.ip.octets());
    bytes[242] = 3;
    bytes.pop();
    bytes.extend([50, 4]);
    bytes.extend(client.ip.octets());
    bytes.extend([54, 4]);
    bytes.extend(Ipv4Addr::LOCALHOST.octets());
    bytes.push(255);
    let request = Packet::parse(&bytes).unwrap();
    assert_eq!(
        reply(&request, client, &config, Ipv4Addr::LOCALHOST)
            .unwrap()
            .unwrap()
            .message,
        5
    );
}
