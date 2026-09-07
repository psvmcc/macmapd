use anyhow::{Context, Result, bail};
use macmapd::{
    APP_VERSION,
    config::{Config, LogFormat},
    dhcp,
    runtime::{self, Shared},
};
use std::{ffi::OsString, path::PathBuf, sync::Arc};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    let original_args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let Some((path, check)) = parse_args(original_args.clone())? else {
        return Ok(());
    };
    let config = Arc::new(Config::load(&path)?);
    init_logging(&config);
    if check {
        println!("configuration valid");
        return Ok(());
    }
    let shared = Arc::new(Shared::new());
    tokio::select! {
        result = dhcp::serve(config.clone(), shared.clone()) => result.context("DHCP stopped")?,
        result = runtime::serve_http(config.clone(), shared.clone()) => result.context("HTTP stopped")?,
        () = runtime::run_sync(config.clone(), shared.clone()) => bail!("synchronizer stopped"),
        result = shutdown_or_reload(&path) => match result? {
            Signal::Shutdown => (),
            Signal::Reload => reload_process(&original_args)?,
        },
    }
    tracing::info!("shutting down");
    Ok(())
}

fn parse_args(args: Vec<OsString>) -> Result<Option<(PathBuf, bool)>> {
    let mut path = PathBuf::from("/etc/macmapd/config.toml");
    let mut check = false;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.to_str().context("arguments must be UTF-8")? {
            "check-config" => check = true,
            "--config" => path = args.next().context("--config requires a path")?.into(),
            "--version" | "-V" => {
                println!("macmapd {APP_VERSION}");
                return Ok(None);
            }
            "--help" | "-h" => {
                println!("macmapd [check-config] [--config PATH] [--version]");
                return Ok(None);
            }
            _ => bail!("unknown argument: {}", arg.to_string_lossy()),
        }
    }
    Ok(Some((path, check)))
}

fn init_logging(config: &Config) {
    let filter = logging_filter(config);
    match (config.logging.format, config.logging.disable_timestamp) {
        (LogFormat::Json, true) => tracing_subscriber::fmt()
            .json()
            .without_time()
            .with_env_filter(filter)
            .init(),
        (LogFormat::Json, false) => tracing_subscriber::fmt()
            .json()
            .with_env_filter(filter)
            .init(),
        (LogFormat::Text, true) => tracing_subscriber::fmt()
            .without_time()
            .with_ansi(config.logging.color)
            .with_env_filter(filter)
            .init(),
        (LogFormat::Text, false) => tracing_subscriber::fmt()
            .with_ansi(config.logging.color)
            .with_env_filter(filter)
            .init(),
    }
}

fn logging_filter(config: &Config) -> EnvFilter {
    let filter = EnvFilter::new(config.logging.level.clone());
    if config.logging.dhcp_packet_debug {
        filter.add_directive(
            "macmapd::dhcp=debug"
                .parse()
                .expect("static DHCP logging directive"),
        )
    } else {
        filter
    }
}

enum Signal {
    Shutdown,
    Reload,
}

async fn shutdown_or_reload(path: &std::path::Path) -> Result<Signal> {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        let mut hup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
        loop {
            tokio::select! {
                result = tokio::signal::ctrl_c() => { result?; return Ok(Signal::Shutdown); },
                _ = term.recv() => return Ok(Signal::Shutdown),
                _ = hup.recv() => match Config::load(path) {
                    Ok(_) => {
                        tracing::info!(config=%path.display(), "configuration reload requested");
                        return Ok(Signal::Reload);
                    }
                    Err(error) => tracing::error!(%error,config=%path.display(), "configuration reload rejected"),
                }
            }
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await?;
        Ok(Signal::Shutdown)
    }
}

#[cfg(unix)]
fn reload_process(args: &[OsString]) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let executable = std::env::current_exe().context("resolve current executable")?;
    tracing::info!(executable=%executable.display(), "reloading configuration");
    let error = std::process::Command::new(executable).args(args).exec();
    Err(error).context("replace process for configuration reload")
}

#[cfg(not(unix))]
fn reload_process(_args: &[OsString]) -> Result<()> {
    bail!("configuration reload is only supported on Unix")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_and_overridden_config_paths() {
        assert_eq!(
            parse_args(vec![]).unwrap().unwrap().0,
            PathBuf::from("/etc/macmapd/config.toml")
        );
        assert_eq!(
            parse_args(vec!["--config".into(), "/tmp/custom.toml".into()])
                .unwrap()
                .unwrap()
                .0,
            PathBuf::from("/tmp/custom.toml")
        );
    }

    #[test]
    fn packet_debug_enables_dhcp_debug_with_an_info_global_level() {
        let mut config = Config::load(std::path::Path::new("examples/server.toml")).unwrap();
        config.logging.level = "info".into();
        config.logging.dhcp_packet_debug = true;
        let filter = logging_filter(&config).to_string();
        assert!(filter.contains("macmapd::dhcp=debug"), "{filter}");
    }
}
