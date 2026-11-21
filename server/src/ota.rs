//! Firmware updates: push an image to one device, or roll it out to every
//! device running that project one at a time, stopping at the first failure.
//!
//! A job follows the device through: queued -> pushing -> rebooting ->
//! verifying (new image booted, pending self-check) -> ok.
//! If the device comes back running the old image after it rebooted, the
//! bootloader rolled the update back: rolled_back. No answer in time: failed.

use crate::db::{self, Device, Firmware};
use crate::{device, App, Shared};
use anyhow::{bail, Result};
use serde_json::Value;
use std::time::{Duration, Instant};

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Outcome {
    Ok,
    RolledBack,
    Failed,
}

fn info_str<'a>(d: &'a Device, k: &str) -> &'a str {
    d.info.as_ref().and_then(|i| i.get(k)).and_then(Value::as_str).unwrap_or("")
}

/// Why this firmware can't go to this device right now, if anything.
pub fn ineligible(app: &App, d: &Device, fw: &Firmware) -> Option<String> {
    if app.cfg.device_token.is_none() {
        return Some("FLEET_DEVICE_TOKEN is not set on the server".into());
    }
    if !d.online {
        return Some("device is offline".into());
    }
    let ota_cap = d.info.as_ref().and_then(|i| i.pointer("/caps/ota")).and_then(Value::as_bool);
    if ota_cap != Some(true) {
        return Some("device does not accept fleet OTA (no fleet token on the device?)".into());
    }
    if info_str(d, "project") != fw.project {
        return Some(format!("device runs {}, firmware is for {}", info_str(d, "project"), fw.project));
    }
    if d.info.as_ref().and_then(|i| i.get("pending_verify")).and_then(Value::as_bool) == Some(true) {
        return Some("device is still verifying its current firmware".into());
    }
    if db::active_job_for(&app.db(), &d.id).ok().flatten().is_some() {
        return Some("an update is already in progress".into());
    }
    None
}

pub fn firmware_path(app: &App, fw: &Firmware) -> std::path::PathBuf {
    app.firmware_dir().join(format!("{}.bin", fw.sha256))
}

/// Push one firmware to one device, now.
pub fn start_single(app: &Shared, device_id: &str, firmware_id: i64) -> Result<i64> {
    let (d, fw) = {
        let c = app.db();
        let Some(d) = db::device(&c, device_id)? else { bail!("unknown device") };
        let Some(fw) = db::firmware(&c, firmware_id)? else { bail!("unknown firmware") };
        (d, fw)
    };
    if let Some(why) = ineligible(app, &d, &fw) {
        bail!(why);
    }
    let job = db::add_job(&app.db(), None, &d.id, fw.id, info_str(&d, "version"))?;
    let app = app.clone();
    tokio::spawn(async move {
        run_job(&app, job, &d, &fw).await;
    });
    Ok(job)
}

pub struct RolloutPlan {
    pub rollout_id: i64,
    pub targets: Vec<String>,
    pub skipped: Vec<(String, String)>,
}

/// Roll a firmware out to `device_ids` (or every device running its project
/// that doesn't have it yet), one device at a time.
pub fn start_rollout(app: &Shared, firmware_id: i64, device_ids: Option<Vec<String>>, stop_on_failure: bool) -> Result<RolloutPlan> {
    let (fw, devices) = {
        let c = app.db();
        let Some(fw) = db::firmware(&c, firmware_id)? else { bail!("unknown firmware") };
        (fw, db::devices(&c)?)
    };
    let mut targets = Vec::new();
    let mut skipped = Vec::new();
    for d in devices {
        let picked = match &device_ids {
            Some(ids) => ids.iter().any(|i| i == &d.id),
            None => info_str(&d, "project") == fw.project,
        };
        if !picked {
            continue;
        }
        if info_str(&d, "elf_sha256") == fw.elf_sha256 {
            skipped.push((d.id.clone(), "already running this firmware".into()));
        } else if let Some(why) = ineligible(app, &d, &fw) {
            skipped.push((d.id.clone(), why));
        } else {
            targets.push(d);
        }
    }
    if targets.is_empty() {
        bail!(
            "no device to update{}",
            if skipped.is_empty() { String::new() } else { format!(" ({} skipped)", skipped.len()) }
        );
    }
    let rollout_id = db::add_rollout(&app.db(), fw.id, stop_on_failure, targets.len() as i64)?;
    let mut jobs = Vec::new();
    for d in &targets {
        jobs.push(db::add_job(&app.db(), Some(rollout_id), &d.id, fw.id, info_str(d, "version"))?);
    }
    app.event(
        None,
        "info",
        "rollout",
        &format!("rollout #{rollout_id}: {} {} to {} device(s)", fw.project, fw.version, targets.len()),
    );

    let plan = RolloutPlan {
        rollout_id,
        targets: targets.iter().map(|d| d.id.clone()).collect(),
        skipped,
    };
    let app = app.clone();
    tokio::spawn(async move {
        let (mut ok, mut bad) = (0, 0);
        let mut halted = false;
        for (d, job) in targets.iter().zip(jobs) {
            if halted {
                let _ = db::set_job(&app.db(), job, "skipped", "rollout halted after a failure");
                continue;
            }
            // re-read: the device may have changed while earlier ones updated
            let fresh = db::device(&app.db(), &d.id).ok().flatten();
            let why = match &fresh {
                None => Some("device was removed".to_string()),
                Some(f) => ineligible_except_job(&app, f, &fw),
            };
            if let Some(why) = why {
                let _ = db::set_job(&app.db(), job, "skipped", &why);
                continue;
            }
            match run_job(&app, job, fresh.as_ref().unwrap(), &fw).await {
                Outcome::Ok => ok += 1,
                _ => {
                    bad += 1;
                    halted = stop_on_failure;
                }
            }
        }
        let (state, msg) = if halted {
            ("halted", format!("stopped after a failure: {ok} updated, {bad} failed"))
        } else {
            ("done", format!("{ok} updated, {bad} failed"))
        };
        let _ = db::set_rollout(&app.db(), rollout_id, state, &msg);
        let sev = if bad > 0 { "warn" } else { "info" };
        app.event(None, sev, "rollout", &format!("rollout #{rollout_id} {state}: {msg}"));
    });
    Ok(plan)
}

/// `ineligible` minus the active-job check (the queued job is our own).
fn ineligible_except_job(app: &App, d: &Device, fw: &Firmware) -> Option<String> {
    match ineligible(app, d, fw) {
        Some(w) if w == "an update is already in progress" => None,
        other => other,
    }
}

async fn run_job(app: &App, job: i64, d: &Device, fw: &Firmware) -> Outcome {
    let name = device::display_name(d);
    let set = |state: &str, msg: &str| {
        if let Err(e) = db::set_job(&app.db(), job, state, msg) {
            tracing::error!("updating job {job}: {e:#}");
        }
    };
    let fail = |state: &str, msg: String| {
        set(state, &msg);
        let kind = if state == "rolled_back" { "rollback" } else { "ota_failed" };
        app.event(Some(&d.id), "error", kind, &format!("{name}: update to {} {}: {msg}", fw.project, fw.version));
    };

    let image = match tokio::fs::read(firmware_path(app, fw)).await {
        Ok(b) => b,
        Err(e) => {
            fail("failed", format!("firmware file missing on the server: {e}"));
            return Outcome::Failed;
        }
    };

    set("pushing", &format!("sending {} KB", image.len() / 1024));
    let pushed = Instant::now();
    if let Err(e) = device::push_firmware(app, d, image).await {
        fail("failed", format!("{e:#}"));
        return Outcome::Failed;
    }
    set("rebooting", "installed, waiting for the device to reboot");

    let timeout = Duration::from_secs(app.cfg.ota_timeout_s);
    let mut verifying = false;
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        if pushed.elapsed() > timeout {
            let what = if verifying {
                "new firmware booted but did not confirm itself in time"
            } else {
                "device did not come back in time"
            };
            fail("failed", what.into());
            return Outcome::Failed;
        }
        let Ok(info) = device::fetch_info(app, &d.host, d.port).await else { continue };
        let elf = info.get("elf_sha256").and_then(Value::as_str).unwrap_or("");
        let pending = info.get("pending_verify").and_then(Value::as_bool).unwrap_or(false);
        let uptime = info.get("uptime_s").and_then(Value::as_u64).unwrap_or(u64::MAX);
        let rebooted_since_push = uptime < pushed.elapsed().as_secs();

        if elf == fw.elf_sha256 {
            if pending {
                if !verifying {
                    verifying = true;
                    set("verifying", "new firmware booted, waiting for it to confirm itself");
                }
                continue;
            }
            set("ok", &format!("running {}", fw.version));
            app.event(Some(&d.id), "info", "ota_ok", &format!("{name}: updated to {} {}", fw.project, fw.version));
            crate::poller::poll_one(app, &d.id).await;
            return Outcome::Ok;
        }
        if rebooted_since_push {
            let ver = info.get("version").and_then(Value::as_str).unwrap_or("?");
            let reason = info.get("reset_reason").and_then(Value::as_str).unwrap_or("?");
            fail(
                "rolled_back",
                format!("device rebooted ({reason}) and is back on {ver}: the new firmware did not pass its boot check"),
            );
            crate::poller::poll_one(app, &d.id).await;
            return Outcome::RolledBack;
        }
    }
}
