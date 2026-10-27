//! config.yaml: loading and validation.
//!
//! The SAME file is copied to both boxes.  Everything the Python version
//! accepted is still accepted, with the same defaults and the same checks;
//! keys that only meant something to udpcast (`fec`, `autostart`,
//! `start_timeout`, `file_gap`) are read and reported as ignored, so an
//! existing config.yaml keeps working unchanged.
//!
//! New in the Rust version: a `lidi:` section with the transport
//! parameters.  They must be identical on both boxes, which they are when
//! the file is.

use serde_yaml::{Mapping, Value};
use std::fmt;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::path::{Component, Path, PathBuf};

#[derive(Debug)]
pub struct ConfigError(pub String);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

macro_rules! bail {
    ($($t:tt)*) => { return Err(ConfigError(format!($($t)*))) };
}

pub type Result<T> = std::result::Result<T, ConfigError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    In,
    Out,
}

impl Side {
    pub fn as_str(self) -> &'static str {
        match self {
            Side::In => "in",
            Side::Out => "out",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Network {
    pub in_ip: Ipv4Addr,
    pub out_ip: Ipv4Addr,
    pub in_interface: String,
    pub out_interface: String,
    pub out_mac: Option<String>,
    /// Seconds between re-asserting the static ARP entry.
    pub arp_interval: f64,
}

#[derive(Clone, Debug)]
pub struct SerialConfig {
    pub device: String,
    pub baudrate: u32,
}

#[derive(Clone, Debug)]
pub struct LoggingConfig {
    /// None = stderr only.
    pub dir: Option<PathBuf>,
    pub human: String,
    pub json: String,
    pub per_file_events: bool,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            dir: Some(PathBuf::from("/var/log/dyode-transfer")),
            human: "dyode.log".into(),
            json: "transfer.jsonl".into(),
            per_file_events: true,
        }
    }
}

#[derive(Clone, Debug)]
pub struct AuthConfig {
    /// Absolute (relative paths are taken from config.yaml's folder).
    pub key_file: PathBuf,
    pub required: bool,
}

/// UDP transfer mode of the lidi engines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LidiMode {
    Native,
    Msg,
    Mmsg,
}

impl LidiMode {
    pub fn as_str(self) -> &'static str {
        match self {
            LidiMode::Native => "native",
            LidiMode::Msg => "msg",
            LidiMode::Mmsg => "mmsg",
        }
    }
}

/// The `lidi:` section: parameters of the RaptorQ transport.
#[derive(Clone, Debug)]
pub struct LidiConfig {
    /// UDP ports through the diode; one encode/decode thread each.
    pub ports: Vec<u16>,
    pub mtu: u16,
    /// RaptorQ block size, bytes.
    pub block: u32,
    /// Repair packets, percent of each block (lidi adds 2 more on top).
    pub repair: u8,
    /// Seconds; 0 disables.
    pub heartbeat: u64,
    /// Simultaneous transfers; None = one per module plus two.
    pub max_clients: Option<u32>,
    pub mode: Option<LidiMode>,
    /// Receiver: reset the decoder after this long with no packet (s).
    pub reset_timeout: u64,
    /// Receiver: close a transfer that received nothing for this long (s).
    pub abort_timeout: Option<u64>,
    /// Receiver: RaptorQ blocks buffered per transfer before it is dropped.
    pub client_queue_size: usize,
    /// Cap on everything DYODE hands to lidi, Mbit/s (default 100; None =
    /// no cap).  lidi sends each RaptorQ block as one burst at line rate:
    /// unpaced, a receiver or switch that cannot absorb the burst drops
    /// packets, and a block that loses more than its repair packets aborts
    /// every transfer in flight.
    pub max_mbps: Option<f64>,
    /// Log level for lidi's own messages.
    pub log_level: log::LevelFilter,
    /// Source address of the sending UDP sockets (default: dyode_in.ip:0).
    pub send_bind: Option<SocketAddr>,
    pub prometheus_in: Option<SocketAddr>,
    pub prometheus_out: Option<SocketAddr>,
}

impl Default for LidiConfig {
    fn default() -> Self {
        Self {
            ports: vec![5000],
            mtu: 1500,
            block: 220_000,
            repair: 5,
            heartbeat: 10,
            max_clients: None,
            mode: None,
            reset_timeout: 2,
            abort_timeout: Some(120),
            client_queue_size: 1024,
            max_mbps: Some(100.0),
            log_level: log::LevelFilter::Warn,
            send_bind: None,
            prometheus_in: None,
            prometheus_out: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct FolderConfig {
    pub input: PathBuf,
    pub output: PathBuf,
    /// Seconds a file must be unchanged before it is sent.
    pub settle: f64,
    /// Seconds between full rescans when nothing wakes us up.
    pub rescan: f64,
    /// Mbit/s cap for this module; None = no cap of its own.
    pub bitrate: Option<f64>,
    /// Sent files are kept under .dyode_sent this long; 0 = delete.
    pub keep_sent_hours: u64,
    /// Seconds of silence before an incomplete batch is reported.
    pub batch_timeout: f64,
    /// Where incoming files are written before verification.
    pub staging: Option<PathBuf>,
    /// Files of one batch in flight at once (input side), 1 to 16.
    pub parallel_files: u32,
    /// Keys that only meant something to udpcast.
    pub ignored: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unmapped {
    Exception,
    Zero,
}

#[derive(Clone, Debug)]
pub struct ModbusConfig {
    pub ip: String,
    pub plc_port: u16,
    pub port_out: u16,
    pub bind_out: String,
    pub unit: u8,
    /// (start, end) with end EXCLUDED.
    pub registers: Vec<(u32, u32)>,
    pub coils: Vec<(u32, u32)>,
    pub interval: f64,
    pub stale_after: f64,
    pub unmapped: Unmapped,
}

impl ModbusConfig {
    /// Size of the mirror datastore: the highest configured end.
    pub fn size(&self) -> u32 {
        self.registers
            .iter()
            .chain(self.coils.iter())
            .map(|r| r.1)
            .max()
            .unwrap_or(0)
    }
}

#[derive(Clone, Debug)]
pub struct ScreenConfig {
    pub input: PathBuf,
    pub output: PathBuf,
    pub http_port: u16,
    pub http_bind: Option<Ipv4Addr>,
    pub max_fps: f64,
    pub max_streams: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogFormat {
    Prefixed,
    Raw,
    Json,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForwardProto {
    Udp,
    Tcp,
}

#[derive(Clone, Debug)]
pub struct Forward {
    pub proto: ForwardProto,
    pub host: String,
    pub port: u16,
}

impl fmt::Display for Forward {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let p = match self.proto {
            ForwardProto::Udp => "udp",
            ForwardProto::Tcp => "tcp",
        };
        write!(f, "{p}://{}:{}", self.host, self.port)
    }
}

/// An IPv4/IPv6 network for `allow_from`.
#[derive(Clone, Copy, Debug)]
pub struct Cidr {
    addr: std::net::IpAddr,
    prefix: u8,
}

impl Cidr {
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let (a, p) = match text.split_once('/') {
            Some((a, p)) => (a, Some(p)),
            None => (text, None),
        };
        let addr: std::net::IpAddr = a.parse().ok()?;
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = match p {
            Some(p) => p.parse::<u8>().ok().filter(|p| *p <= max)?,
            None => max,
        };
        Some(Self { addr, prefix })
    }

    pub fn contains(&self, ip: std::net::IpAddr) -> bool {
        // An IPv4-mapped IPv6 peer is compared as IPv4.
        let ip = match ip {
            std::net::IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, std::net::IpAddr::V4),
            v4 => v4,
        };
        match (self.addr, ip) {
            (std::net::IpAddr::V4(n), std::net::IpAddr::V4(a)) => {
                let mask = if self.prefix == 0 {
                    0
                } else {
                    u32::MAX << (32 - self.prefix)
                };
                (u32::from(n) & mask) == (u32::from(a) & mask)
            }
            (std::net::IpAddr::V6(n), std::net::IpAddr::V6(a)) => {
                let mask = if self.prefix == 0 {
                    0
                } else {
                    u128::MAX << (128 - self.prefix)
                };
                (u128::from(n) & mask) == (u128::from(a) & mask)
            }
            _ => false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct LogsConfig {
    // input side
    pub listen_udp: Option<(String, u16)>,
    pub listen_tcp: Option<(String, u16)>,
    pub tail: Vec<String>,
    pub tail_from_start: bool,
    pub tail_state: Option<PathBuf>,
    pub allow_from: Vec<Cidr>,
    pub max_line: usize,
    pub buffer_lines: usize,
    pub batch_interval: f64,
    pub batch_bytes: usize,
    pub max_kbps: f64,
    pub copies: u32,
    pub heartbeat: f64,
    pub max_tcp_clients: usize,
    // output side
    pub out: Option<PathBuf>,
    pub out_file: String,
    pub split_by_source: bool,
    pub format: LogFormat,
    pub forward: Vec<Forward>,
    pub forward_wrap: bool,
    pub reorder_window: f64,
    pub stale_after: f64,
}

impl LogsConfig {
    /// Seconds between the copies of one batch.
    pub fn repeat_delay(&self) -> f64 {
        self.batch_interval.max(0.2)
    }

    /// Time from a batch's first copy until its last one has certainly
    /// been sent: one delay per extra copy, plus a send-loop tick of slack.
    pub fn min_reorder_window(&self) -> f64 {
        f64::from(self.copies - 1) * (self.repeat_delay() + self.batch_interval) + 0.5
    }
}

#[derive(Clone, Debug)]
pub enum ModuleKind {
    Folder(FolderConfig),
    Modbus(ModbusConfig),
    Screen(ScreenConfig),
    Logs(LogsConfig),
}

impl ModuleKind {
    pub fn type_name(&self) -> &'static str {
        match self {
            ModuleKind::Folder(_) => "folder",
            ModuleKind::Modbus(_) => "modbus",
            ModuleKind::Screen(_) => "screen",
            ModuleKind::Logs(_) => "logs",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Module {
    pub name: String,
    /// Position in config.yaml = the lidi endpoint id.
    pub index: u16,
    /// The `port` key, if any.  Kept for reference only: every module now
    /// shares the lidi ports, and is told apart by its endpoint id.
    pub legacy_port: Option<u16>,
    pub kind: ModuleKind,
}

impl Module {
    /// 8 bytes identifying this module's name and type.  Carried in every
    /// envelope so two boxes with different config.yaml files refuse each
    /// other's traffic instead of mixing up modules.
    pub fn tag(&self) -> [u8; 8] {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(b"dyode-module-v1\0");
        h.update(self.kind.type_name().as_bytes());
        h.update(b"\0");
        h.update(self.name.as_bytes());
        let d = h.finalize();
        let mut tag = [0u8; 8];
        tag.copy_from_slice(&d[..8]);
        tag
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub path: PathBuf,
    pub config_name: Option<String>,
    pub config_version: Option<String>,
    pub config_date: Option<String>,
    pub network: Network,
    pub serial: SerialConfig,
    pub logging: LoggingConfig,
    pub auth: AuthConfig,
    pub lidi: LidiConfig,
    pub modules: Vec<Module>,
}

impl Config {
    pub fn modules_of(&self, ty: &str) -> impl Iterator<Item = &Module> {
        self.modules
            .iter()
            .filter(move |m| m.kind.type_name() == ty)
    }

    /// Transfers the modules may have in flight at once: one per module,
    /// plus the extra files of folder modules with `parallel_files`.
    pub fn concurrent_transfers(&self) -> u32 {
        self.modules
            .iter()
            .map(|m| match &m.kind {
                ModuleKind::Folder(f) => f.parallel_files,
                _ => 1,
            })
            .sum()
    }

    /// lidi max_clients: configured, or what the modules need plus two
    /// (a batch summary and a heartbeat may overlap the transfers).
    pub fn max_clients(&self) -> u32 {
        self.lidi
            .max_clients
            .unwrap_or(self.concurrent_transfers() + 2)
    }
}

// --------------------------------------------------------------------------
// Value helpers.  YAML is loose: numbers may come as strings ("9600"), as in
// the Python version which ran int()/float() on everything.
// --------------------------------------------------------------------------

fn key<'a>(m: &'a Mapping, k: &str) -> Option<&'a Value> {
    match m.get(Value::String(k.to_string())) {
        None | Some(Value::Null) => None,
        Some(v) => Some(v),
    }
}

fn as_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn as_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64)),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Python's int(): accepts 5, "5", 5.0 -- and, like int(5.7), truncates.
fn as_int_trunc(v: &Value) -> Option<i64> {
    as_i64(v).or_else(|| as_f64(v).map(|f| f as i64))
}

fn text_list(v: Option<&Value>) -> Vec<String> {
    match v {
        None => vec![],
        Some(Value::Sequence(seq)) => seq.iter().filter_map(as_text).collect(),
        Some(other) => as_text(other).into_iter().collect(),
    }
}

fn bool_of(m: &Mapping, k: &str, default: bool) -> bool {
    match key(m, k) {
        None => default,
        Some(Value::Bool(b)) => *b,
        Some(v) => match as_text(v).map(|s| s.to_lowercase()) {
            Some(s) if matches!(s.as_str(), "true" | "yes" | "on" | "1") => true,
            Some(s) if matches!(s.as_str(), "false" | "no" | "off" | "0" | "") => false,
            _ => default,
        },
    }
}

/// A number >= 0 (or > 0 unless `allow_zero`), with Python's messages.
fn positive(name: &str, k: &str, v: Option<&Value>, default: f64, allow_zero: bool) -> Result<f64> {
    let Some(v) = v else { return Ok(default) };
    let Some(n) = as_f64(v) else {
        bail!("module {name:?}: {k:?} must be a number, got {v:?}");
    };
    if n < 0.0 || (n == 0.0 && !allow_zero) {
        bail!(
            "module {name:?}: {k:?} must be {}, got {n}",
            if allow_zero {
                "zero or more"
            } else {
                "more than zero"
            }
        );
    }
    Ok(n)
}

fn positive_int(
    name: &str,
    k: &str,
    v: Option<&Value>,
    default: i64,
    allow_zero: bool,
) -> Result<i64> {
    let Some(v) = v else { return Ok(default) };
    let Some(n) = as_int_trunc(v) else {
        bail!("module {name:?}: {k:?} must be a number, got {v:?}");
    };
    if n < 0 || (n == 0 && !allow_zero) {
        bail!(
            "module {name:?}: {k:?} must be {}, got {n}",
            if allow_zero {
                "zero or more"
            } else {
                "more than zero"
            }
        );
    }
    Ok(n)
}

fn port_of(name: &str, k: &str, v: &Value) -> Result<u16> {
    match as_int_trunc(v) {
        Some(p) if (0..=65535).contains(&p) => Ok(p as u16),
        _ => bail!("module {name:?}: {k:?} must be a port number, got {v:?}"),
    }
}

/// 'start-end', end EXCLUSIVE: '0-100' means addresses 0..99.
pub fn parse_range(text: &str) -> Result<(u32, u32)> {
    let bad = || {
        ConfigError(format!(
            "bad range {text:?} (expected 'start-end', e.g. '0-100')"
        ))
    };
    let (a, b) = text.split_once('-').ok_or_else(bad)?;
    if b.contains('-') {
        return Err(bad());
    }
    let start: i64 = a.trim().parse().map_err(|_| bad())?;
    let end: i64 = b.trim().parse().map_err(|_| bad())?;
    if start < 0 || end <= start || end > 65536 {
        bail!("bad range {text:?} (need 0 <= start < end <= 65536)");
    }
    Ok((start as u32, end as u32))
}

/// '514' / ':514' / '10.0.0.1:514' / 'host:514' -> (host, port).
pub fn parse_hostport(
    text: &str,
    default_host: &str,
) -> std::result::Result<(String, u16), String> {
    let text = text.trim();
    let (host, port) = match text.rsplit_once(':') {
        Some((h, p)) => (h.trim_matches(|c| c == '[' || c == ']'), p),
        None => ("", text),
    };
    let host = if host.is_empty() { default_host } else { host };
    let port: i64 = port
        .trim()
        .parse()
        .map_err(|_| format!("invalid port in {text:?}"))?;
    if !(0..=65535).contains(&port) {
        return Err(format!("port out of range: {port}"));
    }
    Ok((host.to_string(), port as u16))
}

/// 'udp://host:514' or 'tcp://host:601'.
pub fn parse_forward(text: &str) -> std::result::Result<Forward, String> {
    let text = text.trim();
    let Some((proto, rest)) = text.split_once("://") else {
        return Err(format!(
            "expected udp://host:port or tcp://host:port, got {text:?}"
        ));
    };
    let proto = match proto.to_lowercase().as_str() {
        "udp" => ForwardProto::Udp,
        "tcp" => ForwardProto::Tcp,
        _ => {
            return Err(format!(
                "expected udp://host:port or tcp://host:port, got {text:?}"
            ));
        }
    };
    let (host, port) = parse_hostport(rest, "")?;
    if host.is_empty() || port == 0 {
        return Err(format!("forward target needs a host and a port: {text:?}"));
    }
    Ok(Forward { proto, host, port })
}

/// True when `inner` is `outer` or below it (lexically, like
/// os.path.commonpath on absolute paths).
pub fn path_within(inner: &Path, outer: &Path) -> bool {
    let norm = |p: &Path| -> Vec<String> {
        let abs = if p.is_absolute() {
            p.to_path_buf()
        } else {
            std::env::current_dir().unwrap_or_default().join(p)
        };
        let mut out: Vec<String> = Vec::new();
        for c in abs.components() {
            match c {
                Component::ParentDir => {
                    out.pop();
                }
                Component::Normal(s) => out.push(s.to_string_lossy().into_owned()),
                _ => {}
            }
        }
        out
    };
    let (i, o) = (norm(inner), norm(outer));
    i.len() >= o.len() && i[..o.len()] == o[..]
}

// --------------------------------------------------------------------------
// Loading
// --------------------------------------------------------------------------

/// Return the config path: explicit > ./config.yaml > next to the binary.
pub fn find_config(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p.to_path_buf());
    }
    let mut candidates = vec![PathBuf::from("config.yaml")];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("config.yaml"));
        }
    }
    candidates.push(PathBuf::from("/etc/dyode/config.yaml"));
    for c in &candidates {
        if c.is_file() {
            return Ok(c.clone());
        }
    }
    let list: Vec<String> = candidates.iter().map(|c| c.display().to_string()).collect();
    bail!("config.yaml not found (looked in: {})", list.join(", "))
}

pub fn load(path: &Path) -> Result<Config> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| ConfigError(format!("cannot read {}: {e}", path.display())))?;
    let base = path
        .canonicalize()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    parse(&text, path, &base)
}

pub fn parse(text: &str, path: &Path, base_dir: &Path) -> Result<Config> {
    let raw: Value =
        serde_yaml::from_str(text).map_err(|e| ConfigError(format!("{}: {e}", path.display())))?;
    let Value::Mapping(raw) = raw else {
        bail!("{}: top level must be a mapping", path.display());
    };

    let Some(Value::Mapping(mods)) = key(&raw, "modules") else {
        bail!("{}: 'modules' must be a non-empty mapping", path.display());
    };
    if mods.is_empty() {
        bail!("{}: 'modules' must be a non-empty mapping", path.display());
    }
    if mods.len() > usize::from(u16::MAX) {
        bail!("too many modules");
    }
    let mut modules = Vec::new();
    for (i, (name, props)) in mods.iter().enumerate() {
        let name = as_text(name).unwrap_or_default();
        modules.push(normalize_module(&name, i as u16, props)?);
    }

    let empty = Mapping::new();
    let section = |k: &str| -> Result<&Mapping> {
        match key(&raw, k) {
            None => Ok(&empty),
            Some(Value::Mapping(m)) => Ok(m),
            Some(_) => bail!("'{k}' must be a mapping"),
        }
    };
    let d_in = section("dyode_in")?;
    let d_out = section("dyode_out")?;
    let ip = |m: &Mapping, default: &str, which: &str| -> Result<Ipv4Addr> {
        let t = key(m, "ip")
            .and_then(as_text)
            .unwrap_or_else(|| default.to_string());
        t.trim()
            .parse()
            .map_err(|_| ConfigError(format!("'{which}.ip' must be an IPv4 address, got {t:?}")))
    };
    let arp_interval = match key(d_in, "arp_interval") {
        None => 60.0,
        Some(v) => match as_f64(v) {
            Some(n) if n > 0.0 => n,
            _ => bail!("module \"dyode_in\": 'arp_interval' must be more than zero, got {v:?}"),
        },
    };
    let network = Network {
        in_ip: ip(d_in, "10.0.1.1", "dyode_in")?,
        out_ip: ip(d_out, "10.0.1.2", "dyode_out")?,
        in_interface: key(d_in, "interface")
            .and_then(as_text)
            .unwrap_or_else(|| "eth0".into()),
        out_interface: key(d_out, "interface")
            .and_then(as_text)
            .unwrap_or_else(|| "eth1".into()),
        out_mac: key(d_out, "mac")
            .and_then(as_text)
            .filter(|s| !s.is_empty()),
        arp_interval,
    };

    let ser = section("serial")?;
    let serial = SerialConfig {
        device: key(ser, "device")
            .and_then(as_text)
            .unwrap_or_else(|| "/dev/serial0".into()),
        baudrate: match key(ser, "baudrate") {
            None => 57600,
            Some(v) => as_int_trunc(v)
                .filter(|b| *b > 0 && *b <= i64::from(u32::MAX))
                .ok_or_else(|| {
                    ConfigError(format!("'serial.baudrate' must be a number, got {v:?}"))
                })? as u32,
        },
    };

    let logging = normalize_logging(key(&raw, "logging"))?;
    let auth = normalize_auth(key(&raw, "auth"), base_dir)?;
    let lidi = normalize_lidi(key(&raw, "lidi"))?;

    let cfg = Config {
        path: path.to_path_buf(),
        config_name: key(&raw, "config_name").and_then(as_text),
        config_version: key(&raw, "config_version").and_then(as_text),
        config_date: key(&raw, "config_date").and_then(as_text),
        network,
        serial,
        logging,
        auth,
        lidi,
        modules,
    };
    if cfg.max_clients() < 1 {
        bail!("'lidi.max_clients' must be at least 1");
    }
    Ok(cfg)
}

fn normalize_auth(raw: Option<&Value>, base_dir: &Path) -> Result<AuthConfig> {
    let empty = Mapping::new();
    let m = match raw {
        None => &empty,
        Some(Value::Mapping(m)) => m,
        Some(_) => bail!("'auth' must be a mapping"),
    };
    let mut unknown: Vec<String> = m
        .keys()
        .filter_map(as_text)
        .filter(|k| k != "key_file" && k != "required")
        .collect();
    if !unknown.is_empty() {
        unknown.sort();
        bail!("'auth': unknown key(s) {}", unknown.join(", "));
    }
    let required = match key(m, "required") {
        None => true,
        Some(Value::Bool(b)) => *b,
        Some(_) => bail!("'auth.required' must be true or false"),
    };
    let key_file = PathBuf::from(
        key(m, "key_file")
            .and_then(as_text)
            .unwrap_or_else(|| "dyode.key".into()),
    );
    let key_file = if key_file.is_absolute() {
        key_file
    } else {
        base_dir.join(key_file)
    };
    Ok(AuthConfig { key_file, required })
}

fn normalize_logging(raw: Option<&Value>) -> Result<LoggingConfig> {
    let mut cfg = LoggingConfig::default();
    let m = match raw {
        None => return Ok(cfg),
        Some(Value::Mapping(m)) => m,
        Some(_) => bail!("'logging' must be a mapping"),
    };
    let known = ["dir", "human", "json", "per_file_events"];
    let mut unknown: Vec<String> = m
        .keys()
        .filter_map(as_text)
        .filter(|k| !known.contains(&k.as_str()))
        .collect();
    if !unknown.is_empty() {
        unknown.sort();
        bail!(
            "'logging': unknown key(s) {} (expected dir, human, json, per_file_events)",
            unknown.join(", ")
        );
    }
    let present = |k: &str| m.get(Value::String(k.into()));
    // An empty 'dir' (or null / false) is the documented way to keep stderr only.
    if let Some(v) = present("dir") {
        match v {
            Value::Null | Value::Bool(false) => cfg.dir = None,
            v => {
                let t = as_text(v).unwrap_or_default().trim().to_string();
                cfg.dir = if t.is_empty() {
                    None
                } else {
                    Some(PathBuf::from(t))
                };
            }
        }
    }
    for k in ["human", "json"] {
        if let Some(v) = present(k) {
            let t = match v {
                Value::Null | Value::Bool(false) => continue,
                v => as_text(v).unwrap_or_default().trim().to_string(),
            };
            if t.is_empty() || t.contains('/') {
                bail!("'logging.{k}' must be a bare file name, got {v:?}");
            }
            if k == "human" {
                cfg.human = t;
            } else {
                cfg.json = t;
            }
        }
    }
    if let Some(v) = present("per_file_events") {
        match v {
            Value::Bool(b) => cfg.per_file_events = *b,
            _ => bail!("'logging.per_file_events' must be true or false, got {v:?}"),
        }
    }
    if let Some(d) = &cfg.dir {
        if !d.is_absolute() {
            bail!(
                "'logging.dir' must be an absolute path, got {:?}",
                d.display().to_string()
            );
        }
    }
    if cfg.human == cfg.json {
        bail!("'logging.human' and 'logging.json' must differ");
    }
    Ok(cfg)
}

fn normalize_lidi(raw: Option<&Value>) -> Result<LidiConfig> {
    let mut c = LidiConfig::default();
    let m = match raw {
        None => return Ok(c),
        Some(Value::Mapping(m)) => m,
        Some(_) => bail!("'lidi' must be a mapping"),
    };
    let known = [
        "ports",
        "mtu",
        "block",
        "repair",
        "heartbeat",
        "max_clients",
        "mode",
        "reset_timeout",
        "abort_timeout",
        "client_queue_size",
        "max_mbps",
        "log_level",
        "send_bind",
        "prometheus_in",
        "prometheus_out",
    ];
    let mut unknown: Vec<String> = m
        .keys()
        .filter_map(as_text)
        .filter(|k| !known.contains(&k.as_str()))
        .collect();
    if !unknown.is_empty() {
        unknown.sort();
        bail!("'lidi': unknown key(s) {}", unknown.join(", "));
    }
    let int = |k: &str, lo: i64, hi: i64| -> Result<Option<i64>> {
        match key(m, k) {
            None => Ok(None),
            Some(v) => match as_i64(v) {
                Some(n) if (lo..=hi).contains(&n) => Ok(Some(n)),
                _ => bail!("'lidi.{k}' must be a whole number between {lo} and {hi}, got {v:?}"),
            },
        }
    };
    if let Some(v) = key(m, "ports") {
        let items: Vec<Value> = match v {
            Value::Sequence(s) => s.clone(),
            Value::String(s) => s
                .split(',')
                .map(|p| Value::String(p.trim().into()))
                .collect(),
            other => vec![other.clone()],
        };
        let mut ports = Vec::new();
        for it in &items {
            match as_i64(it) {
                Some(p) if (1..=65535).contains(&p) => ports.push(p as u16),
                _ => bail!("'lidi.ports' must be a list of UDP ports, got {it:?}"),
            }
        }
        if ports.is_empty() {
            bail!("'lidi.ports' must list at least one UDP port");
        }
        let mut sorted = ports.clone();
        sorted.sort_unstable();
        sorted.dedup();
        if sorted.len() != ports.len() {
            bail!("'lidi.ports' lists a port twice");
        }
        c.ports = ports;
    }
    if let Some(n) = int("mtu", 1280, 9000)? {
        c.mtu = n as u16;
    }
    if let Some(n) = int("block", 1024, 64 * 1024 * 1024)? {
        c.block = n as u32;
    }
    if let Some(n) = int("repair", 0, 99)? {
        c.repair = n as u8;
    }
    if let Some(n) = int("heartbeat", 0, 3600)? {
        c.heartbeat = n as u64;
    }
    if let Some(n) = int("max_clients", 1, 65535)? {
        c.max_clients = Some(n as u32);
    }
    if let Some(n) = int("reset_timeout", 1, 3600)? {
        c.reset_timeout = n as u64;
    }
    if let Some(v) = key(m, "abort_timeout") {
        c.abort_timeout = match as_i64(v) {
            Some(0) => None,
            Some(n) if n > 0 => Some(n as u64),
            _ => bail!("'lidi.abort_timeout' must be a number of seconds (0 = never), got {v:?}"),
        };
    }
    if let Some(n) = int("client_queue_size", 0, 1 << 24)? {
        c.client_queue_size = n as usize;
    }
    if let Some(v) = key(m, "max_mbps") {
        c.max_mbps = match as_f64(v) {
            Some(0.0) => None,
            Some(n) if n > 0.0 => Some(n),
            _ => bail!("'lidi.max_mbps' must be a positive number of Mbit/s, got {v:?}"),
        };
    }
    if let Some(v) = key(m, "mode") {
        c.mode = Some(
            match as_text(v).unwrap_or_default().to_lowercase().as_str() {
                "native" => LidiMode::Native,
                "msg" => LidiMode::Msg,
                "mmsg" => LidiMode::Mmsg,
                _ => bail!("'lidi.mode' must be native, msg or mmsg, got {v:?}"),
            },
        );
    }
    if let Some(v) = key(m, "log_level") {
        c.log_level = match as_text(v).unwrap_or_default().to_lowercase().as_str() {
            "off" => log::LevelFilter::Off,
            "error" => log::LevelFilter::Error,
            "warn" | "warning" => log::LevelFilter::Warn,
            "info" => log::LevelFilter::Info,
            "debug" => log::LevelFilter::Debug,
            "trace" => log::LevelFilter::Trace,
            _ => bail!("'lidi.log_level' must be off, error, warn, info, debug or trace"),
        };
    }
    let sockaddr = |k: &str| -> Result<Option<SocketAddr>> {
        match key(m, k).and_then(as_text) {
            None => Ok(None),
            Some(t) => t
                .parse::<SocketAddrV4>()
                .map(|a| Some(SocketAddr::V4(a)))
                .map_err(|_| ConfigError(format!("'lidi.{k}' must be ip:port, got {t:?}"))),
        }
    };
    c.send_bind = sockaddr("send_bind")?;
    c.prometheus_in = sockaddr("prometheus_in")?;
    c.prometheus_out = sockaddr("prometheus_out")?;
    // Sanity: lidi refuses symbol counts that do not fit in a u16, and a
    // block smaller than one packet makes no sense.
    lidi_protocol::RaptorQ::new(c.mtu, c.block, c.repair)
        .map_err(|e| ConfigError(format!("'lidi': mtu/block/repair rejected by lidi: {e}")))?;
    Ok(c)
}

const UDPCAST_ONLY: [&str; 4] = ["fec", "autostart", "start_timeout", "file_gap"];

/// The most files one folder module sends at once.
pub const MAX_PARALLEL_FILES: i64 = 16;

fn normalize_module(name: &str, index: u16, props: &Value) -> Result<Module> {
    let Value::Mapping(p) = props else {
        bail!("module {name:?}: properties must be a mapping");
    };
    let ty = key(p, "type")
        .and_then(as_text)
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    let legacy_port = match key(p, "port") {
        Some(v) => Some(port_of(name, "port", v)?),
        None => None,
    };
    let folder = |k: &str| -> Result<PathBuf> {
        match key(p, k).and_then(as_text) {
            Some(t) if !t.is_empty() => Ok(PathBuf::from(t)),
            _ => bail!("module {name:?}: '{k}' folder is required"),
        }
    };
    let kind = match ty.as_str() {
        "folder" => {
            let output = folder("out")?;
            let bitrate = match key(p, "bitrate") {
                None => None,
                Some(v) => match as_f64(v) {
                    Some(n) if n >= 1.0 => Some(n),
                    Some(_) => bail!("module {name:?}: 'bitrate' must be at least 1"),
                    None => {
                        bail!("module {name:?}: 'bitrate' must be a number of Mbit/s, got {v:?}")
                    }
                },
            };
            let staging = match key(p, "staging").and_then(as_text) {
                None => None,
                Some(s) => {
                    let s = PathBuf::from(s.trim());
                    if !s.is_absolute() {
                        bail!(
                            "module {name:?}: 'staging' must be an absolute path, got {:?}",
                            s.display().to_string()
                        );
                    }
                    if path_within(&s, &output) {
                        bail!(
                            "module {name:?}: 'staging' must not be inside the output folder, or \
                             anything serving that folder will see partial files"
                        );
                    }
                    Some(s)
                }
            };
            ModuleKind::Folder(FolderConfig {
                input: folder("in")?,
                output,
                settle: positive(name, "settle", key(p, "settle"), 2.0, true)?,
                rescan: positive(name, "rescan", key(p, "rescan"), 30.0, false)?,
                bitrate,
                keep_sent_hours: positive_int(
                    name,
                    "keep_sent_hours",
                    key(p, "keep_sent_hours"),
                    24,
                    true,
                )? as u64,
                batch_timeout: positive(
                    name,
                    "batch_timeout",
                    key(p, "batch_timeout"),
                    300.0,
                    true,
                )?,
                staging,
                parallel_files: match key(p, "parallel_files") {
                    None => 1,
                    Some(v) => match as_int_trunc(v) {
                        Some(n) if (1..=MAX_PARALLEL_FILES).contains(&n) => n as u32,
                        _ => bail!(
                            "module {name:?}: 'parallel_files' must be 1 to {MAX_PARALLEL_FILES}, got {v:?}"
                        ),
                    },
                },
                ignored: UDPCAST_ONLY
                    .iter()
                    .filter(|k| key(p, k).is_some())
                    .map(|k| (*k).to_string())
                    .collect(),
            })
        }
        "screen" => {
            let http_bind = match key(p, "http_bind").and_then(as_text) {
                None => None,
                Some(t) if t.is_empty() => None,
                Some(t) => Some(t.parse::<Ipv4Addr>().map_err(|_| {
                    ConfigError(format!(
                        "module {name:?}: 'http_bind' must be an IPv4 address, got {t:?}"
                    ))
                })?),
            };
            ModuleKind::Screen(ScreenConfig {
                input: folder("in")?,
                output: folder("out")?,
                http_port: match key(p, "http_port") {
                    Some(v) => port_of(name, "http_port", v)?,
                    None => 8080,
                },
                http_bind,
                max_fps: positive(name, "max_fps", key(p, "max_fps"), 10.0, false)?,
                max_streams: positive_int(name, "max_streams", key(p, "max_streams"), 8, false)?
                    as usize,
            })
        }
        "modbus" => {
            let Some(ip) = key(p, "ip").and_then(as_text) else {
                bail!("module {name:?}: PLC 'ip' is required");
            };
            let ranges = |k: &str| -> Result<Vec<(u32, u32)>> {
                text_list(key(p, k))
                    .iter()
                    .map(|r| parse_range(r))
                    .collect()
            };
            let registers = ranges("registers")?;
            let coils = ranges("coils")?;
            if registers.is_empty() && coils.is_empty() {
                bail!("module {name:?}: no registers or coils configured");
            }
            let unmapped = match key(p, "unmapped")
                .and_then(as_text)
                .map(|s| s.to_lowercase())
            {
                None => Unmapped::Exception,
                Some(s) if s == "exception" => Unmapped::Exception,
                Some(s) if s == "zero" => Unmapped::Zero,
                Some(_) => bail!("module {name:?}: 'unmapped' must be exception or zero"),
            };
            let unit = match key(p, "unit") {
                None => 1,
                Some(v) => match as_int_trunc(v) {
                    Some(u) if (0..=255).contains(&u) => u as u8,
                    _ => bail!("module {name:?}: 'unit' must be 0-255, got {v:?}"),
                },
            };
            ModuleKind::Modbus(ModbusConfig {
                ip: ip.trim().to_string(),
                plc_port: match key(p, "plc_port") {
                    Some(v) => port_of(name, "plc_port", v)?,
                    None => 502,
                },
                port_out: match key(p, "port_out") {
                    Some(v) => port_of(name, "port_out", v)?,
                    None => 502,
                },
                bind_out: key(p, "bind_out")
                    .and_then(as_text)
                    .unwrap_or_else(|| "0.0.0.0".into()),
                unit,
                registers,
                coils,
                interval: positive(name, "interval", key(p, "interval"), 1.0, false)?,
                stale_after: positive(name, "stale_after", key(p, "stale_after"), 10.0, false)?,
                unmapped,
            })
        }
        "logs" => ModuleKind::Logs(normalize_logs(name, p)?),
        _ => bail!(
            "module {name:?}: unknown type {:?} (expected one of folder, modbus, screen, logs)",
            key(p, "type").and_then(as_text).unwrap_or_default()
        ),
    };
    Ok(Module {
        name: name.to_string(),
        index,
        legacy_port,
        kind,
    })
}

fn normalize_logs(name: &str, p: &Mapping) -> Result<LogsConfig> {
    let hostport = |k: &str| -> Result<Option<(String, u16)>> {
        match key(p, k).and_then(as_text) {
            None => Ok(None),
            Some(t) if t.is_empty() => Ok(None),
            Some(t) => parse_hostport(&t, "0.0.0.0")
                .map(Some)
                .map_err(|e| ConfigError(format!("module {name:?}: {k:?}: {e}"))),
        }
    };
    let listen_udp = hostport("listen_udp")?;
    let listen_tcp = hostport("listen_tcp")?;
    let tail = text_list(key(p, "tail"));
    for pat in &tail {
        if !Path::new(pat).is_absolute() {
            bail!("module {name:?}: 'tail' entries must be absolute paths, got {pat:?}");
        }
    }
    if listen_udp.is_none() && listen_tcp.is_none() && tail.is_empty() {
        bail!(
            "module {name:?}: a logs module needs at least one source: listen_udp, listen_tcp or tail"
        );
    }
    let tail_from = key(p, "tail_from")
        .and_then(as_text)
        .unwrap_or_else(|| "end".into())
        .to_lowercase();
    if tail_from != "end" && tail_from != "start" {
        bail!("module {name:?}: 'tail_from' must be 'end' or 'start'");
    }
    let tail_state = match key(p, "tail_state").and_then(as_text) {
        None => None,
        Some(t) => {
            let t = PathBuf::from(t);
            if !t.is_absolute() {
                bail!("module {name:?}: 'tail_state' must be an absolute path");
            }
            Some(t)
        }
    };
    let mut allow_from = Vec::new();
    for a in text_list(key(p, "allow_from")) {
        match Cidr::parse(&a) {
            Some(c) => allow_from.push(c),
            None => bail!(
                "module {name:?}: 'allow_from': {a:?} does not appear to be an IPv4 or IPv6 network"
            ),
        }
    }

    let num = |k: &str, default: f64, minimum: f64, allow_zero: bool, int: bool| -> Result<f64> {
        let v = key(p, k);
        let value = match v {
            None => default,
            Some(v) => {
                let n = if int {
                    as_int_trunc(v).map(|i| i as f64)
                } else {
                    as_f64(v)
                };
                match n {
                    Some(n) => n,
                    None => bail!("module {name:?}: {k:?} must be a number, got {v:?}"),
                }
            }
        };
        if value < minimum || (value == 0.0 && !allow_zero) {
            bail!("module {name:?}: {k:?} is out of range: {value}");
        }
        Ok(value)
    };

    let max_line = num("max_line", 16384.0, 64.0, false, true)? as usize;
    let buffer_lines = num("buffer_lines", 100_000.0, 1.0, false, true)? as usize;
    let batch_interval = num("batch_interval", 0.5, 0.05, false, false)?;
    let batch_bytes = num("batch_bytes", 48000.0, 1024.0, false, true)? as usize;
    let max_kbps = num("max_kbps", 2000.0, 8.0, false, false)?;
    let copies = num("copies", 2.0, 1.0, false, true)? as u32;
    let heartbeat = num("heartbeat", 10.0, 1.0, false, false)?;
    let max_tcp_clients = num("max_tcp_clients", 64.0, 1.0, false, true)? as usize;
    let reorder_window = num("reorder_window", 3.0, 0.0, true, false)?;
    if copies > 4 {
        bail!("module {name:?}: 'copies' above 4 only wastes bandwidth");
    }
    if max_line > batch_bytes {
        bail!("module {name:?}: 'max_line' must not exceed 'batch_bytes'");
    }
    let stale_after = match key(p, "stale_after") {
        None => 3.0 * heartbeat,
        Some(_) => num("stale_after", 0.0, 1.0, false, false)?,
    };

    let out = match key(p, "out").and_then(as_text) {
        None => None,
        Some(t) if t.is_empty() => None,
        Some(t) => {
            let t = PathBuf::from(t);
            if !t.is_absolute() {
                bail!("module {name:?}: 'out' must be an absolute directory");
            }
            Some(t)
        }
    };
    let out_file = key(p, "out_file")
        .and_then(as_text)
        .unwrap_or_else(|| "received.log".into());
    if out_file.contains('/') || out_file.is_empty() || out_file == "." || out_file == ".." {
        bail!("module {name:?}: 'out_file' must be a bare file name");
    }
    let format = match key(p, "format")
        .and_then(as_text)
        .unwrap_or_else(|| "prefixed".into())
        .to_lowercase()
        .as_str()
    {
        "prefixed" => LogFormat::Prefixed,
        "raw" => LogFormat::Raw,
        "json" => LogFormat::Json,
        _ => bail!("module {name:?}: 'format' must be one of prefixed, raw, json"),
    };
    let mut forward = Vec::new();
    for f in text_list(key(p, "forward")) {
        forward.push(
            parse_forward(&f)
                .map_err(|e| ConfigError(format!("module {name:?}: 'forward': {e}")))?,
        );
    }
    if out.is_none() && forward.is_empty() {
        bail!(
            "module {name:?}: a logs module needs somewhere to put lines on the output side: \
             'out' and/or 'forward'"
        );
    }
    let mut cfg = LogsConfig {
        listen_udp,
        listen_tcp,
        tail,
        tail_from_start: tail_from == "start",
        tail_state,
        allow_from,
        max_line,
        buffer_lines,
        batch_interval,
        batch_bytes,
        max_kbps,
        copies,
        heartbeat,
        max_tcp_clients,
        out,
        out_file,
        split_by_source: bool_of(p, "split_by_source", false),
        format,
        forward,
        forward_wrap: bool_of(p, "forward_wrap", true),
        reorder_window,
        stale_after,
    };
    // The output side must wait at least as long as the last copy of a batch
    // takes to arrive, or 'copies' is wasted.  Raised rather than rejected:
    // it is a minimum, and the same file configures both boxes.
    cfg.reorder_window = cfg.reorder_window.max(cfg.min_reorder_window());
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = include_str!("../config.example.yaml");

    fn load_text(text: &str) -> Result<Config> {
        parse(text, Path::new("config.yaml"), Path::new("/etc/dyode"))
    }

    #[test]
    fn example_config_loads() {
        let cfg = load_text(EXAMPLE).unwrap();
        assert_eq!(cfg.modules.len(), 4);
        assert_eq!(cfg.network.out_mac.as_deref(), Some("b8:27:eb:b1:ff:ab"));
        assert_eq!(cfg.auth.key_file, PathBuf::from("/etc/dyode/dyode.key"));
        let ModuleKind::Modbus(m) = &cfg.modules[1].kind else {
            panic!()
        };
        assert_eq!(m.registers, vec![(0, 100), (400, 450)]);
        assert_eq!(m.size(), 450);
        assert_eq!(cfg.modules[1].index, 1);
    }

    #[test]
    fn ranges() {
        assert_eq!(parse_range("0-100").unwrap(), (0, 100));
        assert!(parse_range("100-100").is_err());
        assert!(parse_range("0-65537").is_err());
        assert!(parse_range("x").is_err());
    }

    #[test]
    fn rejects_unknown_type_and_bad_staging() {
        let bad = "modules:\n  a:\n    type: nope\n    port: 1\n";
        assert!(load_text(bad).unwrap_err().0.contains("unknown type"));
        let inside = "modules:\n  a:\n    type: folder\n    in: /in\n    out: /srv/out\n    staging: /srv/out/tmp\n";
        assert!(
            load_text(inside)
                .unwrap_err()
                .0
                .contains("must not be inside")
        );
    }

    #[test]
    fn legacy_udpcast_keys_are_accepted() {
        let t = "modules:\n  a:\n    type: Folder\n    port: 9600\n    in: /in\n    out: /out\n    fec: 8x8/128\n    file_gap: 1\n";
        let cfg = load_text(t).unwrap();
        let ModuleKind::Folder(f) = &cfg.modules[0].kind else {
            panic!()
        };
        assert_eq!(f.ignored, vec!["fec".to_string(), "file_gap".to_string()]);
        assert_eq!(cfg.modules[0].legacy_port, Some(9600));
    }

    #[test]
    fn logs_validation() {
        let t = "modules:\n  l:\n    type: logs\n    out: /var/log/x\n";
        assert!(load_text(t).unwrap_err().0.contains("at least one source"));
        let t = "modules:\n  l:\n    type: logs\n    listen_udp: 514\n    forward: [udp://10.0.0.5:514]\n    copies: 3\n";
        let cfg = load_text(t).unwrap();
        let ModuleKind::Logs(l) = &cfg.modules[0].kind else {
            panic!()
        };
        assert_eq!(l.listen_udp, Some(("0.0.0.0".into(), 514)));
        assert!(l.reorder_window >= l.min_reorder_window());
        assert_eq!(l.stale_after, 30.0);
    }

    #[test]
    fn cidr() {
        let n = Cidr::parse("192.168.1.0/24").unwrap();
        assert!(n.contains("192.168.1.77".parse().unwrap()));
        assert!(!n.contains("192.168.2.1".parse().unwrap()));
        assert!(n.contains("::ffff:192.168.1.5".parse().unwrap()));
        assert!(
            Cidr::parse("10.0.0.1")
                .unwrap()
                .contains("10.0.0.1".parse().unwrap())
        );
    }

    #[test]
    fn lidi_section() {
        let t = "lidi:\n  ports: [5001, 5002]\n  repair: 10\n  max_mbps: 0\nmodules:\n  m:\n    type: modbus\n    ip: 1.2.3.4\n    registers: [0-10]\n";
        let cfg = load_text(t).unwrap();
        assert_eq!(cfg.lidi.ports, vec![5001, 5002]);
        assert_eq!(cfg.lidi.max_mbps, None);
        assert_eq!(load_text(EXAMPLE).unwrap().lidi.max_mbps, Some(100.0));
        assert_eq!(cfg.max_clients(), 3);
        let t2 = "lidi:\n  bogus: 1\nmodules:\n  m:\n    type: modbus\n    ip: 1.2.3.4\n    registers: [0-10]\n";
        assert!(load_text(t2).is_err());
    }
}
