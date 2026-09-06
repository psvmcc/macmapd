use crate::config::{is_unicast, prefix_mask, valid_hostname};
use anyhow::{Context, Result, ensure};
use std::{collections::HashMap, net::Ipv4Addr};

#[derive(Debug, Clone)]
pub struct Client {
    pub location: String,
    pub mac: [u8; 6],
    pub boot_type: String,
    pub hostname: String,
    pub ip: Ipv4Addr,
    pub prefix_length: u8,
    pub gateway: Ipv4Addr,
}

#[derive(Debug, Clone, Default)]
pub struct Clients {
    pub records: HashMap<[u8; 6], Client>,
    pub by_ip: HashMap<Ipv4Addr, [u8; 6]>,
}

impl Clients {
    pub fn parse(text: &str) -> Result<Self> {
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(false)
            .trim(csv::Trim::All)
            .from_reader(text.as_bytes());
        let mut clients = Self::default();
        for (index, row) in reader.records().enumerate() {
            let row = row.with_context(|| format!("CSV record {}", index + 1))?;
            let line = row.position().map_or(index as u64 + 1, |p| p.line());
            if index == 0
                && row.iter().eq([
                    "location",
                    "hostname",
                    "boot_type",
                    "mac",
                    "ip",
                    "prefix_length",
                    "gateway",
                ])
            {
                continue;
            }
            let client = parse_client(&row).with_context(|| format!("CSV line {line}"))?;
            ensure!(
                !clients.records.contains_key(&client.mac),
                "CSV line {line}: duplicate MAC"
            );
            ensure!(
                !clients.by_ip.contains_key(&client.ip),
                "CSV line {line}: duplicate IP {}",
                client.ip
            );
            clients.by_ip.insert(client.ip, client.mac);
            clients.records.insert(client.mac, client);
        }
        Ok(clients)
    }
}

fn parse_client(row: &csv::StringRecord) -> Result<Client> {
    ensure!(row.len() == 7, "expected exactly seven fields");
    ensure!(!row[0].is_empty() && row[0].len() <= 64, "invalid location");
    ensure!(valid_hostname(&row[1]), "invalid hostname");
    ensure!(
        matches!(&row[2], "bios" | "uefi"),
        "boot_type must be bios or uefi"
    );
    let pieces: Vec<_> = row[3].split(':').collect();
    ensure!(
        pieces.len() == 6 && pieces.iter().all(|part| part.len() == 2),
        "MAC must contain six colon-separated hex octets"
    );
    let mut mac = [0; 6];
    for (byte, part) in mac.iter_mut().zip(pieces) {
        *byte = u8::from_str_radix(part, 16).context("invalid MAC octet")?;
    }
    ensure!(
        mac != [0; 6] && mac[0] & 1 == 0,
        "MAC must be a nonzero unicast address"
    );
    let ip: Ipv4Addr = row[4].parse().context("invalid client IPv4")?;
    let prefix_length: u8 = row[5].parse().context("invalid prefix length")?;
    // /31 is a point-to-point subnet (RFC 3021): both addresses are usable, so
    // only the regular network/broadcast restrictions are skipped for /31.
    ensure!(
        (1..=31).contains(&prefix_length),
        "prefix must be 1..31; /32 is unsupported for Ethernet assignments"
    );
    let gateway: Ipv4Addr = row[6].parse().context("invalid gateway IPv4")?;
    ensure!(
        is_unicast(ip) && is_unicast(gateway),
        "client and gateway must be unicast IPv4 addresses"
    );
    let mask = prefix_mask(prefix_length);
    let network = u32::from(ip) & mask;
    let broadcast = network | !mask;
    ensure!(
        u32::from(gateway) & mask == network,
        "gateway must belong to client subnet"
    );
    if prefix_length < 31 {
        ensure!(
            u32::from(ip) != network && u32::from(ip) != broadcast,
            "client IP cannot be network or broadcast"
        );
        ensure!(
            u32::from(gateway) != network && u32::from(gateway) != broadcast,
            "gateway cannot be network or broadcast"
        );
    }
    ensure!(ip != gateway, "gateway cannot equal client IP");
    Ok(Client {
        location: row[0].into(),
        mac,
        hostname: row[1].into(),
        boot_type: row[2].into(),
        ip,
        prefix_length,
        gateway,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    const ROW: &str = "dc1,host.name,uefi,AA:BB:CC:DD:EE:FF,1.2.3.4,24,1.2.3.1\n";
    #[test]
    fn duplicates_and_shared_hostname() {
        let other = "dc1,host.name,bios,AA:BB:CC:DD:EE:AA,1.2.3.5,24,1.2.3.1\n";
        assert_eq!(
            Clients::parse(&format!("{ROW}{other}"))
                .unwrap()
                .records
                .len(),
            2
        );
        assert!(Clients::parse(&format!("{ROW}{}", ROW.to_lowercase())).is_err());
        assert!(Clients::parse(&format!("{ROW}{}", other.replace("1.2.3.5", "1.2.3.4"))).is_err());
    }
    #[test]
    fn invalid_networks_reject_whole_file() {
        for (from, to) in [
            ("1.2.3.4", "1.2.3.0"),
            ("1.2.3.4", "1.2.3.255"),
            ("1.2.3.1", "1.2.4.1"),
            (",24,", ",32,"),
            ("host.name", "bad_name"),
            ("dc1", ""),
        ] {
            assert!(Clients::parse(&ROW.replace(from, to)).is_err(), "{to}");
        }
    }

    #[test]
    fn point_to_point_prefix_is_supported() {
        let row = ROW.replace(",1.2.3.4,24,1.2.3.1", ",1.2.3.4,31,1.2.3.5");
        let clients = Clients::parse(&row).unwrap();
        let client = clients.records.values().next().unwrap();
        assert_eq!(client.prefix_length, 31);
        assert_eq!(client.gateway, "1.2.3.5".parse::<Ipv4Addr>().unwrap());
    }
    #[test]
    fn optional_header_and_example() {
        assert_eq!(
            Clients::parse(&format!(
                "location,hostname,boot_type,mac,ip,prefix_length,gateway\n{ROW}"
            ))
            .unwrap()
            .records
            .len(),
            1
        );
        assert_eq!(
            Clients::parse(include_str!("../examples/clients.csv"))
                .unwrap()
                .records
                .len(),
            2
        );
    }
}
