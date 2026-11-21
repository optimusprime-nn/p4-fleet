//! SQLite storage. One connection behind a mutex; every call is short and
//! never held across an await.

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::Serialize;
use serde_json::Value;

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn open(path: &std::path::Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.execute_batch(
        r#"
        PRAGMA journal_mode = WAL;
        PRAGMA foreign_keys = ON;
        CREATE TABLE IF NOT EXISTS devices(
            id TEXT PRIMARY KEY,
            name TEXT,
            host TEXT NOT NULL,
            port INTEGER NOT NULL,
            source TEXT NOT NULL,
            first_seen INTEGER NOT NULL,
            last_seen INTEGER,
            online INTEGER NOT NULL DEFAULT 0,
            last_error TEXT,
            info TEXT
        );
        CREATE TABLE IF NOT EXISTS samples(
            device_id TEXT NOT NULL,
            ts INTEGER NOT NULL,
            uptime_s INTEGER,
            rssi INTEGER,
            heap_int INTEGER,
            heap_int_min INTEGER,
            heap_psram INTEGER
        );
        CREATE INDEX IF NOT EXISTS samples_dev_ts ON samples(device_id, ts);
        CREATE TABLE IF NOT EXISTS events(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            ts INTEGER NOT NULL,
            device_id TEXT,
            severity TEXT NOT NULL,
            kind TEXT NOT NULL,
            message TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS events_ts ON events(ts);
        CREATE TABLE IF NOT EXISTS firmware(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            project TEXT NOT NULL,
            version TEXT NOT NULL,
            idf TEXT NOT NULL,
            build TEXT NOT NULL,
            elf_sha256 TEXT NOT NULL,
            sha256 TEXT NOT NULL UNIQUE,
            size INTEGER NOT NULL,
            filename TEXT NOT NULL,
            uploaded INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS rollouts(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            firmware_id INTEGER NOT NULL,
            state TEXT NOT NULL,
            stop_on_failure INTEGER NOT NULL,
            total INTEGER NOT NULL,
            started INTEGER NOT NULL,
            updated INTEGER NOT NULL,
            message TEXT NOT NULL DEFAULT ''
        );
        CREATE TABLE IF NOT EXISTS jobs(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            rollout_id INTEGER,
            device_id TEXT NOT NULL,
            firmware_id INTEGER NOT NULL,
            state TEXT NOT NULL,
            message TEXT NOT NULL DEFAULT '',
            from_version TEXT NOT NULL DEFAULT '',
            started INTEGER NOT NULL,
            updated INTEGER NOT NULL
        );
        "#,
    )?;
    // Anything that was running when the server stopped can't be resumed.
    let t = now();
    conn.execute(
        "UPDATE jobs SET state='interrupted', message='server restarted', updated=?1
         WHERE state IN ('queued','pushing','rebooting','verifying')",
        params![t],
    )?;
    conn.execute(
        "UPDATE rollouts SET state='interrupted', message='server restarted', updated=?1
         WHERE state='running'",
        params![t],
    )?;
    Ok(conn)
}

// ---------------------------------------------------------------------------
// Devices

#[derive(Serialize, Clone, Debug)]
pub struct Device {
    pub id: String,
    pub name: Option<String>,
    pub host: String,
    pub port: u16,
    pub source: String,
    pub first_seen: i64,
    pub last_seen: Option<i64>,
    pub online: bool,
    pub last_error: Option<String>,
    pub info: Option<Value>,
}

const DEVICE_COLS: &str = "id, name, host, port, source, first_seen, last_seen, online, last_error, info";

fn device_row(r: &Row) -> rusqlite::Result<Device> {
    let info: Option<String> = r.get(9)?;
    Ok(Device {
        id: r.get(0)?,
        name: r.get(1)?,
        host: r.get(2)?,
        port: r.get::<_, i64>(3)? as u16,
        source: r.get(4)?,
        first_seen: r.get(5)?,
        last_seen: r.get(6)?,
        online: r.get::<_, i64>(7)? != 0,
        last_error: r.get(8)?,
        info: info.and_then(|s| serde_json::from_str(&s).ok()),
    })
}

pub fn devices(c: &Connection) -> Result<Vec<Device>> {
    let mut st = c.prepare(&format!(
        "SELECT {DEVICE_COLS} FROM devices ORDER BY COALESCE(name, json_extract(info,'$.label'), id)"
    ))?;
    let rows = st.query_map([], device_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn device(c: &Connection, id: &str) -> Result<Option<Device>> {
    Ok(c
        .query_row(&format!("SELECT {DEVICE_COLS} FROM devices WHERE id=?1"), params![id], device_row)
        .optional()?)
}

/// Insert a newly found device or update its address. Returns true if it is new.
pub fn upsert_device(c: &Connection, id: &str, host: &str, port: u16, source: &str) -> Result<bool> {
    let exists: bool = c
        .query_row("SELECT 1 FROM devices WHERE id=?1", params![id], |_| Ok(true))
        .optional()?
        .unwrap_or(false);
    if exists {
        // Adding a device by hand pins its address; mDNS only moves devices it found.
        c.execute(
            "UPDATE devices SET host=?2, port=?3, source=CASE WHEN ?4='manual' THEN 'manual' ELSE source END
             WHERE id=?1 AND (source=?4 OR ?4='manual')",
            params![id, host, port as i64, source],
        )?;
    } else {
        c.execute(
            "INSERT INTO devices(id, host, port, source, first_seen) VALUES(?1, ?2, ?3, ?4, ?5)",
            params![id, host, port as i64, source, now()],
        )?;
    }
    Ok(!exists)
}

pub fn set_device_name(c: &Connection, id: &str, name: Option<&str>) -> Result<bool> {
    Ok(c.execute("UPDATE devices SET name=?2 WHERE id=?1", params![id, name])? > 0)
}

pub fn delete_device(c: &Connection, id: &str) -> Result<bool> {
    c.execute("DELETE FROM samples WHERE device_id=?1", params![id])?;
    Ok(c.execute("DELETE FROM devices WHERE id=?1", params![id])? > 0)
}

pub fn record_poll_ok(c: &Connection, id: &str, info: &Value) -> Result<()> {
    let t = now();
    c.execute(
        "UPDATE devices SET last_seen=?2, online=1, last_error=NULL, info=?3 WHERE id=?1",
        params![id, t, info.to_string()],
    )?;
    let num = |p: &str| info.pointer(p).and_then(Value::as_i64);
    c.execute(
        "INSERT INTO samples(device_id, ts, uptime_s, rssi, heap_int, heap_int_min, heap_psram)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            id,
            t,
            num("/uptime_s"),
            num("/rssi"),
            num("/heap/int"),
            num("/heap/int_min"),
            num("/heap/psram")
        ],
    )?;
    Ok(())
}

pub fn record_poll_error(c: &Connection, id: &str, err: &str, online: bool) -> Result<()> {
    c.execute(
        "UPDATE devices SET last_error=?2, online=?3 WHERE id=?1",
        params![id, err, online as i64],
    )?;
    Ok(())
}

#[derive(Serialize)]
pub struct Sample {
    pub ts: i64,
    pub uptime_s: Option<i64>,
    pub rssi: Option<i64>,
    pub heap_int: Option<i64>,
    pub heap_int_min: Option<i64>,
    pub heap_psram: Option<i64>,
}

pub fn samples(c: &Connection, id: &str, since: i64) -> Result<Vec<Sample>> {
    let mut st = c.prepare(
        "SELECT ts, uptime_s, rssi, heap_int, heap_int_min, heap_psram FROM samples
         WHERE device_id=?1 AND ts>=?2 ORDER BY ts",
    )?;
    let rows = st
        .query_map(params![id, since], |r| {
            Ok(Sample {
                ts: r.get(0)?,
                uptime_s: r.get(1)?,
                rssi: r.get(2)?,
                heap_int: r.get(3)?,
                heap_int_min: r.get(4)?,
                heap_psram: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn prune(c: &Connection, keep_days: i64) -> Result<()> {
    let t = now();
    c.execute("DELETE FROM samples WHERE ts<?1", params![t - keep_days * 86400])?;
    // events are small and useful for longer
    c.execute("DELETE FROM events WHERE ts<?1", params![t - keep_days * 4 * 86400])?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Events

#[derive(Serialize, Clone, Debug)]
pub struct Event {
    pub id: i64,
    pub ts: i64,
    pub device_id: Option<String>,
    pub severity: String,
    pub kind: String,
    pub message: String,
}

pub fn add_event(c: &Connection, device_id: Option<&str>, severity: &str, kind: &str, message: &str) -> Result<Event> {
    let ts = now();
    c.execute(
        "INSERT INTO events(ts, device_id, severity, kind, message) VALUES(?1, ?2, ?3, ?4, ?5)",
        params![ts, device_id, severity, kind, message],
    )?;
    Ok(Event {
        id: c.last_insert_rowid(),
        ts,
        device_id: device_id.map(str::to_string),
        severity: severity.into(),
        kind: kind.into(),
        message: message.into(),
    })
}

pub fn events(c: &Connection, device_id: Option<&str>, limit: i64) -> Result<Vec<Event>> {
    let mut st = c.prepare(
        "SELECT id, ts, device_id, severity, kind, message FROM events
         WHERE ?1 IS NULL OR device_id=?1 ORDER BY id DESC LIMIT ?2",
    )?;
    let rows = st
        .query_map(params![device_id, limit], |r| {
            Ok(Event {
                id: r.get(0)?,
                ts: r.get(1)?,
                device_id: r.get(2)?,
                severity: r.get(3)?,
                kind: r.get(4)?,
                message: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

// ---------------------------------------------------------------------------
// Firmware

#[derive(Serialize, Clone, Debug)]
pub struct Firmware {
    pub id: i64,
    pub project: String,
    pub version: String,
    pub idf: String,
    pub build: String,
    pub elf_sha256: String,
    pub sha256: String,
    pub size: i64,
    pub filename: String,
    pub uploaded: i64,
}

const FW_COLS: &str = "id, project, version, idf, build, elf_sha256, sha256, size, filename, uploaded";

fn fw_row(r: &Row) -> rusqlite::Result<Firmware> {
    Ok(Firmware {
        id: r.get(0)?,
        project: r.get(1)?,
        version: r.get(2)?,
        idf: r.get(3)?,
        build: r.get(4)?,
        elf_sha256: r.get(5)?,
        sha256: r.get(6)?,
        size: r.get(7)?,
        filename: r.get(8)?,
        uploaded: r.get(9)?,
    })
}

pub fn firmwares(c: &Connection) -> Result<Vec<Firmware>> {
    let mut st = c.prepare(&format!("SELECT {FW_COLS} FROM firmware ORDER BY id DESC"))?;
    let rows = st.query_map([], fw_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn firmware(c: &Connection, id: i64) -> Result<Option<Firmware>> {
    Ok(c
        .query_row(&format!("SELECT {FW_COLS} FROM firmware WHERE id=?1"), params![id], fw_row)
        .optional()?)
}

pub fn firmware_by_sha(c: &Connection, sha: &str) -> Result<Option<Firmware>> {
    Ok(c
        .query_row(&format!("SELECT {FW_COLS} FROM firmware WHERE sha256=?1"), params![sha], fw_row)
        .optional()?)
}

pub fn add_firmware(c: &Connection, f: &Firmware) -> Result<i64> {
    c.execute(
        "INSERT INTO firmware(project, version, idf, build, elf_sha256, sha256, size, filename, uploaded)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![f.project, f.version, f.idf, f.build, f.elf_sha256, f.sha256, f.size, f.filename, f.uploaded],
    )?;
    Ok(c.last_insert_rowid())
}

pub fn delete_firmware(c: &Connection, id: i64) -> Result<bool> {
    Ok(c.execute("DELETE FROM firmware WHERE id=?1", params![id])? > 0)
}

// ---------------------------------------------------------------------------
// Rollouts and per-device jobs

#[derive(Serialize, Clone, Debug)]
pub struct Job {
    pub id: i64,
    pub rollout_id: Option<i64>,
    pub device_id: String,
    pub firmware_id: i64,
    pub state: String,
    pub message: String,
    pub from_version: String,
    pub started: i64,
    pub updated: i64,
}

pub const ACTIVE_JOB_STATES: &str = "'queued','pushing','rebooting','verifying'";

fn job_row(r: &Row) -> rusqlite::Result<Job> {
    Ok(Job {
        id: r.get(0)?,
        rollout_id: r.get(1)?,
        device_id: r.get(2)?,
        firmware_id: r.get(3)?,
        state: r.get(4)?,
        message: r.get(5)?,
        from_version: r.get(6)?,
        started: r.get(7)?,
        updated: r.get(8)?,
    })
}

const JOB_COLS: &str = "id, rollout_id, device_id, firmware_id, state, message, from_version, started, updated";

pub fn add_job(c: &Connection, rollout_id: Option<i64>, device_id: &str, firmware_id: i64, from_version: &str) -> Result<i64> {
    let t = now();
    c.execute(
        "INSERT INTO jobs(rollout_id, device_id, firmware_id, state, from_version, started, updated)
         VALUES(?1, ?2, ?3, 'queued', ?4, ?5, ?5)",
        params![rollout_id, device_id, firmware_id, from_version, t],
    )?;
    Ok(c.last_insert_rowid())
}

pub fn set_job(c: &Connection, id: i64, state: &str, message: &str) -> Result<()> {
    c.execute(
        "UPDATE jobs SET state=?2, message=?3, updated=?4 WHERE id=?1",
        params![id, state, message, now()],
    )?;
    Ok(())
}

pub fn active_job_for(c: &Connection, device_id: &str) -> Result<Option<Job>> {
    Ok(c
        .query_row(
            &format!("SELECT {JOB_COLS} FROM jobs WHERE device_id=?1 AND state IN ({ACTIVE_JOB_STATES}) ORDER BY id DESC LIMIT 1"),
            params![device_id],
            job_row,
        )
        .optional()?)
}

pub fn jobs(c: &Connection, rollout_id: Option<i64>, limit: i64) -> Result<Vec<Job>> {
    let mut st = c.prepare(&format!(
        "SELECT {JOB_COLS} FROM jobs WHERE ?1 IS NULL OR rollout_id=?1 ORDER BY id DESC LIMIT ?2"
    ))?;
    let rows = st.query_map(params![rollout_id, limit], job_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

#[derive(Serialize, Clone, Debug)]
pub struct Rollout {
    pub id: i64,
    pub firmware_id: i64,
    pub state: String,
    pub stop_on_failure: bool,
    pub total: i64,
    pub started: i64,
    pub updated: i64,
    pub message: String,
}

pub fn add_rollout(c: &Connection, firmware_id: i64, stop_on_failure: bool, total: i64) -> Result<i64> {
    let t = now();
    c.execute(
        "INSERT INTO rollouts(firmware_id, state, stop_on_failure, total, started, updated)
         VALUES(?1, 'running', ?2, ?3, ?4, ?4)",
        params![firmware_id, stop_on_failure as i64, total, t],
    )?;
    Ok(c.last_insert_rowid())
}

pub fn set_rollout(c: &Connection, id: i64, state: &str, message: &str) -> Result<()> {
    c.execute(
        "UPDATE rollouts SET state=?2, message=?3, updated=?4 WHERE id=?1",
        params![id, state, message, now()],
    )?;
    Ok(())
}

pub fn rollouts(c: &Connection, limit: i64) -> Result<Vec<Rollout>> {
    let mut st = c.prepare(
        "SELECT id, firmware_id, state, stop_on_failure, total, started, updated, message
         FROM rollouts ORDER BY id DESC LIMIT ?1",
    )?;
    let rows = st
        .query_map(params![limit], |r| {
            Ok(Rollout {
                id: r.get(0)?,
                firmware_id: r.get(1)?,
                state: r.get(2)?,
                stop_on_failure: r.get::<_, i64>(3)? != 0,
                total: r.get(4)?,
                started: r.get(5)?,
                updated: r.get(6)?,
                message: r.get(7)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}
