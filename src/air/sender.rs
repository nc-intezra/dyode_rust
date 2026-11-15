//! The input side of `transport: air`: spool, seal, and broadcast on a
//! carousel.
//!
//! Each object (one DYL1 envelope) is read to the end, split into chunks
//! of `air.chunk` bytes, and each chunk sealed under this boot's key.
//! Small objects stay in memory; larger ones are spooled to `air.spool`.
//! Then the object joins the carousel, which sends it pass after pass:
//!
//! - chunks go out in interleaving windows of at least `air.interleave`
//!   chunks, one symbol of each chunk in turn, so a burst of loss costs
//!   each chunk only a few symbols;
//! - the manifest's symbols are spread through every pass, at least 8 per
//!   pass, so a receiver that joins late still learns what it collects;
//! - passes of one object are at least `air.pass_gap` seconds apart, and
//!   the objects on the carousel take turns window by window, so a small
//!   object's passes land in different fades;
//! - the whole stream is paced to `air.max_mbps`.

use super::keys::{BootKey, ChunkId, Keys};
use super::manifest::Manifest;
use super::packet::{self, FLAG_MANIFEST, Header};
use super::schedule::{self, Spacer};
use crate::config::{AirConfig, Carousel, Config};
use crate::ratelimit::TokenBucket;
use crate::transport::Source;
use raptorq::{EncodingPacket, SourceBlockEncoder};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::net::UdpSocket;
use std::ops::Range;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Objects up to this many sealed bytes are never written to the spool.
const IN_MEMORY: usize = 4 << 20;
/// Messages of one module waiting for the carousel before the module is
/// made to wait (files wait for spool space instead).
const MAX_QUEUED: usize = 256;

/// Spool space: what is written, and how much of it is on the carousel.
struct Spool {
    dir: PathBuf,
    max: u64,
    use_: Mutex<(u64, u64)>,
    freed: Condvar,
}

impl Spool {
    /// Wait for room for `n` bytes.  Only objects already on the carousel
    /// make room, so a spool filled only by files still being read never
    /// waits (it may go over its size instead of deadlocking).
    fn reserve(&self, n: u64) {
        let mut u = self.use_.lock().unwrap();
        while u.0 + n > self.max && u.1 > 0 {
            u = self.freed.wait(u).unwrap();
        }
        u.0 += n;
    }

    fn on_air(&self, n: u64) {
        self.use_.lock().unwrap().1 += n;
    }

    fn release(&self, n: u64, on_air: bool) {
        let mut u = self.use_.lock().unwrap();
        u.0 = u.0.saturating_sub(n);
        if on_air {
            u.1 = u.1.saturating_sub(n);
        }
        self.freed.notify_all();
    }
}

enum Store {
    Mem(Vec<Vec<u8>>),
    File(File, PathBuf),
}

struct Progress {
    /// The pass being sent, 1-based.
    pass: u32,
    /// Next window of that pass.
    window: usize,
    spacer: Option<Spacer>,
    passes_done: u32,
    next_ready: Instant,
    /// Manifest packets sent so far (source first, then repair).
    manifest_sent: u64,
}

struct Obj {
    endpoint: u16,
    object: u64,
    policy: Carousel,
    created: Instant,
    chunk_count: u32,
    sealed_chunk: u64,
    last_sealed: u64,
    store: Store,
    spool_bytes: u64,
    spool: Arc<Spool>,
    windows: Vec<Range<u32>>,
    manifest_len: u32,
    manifest_enc: SourceBlockEncoder,
    manifest_source: Vec<EncodingPacket>,
    progress: Mutex<Progress>,
}

impl Obj {
    fn sealed_len(&self, index: u32) -> u64 {
        if index == self.chunk_count {
            self.last_sealed
        } else {
            self.sealed_chunk
        }
    }

    fn read_chunk(&self, index: u32) -> std::io::Result<Vec<u8>> {
        match &self.store {
            Store::Mem(v) => Ok(v[(index - 1) as usize].clone()),
            Store::File(f, _) => {
                let mut buf = vec![0u8; self.sealed_len(index) as usize];
                f.read_exact_at(&mut buf, u64::from(index - 1) * self.sealed_chunk)?;
                Ok(buf)
            }
        }
    }

    /// The `n`-th manifest packet of this object's life.
    fn manifest_packet(&self, n: u64) -> EncodingPacket {
        let k = self.manifest_source.len() as u64;
        if n < k {
            return self.manifest_source[n as usize].clone();
        }
        let idx = ((n - k) % (1 << 23)) as u32;
        self.manifest_enc
            .repair_packets(idx, 1)
            .pop()
            .expect("one repair packet")
    }
}

impl Drop for Obj {
    fn drop(&mut self) {
        if let Store::File(_, p) = &self.store {
            let _ = std::fs::remove_file(p);
        }
        if self.spool_bytes > 0 {
            self.spool.release(self.spool_bytes, true);
        }
    }
}

#[derive(Default)]
struct Wheel {
    objects: VecDeque<Arc<Obj>>,
    /// The newest object of each `latest` endpoint.
    latest: HashMap<u16, u64>,
    /// Objects waiting or on air per endpoint (for MAX_QUEUED).
    queued: HashMap<u16, usize>,
}

pub struct AirSender {
    cfg: AirConfig,
    keys: Keys,
    boot: u64,
    bkey: BootKey,
    next_object: AtomicU64,
    policies: HashMap<u16, Carousel>,
    tags: HashMap<u16, [u8; 8]>,
    min_passes: u32,
    socket: UdpSocket,
    pacer: TokenBucket,
    spool: Arc<Spool>,
    wheel: Mutex<Wheel>,
    wake: Condvar,
    room: Condvar,
}

impl AirSender {
    /// `endpoints`: every endpoint this box sends on, with its module tag.
    pub fn new(cfg: &Config, key: &[u8], endpoints: &[(u16, [u8; 8])]) -> Result<Self, String> {
        let a = cfg.air.clone();
        std::fs::create_dir_all(&a.spool)
            .map_err(|e| format!("air: cannot create the spool {}: {e}", a.spool.display()))?;
        // Spool files of an earlier run belong to a boot nobody accepts any
        // more; the carousel restarts empty.
        if let Ok(rd) = std::fs::read_dir(&a.spool) {
            for e in rd.flatten() {
                if e.path().extension().is_some_and(|x| x == "spool") {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        let boot_file = a.spool.join("last-boot");
        let last = std::fs::read_to_string(&boot_file)
            .ok()
            .and_then(|t| u64::from_str_radix(t.trim(), 16).ok())
            .unwrap_or(0);
        let boot = super::new_boot(last.saturating_add(1));
        crate::fsafe::write_atomic(&boot_file, format!("{boot:016x}\n").as_bytes())
            .map_err(|e| format!("air: cannot write {}: {e}", boot_file.display()))?;
        let socket =
            UdpSocket::bind(a.bind).map_err(|e| format!("air: cannot bind {}: {e}", a.bind))?;
        let keys = Keys::derive(key);
        let bkey = keys.boot_key(boot);
        let mut policies = HashMap::new();
        let mut tags = HashMap::new();
        for (ep, tag) in endpoints {
            policies.insert(*ep, cfg.carousel_for(*ep));
            tags.insert(*ep, *tag);
        }
        log::info!(
            "air: boot {boot:016x}, sending to {} from {}, {} byte symbols, {} MiB spool at {}",
            a.to,
            socket
                .local_addr()
                .map_or_else(|_| a.bind.to_string(), |s| s.to_string()),
            a.symbol,
            a.spool_max_mb,
            a.spool.display()
        );
        Ok(Self {
            pacer: TokenBucket::new(
                a.max_mbps * 1e6 / 8.0,
                (a.max_mbps * 1e6 / 8.0 * 0.02).max(8192.0),
            ),
            spool: Arc::new(Spool {
                dir: a.spool.clone(),
                max: a.spool_max_mb << 20,
                use_: Mutex::new((0, 0)),
                freed: Condvar::new(),
            }),
            min_passes: a.passes,
            cfg: a,
            keys,
            boot,
            bkey,
            next_object: AtomicU64::new(0),
            policies,
            tags,
            socket,
            wheel: Mutex::new(Wheel::default()),
            wake: Condvar::new(),
            room: Condvar::new(),
        })
    }

    pub fn boot(&self) -> u64 {
        self.boot
    }

    /// Hand an object to the carousel.  `inline` objects (messages) are
    /// read in the caller's thread, which waits while its module already
    /// has many objects queued; others (files) are read in a thread of
    /// their own, which waits for spool space.
    pub fn submit(
        self: &Arc<Self>,
        endpoint: u16,
        source: Source,
        inline: bool,
    ) -> Result<(), String> {
        if !self.tags.contains_key(&endpoint) {
            return Err(format!("air: no module on endpoint {endpoint}"));
        }
        if inline {
            self.wait_room(endpoint);
            return self.take(endpoint, source);
        }
        let me = Arc::clone(self);
        std::thread::Builder::new()
            .name("air-spool".into())
            .spawn(move || {
                me.wait_room(endpoint);
                if let Err(e) = me.take(endpoint, source) {
                    log::error!("air: {e}");
                }
            })
            .map(|_| ())
            .map_err(|e| format!("air: cannot start a spool thread: {e}"))
    }

    fn wait_room(&self, endpoint: u16) {
        if self.policies.get(&endpoint) == Some(&Carousel::Latest) {
            return;
        }
        let mut w = self.wheel.lock().unwrap();
        while w.queued.get(&endpoint).copied().unwrap_or(0) >= MAX_QUEUED {
            w = self.room.wait(w).unwrap();
        }
    }

    /// Read, seal and spool one object, then put it on the carousel.
    fn take(&self, endpoint: u16, mut source: Source) -> Result<(), String> {
        let tag = self.tags[&endpoint];
        let object = self.next_object.fetch_add(1, Ordering::Relaxed) + 1;
        let chunk = self.cfg.chunk as usize;
        let mut hasher = Sha256::new();
        let mut total = 0u64;
        let mut mem: Vec<Vec<u8>> = Vec::new();
        let mut mem_bytes = 0usize;
        let mut file: Option<(File, PathBuf)> = None;
        let mut spooled = 0u64;
        let release = |n: u64| self.spool.release(n, false);
        let mut index = 1u32;
        loop {
            let buf = match read_up_to(&mut source, chunk) {
                Ok(b) => b,
                Err(e) => {
                    release(spooled);
                    if let Some((_, p)) = &file {
                        let _ = std::fs::remove_file(p);
                    }
                    return Err(format!(
                        "object {object} on endpoint {endpoint} not sent: {e}"
                    ));
                }
            };
            if buf.is_empty() {
                break;
            }
            let last = buf.len() < chunk;
            hasher.update(&buf);
            total += buf.len() as u64;
            let id = ChunkId {
                endpoint,
                module_tag: tag,
                object,
                index,
            };
            let sealed = self.bkey.seal(&id, buf);
            if file.is_none() && mem_bytes + sealed.len() > IN_MEMORY {
                let path = self
                    .spool
                    .dir
                    .join(format!("{:016x}-{object}.spool", self.boot));
                let mut f = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .open(&path)
                    .map_err(|e| format!("air: cannot create {}: {e}", path.display()))?;
                for c in mem.drain(..) {
                    self.spool.reserve(c.len() as u64);
                    spooled += c.len() as u64;
                    f.write_all(&c)
                        .map_err(|e| format!("air: spool write: {e}"))?;
                }
                file = Some((f, path));
            }
            match &mut file {
                Some((f, path)) => {
                    self.spool.reserve(sealed.len() as u64);
                    spooled += sealed.len() as u64;
                    if let Err(e) = f.write_all(&sealed) {
                        release(spooled);
                        let _ = std::fs::remove_file(&*path);
                        return Err(format!("air: spool write to {}: {e}", path.display()));
                    }
                }
                None => {
                    mem_bytes += sealed.len();
                    mem.push(sealed);
                }
            }
            index += 1;
            if last {
                break;
            }
        }
        if total == 0 {
            return Err(format!("empty object on endpoint {endpoint}"));
        }
        let chunk_count = index - 1;
        let policy = self.policies[&endpoint];
        let created_unix = super::unix_now();
        let lifetime = match policy {
            Carousel::Until(s) => s,
            _ => 0,
        };
        let manifest = Manifest {
            endpoint,
            module_tag: tag,
            total_len: total,
            chunk_size: self.cfg.chunk,
            chunk_count,
            sha256: hasher.finalize().into(),
            created: created_unix,
            expiry: created_unix + self.cfg.max_age + lifetime,
        };
        let msealed = self.bkey.seal(
            &ChunkId {
                endpoint,
                module_tag: tag,
                object,
                index: 0,
            },
            manifest.encode(),
        );
        let manifest_enc = super::encoder(&msealed, self.cfg.symbol);
        let manifest_source = manifest_enc.source_packets();
        let sealed_chunk = (chunk + super::keys::SEAL_LEN) as u64;
        let last_plain = manifest.chunk_plain_len(chunk_count);
        let store = match file {
            Some((f, p)) => {
                if let Err(e) = f.sync_data() {
                    log::warn!("air: spool sync: {e}");
                }
                Store::File(f, p)
            }
            None => Store::Mem(mem),
        };
        if spooled > 0 {
            self.spool.on_air(spooled);
        }
        let obj = Arc::new(Obj {
            endpoint,
            object,
            policy,
            created: Instant::now(),
            chunk_count,
            sealed_chunk,
            last_sealed: last_plain + super::keys::SEAL_LEN as u64,
            store,
            spool_bytes: spooled,
            spool: Arc::clone(&self.spool),
            windows: schedule::windows(chunk_count, self.cfg.interleave),
            manifest_len: msealed.len() as u32,
            manifest_enc,
            manifest_source,
            progress: Mutex::new(Progress {
                pass: 1,
                window: 0,
                spacer: None,
                passes_done: 0,
                next_ready: Instant::now(),
                manifest_sent: 0,
            }),
        });
        log::debug!(
            "air: object {object} on endpoint {endpoint}: {total} bytes, {chunk_count} chunk(s), {policy}"
        );
        crate::stats::count("air_objects_queued");
        let mut w = self.wheel.lock().unwrap();
        if policy == Carousel::Latest {
            w.latest.insert(endpoint, object);
            w.objects
                .retain(|o| !(o.endpoint == endpoint && o.policy == Carousel::Latest));
        }
        *w.queued.entry(endpoint).or_default() += 1;
        w.objects.push_back(obj);
        drop(w);
        self.wake.notify_all();
        Ok(())
    }

    /// The carousel.  Runs for ever; an error ends it (the supervisor
    /// starts it again, with the objects still on the wheel).
    pub fn run(&self) -> Result<(), String> {
        let mut buf = Vec::with_capacity(packet::OVERHEAD + usize::from(self.cfg.symbol));
        loop {
            let obj = self.next_ready();
            let result = self.send_window(&obj, &mut buf);
            // A spool that cannot be read is this object's problem: drop
            // it.  A socket error is the link's: keep the object and let
            // the supervisor restart the carousel after its delay.
            let (result, broken) = match result {
                Ok(()) => (Ok(()), false),
                Err(WindowError::Spool(e)) => {
                    log::error!("{e}; object dropped");
                    crate::stats::count("air_objects_dropped");
                    (Ok(()), true)
                }
                Err(WindowError::Send(e)) => (Err(e), false),
            };
            let finished = {
                let p = obj.progress.lock().unwrap();
                p.window == 0
                    && match obj.policy {
                        Carousel::Passes(n) => p.passes_done >= n,
                        Carousel::Until(s) => {
                            p.passes_done >= self.min_passes
                                && obj.created.elapsed() >= Duration::from_secs(s)
                        }
                        Carousel::Latest => false,
                    }
            };
            let mut w = self.wheel.lock().unwrap();
            let superseded =
                obj.policy == Carousel::Latest && w.latest.get(&obj.endpoint) != Some(&obj.object);
            if finished || superseded || broken {
                if let Some(q) = w.queued.get_mut(&obj.endpoint) {
                    *q = q.saturating_sub(1);
                }
                if finished {
                    crate::stats::count("air_objects_sent");
                }
                drop(w);
                self.room.notify_all();
            } else {
                w.objects.push_back(obj);
            }
            result?;
        }
    }

    fn next_ready(&self) -> Arc<Obj> {
        let mut w = self.wheel.lock().unwrap();
        loop {
            let now = Instant::now();
            let mut earliest: Option<Instant> = None;
            let mut pick = None;
            for (i, o) in w.objects.iter().enumerate() {
                let at = o.progress.lock().unwrap().next_ready;
                if at <= now {
                    pick = Some(i);
                    break;
                }
                earliest = Some(earliest.map_or(at, |e| e.min(at)));
            }
            if let Some(i) = pick {
                return w.objects.remove(i).expect("index is valid");
            }
            let wait = earliest.map_or(Duration::from_secs(1), |e| e - now);
            w = self.wake.wait_timeout(w, wait).unwrap().0;
        }
    }

    fn send(
        &self,
        obj: &Obj,
        chunk: u32,
        chunk_len: u32,
        p: &EncodingPacket,
        buf: &mut Vec<u8>,
    ) -> Result<(), WindowError> {
        let h = Header {
            flags: if chunk == 0 { FLAG_MANIFEST } else { 0 },
            key_id: self.keys.key_id,
            endpoint: obj.endpoint,
            boot: self.boot,
            object: obj.object,
            chunk,
            chunk_len,
            payload_id: p.payload_id().serialize(),
        };
        packet::encode(&self.keys, &h, p.data(), buf);
        self.pacer.take(buf.len());
        match self.socket.send_to(buf, self.cfg.to) {
            Ok(_) => Ok(()),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::WouldBlock
                ) =>
            {
                crate::stats::count("air_send_errors");
                Ok(())
            }
            Err(e) => Err(WindowError::Send(format!(
                "air: sending to {}: {e}",
                self.cfg.to
            ))),
        }
    }

    fn send_manifest(
        &self,
        obj: &Obj,
        p: &mut Progress,
        n: u32,
        buf: &mut Vec<u8>,
    ) -> Result<(), WindowError> {
        crate::stats::count_n("air_packets_sent", u64::from(n));
        for _ in 0..n {
            let pkt = obj.manifest_packet(p.manifest_sent);
            p.manifest_sent += 1;
            self.send(obj, 0, obj.manifest_len, &pkt, buf)?;
        }
        Ok(())
    }

    /// One interleaving window of the object's current pass.
    fn send_window(&self, obj: &Obj, buf: &mut Vec<u8>) -> Result<(), WindowError> {
        let t = self.cfg.symbol;
        let r = self.cfg.repair;
        let mut p = obj.progress.lock().unwrap();
        let pass = p.pass;
        if p.spacer.is_none() {
            let data: u64 = (1..=obj.chunk_count)
                .map(|i| {
                    let k = super::symbols(obj.sealed_len(i) as usize, t);
                    u64::from(schedule::pass_plan(k, r, pass).packets(k))
                })
                .sum();
            p.spacer = Some(Spacer::new(data, schedule::manifest_symbols(data)));
        }
        let range = obj.windows[p.window].clone();
        let mut lists = Vec::with_capacity(range.len());
        for i in range {
            let sealed = obj.read_chunk(i).map_err(|e| {
                WindowError::Spool(format!(
                    "air: reading object {} chunk {i} from the spool: {e}",
                    obj.object
                ))
            })?;
            let k = super::symbols(sealed.len(), t);
            let enc = super::encoder(&sealed, t);
            lists.push((
                i,
                sealed.len() as u32,
                super::pass_packets(&enc, k, schedule::pass_plan(k, r, pass)),
            ));
        }
        let longest = lists.iter().map(|l| l.2.len()).max().unwrap_or(0);
        let mut sent = 0u64;
        for j in 0..longest {
            for (i, len, pkts) in &lists {
                if let Some(pkt) = pkts.get(j) {
                    let before = p.spacer.as_mut().expect("set above").before_data();
                    self.send_manifest(obj, &mut p, before, buf)?;
                    self.send(obj, *i, *len, pkt, buf)?;
                    sent += 1;
                }
            }
        }
        p.window += 1;
        if p.window == obj.windows.len() {
            let rest = p.spacer.as_mut().expect("set above").remaining();
            self.send_manifest(obj, &mut p, rest, buf)?;
            p.window = 0;
            p.spacer = None;
            p.pass += 1;
            p.passes_done += 1;
            p.next_ready = Instant::now() + Duration::from_secs_f64(self.cfg.pass_gap);
        }
        crate::stats::count_n("air_packets_sent", sent);
        Ok(())
    }
}

enum WindowError {
    Spool(String),
    Send(String),
}

/// Read until `n` bytes or the end.
fn read_up_to(r: &mut Source, n: usize) -> std::io::Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    let mut got = 0;
    while got < n {
        match r.read(&mut buf[got..]) {
            Ok(0) => break,
            Ok(k) => got += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    buf.truncate(got);
    Ok(buf)
}
