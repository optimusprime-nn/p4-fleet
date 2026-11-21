//! Talking to the fleet agent on a device.

use crate::db::Device;
use crate::App;
use anyhow::{anyhow, bail, Result};
use serde_json::Value;
use std::time::Duration;

pub fn base_url(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("http://[{host}]:{port}") // IPv6 literal
    } else {
        format!("http://{host}:{port}")
    }
}

pub fn display_name(d: &Device) -> String {
    if let Some(n) = d.name.as_deref().filter(|n| !n.is_empty()) {
        return n.to_string();
    }
    let label = d.info.as_ref().and_then(|i| i.get("label")).and_then(Value::as_str);
    match label {
        Some(l) => format!("{l} ({})", &d.id[d.id.len().saturating_sub(6)..]),
        None => d.id.clone(),
    }
}

pub async fn fetch_info(app: &App, host: &str, port: u16) -> Result<Value> {
    let url = format!("{}/fleet/info", base_url(host, port));
    let r = app.http.get(&url).timeout(Duration::from_secs(8)).send().await?;
    if !r.status().is_success() {
        bail!("{} from /fleet/info", r.status());
    }
    let v: Value = r.json().await?;
    if v.get("id").and_then(Value::as_str).unwrap_or("").is_empty() {
        bail!("/fleet/info has no device id (not a fleet agent?)");
    }
    Ok(v)
}

fn token(app: &App) -> Result<&str> {
    app.cfg
        .device_token
        .as_deref()
        .ok_or_else(|| anyhow!("FLEET_DEVICE_TOKEN is not set on the server"))
}

async fn check(r: reqwest::Response) -> Result<String> {
    let status = r.status();
    let text = r.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("device answered {status}: {}", text.trim());
    }
    Ok(text.trim().to_string())
}

/// reboot | revert | identify
pub async fn action(app: &App, d: &Device, what: &str) -> Result<String> {
    let url = format!("{}/fleet/{what}", base_url(&d.host, d.port));
    let r = app
        .http
        .post(&url)
        .bearer_auth(token(app)?)
        .timeout(Duration::from_secs(10))
        .send()
        .await?;
    check(r).await
}

pub async fn push_firmware(app: &App, d: &Device, image: Vec<u8>) -> Result<String> {
    let url = format!("{}/fleet/ota", base_url(&d.host, d.port));
    let r = app
        .http
        .post(&url)
        .bearer_auth(token(app)?)
        .header("Content-Type", "application/octet-stream")
        .body(image)
        .timeout(Duration::from_secs(300))
        .send()
        .await?;
    check(r).await
}
