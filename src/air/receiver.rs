//! The output side of `transport: air`: collect, decode, open, deliver.
//!
//! Every datagram is checked (format, key id, packet tag) before it costs
//! anything.  Then, in order:
//!
//! - **Boot.**  Only the newest sender boot is accepted; an older one is
//!   refused, a newer one replaces it (and what was being collected from
//!   the old one is dropped).  A boot that claims to start in the future
//!   beyond `air.clock_skew` is refused, so a key holder cannot lock the
//!   real sender out with a boot from next year.
//! - **Done table.**  Object ids already delivered (or refused) in this
//!   boot are ignored.  The boot and the table are saved in
//!   `air.state/air-state.json`, so a restarted receiver delivers nothing
//!   twice and accepts no recording of an older boot (design correction 2).
//! - **Collecting.**  Each chunk has its own RaptorQ decoder; a decoded
//!   chunk is opened (ChaCha20-Poly1305) at once, manifest or not
//!   (correction 1).  Chunk 1 of a small object stays in memory; anything
//!   bigger goes to `air.state/parts/<object>.part`, with a bitmap of the
//!   chunks it holds beside it, so a restart resumes where it was.
//! - **Expiry.**  The manifest's expiry is checked on arrival and while
//!   collecting; an object without a manifest is forgotten after
//!   `air.orphan_timeout` of silence.
//! - **Delivery.**  A complete object whose SHA-256 matches its manifest
//!   is written to the module's envelope parser -- the same parser and
//!   module handlers lidi feeds -- in a thread per module.

use super::keys::{BootKey, ChunkId, Keys, SEAL_LEN};
use super::manifest::{self, Manifest};
use super::packet;
use crate::config::{AirConfig, Config};
use crate::envelope::{Parser, Registration};
use crate::transport::Lifecycle;
use raptorq::{EncodingPacket, PayloadId, SourceBlockDecoder};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::net::UdpSocket;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Delivered (or refused) object ids, as sorted inclusive ranges.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ranges(Vec<(u64, u64)>);

impl Ranges {
    pub fn contains(&self, v: u64) -> bool {
        let i = self.0.partition_point(|r| r.1 < v);
        self.0.get(i).is_some_and(|r| r.0 <= v)
    }

    pub fn insert(&mut self, v: u64) {
        if self.contains(v) {
            return;
        }
        let i = self.0.partition_point(|r| r.1 < v);
        let joins_prev = i > 0 && self.0[i - 1].1 + 1 == v;
        let joins_next = i < self.0.len() && v + 1 == self.0[i].0;
        match (joins_prev, joins_next) {
            (true, true) => {
                self.0[i - 1].1 = self.0[i].1;
                self.0.remove(i);
            }
            (true, false) => self.0[i - 1].1 = v,
            (false, true) => self.0[i].0 = v,
            (false, false) => self.0.insert(i, (v, v)),
        }
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

struct Decoding {
    dec: SourceBlockDecoder,
    len: u32,
    symbols: u64,
}

struct Collect {
    endpoint: u16,
    module_tag: [u8; 8],
    manifest: Option<Manifest>,
    decoding: HashMap<u32, Decoding>,
    /// Chunks held, by index (0 = manifest).
    have: Vec<bool>,
    /// Data chunk 1, opened, while the object may be a one-chunk object.
    first: Option<Vec<u8>>,
    part: Option<File>,
    part_bytes: u64,
    last_packet: Instant,
}

impl Collect {
    fn new(endpoint: u16, module_tag: [u8; 8]) -> Self {
        Self {
            endpoint,
            module_tag,
            manifest: None,
            decoding: HashMap::new(),
            have: Vec::new(),
            first: None,
            part: None,
            part_bytes: 0,
            last_packet: Instant::now(),
        }
    }

    fn has(&self, i: u32) -> bool {
        self.have.get(i as usize).copied().unwrap_or(false)
    }

    fn set(&mut self, i: u32) {
        let i = i as usize;
        if self.have.len() <= i {
            self.have.resize(i + 1, false);
        }
        self.have[i] = true;
    }

    fn complete(&self) -> bool {
        match &self.manifest {
            Some(m) => (1..=m.chunk_count).all(|i| self.has(i)),
            None => false,
        }
    }

    fn buffered(&self, symbol: u16) -> u64 {
        self.decoding.values().map(|d| d.symbols).sum::<u64>() * u64::from(symbol)
    }
}

enum Payload {
    Mem(Vec<u8>),
    /// A .part file, checked against the manifest's SHA-256 in the
    /// delivery thread (hashing gigabytes must not stop packet reading).
    File {
        path: PathBuf,
        object: u64,
        len: u64,
        sha256: [u8; 32],
    },
}

struct Rx {
    boot: u64,
    bkey: Option<BootKey>,
    done: Ranges,
    objects: HashMap<u64, Collect>,
    buffered: u64,
    partial: u64,
    last_sweep: Instant,
    received: u64,
    old_boot: u64,
    duplicate: u64,
}

pub struct AirReceiver {
    cfg: AirConfig,
    keys: Keys,
    regs: HashMap<u16, Arc<Registration>>,
    socket: UdpSocket,
    state_file: PathBuf,
    parts: PathBuf,
    rx: Mutex<Rx>,
    deliver: HashMap<u16, crossbeam_channel::Sender<Payload>>,
    /// Endpoints whose sender keeps only the newest object on air.
    latest: std::collections::HashSet<u16>,
    stopped: std::sync::atomic::AtomicBool,
}

impl AirReceiver {
    /// Bind `air.listen`, load the saved state and start one delivery
    /// thread per module.  Packets are read by [`AirReceiver::run`].
    pub fn new(cfg: &Config, key: &[u8], lifecycle: &Lifecycle) -> Result<Arc<Self>, String> {
        let a = cfg.air.clone();
        let parts = a.state.join("parts");
        std::fs::create_dir_all(&parts)
            .map_err(|e| format!("air: cannot create {}: {e}", parts.display()))?;
        let socket = UdpSocket::bind(a.listen)
            .map_err(|e| format!("air: cannot listen on {}: {e}", a.listen))?;
        socket
            .set_read_timeout(Some(Duration::from_millis(500)))
            .map_err(|e| format!("air: {e}"))?;
        grow_receive_buffer(&socket);
        let keys = Keys::derive(key);
        let state_file = a.state.join("air-state.json");
        let (boot, done) = load_state(&state_file);
        let mut regs = HashMap::new();
        let mut deliver = HashMap::new();
        for reg in lifecycle.registrations() {
            let (tx, rx) = crossbeam_channel::unbounded::<Payload>();
            let r = Arc::clone(&reg);
            std::thread::Builder::new()
                .name(format!("air-deliver{}", reg.endpoint))
                .spawn(move || deliver_loop(&r, &rx))
                .map_err(|e| format!("air: cannot start a delivery thread: {e}"))?;
            deliver.insert(reg.endpoint, tx);
            regs.insert(reg.endpoint, reg);
        }
        let regs_latest = regs
            .keys()
            .copied()
            .filter(|e| cfg.carousel_for(*e) == crate::config::Carousel::Latest)
            .collect();
        let me = Self {
            rx: Mutex::new(Rx {
                boot,
                bkey: (boot > 0).then(|| keys.boot_key(boot)),
                done,
                objects: HashMap::new(),
                buffered: 0,
                partial: 0,
                last_sweep: Instant::now(),
                received: 0,
                old_boot: 0,
                duplicate: 0,
            }),
            cfg: a,
            keys,
            regs,
            socket,
            state_file,
            parts,
            latest: regs_latest,
            deliver,
            stopped: std::sync::atomic::AtomicBool::new(false),
        };
        me.resume_parts();
        let rx = me.rx.lock().unwrap();
        log::info!(
            "air: listening on {}; sender boot {}; {} object(s) resumed from {}",
            me.cfg.listen,
            if rx.boot == 0 {
                "not seen yet".to_string()
            } else {
                format!("{:016x}", rx.boot)
            },
            rx.objects.len(),
            me.parts.display()
        );
        drop(rx);
        Ok(Arc::new(me))
    }

    pub fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.socket.local_addr()
    }

    /// Make [`AirReceiver::run`] return (within half a second).
    pub fn stop(&self) {
        self.stopped
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// The objects being collected: (object id, chunks held, manifest
    /// known).  For tests and diagnostics.
    pub fn collecting(&self) -> Vec<(u64, usize, bool)> {
        let rx = self.rx.lock().unwrap();
        let mut v: Vec<_> = rx
            .objects
            .iter()
            .map(|(k, c)| {
                (
                    *k,
                    c.have.iter().filter(|h| **h).count(),
                    c.manifest.is_some(),
                )
            })
            .collect();
        v.sort_unstable();
        v
    }

    /// Read packets until stopped.
    pub fn run(&self) -> Result<(), String> {
        let mut buf = vec![0u8; 65536];
        loop {
            if self.stopped.load(std::sync::atomic::Ordering::Relaxed) {
                return Ok(());
            }
            match self.socket.recv(&mut buf) {
                Ok(n) => {
                    let mut rx = self.rx.lock().unwrap();
                    self.handle(&mut rx, &buf[..n]);
                    if rx.last_sweep.elapsed() >= Duration::from_secs(5) {
                        self.sweep(&mut rx);
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    let mut rx = self.rx.lock().unwrap();
                    if rx.last_sweep.elapsed() >= Duration::from_secs(5) {
                        self.sweep(&mut rx);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(format!("air: receiving on {}: {e}", self.cfg.listen)),
            }
        }
    }

    fn handle(&self, rx: &mut Rx, buf: &[u8]) {
        let t = self.cfg.symbol;
        let (h, sym) = match packet::decode(&self.keys, buf, usize::from(t)) {
            Ok(x) => x,
            Err(b) => {
                crate::stats::count(b.counter());
                return;
            }
        };
        rx.received += 1;
        if h.boot < rx.boot {
            rx.old_boot += 1;
            return;
        }
        if h.boot > rx.boot {
            let now = super::unix_now();
            if super::boot_secs(h.boot) > now + self.cfg.clock_skew {
                crate::stats::count("air_boot_in_future");
                return;
            }
            self.switch_boot(rx, h.boot);
        }
        if rx.done.contains(h.object) {
            rx.duplicate += 1;
            return;
        }
        let Some(reg) = self.regs.get(&h.endpoint) else {
            crate::stats::count("air_packets_unknown_endpoint");
            return;
        };
        if !rx.objects.contains_key(&h.object) {
            if self.latest.contains(&h.endpoint) {
                // The sender keeps only the newest object of a `latest`
                // module on air: older ones of it will never complete.
                let older: Vec<u64> = rx
                    .objects
                    .iter()
                    .filter(|(k, c)| c.endpoint == h.endpoint && **k < h.object)
                    .map(|(k, _)| *k)
                    .collect();
                for o in older {
                    self.drop_object(rx, o, "air_objects_superseded");
                }
            }
            if rx.objects.len() >= self.cfg.max_objects {
                self.evict(rx);
            }
            rx.objects
                .insert(h.object, Collect::new(h.endpoint, reg.module_tag));
        }
        let max_buffer = self.cfg.max_buffer_mb << 20;
        let chunk_size = self.cfg.chunk;
        let buffered = rx.buffered;
        let c = rx.objects.get_mut(&h.object).expect("inserted above");
        c.last_packet = Instant::now();
        if c.endpoint != h.endpoint || c.has(h.chunk) {
            if c.endpoint != h.endpoint {
                crate::stats::count("air_packets_inconsistent");
            }
            return;
        }
        let max_len = if h.chunk == 0 {
            (manifest::LEN + SEAL_LEN) as u32
        } else {
            chunk_size + SEAL_LEN as u32
        };
        let expected = match &c.manifest {
            Some(m) if h.chunk > m.chunk_count => {
                crate::stats::count("air_packets_inconsistent");
                return;
            }
            Some(m) if h.chunk > 0 => Some(m.chunk_plain_len(h.chunk) as u32 + SEAL_LEN as u32),
            _ => None,
        };
        let pid = PayloadId::deserialize(&h.payload_id);
        if h.chunk_len == 0
            || h.chunk_len > max_len
            || expected.is_some_and(|e| e != h.chunk_len)
            || pid.source_block_number() != 0
        {
            crate::stats::count("air_packets_inconsistent");
            return;
        }
        let d = c.decoding.entry(h.chunk).or_insert_with(|| {
            let padded = super::padded(h.chunk_len as usize, t);
            Decoding {
                dec: SourceBlockDecoder::new(0, &super::oti(padded, t), padded as u64),
                len: h.chunk_len,
                symbols: 0,
            }
        });
        if d.len != h.chunk_len {
            crate::stats::count("air_packets_inconsistent");
            return;
        }
        if buffered + u64::from(t) > max_buffer {
            crate::stats::count("air_buffer_full");
            return;
        }
        d.symbols += 1;
        rx.buffered += u64::from(t);
        let Some(mut sealed) = d
            .dec
            .decode(std::iter::once(EncodingPacket::new(pid, sym.to_vec())))
        else {
            return;
        };
        let d = c.decoding.remove(&h.chunk).expect("present");
        rx.buffered = rx.buffered.saturating_sub(d.symbols * u64::from(t));
        sealed.truncate(d.len as usize);
        let id = ChunkId {
            endpoint: c.endpoint,
            module_tag: c.module_tag,
            object: h.object,
            index: h.chunk,
        };
        let Some(plain) = rx.bkey.as_ref().and_then(|k| k.open(&id, sealed)) else {
            // A forged or poisoned chunk (only a key holder could have
            // signed its packets).  Start this chunk again.
            crate::stats::count("air_chunks_bad_seal");
            return;
        };
        if h.chunk == 0 {
            self.got_manifest(rx, h.object, &plain);
        } else {
            self.got_chunk(rx, h.object, h.chunk, plain);
        }
        let complete = rx.objects.get(&h.object).is_some_and(Collect::complete);
        if complete {
            self.finish(rx, h.object);
        }
    }

    /// Refuse an object for good: drop it and remember its id.
    fn refuse(&self, rx: &mut Rx, object: u64, counter: &str, why: &str) {
        log::warn!("air: object {object} refused: {why}");
        crate::stats::count(counter);
        if let Some(c) = rx.objects.remove(&object) {
            self.discard(rx, object, &c);
        }
        rx.done.insert(object);
        self.save_state(rx);
    }

    fn got_manifest(&self, rx: &mut Rx, object: u64, plain: &[u8]) {
        let now = super::unix_now();
        let max_partial = self.cfg.max_partial_mb << 20;
        let Some(c) = rx.objects.get(&object) else {
            return;
        };
        let Some(m) = Manifest::decode(plain) else {
            return self.refuse(
                rx,
                object,
                "air_objects_bad_manifest",
                "unreadable manifest",
            );
        };
        if m.endpoint != c.endpoint || m.module_tag != c.module_tag {
            return self.refuse(
                rx,
                object,
                "air_objects_bad_manifest",
                "manifest for another module (do both boxes have the same config.yaml?)",
            );
        }
        if m.chunk_size != self.cfg.chunk {
            return self.refuse(
                rx,
                object,
                "air_objects_bad_manifest",
                &format!(
                    "chunk size {} but air.chunk is {} here (the two boxes' config.yaml differ)",
                    m.chunk_size, self.cfg.chunk
                ),
            );
        }
        if m.expiry <= now {
            return self.refuse(
                rx,
                object,
                "air_objects_expired",
                "expired before it arrived",
            );
        }
        if m.created > now + self.cfg.clock_skew {
            return self.refuse(
                rx,
                object,
                "air_objects_bad_manifest",
                "created in the future (check both boxes' clocks)",
            );
        }
        if m.chunk_count > 1 && m.total_len > max_partial {
            return self.refuse(
                rx,
                object,
                "air_objects_too_big",
                &format!("{} bytes is more than air.max_partial_mb", m.total_len),
            );
        }
        let c = rx.objects.get_mut(&object).expect("checked above");
        let count = m.chunk_count;
        c.decoding.retain(|i, _| *i <= count);
        c.set(0);
        let spill = count > 1 && c.first.is_some();
        c.manifest = Some(m);
        if spill {
            let first = c.first.take().expect("checked");
            self.store_chunk(rx, object, 1, &first);
        }
        if rx.objects.get(&object).is_some_and(|c| c.part.is_some()) {
            self.save_sidecar(rx, object);
        }
    }

    fn got_chunk(&self, rx: &mut Rx, object: u64, index: u32, plain: Vec<u8>) {
        let chunk = self.cfg.chunk as usize;
        let Some(c) = rx.objects.get_mut(&object) else {
            return;
        };
        let ok_len = match &c.manifest {
            Some(m) => plain.len() as u64 == m.chunk_plain_len(index),
            None => !plain.is_empty() && plain.len() <= chunk,
        };
        if !ok_len {
            return self.refuse(
                rx,
                object,
                "air_objects_bad_manifest",
                "chunk length disagrees with the manifest",
            );
        }
        let single = c.manifest.as_ref().map(|m| m.chunk_count == 1);
        if index == 1 && single != Some(false) && c.part.is_none() {
            c.first = Some(plain);
            c.set(1);
            return;
        }
        if let Some(first) = c.first.take() {
            self.store_chunk(rx, object, 1, &first);
        }
        self.store_chunk(rx, object, index, &plain);
        if rx.objects.contains_key(&object) {
            self.save_sidecar(rx, object);
        }
    }

    /// Write an opened chunk into the object's .part file.
    fn store_chunk(&self, rx: &mut Rx, object: u64, index: u32, plain: &[u8]) {
        let max_partial = self.cfg.max_partial_mb << 20;
        if rx.partial + plain.len() as u64 > max_partial {
            // Make room from the object idle the longest, or give up on
            // this one.
            crate::stats::count("air_partial_full");
            if let Some(victim) = rx
                .objects
                .iter()
                .filter(|(k, c)| **k != object && c.part.is_some())
                .min_by_key(|(_, c)| c.last_packet)
                .map(|(k, _)| *k)
            {
                log::warn!("air: partial objects use air.max_partial_mb; object {victim} dropped");
                self.drop_object(rx, victim, "air_objects_evicted");
            }
            if rx.partial + plain.len() as u64 > max_partial {
                log::warn!("air: object {object} does not fit in air.max_partial_mb; dropped");
                self.drop_object(rx, object, "air_objects_evicted");
                return;
            }
        }
        let path = self.part_path(object);
        let chunk = u64::from(self.cfg.chunk);
        let Some(c) = rx.objects.get_mut(&object) else {
            return;
        };
        if c.part.is_none() {
            match OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)
            {
                Ok(f) => c.part = Some(f),
                Err(e) => {
                    log::error!("air: cannot create {}: {e}", path.display());
                    return self.drop_object(rx, object, "air_objects_dropped");
                }
            }
        }
        let f = c.part.as_ref().expect("opened above");
        if let Err(e) = f.write_all_at(plain, u64::from(index - 1) * chunk) {
            log::error!("air: writing {}: {e}", path.display());
            return self.drop_object(rx, object, "air_objects_dropped");
        }
        c.set(index);
        c.part_bytes += plain.len() as u64;
        rx.partial += plain.len() as u64;
    }

    /// All chunks are in: check the whole object and hand it over.
    fn finish(&self, rx: &mut Rx, object: u64) {
        let Some(c) = rx.objects.remove(&object) else {
            return;
        };
        let m = c.manifest.clone().expect("complete means a manifest");
        rx.done.insert(object);
        self.save_state(rx);
        let _ = std::fs::remove_file(self.sidecar_path(object));
        rx.partial = rx.partial.saturating_sub(c.part_bytes);
        rx.buffered = rx.buffered.saturating_sub(c.buffered(self.cfg.symbol));
        let payload = match (&c.first, &c.part) {
            (Some(b), None) => {
                let digest: [u8; 32] = Sha256::digest(b).into();
                if digest != m.sha256 {
                    log::warn!("air: object {object} refused: SHA-256 differs from its manifest");
                    crate::stats::count("air_objects_bad_hash");
                    return;
                }
                Payload::Mem(b.clone())
            }
            (_, Some(_)) => Payload::File {
                path: self.part_path(object),
                object,
                len: m.total_len,
                sha256: m.sha256,
            },
            (None, None) => return,
        };
        log::debug!(
            "air: object {object} complete ({} bytes) for endpoint {}",
            m.total_len,
            c.endpoint
        );
        if let Some(tx) = self.deliver.get(&c.endpoint) {
            let _ = tx.send(payload);
        }
    }

    fn switch_boot(&self, rx: &mut Rx, boot: u64) {
        if rx.boot != 0 {
            log::info!(
                "air: the sender restarted (boot {boot:016x}, was {:016x}); {} partial object(s) dropped",
                rx.boot,
                rx.objects.len()
            );
        } else {
            log::info!("air: sender boot {boot:016x}");
        }
        crate::stats::count("air_sender_boots");
        let old: Vec<u64> = rx.objects.keys().copied().collect();
        for o in old {
            if let Some(c) = rx.objects.remove(&o) {
                self.discard(rx, o, &c);
            }
        }
        rx.boot = boot;
        rx.bkey = Some(self.keys.boot_key(boot));
        rx.done = Ranges::default();
        rx.buffered = 0;
        rx.partial = 0;
        self.save_state(rx);
    }

    fn drop_object(&self, rx: &mut Rx, object: u64, counter: &str) {
        if let Some(c) = rx.objects.remove(&object) {
            crate::stats::count(counter);
            self.discard(rx, object, &c);
        }
    }

    /// Forget an object's resources (it has already left the table).
    fn discard(&self, rx: &mut Rx, object: u64, c: &Collect) {
        rx.buffered = rx.buffered.saturating_sub(c.buffered(self.cfg.symbol));
        rx.partial = rx.partial.saturating_sub(c.part_bytes);
        if c.part.is_some() {
            let _ = std::fs::remove_file(self.part_path(object));
        }
        let _ = std::fs::remove_file(self.sidecar_path(object));
    }

    /// Too many objects at once: drop the one idle longest, preferring one
    /// without a manifest.
    fn evict(&self, rx: &mut Rx) {
        let victim = rx
            .objects
            .iter()
            .min_by_key(|(_, c)| (c.manifest.is_some(), c.last_packet))
            .map(|(k, _)| *k);
        if let Some(v) = victim {
            log::warn!("air: more than air.max_objects objects at once; object {v} dropped");
            self.drop_object(rx, v, "air_objects_evicted");
        }
    }

    /// Expired objects, orphans, idle decoders; counters to the registry.
    fn sweep(&self, rx: &mut Rx) {
        rx.last_sweep = Instant::now();
        crate::stats::count_n("air_packets_received", std::mem::take(&mut rx.received));
        crate::stats::count_n("air_packets_old_boot", std::mem::take(&mut rx.old_boot));
        crate::stats::count_n("air_packets_duplicate", std::mem::take(&mut rx.duplicate));
        let now = super::unix_now();
        let orphan = Duration::from_secs(self.cfg.orphan_timeout);
        let mut expired = Vec::new();
        let mut orphans = Vec::new();
        for (k, c) in &rx.objects {
            match &c.manifest {
                Some(m) if m.expiry <= now => expired.push(*k),
                None if c.last_packet.elapsed() >= orphan => orphans.push(*k),
                _ => {}
            }
        }
        for k in expired {
            log::warn!("air: object {k} expired before it was complete");
            self.drop_object(rx, k, "air_objects_expired");
        }
        for k in orphans {
            log::info!(
                "air: object {k} dropped: no manifest and nothing heard for air.orphan_timeout"
            );
            self.drop_object(rx, k, "air_objects_orphaned");
        }
        // Idle objects keep their chunks but not their decoders' symbols.
        let t = self.cfg.symbol;
        let mut freed = 0;
        for c in rx.objects.values_mut() {
            if c.last_packet.elapsed() >= orphan && !c.decoding.is_empty() {
                freed += c.buffered(t);
                c.decoding.clear();
            }
        }
        rx.buffered = rx.buffered.saturating_sub(freed);
    }

    fn part_path(&self, object: u64) -> PathBuf {
        self.parts.join(format!("{object}.part"))
    }

    fn sidecar_path(&self, object: u64) -> PathBuf {
        self.parts.join(format!("{object}.json"))
    }

    fn save_state(&self, rx: &Rx) {
        let v = json!({
            "boot": format!("{:016x}", rx.boot),
            "done": rx.done.0.iter().map(|r| [r.0, r.1]).collect::<Vec<_>>(),
        });
        if let Err(e) = crate::fsafe::write_atomic(&self.state_file, v.to_string().as_bytes()) {
            log::error!(
                "air: cannot save {}: {e} (a restart may deliver objects twice)",
                self.state_file.display()
            );
        }
    }

    fn save_sidecar(&self, rx: &Rx, object: u64) {
        let Some(c) = rx.objects.get(&object) else {
            return;
        };
        let mut bits = vec![0u8; c.have.len().div_ceil(8)];
        for (i, h) in c.have.iter().enumerate() {
            if *h {
                bits[i / 8] |= 1 << (i % 8);
            }
        }
        let v = json!({
            "boot": format!("{:016x}", rx.boot),
            "endpoint": c.endpoint,
            "manifest": c.manifest.as_ref().map(|m| crate::hex::encode(m.encode())),
            "have": crate::hex::encode(&bits),
            "bytes": c.part_bytes,
        });
        let _ = crate::fsafe::write_atomic(&self.sidecar_path(object), v.to_string().as_bytes());
    }

    /// Pick up the partial objects of the current boot after a restart.
    fn resume_parts(&self) {
        let mut rx = self.rx.lock().unwrap();
        let Ok(rd) = std::fs::read_dir(&self.parts) else {
            return;
        };
        let boot = format!("{:016x}", rx.boot);
        let mut keep = Vec::new();
        for e in rd.flatten() {
            let p = e.path();
            let Some(stem) = p
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.parse::<u64>().ok())
            else {
                let _ = std::fs::remove_file(&p);
                continue;
            };
            if p.extension().is_some_and(|x| x == "json") {
                keep.push((stem, p));
            }
        }
        let mut wanted = std::collections::HashSet::new();
        for (object, side) in keep {
            let v: Option<Value> = std::fs::read(&side)
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok());
            let parsed = v.and_then(|v| {
                (v["boot"].as_str() == Some(boot.as_str()) && !rx.done.contains(object))
                    .then_some(())?;
                let endpoint = u16::try_from(v["endpoint"].as_u64()?).ok()?;
                let reg = self.regs.get(&endpoint)?;
                let manifest = match v["manifest"].as_str() {
                    Some(h) => Some(Manifest::decode(&crate::hex::decode(h)?)?),
                    None => None,
                };
                let bits = crate::hex::decode(v["have"].as_str()?)?;
                let f = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(self.part_path(object))
                    .ok()?;
                let mut c = Collect::new(endpoint, reg.module_tag);
                for i in 0..bits.len() * 8 {
                    if bits[i / 8] & (1 << (i % 8)) != 0 {
                        c.set(i as u32);
                    }
                }
                c.part_bytes = v["bytes"].as_u64()?;
                c.part = Some(f);
                c.manifest = manifest;
                Some(c)
            });
            match parsed {
                Some(c) => {
                    rx.partial += c.part_bytes;
                    rx.objects.insert(object, c);
                    wanted.insert(object);
                }
                None => {
                    let _ = std::fs::remove_file(&side);
                }
            }
        }
        // .part files nobody describes any more
        if let Ok(rd) = std::fs::read_dir(&self.parts) {
            for e in rd.flatten() {
                let p = e.path();
                let obj = p
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .and_then(|s| s.parse::<u64>().ok());
                if p.extension().is_some_and(|x| x == "part")
                    && !obj.is_some_and(|o| wanted.contains(&o))
                {
                    let _ = std::fs::remove_file(&p);
                }
            }
        }
        // An object completed just before a crash finishes now.
        let complete: Vec<u64> = rx
            .objects
            .iter()
            .filter(|(_, c)| c.complete())
            .map(|(k, _)| *k)
            .collect();
        for o in complete {
            self.finish(&mut rx, o);
        }
    }
}

fn load_state(path: &Path) -> (u64, Ranges) {
    let Some(v) = std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
    else {
        return (0, Ranges::default());
    };
    let boot = v["boot"]
        .as_str()
        .and_then(|s| u64::from_str_radix(s, 16).ok())
        .unwrap_or(0);
    let mut ranges: Vec<(u64, u64)> = v["done"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|r| Some((r.get(0)?.as_u64()?, r.get(1)?.as_u64()?)))
                .filter(|r| r.0 <= r.1)
                .collect()
        })
        .unwrap_or_default();
    ranges.sort_unstable();
    let mut done = Ranges::default();
    for (a, b) in ranges {
        // re-insert ranges through the merging logic
        if done.0.last().is_some_and(|l| l.1 + 1 >= a) {
            let l = done.0.last_mut().expect("checked");
            l.1 = l.1.max(b);
        } else {
            done.0.push((a, b));
        }
    }
    (boot, done)
}

fn file_digest(f: &File, len: u64) -> [u8; 32] {
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut off = 0u64;
    while off < len {
        let n = (len - off).min(buf.len() as u64) as usize;
        if f.read_exact_at(&mut buf[..n], off).is_err() {
            return [0u8; 32];
        }
        h.update(&buf[..n]);
        off += n as u64;
    }
    h.finalize().into()
}

/// Feed each complete object to a fresh envelope parser of its module.
/// Dropping the parser settles it: delivered if genuine, refused if not.
fn deliver_loop(reg: &Arc<Registration>, rx: &crossbeam_channel::Receiver<Payload>) {
    for p in rx {
        match p {
            Payload::Mem(b) => {
                let mut parser = Parser::new(Arc::clone(reg));
                let _ = parser.write_all(&b);
                drop(parser);
            }
            Payload::File {
                path,
                object,
                len,
                sha256,
            } => {
                deliver_file(reg, &path, object, len, sha256);
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

/// Check a complete .part file against its manifest, then stream it into
/// a parser.  A parser is only made for bytes that passed the check.
fn deliver_file(reg: &Arc<Registration>, path: &Path, object: u64, len: u64, sha256: [u8; 32]) {
    let mut f = match File::open(path) {
        Ok(f) => f,
        Err(e) => {
            log::error!("air: cannot open {}: {e}", path.display());
            return;
        }
    };
    if file_digest(&f, len) != sha256 {
        log::warn!("air: object {object} refused: SHA-256 differs from its manifest");
        crate::stats::count("air_objects_bad_hash");
        return;
    }
    let mut parser = Parser::new(Arc::clone(reg));
    let mut buf = vec![0u8; 1 << 20];
    let mut left = len;
    while left > 0 {
        let want = buf.len().min(left as usize);
        match f.read(&mut buf[..want]) {
            Ok(0) => break,
            Ok(n) => {
                left -= n as u64;
                if parser.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => {
                log::error!("air: reading {}: {e}", path.display());
                break;
            }
        }
    }
    // dropping the parser settles the transfer: delivered or refused
    drop(parser);
}

/// Ask for a large socket buffer: a radio delivers in bursts, and a full
/// buffer is loss the FEC then has to pay for.
fn grow_receive_buffer(s: &UdpSocket) {
    use std::os::fd::AsRawFd;
    let size: libc::c_int = 8 << 20;
    // SAFETY: setsockopt with a valid fd and a pointer to a c_int of the
    // size passed.
    unsafe {
        libc::setsockopt(
            s.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            (&raw const size).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_merge() {
        let mut r = Ranges::default();
        for v in [5, 3, 4, 10, 1, 2, 12, 11] {
            r.insert(v);
        }
        assert_eq!(r.0, vec![(1, 5), (10, 12)]);
        assert!(r.contains(1) && r.contains(5) && r.contains(11));
        assert!(!r.contains(6) && !r.contains(0) && !r.contains(13));
        r.insert(6);
        r.insert(9);
        r.insert(8);
        r.insert(7);
        assert_eq!(r.0, vec![(1, 12)]);
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn state_file_round_trip() {
        let d = crate::testutil::TempDir::new();
        let p = d.path().join("s.json");
        std::fs::write(
            &p,
            br#"{"boot":"00000064000000ab","done":[[4,6],[1,3],[9,9]]}"#,
        )
        .unwrap();
        let (boot, done) = load_state(&p);
        assert_eq!(boot, 0x64_0000_00ab);
        assert_eq!(done.0, vec![(1, 6), (9, 9)]);
        assert_eq!(
            load_state(&d.path().join("missing")),
            (0, Ranges::default())
        );
    }
}
