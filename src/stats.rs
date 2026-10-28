//! What each box knows about itself, for `dyode status`, the status message
//! the input box sends through the diode, and the API's `/v1/status` and
//! `/metrics`.
//!
//! Everything is counted where it already happens: every [`Event`] that
//! goes to transfer.jsonl is also counted here (before the per-file switch,
//! so `per_file_events: false` does not hide counts), and the supervisor
//! reports module starts and restarts.  Nothing in the modules changes.
//!
//! [`Event`]: crate::logging::Event

use crate::config::{Config, ModuleKind};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

/// Seconds since the Unix epoch, as f64.
fn now() -> f64 {
    crate::timefmt::unix_now_f64()
}

#[derive(Clone, Debug, Default)]
pub struct ModuleStats {
    pub name: String,
    pub kind: String,
    pub endpoint: u16,
    /// "starting", "running" or "restarting".
    pub state: String,
    pub restarts: u64,
    pub last_activity: Option<f64>,
    /// transfer.jsonl events by name.
    pub events: BTreeMap<String, u64>,
    pub bytes_sent: u64,
    pub bytes_stored: u64,
    /// Folders whose free space is reported: (label, path).
    dirs: Vec<(String, PathBuf)>,
}

/// The last status the other box sent through the diode.
#[derive(Clone, Debug)]
pub struct Peer {
    pub status: Value,
    pub received: f64,
}

struct Registry {
    side: String,
    config_name: String,
    transport_hash: String,
    started: Option<Instant>,
    modules: Vec<ModuleStats>,
    global: BTreeMap<String, u64>,
    peer: Option<Peer>,
    arp_ok: Option<bool>,
}

static REG: Mutex<Registry> = Mutex::new(Registry {
    side: String::new(),
    config_name: String::new(),
    transport_hash: String::new(),
    started: None,
    modules: Vec::new(),
    global: BTreeMap::new(),
    peer: None,
    arp_ok: None,
});

fn reg() -> std::sync::MutexGuard<'static, Registry> {
    REG.lock().unwrap_or_else(|p| p.into_inner())
}

/// A short hash of everything both boxes must agree on: the lidi
/// parameters and every module's name and type.  Two boxes with different
/// hashes refuse at least some of each other's traffic.
pub fn transport_hash(cfg: &Config) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(crate::transport::lidi_toml(cfg).as_bytes());
    for m in &cfg.modules {
        h.update(m.tag());
    }
    crate::hex::encode(&h.finalize()[..6])
}

/// Set up the registry for this box.  Called once at start.
pub fn init(cfg: &Config, side: &str) {
    let mut r = reg();
    r.side = side.to_string();
    r.config_name = cfg.config_name.clone().unwrap_or_default();
    r.transport_hash = transport_hash(cfg);
    r.started = Some(Instant::now());
    r.modules = cfg
        .modules
        .iter()
        .map(|m| {
            let dirs = match (&m.kind, side) {
                (ModuleKind::Folder(f), "in") => vec![("in".to_string(), f.input.clone())],
                (ModuleKind::Folder(f), _) => vec![("out".to_string(), f.output.clone())],
                (ModuleKind::Screen(s), "in") => vec![("in".to_string(), s.input.clone())],
                (ModuleKind::Screen(s), _) => vec![("out".to_string(), s.output.clone())],
                (ModuleKind::Logs(l), "out") => l
                    .out
                    .iter()
                    .map(|p| ("out".to_string(), p.clone()))
                    .collect(),
                _ => vec![],
            };
            ModuleStats {
                name: m.name.clone(),
                kind: m.kind.type_name().to_string(),
                endpoint: m.index,
                state: "starting".into(),
                dirs,
                ..Default::default()
            }
        })
        .collect();
}

fn with_module(name: &str, f: impl FnOnce(&mut ModuleStats)) {
    let mut r = reg();
    if let Some(m) = r.modules.iter_mut().find(|m| m.name == name) {
        f(m);
    }
}

/// Every transfer.jsonl event passes through here.
pub fn on_event(event: &str, module: Option<&str>, fields: &Map<String, Value>) {
    let Some(name) = module else {
        *reg().global.entry(event.to_string()).or_default() += 1;
        return;
    };
    let bytes = fields.get("bytes").and_then(Value::as_u64).unwrap_or(0);
    with_module(name, |m| {
        *m.events.entry(event.to_string()).or_default() += 1;
        m.last_activity = Some(now());
        match event {
            "file_sent" => m.bytes_sent += bytes,
            "file_stored" => m.bytes_stored += bytes,
            _ => {}
        }
    });
}

/// From the supervisor: a module (re)started or stopped.
pub fn module_started(name: &str) {
    with_module(name, |m| m.state = "running".into());
}

pub fn module_stopped(name: &str) {
    with_module(name, |m| {
        m.state = "restarting".into();
        m.restarts += 1;
    });
}

/// A counter that belongs to no module (lidi aborts, API refusals).
pub fn count(what: &str) {
    *reg().global.entry(what.to_string()).or_default() += 1;
}

/// The input box's static ARP entry, as last checked.
pub fn set_arp_ok(ok: bool) {
    reg().arp_ok = Some(ok);
}

/// The output box received the input box's status.
pub fn set_peer(status: Value) {
    reg().peer = Some(Peer {
        status,
        received: now(),
    });
}

pub fn peer() -> Option<Peer> {
    reg().peer.clone()
}

fn free_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: a valid C string and an out-pointer to a statvfs.
    (unsafe { libc::statvfs(c.as_ptr(), &mut s) } == 0)
        .then(|| s.f_bavail.saturating_mul(s.f_frsize))
}

/// This box's status, as JSON (the same shape is sent through the diode).
pub fn snapshot() -> Value {
    let r = reg();
    let modules: Vec<Value> = r
        .modules
        .iter()
        .map(|m| {
            let free: Vec<Value> = m
                .dirs
                .iter()
                .filter_map(|(label, p)| {
                    free_bytes(p).map(|b| json!({"dir": label, "path": p, "free_bytes": b}))
                })
                .collect();
            json!({
                "name": m.name,
                "type": m.kind,
                "endpoint": m.endpoint,
                "state": m.state,
                "restarts": m.restarts,
                "last_activity": m.last_activity.map(crate::timefmt::iso_utc),
                "events": m.events,
                "bytes_sent": m.bytes_sent,
                "bytes_stored": m.bytes_stored,
                "free": free,
            })
        })
        .collect();
    let mut v = json!({
        "side": r.side,
        "version": env!("CARGO_PKG_VERSION"),
        "config_name": r.config_name,
        "transport_hash": r.transport_hash,
        "time": crate::timefmt::utc_now(),
        "uptime_s": r.started.map_or(0, |s| s.elapsed().as_secs()),
        "modules": modules,
        "counters": r.global,
    });
    if let Some(a) = r.arp_ok {
        v["static_arp_ok"] = a.into();
    }
    v
}

/// Seconds since the peer's status arrived; None if it never did.
pub fn peer_age() -> Option<f64> {
    peer().map(|p| (now() - p.received).max(0.0))
}

/// Both boxes, as the output box's API serves them.  `stale_after` is
/// when the input box counts as silent.
pub fn combined(stale_after: f64) -> Value {
    let me = snapshot();
    let mut warnings: Vec<String> = Vec::new();
    let mut boxes = Map::new();
    let side = me["side"].as_str().unwrap_or("").to_string();
    if side == "out" {
        match peer() {
            Some(p) => {
                let age = (now() - p.received).max(0.0);
                if age > stale_after {
                    warnings.push(format!(
                        "no status from the input box for {age:.0}s: the input box, the diode or the link may be down"
                    ));
                }
                if p.status["transport_hash"] != me["transport_hash"] {
                    warnings.push(
                        "the input box runs a different configuration (transport hash differs): some traffic will be refused".into(),
                    );
                }
                let mut s = p.status;
                s["status_age_s"] = json!((age * 10.0).round() / 10.0);
                s["received"] = crate::timefmt::iso_utc(p.received).into();
                boxes.insert("in".into(), s);
            }
            None => {
                warnings.push("no status received from the input box yet".into());
                boxes.insert("in".into(), Value::Null);
            }
        }
    }
    let mut me = me;
    me["status_age_s"] = json!(0);
    boxes.insert(side, me);
    json!({"boxes": boxes, "warnings": warnings})
}

// --------------------------------------------------------------------------
// The status file and `dyode status`
// --------------------------------------------------------------------------

pub fn run_dir() -> PathBuf {
    std::env::var_os("DYODE_RUN_DIR").map_or_else(|| PathBuf::from("/run/dyode"), PathBuf::from)
}

pub fn status_path(side: &str) -> PathBuf {
    run_dir().join(format!("status-{side}.json"))
}

/// Write this box's status (and, on the output box, the input box's) for
/// `dyode status`.  Atomic, readable by root only.
pub fn write_status_file(side: &str, stale_after: f64) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let dir = run_dir();
    std::fs::create_dir_all(&dir)?;
    let path = status_path(side);
    let tmp = path.with_extension("tmp");
    let text = serde_json::to_vec_pretty(&combined(stale_after)).unwrap_or_default();
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o640)
        .open(&tmp)?;
    f.write_all(&text)?;
    std::fs::rename(&tmp, &path)
}

/// Render the status file for a person.
pub fn describe(v: &Value) -> String {
    let mut out = String::new();
    let boxes = v["boxes"].as_object().cloned().unwrap_or_default();
    for side in ["in", "out"] {
        let Some(b) = boxes.get(side) else { continue };
        let label = if side == "in" {
            "input box"
        } else {
            "output box"
        };
        if b.is_null() {
            out.push_str(&format!("{label}: no status received yet\n"));
            continue;
        }
        let age = b["status_age_s"].as_f64().unwrap_or(0.0);
        let as_of = if age > 0.0 {
            format!(", as of {age:.0}s ago")
        } else {
            String::new()
        };
        out.push_str(&format!(
            "{label}: DYODE {} up {}s, config {:?} ({}){as_of}\n",
            b["version"].as_str().unwrap_or("?"),
            b["uptime_s"].as_u64().unwrap_or(0),
            b["config_name"].as_str().unwrap_or(""),
            b["transport_hash"].as_str().unwrap_or("?"),
        ));
        if let Some(a) = b["static_arp_ok"].as_bool() {
            out.push_str(&format!(
                "  static ARP entry: {}\n",
                if a { "in place" } else { "MISSING" }
            ));
        }
        for m in b["modules"].as_array().into_iter().flatten() {
            let ev = m["events"].as_object().cloned().unwrap_or_default();
            let mut parts: Vec<String> = ev.iter().map(|(k, n)| format!("{k} {n}")).collect();
            parts.truncate(6);
            out.push_str(&format!(
                "  {:<20} {:<7} {:<10} restarts {}{}\n",
                m["name"].as_str().unwrap_or(""),
                m["type"].as_str().unwrap_or(""),
                m["state"].as_str().unwrap_or(""),
                m["restarts"].as_u64().unwrap_or(0),
                if parts.is_empty() {
                    String::new()
                } else {
                    format!("  [{}]", parts.join(", "))
                }
            ));
        }
    }
    for w in v["warnings"].as_array().into_iter().flatten() {
        out.push_str(&format!("warning: {}\n", w.as_str().unwrap_or("")));
    }
    out
}

// --------------------------------------------------------------------------
// Prometheus text
// --------------------------------------------------------------------------

fn esc(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// `/metrics`: this box, and on the output box the input box too.
pub fn prometheus(stale_after: f64) -> String {
    let all = combined(stale_after);
    let mut o = String::new();
    let help = |o: &mut String, name: &str, ty: &str, text: &str| {
        o.push_str(&format!("# HELP {name} {text}\n# TYPE {name} {ty}\n"));
    };
    let boxes = all["boxes"].as_object().cloned().unwrap_or_default();
    let present: Vec<(&str, &Value)> = ["in", "out"]
        .iter()
        .filter_map(|s| boxes.get(*s).map(|v| (*s, v)))
        .collect();

    help(
        &mut o,
        "dyode_info",
        "gauge",
        "DYODE version and transport hash of each box (1 when known).",
    );
    for (side, b) in &present {
        if b.is_null() {
            continue;
        }
        o.push_str(&format!(
            "dyode_info{{side=\"{side}\",version=\"{}\",transport_hash=\"{}\",config=\"{}\"}} 1\n",
            esc(b["version"].as_str().unwrap_or("")),
            esc(b["transport_hash"].as_str().unwrap_or("")),
            esc(b["config_name"].as_str().unwrap_or(""))
        ));
    }
    help(
        &mut o,
        "dyode_status_age_seconds",
        "gauge",
        "Seconds since this box's status was taken; for the input box, as seen by the output box.",
    );
    for (side, b) in &present {
        // A box never heard from has no age: absent rather than 0.
        if let Some(a) = b["status_age_s"].as_f64() {
            o.push_str(&format!(
                "dyode_status_age_seconds{{side=\"{side}\"}} {a}\n"
            ));
        }
    }
    help(
        &mut o,
        "dyode_status_received",
        "gauge",
        "1 when the box's status is known, 0 when the output box has never heard from the input box.",
    );
    for (side, b) in &present {
        o.push_str(&format!(
            "dyode_status_received{{side=\"{side}\"}} {}\n",
            u8::from(!b.is_null())
        ));
    }
    help(
        &mut o,
        "dyode_uptime_seconds",
        "gauge",
        "Seconds since DYODE started on the box.",
    );
    for (side, b) in &present {
        if let Some(u) = b["uptime_s"].as_u64() {
            o.push_str(&format!("dyode_uptime_seconds{{side=\"{side}\"}} {u}\n"));
        }
    }
    if let (Some(i), Some(out)) = (boxes.get("in"), boxes.get("out")) {
        if !i.is_null() {
            help(
                &mut o,
                "dyode_config_match",
                "gauge",
                "1 when both boxes run the same transport configuration.",
            );
            o.push_str(&format!(
                "dyode_config_match {}\n",
                u8::from(i["transport_hash"] == out["transport_hash"])
            ));
        }
    }
    let modules = |b: &Value| b["modules"].as_array().cloned().unwrap_or_default();
    help(
        &mut o,
        "dyode_module_up",
        "gauge",
        "1 when the module is running, 0 while it restarts.",
    );
    for (side, b) in &present {
        for m in modules(b) {
            o.push_str(&format!(
                "dyode_module_up{{side=\"{side}\",module=\"{}\",type=\"{}\"}} {}\n",
                esc(m["name"].as_str().unwrap_or("")),
                esc(m["type"].as_str().unwrap_or("")),
                u8::from(m["state"] == "running")
            ));
        }
    }
    help(
        &mut o,
        "dyode_module_restarts_total",
        "counter",
        "Times the module stopped and was restarted.",
    );
    for (side, b) in &present {
        for m in modules(b) {
            o.push_str(&format!(
                "dyode_module_restarts_total{{side=\"{side}\",module=\"{}\"}} {}\n",
                esc(m["name"].as_str().unwrap_or("")),
                m["restarts"].as_u64().unwrap_or(0)
            ));
        }
    }
    help(
        &mut o,
        "dyode_events_total",
        "counter",
        "transfer.jsonl events by module and event name.",
    );
    for (side, b) in &present {
        for m in modules(b) {
            for (ev, n) in m["events"].as_object().cloned().unwrap_or_default() {
                o.push_str(&format!(
                    "dyode_events_total{{side=\"{side}\",module=\"{}\",event=\"{}\"}} {}\n",
                    esc(m["name"].as_str().unwrap_or("")),
                    esc(&ev),
                    n.as_u64().unwrap_or(0)
                ));
            }
        }
    }
    help(
        &mut o,
        "dyode_bytes_total",
        "counter",
        "File bytes sent (input box) or stored (output box).",
    );
    for (side, b) in &present {
        for m in modules(b) {
            let (dir, n) = if *side == "in" {
                ("sent", &m["bytes_sent"])
            } else {
                ("stored", &m["bytes_stored"])
            };
            o.push_str(&format!(
                "dyode_bytes_total{{side=\"{side}\",module=\"{}\",direction=\"{dir}\"}} {}\n",
                esc(m["name"].as_str().unwrap_or("")),
                n.as_u64().unwrap_or(0)
            ));
        }
    }
    help(
        &mut o,
        "dyode_last_activity_timestamp_seconds",
        "gauge",
        "Unix time of the module's last event.",
    );
    for (side, b) in &present {
        for m in modules(b) {
            if let Some(t) = m["last_activity"].as_str().and_then(parse_iso) {
                o.push_str(&format!(
                    "dyode_last_activity_timestamp_seconds{{side=\"{side}\",module=\"{}\"}} {t}\n",
                    esc(m["name"].as_str().unwrap_or(""))
                ));
            }
        }
    }
    help(
        &mut o,
        "dyode_free_bytes",
        "gauge",
        "Free space under a module's folder.",
    );
    for (side, b) in &present {
        for m in modules(b) {
            for f in m["free"].as_array().cloned().unwrap_or_default() {
                o.push_str(&format!(
                    "dyode_free_bytes{{side=\"{side}\",module=\"{}\",dir=\"{}\"}} {}\n",
                    esc(m["name"].as_str().unwrap_or("")),
                    esc(f["dir"].as_str().unwrap_or("")),
                    f["free_bytes"].as_u64().unwrap_or(0)
                ));
            }
        }
    }
    help(
        &mut o,
        "dyode_counter_total",
        "counter",
        "Box-wide counters: lidi transfers aborted, API requests refused, and events without a module.",
    );
    for (side, b) in &present {
        for (k, n) in b["counters"].as_object().cloned().unwrap_or_default() {
            o.push_str(&format!(
                "dyode_counter_total{{side=\"{side}\",counter=\"{}\"}} {}\n",
                esc(&k),
                n.as_u64().unwrap_or(0)
            ));
        }
    }
    if let Some(b) = boxes.get("in") {
        if let Some(a) = b["static_arp_ok"].as_bool() {
            help(
                &mut o,
                "dyode_static_arp_ok",
                "gauge",
                "1 when the input box's static ARP entry for the output box is in place.",
            );
            o.push_str(&format!("dyode_static_arp_ok {}\n", u8::from(a)));
        }
    }
    o
}

/// '2026-10-28T09:14:02.123Z' -> Unix seconds.
fn parse_iso(s: &str) -> Option<f64> {
    let d = crate::timefmt::Date::parse(s.get(..10)?)?;
    let h: f64 = s.get(11..13)?.parse().ok()?;
    let m: f64 = s.get(14..16)?.parse().ok()?;
    let sec: f64 = s.get(17..).map(|x| x.trim_end_matches('Z'))?.parse().ok()?;
    Some(d.days() as f64 * 86400.0 + h * 3600.0 + m * 60.0 + sec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_round_trip() {
        let t = 1_793_150_042.25;
        let s = crate::timefmt::iso_utc(t);
        assert!((parse_iso(&s).unwrap() - t).abs() < 0.01, "{s}");
    }

    #[test]
    fn label_escaping() {
        assert_eq!(esc("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
    }
}
