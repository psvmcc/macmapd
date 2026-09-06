use anyhow::{Context, Result, bail};
use macmapd::{
    APP_VERSION,
    config::{Config, LogFormat},
    dhcp,
    runtime::{self, Shared},
};
use std::{path::PathBuf, sync::Arc};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    let mut path = PathBuf::from("server.toml");
    let mut check = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "check-config" => check = true,
            "--config" => path = args.next().context("--config requires a path")?.into(),
            "--version" | "-V" => {
                println!("macmapd {APP_VERSION}");
                return Ok(());
            }
            "--help" | "-h" => {
                println!("macmapd [check-config] [--config PATH] [--version]");
                return Ok(());
            }
            _ => bail!("unknown argument: {arg}"),
        }
    }
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
        result = shutdown() => result?,
    }
    tracing::info!("shutting down");
    Ok(())
}

fn init_logging(config: &Config) {
    let filter = EnvFilter::new(config.logging.level.clone());
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

async fn shutdown() -> Result<()> {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! { result = tokio::signal::ctrl_c() => result?, _ = term.recv() => {} }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    Ok(())
}
