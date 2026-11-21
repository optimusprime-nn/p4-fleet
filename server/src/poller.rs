//! Polls every device's /fleet/info, stores health samples and turns changes
//! (offline, back online, crash, reboot, new firmware) into events.

use crate::{db, device, App, Shared};
use serde_json::Value;
use std::time::Duration;

pub async fn run(app: Shared) {
    let mut tick = tokio::time::interval(Duration::from_secs(app.cfg.poll_s));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_prune = 0i64;
    loop {
        tick.tick().await;
        let ids: Vec<String> = match db::devices(&app.db()) {
            Ok(v) => v.into_iter().map(|d| d.id).collect(),
            Err(e) => {
                tracing::error!("listing devices: {e:#}");
                continue;
            }
        };
        let mut set = tokio::task::JoinSet::new();
        for id in ids {
            let app = app.clone();
            set.spawn(async move { poll_one(&app, &id).await });
        }
        while set.join_next().await.is_some() {}

        let now = db::now();
        if now - last_prune > 3600 {
            last_prune = now;
            if let Err(e) = db::prune(&app.db(), app.cfg.retention_days) {
                tracing::warn!("pruning: {e:#}");
            }
        }
    }
}

fn is_crash(reason: &str) -> bool {
    matches!(reason, "panic" | "int_wdt" | "task_wdt" | "wdt" | "brownout")
}

fn fmt_dur(s: i64) -> String {
    match s {
        s if s < 120 => format!("{s}s"),
        s if s < 7200 => format!("{}m", s / 60),
        s if s < 172800 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86400),
    }
}

pub async fn poll_one(app: &App, id: &str) {
    let Some(dev) = db::device(&app.db(), id).ok().flatten() else { return };
    let busy = db::active_job_for(&app.db(), id).ok().flatten().is_some();
    let name = device::display_name(&dev);

    match device::fetch_info(app, &dev.host, dev.port).await {
        Ok(info) => {
            let got = info.get("id").and_then(Value::as_str).unwrap_or("").to_lowercase();
            if got != dev.id {
                let msg = format!("{}:{} now answers as device {got}", dev.host, dev.port);
                let _ = db::record_poll_error(&app.db(), id, &msg, false);
                return;
            }
            let events = changes(&dev, &info, busy);
            if let Err(e) = db::record_poll_ok(&app.db(), id, &info) {
                tracing::error!("storing poll of {name}: {e:#}");
            }
            for (sev, kind, msg) in events {
                app.event(Some(id), sev, kind, &format!("{name}: {msg}"));
            }
        }
        Err(e) => {
            let msg = format!("{e:#}");
            let grace = (app.cfg.poll_s * 3 + 10) as i64;
            let stale = dev.last_seen.map(|t| db::now() - t > grace).unwrap_or(true);
            let online = dev.online && !stale;
            let _ = db::record_poll_error(&app.db(), id, &msg, online);
            // a device rebooting into new firmware is expected to vanish for a while
            if dev.online && !online && !busy {
                app.event(Some(id), "warn", "offline", &format!("{name}: went offline ({msg})"));
            }
        }
    }
}

/// Compare the previous info with the new one.
fn changes(dev: &db::Device, new: &Value, busy: bool) -> Vec<(&'static str, &'static str, String)> {
    let mut out = Vec::new();
    if !dev.online && dev.last_seen.is_some() {
        out.push(("info", "online", "back online".to_string()));
    }
    let Some(old) = dev.info.as_ref() else { return out };
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let n = |v: &Value, k: &str| v.get(k).and_then(Value::as_i64);

    if let (Some(ou), Some(nu)) = (n(old, "uptime_s"), n(new, "uptime_s")) {
        if nu < ou {
            let reason = s(new, "reset_reason");
            if is_crash(&reason) {
                let crash = new.pointer("/status/last_crash").and_then(Value::as_str).unwrap_or("");
                let detail = if crash.is_empty() { String::new() } else { format!(": {crash}") };
                out.push(("error", "crash", format!("crashed ({reason}) after {} uptime{detail}", fmt_dur(ou))));
            } else {
                let sev = if busy { "info" } else { "warn" };
                out.push((sev, "reboot", format!("rebooted ({reason}) after {} uptime", fmt_dur(ou))));
            }
        }
    }
    if s(old, "elf_sha256") != s(new, "elf_sha256") {
        let (ov, nv) = (s(old, "version"), s(new, "version"));
        let msg = if ov == nv {
            format!("firmware changed (same version {nv}, different build)")
        } else {
            format!("firmware changed {ov} -> {nv}")
        };
        out.push(("info", "firmware", msg));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn dev(info: Value, online: bool) -> db::Device {
        db::Device {
            id: "aabbccddeeff".into(),
            name: None,
            host: "10.0.0.2".into(),
            port: 80,
            source: "manual".into(),
            first_seen: 0,
            last_seen: Some(1),
            online,
            last_error: None,
            info: Some(info),
        }
    }

    #[test]
    fn detects_crash_and_firmware_change() {
        let old = json!({"uptime_s": 7200, "elf_sha256": "aa", "version": "0.1.0"});
        let new = json!({"uptime_s": 5, "reset_reason": "panic", "elf_sha256": "bb", "version": "0.2.0",
                         "status": {"last_crash": "LoadProhibited in ui_task"}});
        let ev = changes(&dev(old, true), &new, false);
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[0].1, "crash");
        assert!(ev[0].2.contains("LoadProhibited"));
        assert_eq!(ev[1].1, "firmware");
        assert!(ev[1].2.contains("0.1.0 -> 0.2.0"));
    }

    #[test]
    fn back_online_and_quiet_when_unchanged() {
        let info = json!({"uptime_s": 100, "elf_sha256": "aa"});
        let newer = json!({"uptime_s": 130, "elf_sha256": "aa"});
        assert!(changes(&dev(info.clone(), true), &newer, false).is_empty());
        let ev = changes(&dev(info, false), &newer, false);
        assert_eq!(ev[0].1, "online");
    }
}
