use macmapd::{
    clients::Clients,
    config::Config,
    dhcp::{Packet, reply},
};
use std::net::Ipv4Addr;

#[tokio::test]
async fn relay_discover_request_over_udp() {
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
    let relay = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let server = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    relay
        .send_to(&bytes, server.local_addr().unwrap())
        .await
        .unwrap();
    let mut buffer = [0; 2048];
    let (len, peer) = server.recv_from(&mut buffer).await.unwrap();
    let request = Packet::parse(&buffer[..len]).unwrap();
    let offer = reply(&request, client, &config, Ipv4Addr::LOCALHOST)
        .unwrap()
        .unwrap();
    assert_eq!(offer.destination.ip(), &Ipv4Addr::LOCALHOST);
    assert_eq!(offer.destination.port(), 67);
    assert_eq!(&offer.bytes[16..20], &client.ip.octets());
    server.send_to(&offer.bytes, peer).await.unwrap();
    let (len, _) = relay.recv_from(&mut buffer).await.unwrap();
    assert_eq!(&buffer[..len], offer.bytes.as_slice());
    let mut request = request;
    request.message = 3;
    request.options.insert(50, client.ip.octets().to_vec());
    request
        .options
        .insert(54, Ipv4Addr::LOCALHOST.octets().to_vec());
    assert_eq!(
        reply(&request, client, &config, Ipv4Addr::LOCALHOST)
            .unwrap()
            .unwrap()
            .message,
        5
    );
}
