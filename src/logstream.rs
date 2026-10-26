//! Log streaming through the diode (module type `logs`).
//!
//! Input side (OT): collect log lines from any mix of syslog over UDP
//! (`listen_udp`), syslog over TCP (`listen_tcp`; LF or RFC 6587
//! octet-counted framing) and local files followed like `tail -F`
//! (`tail`), and send them in compressed, numbered batches.
//! Output side (IT): write the lines to files and/or forward them to a SIEM
//! or syslog server over UDP or TCP.
//!
//! A diode has no return path, so nothing can be re-requested.  Instead:
//!
//! * every batch carries a sequence number, so a lost batch is *reported*
//!   (log line + `logs_gap` event) rather than silently missing;
//! * every batch is sent again after a short delay (`copies`, default 2);
//! * batches are zlib-compressed;
//! * a heartbeat keeps the sequence moving when nothing is logged, so the
//!   output side can tell "quiet" from "link down" (`stale_after`);
//! * a bandwidth cap (`max_kbps`) stops a log storm on the OT side from
//!   starving the other modules sharing the link.
//!
//! Each batch is one lidi transfer.  Payload (inside the DYODE envelope):
//! `b"DYLG" | version 1 | zlib(JSON)`, JSON =
//! `{"b": boot id, "s": seq, "t": send time, "d": dropped so far,
//!   "l": [[ts, src, chan, msg(, raw hex)], ...]}`.
//! A line that is not valid UTF-8 carries its original bytes as a fifth
//! element (hex), so the output side writes exactly what was received.

use crate::config::{Forward, ForwardProto, LogFormat, LogsConfig, Module};
use crate::envelope::{Body, Failure, Header, Inbound, Kind};
use crate::hex;
use crate::logging::Event;
use crate::ratelimit::TokenBucket;
use crate::timefmt::{iso_utc, unix_now_f64};
use crate::transport::ModuleLink;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{IpAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub const MAGIC: &[u8; 4] = b"DYLG";
pub const VERSION: u8 = 1;
const STATS_EVERY: Duration = Duration::from_secs(60);
const MAX_PAYLOAD: u64 = 16 * 1024 * 1024;
const MAX_DECOMPRESSED: u64 = 64 * 1024 * 1024;
/// Per-transfer overhead on the wire: envelope + lidi block headers, plus
/// UDP/IP/Ethernet per 1400 bytes, roughly.
const ENVELOPE_OVERHEAD: usize = crate::envelope::FIXED_HEADER + crate::envelope::TRAILER + 16;

/// One log line.
#[derive(Clone, Debug, PartialEq)]
pub struct Rec {
    pub ts: f64,
    pub src: String,
    pub chan: String,
    pub msg: Vec<u8>,
}

pub fn wire_bytes(payload_len: usize) -> usize {
    let chunks = payload_len.div_ceil(1400).max(1);
    payload_len + ENVELOPE_OVERHEAD + chunks * 42
}

/// Bytes off the wire, truncated to `max_line` with a marker.
pub fn clip(raw: &[u8], max_line: usize) -> Vec<u8> {
    if raw.len() <= max_line {
        return raw.to_vec();
    }
    let mut out = raw[..max_line].to_vec();
    out.extend_from_slice(format!(" [dyode: truncated {} bytes]", raw.len() - max_line).as_bytes());
    out
}

/// Escape control characters (except tab) for line-oriented outputs, so
/// one record is always one line and nothing reaches a terminal or parser
/// as an escape sequence.
pub fn escape_controls(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    for &b in raw {
        if (b < 32 && b != b'\t') || b == 127 {
            out.extend_from_slice(format!("\\x{b:02x}").as_bytes());
        } else {
            out.push(b);
        }
    }
    out
}

pub fn safe_name(text: &str) -> String {
    let s: String = text
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    let s = s.trim_matches('.').to_string();
    if s.is_empty() { "unknown".into() } else { s }
}

fn rec_to_json(r: &Rec) -> Value {
    let ts = (r.ts * 1000.0).round() / 1000.0;
    match std::str::from_utf8(&r.msg) {
        Ok(t) => json!([ts, r.src, r.chan, t]),
        Err(_) => json!([
            ts,
            r.src,
            r.chan,
            String::from_utf8_lossy(&r.msg),
            hex::encode(&r.msg)
        ]),
    }
}

pub fn encode_batch(boot: u32, seq: u64, records: &[Rec], dropped: u64) -> Vec<u8> {
    let body = json!({
        "b": boot,
        "s": seq,
        "t": (unix_now_f64() * 1000.0).round() / 1000.0,
        "d": dropped,
        "l": records.iter().map(rec_to_json).collect::<Vec<_>>(),
    });
    let mut out = MAGIC.to_vec();
    out.push(VERSION);
    let mut z = ZlibEncoder::new(out, flate2::Compression::new(6));
    z.write_all(&serde_json::to_vec(&body).unwrap()).unwrap();
    z.finish().unwrap()
}

#[derive(Debug)]
pub struct Batch {
    pub boot: u64,
    pub seq: u64,
    pub dropped: u64,
    pub lines: Vec<Rec>,
}

/// The batch, or None if the payload is not a valid batch.
pub fn decode_batch(payload: &[u8]) -> Option<Batch> {
    if payload.len() < 5 || &payload[..4] != MAGIC || payload[4] != VERSION {
        return None;
    }
    let mut body = Vec::new();
    // Bounded: a decompression bomb is refused, not inflated.
    let mut dec = ZlibDecoder::new(&payload[5..]).take(MAX_DECOMPRESSED + 1);
    dec.read_to_end(&mut body).ok()?;
    if body.len() as u64 > MAX_DECOMPRESSED {
        return None;
    }
    let v: Value = serde_json::from_slice(&body).ok()?;
    let num = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_u64().or_else(|| x.as_f64().map(|f| f as u64)))
    };
    let mut lines = Vec::new();
    for rec in v
        .get("l")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        let a = rec.as_array()?;
        if a.len() < 4 {
            return None;
        }
        let msg = match a.get(4).and_then(Value::as_str) {
            Some(h) => hex::decode(h)?,
            None => a[3].as_str()?.as_bytes().to_vec(),
        };
        lines.push(Rec {
            ts: a[0].as_f64()?,
            src: a[1].as_str()?.to_string(),
            chan: a[2].as_str()?.to_string(),
            msg,
        });
    }
    Some(Batch {
        boot: num("b")?,
        seq: num("s")?,
        dropped: num("d").unwrap_or(0),
        lines,
    })
}

// --------------------------------------------------------------------------
// Input side: the batch sender
// --------------------------------------------------------------------------

#[derive(Default)]
pub struct Stats(Mutex<BTreeMap<&'static str, u64>>);

impl Stats {
    pub fn add(&self, k: &'static str, n: u64) {
        *self.0.lock().unwrap().entry(k).or_insert(0) += n;
    }

    pub fn take(&self) -> Map<String, Value> {
        let mut m = self.0.lock().unwrap();
        let out = m
            .iter()
            .map(|(k, v)| ((*k).to_string(), json!(v)))
            .collect();
        m.clear();
        out
    }
}

struct Queue {
    lines: VecDeque<Rec>,
    dropped: u64,
}

/// The bounded record buffer the sources write into.
pub struct LogBuffer {
    limit: usize,
    q: Mutex<Queue>,
    pub stats: Stats,
}

impl LogBuffer {
    pub fn new(limit: usize) -> Self {
        Self {
            limit,
            q: Mutex::new(Queue {
                lines: VecDeque::new(),
                dropped: 0,
            }),
            stats: Stats::default(),
        }
    }

    /// Called by the sources.  False if the buffer was full.
    pub fn add(&self, src: &str, chan: &str, msg: Vec<u8>) -> bool {
        let mut q = self.q.lock().unwrap();
        if q.lines.len() >= self.limit {
            q.dropped += 1;
            drop(q);
            self.stats.add("dropped", 1);
            return false;
        }
        q.lines.push_back(Rec {
            ts: unix_now_f64(),
            src: src.to_string(),
            chan: chan.to_string(),
            msg,
        });
        drop(q);
        self.stats.add("lines_in", 1);
        true
    }

    pub fn dropped(&self) -> u64 {
        self.q.lock().unwrap().dropped
    }

    fn take_batch(&self, limit: usize) -> (Vec<Rec>, u64) {
        let mut q = self.q.lock().unwrap();
        let mut out = Vec::new();
        let mut size = 0;
        while let Some(r) = q.lines.front() {
            let cost = r.msg.len() + r.src.len() + r.chan.len() + 24;
            if !out.is_empty() && size + cost > limit {
                break;
            }
            size += cost;
            out.push(q.lines.pop_front().unwrap());
        }
        (out, q.dropped)
    }
}

/// Batching + repeats + rate cap.  Owned by the send loop.
pub struct Batcher<F: FnMut(&[u8])> {
    cfg: LogsConfig,
    send: F,
    pub boot: u32,
    seq: u64,
    repeats: VecDeque<(Instant, Arc<Vec<u8>>, u32)>,
    bucket: TokenBucket,
    last_send: Option<Instant>,
}

impl<F: FnMut(&[u8])> Batcher<F> {
    pub fn new(cfg: &LogsConfig, send: F) -> Self {
        let rate = cfg.max_kbps * 1000.0 / 8.0;
        let burst = rate.max(wire_bytes(cfg.batch_bytes) as f64 * 2.0);
        Self {
            cfg: cfg.clone(),
            send,
            boot: crate::auth::random_u32(),
            seq: 0,
            repeats: VecDeque::new(),
            bucket: TokenBucket::new(rate, burst),
            last_send: None,
        }
    }

    fn emit(&mut self, buf: &LogBuffer, records: &[Rec], dropped: u64) {
        let payload = Arc::new(encode_batch(self.boot, self.seq, records, dropped));
        self.seq += 1;
        let cost = wire_bytes(payload.len()) as f64;
        // New data waits for bandwidth; it is never skipped.  While we wait
        // the buffer fills, and overflow is counted and reported.
        while !self.bucket.try_take(cost) {
            std::thread::sleep(self.bucket.wait_time(cost).min(Duration::from_secs(1)));
        }
        (self.send)(&payload);
        let now = Instant::now();
        self.last_send = Some(now);
        buf.stats.add("batches", 1);
        buf.stats.add("lines_sent", records.len() as u64);
        buf.stats.add("wire_bytes", cost as u64);
        if self.cfg.copies > 1 {
            let delay = Duration::from_secs_f64(self.cfg.repeat_delay());
            self.repeats
                .push_back((now + delay, payload, self.cfg.copies - 1));
        }
    }

    fn send_repeats(&mut self, buf: &LogBuffer) {
        let now = Instant::now();
        let delay = Duration::from_secs_f64(self.cfg.repeat_delay());
        let mut keep = VecDeque::new();
        while let Some((due, payload, remaining)) = self.repeats.pop_front() {
            if due > now {
                keep.push_back((due, payload, remaining));
                continue;
            }
            let cost = wire_bytes(payload.len()) as f64;
            // Repeats are insurance: under bandwidth pressure they are
            // dropped in favour of new data.
            if self.bucket.try_take(cost) {
                (self.send)(&payload);
                buf.stats.add("repeats", 1);
                buf.stats.add("wire_bytes", cost as u64);
            } else {
                buf.stats.add("repeats_skipped", 1);
            }
            if remaining > 1 {
                keep.push_back((now + delay, payload, remaining - 1));
            }
        }
        self.repeats = keep;
    }

    /// Send everything queued, due repeats, and a heartbeat if idle.
    pub fn tick(&mut self, buf: &LogBuffer) {
        let mut sent_any = false;
        loop {
            let (records, dropped) = buf.take_batch(self.cfg.batch_bytes);
            if records.is_empty() {
                break;
            }
            self.emit(buf, &records, dropped);
            sent_any = true;
            self.send_repeats(buf);
        }
        if !sent_any {
            let idle = self
                .last_send
                .is_none_or(|t| t.elapsed().as_secs_f64() >= self.cfg.heartbeat);
            if idle {
                let d = buf.dropped();
                self.emit(buf, &[], d);
            }
        }
        self.send_repeats(buf);
    }
}

// --------------------------------------------------------------------------
// Input side: sources
// --------------------------------------------------------------------------

fn allowed(cfg: &LogsConfig, ip: IpAddr) -> bool {
    cfg.allow_from.is_empty() || cfg.allow_from.iter().any(|n| n.contains(ip))
}

fn trim_end(mut b: &[u8]) -> &[u8] {
    while let Some((&last, rest)) = b.split_last() {
        if matches!(last, b'\r' | b'\n' | 0) {
            b = rest;
        } else {
            break;
        }
    }
    b
}

pub fn run_udp_source(sock: UdpSocket, buf: Arc<LogBuffer>, cfg: LogsConfig) {
    let mut data = vec![0u8; 65536];
    loop {
        let (n, peer) = match sock.recv_from(&mut data) {
            Ok(v) => v,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(e) => {
                log::error!("syslog UDP source failed: {e}");
                return;
            }
        };
        if !allowed(&cfg, peer.ip()) {
            buf.stats.add("rejected", 1);
            continue;
        }
        let msg = trim_end(&data[..n]);
        if !msg.is_empty() {
            buf.add(&peer.ip().to_string(), "udp", clip(msg, cfg.max_line));
        }
    }
}

/// Split a TCP syslog stream into messages, handling both RFC 6587
/// framings per message: octet counting ('123 <13>1 ...': digits, space,
/// then the '<' of a PRI) and non-transparent (LF- or NUL-terminated).
pub struct StreamFramer {
    max_line: usize,
    buf: Vec<u8>,
    skipping: bool,
}

impl StreamFramer {
    pub fn new(max_line: usize) -> Self {
        Self {
            max_line,
            buf: Vec::new(),
            skipping: false,
        }
    }

    fn eol(&self) -> Option<usize> {
        self.buf.iter().position(|&b| b == b'\n' || b == 0)
    }

    fn octet_count(&self) -> Option<(usize, usize)> {
        let head = &self.buf[..self.buf.len().min(16)];
        let digits = head.iter().take_while(|b| b.is_ascii_digit()).count();
        if digits == 0
            || digits > 7
            || head.len() < digits + 2
            || head[digits] != b' '
            || head[digits + 1] != b'<'
        {
            return None;
        }
        let n: usize = std::str::from_utf8(&head[..digits]).ok()?.parse().ok()?;
        Some((n, digits + 1))
    }

    pub fn feed(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        self.buf.extend_from_slice(data);
        let mut out = Vec::new();
        while !self.buf.is_empty() {
            if self.skipping {
                match self.eol() {
                    None => {
                        self.buf.clear();
                        break;
                    }
                    Some(i) => {
                        self.buf.drain(..=i);
                        self.skipping = false;
                        continue;
                    }
                }
            }
            // An absurd length is not trusted; it falls through to LF framing.
            if let Some((len, start)) = self.octet_count().filter(|(n, _)| *n <= self.max_line * 4)
            {
                if self.buf.len() < start + len {
                    break;
                }
                let msg = self.buf[start..start + len].to_vec();
                self.buf.drain(..start + len);
                out.push(msg[..msg.len().min(self.max_line + 1)].to_vec());
                continue;
            }
            match self.eol() {
                None => {
                    if self.buf.len() > self.max_line {
                        out.push(self.buf[..self.max_line + 1].to_vec());
                        self.buf.clear();
                        self.skipping = true;
                    }
                    break;
                }
                Some(i) => {
                    let mut line = &self.buf[..i];
                    if line.last() == Some(&b'\r') {
                        line = &line[..line.len() - 1];
                    }
                    let line = line.to_vec();
                    self.buf.drain(..=i);
                    if !line.is_empty() {
                        out.push(line);
                    }
                }
            }
        }
        out
    }
}

pub fn run_tcp_source(listener: TcpListener, buf: Arc<LogBuffer>, cfg: LogsConfig) {
    let clients = Arc::new(AtomicUsize::new(0));
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let Ok(peer) = conn.peer_addr() else { continue };
        if !allowed(&cfg, peer.ip()) {
            buf.stats.add("rejected", 1);
            continue;
        }
        if clients.load(Ordering::Relaxed) >= cfg.max_tcp_clients {
            log::warn!("too many syslog TCP clients; refusing {}", peer.ip());
            continue;
        }
        clients.fetch_add(1, Ordering::Relaxed);
        let (buf, cfg, count) = (Arc::clone(&buf), cfg.clone(), Arc::clone(&clients));
        let spawned = std::thread::Builder::new()
            .name(format!("logs-tcp-{}", peer.ip()))
            .spawn(move || {
                let mut conn = conn;
                crate::netutil::set_keepalive(&conn);
                let mut framer = StreamFramer::new(cfg.max_line);
                let mut data = vec![0u8; 65536];
                let src = peer.ip().to_string();
                loop {
                    match conn.read(&mut data) {
                        Ok(0) => break,
                        Ok(n) => {
                            for msg in framer.feed(&data[..n]) {
                                buf.add(&src, "tcp", clip(&msg, cfg.max_line));
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(e) => {
                            log::debug!("tcp client {src}: {e}");
                            break;
                        }
                    }
                }
                count.fetch_sub(1, Ordering::Relaxed);
            });
        if spawned.is_err() {
            clients.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

struct Tailed {
    file: File,
    ino: u64,
    pos: u64,
    partial: Vec<u8>,
}

/// Follow files matching glob patterns, like `tail -F`: rotation by rename
/// (the old file is read to its end first), truncation (restart at 0),
/// files appearing later (read from the start), and optional offset
/// persistence (`tail_state`) so a restart resumes where it stopped.
pub struct FileTailer {
    patterns: Vec<String>,
    from_start: bool,
    state_path: Option<PathBuf>,
    max_line: usize,
    buf: Arc<LogBuffer>,
    host: String,
    files: BTreeMap<String, Tailed>,
    first_scan: bool,
    saved: HashMap<String, (u64, u64)>,
    last_save: Option<Instant>,
}

pub fn hostname() -> String {
    let mut b = [0u8; 256];
    // SAFETY: buffer valid for its length.
    if unsafe { libc::gethostname(b.as_mut_ptr().cast(), b.len()) } == 0 {
        let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
        return String::from_utf8_lossy(&b[..n]).into_owned();
    }
    "localhost".into()
}

impl FileTailer {
    pub fn new(cfg: &LogsConfig, buf: Arc<LogBuffer>) -> Self {
        let saved = cfg
            .tail_state
            .as_ref()
            .and_then(|p| fs::read(p).ok())
            .and_then(|b| serde_json::from_slice::<HashMap<String, (u64, u64)>>(&b).ok())
            .unwrap_or_default();
        Self {
            patterns: cfg.tail.clone(),
            from_start: cfg.tail_from_start,
            state_path: cfg.tail_state.clone(),
            max_line: cfg.max_line,
            buf,
            host: hostname(),
            files: BTreeMap::new(),
            first_scan: true,
            saved,
            last_save: None,
        }
    }

    pub fn save_state(&mut self, force: bool) {
        let Some(path) = self.state_path.clone() else {
            return;
        };
        if !force
            && self
                .last_save
                .is_some_and(|t| t.elapsed() < Duration::from_secs(5))
        {
            return;
        }
        self.last_save = Some(Instant::now());
        let data: HashMap<&String, (u64, u64)> = self
            .files
            .iter()
            .map(|(p, f)| (p, (f.ino, f.pos - f.partial.len() as u64)))
            .collect();
        let tmp = path.with_extension("tmp");
        let res = path
            .parent()
            .map_or(Ok(()), fs::create_dir_all)
            .and_then(|()| fs::write(&tmp, serde_json::to_vec(&data).unwrap()))
            .and_then(|()| fs::rename(&tmp, &path));
        if let Err(e) = res {
            log::warn!("cannot save tail state to {}: {e}", path.display());
        }
    }

    fn open(&mut self, path: &str, ino: u64, size: u64, at_end: bool) {
        let mut file = match File::open(path) {
            Ok(f) => f,
            Err(e) => {
                log::debug!("cannot open {path}: {e}");
                return;
            }
        };
        let pos = match self.saved.remove(path) {
            Some((sino, off)) if sino == ino && off <= size => off,
            _ if at_end => size,
            _ => 0,
        };
        if file.seek(SeekFrom::Start(pos)).is_err() {
            return;
        }
        self.files.insert(
            path.to_string(),
            Tailed {
                file,
                ino,
                pos,
                partial: Vec::new(),
            },
        );
        log::info!("tailing {path}");
    }

    fn drain(buf: &LogBuffer, host: &str, max_line: usize, path: &str, f: &mut Tailed, last: bool) {
        let chan = format!("file:{path}");
        let mut chunk = vec![0u8; 65536];
        loop {
            let n = match f.file.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            f.pos += n as u64;
            f.partial.extend_from_slice(&chunk[..n]);
            while let Some(i) = f.partial.iter().position(|&b| b == b'\n') {
                let mut line: Vec<u8> = f.partial.drain(..=i).collect();
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                if !line.is_empty() {
                    buf.add(host, &chan, clip(&line, max_line));
                }
            }
            if f.partial.len() > max_line {
                let p = std::mem::take(&mut f.partial);
                buf.add(host, &chan, clip(&p, max_line));
            }
        }
        if last && !f.partial.is_empty() {
            let p = std::mem::take(&mut f.partial);
            buf.add(host, &chan, clip(&p, max_line));
        }
    }

    pub fn poll(&mut self) {
        let mut seen = Vec::new();
        for pattern in self.patterns.clone() {
            let Ok(paths) = glob::glob(&pattern) else {
                continue;
            };
            for p in paths.flatten() {
                let Ok(md) = fs::metadata(&p) else { continue };
                if !md.is_file() {
                    continue;
                }
                let path = p.to_string_lossy().into_owned();
                seen.push(path.clone());
                let (ino, size) = (md.ino(), md.size());
                match self.files.get(&path).map(|f| (f.ino, f.pos)) {
                    None => {
                        let at_end = self.first_scan && !self.from_start;
                        self.open(&path, ino, size, at_end);
                    }
                    Some((old_ino, _)) if old_ino != ino => {
                        // Renamed away: finish the old file, then start the new.
                        let mut f = self.files.remove(&path).unwrap();
                        Self::drain(&self.buf, &self.host, self.max_line, &path, &mut f, true);
                        self.open(&path, ino, size, false);
                    }
                    Some((_, pos)) if size < pos => {
                        log::info!("{path} was truncated; reading from the start");
                        let f = self.files.get_mut(&path).unwrap();
                        let _ = f.file.seek(SeekFrom::Start(0));
                        f.pos = 0;
                        f.partial.clear();
                    }
                    Some(_) => {}
                }
                if let Some(f) = self.files.get_mut(&path) {
                    Self::drain(&self.buf, &self.host, self.max_line, &path, f, false);
                }
            }
        }
        let gone: Vec<String> = self
            .files
            .keys()
            .filter(|p| !seen.contains(p))
            .cloned()
            .collect();
        for path in gone {
            let mut f = self.files.remove(&path).unwrap();
            Self::drain(&self.buf, &self.host, self.max_line, &path, &mut f, true);
        }
        self.first_scan = false;
        self.save_state(false);
    }
}

/// Input agent.  Binds the sources here, so a bad address or a port in use
/// fails the module start loudly (and the supervisor retries).
pub fn run_input(module: &Module, cfg: &LogsConfig, link: &ModuleLink) -> Result<(), String> {
    let buf = Arc::new(LogBuffer::new(cfg.buffer_lines));
    let mut threads: Vec<(String, JoinHandle<()>)> = Vec::new();
    let mut sources = Vec::new();
    if let Some((host, port)) = &cfg.listen_udp {
        let sock = UdpSocket::bind((host.as_str(), *port))
            .map_err(|e| format!("cannot bind syslog udp {host}:{port}: {e}"))?;
        crate::netutil::set_rcvbuf(&sock, 4 * 1024 * 1024);
        let (b, c) = (Arc::clone(&buf), cfg.clone());
        threads.push((
            "logs-udp".into(),
            std::thread::Builder::new()
                .name("logs-udp".into())
                .spawn(move || run_udp_source(sock, b, c))
                .unwrap(),
        ));
        sources.push(format!("udp {host}:{port}"));
    }
    if let Some((host, port)) = &cfg.listen_tcp {
        let l = TcpListener::bind((host.as_str(), *port))
            .map_err(|e| format!("cannot bind syslog tcp {host}:{port}: {e}"))?;
        let (b, c) = (Arc::clone(&buf), cfg.clone());
        threads.push((
            "logs-tcp".into(),
            std::thread::Builder::new()
                .name("logs-tcp".into())
                .spawn(move || run_tcp_source(l, b, c))
                .unwrap(),
        ));
        sources.push(format!("tcp {host}:{port}"));
    }
    if !cfg.tail.is_empty() {
        let mut tailer = FileTailer::new(cfg, Arc::clone(&buf));
        threads.push((
            "logs-tail".into(),
            std::thread::Builder::new()
                .name("logs-tail".into())
                .spawn(move || {
                    loop {
                        tailer.poll();
                        std::thread::sleep(Duration::from_millis(500));
                    }
                })
                .unwrap(),
        ));
        sources.push(format!("{} file pattern(s)", cfg.tail.len()));
    }
    log::info!(
        "module {:?}: collecting logs from {}, endpoint {} (cap {} kbit/s, {} cop{})",
        module.name,
        sources.join(", "),
        module.index,
        cfg.max_kbps,
        cfg.copies,
        if cfg.copies == 1 { "y" } else { "ies" }
    );
    let name = module.name.clone();
    let mut send_fail =
        crate::logging::FailureLog::new(format!("module {name:?}: sending through lidi"));
    let mut batcher = Batcher::new(cfg, |payload: &[u8]| {
        match link.send_message(b"", payload) {
            Ok(()) => send_fail.ok(),
            Err(e) => send_fail.fail(e),
        }
    });
    let mut last_stats = Instant::now();
    let mut reported_drop = 0;
    loop {
        std::thread::sleep(Duration::from_secs_f64(cfg.batch_interval));
        for (tname, t) in &threads {
            if t.is_finished() {
                return Err(format!("log source thread {tname} died"));
            }
        }
        batcher.tick(&buf);
        let d = buf.dropped();
        if d > reported_drop {
            log::warn!(
                "module {name:?}: buffer full, {d} line(s) dropped so far (input faster than max_kbps allows)"
            );
            reported_drop = d;
        }
        if last_stats.elapsed() >= STATS_EVERY {
            last_stats = Instant::now();
            Event::new("logs_stats")
                .module(&name)
                .fields(buf.stats.take())
                .emit();
        }
    }
}

// --------------------------------------------------------------------------
// Output side: sinks
// --------------------------------------------------------------------------

pub trait Sink: Send {
    fn write(&mut self, records: &[Rec]) -> std::io::Result<()>;
}

/// Append lines to <out>/<out_file>, or one file per source.  Files are
/// re-opened when logrotate moves them away, and created 0640.
pub struct FileSink {
    dir: PathBuf,
    out_file: String,
    split: bool,
    fmt: LogFormat,
    handles: Vec<(PathBuf, File, u64)>,
}

impl FileSink {
    const MAX_OPEN: usize = 128;

    pub fn new(dir: &Path, out_file: &str, split: bool, fmt: LogFormat) -> std::io::Result<Self> {
        std::os::unix::fs::DirBuilderExt::mode(fs::DirBuilder::new().recursive(true), 0o750)
            .create(dir)?;
        Ok(Self {
            dir: dir.to_path_buf(),
            out_file: out_file.to_string(),
            split,
            fmt,
            handles: Vec::new(),
        })
    }

    fn path(&self, src: &str) -> PathBuf {
        if self.split {
            self.dir.join(format!("{}.log", safe_name(src)))
        } else {
            self.dir.join(&self.out_file)
        }
    }

    fn handle(&mut self, path: &Path) -> std::io::Result<&mut File> {
        let ino = fs::metadata(path).ok().map(|m| m.ino());
        if let Some(i) = self.handles.iter().position(|h| h.0 == path) {
            let h = self.handles.remove(i);
            if Some(h.2) == ino {
                self.handles.push(h);
                return Ok(&mut self.handles.last_mut().unwrap().1);
            }
        }
        let f = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o640)
            .open(path)?;
        let ino = f.metadata()?.ino();
        self.handles.push((path.to_path_buf(), f, ino));
        while self.handles.len() > Self::MAX_OPEN {
            self.handles.remove(0);
        }
        Ok(&mut self.handles.last_mut().unwrap().1)
    }

    pub fn format(&self, r: &Rec) -> Vec<u8> {
        match self.fmt {
            LogFormat::Json => serde_json::to_vec(&json!({
                "ts": iso_utc(r.ts), "src": r.src, "chan": r.chan, "msg": String::from_utf8_lossy(&r.msg)
            }))
            .unwrap(),
            LogFormat::Raw => escape_controls(&r.msg),
            LogFormat::Prefixed => {
                let mut out = format!("{} {} ", iso_utc(r.ts), r.src).into_bytes();
                out.extend_from_slice(&escape_controls(&r.msg));
                out
            }
        }
    }
}

impl Sink for FileSink {
    fn write(&mut self, records: &[Rec]) -> std::io::Result<()> {
        let mut groups: Vec<(PathBuf, Vec<u8>)> = Vec::new();
        for r in records {
            let p = self.path(&r.src);
            let mut line = self.format(r);
            line.push(b'\n');
            match groups.iter_mut().find(|g| g.0 == p) {
                Some(g) => g.1.extend_from_slice(&line),
                None => groups.push((p, line)),
            }
        }
        for (p, data) in groups {
            self.handle(&p)?.write_all(&data)?;
        }
        Ok(())
    }
}

/// Give a non-syslog line (from a tailed file, say) an RFC 5424 header, so
/// the SIEM sees a proper timestamp and the original host.  Lines that
/// already start with a <PRI> are passed through untouched.
pub fn syslog_wrap(r: &Rec) -> Vec<u8> {
    let m = &r.msg;
    let digits = m.iter().skip(1).take_while(|b| b.is_ascii_digit()).count();
    if m.first() == Some(&b'<') && (1..=3).contains(&digits) && m.get(1 + digits) == Some(&b'>') {
        return m.clone();
    }
    // facility user(1), severity notice(5) -> PRI 13
    let mut out =
        format!("<13>1 {} {} dyode - - - ", iso_utc(r.ts), safe_name(&r.src)).into_bytes();
    out.extend_from_slice(m);
    out
}

pub struct UdpForwarder {
    target: String,
    wrap: bool,
    sock: UdpSocket,
    errors: u64,
}

impl UdpForwarder {
    pub fn new(f: &Forward, wrap: bool) -> std::io::Result<Self> {
        Ok(Self {
            target: format!("{}:{}", f.host, f.port),
            wrap,
            sock: UdpSocket::bind("0.0.0.0:0")?,
            errors: 0,
        })
    }
}

impl Sink for UdpForwarder {
    fn write(&mut self, records: &[Rec]) -> std::io::Result<()> {
        for r in records {
            let data = if self.wrap {
                syslog_wrap(r)
            } else {
                r.msg.clone()
            };
            if let Err(e) = self
                .sock
                .send_to(&data[..data.len().min(65000)], &self.target)
            {
                self.errors += 1;
                if self.errors == 1 || self.errors % 1000 == 0 {
                    log::warn!(
                        "forward to udp://{} failed ({} so far): {e}",
                        self.target,
                        self.errors
                    );
                }
            }
        }
        Ok(())
    }
}

struct TcpQueue {
    lines: VecDeque<Vec<u8>>,
    dropped: u64,
}

/// LF-framed syslog over TCP, with reconnect and a bounded queue, in its
/// own thread so a slow or absent SIEM never stalls the receiver.  Lines in
/// flight when a connection breaks can be lost: TCP syslog without RELP
/// has no application-level acknowledgement.
pub struct TcpForwarder {
    wrap: bool,
    shared: Arc<(Mutex<TcpQueue>, Condvar)>,
    max_queue: usize,
}

impl TcpForwarder {
    pub fn new(f: &Forward, wrap: bool) -> Self {
        let shared = Arc::new((
            Mutex::new(TcpQueue {
                lines: VecDeque::new(),
                dropped: 0,
            }),
            Condvar::new(),
        ));
        let target = format!("{}:{}", f.host, f.port);
        let s2 = Arc::clone(&shared);
        std::thread::Builder::new()
            .name(format!("logs-fwd-{}", f.host).chars().take(15).collect())
            .spawn(move || Self::run(&target, &s2))
            .expect("thread");
        Self {
            wrap,
            shared,
            max_queue: 200_000,
        }
    }

    fn run(target: &str, shared: &(Mutex<TcpQueue>, Condvar)) {
        let mut backoff = 1.0f64;
        let mut warned = false;
        loop {
            let conn = target
                .to_socket_addrs()
                .ok()
                .and_then(|mut a| a.next())
                .ok_or_else(|| std::io::Error::other("cannot resolve"))
                .and_then(|a| TcpStream::connect_timeout(&a, Duration::from_secs(10)));
            let mut s = match conn {
                Ok(s) => s,
                Err(e) => {
                    if !warned {
                        log::warn!(
                            "cannot connect to tcp://{target} ({e}); retrying, lines are queued meanwhile"
                        );
                        warned = true;
                    }
                    std::thread::sleep(Duration::from_secs_f64(backoff));
                    backoff = (backoff * 2.0).min(30.0);
                    continue;
                }
            };
            if warned {
                log::info!("connected to tcp://{target}");
            }
            warned = false;
            backoff = 1.0;
            crate::netutil::set_keepalive(&s);
            loop {
                let chunk: Vec<Vec<u8>> = {
                    let (m, cv) = shared;
                    let mut q = m.lock().unwrap();
                    while q.lines.is_empty() {
                        q = cv.wait_timeout(q, Duration::from_secs(1)).unwrap().0;
                    }
                    let n = q.lines.len().min(1000);
                    q.lines.drain(..n).collect()
                };
                if let Err(e) = s.write_all(&chunk.concat()) {
                    log::warn!("lost connection to tcp://{target}: {e}");
                    break;
                }
            }
        }
    }
}

impl Sink for TcpForwarder {
    fn write(&mut self, records: &[Rec]) -> std::io::Result<()> {
        let (m, cv) = &*self.shared;
        let mut q = m.lock().unwrap();
        for r in records {
            if q.lines.len() >= self.max_queue {
                q.dropped += 1;
                continue;
            }
            let data = if self.wrap {
                syslog_wrap(r)
            } else {
                r.msg.clone()
            };
            let mut line = escape_controls(&data);
            line.push(b'\n');
            q.lines.push_back(line);
        }
        cv.notify_one();
        Ok(())
    }
}

pub fn make_sinks(cfg: &LogsConfig) -> Result<Vec<Box<dyn Sink>>, String> {
    let mut sinks: Vec<Box<dyn Sink>> = Vec::new();
    if let Some(out) = &cfg.out {
        sinks.push(Box::new(
            FileSink::new(out, &cfg.out_file, cfg.split_by_source, cfg.format)
                .map_err(|e| format!("cannot use {}: {e}", out.display()))?,
        ));
    }
    for f in &cfg.forward {
        match f.proto {
            ForwardProto::Udp => sinks.push(Box::new(
                UdpForwarder::new(f, cfg.forward_wrap).map_err(|e| e.to_string())?,
            )),
            ForwardProto::Tcp => sinks.push(Box::new(TcpForwarder::new(f, cfg.forward_wrap))),
        }
    }
    Ok(sinks)
}

// --------------------------------------------------------------------------
// Output side: sequencing
// --------------------------------------------------------------------------

/// Turn possibly duplicated, reordered, lossy batches into an in-order
/// stream of lines, and account for everything that went missing.
pub struct LogReceiver {
    sinks: Vec<Box<dyn Sink>>,
    window: f64,
    stale_after: f64,
    module: String,
    boot: Option<u64>,
    next_seq: u64,
    buffer: BTreeMap<u64, Batch>,
    gap_since: Option<Instant>,
    last_rx: Option<Instant>,
    stale: bool,
    src_dropped: u64,
    pub stats: Stats,
}

impl LogReceiver {
    const MAX_BUFFERED: usize = 2000;

    pub fn new(sinks: Vec<Box<dyn Sink>>, cfg: &LogsConfig, module: &str) -> Self {
        Self {
            sinks,
            window: cfg.reorder_window,
            stale_after: cfg.stale_after,
            module: module.to_string(),
            boot: None,
            next_seq: 0,
            buffer: BTreeMap::new(),
            gap_since: None,
            last_rx: None,
            stale: false,
            src_dropped: 0,
            stats: Stats::default(),
        }
    }

    pub fn feed(&mut self, payload: &[u8]) {
        let Some(batch) = decode_batch(payload) else {
            self.stats.add("bad", 1);
            log::warn!(
                "module {:?}: received something that is not a valid log batch; ignored",
                self.module
            );
            return;
        };
        self.last_rx = Some(Instant::now());
        if self.stale {
            self.stale = false;
            log::info!("module {:?}: log stream resumed", self.module);
            Event::new("logs_resumed").module(&self.module).emit();
        }
        if Some(batch.boot) != self.boot {
            if self.boot.is_some() {
                self.resolve_gap(true);
                log::info!(
                    "module {:?}: input side restarted (new stream)",
                    self.module
                );
                Event::new("logs_sender_restart")
                    .module(&self.module)
                    .emit();
            }
            self.boot = Some(batch.boot);
            self.next_seq = batch.seq;
            self.buffer.clear();
            self.gap_since = None;
            self.src_dropped = 0;
        }
        let seq = batch.seq;
        if seq < self.next_seq || self.buffer.contains_key(&seq) {
            self.stats.add("duplicates", 1);
            return;
        }
        self.buffer.insert(seq, batch);
        self.deliver();
        if !self.buffer.is_empty() && self.gap_since.is_none() {
            self.gap_since = Some(Instant::now());
        }
        if self.buffer.len() > Self::MAX_BUFFERED {
            self.resolve_gap(true);
        }
    }

    fn deliver(&mut self) {
        while let Some(batch) = self.buffer.remove(&self.next_seq) {
            self.next_seq += 1;
            self.stats.add("batches", 1);
            if batch.dropped > self.src_dropped {
                let lost = batch.dropped - self.src_dropped;
                self.src_dropped = batch.dropped;
                log::warn!(
                    "module {:?}: input side dropped {lost} line(s): its buffer was full",
                    self.module
                );
                Event::new("logs_dropped_at_source")
                    .module(&self.module)
                    .field("lines", lost)
                    .emit();
            }
            if !batch.lines.is_empty() {
                self.stats.add("lines", batch.lines.len() as u64);
                for s in &mut self.sinks {
                    if let Err(e) = s.write(&batch.lines) {
                        log::error!("module {:?}: cannot write logs: {e}", self.module);
                    }
                }
            }
        }
        if self.buffer.is_empty() {
            self.gap_since = None;
        }
    }

    fn resolve_gap(&mut self, force: bool) {
        loop {
            let Some(&first) = self.buffer.keys().next() else {
                return;
            };
            let lost = first - self.next_seq;
            if lost > 0 {
                self.stats.add("lost_batches", lost);
                log::warn!(
                    "module {:?}: {lost} log batch(es) lost in the diode (sequence {}-{})",
                    self.module,
                    self.next_seq,
                    first - 1
                );
                Event::new("logs_gap")
                    .module(&self.module)
                    .field("batches", lost)
                    .field("first", self.next_seq)
                    .field("last", first - 1)
                    .emit();
            }
            self.next_seq = first;
            self.gap_since = None;
            self.deliver();
            if self.buffer.is_empty() {
                return;
            }
            self.gap_since = Some(Instant::now());
            if !force {
                return;
            }
        }
    }

    pub fn poll(&mut self) {
        if self
            .gap_since
            .is_some_and(|t| t.elapsed().as_secs_f64() >= self.window)
        {
            self.resolve_gap(false);
        }
        if let Some(t) = self.last_rx {
            let silent = t.elapsed().as_secs_f64();
            if !self.stale && silent > self.stale_after {
                self.stale = true;
                log::warn!(
                    "module {:?}: no log batches or heartbeats for {silent:.0}s; the link or the input side may be down",
                    self.module
                );
                Event::new("logs_stale")
                    .module(&self.module)
                    .field("silent_s", (silent * 10.0).round() / 10.0)
                    .emit();
            }
        }
    }
}

/// lidi delivery for a logs module: hand payloads to the receiver thread.
pub struct LogsInbound {
    pub name: String,
    pub tx: crossbeam_channel::Sender<Vec<u8>>,
    pub incomplete: AtomicUsize,
    refusals: crate::logging::RefusalLog,
}

impl LogsInbound {
    pub fn new(name: &str, tx: crossbeam_channel::Sender<Vec<u8>>) -> Self {
        Self {
            name: name.to_string(),
            tx,
            incomplete: AtomicUsize::new(0),
            refusals: crate::logging::RefusalLog::new(name),
        }
    }
}

impl Inbound for LogsInbound {
    fn module_name(&self) -> &str {
        &self.name
    }

    fn open_body(&self, hdr: &Header) -> Result<Body, String> {
        if hdr.kind != Kind::Message || hdr.body_len > MAX_PAYLOAD {
            return Err("not a log batch".into());
        }
        Ok(Body::Memory(Vec::with_capacity(hdr.body_len as usize)))
    }

    fn deliver(&self, _hdr: Header, body: Body, _digest: [u8; 32]) {
        if let Body::Memory(b) = body {
            let _ = self.tx.send(b);
        }
    }

    fn failed(&self, _hdr: Option<&Header>, _body: Option<Body>, why: Failure) {
        match why {
            // The sequence numbers report the gap if no copy gets through.
            Failure::Incomplete { .. } => {
                self.incomplete.fetch_add(1, Ordering::Relaxed);
            }
            other => self.refusals.refused(&other),
        }
    }
}

/// Output agent: sequence batches and write them to the sinks.
pub fn run_output(
    module: &Module,
    cfg: &LogsConfig,
    rx: &crossbeam_channel::Receiver<Vec<u8>>,
    inbound: &LogsInbound,
) -> Result<(), String> {
    let sinks = make_sinks(cfg)?;
    let mut where_ = Vec::new();
    if let Some(out) = &cfg.out {
        where_.push(
            out.join(if cfg.split_by_source {
                "<source>.log"
            } else {
                &cfg.out_file
            })
            .display()
            .to_string(),
        );
    }
    where_.extend(cfg.forward.iter().map(ToString::to_string));
    log::info!(
        "module {:?}: receiving logs on endpoint {} -> {}",
        module.name,
        module.index,
        where_.join(", ")
    );
    let mut recv = LogReceiver::new(sinks, cfg, &module.name);
    let mut last_stats = Instant::now();
    loop {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(p) => recv.feed(&p),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                return Err("inbound channel closed".into());
            }
        }
        recv.poll();
        if last_stats.elapsed() >= STATS_EVERY {
            last_stats = Instant::now();
            let mut stats = recv.stats.take();
            stats.insert(
                "incomplete_messages".into(),
                json!(inbound.incomplete.swap(0, Ordering::Relaxed)),
            );
            Event::new("logs_stats")
                .module(&module.name)
                .fields(stats)
                .emit();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_roundtrip_keeps_raw_bytes() {
        let recs = vec![
            Rec {
                ts: 1.5,
                src: "10.0.0.1".into(),
                chan: "udp".into(),
                msg: b"hello".to_vec(),
            },
            Rec {
                ts: 2.0,
                src: "h".into(),
                chan: "file:/x".into(),
                msg: vec![0xff, b'a', 0xfe],
            },
        ];
        let p = encode_batch(7, 3, &recs, 2);
        let b = decode_batch(&p).unwrap();
        assert_eq!((b.boot, b.seq, b.dropped), (7, 3, 2));
        assert_eq!(b.lines, recs);
        assert!(decode_batch(b"DYLG\x01garbage").is_none());
    }

    #[test]
    fn framer() {
        let mut f = StreamFramer::new(64);
        let out = f.feed(b"12 <13>1 hello!plain line\r\n123abc no\n");
        assert_eq!(
            out,
            vec![
                b"<13>1 hello!".to_vec(),
                b"plain line".to_vec(),
                b"123abc no".to_vec()
            ]
        );
        let out = f.feed(&[b'x'; 100]);
        assert_eq!(out.len(), 1);
        assert_eq!(f.feed(b"rest of the long line\nok\n"), vec![b"ok".to_vec()]);
    }

    #[test]
    fn controls_and_wrap() {
        assert_eq!(
            escape_controls(b"a\x1b[31m\tb\x7f"),
            b"a\\x1b[31m\tb\\x7f".to_vec()
        );
        let r = Rec {
            ts: 0.0,
            src: "plc 1".into(),
            chan: "c".into(),
            msg: b"x".to_vec(),
        };
        assert_eq!(
            syslog_wrap(&r),
            b"<13>1 1970-01-01T00:00:00.000Z plc_1 dyode - - - x".to_vec()
        );
        let r2 = Rec {
            msg: b"<34>1 already".to_vec(),
            ..r
        };
        assert_eq!(syslog_wrap(&r2), b"<34>1 already".to_vec());
    }

    struct Mem(Arc<Mutex<Vec<Vec<u8>>>>);
    impl Sink for Mem {
        fn write(&mut self, records: &[Rec]) -> std::io::Result<()> {
            self.0
                .lock()
                .unwrap()
                .extend(records.iter().map(|r| r.msg.clone()));
            Ok(())
        }
    }

    fn logs_cfg() -> LogsConfig {
        let t = "modules:\n  l:\n    type: logs\n    listen_udp: 514\n    out: /tmp/x\n    reorder_window: 0\n    copies: 1\n";
        let c = crate::config::parse(t, Path::new("c"), Path::new("/")).unwrap();
        match c.modules[0].kind.clone() {
            crate::config::ModuleKind::Logs(l) => l,
            _ => unreachable!(),
        }
    }

    #[test]
    fn receiver_orders_dedups_and_reports_gaps() {
        let mut cfg = logs_cfg();
        cfg.reorder_window = 0.0;
        let got = Arc::new(Mutex::new(Vec::new()));
        let mut r = LogReceiver::new(vec![Box::new(Mem(Arc::clone(&got)))], &cfg, "l");
        let line = |s: &str| {
            vec![Rec {
                ts: 0.0,
                src: "s".into(),
                chan: "c".into(),
                msg: s.as_bytes().to_vec(),
            }]
        };
        r.feed(&encode_batch(1, 0, &line("a"), 0));
        r.feed(&encode_batch(1, 2, &line("c"), 0)); // 1 missing
        r.feed(&encode_batch(1, 0, &line("a"), 0)); // duplicate
        r.feed(&encode_batch(1, 1, &line("b"), 0)); // arrives late
        assert_eq!(
            *got.lock().unwrap(),
            vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]
        );
        r.feed(&encode_batch(1, 5, &line("f"), 0));
        r.poll(); // window 0: gap 3-4 reported, f delivered
        assert_eq!(got.lock().unwrap().last().unwrap(), b"f");
        assert_eq!(r.stats.take().get("lost_batches"), Some(&json!(2)));
    }

    #[test]
    fn batcher_sends_heartbeats_and_copies() {
        let mut cfg = logs_cfg();
        cfg.copies = 2;
        cfg.batch_interval = 0.05;
        let sent = Arc::new(Mutex::new(0usize));
        let s2 = Arc::clone(&sent);
        let buf = LogBuffer::new(10);
        let mut b = Batcher::new(&cfg, move |_p: &[u8]| *s2.lock().unwrap() += 1);
        buf.add("h", "c", b"one".to_vec());
        b.tick(&buf); // one batch
        assert_eq!(*sent.lock().unwrap(), 1);
        std::thread::sleep(Duration::from_millis(250));
        b.tick(&buf); // its repeat (idle, but a batch went out <10 s ago: no heartbeat)
        assert_eq!(*sent.lock().unwrap(), 2);
        for _ in 0..20 {
            buf.add("h", "c", b"x".to_vec());
        }
        assert_eq!(buf.dropped(), 10);
    }
}
