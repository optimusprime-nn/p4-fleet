//! p4fleet - fleet manager for ESP32-P4 devices running the fleet agent.

mod api;
mod db;
mod device;
mod discovery;
mod firmware;
mod ota;
mod poller;

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub struct Config {
    pub listen: String,
    pub data_dir: PathBuf,
    /// Shared secret the devices expect for OTA/reboot/revert/identify.
    pub device_token: Option<String>,
    /// Protects the web UI and API. None = open (trusted LAN only).
    pub admin_token: Option<String>,
    pub poll_s: u64,
    pub retention_days: i64,
    pub webhook: Option<String>,
    pub mdns: bool,
    /// How long a device may take to reboot into new firmware and confirm it.
    pub ota_timeout_s: u64,
}

fn env_opt(k: &str) -> Option<String> {
    std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn env_num<T: std::str::FromStr>(k: &str, default: T) -> T {
    env_opt(k).and_then(|v| v.parse().ok()).unwrap_or(default)
}

impl Config {
    fn from_env() -> Self {
        Config {
            listen: env_opt("FLEET_LISTEN").unwrap_or_else(|| "0.0.0.0:8790".into()),
            data_dir: env_opt("FLEET_DATA_DIR").unwrap_or_else(|| "./data".into()).into(),
            device_token: env_opt("FLEET_DEVICE_TOKEN"),
            admin_token: env_opt("FLEET_ADMIN_TOKEN"),
            poll_s: env_num("FLEET_POLL_S", 30u64).clamp(5, 3600),
            retention_days: env_num("FLEET_RETENTION_DAYS", 14i64).clamp(1, 3650),
            webhook: env_opt("FLEET_WEBHOOK_URL"),
            mdns: env_opt("FLEET_MDNS").map(|v| v != "0" && v != "false").unwrap_or(true),
            ota_timeout_s: env_num("FLEET_OTA_TIMEOUT_S", 600u64).clamp(60, 3600),
        }
    }
}

pub struct App {
    pub cfg: Config,
    pub db: Mutex<rusqlite::Connection>,
    pub http: reqwest::Client,
}

pub type Shared = Arc<App>;

impl App {
    pub fn firmware_dir(&self) -> PathBuf {
        self.cfg.data_dir.join("firmware")
    }

    /// Lock the database. A poisoned lock only means another task panicked
    /// mid-query; SQLite itself is still consistent.
    pub fn db(&self) -> std::sync::MutexGuard<'_, rusqlite::Connection> {
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Record an event, log it and forward it to the webhook.
    pub fn event(&self, device_id: Option<&str>, severity: &str, kind: &str, message: &str) {
        let ev = match db::add_event(&self.db(), device_id, severity, kind, message) {
            Ok(ev) => ev,
            Err(e) => {
                tracing::error!("could not store event: {e:#}");
                return;
            }
        };
        match severity {
            "error" => tracing::error!(device = device_id, kind, "{message}"),
            "warn" => tracing::warn!(device = device_id, kind, "{message}"),
            _ => tracing::info!(device = device_id, kind, "{message}"),
        }
        if let Some(url) = self.cfg.webhook.clone() {
            let http = self.http.clone();
            let name = device_id.and_then(|id| db::device(&self.db(), id).ok().flatten()).map(|d| device::display_name(&d));
            tokio::spawn(async move {
                let body = serde_json::json!({ "event": ev, "device_name": name });
                if let Err(e) = http.post(&url).json(&body).send().await.and_then(|r| r.error_for_status()) {
                    tracing::warn!("webhook failed: {e}");
                }
            });
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "p4fleet=info,warn".into()),
        )
        .init();

    let cfg = Config::from_env();
    std::fs::create_dir_all(cfg.data_dir.join("firmware"))
        .with_context(|| format!("creating {}", cfg.data_dir.display()))?;
    let conn = db::open(&cfg.data_dir.join("fleet.db"))?;
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .user_agent(concat!("p4fleet/", env!("CARGO_PKG_VERSION")))
        .build()?;

    if cfg.device_token.is_none() {
        tracing::warn!("FLEET_DEVICE_TOKEN not set: monitoring only (no OTA, reboot, revert or identify)");
    }
    if cfg.admin_token.is_none() {
        tracing::warn!("FLEET_ADMIN_TOKEN not set: the web UI and API are open to anyone on the network");
    }

    let app: Shared = Arc::new(App { cfg, db: Mutex::new(conn), http });

    if app.cfg.mdns {
        discovery::start(app.clone());
    }
    tokio::spawn(poller::run(app.clone()));

    let listener = tokio::net::TcpListener::bind(&app.cfg.listen)
        .await
        .with_context(|| format!("binding {}", app.cfg.listen))?;
    tracing::info!("p4fleet listening on http://{}", app.cfg.listen);
    axum::serve(listener, api::router(app))
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

/// Ctrl-C, or SIGTERM from `docker stop`.
async fn shutdown() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("installing SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
