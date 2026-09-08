use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{
    collections::HashMap,
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub dhcp: DhcpConfig,
    pub http: HttpConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    pub client_options: ClientOptions,
    pub boot: BootConfig,
    pub clients_source: ClientsSource,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoggingConfig {
    pub level: String,
    pub format: LogFormat,
    pub disable_timestamp: bool,
    pub color: bool,
    #[serde(default)]
    pub dhcp_packet_debug: bool,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    Json,
    Text,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".into(),
            format: LogFormat::Json,
            disable_timestamp: false,
            color: true,
            dhcp_packet_debug: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DhcpConfig {
    pub listen_ip: Ipv4Addr,
    pub server_identifier: Option<Ipv4Addr>,
    pub lease_seconds: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpConfig {
    pub listen: SocketAddr,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientOptions {
    pub dns: Vec<Ipv4Addr>,
    pub ntp: Vec<Ipv4Addr>,
    #[serde(default = "default_timezone")]
    pub timezone: String,
    pub classless_routes: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootConfig {
    pub tftp_server: Ipv4Addr,
    pub architectures: HashMap<String, BootFiles>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootFiles {
    pub bios_file: Option<String>,
    pub uefi_file: Option<String>,
    pub ipxe_file: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientsSource {
    pub url: String,
    pub poll_interval_seconds: u64,
    pub timeout_seconds: u64,
    pub state_file: PathBuf,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read config {}", path.display()))?;
        let config: Self = toml::from_str(&text).context("parse TOML config")?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.dhcp.listen_ip.is_unspecified() || is_unicast(self.dhcp.listen_ip),
            "dhcp.listen_ip must be wildcard or unicast"
        );
        if let Some(ip) = self.dhcp.server_identifier {
            ensure!(
                is_unicast(ip),
                "dhcp.server_identifier must be a concrete unicast IPv4 address"
            );
        }
        ensure!(
            self.dhcp.lease_seconds > 0,
            "lease_seconds must be positive"
        );
        ensure!(self.http.listen.port() > 0, "HTTP port must be positive");
        tracing_subscriber::EnvFilter::try_new(&self.logging.level)
            .context("invalid logging.level")?;
        ensure!(
            self.clients_source.poll_interval_seconds > 0,
            "poll_interval_seconds must be positive"
        );
        ensure!(
            self.clients_source.timeout_seconds > 0,
            "timeout_seconds must be positive"
        );
        let source = &self.clients_source.url;
        ensure!(
            source.starts_with("https://") || source.starts_with("http://"),
            "clients_source.url must use HTTP or HTTPS"
        );
        ensure!(
            !source.chars().any(char::is_whitespace)
                && source
                    .split_once("://")
                    .is_some_and(|(_, host)| !host.is_empty() && !host.starts_with('/')),
            "invalid clients_source.url"
        );
        ensure!(
            !self.clients_source.state_file.as_os_str().is_empty(),
            "state_file must not be empty"
        );
        ensure!(
            valid_timezone(&self.client_options.timezone),
            "invalid timezone"
        );
        for ip in self
            .client_options
            .dns
            .iter()
            .chain(&self.client_options.ntp)
        {
            ensure!(is_unicast(*ip), "DNS/NTP addresses must be unicast: {ip}");
        }
        ensure!(
            !self.client_options.classless_routes.is_empty(),
            "classless_routes must not be empty"
        );
        for route in &self.client_options.classless_routes {
            parse_route(route)?;
        }
        ensure!(
            is_unicast(self.boot.tftp_server),
            "tftp_server must be unicast"
        );
        for (architecture, files) in &self.boot.architectures {
            ensure!(!architecture.is_empty(), "empty architecture name");
            for file in [&files.bios_file, &files.uefi_file, &files.ipxe_file]
                .into_iter()
                .flatten()
            {
                ensure!(
                    !file.is_empty() && file.len() <= 255 && !file.chars().any(char::is_control),
                    "invalid boot filename for {architecture}: expected 1..255 bytes without control characters"
                );
            }
        }
        Ok(())
    }
}

fn default_timezone() -> String {
    "Etc/UTC".into()
}

fn valid_timezone(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 255
        && !value.chars().any(char::is_control)
        && value.split('/').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'+'))
        })
}

pub fn prefix_mask(prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    }
}

pub fn parse_route(route: &str) -> Result<(Ipv4Addr, u8)> {
    let (network, prefix) = route
        .split_once('/')
        .context("route must use IPv4/prefix syntax")?;
    let network: Ipv4Addr = network
        .parse()
        .with_context(|| format!("invalid route {route}"))?;
    let prefix: u8 = prefix
        .parse()
        .with_context(|| format!("invalid route prefix {route}"))?;
    ensure!(
        (1..=32).contains(&prefix),
        "route {route}: prefix must be 1..32 (default route is forbidden)"
    );
    ensure!(
        u32::from(network) & !prefix_mask(prefix) == 0,
        "route {route}: host bits must be zero"
    );
    Ok((network, prefix))
}

pub fn is_unicast(ip: Ipv4Addr) -> bool {
    !ip.is_unspecified() && !ip.is_multicast() && !ip.is_broadcast() && ip.octets()[0] < 240
}

pub fn valid_hostname(name: &str) -> bool {
    let name = name.strip_suffix('.').unwrap_or(name);
    !name.is_empty()
        && name.len() <= 253
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-')
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn example_and_wildcard_are_valid() {
        let mut config: Config = toml::from_str(include_str!("../examples/server.toml")).unwrap();
        config.dhcp.server_identifier = None;
        config.validate().unwrap();
        config.dhcp.server_identifier = Some(Ipv4Addr::UNSPECIFIED);
        assert!(config.validate().is_err());
    }

    #[test]
    fn logging_config_defaults_and_validation() {
        let config: Config = toml::from_str(include_str!("../examples/server.toml")).unwrap();
        assert_eq!(config.logging.level, "info");
        assert_eq!(config.logging.format, LogFormat::Text);
        assert!(!config.logging.disable_timestamp);
        assert!(config.logging.color);
        assert!(!config.logging.dhcp_packet_debug);

        let without_logging = include_str!("../examples/server.toml").replace(
            "[logging]\nlevel = \"info\"\nformat = \"text\"\ndisable_timestamp = false\ncolor = true\ndhcp_packet_debug = false\n\n",
            "",
        );
        let config: Config = toml::from_str(&without_logging).unwrap();
        assert_eq!(config.logging.level, "info");
        assert_eq!(config.logging.format, LogFormat::Json);
        assert_eq!(config.client_options.timezone, "Etc/UTC");
        config.validate().unwrap();

        let without_timezone =
            include_str!("../examples/server.toml").replace("timezone = \"Etc/UTC\"\n", "");
        let config: Config = toml::from_str(&without_timezone).unwrap();
        assert_eq!(config.client_options.timezone, "Etc/UTC");

        let json_logging = include_str!("../examples/server.toml").replace(
            "format = \"text\"\ndisable_timestamp = false",
            "format = \"json\"\ndisable_timestamp = true",
        );
        let config: Config = toml::from_str(&json_logging).unwrap();
        assert_eq!(config.logging.format, LogFormat::Json);
        assert!(config.logging.disable_timestamp);
        config.validate().unwrap();

        let invalid_level = include_str!("../examples/server.toml")
            .replace("level = \"info\"", "level = \"macdack==debug\"");
        let config: Config = toml::from_str(&invalid_level).unwrap();
        assert!(config.validate().is_err());

        let invalid_format = include_str!("../examples/server.toml")
            .replace("format = \"text\"", "format = \"pretty\"");
        assert!(toml::from_str::<Config>(&invalid_format).is_err());
    }

    #[test]
    fn route_validation() {
        assert!(parse_route("10.0.0.0/8").is_ok());
        assert!(parse_route("10.0.0.1/32").is_ok());
        for route in ["0.0.0.0/0", "10.0.0.1/8", "10.0.0.0/33", "::/8"] {
            assert!(parse_route(route).is_err());
        }
    }
}
