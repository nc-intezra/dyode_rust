//! `dyode probe`: measure a one-way link before trusting video to it.
//!
//! iperf3 cannot run through a diode or a receive-only radio (it needs a
//! TCP control channel), so DYODE carries its own probe:
//!
//! * `dyode probe send` sends numbered, timestamped UDP datagrams at a fixed
//!   rate, one thread and one UDP port per stream (`--streams`), each paced
//!   on its own schedule.
//! * `dyode probe recv` listens on the same ports, one thread per port, and
//!   reports loss, the burst-length histogram (packets and milliseconds), a
//!   Gilbert-Elliott fit (with the `tc netem` line that reproduces it on the
//!   bench), one-way delay and jitter, and what RaptorQ with a given span T
//!   and repair ratio r would have left unrecovered (`--fec-sim`).
//! * `dyode probe analyze` replays a receive log (`recv --log`) through the
//!   same analysis, for field logs or other `--fec-sim` settings.
//!
//! Wire format (little-endian), `HEADER_LEN` bytes then filler up to `--size`:
//!
//! ```text
//!  0  "DYP1"     magic
//!  4  u8         version (1)
//!  5  u8         flags (1 = end marker)
//!  6  u16        stream (= destination port - base port)
//!  8  u64        run id (random per sender run)
//! 16  u64        sequence number (end marker: packets sent)
//! 24  u64        send time, ns since the Unix epoch (CLOCK_REALTIME)
//! 32  u32        CRC-32 of the whole datagram with this field zeroed
//! 36  u32        nominal interval between packets, microseconds
//! ```
//!
//! Probe datagrams carry no HMAC: this is a measurement tool for test links,
//! not a transport.  Never point it at a production DYODE port.

use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const MAGIC: [u8; 4] = *b"DYP1";
pub const VERSION: u8 = 1;
pub const HEADER_LEN: usize = 40;
/// Largest UDP payload over IPv4.
pub const MAX_SIZE: usize = 65_507;
pub const FLAG_END: u8 = 1;
/// End markers sent after the last packet (several, the link is lossy).
const END_MARKERS: u32 = 5;
const END_SPACING: Duration = Duration::from_millis(50);

// ---------------------------------------------------------------- packets

/// The probe header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub flags: u8,
    pub stream: u16,
    pub run_id: u64,
    pub seq: u64,
    pub send_ns: u64,
    pub interval_us: u32,
}

impl Header {
    pub fn is_end(&self) -> bool {
        self.flags & FLAG_END != 0
    }
}

/// Why a datagram was not accepted as a probe packet.
#[derive(Debug, PartialEq, Eq)]
pub enum Bad {
    /// Too short, wrong magic or unknown version: not ours.
    Foreign,
    /// Ours, but the CRC does not match: damaged on the way.
    Corrupt,
}

/// Write `h` into the start of `buf` and seal the whole buffer with the CRC.
pub fn encode(buf: &mut [u8], h: &Header) {
    assert!(
        buf.len() >= HEADER_LEN,
        "probe packet shorter than its header"
    );
    buf[0..4].copy_from_slice(&MAGIC);
    buf[4] = VERSION;
    buf[5] = h.flags;
    buf[6..8].copy_from_slice(&h.stream.to_le_bytes());
    buf[8..16].copy_from_slice(&h.run_id.to_le_bytes());
    buf[16..24].copy_from_slice(&h.seq.to_le_bytes());
    buf[24..32].copy_from_slice(&h.send_ns.to_le_bytes());
    buf[32..36].copy_from_slice(&[0; 4]);
    buf[36..40].copy_from_slice(&h.interval_us.to_le_bytes());
    let crc = crc32fast::hash(buf);
    buf[32..36].copy_from_slice(&crc.to_le_bytes());
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

/// Parse and check a received datagram.
pub fn decode(buf: &[u8]) -> Result<Header, Bad> {
    if buf.len() < HEADER_LEN || buf[0..4] != MAGIC || buf[4] != VERSION {
        return Err(Bad::Foreign);
    }
    let want = u32::from_le_bytes(buf[32..36].try_into().unwrap());
    let mut h = crc32fast::Hasher::new();
    h.update(&buf[..32]);
    h.update(&[0; 4]);
    h.update(&buf[36..]);
    if h.finalize() != want {
        return Err(Bad::Corrupt);
    }
    Ok(Header {
        flags: buf[5],
        stream: u16::from_le_bytes([buf[6], buf[7]]),
        run_id: u64_at(buf, 8),
        seq: u64_at(buf, 16),
        send_ns: u64_at(buf, 24),
        interval_us: u32::from_le_bytes(buf[36..40].try_into().unwrap()),
    })
}

pub fn realtime_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------- FEC what-if

/// One `--fec-sim` setting: RaptorQ protecting every `span` of packets with
/// repair ratio `ratio` (repair packets = source packets x ratio).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FecSpec {
    pub span_ms: f64,
    pub ratio: f64,
}

impl FecSpec {
    /// `200ms:1`, `0.5s:3`, `200:0.25` (plain numbers are milliseconds).
    pub fn parse(text: &str) -> Result<Self, String> {
        let (span, ratio) = text
            .split_once(':')
            .ok_or_else(|| format!("FEC setting {text:?}: expected SPAN:RATIO, e.g. 200ms:1"))?;
        let span = span.trim();
        let span_ms = if let Some(ms) = span.strip_suffix("ms") {
            ms.trim().parse::<f64>().ok()
        } else if let Some(s) = span.strip_suffix('s') {
            s.trim().parse::<f64>().ok().map(|s| s * 1000.0)
        } else {
            span.parse::<f64>().ok()
        }
        .filter(|v| v.is_finite() && *v > 0.0)
        .ok_or_else(|| format!("FEC setting {text:?}: bad span {span:?}"))?;
        let ratio = ratio
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite() && *v >= 0.0 && *v <= 100.0)
            .ok_or_else(|| {
                format!("FEC setting {text:?}: bad repair ratio {ratio:?} (0 to 100)")
            })?;
        Ok(Self { span_ms, ratio })
    }

    pub fn parse_list(text: &str) -> Result<Vec<Self>, String> {
        text.split(',')
            .filter(|s| !s.trim().is_empty())
            .map(Self::parse)
            .collect()
    }

    /// The settings the streaming design doc tabulates.
    pub fn defaults() -> Vec<Self> {
        [(100.0, 0.25), (200.0, 1.0), (200.0, 3.0), (500.0, 1.0)]
            .into_iter()
            .map(|(span_ms, ratio)| Self { span_ms, ratio })
            .collect()
    }
}

/// Replays the arrival pattern through fixed groups of n = T x packet rate
/// consecutive packets, of which k = n / (1 + r) are source.  A group is
/// lost when fewer than k arrived (RaptorQ needs K symbols; K + 2 gives
/// ~99.99%, so groups with K or K + 1 are counted "at risk").  Only the
/// source packets missing from a failed group are unrecoverable: the code is
/// systematic, so the ones that arrived are still delivered.
struct FecSim {
    spec: FecSpec,
    n: u64,
    k: u64,
    in_group: u64,
    lost_in_group: u64,
    groups: u64,
    failed: u64,
    at_risk: u64,
    unrecovered_src: f64,
}

impl FecSim {
    fn new(spec: FecSpec, interval_ns: u64) -> Self {
        let n = ((spec.span_ms * 1e6 / interval_ns.max(1) as f64).round() as u64).max(2);
        let k = ((n as f64 / (1.0 + spec.ratio)).round() as u64).clamp(1, n);
        Self {
            spec,
            n,
            k,
            in_group: 0,
            lost_in_group: 0,
            groups: 0,
            failed: 0,
            at_risk: 0,
            unrecovered_src: 0.0,
        }
    }

    fn push(&mut self, got: bool) {
        self.in_group += 1;
        if !got {
            self.lost_in_group += 1;
        }
        if self.in_group == self.n {
            self.groups += 1;
            let rx = self.n - self.lost_in_group;
            if rx < self.k {
                self.failed += 1;
                self.unrecovered_src += self.lost_in_group as f64 * self.k as f64 / self.n as f64;
            } else if rx < self.k + 2 && self.k < self.n {
                self.at_risk += 1;
            }
            self.in_group = 0;
            self.lost_in_group = 0;
        }
    }

    fn report(&self) -> FecReport {
        let src = (self.groups * self.k) as f64;
        FecReport {
            span_ms: self.spec.span_ms,
            ratio: self.spec.ratio,
            n: self.n,
            k: self.k,
            blocks: self.groups,
            failed: self.failed,
            failed_pct: pct(self.failed as f64, self.groups as f64),
            at_risk: self.at_risk,
            unrecovered_pct: pct(self.unrecovered_src, src),
        }
    }
}

fn pct(a: f64, b: f64) -> f64 {
    if b > 0.0 { 100.0 * a / b } else { 0.0 }
}

// ---------------------------------------------------------------- analysis

/// Burst-duration buckets, upper bounds in ms (the last is open-ended).
const MS_BUCKETS: [f64; 12] = [
    1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0, 200.0, 500.0, 1000.0, 2000.0, 5000.0,
];

fn ms_bucket_label(i: usize) -> String {
    match i {
        0 => "<1ms".into(),
        i if i < MS_BUCKETS.len() => format!("{}-{}ms", MS_BUCKETS[i - 1], MS_BUCKETS[i]),
        _ => format!(">={}ms", MS_BUCKETS[MS_BUCKETS.len() - 1]),
    }
}

#[derive(Default)]
struct DelayStats {
    n: u64,
    min: i64,
    max: i64,
    sum: f64,
    samples: Vec<i64>,
    stride: u64,
    jitter: f64,
    last_transit: Option<i64>,
}

const DELAY_SAMPLES: usize = 200_000;

impl DelayStats {
    fn add(&mut self, d: i64) {
        if self.n == 0 {
            self.min = d;
            self.max = d;
            self.stride = 1;
        }
        self.min = self.min.min(d);
        self.max = self.max.max(d);
        self.sum += d as f64;
        if self.n % self.stride == 0 {
            if self.samples.len() >= DELAY_SAMPLES {
                // Keep every other sample and halve the sampling rate.
                let kept: Vec<i64> = self.samples.iter().step_by(2).copied().collect();
                self.samples = kept;
                self.stride *= 2;
            }
            self.samples.push(d);
        }
        self.n += 1;
        // RFC 3550 interarrival jitter.
        if let Some(prev) = self.last_transit {
            let diff = (d - prev).abs() as f64;
            self.jitter += (diff - self.jitter) / 16.0;
        }
        self.last_transit = Some(d);
    }

    fn report(&self) -> Option<DelayReport> {
        if self.n == 0 {
            return None;
        }
        let mut s = self.samples.clone();
        s.sort_unstable();
        let q = |p: f64| s[((s.len() - 1) as f64 * p).round() as usize] as f64 / 1e6;
        Some(DelayReport {
            min_ms: self.min as f64 / 1e6,
            median_ms: q(0.5),
            p99_ms: q(0.99),
            max_ms: self.max as f64 / 1e6,
            mean_ms: self.sum / self.n as f64 / 1e6,
            variation_p99_ms: q(0.99) - self.min as f64 / 1e6,
            jitter_ms: self.jitter / 1e6,
        })
    }
}

/// Loss and timing analysis of one stream run.  Packets may arrive out of
/// order inside a reorder window; a sequence number is judged received or
/// lost once it leaves the window, in order, which drives the burst and
/// Gilbert-Elliott statistics and the FEC simulation.
pub struct Analyzer {
    pub stream: u16,
    pub run_id: u64,
    interval_us: u32,
    interval_ns: u64,
    window: u64,
    ring_seq: Vec<u64>,
    ring_send: Vec<u64>,
    started: bool,
    base: u64,
    first_seq: u64,
    highest: Option<u64>,
    end_count: Option<u64>,
    finished: bool,
    received: u64,
    duplicates: u64,
    reordered: u64,
    late: u64,
    bytes: u64,
    size: usize,
    judged: u64,
    lost: u64,
    loss_run: u64,
    prev_rx_send: Option<u64>,
    bursts: BTreeMap<u64, u64>,
    burst_ms: [u64; MS_BUCKETS.len() + 1],
    longest: u64,
    longest_ms: f64,
    prev: Option<bool>,
    /// Transition counts, index = prev_received * 2 + received.
    trans: [u64; 4],
    delay: DelayStats,
    fec: Vec<FecSim>,
    first_recv_ns: u64,
    last_recv_ns: u64,
    // interval counters for the live report
    iv_rx: u64,
    iv_bytes: u64,
    iv_judged: u64,
    iv_lost: u64,
    iv_longest: u64,
}

impl Analyzer {
    /// `reorder_ms`: how long a packet may arrive late and still count.
    pub fn new(
        stream: u16,
        run_id: u64,
        interval_us: u32,
        reorder_ms: f64,
        fec: &[FecSpec],
    ) -> Self {
        let interval_ns = u64::from(interval_us.max(1)) * 1000;
        let window = ((reorder_ms * 1e6 / interval_ns as f64).ceil() as u64).clamp(64, 1 << 18);
        Self {
            stream,
            run_id,
            interval_us,
            interval_ns,
            window,
            ring_seq: vec![0; window as usize],
            ring_send: vec![0; window as usize],
            started: false,
            base: 0,
            first_seq: 0,
            highest: None,
            end_count: None,
            finished: false,
            received: 0,
            duplicates: 0,
            reordered: 0,
            late: 0,
            bytes: 0,
            size: 0,
            judged: 0,
            lost: 0,
            loss_run: 0,
            prev_rx_send: None,
            bursts: BTreeMap::new(),
            burst_ms: [0; MS_BUCKETS.len() + 1],
            longest: 0,
            longest_ms: 0.0,
            prev: None,
            trans: [0; 4],
            delay: DelayStats::default(),
            fec: fec.iter().map(|s| FecSim::new(*s, interval_ns)).collect(),
            first_recv_ns: 0,
            last_recv_ns: 0,
            iv_rx: 0,
            iv_bytes: 0,
            iv_judged: 0,
            iv_lost: 0,
            iv_longest: 0,
        }
    }

    /// A data packet arrived.
    pub fn packet(&mut self, seq: u64, send_ns: u64, recv_ns: u64, len: usize) {
        if self.finished {
            self.late += 1;
            return;
        }
        if !self.started {
            self.started = true;
            // A receiver started after the sender misses the first packets;
            // only count from seq 0 if that is plausibly within reach.
            self.base = if seq < self.window { 0 } else { seq };
            self.first_seq = self.base;
            self.first_recv_ns = recv_ns;
        }
        if seq < self.base {
            self.late += 1;
            return;
        }
        while seq >= self.base + self.window {
            self.judge_next();
        }
        let slot = (seq % self.window) as usize;
        if self.ring_seq[slot] == seq + 1 {
            self.duplicates += 1;
            return;
        }
        self.ring_seq[slot] = seq + 1;
        self.ring_send[slot] = send_ns;
        self.received += 1;
        self.bytes += len as u64;
        self.size = self.size.max(len);
        self.iv_rx += 1;
        self.iv_bytes += len as u64;
        self.last_recv_ns = recv_ns;
        match self.highest {
            Some(h) if seq < h => self.reordered += 1,
            _ => self.highest = Some(seq),
        }
        self.delay.add(recv_ns as i64 - send_ns as i64);
    }

    /// An end marker arrived: the sender sent `count` packets.
    pub fn end(&mut self, count: u64) {
        self.end_count = Some(count);
    }

    pub fn has_end(&self) -> bool {
        self.end_count.is_some()
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }

    fn judge_next(&mut self) {
        let seq = self.base;
        let slot = (seq % self.window) as usize;
        let got = self.ring_seq[slot] == seq + 1;
        let send = self.ring_send[slot];
        self.ring_seq[slot] = 0;
        self.base += 1;
        self.judged += 1;
        self.iv_judged += 1;
        if got {
            if self.loss_run > 0 {
                self.record_burst(Some(send));
            }
            self.prev_rx_send = Some(send);
        } else {
            self.lost += 1;
            self.iv_lost += 1;
            self.loss_run += 1;
        }
        if let Some(p) = self.prev {
            self.trans[usize::from(p) * 2 + usize::from(got)] += 1;
        }
        self.prev = Some(got);
        for f in &mut self.fec {
            f.push(got);
        }
    }

    fn record_burst(&mut self, next_send: Option<u64>) {
        let len = self.loss_run;
        self.loss_run = 0;
        // The outage lasted from just after the last packet received before
        // it to just before the first one after it.
        let nominal = len * self.interval_ns;
        let ns = match (self.prev_rx_send, next_send) {
            (Some(a), Some(b)) if b > a => (b - a).saturating_sub(self.interval_ns).max(1),
            _ => nominal,
        };
        let ms = ns as f64 / 1e6;
        *self.bursts.entry(len).or_default() += 1;
        let i = MS_BUCKETS
            .iter()
            .position(|b| ms < *b)
            .unwrap_or(MS_BUCKETS.len());
        self.burst_ms[i] += 1;
        if len > self.longest {
            self.longest = len;
        }
        self.longest_ms = self.longest_ms.max(ms);
        self.iv_longest = self.iv_longest.max(len);
    }

    /// Judge everything up to the end marker (or the highest packet seen)
    /// and close the analysis.
    pub fn finish(&mut self) {
        if self.finished {
            return;
        }
        if self.started {
            let mut target = self.highest.map_or(self.base, |h| h + 1);
            if let Some(end) = self.end_count {
                target = target.max(end);
            }
            while self.base < target {
                self.judge_next();
            }
            if self.loss_run > 0 {
                self.record_burst(None);
            }
        }
        self.finished = true;
    }

    /// One line for the live report; resets the interval counters.
    pub fn interval_line(&mut self, secs: f64) -> String {
        let line = format!(
            "stream {} | rx {:>8.0} pkt/s {:>9.3} Mbit/s | loss {:>7.3}% | longest burst {} pkt",
            self.stream,
            self.iv_rx as f64 / secs,
            self.iv_bytes as f64 * 8.0 / secs / 1e6,
            pct(self.iv_lost as f64, self.iv_judged as f64),
            self.iv_longest,
        );
        self.iv_rx = 0;
        self.iv_bytes = 0;
        self.iv_judged = 0;
        self.iv_lost = 0;
        self.iv_longest = 0;
        line
    }

    pub fn report(&self) -> Report {
        // Index = prev_received * 2 + received.
        let [lost_lost, lost_rx, rx_lost, rx_rx] = self.trans.map(|v| v as f64);
        let (rr, rl, lr, ll) = (rx_rx, rx_lost, lost_rx, lost_lost);
        let p = if rr + rl > 0.0 { rl / (rr + rl) } else { 0.0 };
        let r = if lr + ll > 0.0 { lr / (lr + ll) } else { 1.0 };
        let secs = self.last_recv_ns.saturating_sub(self.first_recv_ns) as f64 / 1e9;
        let bursts_total: u64 = self.bursts.values().sum();
        Report {
            stream: self.stream,
            run_id: format!("{:016x}", self.run_id),
            interval_us: self.interval_us,
            packet_rate: 1e6 / f64::from(self.interval_us.max(1)),
            packet_size: self.size,
            reorder_window: self.window,
            first_seq: self.first_seq,
            expected: self.judged,
            received: self.received,
            lost: self.lost,
            loss_pct: pct(self.lost as f64, self.judged as f64),
            duplicates: self.duplicates,
            reordered: self.reordered,
            late: self.late,
            end_marker: self.end_count.is_some(),
            seconds: secs,
            mbps: if secs > 0.0 {
                self.bytes as f64 * 8.0 / secs / 1e6
            } else {
                0.0
            },
            bursts: bursts_total,
            burst_lengths: self.bursts.iter().map(|(k, v)| (*k, *v)).collect(),
            burst_durations: self
                .burst_ms
                .iter()
                .enumerate()
                .filter(|(_, c)| **c > 0)
                .map(|(i, c)| (ms_bucket_label(i), *c))
                .collect(),
            longest_burst: self.longest,
            longest_burst_ms: self.longest_ms,
            mean_burst: if bursts_total > 0 {
                self.lost as f64 / bursts_total as f64
            } else {
                0.0
            },
            gilbert: Gilbert {
                p_pct: 100.0 * p,
                r_pct: 100.0 * r,
                netem: format!("loss gemodel {:.4}% {:.4}%", 100.0 * p, 100.0 * r),
            },
            delay: self.delay.report(),
            fec: self.fec.iter().map(FecSim::report).collect(),
        }
    }
}

#[derive(Serialize, Clone, Debug)]
pub struct Gilbert {
    /// P(lose the next packet | this one arrived), %.
    pub p_pct: f64,
    /// P(next packet arrives | this one was lost), %.
    pub r_pct: f64,
    /// Reproduces the pattern with `tc qdisc ... netem <this>`.
    pub netem: String,
}

#[derive(Serialize, Clone, Debug)]
pub struct DelayReport {
    pub min_ms: f64,
    pub median_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
    pub mean_ms: f64,
    pub variation_p99_ms: f64,
    pub jitter_ms: f64,
}

#[derive(Serialize, Clone, Debug)]
pub struct FecReport {
    pub span_ms: f64,
    pub ratio: f64,
    pub n: u64,
    pub k: u64,
    pub blocks: u64,
    pub failed: u64,
    pub failed_pct: f64,
    pub at_risk: u64,
    pub unrecovered_pct: f64,
}

#[derive(Serialize, Clone, Debug)]
pub struct Report {
    pub stream: u16,
    pub run_id: String,
    pub interval_us: u32,
    pub packet_rate: f64,
    pub packet_size: usize,
    pub reorder_window: u64,
    pub first_seq: u64,
    pub expected: u64,
    pub received: u64,
    pub lost: u64,
    pub loss_pct: f64,
    pub duplicates: u64,
    pub reordered: u64,
    pub late: u64,
    pub end_marker: bool,
    pub seconds: f64,
    pub mbps: f64,
    pub bursts: u64,
    pub burst_lengths: Vec<(u64, u64)>,
    pub burst_durations: Vec<(String, u64)>,
    pub longest_burst: u64,
    pub longest_burst_ms: f64,
    pub mean_burst: f64,
    pub gilbert: Gilbert,
    pub delay: Option<DelayReport>,
    pub fec: Vec<FecReport>,
}

impl Report {
    /// The human-readable block printed at the end of a run.
    pub fn render(&self) -> String {
        let mut o = String::new();
        let w = |o: &mut String, s: String| {
            o.push_str(&s);
            o.push('\n');
        };
        w(
            &mut o,
            format!("stream {}  (run {})", self.stream, self.run_id),
        );
        let marker = if self.end_marker {
            "end marker seen"
        } else {
            "NO end marker: tail losses unknown"
        };
        w(
            &mut o,
            format!(
                "  packets   expected {}, received {}, lost {} ({:.4}%); duplicates {}, reordered {}, late {}  [{marker}]",
                self.expected,
                self.received,
                self.lost,
                self.loss_pct,
                self.duplicates,
                self.reordered,
                self.late
            ),
        );
        if self.first_seq > 0 {
            w(
                &mut o,
                format!(
                    "            joined late: counting from packet {}",
                    self.first_seq
                ),
            );
        }
        w(
            &mut o,
            format!(
                "  rate      {:.3} Mbit/s received over {:.1} s; {}-byte packets sent at {:.1} pkt/s",
                self.mbps, self.seconds, self.packet_size, self.packet_rate
            ),
        );
        if self.bursts == 0 {
            w(&mut o, "  bursts    none".into());
        } else {
            let mut lens: Vec<String> = self
                .burst_lengths
                .iter()
                .take(12)
                .map(|(l, c)| format!("{l}:{c}"))
                .collect();
            if self.burst_lengths.len() > 12 {
                lens.push(format!("... up to {}", self.longest_burst));
            }
            w(
                &mut o,
                format!(
                    "  bursts    {} bursts, mean {:.2} pkt, longest {} pkt ({:.1} ms)",
                    self.bursts, self.mean_burst, self.longest_burst, self.longest_burst_ms
                ),
            );
            w(
                &mut o,
                format!("            length:count  {}", lens.join(" ")),
            );
            let durs: Vec<String> = self
                .burst_durations
                .iter()
                .map(|(l, c)| format!("{l}:{c}"))
                .collect();
            w(
                &mut o,
                format!("            duration      {}", durs.join(" ")),
            );
        }
        w(
            &mut o,
            format!(
                "  gilbert   p {:.4}%  r {:.4}%   bench replay: tc qdisc add dev IFACE root netem {}",
                self.gilbert.p_pct, self.gilbert.r_pct, self.gilbert.netem
            ),
        );
        if let Some(d) = &self.delay {
            w(
                &mut o,
                format!(
                    "  delay     min {:.3} ms, median {:.3}, p99 {:.3}, max {:.3}; variation p99 {:.3} ms; jitter {:.3} ms",
                    d.min_ms, d.median_ms, d.p99_ms, d.max_ms, d.variation_p99_ms, d.jitter_ms
                ),
            );
            w(
                &mut o,
                "            (absolute delay is only meaningful with synchronised clocks)".into(),
            );
        }
        for f in &self.fec {
            w(
                &mut o,
                format!(
                    "  fec sim   T {:>6.0} ms r {:<5} (n {} k {}): {} of {} blocks failed ({:.4}%), {} at risk, unrecovered {:.5}%",
                    f.span_ms,
                    f.ratio,
                    f.n,
                    f.k,
                    f.failed,
                    f.blocks,
                    f.failed_pct,
                    f.at_risk,
                    f.unrecovered_pct
                ),
            );
        }
        o
    }
}

// ---------------------------------------------------------------- sockets

fn setsockopt_int(sock: &UdpSocket, opt: libc::c_int, value: usize) -> bool {
    let v: libc::c_int = value.min(i32::MAX as usize) as libc::c_int;
    // SAFETY: a valid socket descriptor and a pointer to a live c_int.
    unsafe {
        libc::setsockopt(
            sock.as_raw_fd(),
            libc::SOL_SOCKET,
            opt,
            (&v as *const libc::c_int).cast(),
            size_of::<libc::c_int>() as libc::socklen_t,
        ) == 0
    }
}

fn getsockopt_int(sock: &UdpSocket, opt: libc::c_int) -> usize {
    let mut v: libc::c_int = 0;
    let mut len = size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: as above, with a writable c_int and its length.
    unsafe {
        libc::getsockopt(
            sock.as_raw_fd(),
            libc::SOL_SOCKET,
            opt,
            (&mut v as *mut libc::c_int).cast(),
            &mut len,
        );
    }
    v.max(0) as usize
}

/// Ask for a socket buffer of `want` bytes (the privileged *FORCE option
/// first, which ignores net.core.[rw]mem_max), return what the kernel gave.
fn set_buffer(sock: &UdpSocket, receive: bool, want: usize) -> usize {
    let (force, plain) = if receive {
        (libc::SO_RCVBUFFORCE, libc::SO_RCVBUF)
    } else {
        (libc::SO_SNDBUFFORCE, libc::SO_SNDBUF)
    };
    if !setsockopt_int(sock, force, want) {
        setsockopt_int(sock, plain, want);
    }
    // Linux reports double the requested size (bookkeeping overhead).
    getsockopt_int(sock, plain) / 2
}

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sigint(_: libc::c_int) {
    INTERRUPTED.store(true, Ordering::Relaxed);
}

/// Ctrl-C stops a run cleanly (the receiver still prints its report).
pub fn catch_ctrl_c() {
    // SAFETY: the handler only stores to an atomic, which is async-signal-safe.
    unsafe {
        libc::signal(libc::SIGINT, on_sigint as *const () as libc::sighandler_t);
    }
}

fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------- sender

#[derive(Clone, Debug)]
pub struct SendOptions {
    /// Destination of stream 0; stream i goes to port + i.
    pub to: SocketAddr,
    pub bind: Option<IpAddr>,
    pub streams: u16,
    /// Packets per second, per stream.
    pub pps: f64,
    /// UDP payload bytes per packet.
    pub size: usize,
    pub duration: Option<Duration>,
    pub count: Option<u64>,
    /// Packets sent back to back per tick (the average rate is unchanged).
    pub burst: u32,
    pub sndbuf: Option<usize>,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct SendReport {
    pub stream: u16,
    pub run_id: String,
    pub sent: u64,
    pub errors: u64,
    pub seconds: f64,
    pub mbps: f64,
    pub pps: f64,
    /// Worst lateness against the schedule (the sender could not keep up).
    pub max_late_ms: f64,
}

/// Sleep, then spin, until `due`.  Returns how late we already were.
fn wait_until(due: Instant) -> Duration {
    loop {
        let now = Instant::now();
        if now >= due {
            return now - due;
        }
        let left = due - now;
        if left > Duration::from_millis(2) {
            std::thread::sleep(left - Duration::from_millis(1));
        } else if left > Duration::from_micros(50) {
            std::thread::yield_now();
        } else {
            std::hint::spin_loop();
        }
    }
}

fn random_u64() -> u64 {
    let mut b = [0u8; 8];
    if getrandom::fill(&mut b).is_err() {
        return realtime_ns() ^ u64::from(std::process::id()) << 32;
    }
    u64::from_le_bytes(b)
}

fn send_stream(opts: &SendOptions, stream: u16, run_id: u64) -> Result<SendReport, String> {
    let dest = SocketAddr::new(opts.to.ip(), opts.to.port() + stream);
    let bind_ip = opts.bind.unwrap_or(match dest {
        SocketAddr::V4(_) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        SocketAddr::V6(_) => IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED),
    });
    let sock = UdpSocket::bind((bind_ip, 0))
        .map_err(|e| format!("stream {stream}: cannot bind {bind_ip}: {e}"))?;
    if let Some(b) = opts.sndbuf {
        set_buffer(&sock, false, b);
    }
    let interval_ns = 1e9 / opts.pps;
    let interval_us = (interval_ns / 1000.0)
        .round()
        .clamp(1.0, f64::from(u32::MAX)) as u32;
    let burst = u64::from(opts.burst.max(1));
    // Filler: a fixed pseudo-random pattern, so nothing on the path can
    // compress it and the CRC sees every byte.
    let mut buf = vec![0u8; opts.size];
    let mut x = run_id | 1;
    for b in &mut buf {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *b = x as u8;
    }
    let mut h = Header {
        flags: 0,
        stream,
        run_id,
        seq: 0,
        send_ns: 0,
        interval_us,
    };
    let start = Instant::now();
    let stop_at = opts.duration.map(|d| start + d);
    let (mut sent, mut errors, mut max_late) = (0u64, 0u64, Duration::ZERO);
    let mut seq = 0u64;
    'outer: loop {
        if interrupted() || opts.count.is_some_and(|c| seq >= c) {
            break;
        }
        let due = start
            + Duration::from_nanos(((seq / burst) as f64 * interval_ns * burst as f64) as u64);
        if stop_at.is_some_and(|s| due >= s) {
            break;
        }
        max_late = max_late.max(wait_until(due));
        for _ in 0..burst {
            if opts.count.is_some_and(|c| seq >= c) {
                break 'outer;
            }
            h.seq = seq;
            h.send_ns = realtime_ns();
            encode(&mut buf, &h);
            match sock.send_to(&buf, dest) {
                Ok(_) => sent += 1,
                // ENOBUFS and friends: the packet never left; count it as
                // sent-and-lost so the receiver's numbers stay comparable.
                Err(_) => errors += 1,
            }
            seq += 1;
        }
    }
    let secs = start.elapsed().as_secs_f64();
    h.flags = FLAG_END;
    h.seq = seq;
    for i in 0..END_MARKERS {
        if i > 0 {
            std::thread::sleep(END_SPACING);
        }
        h.send_ns = realtime_ns();
        encode(&mut buf[..HEADER_LEN], &h);
        let _ = sock.send_to(&buf[..HEADER_LEN], dest);
    }
    Ok(SendReport {
        stream,
        run_id: format!("{run_id:016x}"),
        sent,
        errors,
        seconds: secs,
        mbps: sent as f64 * opts.size as f64 * 8.0 / secs.max(1e-9) / 1e6,
        pps: sent as f64 / secs.max(1e-9),
        max_late_ms: max_late.as_secs_f64() * 1e3,
    })
}

/// Run every stream in its own thread; returns one report per stream.
pub fn run_send(opts: &SendOptions) -> Result<Vec<SendReport>, String> {
    if opts.size < HEADER_LEN || opts.size > MAX_SIZE {
        return Err(format!("--size must be {HEADER_LEN} to {MAX_SIZE} bytes"));
    }
    if !(opts.pps.is_finite() && opts.pps > 0.0) {
        return Err("the packet rate must be above zero".into());
    }
    if opts.streams == 0 || u32::from(opts.to.port()) + u32::from(opts.streams) - 1 > 65_535 {
        return Err("--streams: ports run past 65535".into());
    }
    let run_id = random_u64();
    let handles: Vec<_> = (0..opts.streams)
        .map(|s| {
            let o = opts.clone();
            std::thread::Builder::new()
                .name(format!("probe-send-{s}"))
                .spawn(move || send_stream(&o, s, run_id.wrapping_add(u64::from(s))))
                .map_err(|e| format!("cannot start a sender thread: {e}"))
        })
        .collect::<Result<_, _>>()?;
    handles
        .into_iter()
        .map(|h| {
            h.join()
                .map_err(|_| "a sender thread panicked".to_string())?
        })
        .collect()
}

// ---------------------------------------------------------------- receiver

#[derive(Clone, Debug)]
pub struct RecvOptions {
    /// Address and port of stream 0; stream i is on port + i.
    pub listen: SocketAddr,
    pub streams: u16,
    /// Stop when nothing arrived for this long (after the first packet).
    pub idle: Duration,
    pub duration: Option<Duration>,
    /// Seconds between live lines; zero for none.
    pub report_every: Duration,
    pub reorder_ms: f64,
    pub fec: Vec<FecSpec>,
    pub rcvbuf: usize,
    /// Keep listening after a run ends (field soak: one report per run).
    pub keep: bool,
    pub quiet: bool,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct PortCounters {
    pub port: u16,
    pub rcvbuf: usize,
    pub datagrams: u64,
    pub corrupt: u64,
    pub foreign: u64,
}

pub struct RecvResult {
    pub reports: Vec<Report>,
    pub ports: Vec<PortCounters>,
}

/// Per-packet log line: stream,run,seq,send_ns,recv_ns,len,interval_us,kind
type Log = Arc<Mutex<BufWriter<File>>>;

fn log_line(log: &Option<Log>, h: &Header, recv_ns: u64, len: usize) {
    if let Some(l) = log {
        let kind = if h.is_end() { 'e' } else { 'd' };
        let _ = writeln!(
            l.lock().unwrap(),
            "{},{:016x},{},{},{},{},{},{kind}",
            h.stream,
            h.run_id,
            h.seq,
            h.send_ns,
            recv_ns,
            len,
            h.interval_us
        );
    }
}

struct Shared {
    seen_any: AtomicBool,
    out: Mutex<()>,
}

fn recv_port(
    opts: &RecvOptions,
    port_index: u16,
    log: Option<Log>,
    shared: &Shared,
    reports: &Mutex<Vec<Report>>,
) -> Result<PortCounters, String> {
    let addr = SocketAddr::new(opts.listen.ip(), opts.listen.port() + port_index);
    let sock = UdpSocket::bind(addr).map_err(|e| format!("cannot listen on {addr}: {e}"))?;
    let rcvbuf = set_buffer(&sock, true, opts.rcvbuf);
    sock.set_read_timeout(Some(Duration::from_millis(100)))
        .map_err(|e| e.to_string())?;
    let mut counters = PortCounters {
        port: addr.port(),
        rcvbuf,
        ..Default::default()
    };
    let mut runs: HashMap<(u16, u64), Analyzer> = HashMap::new();
    let mut end_seen: HashMap<(u16, u64), Instant> = HashMap::new();
    let mut done_runs = 0u64;
    let mut buf = vec![0u8; MAX_SIZE + 1];
    let start = Instant::now();
    let mut last_packet: Option<Instant> = None;
    let mut last_line = Instant::now();
    let grace = Duration::from_secs_f64((opts.reorder_ms / 1000.0).max(0.2));
    let finish_run = |a: &mut Analyzer, reports: &Mutex<Vec<Report>>| {
        a.finish();
        let r = a.report();
        if !opts.quiet {
            let _g = shared.out.lock().unwrap();
            print!("{}", r.render());
        }
        reports.lock().unwrap().push(r);
    };
    loop {
        if interrupted() || opts.duration.is_some_and(|d| start.elapsed() >= d) {
            break;
        }
        match sock.recv_from(&mut buf) {
            Ok((len, _)) => {
                let now_ns = realtime_ns();
                counters.datagrams += 1;
                last_packet = Some(Instant::now());
                shared.seen_any.store(true, Ordering::Relaxed);
                match decode(&buf[..len]) {
                    Err(Bad::Foreign) => counters.foreign += 1,
                    Err(Bad::Corrupt) => counters.corrupt += 1,
                    Ok(h) => {
                        log_line(&log, &h, now_ns, len);
                        let key = (h.stream, h.run_id);
                        if !runs.contains_key(&key) {
                            if end_seen.contains_key(&key) {
                                continue; // a straggler of a run already reported
                            }
                            // A new run on this stream closes the previous one.
                            let old: Vec<_> =
                                runs.keys().filter(|k| k.0 == h.stream).copied().collect();
                            for k in old {
                                let mut a = runs.remove(&k).unwrap();
                                finish_run(&mut a, reports);
                                end_seen.insert(k, Instant::now());
                                done_runs += 1;
                            }
                            runs.insert(
                                key,
                                Analyzer::new(
                                    h.stream,
                                    h.run_id,
                                    h.interval_us,
                                    opts.reorder_ms,
                                    &opts.fec,
                                ),
                            );
                        }
                        let a = runs.get_mut(&key).unwrap();
                        if h.is_end() {
                            if !a.has_end() {
                                end_seen.insert(key, Instant::now());
                            }
                            a.end(h.seq);
                        } else {
                            a.packet(h.seq, h.send_ns, now_ns, len);
                        }
                    }
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(e) => return Err(format!("receive on {addr}: {e}")),
        }
        // Runs whose end marker arrived and whose grace period is over.
        let ready: Vec<_> = runs
            .iter()
            .filter(|(k, a)| a.has_end() && end_seen.get(*k).is_some_and(|t| t.elapsed() >= grace))
            .map(|(k, _)| *k)
            .collect();
        for k in ready {
            let mut a = runs.remove(&k).unwrap();
            finish_run(&mut a, reports);
            done_runs += 1;
        }
        if !opts.report_every.is_zero() && last_line.elapsed() >= opts.report_every {
            let secs = last_line.elapsed().as_secs_f64();
            last_line = Instant::now();
            if !opts.quiet {
                let mut keys: Vec<_> = runs.keys().copied().collect();
                keys.sort();
                for k in keys {
                    let line = runs.get_mut(&k).unwrap().interval_line(secs);
                    let _g = shared.out.lock().unwrap();
                    println!("{:>8.1}s  {line}", start.elapsed().as_secs_f64());
                }
            }
        }
        if !opts.keep && done_runs > 0 && runs.is_empty() {
            break;
        }
        // Idle: counted from our own last packet, or from the moment any
        // port saw traffic (a port whose stream never came).
        if shared.seen_any.load(Ordering::Relaxed) {
            let since = last_packet.map_or(Duration::MAX, |t| t.elapsed());
            if last_packet.is_none() {
                last_packet = Some(Instant::now());
            } else if since >= opts.idle {
                if opts.keep {
                    for (k, mut a) in runs.drain() {
                        finish_run(&mut a, reports);
                        end_seen.insert(k, Instant::now());
                    }
                    last_packet = None;
                    shared.seen_any.store(false, Ordering::Relaxed);
                } else {
                    break;
                }
            }
        }
    }
    let mut left: Vec<_> = runs.into_iter().collect();
    left.sort_by_key(|(k, _)| *k);
    for (_, mut a) in left {
        finish_run(&mut a, reports);
    }
    Ok(counters)
}

/// Listen on every stream's port, one thread each, until the runs end.
pub fn run_recv(opts: &RecvOptions, log_path: Option<&Path>) -> Result<RecvResult, String> {
    if opts.streams == 0 || u32::from(opts.listen.port()) + u32::from(opts.streams) - 1 > 65_535 {
        return Err("--streams: ports run past 65535".into());
    }
    let log: Option<Log> = match log_path {
        Some(p) => {
            let mut f = BufWriter::new(
                File::create(p).map_err(|e| format!("cannot create {}: {e}", p.display()))?,
            );
            let _ = writeln!(
                f,
                "# dyode probe log v1: stream,run,seq,send_ns,recv_ns,len,interval_us,kind(d=data,e=end)"
            );
            Some(Arc::new(Mutex::new(f)))
        }
        None => None,
    };
    let shared = Arc::new(Shared {
        seen_any: AtomicBool::new(false),
        out: Mutex::new(()),
    });
    let reports = Arc::new(Mutex::new(Vec::new()));
    let handles: Vec<_> = (0..opts.streams)
        .map(|i| {
            let (o, l, s, r) = (
                opts.clone(),
                log.clone(),
                Arc::clone(&shared),
                Arc::clone(&reports),
            );
            std::thread::Builder::new()
                .name(format!("probe-recv-{i}"))
                .spawn(move || recv_port(&o, i, l, &s, &r))
                .map_err(|e| format!("cannot start a receiver thread: {e}"))
        })
        .collect::<Result<_, _>>()?;
    let mut ports = Vec::new();
    let mut first_err = None;
    for h in handles {
        match h.join() {
            Ok(Ok(c)) => ports.push(c),
            Ok(Err(e)) => {
                first_err.get_or_insert(e);
            }
            Err(_) => {
                first_err.get_or_insert("a receiver thread panicked".to_string());
            }
        }
    }
    if let Some(l) = &log {
        let _ = l.lock().unwrap().flush();
    }
    if let Some(e) = first_err {
        return Err(e);
    }
    let mut reports = std::mem::take(&mut *reports.lock().unwrap());
    reports.sort_by_key(|r| (r.stream, r.run_id.clone()));
    Ok(RecvResult { reports, ports })
}

// ---------------------------------------------------------------- analyze

/// Replay a `recv --log` file.  Lines keep their arrival order, so the
/// result matches what the live receiver saw (with other `--fec-sim` or
/// `--reorder-ms` settings if wanted).
pub fn analyze_log(path: &Path, reorder_ms: f64, fec: &[FecSpec]) -> Result<Vec<Report>, String> {
    let f = File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    let mut order: VecDeque<(u16, u64)> = VecDeque::new();
    let mut runs: HashMap<(u16, u64), Analyzer> = HashMap::new();
    for (n, line) in BufReader::new(f).lines().enumerate() {
        let line = line.map_err(|e| format!("{}: {e}", path.display()))?;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let bad = || format!("{}:{}: not a probe log line", path.display(), n + 1);
        let c: Vec<&str> = line.split(',').collect();
        if c.len() != 8 {
            return Err(bad());
        }
        let num = |i: usize| c[i].parse::<u64>().map_err(|_| bad());
        let stream = u16::try_from(num(0)?).map_err(|_| bad())?;
        let run = u64::from_str_radix(c[1], 16).map_err(|_| bad())?;
        let (seq, send_ns, recv_ns, len) = (num(2)?, num(3)?, num(4)?, num(5)? as usize);
        let interval_us = u32::try_from(num(6)?).map_err(|_| bad())?;
        let key = (stream, run);
        let a = runs.entry(key).or_insert_with(|| {
            order.push_back(key);
            Analyzer::new(stream, run, interval_us, reorder_ms, fec)
        });
        match c[7] {
            "e" => a.end(seq),
            "d" => a.packet(seq, send_ns, recv_ns, len),
            _ => return Err(bad()),
        }
    }
    Ok(order
        .into_iter()
        .map(|k| {
            let mut a = runs.remove(&k).unwrap();
            a.finish();
            a.report()
        })
        .collect())
}

/// The JSON document written by `--json`.
pub fn reports_json(kind: &str, reports: &[Report], ports: Option<&[PortCounters]>) -> Value {
    json!({
        "tool": "dyode probe",
        "version": env!("CARGO_PKG_VERSION"),
        "kind": kind,
        "time_ns": realtime_ns(),
        "ports": ports,
        "reports": reports,
    })
}

/// Totals across streams, for the last line of a multi-stream run.
pub fn totals_line(reports: &[Report]) -> String {
    let expected: u64 = reports.iter().map(|r| r.expected).sum();
    let lost: u64 = reports.iter().map(|r| r.lost).sum();
    let mbps: f64 = reports.iter().map(|r| r.mbps).sum();
    let longest = reports.iter().map(|r| r.longest_burst).max().unwrap_or(0);
    format!(
        "total: {} streams, {:.3} Mbit/s, expected {expected}, lost {lost} ({:.4}%), longest burst {longest} pkt",
        reports.len(),
        mbps,
        pct(lost as f64, expected as f64)
    )
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(a: &mut Analyzer, seqs: impl IntoIterator<Item = u64>) {
        for s in seqs {
            a.packet(s, s * 1_000_000, s * 1_000_000 + 500_000, 100);
        }
    }

    #[test]
    fn codec_roundtrip_and_crc() {
        let h = Header {
            flags: FLAG_END,
            stream: 3,
            run_id: 0xdead_beef,
            seq: 42,
            send_ns: 7,
            interval_us: 1000,
        };
        let mut buf = vec![0xAA; 200];
        encode(&mut buf, &h);
        assert_eq!(decode(&buf), Ok(h));
        assert!(decode(&buf).unwrap().is_end());
        buf[150] ^= 1;
        assert_eq!(decode(&buf), Err(Bad::Corrupt));
        assert_eq!(decode(b"hello"), Err(Bad::Foreign));
        let mut other = buf.clone();
        other[0] = b'X';
        assert_eq!(decode(&other), Err(Bad::Foreign));
    }

    #[test]
    fn fec_spec_parsing() {
        assert_eq!(
            FecSpec::parse("200ms:1").unwrap(),
            FecSpec {
                span_ms: 200.0,
                ratio: 1.0
            }
        );
        assert_eq!(
            FecSpec::parse("0.5s:3").unwrap(),
            FecSpec {
                span_ms: 500.0,
                ratio: 3.0
            }
        );
        assert_eq!(
            FecSpec::parse("100:0.25").unwrap(),
            FecSpec {
                span_ms: 100.0,
                ratio: 0.25
            }
        );
        assert_eq!(FecSpec::parse_list("100:1, 200ms:3").unwrap().len(), 2);
        assert!(FecSpec::parse("200").is_err());
        assert!(FecSpec::parse("0ms:1").is_err());
        assert!(FecSpec::parse("200ms:-1").is_err());
    }

    #[test]
    fn losses_bursts_and_gilbert() {
        // 1 ms apart; lose 100..=104 (a burst of 5) and 500 (a single).
        let mut a = Analyzer::new(0, 1, 1000, 100.0, &[]);
        feed(
            &mut a,
            (0..1000).filter(|s| !(100..=104).contains(s) && *s != 500),
        );
        a.end(1000);
        a.finish();
        let r = a.report();
        assert_eq!((r.expected, r.received, r.lost), (1000, 994, 6));
        assert_eq!(r.burst_lengths, vec![(1, 1), (5, 1)]);
        assert_eq!(r.longest_burst, 5);
        assert!(
            (r.longest_burst_ms - 5.0).abs() < 1e-9,
            "{}",
            r.longest_burst_ms
        );
        assert!((r.mean_burst - 3.0).abs() < 1e-9);
        // received->lost twice out of 993 transitions from received;
        // lost->received twice out of 6 from lost.
        assert!(
            (r.gilbert.p_pct - 100.0 * 2.0 / 993.0).abs() < 1e-9,
            "{:?}",
            r.gilbert
        );
        assert!((r.gilbert.r_pct - 100.0 * 2.0 / 6.0).abs() < 1e-9);
        assert!(r.gilbert.netem.starts_with("loss gemodel "));
        let d = r.delay.unwrap();
        assert!((d.min_ms - 0.5).abs() < 1e-9 && d.jitter_ms.abs() < 1e-9);
    }

    #[test]
    fn reordering_duplicates_and_late() {
        let mut a = Analyzer::new(0, 1, 1000, 100.0, &[]);
        feed(&mut a, [0, 2, 1, 3, 3]);
        feed(&mut a, 4..300);
        feed(&mut a, [5]); // long gone from the window: late (or a duplicate)
        a.finish();
        let r = a.report();
        assert_eq!((r.lost, r.reordered, r.duplicates, r.late), (0, 1, 1, 1));
    }

    #[test]
    fn tail_loss_needs_the_end_marker() {
        let mut a = Analyzer::new(0, 1, 1000, 100.0, &[]);
        feed(&mut a, 0..90);
        a.end(100);
        a.finish();
        let r = a.report();
        assert_eq!((r.expected, r.lost, r.longest_burst), (100, 10, 10));
        let mut b = Analyzer::new(0, 1, 1000, 100.0, &[]);
        feed(&mut b, 0..90);
        b.finish();
        assert_eq!(b.report().lost, 0);
        assert!(!b.report().end_marker);
    }

    #[test]
    fn late_joiner_counts_from_first_packet() {
        let mut a = Analyzer::new(0, 1, 1000, 100.0, &[]);
        feed(&mut a, 5000..5100);
        a.finish();
        let r = a.report();
        assert_eq!((r.first_seq, r.expected, r.lost), (5000, 100, 0));
    }

    #[test]
    fn fec_simulation() {
        // 1 ms packets, T = 10 ms -> n = 10; r = 1 -> k = 5.
        let spec = FecSpec {
            span_ms: 10.0,
            ratio: 1.0,
        };
        let mut a = Analyzer::new(0, 1, 1000, 100.0, &[spec]);
        // Group 0 (0..10): lose 4 -> recovered, but with 6 < k + 2 it is at risk.
        // Group 1 (10..20): lose 6 -> fails, 6 x 5/10 = 3 source packets lost.
        // Group 2 (20..30): clean.
        let lost: Vec<u64> = vec![0, 1, 2, 3, 10, 11, 12, 13, 14, 15];
        feed(&mut a, (0..30).filter(|s| !lost.contains(s)));
        a.end(30);
        a.finish();
        let f = &a.report().fec[0];
        assert_eq!((f.n, f.k, f.blocks, f.failed, f.at_risk), (10, 5, 3, 1, 1));
        assert!((f.unrecovered_pct - 100.0 * 3.0 / 15.0).abs() < 1e-9);
    }

    #[test]
    fn loopback_two_streams() {
        let base = 30_000 + (std::process::id() % 20_000) as u16;
        let rx = RecvOptions {
            listen: SocketAddr::from(([127, 0, 0, 1], base)),
            streams: 2,
            idle: Duration::from_secs(5),
            duration: Some(Duration::from_secs(30)),
            report_every: Duration::ZERO,
            reorder_ms: 200.0,
            fec: FecSpec::defaults(),
            rcvbuf: 4 << 20,
            keep: false,
            quiet: true,
        };
        let dir = crate::testutil::TempDir::new();
        let log = dir.path().join("probe.csv");
        let log2 = log.clone();
        let receiver = std::thread::spawn(move || run_recv(&rx, Some(&log2)));
        std::thread::sleep(Duration::from_millis(300));
        let tx = SendOptions {
            to: SocketAddr::from(([127, 0, 0, 1], base)),
            bind: None,
            streams: 2,
            pps: 5000.0,
            size: 300,
            duration: None,
            count: Some(1500),
            burst: 1,
            sndbuf: None,
        };
        let sent = run_send(&tx).unwrap();
        assert_eq!(sent.len(), 2);
        assert!(sent.iter().all(|s| s.sent == 1500 && s.errors == 0));
        let got = receiver.join().unwrap().unwrap();
        assert_eq!(got.reports.len(), 2, "one report per stream");
        for r in &got.reports {
            assert!(r.end_marker);
            assert_eq!(
                (r.expected, r.received, r.lost),
                (1500, 1500, 0),
                "stream {}",
                r.stream
            );
            assert_eq!(r.packet_size, 300);
            assert_eq!(r.fec.len(), 4);
        }
        // The log replays to the same numbers.
        let again = analyze_log(&log, 200.0, &[]).unwrap();
        assert_eq!(again.len(), 2);
        assert!(
            again
                .iter()
                .all(|r| r.expected == 1500 && r.lost == 0 && r.end_marker)
        );
        let j = reports_json("recv", &got.reports, Some(&got.ports));
        assert_eq!(j["reports"].as_array().unwrap().len(), 2);
    }
}
