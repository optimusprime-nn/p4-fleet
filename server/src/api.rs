//! HTTP API, Prometheus metrics and the embedded web UI.

use crate::db::{self, Device, Job};
use crate::{device, firmware, ota, Shared};
use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Query, Request, State},
    http::{header, HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const INDEX: &str = include_str!("../ui/index.html");

pub fn router(app: Shared) -> Router {
    let api = Router::new()
        .route("/config", get(config))
        .route("/devices", get(list_devices).post(add_device))
        .route("/devices/{id}", get(get_device).patch(patch_device).delete(delete_device))
        .route("/devices/{id}/ota", post(device_ota))
        .route("/devices/{id}/{action}", post(device_action))
        .route("/firmware", get(list_firmware).post(upload_firmware))
        .route("/firmware/{id}", axum::routing::delete(delete_firmware))
        .route("/firmware/{id}/rollout", post(rollout))
        .route("/rollouts", get(list_rollouts))
        .route("/events", get(list_events))
        .layer(DefaultBodyLimit::max(32 * 1024 * 1024))
        .layer(middleware::from_fn_with_state(app.clone(), auth));

    Router::new()
        .route("/", get(|| async { Html(INDEX) }))
        .route("/metrics", get(metrics))
        .nest("/api", api)
        .with_state(app)
}

// ---------------------------------------------------------------------------
// Errors and auth

pub struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        tracing::error!("{e:#}");
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
    }
}

fn bad(msg: impl std::fmt::Display) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, msg.to_string())
}

fn not_found(what: &str) -> ApiError {
    ApiError(StatusCode::NOT_FOUND, format!("{what} not found"))
}

type ApiResult<T> = Result<T, ApiError>;

fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut diff = (a.len() != b.len()) as u8;
    for (i, x) in a.iter().enumerate() {
        diff |= x ^ b.get(i).copied().unwrap_or(0);
    }
    diff == 0
}

async fn auth(State(app): State<Shared>, req: Request, next: Next) -> Response {
    if let Some(tok) = &app.cfg.admin_token {
        let ok = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(|t| ct_eq(t, tok))
            .unwrap_or(false);
        if !ok {
            return ApiError(StatusCode::UNAUTHORIZED, "admin token required".into()).into_response();
        }
    }
    next.run(req).await
}

// ---------------------------------------------------------------------------

async fn config(State(app): State<Shared>) -> Json<Value> {
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "device_token_set": app.cfg.device_token.is_some(),
        "mdns": app.cfg.mdns,
        "poll_s": app.cfg.poll_s,
        "ota_timeout_s": app.cfg.ota_timeout_s,
        "webhook": app.cfg.webhook.is_some(),
    }))
}

#[derive(Serialize)]
struct DeviceView {
    #[serde(flatten)]
    device: Device,
    display_name: String,
    job: Option<Job>,
}

fn view(app: &Shared, d: Device) -> DeviceView {
    let job = db::active_job_for(&app.db(), &d.id).ok().flatten();
    DeviceView { display_name: device::display_name(&d), device: d, job }
}

async fn list_devices(State(app): State<Shared>) -> ApiResult<Json<Vec<DeviceView>>> {
    let list = db::devices(&app.db())?;
    Ok(Json(list.into_iter().map(|d| view(&app, d)).collect()))
}

async fn get_device(State(app): State<Shared>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let d = db::device(&app.db(), &id)?.ok_or_else(|| not_found("device"))?;
    let samples = db::samples(&app.db(), &id, db::now() - 86400)?;
    let events = db::events(&app.db(), Some(&id), 50)?;
    let jobs: Vec<Job> = db::jobs(&app.db(), None, 200)?.into_iter().filter(|j| j.device_id == id).take(20).collect();
    Ok(Json(json!({ "device": view(&app, d), "samples": samples, "events": events, "jobs": jobs })))
}

#[derive(Deserialize)]
struct AddDevice {
    host: String,
    port: Option<u16>,
}

/// Accepts "10.0.0.5", "10.0.0.5:8080", "display.lan" or "http://display.lan/".
fn parse_host(input: &str, port: Option<u16>) -> Option<(String, u16)> {
    let s = input.trim().trim_start_matches("http://").trim_end_matches('/');
    if s.is_empty() || s.contains('/') || s.contains(char::is_whitespace) {
        return None;
    }
    if let Some(v6) = s.strip_prefix('[') {
        let (h, rest) = v6.split_once(']')?;
        let p = rest.strip_prefix(':').and_then(|p| p.parse().ok());
        return Some((h.to_string(), port.or(p).unwrap_or(80)));
    }
    match s.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => Some((h.to_string(), port.or(p.parse().ok()).unwrap_or(80))),
        Some(_) => Some((s.to_string(), port.unwrap_or(80))), // bare IPv6
        None => Some((s.to_string(), port.unwrap_or(80))),
    }
}

async fn add_device(State(app): State<Shared>, Json(req): Json<AddDevice>) -> ApiResult<Json<DeviceView>> {
    let (host, port) = parse_host(&req.host, req.port).ok_or_else(|| bad("enter a host name or IP address"))?;
    let info = device::fetch_info(&app, &host, port)
        .await
        .map_err(|e| bad(format!("no fleet agent at {host}:{port}: {e:#}")))?;
    let id = info["id"].as_str().unwrap_or_default().to_lowercase();
    let new = db::upsert_device(&app.db(), &id, &host, port, "manual")?;
    if new {
        let label = info["label"].as_str().unwrap_or("device");
        app.event(Some(&id), "info", "added", &format!("{label} added at {host}:{port}"));
    }
    crate::poller::poll_one(&app, &id).await;
    let d = db::device(&app.db(), &id)?.ok_or_else(|| not_found("device"))?;
    Ok(Json(view(&app, d)))
}

#[derive(Deserialize)]
struct PatchDevice {
    name: Option<String>,
}

async fn patch_device(State(app): State<Shared>, Path(id): Path<String>, Json(req): Json<PatchDevice>) -> ApiResult<StatusCode> {
    let name = req.name.as_deref().map(str::trim).filter(|n| !n.is_empty());
    if !db::set_device_name(&app.db(), &id, name)? {
        return Err(not_found("device"));
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_device(State(app): State<Shared>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    if !db::delete_device(&app.db(), &id)? {
        return Err(not_found("device"));
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn device_action(State(app): State<Shared>, Path((id, action)): Path<(String, String)>) -> ApiResult<Json<Value>> {
    if !matches!(action.as_str(), "reboot" | "revert" | "identify") {
        return Err(not_found("action"));
    }
    let d = db::device(&app.db(), &id)?.ok_or_else(|| not_found("device"))?;
    let cap = d.info.as_ref().and_then(|i| i.pointer(&format!("/caps/{action}"))).and_then(Value::as_bool);
    if cap != Some(true) {
        return Err(bad(format!("device does not allow {action} (no fleet token on the device?)")));
    }
    let msg = device::action(&app, &d, &action).await.map_err(|e| bad(format!("{e:#}")))?;
    if action != "identify" {
        app.event(Some(&id), "info", &action, &format!("{}: {action} requested", device::display_name(&d)));
    }
    Ok(Json(json!({ "message": msg })))
}

#[derive(Deserialize)]
struct OtaReq {
    firmware_id: i64,
}

async fn device_ota(State(app): State<Shared>, Path(id): Path<String>, Json(req): Json<OtaReq>) -> ApiResult<Json<Value>> {
    let job = ota::start_single(&app, &id, req.firmware_id).map_err(|e| bad(format!("{e:#}")))?;
    Ok(Json(json!({ "job_id": job })))
}

// ---------------------------------------------------------------------------
// Firmware

async fn list_firmware(State(app): State<Shared>) -> ApiResult<Json<Vec<db::Firmware>>> {
    Ok(Json(db::firmwares(&app.db())?))
}

#[derive(Deserialize)]
struct UploadQuery {
    filename: Option<String>,
}

async fn upload_firmware(State(app): State<Shared>, Query(q): Query<UploadQuery>, body: Bytes) -> ApiResult<Json<Value>> {
    let img = firmware::parse(&body).map_err(bad)?;
    if let Some(existing) = db::firmware_by_sha(&app.db(), &img.sha256)? {
        return Ok(Json(json!({ "firmware": existing, "duplicate": true })));
    }
    let chip = firmware::chip_name(img.chip_id);
    let filename: String = q
        .filename
        .unwrap_or_else(|| format!("{}.bin", img.project))
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || "._-".contains(*c))
        .take(80)
        .collect();
    let dir = app.firmware_dir();
    let tmp = dir.join(format!("{}.part", img.sha256));
    let dst = dir.join(format!("{}.bin", img.sha256));
    tokio::fs::write(&tmp, &body).await.map_err(anyhow::Error::from)?;
    tokio::fs::rename(&tmp, &dst).await.map_err(anyhow::Error::from)?;
    let mut fw = db::Firmware {
        id: 0,
        project: img.project,
        version: img.version,
        idf: img.idf,
        build: img.build,
        elf_sha256: img.elf_sha256,
        sha256: img.sha256,
        size: body.len() as i64,
        filename,
        uploaded: db::now(),
    };
    fw.id = db::add_firmware(&app.db(), &fw)?;
    app.event(None, "info", "firmware", &format!("firmware uploaded: {} {} ({chip}, {} KB)", fw.project, fw.version, fw.size / 1024));
    Ok(Json(json!({ "firmware": fw, "chip": chip, "duplicate": false })))
}

async fn delete_firmware(State(app): State<Shared>, Path(id): Path<i64>) -> ApiResult<StatusCode> {
    let fw = db::firmware(&app.db(), id)?.ok_or_else(|| not_found("firmware"))?;
    let in_use = db::jobs(&app.db(), None, 500)?
        .iter()
        .any(|j| j.firmware_id == id && ["queued", "pushing", "rebooting", "verifying"].contains(&j.state.as_str()));
    if in_use {
        return Err(bad("an update with this firmware is in progress"));
    }
    db::delete_firmware(&app.db(), id)?;
    let _ = tokio::fs::remove_file(ota::firmware_path(&app, &fw)).await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct RolloutReq {
    device_ids: Option<Vec<String>>,
    stop_on_failure: Option<bool>,
}

async fn rollout(State(app): State<Shared>, Path(id): Path<i64>, Json(req): Json<RolloutReq>) -> ApiResult<Json<Value>> {
    let plan = ota::start_rollout(&app, id, req.device_ids, req.stop_on_failure.unwrap_or(true))
        .map_err(|e| bad(format!("{e:#}")))?;
    let skipped: Vec<Value> = plan.skipped.iter().map(|(d, w)| json!({ "device_id": d, "reason": w })).collect();
    Ok(Json(json!({ "rollout_id": plan.rollout_id, "targets": plan.targets, "skipped": skipped })))
}

async fn list_rollouts(State(app): State<Shared>) -> ApiResult<Json<Value>> {
    let rollouts = db::rollouts(&app.db(), 20)?;
    let jobs = db::jobs(&app.db(), None, 200)?;
    Ok(Json(json!({ "rollouts": rollouts, "jobs": jobs })))
}

#[derive(Deserialize)]
struct EventsQuery {
    device: Option<String>,
    limit: Option<i64>,
}

async fn list_events(State(app): State<Shared>, Query(q): Query<EventsQuery>) -> ApiResult<Json<Vec<db::Event>>> {
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    Ok(Json(db::events(&app.db(), q.device.as_deref(), limit)?))
}

// ---------------------------------------------------------------------------
// Prometheus

fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

fn metric_key(k: &str) -> String {
    k.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' }).collect()
}

async fn metrics(State(app): State<Shared>, headers: HeaderMap) -> ApiResult<Response> {
    if let Some(tok) = &app.cfg.admin_token {
        let ok = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(|t| ct_eq(t, tok))
            .unwrap_or(false);
        if !ok {
            return Err(ApiError(StatusCode::UNAUTHORIZED, "admin token required".into()));
        }
    }
    let devices = db::devices(&app.db())?;
    let mut out = String::new();
    let gauges: &[(&str, &str, &str)] = &[
        ("p4fleet_uptime_seconds", "/uptime_s", "Seconds since boot"),
        ("p4fleet_wifi_rssi_dbm", "/rssi", "Wi-Fi signal strength"),
        ("p4fleet_heap_internal_free_bytes", "/heap/int", "Free internal RAM"),
        ("p4fleet_heap_internal_min_free_bytes", "/heap/int_min", "Lowest free internal RAM since boot"),
        ("p4fleet_heap_psram_free_bytes", "/heap/psram", "Free PSRAM"),
    ];
    out.push_str("# HELP p4fleet_device_online 1 if the device answered recently\n# TYPE p4fleet_device_online gauge\n");
    for d in &devices {
        let info = d.info.clone().unwrap_or(Value::Null);
        let s = |k: &str| info.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        out.push_str(&format!(
            "p4fleet_device_online{{id=\"{}\",name=\"{}\",project=\"{}\",version=\"{}\"}} {}\n",
            d.id,
            esc(&device::display_name(d)),
            esc(&s("project")),
            esc(&s("version")),
            d.online as u8
        ));
    }
    for (name, ptr, help) in gauges {
        out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} gauge\n"));
        for d in devices.iter().filter(|d| d.online) {
            if let Some(v) = d.info.as_ref().and_then(|i| i.pointer(ptr)).and_then(Value::as_f64) {
                out.push_str(&format!("{name}{{id=\"{}\",name=\"{}\"}} {v}\n", d.id, esc(&device::display_name(d))));
            }
        }
    }
    out.push_str("# HELP p4fleet_app_value Numeric values the app reports in info.app\n# TYPE p4fleet_app_value gauge\n");
    for d in devices.iter().filter(|d| d.online) {
        let Some(Value::Object(m)) = d.info.as_ref().and_then(|i| i.get("app")) else { continue };
        for (k, v) in m {
            let v = match v {
                Value::Number(n) => n.as_f64(),
                Value::Bool(b) => Some(*b as u8 as f64),
                _ => None,
            };
            if let Some(v) = v {
                out.push_str(&format!(
                    "p4fleet_app_value{{id=\"{}\",name=\"{}\",key=\"{}\"}} {v}\n",
                    d.id,
                    esc(&device::display_name(d)),
                    metric_key(k)
                ));
            }
        }
    }
    Ok(([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], out).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_parsing() {
        assert_eq!(parse_host("10.0.0.5", None), Some(("10.0.0.5".into(), 80)));
        assert_eq!(parse_host("10.0.0.5:8080", None), Some(("10.0.0.5".into(), 8080)));
        assert_eq!(parse_host("http://display.lan/", None), Some(("display.lan".into(), 80)));
        assert_eq!(parse_host("[fe80::1]:81", None), Some(("fe80::1".into(), 81)));
        assert_eq!(parse_host("  ", None), None);
        assert_eq!(parse_host("a/b", None), None);
    }

    #[test]
    fn constant_time_eq() {
        assert!(ct_eq("secret", "secret"));
        assert!(!ct_eq("secret", "secreT"));
        assert!(!ct_eq("secret", "secret2"));
        assert!(!ct_eq("", "x"));
    }
}
