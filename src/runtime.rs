//! Immutable client snapshots, durable polling, and operational endpoints.
use crate::{APP_VERSION, clients::Clients, config::Config};
use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderName, StatusCode, header::CONTENT_TYPE},
    response::IntoResponse,
    routing::get,
};
use reqwest::header::{ETAG, IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const MAX_CSV_BYTES: usize = 16 * 1024 * 1024;
const SERVER_HEADER: &str = concat!("macdack/", env!("CARGO_PKG_VERSION"));
type RequestLabels = (String, String, String, String, String);
const METRIC_IDLE_TTL: Duration = Duration::from_secs(24 * 3600);

#[derive(Default)]
struct RequestMetrics {
    series: BTreeMap<RequestLabels, (u64, Instant)>,
    last_pruned: Option<Instant>,
}

impl RequestMetrics {
    fn prune(&mut self, now: Instant) {
        if self
            .last_pruned
            .is_none_or(|last| now.duration_since(last) >= Duration::from_secs(60))
        {
            self.series
                .retain(|_, (_, seen)| now.duration_since(*seen) < METRIC_IDLE_TTL);
            self.last_pruned = Some(now);
        }
    }
}

#[derive(Default)]
struct SyncStatus {
    source: Option<&'static str>,
    last_success: Option<u64>,
    data_since: Option<u64>,
    last_error: Option<String>,
}

#[derive(Default)]
pub struct Shared {
    pub snapshot: RwLock<Option<Arc<Clients>>>,
    pub dhcp_ready: AtomicBool,
    pub requests: AtomicU64,
    pub responses: AtomicU64,
    pub errors: AtomicU64,
    pub unknown: AtomicU64,
    pub boot_mode_mismatches: AtomicU64,
    sync_success: AtomicU64,
    sync_errors: AtomicU64,
    state_read_errors: AtomicU64,
    state_write_errors: AtomicU64,
    status: Mutex<SyncStatus>,
    request_labels: Mutex<RequestMetrics>,
    response_labels: Mutex<BTreeMap<String, u64>>,
    durations: Mutex<(u64, f64)>,
}

impl Shared {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_request(
        &self,
        location: &str,
        hostname: &str,
        stage: &str,
        route: &str,
        message: &str,
    ) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        let key = (
            location.into(),
            hostname.into(),
            stage.into(),
            route.into(),
            message.into(),
        );
        let now = Instant::now();
        let mut metrics = self.request_labels.lock().unwrap();
        metrics.prune(now);
        let entry = metrics.series.entry(key).or_insert((0, now));
        entry.0 += 1;
        entry.1 = now;
    }

    pub fn record_response(&self, message: &str) {
        self.responses.fetch_add(1, Ordering::Relaxed);
        *self
            .response_labels
            .lock()
            .unwrap()
            .entry(message.into())
            .or_default() += 1;
    }

    pub fn record_duration(&self, seconds: f64) {
        let mut durations = self.durations.lock().unwrap();
        durations.0 += 1;
        durations.1 += seconds;
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Load a saved snapshot before attempting the first remote request.
pub async fn run_sync(config: Arc<Config>, shared: Arc<Shared>) {
    let path = &config.clients_source.state_file;
    match tokio::fs::read(path).await {
        Ok(bytes) => {
            let result = (|| -> Result<Clients> {
                if bytes.len() > MAX_CSV_BYTES {
                    bail!("saved CSV exceeds size limit");
                }
                Clients::parse(std::str::from_utf8(&bytes)?)
            })();
            match result {
                Ok(clients) => {
                    *shared.snapshot.write().unwrap() = Some(Arc::new(clients));
                    let mut status = shared.status.lock().unwrap();
                    status.source = Some("state");
                    // A cached file's mtime is not proof of a successful remote poll.
                    tracing::info!(path = %path.display(), "loaded saved client snapshot");
                }
                Err(error) => state_read_error(&shared, &error),
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(error) => state_read_error(&shared, &error.into()),
    }
    let client = match polling_client(config.clients_source.timeout_seconds) {
        Ok(client) => client,
        Err(error) => {
            shared.status.lock().unwrap().last_error = Some(error.to_string());
            tracing::error!(%error, "cannot construct polling HTTP client");
            return;
        }
    };
    let mut cache = RemoteCache::default();
    loop {
        match poll(&client, &config, &shared, &mut cache).await {
            Ok(()) => {
                shared.sync_success.fetch_add(1, Ordering::Relaxed);
                let mut status = shared.status.lock().unwrap();
                status.last_success = Some(now());
                status.last_error = None;
            }
            Err(error) => {
                shared.sync_errors.fetch_add(1, Ordering::Relaxed);
                shared.status.lock().unwrap().last_error = Some(format!("{error:#}"));
                tracing::warn!(error = %format!("{error:#}"), "client synchronization failed; retaining snapshot");
            }
        }
        tokio::time::sleep(Duration::from_secs(
            config.clients_source.poll_interval_seconds,
        ))
        .await;
    }
}

fn polling_client(timeout_seconds: u64) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_seconds))
        .user_agent(SERVER_HEADER)
        .build()
        .context("build polling HTTP client")
}

fn state_read_error(shared: &Shared, error: &anyhow::Error) {
    shared.state_read_errors.fetch_add(1, Ordering::Relaxed);
    shared.status.lock().unwrap().last_error = Some(error.to_string());
    tracing::warn!(%error, "cannot load saved client snapshot");
}

#[derive(Default)]
struct RemoteCache {
    etag: Option<reqwest::header::HeaderValue>,
    modified: Option<reqwest::header::HeaderValue>,
    body: Option<Vec<u8>>,
}

async fn poll(
    client: &reqwest::Client,
    config: &Config,
    shared: &Shared,
    cache: &mut RemoteCache,
) -> Result<()> {
    let mut request = client.get(&config.clients_source.url);
    if let Some(value) = &cache.etag {
        request = request.header(IF_NONE_MATCH, value);
    }
    if let Some(value) = &cache.modified {
        request = request.header(IF_MODIFIED_SINCE, value);
    }
    let mut response = request.send().await.context("fetch CSV")?;
    if response.status() == reqwest::StatusCode::NOT_MODIFIED {
        if cache.body.is_none() {
            bail!("304 received without a previously downloaded snapshot");
        }
        return Ok(());
    }
    response.error_for_status_ref()?;
    if response.status() != reqwest::StatusCode::OK {
        bail!(
            "expected HTTP 200 or cached 304, received {}",
            response.status()
        );
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_CSV_BYTES as u64)
    {
        bail!("CSV exceeds size limit");
    }
    let etag = response.headers().get(ETAG).cloned();
    let modified = response.headers().get(LAST_MODIFIED).cloned();
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len() + chunk.len() > MAX_CSV_BYTES {
            bail!("CSV exceeds size limit");
        }
        bytes.extend_from_slice(&chunk);
    }
    if cache.body.as_ref() != Some(&bytes) {
        let clients = Clients::parse(std::str::from_utf8(&bytes).context("CSV is not UTF-8")?)?;
        if let Err(error) = persist(&config.clients_source.state_file, &bytes).await {
            shared.state_write_errors.fetch_add(1, Ordering::Relaxed);
            return Err(error.context("save client snapshot"));
        }
        let count = clients.records.len();
        *shared.snapshot.write().unwrap() = Some(Arc::new(clients));
        let mut status = shared.status.lock().unwrap();
        status.source = Some("remote");
        status.data_since = Some(now());
        tracing::info!(clients = count, "activated client snapshot");
    }
    cache.body = Some(bytes);
    cache.etag = etag;
    cache.modified = modified;
    Ok(())
}

async fn persist(path: &Path, bytes: &[u8]) -> Result<()> {
    let path = path.to_owned();
    let bytes = bytes.to_owned();
    // Keep the transaction in one blocking job. Once started, cancellation of
    // the polling future cannot interrupt it between write, rename and fsync;
    // normal Tokio runtime shutdown waits for this job to finish.
    tokio::task::spawn_blocking(move || persist_blocking(&path, &bytes))
        .await
        .context("snapshot persistence worker failed")?
}

fn persist_blocking(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let name = path
        .file_name()
        .context("state path needs a filename")?
        .to_string_lossy();
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let temp = parent.join(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let result: Result<()> = (|| {
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp, path)?;
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

async fn health(State(shared): State<Arc<Shared>>) -> impl IntoResponse {
    let count = shared
        .snapshot
        .read()
        .unwrap()
        .as_ref()
        .map(|clients| clients.records.len());
    let healthy = count.is_some() && shared.dhcp_ready.load(Ordering::Relaxed);
    let status = shared.status.lock().unwrap();
    (
        if healthy {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        },
        [
            (HeaderName::from_static("server"), SERVER_HEADER),
            (HeaderName::from_static("x-app-version"), APP_VERSION),
        ],
        Json(serde_json::json!({
            "healthy": healthy, "source": status.source, "clients": count.unwrap_or(0),
            "last_successful_sync": status.last_success,
            "data_age_seconds": status.data_since.map(|timestamp| now().saturating_sub(timestamp)),
            "last_sync_error": status.last_error,
        })),
    )
}

fn escape_label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('"', "\\\"")
}

async fn metrics(State(shared): State<Arc<Shared>>) -> impl IntoResponse {
    use std::fmt::Write;
    let mut text = String::new();
    let _ = writeln!(
        text,
        "# TYPE macdack_build_info gauge\nmacdack_build_info{{version=\"{}\"}} 1",
        escape_label(APP_VERSION)
    );
    for (name, counter) in [
        ("macdack_requests_total", &shared.requests),
        ("macdack_responses_total", &shared.responses),
        ("macdack_errors_total", &shared.errors),
        ("macdack_unknown_clients_total", &shared.unknown),
        (
            "macdack_boot_mode_mismatches_total",
            &shared.boot_mode_mismatches,
        ),
        ("macdack_sync_success_total", &shared.sync_success),
        ("macdack_sync_errors_total", &shared.sync_errors),
        ("macdack_state_read_errors_total", &shared.state_read_errors),
        (
            "macdack_state_write_errors_total",
            &shared.state_write_errors,
        ),
    ] {
        let _ = writeln!(
            text,
            "# TYPE {name} counter\n{name} {}",
            counter.load(Ordering::Relaxed)
        );
    }
    let count = shared
        .snapshot
        .read()
        .unwrap()
        .as_ref()
        .map_or(0, |clients| clients.records.len());
    let _ = writeln!(
        text,
        "# TYPE macdack_clients gauge\nmacdack_clients {count}"
    );
    let status = shared.status.lock().unwrap();
    let _ = writeln!(
        text,
        "# TYPE macdack_last_successful_sync_timestamp_seconds gauge\nmacdack_last_successful_sync_timestamp_seconds {}",
        status.last_success.unwrap_or(0)
    );
    let _ = writeln!(
        text,
        "# TYPE macdack_data_age_seconds gauge\nmacdack_data_age_seconds {}",
        status
            .data_since
            .map_or(f64::NAN, |t| now().saturating_sub(t) as f64)
    );
    drop(status);
    text.push_str("# TYPE macdack_client_requests_total counter\n");
    let request_series = {
        let mut metrics = shared.request_labels.lock().unwrap();
        metrics.prune(Instant::now());
        metrics.series.clone()
    };
    for ((location, hostname, stage, route, message), (count, _)) in &request_series {
        let _ = writeln!(
            text,
            "macdack_client_requests_total{{location=\"{}\",hostname=\"{}\",stage=\"{}\",route=\"{}\",message=\"{}\"}} {count}",
            escape_label(location),
            escape_label(hostname),
            escape_label(stage),
            escape_label(route),
            escape_label(message)
        );
    }
    text.push_str("# TYPE macdack_message_responses_total counter\n");
    for (message, count) in shared.response_labels.lock().unwrap().iter() {
        let _ = writeln!(
            text,
            "macdack_message_responses_total{{message=\"{}\"}} {count}",
            escape_label(message)
        );
    }
    let durations = shared.durations.lock().unwrap();
    let _ = writeln!(
        text,
        "# TYPE macdack_response_duration_seconds summary\nmacdack_response_duration_seconds_count {}\nmacdack_response_duration_seconds_sum {}",
        durations.0, durations.1
    );
    (
        [
            (CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8"),
            (HeaderName::from_static("server"), SERVER_HEADER),
            (HeaderName::from_static("x-app-version"), APP_VERSION),
        ],
        text,
    )
}

pub async fn serve_http(config: Arc<Config>, shared: Arc<Shared>) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(config.http.listen)
        .await
        .context("bind HTTP listener")?;
    let app = Router::new()
        .route("/health", get(health))
        .route("/metrics", get(metrics))
        .with_state(shared);
    axum::serve(listener, app).await.context("serve HTTP")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn health_requires_dhcp_and_snapshot_but_tolerates_poll_failure() {
        let shared = Arc::new(Shared::new());
        assert_eq!(
            health(State(shared.clone())).await.into_response().status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        shared.dhcp_ready.store(true, Ordering::Relaxed);
        assert_eq!(
            health(State(shared.clone())).await.into_response().status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        *shared.snapshot.write().unwrap() = Some(Arc::new(Clients::default()));
        {
            let mut status = shared.status.lock().unwrap();
            status.source = Some("state");
            status.last_error = Some("remote unavailable".into());
        }
        let response = health(State(shared.clone())).await.into_response();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers().get("server").unwrap(), SERVER_HEADER);
        assert_eq!(
            response.headers().get("x-app-version").unwrap(),
            APP_VERSION
        );
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["source"], "state");
        assert!(json["last_successful_sync"].is_null());
        assert!(json["data_age_seconds"].is_null());
        assert_eq!(json["last_sync_error"], "remote unavailable");
        shared.dhcp_ready.store(false, Ordering::Relaxed);
        assert_eq!(
            health(State(shared)).await.into_response().status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[tokio::test]
    async fn invalid_remote_update_preserves_snapshot_and_saved_state() {
        let body = Arc::new(Mutex::new((
            StatusCode::OK,
            String::from(
                "dc1,host.example,example.internal,uefi,1500,AA:BB:CC:DD:EE:02,10.0.0.2,24,10.0.0.1\n",
            ),
        )));
        let response_body = body.clone();
        let user_agent = Arc::new(Mutex::new(None));
        let observed_user_agent = user_agent.clone();
        let app = Router::new().route(
            "/clients.csv",
            get(move |headers: axum::http::HeaderMap| {
                let body = response_body.lock().unwrap().clone();
                *observed_user_agent.lock().unwrap() = headers
                    .get(reqwest::header::USER_AGENT)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned);
                async move { body }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut config: Config = toml::from_str(include_str!("../examples/server.toml")).unwrap();
        let dir = std::env::temp_dir().join(format!(
            "macdack-poll-{}-{}",
            std::process::id(),
            address.port()
        ));
        config.clients_source.state_file = dir.join("clients.csv");
        config.clients_source.url = format!("http://{address}/clients.csv");
        let shared = Shared::new();
        let client = polling_client(config.clients_source.timeout_seconds).unwrap();
        let mut cache = RemoteCache::default();
        poll(&client, &config, &shared, &mut cache).await.unwrap();
        assert_eq!(user_agent.lock().unwrap().as_deref(), Some(SERVER_HEADER));
        let snapshot = shared.snapshot.read().unwrap().clone().unwrap();
        let saved = tokio::fs::read(&config.clients_source.state_file)
            .await
            .unwrap();
        for (status, text) in [
            (StatusCode::OK, "broken CSV"),
            (StatusCode::NO_CONTENT, ""),
            (
                StatusCode::PARTIAL_CONTENT,
                std::str::from_utf8(&saved).unwrap(),
            ),
            (StatusCode::FOUND, ""),
        ] {
            *body.lock().unwrap() = (status, text.into());
            assert!(
                poll(&client, &config, &shared, &mut cache).await.is_err(),
                "{status}"
            );
            assert!(Arc::ptr_eq(
                &snapshot,
                shared.snapshot.read().unwrap().as_ref().unwrap()
            ));
            assert_eq!(
                tokio::fs::read(&config.clients_source.state_file)
                    .await
                    .unwrap(),
                saved
            );
        }
        *body.lock().unwrap() = (StatusCode::NOT_MODIFIED, String::new());
        poll(&client, &config, &shared, &mut cache).await.unwrap();
        server.abort();
        tokio::fs::remove_file(&config.clients_source.state_file)
            .await
            .unwrap();
        tokio::fs::remove_dir(dir).await.unwrap();
    }

    #[tokio::test]
    async fn persistence_replaces_complete_file() {
        let dir =
            std::env::temp_dir().join(format!("macdack-runtime-{}-{}", std::process::id(), now()));
        let path = dir.join("clients.csv");
        persist(&path, b"old").await.unwrap();
        persist(&path, b"new complete contents").await.unwrap();
        assert_eq!(
            tokio::fs::read(&path).await.unwrap(),
            b"new complete contents"
        );
        tokio::fs::remove_file(path).await.unwrap();
        tokio::fs::remove_dir(dir).await.unwrap();
    }

    #[test]
    fn idle_client_metrics_expire_without_resetting_global_counters() {
        let shared = Shared::new();
        shared.record_request("old", "retired", "os", "classless", "discover");
        shared.record_request("current", "active", "os", "classless", "discover");
        let mut metrics = shared.request_labels.lock().unwrap();
        let now = Instant::now();
        for (labels, (_, seen)) in &mut metrics.series {
            if labels.0 == "old" {
                *seen = now - METRIC_IDLE_TTL;
            }
        }
        metrics.last_pruned = None;
        metrics.prune(now);
        assert_eq!(metrics.series.len(), 1);
        assert_eq!(metrics.series.keys().next().unwrap().0, "current");
        assert_eq!(shared.requests.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn prometheus_labels_are_escaped() {
        assert_eq!(escape_label("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
    }

    #[tokio::test]
    async fn metrics_include_version_and_headers() {
        let shared = Arc::new(Shared::new());
        shared.record_request("dc\"1", "host\"x", "os\\stage", "121\nonly", "discover");
        let response = metrics(State(shared)).await.into_response();
        assert_eq!(response.headers().get("server").unwrap(), SERVER_HEADER);
        assert_eq!(
            response.headers().get("x-app-version").unwrap(),
            APP_VERSION
        );
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(text.contains(concat!(
            "macdack_build_info{version=\"",
            env!("CARGO_PKG_VERSION"),
            "\"} 1"
        )));
        assert!(text.contains(
            "location=\"dc\\\"1\",hostname=\"host\\\"x\",stage=\"os\\\\stage\",route=\"121\\nonly\",message=\"discover\""
        ));
    }
}
