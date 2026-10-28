//! What `dyode setup` is building: the answers so far, the facts about
//! this machine it relies on, the config.yaml they make, and the changes
//! that writing it makes on disk.  Nothing in here talks to the person.

use crate::config::{self, Cidr, Side};
use crate::setup::Interface;
use serde_yaml::{Mapping, Value};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Variant {
    /// Ethernet diode, lidi transport.
    V1,
    /// Serial link, Modbus only.
    V2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModType {
    Folder,
    Modbus,
    Logs,
    Screen,
}

impl ModType {
    pub const ALL: [ModType; 4] = [
        ModType::Folder,
        ModType::Modbus,
        ModType::Logs,
        ModType::Screen,
    ];

    pub fn name(self) -> &'static str {
        match self {
            ModType::Folder => "folder",
            ModType::Modbus => "modbus",
            ModType::Logs => "logs",
            ModType::Screen => "screen",
        }
    }

    pub fn from_name(s: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|t| t.name().eq_ignore_ascii_case(s))
    }

    pub fn describe(self) -> &'static str {
        match self {
            ModType::Folder => "copy files dropped in a folder across",
            ModType::Modbus => "mirror a PLC's registers and coils (read-only)",
            ModType::Logs => "stream syslog / log files one way",
            ModType::Screen => "relay screenshots to a web page",
        }
    }
}

/// One module: its name, type and its whole mapping, so options the wizard
/// does not ask about (settle, bitrate, ...) survive an import untouched.
#[derive(Clone, Debug)]
pub struct ModDraft {
    pub name: String,
    pub ty: ModType,
    pub map: Mapping,
}

impl ModDraft {
    pub fn new(name: &str, ty: ModType) -> Self {
        let mut map = Mapping::new();
        map.insert("type".into(), ty.name().into());
        Self {
            name: name.into(),
            ty,
            map,
        }
    }

    /// A field as the text the wizard shows: lists joined with ", ".
    pub fn get(&self, key: &str) -> String {
        match self.map.get(key) {
            None | Some(Value::Null) => String::new(),
            Some(Value::Sequence(s)) => s.iter().map(scalar_text).collect::<Vec<_>>().join(", "),
            Some(v) => scalar_text(v),
        }
    }

    /// Set a field from text; empty removes it.
    pub fn set(&mut self, key: &str, text: &str, kind: FieldKind) {
        let text = text.trim();
        if text.is_empty() {
            self.map.remove(key);
            return;
        }
        let v = match kind {
            FieldKind::Text => Value::String(text.into()),
            FieldKind::Int => text
                .parse::<u64>()
                .map_or_else(|_| Value::String(text.into()), Value::from),
            FieldKind::Num => match (text.parse::<u64>(), text.parse::<f64>()) {
                (Ok(n), _) => Value::from(n),
                (_, Ok(f)) => Value::from(f),
                _ => Value::String(text.into()),
            },
            FieldKind::Bool => match text.to_lowercase().as_str() {
                "true" | "yes" | "on" | "1" => Value::Bool(true),
                "false" | "no" | "off" | "0" => Value::Bool(false),
                _ => Value::String(text.into()),
            },
            FieldKind::List => {
                Value::Sequence(split_list(text).into_iter().map(Value::String).collect())
            }
        };
        self.map.insert(key.into(), v);
    }

    /// One line describing the module, for menus and the review.
    pub fn summary(&self) -> String {
        let n = advanced(self.ty)
            .iter()
            .filter(|f| !self.get(f.key).is_empty())
            .count();
        let s = self.main_summary();
        if n == 0 {
            s
        } else {
            format!("{s} (+{n} advanced)")
        }
    }

    fn main_summary(&self) -> String {
        let g = |k| self.get(k);
        match self.ty {
            ModType::Folder => format!("{} -> {}", g("in"), g("out")),
            ModType::Screen => format!(
                "{} -> {}, web port {}",
                g("in"),
                g("out"),
                or(&g("http_port"), "8080")
            ),
            ModType::Modbus => {
                let mut parts = vec![
                    format!("PLC {}", g("ip")),
                    format!("served on {}", or(&g("port_out"), "502")),
                ];
                if !g("registers").is_empty() {
                    parts.push(format!("registers {}", g("registers")));
                }
                if !g("coils").is_empty() {
                    parts.push(format!("coils {}", g("coils")));
                }
                parts.join(", ")
            }
            ModType::Logs => {
                let mut src = Vec::new();
                if !g("listen_udp").is_empty() {
                    src.push(format!("udp {}", g("listen_udp")));
                }
                if !g("listen_tcp").is_empty() {
                    src.push(format!("tcp {}", g("listen_tcp")));
                }
                if !g("tail").is_empty() {
                    src.push(format!("tail {}", g("tail")));
                }
                let mut dst = Vec::new();
                if !g("out").is_empty() {
                    dst.push(g("out"));
                }
                if !g("forward").is_empty() {
                    dst.push(g("forward"));
                }
                format!(
                    "{} -> {}",
                    or(&src.join(", "), "(no source)"),
                    or(&dst.join(", "), "(no destination)")
                )
            }
        }
    }
}

fn or(s: &str, d: &str) -> String {
    if s.is_empty() { d.into() } else { s.into() }
}

fn scalar_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => String::new(),
        other => serde_yaml::to_string(other)
            .unwrap_or_default()
            .trim()
            .to_string(),
    }
}

pub fn split_list(text: &str) -> Vec<String> {
    text.split([',', ' ', '\n'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldKind {
    Text,
    Int,
    /// A number that may have decimals (seconds, Mbit/s).
    Num,
    Bool,
    List,
}

/// A question about one module field.
pub struct Field {
    pub key: &'static str,
    pub label: &'static str,
    pub help: &'static str,
    pub default: &'static str,
    pub kind: FieldKind,
    pub check: fn(&str) -> Result<(), String>,
}

pub fn fields(ty: ModType) -> Vec<Field> {
    use FieldKind::*;
    let f = |key, label, help, default, kind, check| Field {
        key,
        label,
        help,
        default,
        kind,
        check,
    };
    match ty {
        ModType::Folder => vec![
            f(
                "in",
                "Folder to send from (input box)",
                "Files dropped here on the input box are sent across, then moved to .dyode_sent.",
                "/srv/dyode/in",
                Text,
                check_abs_path,
            ),
            f(
                "out",
                "Folder to receive into (output box)",
                "Received files appear here once their signature is checked.",
                "/srv/dyode/out",
                Text,
                check_abs_path,
            ),
        ],
        ModType::Screen => vec![
            f(
                "in",
                "Screenshot folder (input box)",
                "Where the screenshot tool writes its images.",
                "/srv/dyode/screen",
                Text,
                check_abs_path,
            ),
            f(
                "out",
                "Screenshot folder (output box)",
                "Where the output box keeps the latest image for the web page.",
                "/srv/dyode/screen",
                Text,
                check_abs_path,
            ),
            f(
                "http_port",
                "Web page port (output box)",
                "The page that shows the stream, on the IT side.",
                "8080",
                Int,
                check_port,
            ),
        ],
        ModType::Modbus => vec![
            f(
                "ip",
                "PLC address",
                "The Modbus TCP device the input box polls.",
                "192.168.1.150",
                Text,
                check_ipv4,
            ),
            f(
                "port_out",
                "Modbus server port (output box)",
                "Clients on the IT side read the mirror here. Writes are always refused.",
                "502",
                Int,
                check_port,
            ),
            f(
                "registers",
                "Holding/input register ranges",
                "start-end with the end excluded, comma separated: 0-100 is addresses 0..99. Empty = none.",
                "0-100",
                List,
                check_ranges,
            ),
            f(
                "coils",
                "Coil / discrete input ranges",
                "Same form as the registers. Empty = none.",
                "",
                List,
                check_ranges,
            ),
        ],
        ModType::Logs => vec![
            f(
                "listen_udp",
                "Syslog over UDP (input box)",
                "A port (514) or address:port to bind one NIC. Empty = none.",
                "514",
                Text,
                check_hostport_opt,
            ),
            f(
                "tail",
                "Log files to follow (input box)",
                "Paths or globs, comma separated, followed like tail -F. Empty = none.",
                "",
                List,
                check_abs_list,
            ),
            f(
                "allow_from",
                "Accept syslog only from (input box)",
                "Networks, comma separated: 192.168.1.0/24. Empty = from anyone who can reach the port.",
                "",
                List,
                check_cidrs,
            ),
            f(
                "out",
                "Write received logs to (output box)",
                "A folder; received.log is written there. Empty = only forward.",
                "/var/log/dyode-ot",
                Text,
                check_abs_path_opt,
            ),
            f(
                "forward",
                "Forward to (output box)",
                "udp://host:514 or tcp://host:601, comma separated, e.g. a SIEM. Empty = none.",
                "",
                List,
                check_forwards,
            ),
        ],
    }
}

/// The options the main screens do not ask about, with DYODE's defaults.
/// Empty always means "the default"; `default` is what that is.
pub fn advanced(ty: ModType) -> Vec<Field> {
    use FieldKind::*;
    let f = |key, label, help, default, kind, check| Field {
        key,
        label,
        help,
        default,
        kind,
        check,
    };
    match ty {
        ModType::Folder => vec![
            f(
                "parallel_files",
                "Files at once (input box)",
                "How many files of a batch are sent at the same time, each as its own lidi transfer. More than 1 helps with many files on a multi-core box with several lidi ports. 1 to 16.",
                "1",
                Int,
                check_parallel,
            ),
            f(
                "settle",
                "Settle time, s (input box)",
                "A file must stay unchanged this long before it is sent, so half-written files are not.",
                "2",
                Num,
                check_nonneg,
            ),
            f(
                "rescan",
                "Rescan interval, s (input box)",
                "Seconds between full rescans of the folder, besides change notifications.",
                "30",
                Num,
                check_pos,
            ),
            f(
                "bitrate",
                "Bit rate cap, Mbit/s (input box)",
                "A cap for this module alone, under lidi.max_mbps. Empty = no cap of its own.",
                "none",
                Num,
                check_ge1,
            ),
            f(
                "keep_sent_hours",
                "Keep sent files, hours (input box)",
                "Sent files move to .dyode_sent and stay this long, because a diode gives no proof of delivery. Copy one back to re-send it. 0 = delete at once.",
                "24",
                Int,
                check_nonneg_int,
            ),
            f(
                "staging",
                "Staging folder (output box)",
                "Where files are written before their signature is checked. Must not be inside the output folder; keep it on the same filesystem so the final move is atomic.",
                "<out>.incoming",
                Text,
                check_abs_path_opt,
            ),
            f(
                "batch_timeout",
                "Batch timeout, s (output box)",
                "With nothing received for this long, an incomplete batch is closed and its missing files named in the log.",
                "300",
                Num,
                check_pos,
            ),
        ],
        ModType::Modbus => vec![
            f(
                "plc_port",
                "PLC port (input box)",
                "The Modbus TCP port of the PLC.",
                "502",
                Int,
                check_port,
            ),
            f(
                "unit",
                "Unit id (input box)",
                "The Modbus unit (slave) id to poll.",
                "1",
                Int,
                check_unit,
            ),
            f(
                "interval",
                "Poll interval, s (input box)",
                "Seconds between reads of the PLC.",
                "1.0",
                Num,
                check_pos,
            ),
            f(
                "stale_after",
                "Stale after, s (output box)",
                "With no update for this long, the mirror answers with an exception instead of old values.",
                "10",
                Num,
                check_pos,
            ),
            f(
                "bind_out",
                "Listen address (output box)",
                "The address the Modbus server listens on.",
                "0.0.0.0",
                Text,
                check_ipv4,
            ),
            f(
                "unmapped",
                "Outside the ranges (output box)",
                "exception: reads outside the ranges get Modbus exception 02. zero: they read as 0.",
                "exception",
                Text,
                check_unmapped,
            ),
        ],
        ModType::Screen => vec![
            f(
                "http_bind",
                "Web page address (output box)",
                "The address the page listens on. Default: the first address not on the diode card (the IT side).",
                "auto",
                Text,
                check_ipv4,
            ),
            f(
                "max_fps",
                "Frames per second (output box)",
                "The most images per second sent to each viewer.",
                "10",
                Num,
                check_pos,
            ),
            f(
                "max_streams",
                "Viewers at once (output box)",
                "How many browsers can watch at the same time.",
                "8",
                Int,
                check_pos_int,
            ),
        ],
        ModType::Logs => vec![
            f(
                "listen_tcp",
                "Syslog over TCP (input box)",
                "A port (601) or address:port; LF or octet-counted framing. Empty = no TCP listener.",
                "none",
                Text,
                check_hostport_opt,
            ),
            f(
                "tail_from",
                "Start new files from (input box)",
                "end: only lines written from now on. start: the whole file the first time it is seen.",
                "end",
                Text,
                check_tail_from,
            ),
            f(
                "tail_state",
                "Tail position file (input box)",
                "Where followed files' positions are kept across restarts.",
                "/var/lib/dyode/tail.json",
                Text,
                check_abs_path_opt,
            ),
            f(
                "copies",
                "Copies of each batch (input box)",
                "Every batch is sent again about 0.5 s later, so a lost lidi block must hit every copy. 1 to 4.",
                "2",
                Int,
                check_copies,
            ),
            f(
                "max_kbps",
                "Rate cap, kbit/s (input box)",
                "This module's share of the link.",
                "2000",
                Num,
                check_kbps,
            ),
            f(
                "heartbeat",
                "Heartbeat, s (input box)",
                "A small message sent this often when there are no logs, so the output box can tell silence from a cut link.",
                "10",
                Num,
                check_ge1,
            ),
            f(
                "stale_after",
                "Silence warning, s (output box)",
                "Warn when nothing, not even a heartbeat, arrives for this long.",
                "3 x heartbeat",
                Num,
                check_ge1,
            ),
            f(
                "format",
                "File format (output box)",
                "prefixed: time and source before each line. raw: the line as received. json: one object per line.",
                "prefixed",
                Text,
                check_format,
            ),
            f(
                "split_by_source",
                "One file per sender (output box)",
                "true: received.log is split into one file per sending host.",
                "false",
                Bool,
                check_bool,
            ),
        ],
    }
}

// --------------------------------------------------------------------------
// Checks: each returns the message the person sees.
// --------------------------------------------------------------------------

pub fn check_ipv4(s: &str) -> Result<(), String> {
    s.parse::<Ipv4Addr>()
        .map(|_| ())
        .map_err(|_| "an IPv4 address, like 10.0.1.1".into())
}

pub fn check_mac(s: &str) -> Result<(), String> {
    let parts: Vec<&str> = s.split(':').collect();
    let ok = parts.len() == 6
        && parts
            .iter()
            .all(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_hexdigit()));
    if !ok {
        return Err("six hex pairs, like b8:27:eb:b1:ff:ab".into());
    }
    if parts.iter().all(|p| *p == "00") {
        return Err("00:00:00:00:00:00 is not a real MAC".into());
    }
    if u8::from_str_radix(parts[0], 16).is_ok_and(|b| b & 1 == 1) {
        return Err("that is a multicast address, not a NIC".into());
    }
    Ok(())
}

pub fn check_mac_opt(s: &str) -> Result<(), String> {
    if s.is_empty() { Ok(()) } else { check_mac(s) }
}

pub fn check_port(s: &str) -> Result<(), String> {
    match s.parse::<u32>() {
        Ok(p) if (1..=65535).contains(&p) => Ok(()),
        _ => Err("a port number, 1 to 65535".into()),
    }
}

pub fn check_abs_path(s: &str) -> Result<(), String> {
    if s.starts_with('/') && !s.contains('\0') {
        Ok(())
    } else {
        Err("a full path, starting with /".into())
    }
}

fn check_abs_path_opt(s: &str) -> Result<(), String> {
    if s.is_empty() {
        Ok(())
    } else {
        check_abs_path(s)
    }
}

fn check_abs_list(s: &str) -> Result<(), String> {
    split_list(s)
        .iter()
        .try_for_each(|p| check_abs_path(p).map_err(|e| format!("{p}: {e}")))
}

fn check_ranges(s: &str) -> Result<(), String> {
    split_list(s)
        .iter()
        .try_for_each(|r| config::parse_range(r).map(|_| ()).map_err(|e| e.0))
}

fn check_cidrs(s: &str) -> Result<(), String> {
    split_list(s).iter().try_for_each(|c| {
        Cidr::parse(c)
            .map(|_| ())
            .ok_or_else(|| format!("{c}: a network like 192.168.1.0/24"))
    })
}

fn check_forwards(s: &str) -> Result<(), String> {
    split_list(s)
        .iter()
        .try_for_each(|f| config::parse_forward(f).map(|_| ()))
}

fn check_hostport_opt(s: &str) -> Result<(), String> {
    if s.is_empty() {
        return Ok(());
    }
    match config::parse_hostport(s, "0.0.0.0")? {
        (_, 0) => Err("the port cannot be 0".into()),
        _ => Ok(()),
    }
}

fn check_number(s: &str, ok: impl Fn(f64) -> bool, what: &str) -> Result<(), String> {
    match s.parse::<f64>() {
        Ok(v) if v.is_finite() && ok(v) => Ok(()),
        _ => Err(what.into()),
    }
}
fn check_nonneg(s: &str) -> Result<(), String> {
    check_number(s, |v| v >= 0.0, "a number, 0 or more")
}
fn check_pos(s: &str) -> Result<(), String> {
    check_number(s, |v| v > 0.0, "a number above 0")
}
fn check_ge1(s: &str) -> Result<(), String> {
    check_number(s, |v| v >= 1.0, "a number, 1 or more")
}
fn check_kbps(s: &str) -> Result<(), String> {
    check_number(s, |v| v >= 8.0, "a number of kbit/s, 8 or more")
}
fn check_nonneg_int(s: &str) -> Result<(), String> {
    s.parse::<u64>()
        .map(|_| ())
        .map_err(|_| "a whole number, 0 or more".into())
}
fn check_pos_int(s: &str) -> Result<(), String> {
    match s.parse::<u64>() {
        Ok(n) if n > 0 => Ok(()),
        _ => Err("a whole number above 0".into()),
    }
}
fn check_unit(s: &str) -> Result<(), String> {
    match s.parse::<u8>() {
        Ok(_) => Ok(()),
        _ => Err("a unit id, 0 to 255".into()),
    }
}
fn check_parallel(s: &str) -> Result<(), String> {
    match s.parse::<u8>() {
        Ok(1..=16) => Ok(()),
        _ => Err("1 to 16".into()),
    }
}
fn check_copies(s: &str) -> Result<(), String> {
    match s.parse::<u8>() {
        Ok(1..=4) => Ok(()),
        _ => Err("1, 2, 3 or 4".into()),
    }
}
fn one_of(s: &str, allowed: &[&str]) -> Result<(), String> {
    if allowed.iter().any(|a| a.eq_ignore_ascii_case(s)) {
        Ok(())
    } else {
        Err(format!("one of: {}", allowed.join(", ")))
    }
}
fn check_unmapped(s: &str) -> Result<(), String> {
    one_of(s, &["exception", "zero"])
}
fn check_format(s: &str) -> Result<(), String> {
    one_of(s, &["prefixed", "raw", "json"])
}
fn check_tail_from(s: &str) -> Result<(), String> {
    one_of(s, &["end", "start"])
}
fn check_bool(s: &str) -> Result<(), String> {
    one_of(s, &["true", "false", "yes", "no"])
}

pub fn check_ports(s: &str) -> Result<(), String> {
    let list = split_list(s);
    if list.is_empty() {
        return Err("at least one port".into());
    }
    let mut seen = Vec::new();
    for p in &list {
        check_port(p).map_err(|e| format!("{p}: {e}"))?;
        if seen.contains(p) {
            return Err(format!("{p} is listed twice"));
        }
        seen.push(p.clone());
    }
    Ok(())
}

pub fn check_mbps(s: &str) -> Result<(), String> {
    match s.parse::<f64>() {
        Ok(v) if v >= 0.0 && v.is_finite() => Ok(()),
        _ => Err("a number of Mbit/s; 0 = no cap".into()),
    }
}

pub fn check_name(s: &str) -> Result<(), String> {
    if s.trim().is_empty() {
        Err("a name is needed".into())
    } else {
        Ok(())
    }
}

// --------------------------------------------------------------------------
// This machine
// --------------------------------------------------------------------------

/// Facts about the machine.  A fake one drives the tests and screenshots.
pub struct Env {
    pub interfaces: Vec<Interface>,
    /// The NIC that carries the default route: almost never the diode NIC.
    pub default_route: Option<String>,
    pub cpus: usize,
    pub etc: PathBuf,
    pub serial_devices: Vec<String>,
    /// Write the systemd unit too (false when install.sh does it).
    pub write_unit: bool,
    pub binary: PathBuf,
    pub dry_run: bool,
}

impl Env {
    pub fn real(etc: PathBuf, dry_run: bool) -> Self {
        Self {
            interfaces: crate::setup::list_interfaces(Path::new("/sys/class/net"), false),
            default_route: default_route_iface(
                &std::fs::read_to_string("/proc/net/route").unwrap_or_default(),
            ),
            cpus: std::thread::available_parallelism().map_or(1, usize::from),
            etc,
            serial_devices: serial_devices(Path::new("/dev")),
            write_unit: std::env::var_os("DYODE_INSTALLER").is_none(),
            binary: std::env::current_exe().unwrap_or_else(|_| "/usr/local/bin/dyode".into()),
            dry_run,
        }
    }

    pub fn iface(&self, name: &str) -> Option<&Interface> {
        self.interfaces.iter().find(|i| i.name == name)
    }

    pub fn config_path(&self) -> PathBuf {
        self.etc.join("config.yaml")
    }
}

/// The interface of the 0.0.0.0/0 route in /proc/net/route.
pub fn default_route_iface(route: &str) -> Option<String> {
    route.lines().skip(1).find_map(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        (f.len() > 7 && f[1] == "00000000" && f[7] == "00000000").then(|| f[0].to_string())
    })
}

fn serial_devices(dev: &Path) -> Vec<String> {
    let mut out: Vec<String> = ["serial0", "serial1", "ttyAMA0", "ttyS0", "ttyS1"]
        .iter()
        .filter(|n| dev.join(n).exists())
        .map(|n| format!("/dev/{n}"))
        .collect();
    if let Ok(rd) = std::fs::read_dir(dev) {
        let mut usb: Vec<String> = rd
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("ttyUSB") || n.starts_with("ttyACM"))
            .map(|n| format!("/dev/{n}"))
            .collect();
        usb.sort();
        out.extend(usb);
    }
    out
}

/// How many lidi ports to suggest: one RaptorQ thread per port on each
/// box, leaving a core for the modules; never more than 4.
pub fn suggested_ports(cpus: usize) -> usize {
    cpus.saturating_sub(1).clamp(1, 4)
}

pub fn port_list(first: u16, n: usize) -> String {
    (0..n)
        .map(|i| (u32::from(first) + i as u32).to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

// --------------------------------------------------------------------------
// The draft
// --------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyPlan {
    /// A key is already beside config.yaml.
    Keep,
    /// Copy the key that sat beside the imported config.
    Copy,
    /// Create a new one now (then copy it to the other box).
    Create,
    /// Put it there later.
    Later,
}

#[derive(Clone, Debug)]
pub struct Draft {
    pub variant: Variant,
    pub side: Side,
    pub name: String,
    pub in_ip: String,
    pub out_ip: String,
    pub in_iface: String,
    pub out_iface: String,
    pub out_mac: String,
    pub ports: String,
    pub max_mbps: String,
    pub serial_device: String,
    pub baudrate: String,
    pub modules: Vec<ModDraft>,
    pub key: KeyPlan,
    pub write_unit: bool,
    /// This box's API address ("10.20.0.5:8443"); empty = no API.
    pub api_listen: String,
    /// api.in.listen / api.out.listen as imported.
    pub api_imported: Vec<(String, String)>,
    /// The imported config.yaml, as read: kept underneath the answers.
    pub base: Mapping,
    pub imported_from: Option<PathBuf>,
}

impl Draft {
    pub fn new() -> Self {
        Self {
            variant: Variant::V1,
            side: Side::In,
            name: "DYODE".into(),
            in_ip: "10.0.1.1".into(),
            out_ip: "10.0.1.2".into(),
            in_iface: "eth0".into(),
            out_iface: "eth1".into(),
            out_mac: String::new(),
            ports: "5000".into(),
            max_mbps: "100".into(),
            serial_device: "/dev/serial0".into(),
            baudrate: "57600".into(),
            modules: Vec::new(),
            key: KeyPlan::Later,
            write_unit: true,
            api_listen: String::new(),
            api_imported: Vec::new(),
            base: Mapping::new(),
            imported_from: None,
        }
    }

    /// Start from an existing config.yaml.
    pub fn import(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let base_dir = path.parent().unwrap_or(Path::new("."));
        // Validate first, so we never import something DYODE would refuse.
        config::parse(&text, path, base_dir).map_err(|e| e.0)?;
        let Ok(Value::Mapping(raw)) = serde_yaml::from_str::<Value>(&text) else {
            return Err("top level must be a mapping".into());
        };
        let mut d = Self::new();
        let sect = |k: &str| {
            raw.get(k)
                .and_then(Value::as_mapping)
                .cloned()
                .unwrap_or_default()
        };
        let s = |m: &Mapping, k: &str| m.get(k).map(scalar_text).unwrap_or_default();
        let (din, dout, lidi, ser) = (
            sect("dyode_in"),
            sect("dyode_out"),
            sect("lidi"),
            sect("serial"),
        );
        d.name = or(&s(&raw, "config_name"), "DYODE");
        d.in_ip = or(&s(&din, "ip"), "10.0.1.1");
        d.in_iface = or(&s(&din, "interface"), "eth0");
        d.out_ip = or(&s(&dout, "ip"), "10.0.1.2");
        d.out_iface = or(&s(&dout, "interface"), "eth1");
        d.out_mac = s(&dout, "mac");
        if let Some(Value::Sequence(p)) = lidi.get("ports") {
            d.ports = p.iter().map(scalar_text).collect::<Vec<_>>().join(", ");
        } else if lidi.get("ports").is_some() {
            d.ports = s(&lidi, "ports");
        }
        if lidi.get("max_mbps").is_some() {
            d.max_mbps = s(&lidi, "max_mbps");
        }
        if ser.get("device").is_some() {
            d.serial_device = s(&ser, "device");
            d.variant = Variant::V2;
        }
        if ser.get("baudrate").is_some() {
            d.baudrate = s(&ser, "baudrate");
        }
        if let Some(Value::Mapping(mods)) = raw.get("modules") {
            for (name, props) in mods {
                let map = props.as_mapping().cloned().unwrap_or_default();
                let ty = map
                    .get("type")
                    .map(scalar_text)
                    .and_then(|t| ModType::from_name(&t))
                    .unwrap_or(ModType::Folder);
                d.modules.push(ModDraft {
                    name: scalar_text(name),
                    ty,
                    map,
                });
            }
        }
        if let Some(Value::Mapping(api)) = raw.get("api") {
            // Remembered for both sides; the wizard shows the one for this box.
            d.api_imported = ["in", "out"]
                .iter()
                .filter_map(|s| {
                    api.get(*s)
                        .and_then(Value::as_mapping)
                        .and_then(|m| m.get("listen"))
                        .map(|v| ((*s).to_string(), scalar_text(v)))
                })
                .collect();
        }
        d.base = raw;
        d.imported_from = Some(path.to_path_buf());
        Ok(d)
    }

    /// This box's interface name, and the other box's.
    pub fn my_iface(&self) -> &str {
        if self.side == Side::In {
            &self.in_iface
        } else {
            &self.out_iface
        }
    }
    pub fn my_ip(&self) -> &str {
        if self.side == Side::In {
            &self.in_ip
        } else {
            &self.out_ip
        }
    }
    pub fn peer_ip(&self) -> &str {
        if self.side == Side::In {
            &self.out_ip
        } else {
            &self.in_ip
        }
    }
    pub fn set_my_iface(&mut self, v: String) {
        if self.side == Side::In {
            self.in_iface = v
        } else {
            self.out_iface = v
        }
    }
    pub fn set_my_ip(&mut self, v: String) {
        if self.side == Side::In {
            self.in_ip = v
        } else {
            self.out_ip = v
        }
    }
    pub fn set_peer_ip(&mut self, v: String) {
        if self.side == Side::In {
            self.out_ip = v
        } else {
            self.in_ip = v
        }
    }
    pub fn set_peer_iface(&mut self, v: String) {
        if self.side == Side::In {
            self.out_iface = v
        } else {
            self.in_iface = v
        }
    }
    pub fn peer_iface(&self) -> &str {
        if self.side == Side::In {
            &self.out_iface
        } else {
            &self.in_iface
        }
    }

    pub fn module_name_free(&self, name: &str, except: Option<usize>) -> bool {
        !self
            .modules
            .iter()
            .enumerate()
            .any(|(i, m)| Some(i) != except && m.name == name)
    }

    /// The config.yaml text.
    pub fn to_yaml(&self) -> String {
        let mut doc = self.base.clone();
        let set = |m: &mut Mapping, k: &str, v: Value| {
            m.insert(k.into(), v);
        };
        let num_or_text = |t: &str| -> Value {
            if let Ok(n) = t.parse::<u64>() {
                Value::from(n)
            } else if let Ok(f) = t.parse::<f64>() {
                Value::from(f)
            } else {
                Value::String(t.into())
            }
        };
        set(&mut doc, "config_name", self.name.clone().into());
        if !doc.contains_key("config_version") {
            set(&mut doc, "config_version", "1.0".into());
        }
        let today = crate::timefmt::Date::today_utc();
        set(
            &mut doc,
            "config_date",
            format!("{:04}-{:02}-{:02}", today.year, today.month, today.day).into(),
        );
        let sub = |doc: &mut Mapping, k: &str| -> Mapping {
            let m = doc
                .get(k)
                .and_then(Value::as_mapping)
                .cloned()
                .unwrap_or_default();
            doc.remove(k);
            m
        };
        let mut din = sub(&mut doc, "dyode_in");
        set(&mut din, "ip", self.in_ip.clone().into());
        set(&mut din, "interface", self.in_iface.clone().into());
        let mut dout = sub(&mut doc, "dyode_out");
        set(&mut dout, "ip", self.out_ip.clone().into());
        if self.out_mac.is_empty() {
            dout.remove("mac");
        } else {
            set(&mut dout, "mac", self.out_mac.to_lowercase().into());
        }
        set(&mut dout, "interface", self.out_iface.clone().into());
        let mut lidi = sub(&mut doc, "lidi");
        let ports: Vec<Value> = split_list(&self.ports)
            .iter()
            .map(|p| num_or_text(p))
            .collect();
        set(&mut lidi, "ports", Value::Sequence(ports));
        set(&mut lidi, "max_mbps", num_or_text(&self.max_mbps));
        let mut serial = sub(&mut doc, "serial");
        let logging = sub(&mut doc, "logging");
        let auth = sub(&mut doc, "auth");
        let mut api = sub(&mut doc, "api");
        if self.variant == Variant::V1 {
            let side = self.side.as_str();
            if self.api_listen.is_empty() {
                api.remove(side);
            } else {
                // Keep options the wizard does not ask about (tokens, max_clients).
                let mut mine = api
                    .get(side)
                    .and_then(Value::as_mapping)
                    .cloned()
                    .unwrap_or_default();
                set(&mut mine, "listen", self.api_listen.clone().into());
                if !mine.contains_key("tls") {
                    let mut tls = Mapping::new();
                    set(&mut tls, "cert", "api.crt".into());
                    set(&mut tls, "key", "api.key".into());
                    set(&mut mine, "tls", tls.into());
                }
                api.insert(side.into(), mine.into());
            }
        }
        let _ = sub(&mut doc, "modules");
        // Fixed order, the same as config.example.yaml; anything else the
        // imported file had stays after it.
        let rest = std::mem::take(&mut doc);
        let mut out = Mapping::new();
        for k in ["config_name", "config_version", "config_date"] {
            if let Some(v) = rest.get(k) {
                out.insert(k.into(), v.clone());
            }
        }
        out.insert("dyode_in".into(), din.into());
        out.insert("dyode_out".into(), dout.into());
        if self.variant == Variant::V2 {
            set(&mut serial, "device", self.serial_device.clone().into());
            set(&mut serial, "baudrate", num_or_text(&self.baudrate));
        }
        if !serial.is_empty() {
            out.insert("serial".into(), serial.into());
        }
        out.insert("lidi".into(), lidi.into());
        if !logging.is_empty() {
            out.insert("logging".into(), logging.into());
        }
        if !auth.is_empty() {
            out.insert("auth".into(), auth.into());
        }
        if !api.is_empty() {
            out.insert("api".into(), api.into());
        }
        for (k, v) in rest {
            if !out.contains_key(&k) {
                out.insert(k, v);
            }
        }
        let mut mods = Mapping::new();
        for m in &self.modules {
            let mut map = m.map.clone();
            map.insert("type".into(), m.ty.name().into());
            mods.insert(m.name.clone().into(), map.into());
        }
        out.insert("modules".into(), mods.into());
        let body = serde_yaml::to_string(&Value::Mapping(out)).unwrap_or_default();
        format!(
            "# DYODE configuration, written by `dyode setup` ({}).\n\
             # Copy this SAME file to the other box, beside its dyode.key.\n\
             # Every option is described in config.example.yaml.\n{body}",
            crate::timefmt::utc_now()
        )
    }

    /// The parsed result, or what DYODE would refuse.
    pub fn validate(&self, env: &Env) -> Result<config::Config, String> {
        config::parse(&self.to_yaml(), &env.config_path(), &env.etc).map_err(|e| e.0)
    }

    pub fn key_path(&self, env: &Env) -> PathBuf {
        let rel = self
            .base
            .get("auth")
            .and_then(Value::as_mapping)
            .and_then(|a| a.get("key_file"))
            .map(scalar_text);
        let p = PathBuf::from(rel.unwrap_or_else(|| "dyode.key".into()));
        if p.is_absolute() { p } else { env.etc.join(p) }
    }

    /// The key beside the imported config, when it is somewhere else.
    pub fn import_key(&self, env: &Env) -> Option<PathBuf> {
        let from = self.imported_from.as_ref()?.parent()?.join("dyode.key");
        (from.is_file() && from != self.key_path(env)).then_some(from)
    }

    /// Things that will probably go wrong, in plain words.
    pub fn warnings(&self, env: &Env) -> Vec<String> {
        let mut w = Vec::new();
        if self.variant == Variant::V1 {
            let me = self.my_iface();
            match env.iface(me) {
                None => w.push(format!(
                    "{me} does not exist on this box (it may be renamed or not plugged in yet)."
                )),
                Some(i) => {
                    if !i.carrier {
                        w.push(format!("{me} has no link: check the cable to the diode."));
                    }
                    if let Some(ip) = &i.ipv4 {
                        if ip.split('/').next() != Some(self.my_ip()) {
                            w.push(format!(
                                "{me} has address {ip}, but this box is {} in the config: set {} on {me} in the network settings.",
                                self.my_ip(),
                                self.my_ip()
                            ));
                        }
                    } else {
                        w.push(format!(
                            "{me} has no IPv4 address yet: give it {} in the network settings.",
                            self.my_ip()
                        ));
                    }
                }
            }
            if env.default_route.as_deref() == Some(me) {
                w.push(format!("{me} carries this box's default route: it is probably the IT/OT network, not the diode."));
            }
            if self.out_mac.is_empty() {
                w.push("dyode_out.mac is not set: the input box cannot address the output box through the diode (no ARP replies come back).".into());
            }
            let was = self
                .base
                .get("dyode_out")
                .and_then(Value::as_mapping)
                .and_then(|m| m.get("mac"))
                .map(scalar_text);
            if let Some(was) =
                was.filter(|w| !w.is_empty() && !w.eq_ignore_ascii_case(&self.out_mac))
            {
                w.push(format!(
                    "dyode_out.mac changes from {was} (imported) to {}: copy this config.yaml back to the input box.",
                    if self.out_mac.is_empty() { "nothing" } else { &self.out_mac }
                ));
            }
            if self.in_ip == self.out_ip {
                w.push("Both boxes have the same IP address.".into());
            }
            let n = split_list(&self.ports).len();
            if n > suggested_ports(env.cpus) {
                w.push(format!(
                    "{n} lidi ports on a {}-CPU box: each port needs a core on BOTH boxes.",
                    env.cpus
                ));
            }
            if self
                .max_mbps
                .parse::<f64>()
                .is_ok_and(|v| v == 0.0 || v > 400.0)
            {
                w.push(
                    "max_mbps is uncapped or above 400: measure the link first (CAVEATS.md)."
                        .into(),
                );
            }
            match self.key {
                KeyPlan::Later => w.push(format!("No key yet: DYODE will not start until {} is in place.", self.key_path(env).display())),
                KeyPlan::Create => w.push("A new key is created: copy it to the other box, or the boxes refuse each other.".into()),
                _ => {}
            }
        }
        for m in &self.modules {
            if m.ty == ModType::Logs
                && m.get("allow_from").is_empty()
                && !m.get("listen_udp").is_empty()
            {
                w.push(format!("Module {:?} accepts syslog from anyone who can reach the input box (allow_from is empty).", m.name));
            }
            if self.variant == Variant::V2 && m.ty != ModType::Modbus {
                w.push(format!(
                    "Module {:?} is a {} module: DYODE v2 (serial) only carries Modbus.",
                    m.name,
                    m.ty.name()
                ));
            }
        }
        w
    }

    /// The changes writing makes, in order, for the review.
    pub fn plan(&self, env: &Env) -> Vec<String> {
        let cfg = env.config_path();
        let mut p = Vec::new();
        if cfg.exists() {
            p.push(format!(
                "back up {} to config.yaml.bak-<time>",
                cfg.display()
            ));
        }
        p.push(format!("write {}", cfg.display()));
        if self.variant == Variant::V1 {
            let key = self.key_path(env);
            match self.key {
                KeyPlan::Keep => p.push(format!("keep {}", key.display())),
                KeyPlan::Copy => p.push(format!(
                    "copy {} to {}",
                    self.import_key(env)
                        .map(|k| k.display().to_string())
                        .unwrap_or_default(),
                    key.display()
                )),
                KeyPlan::Create => p.push(format!("create a new key {}", key.display())),
                KeyPlan::Later => {}
            }
        }
        if self.variant == Variant::V1 && !self.api_listen.is_empty() {
            if !env.etc.join("api.crt").exists() {
                p.push(format!(
                    "create a TLS certificate {} for the API",
                    env.etc.join("api.crt").display()
                ));
            }
            if crate::apitoken::list(&env.etc.join("api-tokens")).map_or(true, |t| t.is_empty()) {
                p.push("create an API token \"monitoring\" (shown once, at the end)".into());
            }
        }
        if self.write_unit && env.write_unit {
            p.push(format!(
                "write /etc/systemd/system/{}",
                crate::setup::unit_name(self.side)
            ));
        }
        p
    }
}

impl Default for Draft {
    fn default() -> Self {
        Self::new()
    }
}

// --------------------------------------------------------------------------
// Writing
// --------------------------------------------------------------------------

fn write_atomic(path: &Path, text: &str, mode: u32) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension("tmp-setup");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(&tmp)?;
    f.write_all(text.as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)
}

fn backup(path: &Path) -> std::io::Result<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let dest = path.with_file_name(format!(
        "{name}.bak-{}",
        crate::timefmt::local_compact_stamp()
    ));
    std::fs::copy(path, &dest)?;
    Ok(Some(dest))
}

/// Carry out the plan.  Returns what was done, line by line.
pub fn apply(d: &Draft, env: &Env) -> Result<Vec<String>, String> {
    let mut done = Vec::new();
    let yaml = d.to_yaml();
    d.validate(env)?;
    if env.dry_run {
        done.push("dry run: nothing written".into());
        done.extend(d.plan(env).into_iter().map(|l| format!("would {l}")));
        return Ok(done);
    }
    std::fs::create_dir_all(&env.etc)
        .map_err(|e| format!("cannot create {}: {e}", env.etc.display()))?;
    let cfg = env.config_path();
    if let Some(b) = backup(&cfg).map_err(|e| format!("cannot back up {}: {e}", cfg.display()))? {
        done.push(format!("backed up the old config to {}", b.display()));
    }
    write_atomic(&cfg, &yaml, 0o640).map_err(|e| format!("cannot write {}: {e}", cfg.display()))?;
    done.push(format!("wrote {}", cfg.display()));
    if d.variant == Variant::V1 {
        let key = d.key_path(env);
        match d.key {
            KeyPlan::Copy => {
                let src = d.import_key(env).ok_or("the key to copy has gone")?;
                crate::setup::install_key_copy(&src, &key)?;
                done.push(format!("copied the key to {}", key.display()));
            }
            KeyPlan::Create => {
                crate::auth::generate_key_file(&key)
                    .map_err(|e| format!("cannot create {}: {e}", key.display()))?;
                done.push(format!("created {}", key.display()));
            }
            KeyPlan::Keep | KeyPlan::Later => {}
        }
    }
    if d.variant == Variant::V1 && !d.api_listen.is_empty() {
        if !env.etc.join("api.crt").exists() {
            let ip = d
                .api_listen
                .rsplit_once(':')
                .map(|(h, _)| h.trim_matches(['[', ']']).to_string());
            let host = std::fs::read_to_string("/etc/hostname")
                .map(|s| s.trim().to_string())
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| format!("dyode-{}", d.side.as_str()));
            let c = crate::setup::make_api_cert(
                &env.etc,
                &host,
                &ip.into_iter().collect::<Vec<_>>(),
                825,
                false,
            )?;
            done.push(format!("created {} ({})", c.cert.display(), c.fingerprint));
        }
        let tokens = env.etc.join("api-tokens");
        if crate::apitoken::list(&tokens).map_or(true, |t| t.is_empty()) {
            let t = crate::apitoken::add(&tokens, "monitoring", &["read".into()])?;
            done.push(format!(
                "API token \"monitoring\" (read), shown only now: {t}"
            ));
        }
    }
    if d.write_unit && env.write_unit {
        let cmd = match (d.variant, d.side) {
            (Variant::V1, Side::In) => "in",
            (Variant::V1, Side::Out) => "out",
            (Variant::V2, Side::In) => "serial-in",
            (Variant::V2, Side::Out) => "serial-out",
        };
        let iface = (d.variant == Variant::V1).then(|| d.my_iface().to_string());
        let text =
            crate::setup::systemd_unit_text(d.side, cmd, &env.etc, &env.binary, iface.as_deref());
        let unit = Path::new("/etc/systemd/system").join(crate::setup::unit_name(d.side));
        if let Ok(Some(b)) = backup(&unit) {
            done.push(format!("backed up the old unit to {}", b.display()));
        }
        write_atomic(&unit, &text, 0o644)
            .map_err(|e| format!("cannot write {}: {e}", unit.display()))?;
        done.push(format!("wrote {}", unit.display()));
    }
    Ok(done)
}

/// What to do after writing, as commands.
pub fn next_steps(d: &Draft, env: &Env) -> Vec<String> {
    let cfg = env.config_path();
    let side = d.side.as_str();
    let mut s = vec![format!(
        "Copy {} to the other box (same path).",
        cfg.display()
    )];
    if d.variant == Variant::V1 {
        match d.key {
            KeyPlan::Create => s.push(format!(
                "Copy {} to the other box too (chmod 600).",
                d.key_path(env).display()
            )),
            KeyPlan::Later => s.push(format!(
                "Put the shared key at {} (create it once, on ONE box: sudo dyode new-key -c {}).",
                d.key_path(env).display(),
                cfg.display()
            )),
            _ => {}
        }
    }
    let serial = if d.variant == Variant::V2 {
        " --serial"
    } else {
        ""
    };
    s.push(format!(
        "Check:   sudo dyode check -c {} --side {side}{serial}",
        cfg.display()
    ));
    if env.write_unit {
        s.push(format!("Start:   sudo systemctl daemon-reload && sudo systemctl enable --now dyode-{side}.service"));
        s.push(format!("Watch:   journalctl -u dyode-{side} -f"));
        if d.side == Side::Out && d.variant == Variant::V1 {
            s.push("Firewall: sudo ./install.sh --side out --firewall-only".into());
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn fake_env(etc: &Path) -> Env {
        Env {
            interfaces: vec![],
            default_route: None,
            cpus: 4,
            etc: etc.to_path_buf(),
            serial_devices: vec![],
            write_unit: false,
            binary: "/usr/local/bin/dyode".into(),
            dry_run: false,
        }
    }

    #[test]
    fn checks() {
        assert!(check_mac("b8:27:eb:b1:ff:ab").is_ok());
        assert!(check_mac("00:00:00:00:00:00").is_err());
        assert!(check_mac("01:00:5e:00:00:01").is_err());
        assert!(check_mac("b8:27:eb:b1:ff").is_err());
        assert!(check_ports("5000, 5001").is_ok());
        assert!(check_ports("5000, 5000").is_err());
        assert!(check_ranges("0-100, 400-450").is_ok());
        assert!(check_ranges("100-0").is_err());
        assert!(check_forwards("udp://10.0.0.5:514").is_ok());
        assert!(check_cidrs("192.168.1.0/24, 10.0.0.1").is_ok());
        assert!(check_cidrs("nope").is_err());
    }

    #[test]
    fn advanced_defaults_are_valid_and_accepted() {
        // Every advanced field, set to its default (where that is a value),
        // gives a config DYODE accepts.
        let dir = crate::testutil::TempDir::new();
        let env = fake_env(dir.path());
        for ty in ModType::ALL {
            let mut d = Draft::new();
            d.out_mac = "b8:27:eb:b1:ff:ab".into();
            let mut m = ModDraft::new("M", ty);
            for f in fields(ty) {
                m.set(f.key, f.default, f.kind);
            }
            for f in advanced(ty) {
                if (f.check)(f.default).is_ok() {
                    m.set(f.key, f.default, f.kind);
                }
            }
            if ty == ModType::Screen {
                m.set("http_bind", "192.168.50.4", FieldKind::Text);
            }
            d.modules.push(m);
            d.validate(&env)
                .unwrap_or_else(|e| panic!("{ty:?}: {e}\n{}", d.to_yaml()));
        }
    }

    #[test]
    fn route() {
        let r = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\n\
                 eth1\t0001000A\t00000000\t0001\t0\t0\t0\t00FFFFFF\n\
                 wlan0\t00000000\t0101A8C0\t0003\t0\t0\t600\t00000000\n";
        assert_eq!(default_route_iface(r).as_deref(), Some("wlan0"));
    }

    #[test]
    fn new_config_round_trips() {
        let dir = crate::testutil::TempDir::new();
        let env = fake_env(dir.path());
        let mut d = Draft::new();
        d.out_mac = "B8:27:EB:B1:FF:AB".into();
        d.ports = "5000, 5001".into();
        let mut m = ModDraft::new("Files", ModType::Folder);
        m.set("in", "/srv/in", FieldKind::Text);
        m.set("out", "/srv/out", FieldKind::Text);
        d.modules.push(m);
        let mut m = ModDraft::new("PLC", ModType::Modbus);
        m.set("ip", "192.168.1.150", FieldKind::Text);
        m.set("port_out", "502", FieldKind::Int);
        m.set("registers", "0-100, 400-450", FieldKind::List);
        d.modules.push(m);
        let cfg = d.validate(&env).expect("valid");
        assert_eq!(cfg.lidi.ports, vec![5000, 5001]);
        assert_eq!(cfg.network.out_mac.as_deref(), Some("b8:27:eb:b1:ff:ab"));
        assert_eq!(cfg.modules.len(), 2);
        assert_eq!(cfg.modules[1].kind.type_name(), "modbus");
    }

    #[test]
    fn import_keeps_unasked_options() {
        let dir = crate::testutil::TempDir::new();
        let src = dir.path().join("old.yaml");
        std::fs::write(
            &src,
            "config_name: Old\ndyode_in: {ip: 10.0.1.1, interface: enp1s0, arp_interval: 30}\n\
             dyode_out: {ip: 10.0.1.2, mac: 'b8:27:eb:b1:ff:ab'}\n\
             logging: {dir: /var/log/x}\n\
             modules:\n  A: {type: folder, in: /a, out: /b, settle: 5}\n  B: {type: Modbus, ip: 1.2.3.4, port_out: 502, registers: [0-10]}\n",
        )
        .unwrap();
        let mut d = Draft::import(&src).unwrap();
        assert_eq!(d.in_iface, "enp1s0");
        assert_eq!(d.modules[1].ty, ModType::Modbus);
        d.name = "New".into();
        let y = d.to_yaml();
        assert!(y.contains("arp_interval: 30"), "{y}");
        assert!(y.contains("settle: 5"), "{y}");
        assert!(y.contains("/var/log/x"), "{y}");
        let env = fake_env(dir.path());
        let cfg = d.validate(&env).unwrap();
        assert_eq!(cfg.config_name.as_deref(), Some("New"));
        assert_eq!(cfg.modules[0].name, "A");
    }

    #[test]
    fn apply_backs_up_and_creates_key() {
        let dir = crate::testutil::TempDir::new();
        let env = fake_env(dir.path());
        std::fs::write(env.config_path(), "old").unwrap();
        let mut d = Draft::new();
        d.out_mac = "b8:27:eb:b1:ff:ab".into();
        let mut m = ModDraft::new("Files", ModType::Folder);
        m.set("in", "/srv/in", FieldKind::Text);
        m.set("out", "/srv/out", FieldKind::Text);
        d.modules.push(m);
        d.key = KeyPlan::Create;
        let done = apply(&d, &env).unwrap();
        assert!(done.iter().any(|l| l.contains("backed up")), "{done:?}");
        assert!(dir.path().join("dyode.key").is_file());
        assert!(crate::config::load(&env.config_path()).is_ok());
    }
}
