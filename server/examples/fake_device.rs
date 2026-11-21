//! A pretend P4 device that speaks the fleet agent API, for testing the server
//! without hardware.
//!
//!   cargo run --example fake_device -- serve <port> <id> <project> <version> [ok|rollback]
//!   cargo run --example fake_device -- mkimage <out.bin> <project> <version>
//!
//! `serve` announces itself over mDNS, answers /fleet/info, and on /fleet/ota
//! "reboots": it goes silent for a few seconds, then runs the new image
//! (pending verify for 8 s, then confirmed). With `rollback` the new image
//! "crashes" and the device comes back on the old one, like the bootloader
//! rollback. The fleet token is taken from FAKE_TOKEN (default "test").

use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct Dev {
    id: String,
    project: String,
    version: String,
    elf: String,
    prev_version: String,
    booted: Instant,
    reset_reason: &'static str,
    pending_until: Option<Instant>,
    offline_until: Option<Instant>,
    rollback: bool,
    rssi: i64,
}

type S = Arc<Mutex<Dev>>;

fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).to_string()
}

fn pseudo_hex(seed: &str) -> String {
    // deterministic stand-in for an ELF SHA-256
    let mut h: u64 = 1469598103934665603;
    let mut out = String::new();
    for i in 0..4u8 {
        for b in seed.bytes().chain([i]) {
            h ^= b as u64;
            h = h.wrapping_mul(1099511628211);
        }
        out.push_str(&format!("{h:016x}"));
    }
    out
}

fn mkimage(path: &str, project: &str, version: &str) {
    let mut img = vec![0u8; 64 * 1024];
    img[0] = 0xE9;
    img[12..14].copy_from_slice(&0x0012u16.to_le_bytes());
    let d = 32;
    img[d..d + 4].copy_from_slice(&0xABCD_5432u32.to_le_bytes());
    img[d + 16..d + 16 + version.len()].copy_from_slice(version.as_bytes());
    img[d + 48..d + 48 + project.len()].copy_from_slice(project.as_bytes());
    img[d + 80..d + 88].copy_from_slice(b"12:00:00");
    img[d + 96..d + 107].copy_from_slice(b"Nov 21 2026");
    img[d + 112..d + 116].copy_from_slice(b"v6.1");
    let elf = pseudo_hex(&format!("{project}{version}"));
    for i in 0..32 {
        img[d + 144 + i] = u8::from_str_radix(&elf[i * 2..i * 2 + 2], 16).unwrap();
    }
    std::fs::write(path, img).unwrap();
    println!("wrote {path}: {project} {version} elf {elf}");
}

async fn info(State(s): State<S>) -> impl IntoResponse {
    let mut d = s.lock().unwrap();
    let now = Instant::now();
    if d.offline_until.is_some_and(|t| now < t) {
        return (StatusCode::SERVICE_UNAVAILABLE, "rebooting").into_response();
    }
    d.offline_until = None;
    let pending = d.pending_until.is_some_and(|t| now < t);
    d.rssi = -50 - ((now.elapsed().as_nanos() as i64 + d.booted.elapsed().as_secs() as i64 * 7) % 15);
    Json(json!({
        "schema": 1, "id": d.id, "hostname": format!("fake-{}", &d.id[6..]), "label": "Fake device",
        "project": d.project, "version": d.version, "idf": "v6.1", "build": "Nov 21 2026 12:00:00",
        "elf_sha256": d.elf, "chip": "esp32p4", "chip_rev": 100, "partition": "ota_0",
        "pending_verify": pending, "uptime_s": d.booted.elapsed().as_secs(), "reset_reason": d.reset_reason,
        "time": 0, "ip": "127.0.0.1", "rssi": d.rssi,
        "heap": {"int": 180000, "int_min": 150000, "psram": 30000000, "psram_min": 29000000},
        "caps": {"ota": true, "reboot": true, "revert": true, "identify": true},
        "status": {"can_revert": !d.prev_version.is_empty(), "prev_version": d.prev_version, "last_crash": if d.reset_reason == "panic" { "Guru Meditation Error: LoadProhibited (fake)" } else { "" }},
        "app": {"offset_us": 12.5, "stratum": 1, "gps_lock": true}
    }))
    .into_response()
}

fn authed(h: &HeaderMap) -> bool {
    let tok = std::env::var("FAKE_TOKEN").unwrap_or_else(|_| "test".into());
    h.get("authorization").and_then(|v| v.to_str().ok()) == Some(&format!("Bearer {tok}"))
}

async fn ota(State(s): State<S>, h: HeaderMap, body: Bytes) -> impl IntoResponse {
    if !authed(&h) {
        return (StatusCode::UNAUTHORIZED, "missing or wrong fleet token".to_string());
    }
    if body.len() < 300 || body[0] != 0xE9 {
        return (StatusCode::BAD_REQUEST, "Update failed: not an ESP firmware image (.bin)".into());
    }
    let version = cstr(&body[48..80]);
    let project = cstr(&body[80..112]);
    let elf = hex(&body[176..208]);
    {
        let d = s.lock().unwrap();
        if project != d.project {
            return (StatusCode::BAD_REQUEST, "Update failed: image is a different project".into());
        }
    }
    let s2 = s.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let mut d = s2.lock().unwrap();
        let now = Instant::now();
        d.offline_until = Some(now + Duration::from_secs(6));
        if d.rollback {
            // new image fails its boot check; bootloader returns to the old one
            d.booted = now + Duration::from_secs(4);
            d.reset_reason = "panic";
            d.pending_until = None;
        } else {
            d.prev_version = std::mem::replace(&mut d.version, version);
            d.elf = elf;
            d.booted = now + Duration::from_secs(4);
            d.reset_reason = "software";
            d.pending_until = Some(now + Duration::from_secs(14));
        }
    });
    (StatusCode::OK, "Update installed. The display is rebooting into the new firmware.".into())
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

async fn reboot(State(s): State<S>, h: HeaderMap) -> impl IntoResponse {
    if !authed(&h) {
        return (StatusCode::UNAUTHORIZED, "missing or wrong fleet token");
    }
    let mut d = s.lock().unwrap();
    let now = Instant::now();
    d.offline_until = Some(now + Duration::from_secs(4));
    d.booted = now + Duration::from_secs(3);
    d.reset_reason = "software";
    (StatusCode::ACCEPTED, "rebooting")
}

async fn identify(h: HeaderMap) -> impl IntoResponse {
    if !authed(&h) {
        return (StatusCode::UNAUTHORIZED, "missing or wrong fleet token");
    }
    println!("*blink blink*");
    (StatusCode::OK, "identifying")
}

#[tokio::main]
async fn main() {
    let a: Vec<String> = std::env::args().collect();
    match a.get(1).map(String::as_str) {
        Some("mkimage") if a.len() == 5 => mkimage(&a[2], &a[3], &a[4]),
        Some("serve") if a.len() >= 6 => {
            let port: u16 = a[2].parse().expect("port");
            let (id, project, version) = (a[3].clone(), a[4].clone(), a[5].clone());
            let rollback = a.get(6).map(String::as_str) == Some("rollback");
            let dev = Dev {
                elf: pseudo_hex(&format!("{project}{version}")),
                id: id.clone(),
                project: project.clone(),
                version: version.clone(),
                prev_version: String::new(),
                booted: Instant::now(),
                reset_reason: "power_on",
                pending_until: None,
                offline_until: None,
                rollback,
                rssi: -60,
            };
            let s: S = Arc::new(Mutex::new(dev));

            // announce like the real agent does
            let mdns = mdns_sd::ServiceDaemon::new().expect("mdns");
            let host = format!("fake-{}.local.", &id[6..]);
            let props = [("id", id.as_str()), ("project", project.as_str()), ("ver", version.as_str()), ("label", "Fake device")];
            match mdns_sd::ServiceInfo::new("_p4fleet._tcp.local.", &format!("Fake device {}", &id[6..]), &host, "", port, &props[..]) {
                Ok(info) => {
                    if let Err(e) = mdns.register(info.enable_addr_auto()) {
                        eprintln!("mDNS register failed: {e}");
                    }
                }
                Err(e) => eprintln!("mDNS info failed: {e}"),
            }

            let app = Router::new()
                .route("/fleet/info", get(info))
                .route("/fleet/ota", post(ota))
                .route("/fleet/reboot", post(reboot))
                .route("/fleet/revert", post(reboot))
                .route("/fleet/identify", post(identify))
                .layer(axum::extract::DefaultBodyLimit::max(32 * 1024 * 1024))
                .with_state(s);
            let l = tokio::net::TcpListener::bind(("0.0.0.0", port)).await.expect("bind");
            println!("fake device {id} ({project} {version}{}) on :{port}", if rollback { ", will roll back" } else { "" });
            axum::serve(l, app).await.unwrap();
            drop(mdns);
        }
        _ => eprintln!("usage: fake_device serve <port> <id> <project> <version> [ok|rollback]\n       fake_device mkimage <out.bin> <project> <version>"),
    }
}
