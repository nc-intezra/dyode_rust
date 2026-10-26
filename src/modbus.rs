//! Modbus mirroring (module type `modbus`).
//!
//! Input side: poll a PLC over Modbus/TCP and push the values out.
//! Output side: serve the last values from a read-only Modbus/TCP server.
//!
//! The Python version used pymodbus; this is a small self-contained
//! implementation of exactly what DYODE needs: function codes 1 (read
//! coils) and 3 (read holding registers) as a client, and as a server the
//! same plus 2/4 and basic device identification.  Every write function is
//! refused with exception 01 (Illegal Function), and with `unmapped:
//! exception` a read outside the configured ranges gets exception 02
//! (Illegal Data Address) -- the 1.5.0 behaviour (D-03).
//!
//! Messages are the same JSON as before:
//! `{"module", "seq", "time", "registers": {"<start>": [..]}, "coils": {..}}`.
//! The transport is either lidi (DYODE v1, one transfer per poll) or the
//! serial line (DYODE v2, see serial.rs).

use crate::config::{ModbusConfig, Module, Unmapped};
use crate::envelope::{Body, Failure, Header, Inbound, Kind};
use crate::logging::{Every, FailureLog};
use crate::transport::ModuleLink;
use serde_json::{Map, Value, json};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

pub const MAX_REGISTERS_PER_READ: u32 = 125;
pub const MAX_COILS_PER_READ: u32 = 2000;
const MAX_MESSAGE: u64 = 4 * 1024 * 1024;
const MAX_SERVER_CLIENTS: usize = 64;

pub const FC_READ_COILS: u8 = 1;
pub const FC_READ_DISCRETE: u8 = 2;
pub const FC_READ_HOLDING: u8 = 3;
pub const FC_READ_INPUT: u8 = 4;
const WRITE_FCS: [u8; 6] = [5, 6, 15, 16, 22, 23];
const FC_MEI: u8 = 0x2B;

pub const EX_ILLEGAL_FUNCTION: u8 = 1;
pub const EX_ILLEGAL_ADDRESS: u8 = 2;
pub const EX_ILLEGAL_VALUE: u8 = 3;

// ==========================================================================
// Client (input side)
// ==========================================================================

/// A Modbus/TCP client that keeps its connection between polls and
/// reconnects after any failure.
pub struct TcpClient {
    target: String,
    unit: u8,
    timeout: Duration,
    stream: Option<TcpStream>,
    tid: u16,
}

impl TcpClient {
    pub fn new(host: &str, port: u16, unit: u8) -> Self {
        Self {
            target: format!("{host}:{port}"),
            unit,
            timeout: Duration::from_secs(3),
            stream: None,
            tid: 0,
        }
    }

    fn connect(&mut self) -> Result<&mut TcpStream, String> {
        if self.stream.is_none() {
            let addr = self
                .target
                .to_socket_addrs()
                .map_err(|e| format!("cannot resolve {}: {e}", self.target))?
                .next()
                .ok_or_else(|| format!("cannot resolve {}", self.target))?;
            let s = TcpStream::connect_timeout(&addr, self.timeout)
                .map_err(|e| format!("cannot connect to {}: {e}", self.target))?;
            s.set_read_timeout(Some(self.timeout)).ok();
            s.set_write_timeout(Some(self.timeout)).ok();
            s.set_nodelay(true).ok();
            self.stream = Some(s);
        }
        Ok(self.stream.as_mut().unwrap())
    }

    pub fn close(&mut self) {
        self.stream = None;
    }

    /// One read request; returns the data bytes of the response.
    fn read(&mut self, fc: u8, address: u32, count: u32) -> Result<Vec<u8>, String> {
        self.tid = self.tid.wrapping_add(1);
        let tid = self.tid;
        let unit = self.unit;
        let mut req = Vec::with_capacity(12);
        req.extend_from_slice(&tid.to_be_bytes());
        req.extend_from_slice(&0u16.to_be_bytes());
        req.extend_from_slice(&6u16.to_be_bytes());
        req.push(unit);
        req.push(fc);
        req.extend_from_slice(&(address as u16).to_be_bytes());
        req.extend_from_slice(&(count as u16).to_be_bytes());
        let s = self.connect()?;
        s.write_all(&req).map_err(|e| format!("send failed: {e}"))?;
        loop {
            let mut head = [0u8; 7];
            s.read_exact(&mut head)
                .map_err(|e| format!("no response: {e}"))?;
            let rtid = u16::from_be_bytes([head[0], head[1]]);
            let len = usize::from(u16::from_be_bytes([head[4], head[5]]));
            if !(2..=260).contains(&len) || head[2..4] != [0, 0] {
                return Err("malformed response".into());
            }
            let mut pdu = vec![0u8; len - 1];
            s.read_exact(&mut pdu)
                .map_err(|e| format!("short response: {e}"))?;
            if rtid != tid {
                continue; // a late answer to an earlier request
            }
            if pdu[0] == fc | 0x80 {
                return Err(format!(
                    "read of {count} value(s) at {address} failed: exception {}",
                    pdu.get(1).unwrap_or(&0)
                ));
            }
            if pdu[0] != fc || pdu.len() < 2 || pdu.len() != 2 + usize::from(pdu[1]) {
                return Err(format!(
                    "read of {count} value(s) at {address} failed: malformed response"
                ));
            }
            return Ok(pdu[2..].to_vec());
        }
    }

    pub fn read_holding_registers(&mut self, address: u32, count: u32) -> Result<Vec<u16>, String> {
        let data = self.read(FC_READ_HOLDING, address, count)?;
        if data.len() < 2 * count as usize {
            return Err(format!("short register response at {address}"));
        }
        Ok(data
            .chunks(2)
            .take(count as usize)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect())
    }

    pub fn read_coils(&mut self, address: u32, count: u32) -> Result<Vec<bool>, String> {
        let data = self.read(FC_READ_COILS, address, count)?;
        if data.len() * 8 < count as usize {
            return Err(format!("short coil response at {address}"));
        }
        // Padded to a multiple of 8: keep only `count`.
        Ok((0..count as usize)
            .map(|i| data[i / 8] & (1 << (i % 8)) != 0)
            .collect())
    }
}

/// Values by start address, as sent: {"0": [..], "400": [..]}.
type Blocks = Map<String, Value>;

/// Reads the configured ranges from one PLC.
pub struct PlcPoller {
    pub name: String,
    cfg: ModbusConfig,
    client: TcpClient,
    seq: u64,
}

impl PlcPoller {
    pub fn new(name: &str, cfg: &ModbusConfig) -> Self {
        Self {
            name: name.to_string(),
            cfg: cfg.clone(),
            client: TcpClient::new(&cfg.ip, cfg.plc_port, cfg.unit),
            seq: 0,
        }
    }

    fn poll_inner(&mut self) -> Result<(Blocks, Blocks), String> {
        let mut registers = Map::new();
        for &(start, end) in &self.cfg.registers {
            let mut values = Vec::new();
            let mut addr = start;
            while addr < end {
                let count = MAX_REGISTERS_PER_READ.min(end - addr);
                values.extend(self.client.read_holding_registers(addr, count)?);
                addr += count;
            }
            registers.insert(start.to_string(), json!(values));
        }
        let mut coils = Map::new();
        for &(start, end) in &self.cfg.coils {
            let mut values = Vec::new();
            let mut addr = start;
            while addr < end {
                let count = MAX_COILS_PER_READ.min(end - addr);
                values.extend(self.client.read_coils(addr, count)?);
                addr += count;
            }
            coils.insert(start.to_string(), json!(values));
        }
        Ok((registers, coils))
    }

    /// One message with all configured values.
    pub fn poll(&mut self) -> Result<Value, String> {
        match self.poll_inner() {
            Ok((registers, coils)) => {
                self.seq += 1;
                Ok(json!({
                    "module": self.name,
                    "seq": self.seq,
                    "time": crate::timefmt::unix_now_f64(),
                    "registers": registers,
                    "coils": coils,
                }))
            }
            Err(e) => {
                // Drop the connection so the next poll starts clean.
                self.client.close();
                Err(e)
            }
        }
    }
}

/// DYODE v1 input agent: poll one PLC, send each poll through lidi.
pub fn run_input(module: &Module, cfg: &ModbusConfig, link: &ModuleLink) -> Result<(), String> {
    let mut poller = PlcPoller::new(&module.name, cfg);
    let mut status = FailureLog::new(format!("module {:?}", module.name));
    log::info!(
        "module {:?}: polling {}:{} every {}s (endpoint {})",
        module.name,
        cfg.ip,
        cfg.plc_port,
        cfg.interval,
        module.index
    );
    let interval = Duration::from_secs_f64(cfg.interval);
    loop {
        let started = Instant::now();
        let res = poller
            .poll()
            .and_then(|msg| link.send_message(b"", msg.to_string().as_bytes()));
        match res {
            Ok(()) => status.ok(),
            Err(e) => status.fail(e),
        }
        std::thread::sleep(interval.saturating_sub(started.elapsed()));
    }
}

// ==========================================================================
// Mirror (output side)
// ==========================================================================

struct Store {
    coils: Vec<bool>,
    registers: Vec<u16>,
}

pub struct Mirror {
    pub name: String,
    cfg: ModbusConfig,
    size: u32,
    store: RwLock<Store>,
    last_update: Mutex<Option<Instant>>,
    /// (boot of the sending stream, last seq applied).
    last: Mutex<Option<([u8; 8], u64)>>,
    stale_logged: Mutex<bool>,
    refused_writes: AtomicU64,
    write_warn: Mutex<Every>,
    clients: AtomicUsize,
}

impl Mirror {
    pub fn new(name: &str, cfg: &ModbusConfig) -> Self {
        let size = cfg.size();
        Self {
            name: name.to_string(),
            cfg: cfg.clone(),
            size,
            store: RwLock::new(Store {
                coils: vec![false; size as usize],
                registers: vec![0; size as usize],
            }),
            last_update: Mutex::new(None),
            last: Mutex::new(None),
            stale_logged: Mutex::new(false),
            refused_writes: AtomicU64::new(0),
            write_warn: Mutex::new(Every::new(Duration::from_secs(60))),
            clients: AtomicUsize::new(0),
        }
    }

    fn covers(ranges: &[(u32, u32)], address: u32, count: u32) -> bool {
        ranges
            .iter()
            .any(|&(s, e)| s <= address && address + count <= e)
    }

    fn write_block(&self, blocks: Option<&Value>, registers: bool) -> Result<(), String> {
        let kind = if registers { "registers" } else { "coils" };
        let Some(blocks) = blocks else { return Ok(()) };
        let Some(blocks) = blocks.as_object() else {
            return Err(format!("{kind} must be an object"));
        };
        // Validate everything before writing anything.
        let mut writes: Vec<(u32, Value)> = Vec::new();
        for (start_s, values) in blocks {
            let start: i64 = start_s
                .parse()
                .map_err(|_| format!("{kind}: bad start {start_s:?}"))?;
            let Some(values) = values.as_array() else {
                return Err(format!("{kind} at {start}: values must be a list"));
            };
            if registers
                && !values
                    .iter()
                    .all(|v| v.as_u64().is_some_and(|n| n <= 0xFFFF))
            {
                return Err(format!("register values at {start} out of range"));
            }
            if start < 0 || start >= i64::from(self.size) {
                log::warn!(
                    "module {:?}: {kind} at {start} outside datastore (0-{}), ignored. Do both config.yaml files match?",
                    self.name,
                    self.size.saturating_sub(1)
                );
                continue;
            }
            writes.push((start as u32, Value::Array(values.clone())));
        }
        let mut st = self.store.write().unwrap();
        for (start, values) in writes {
            let values = values.as_array().unwrap();
            let room = (self.size - start) as usize;
            if values.len() > room {
                log::warn!(
                    "module {:?}: {kind} at {start} truncated to fit datastore (0-{})",
                    self.name,
                    self.size - 1
                );
            }
            for (i, v) in values.iter().take(room).enumerate() {
                let a = start as usize + i;
                if registers {
                    st.registers[a] = v.as_u64().unwrap_or(0) as u16;
                } else {
                    st.coils[a] = match v {
                        Value::Bool(b) => *b,
                        Value::Number(n) => n.as_f64().unwrap_or(0.0) != 0.0,
                        _ => false,
                    };
                }
            }
        }
        Ok(())
    }

    /// Apply one decoded message.  Returns true if accepted.
    pub fn apply(&self, msg: &Value, boot: Option<[u8; 8]>) -> bool {
        if !msg.is_object() {
            log::error!(
                "module {:?}: rejected malformed message: not an object",
                self.name
            );
            return false;
        }
        let seq = msg.get("seq").and_then(Value::as_u64);
        // lidi delivers transfers concurrently: never let an older poll
        // overwrite a newer one.
        if let (Some(b), Some(s)) = (boot, seq) {
            let mut last = self.last.lock().unwrap();
            if let Some((lb, ls)) = *last {
                if lb == b && s <= ls {
                    log::debug!(
                        "module {:?}: update {s} older than {ls}; skipped",
                        self.name
                    );
                    return false;
                }
                if lb == b && s != ls + 1 {
                    log::debug!(
                        "module {:?}: seq jumped {ls} -> {s} (lost updates)",
                        self.name
                    );
                }
            }
            *last = Some((b, s));
        }
        let res = self
            .write_block(msg.get("registers"), true)
            .and_then(|()| self.write_block(msg.get("coils"), false));
        if let Err(e) = res {
            log::error!("module {:?}: rejected malformed message: {e}", self.name);
            return false;
        }
        let mut stale = self.stale_logged.lock().unwrap();
        if *stale {
            log::info!("module {:?}: updates resumed", self.name);
            *stale = false;
        }
        *self.last_update.lock().unwrap() = Some(Instant::now());
        true
    }

    /// Answer one request PDU (function code + data).  Returns the
    /// response PDU.
    pub fn handle_pdu(&self, pdu: &[u8]) -> Vec<u8> {
        let exc = |fc: u8, code: u8| vec![fc | 0x80, code];
        let Some(&fc) = pdu.first() else {
            return exc(0, EX_ILLEGAL_FUNCTION);
        };
        if WRITE_FCS.contains(&fc) {
            let n = self.refused_writes.fetch_add(1, Ordering::Relaxed) + 1;
            if self.write_warn.lock().unwrap().due() {
                let addr = if pdu.len() >= 3 {
                    u16::from_be_bytes([pdu[1], pdu[2]])
                } else {
                    0
                };
                log::warn!(
                    "module {:?}: Modbus client write refused (function {fc}, address {addr}); the mirror is read-only [{n} refused so far]",
                    self.name
                );
            }
            return exc(fc, EX_ILLEGAL_FUNCTION);
        }
        match fc {
            FC_READ_COILS | FC_READ_DISCRETE | FC_READ_HOLDING | FC_READ_INPUT => {
                if pdu.len() != 5 {
                    return exc(fc, EX_ILLEGAL_VALUE);
                }
                let address = u32::from(u16::from_be_bytes([pdu[1], pdu[2]]));
                let count = u32::from(u16::from_be_bytes([pdu[3], pdu[4]]));
                let bits = fc == FC_READ_COILS || fc == FC_READ_DISCRETE;
                let max = if bits {
                    MAX_COILS_PER_READ
                } else {
                    MAX_REGISTERS_PER_READ
                };
                if count == 0 || count > max {
                    return exc(fc, EX_ILLEGAL_VALUE);
                }
                if address + count > 65536 {
                    return exc(fc, EX_ILLEGAL_ADDRESS);
                }
                let mirrored = matches!(fc, FC_READ_COILS | FC_READ_HOLDING);
                let ranges = if fc == FC_READ_COILS {
                    &self.cfg.coils
                } else {
                    &self.cfg.registers
                };
                let exception_mode = self.cfg.unmapped == Unmapped::Exception;
                if exception_mode && !(mirrored && Self::covers(ranges, address, count)) {
                    return exc(fc, EX_ILLEGAL_ADDRESS);
                }
                // 'zero' mode: mirrored blocks end at the datastore size;
                // the unmirrored tables read as zeros.
                if mirrored && address + count > self.size {
                    return exc(fc, EX_ILLEGAL_ADDRESS);
                }
                let st = self.store.read().unwrap();
                let (a, c) = (address as usize, count as usize);
                if bits {
                    let mut out = vec![fc, c.div_ceil(8) as u8];
                    out.resize(2 + c.div_ceil(8), 0);
                    for i in 0..c {
                        let on = fc == FC_READ_COILS && st.coils[a + i];
                        if on {
                            out[2 + i / 8] |= 1 << (i % 8);
                        }
                    }
                    out
                } else {
                    let mut out = vec![fc, (2 * c) as u8];
                    for i in 0..c {
                        let v = if fc == FC_READ_HOLDING {
                            st.registers[a + i]
                        } else {
                            0
                        };
                        out.extend_from_slice(&v.to_be_bytes());
                    }
                    out
                }
            }
            FC_MEI if pdu.len() >= 4 && pdu[1] == 0x0E => {
                // Read Device Identification, basic objects.
                let objects: [(u8, String); 3] = [
                    (0, "ASO+AKO".into()),
                    (1, "DYODE".into()),
                    (2, format!("{}-rust", env!("CARGO_PKG_VERSION"))),
                ];
                let mut out = vec![FC_MEI, 0x0E, pdu[2], 0x01, 0x00, 0x00, objects.len() as u8];
                for (id, v) in objects {
                    out.push(id);
                    out.push(v.len() as u8);
                    out.extend_from_slice(v.as_bytes());
                }
                out
            }
            _ => exc(fc, EX_ILLEGAL_FUNCTION),
        }
    }

    fn serve_client(&self, mut s: TcpStream) {
        s.set_nodelay(true).ok();
        s.set_read_timeout(Some(Duration::from_secs(300))).ok();
        loop {
            let mut head = [0u8; 7];
            if s.read_exact(&mut head).is_err() {
                return;
            }
            let len = usize::from(u16::from_be_bytes([head[4], head[5]]));
            if head[2..4] != [0, 0] || !(2..=254).contains(&len) {
                return;
            }
            let mut pdu = vec![0u8; len - 1];
            if s.read_exact(&mut pdu).is_err() {
                return;
            }
            let resp = self.handle_pdu(&pdu);
            let mut out = Vec::with_capacity(7 + resp.len());
            out.extend_from_slice(&head[0..4]);
            out.extend_from_slice(&((resp.len() + 1) as u16).to_be_bytes());
            out.push(head[6]);
            out.extend_from_slice(&resp);
            if s.write_all(&out).is_err() {
                return;
            }
        }
    }

    /// The Modbus/TCP server.  Runs forever.
    pub fn serve(self: &Arc<Self>) -> Result<(), String> {
        let bind = format!("{}:{}", self.cfg.bind_out, self.cfg.port_out);
        let listener =
            TcpListener::bind(&bind).map_err(|e| format!("cannot listen on {bind}: {e}"))?;
        log::info!(
            "module {:?}: Modbus server on {bind} (any unit id, addresses 0-{}, read-only)",
            self.name,
            self.size.saturating_sub(1)
        );
        for conn in listener.incoming() {
            let Ok(conn) = conn else { continue };
            if self.clients.load(Ordering::Relaxed) >= MAX_SERVER_CLIENTS {
                log::warn!(
                    "module {:?}: too many Modbus clients; refusing one",
                    self.name
                );
                continue;
            }
            self.clients.fetch_add(1, Ordering::Relaxed);
            let me = Arc::clone(self);
            std::thread::Builder::new()
                .name("modbus-client".into())
                .spawn(move || {
                    me.serve_client(conn);
                    me.clients.fetch_sub(1, Ordering::Relaxed);
                })
                .ok();
        }
        Ok(())
    }

    /// Warn when no update has arrived for `stale_after` seconds.  The
    /// server keeps serving the last values: clients cannot tell they are
    /// old, so the log is the only signal.
    pub fn watch_staleness(&self) -> Result<(), String> {
        let started = Instant::now();
        loop {
            std::thread::sleep(Duration::from_secs(1));
            let since = self
                .last_update
                .lock()
                .unwrap()
                .unwrap_or(started)
                .elapsed()
                .as_secs_f64();
            let mut stale = self.stale_logged.lock().unwrap();
            if !*stale && since > self.cfg.stale_after {
                log::warn!(
                    "module {:?}: no update for {since:.0}s, serving stale values",
                    self.name
                );
                *stale = true;
            }
        }
    }
}

/// lidi delivery for a mirror.
pub struct MirrorInbound(pub Arc<Mirror>, pub crate::logging::RefusalLog);

impl MirrorInbound {
    pub fn new(mirror: Arc<Mirror>) -> Self {
        let r = crate::logging::RefusalLog::new(&mirror.name);
        Self(mirror, r)
    }
}

impl Inbound for MirrorInbound {
    fn module_name(&self) -> &str {
        &self.0.name
    }

    fn open_body(&self, hdr: &Header) -> Result<Body, String> {
        if hdr.kind != Kind::Message || hdr.body_len > MAX_MESSAGE {
            return Err("not a Modbus update".into());
        }
        Ok(Body::Memory(Vec::with_capacity(hdr.body_len as usize)))
    }

    fn deliver(&self, hdr: Header, body: Body, _digest: [u8; 32]) {
        let Body::Memory(b) = body else { return };
        match serde_json::from_slice::<Value>(&b) {
            Ok(msg) => {
                self.0.apply(&msg, Some(hdr.boot));
            }
            Err(_) => log::error!("module {:?}: undecodable message", self.0.name),
        }
    }

    fn failed(&self, _hdr: Option<&Header>, _body: Option<Body>, why: Failure) {
        match why {
            // A lost update is superseded by the next poll; not worth a warning.
            Failure::Incomplete { .. } => {
                log::debug!("module {:?}: an update was lost in the diode", self.0.name)
            }
            other => self.1.refused(&other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse_range;

    fn cfg(unmapped: Unmapped) -> ModbusConfig {
        ModbusConfig {
            ip: "127.0.0.1".into(),
            plc_port: 502,
            port_out: 0,
            bind_out: "127.0.0.1".into(),
            unit: 1,
            registers: vec![parse_range("0-10").unwrap()],
            coils: vec![parse_range("0-20").unwrap()],
            interval: 1.0,
            stale_after: 10.0,
            unmapped,
        }
    }

    #[test]
    fn mirror_applies_and_serves_read_only() {
        let m = Mirror::new("m", &cfg(Unmapped::Exception));
        let msg = json!({"seq": 1, "registers": {"0": [1, 2, 65535]}, "coils": {"0": [true, false, true]}});
        assert!(m.apply(&msg, Some([1; 8])));
        // FC3 at 0, count 3
        assert_eq!(
            m.handle_pdu(&[3, 0, 0, 0, 3]),
            vec![3, 6, 0, 1, 0, 2, 0xff, 0xff]
        );
        // FC1 at 0, count 3 -> bits 101
        assert_eq!(m.handle_pdu(&[1, 0, 0, 0, 3]), vec![1, 1, 0b101]);
        // Outside the ranges -> exception 02
        assert_eq!(m.handle_pdu(&[3, 0, 8, 0, 5]), vec![0x83, 2]);
        // Writes -> exception 01
        assert_eq!(m.handle_pdu(&[6, 0, 0, 0, 9]), vec![0x86, 1]);
        assert_eq!(m.handle_pdu(&[16, 0, 0, 0, 1, 2, 0, 9]), vec![0x90, 1]);
        // Input registers are not mirrored
        assert_eq!(m.handle_pdu(&[4, 0, 0, 0, 1]), vec![0x84, 2]);
    }

    #[test]
    fn zero_mode_and_ordering() {
        let m = Mirror::new("m", &cfg(Unmapped::Zero));
        assert_eq!(m.handle_pdu(&[4, 0, 0, 0, 1]), vec![4, 2, 0, 0]);
        assert!(m.apply(&json!({"seq": 5, "registers": {"0": [5]}}), Some([1; 8])));
        assert!(!m.apply(&json!({"seq": 4, "registers": {"0": [4]}}), Some([1; 8])));
        assert_eq!(m.handle_pdu(&[3, 0, 0, 0, 1]), vec![3, 2, 0, 5]);
        // A new sender (restart) starts its own sequence.
        assert!(m.apply(&json!({"seq": 1, "registers": {"0": [7]}}), Some([2; 8])));
        assert_eq!(m.handle_pdu(&[3, 0, 0, 0, 1]), vec![3, 2, 0, 7]);
        assert!(!m.apply(
            &json!({"seq": 9, "registers": {"0": [70000]}}),
            Some([2; 8])
        ));
    }

    #[test]
    fn client_against_mirror_server() {
        let mut c = cfg(Unmapped::Exception);
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        c.port_out = l.local_addr().unwrap().port();
        drop(l);
        let m = Arc::new(Mirror::new("m", &c));
        m.apply(
            &json!({"seq": 1, "registers": {"0": [10, 20, 30]}, "coils": {"0": [false, true]}}),
            None,
        );
        let srv = Arc::clone(&m);
        std::thread::spawn(move || srv.serve());
        std::thread::sleep(Duration::from_millis(200));
        // The mirror is itself a Modbus server: poll it like a PLC.
        let mut pc = c.clone();
        pc.plc_port = c.port_out;
        pc.registers = vec![(0, 3)];
        pc.coils = vec![(0, 2)];
        let mut p = PlcPoller::new("p", &pc);
        let v = p.poll().unwrap();
        assert_eq!(v["registers"]["0"], json!([10, 20, 30]));
        assert_eq!(v["coils"]["0"], json!([false, true]));
        assert_eq!(v["seq"], json!(1));
    }
}
