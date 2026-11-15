//! The air transport end to end over loopback UDP: real sender carousel,
//! real receiver, real module handlers, with a relay in between that can
//! lose, fade, record and replay packets.

use dyode::air::{AirReceiver, AirSender};
use dyode::config::{self, Config, ModuleKind};
use dyode::envelope::{Body, Failure, Header, Inbound};
use dyode::testutil::TempDir;
use dyode::transport::{self, Lifecycle, ModuleLink, Uplink};
use dyode::{folder, modbus};
use std::net::{SocketAddr, UdpSocket};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const KEY: [u8; 32] = [42u8; 32];

fn wait_for(what: &str, secs: u64, mut ok: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < until {
        if ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for {what}");
}

fn free_port() -> u16 {
    UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// What happens to packets between the boxes.
#[derive(Clone, Copy)]
struct Channel {
    /// Gilbert-Elliott: chance to enter the bad state, chance to leave it.
    p: f64,
    r: f64,
    /// A fade of `fade_ms` every `fade_every_ms` (0 = none).
    fade_ms: u64,
    fade_every_ms: u64,
}

const CLEAN: Channel = Channel {
    p: 0.0,
    r: 1.0,
    fade_ms: 0,
    fade_every_ms: 0,
};

struct Relay {
    to: Mutex<SocketAddr>,
    record: AtomicBool,
    recorded: Mutex<Vec<Vec<u8>>>,
    passed: AtomicU64,
    dropped: AtomicU64,
    channel: Mutex<Channel>,
}

impl Relay {
    /// Listens on a free port; returns its address.
    fn start(to: SocketAddr, channel: Channel) -> (Arc<Self>, SocketAddr) {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = sock.local_addr().unwrap();
        let out = UdpSocket::bind("127.0.0.1:0").unwrap();
        let me = Arc::new(Self {
            to: Mutex::new(to),
            record: AtomicBool::new(false),
            recorded: Mutex::new(Vec::new()),
            passed: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            channel: Mutex::new(channel),
        });
        let r = Arc::clone(&me);
        std::thread::spawn(move || {
            let started = Instant::now();
            let mut bad = false;
            let mut seed = 0x9e37_79b9_7f4a_7c15u64;
            let mut rnd = move || {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                (seed >> 11) as f64 / (1u64 << 53) as f64
            };
            let mut buf = vec![0u8; 65536];
            loop {
                let Ok(n) = sock.recv(&mut buf) else { continue };
                let ch = *r.channel.lock().unwrap();
                bad = if bad { rnd() >= ch.r } else { rnd() < ch.p };
                let t = started.elapsed().as_millis() as u64;
                let fading = ch.fade_every_ms > 0 && t % ch.fade_every_ms < ch.fade_ms;
                if r.record.load(Ordering::Relaxed) {
                    r.recorded.lock().unwrap().push(buf[..n].to_vec());
                }
                if bad || fading {
                    r.dropped.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                r.passed.fetch_add(1, Ordering::Relaxed);
                let to = *r.to.lock().unwrap();
                let _ = out.send_to(&buf[..n], to);
            }
        });
        (me, addr)
    }

    fn replay(&self, to: SocketAddr) {
        let out = UdpSocket::bind("127.0.0.1:0").unwrap();
        for p in self.recorded.lock().unwrap().iter() {
            let _ = out.send_to(p, to);
            std::thread::sleep(Duration::from_micros(20));
        }
    }
}

#[derive(Default)]
struct Refusals(Mutex<Vec<String>>);

/// A Modbus mirror that also records refusals and deliveries.
struct Recording(modbus::MirrorInbound, Arc<Refusals>, Arc<AtomicU64>);

impl Inbound for Recording {
    fn module_name(&self) -> &str {
        self.0.module_name()
    }
    fn open_body(&self, hdr: &Header) -> Result<Body, String> {
        self.0.open_body(hdr)
    }
    fn deliver(&self, hdr: Header, body: Body, digest: [u8; 32]) {
        self.2.fetch_add(1, Ordering::Relaxed);
        self.0.deliver(hdr, body, digest);
    }
    fn failed(&self, hdr: Option<&Header>, body: Option<Body>, why: Failure) {
        self.1.0.lock().unwrap().push(why.to_string());
        self.0.failed(hdr, body, why);
    }
}

fn config(d: &Path, to: SocketAddr, listen: SocketAddr, extra: &str) -> Config {
    let mb_port = free_port();
    // defaults for the tests, unless `extra` sets them
    let mut extra = extra.to_string();
    for (k, v) in [("max_mbps", "100"), ("pass_gap", "0.2")] {
        if !extra.contains(&format!("  {k}:")) {
            extra.push_str(&format!("  {k}: {v}\n"));
        }
    }
    let yaml = format!(
        "dyode_in: {{ip: 127.0.0.1, interface: lo}}\n\
         dyode_out: {{ip: 127.0.0.1, interface: lo}}\n\
         transport: air\n\
         air:\n  to: {to}\n  listen: {listen}\n  spool: {d}/spool\n  state: {d}/state\n{extra}\
         lidi: {{heartbeat: 1}}\n\
         logging: {{dir: ''}}\n\
         modules:\n  files:\n    type: folder\n    in: {d}/in\n    out: {d}/out\n\
         \x20 plc:\n    type: modbus\n    ip: 127.0.0.1\n    port_out: {mb_port}\n    bind_out: 127.0.0.1\n    registers: [0-4]\n",
        d = d.display()
    );
    config::parse(&yaml, Path::new("config.yaml"), d).unwrap()
}

struct Out {
    receiver: Arc<AirReceiver>,
    mirror: Arc<modbus::Mirror>,
    refusals: Arc<Refusals>,
    modbus_delivered: Arc<AtomicU64>,
}

fn start_output(cfg: &Config, key: &[u8]) -> Out {
    let ModuleKind::Folder(fc) = &cfg.modules[0].kind else {
        panic!()
    };
    let ModuleKind::Modbus(mc) = &cfg.modules[1].kind else {
        panic!()
    };
    let fin = Arc::new(folder::FolderInbound::new(&cfg.modules[0], fc).unwrap());
    let mirror = Arc::new(modbus::Mirror::new("plc", mc));
    let refusals = Arc::new(Refusals::default());
    let delivered = Arc::new(AtomicU64::new(0));
    let k = Some(key.to_vec());
    let mut lc = Lifecycle::new();
    lc.register(&cfg.modules[0], k.clone(), fin);
    lc.register(
        &cfg.modules[1],
        k.clone(),
        Arc::new(Recording(
            modbus::MirrorInbound::new(Arc::clone(&mirror)),
            Arc::clone(&refusals),
            Arc::clone(&delivered),
        )),
    );
    lc.register_endpoint(
        transport::status_endpoint(cfg),
        transport::status_tag(),
        k,
        Arc::new(dyode::status::StatusInbound::new()),
    );
    let receiver = AirReceiver::new(cfg, key, &lc).unwrap();
    let r = Arc::clone(&receiver);
    std::thread::spawn(move || r.run().unwrap());
    Out {
        receiver,
        mirror,
        refusals,
        modbus_delivered: delivered,
    }
}

fn start_input(cfg: &Config, key: &[u8]) -> (Arc<AirSender>, Arc<Uplink>) {
    let endpoints: Vec<(u16, [u8; 8])> = cfg
        .modules
        .iter()
        .map(|m| (m.index, m.tag()))
        .chain(std::iter::once((
            transport::status_endpoint(cfg),
            transport::status_tag(),
        )))
        .collect();
    let sender = Arc::new(AirSender::new(cfg, key, &endpoints).unwrap());
    let s = Arc::clone(&sender);
    std::thread::spawn(move || s.run().unwrap());
    let uplink = Arc::new(Uplink::air(Arc::clone(&sender)));
    (sender, uplink)
}

/// A box-wide counter from the stats registry (shared by the tests of
/// this file, so only ever checked for "more than zero").
fn counter(name: &str) -> u64 {
    dyode::stats::snapshot()["counters"][name]
        .as_u64()
        .unwrap_or(0)
}

fn pattern(len: usize, salt: u32) -> Vec<u8> {
    (0..len as u32)
        .map(|i| ((i.wrapping_mul(2_654_435_761) ^ salt) >> 13) as u8)
        .collect()
}

/// Files (one multi-chunk, one tiny) and Modbus values over a clean link;
/// another key's packets are ignored.
#[test]
fn folder_and_modbus_over_air() {
    let d = TempDir::new();
    let listen: SocketAddr = format!("127.0.0.1:{}", free_port()).parse().unwrap();
    let cfg = config(d.path(), listen, listen, "  chunk: 262144\n");
    let out = start_output(&cfg, &KEY);
    let (_s, uplink) = start_input(&cfg, &KEY);
    let ModuleKind::Folder(fc) = &cfg.modules[0].kind else {
        panic!()
    };
    let flink = ModuleLink::new(
        Arc::clone(&uplink),
        &cfg.modules[0],
        Some(KEY.to_vec()),
        None,
    );
    let mlink = ModuleLink::new(
        Arc::clone(&uplink),
        &cfg.modules[1],
        Some(KEY.to_vec()),
        None,
    );

    std::fs::create_dir_all(fc.input.join("sub")).unwrap();
    let data = pattern(1_500_000, 1); // 6 chunks of 256 KiB
    std::fs::write(fc.input.join("sub/data.bin"), &data).unwrap();
    std::fs::write(fc.input.join("small.txt"), b"hello air").unwrap();
    let (ready, _) = dyode::fsafe::scan_ready_files(&fc.input, 0.0);
    assert_eq!(folder::send_batch("files", fc, &flink, &ready), 2);
    wait_for("the big file", 30, || {
        std::fs::read(fc.output.join("sub/data.bin")).is_ok_and(|b| b == data)
    });
    wait_for("the small file", 10, || {
        std::fs::read(fc.output.join("small.txt")).is_ok_and(|b| b == b"hello air")
    });

    mlink
        .send_message(b"", br#"{"seq":1,"registers":{"0":[11,22,33,44]}}"#)
        .unwrap();
    wait_for("the Modbus update", 10, || {
        out.mirror.handle_pdu(&[3, 0, 0, 0, 4]) == vec![3, 8, 0, 11, 0, 22, 0, 33, 0, 44]
    });

    // Another link's sender (other key) on the same channel: dropped by
    // key id, before any tag check or decode.
    let d2 = TempDir::new();
    let cfg2 = config(
        d2.path(),
        listen,
        "127.0.0.1:0".parse().unwrap(),
        "  chunk: 262144\n",
    );
    let (_s2, up2) = start_input(&cfg2, &[1u8; 32]);
    let forged = ModuleLink::new(up2, &cfg2.modules[1], Some(vec![1u8; 32]), None);
    forged
        .send_message(b"", br#"{"seq":2,"registers":{"0":[666,666,666,666]}}"#)
        .unwrap();
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(out.mirror.handle_pdu(&[3, 0, 0, 0, 1]), vec![3, 2, 0, 11]);
    assert!(out.refusals.0.lock().unwrap().is_empty());
}

/// A bursty channel with fades: 2% chance of entering a bad state that
/// lasts 4 packets on average, plus a 150 ms fade every 1.5 s.  Three
/// passes, 40% repair.  Every file arrives intact.
#[test]
fn files_survive_bursts_and_fades() {
    let d = TempDir::new();
    let listen: SocketAddr = format!("127.0.0.1:{}", free_port()).parse().unwrap();
    let ch = Channel {
        p: 0.02,
        r: 0.25,
        fade_ms: 150,
        fade_every_ms: 1500,
    };
    let (relay, relay_addr) = Relay::start(listen, ch);
    let cfg = config(
        d.path(),
        relay_addr,
        listen,
        "  chunk: 131072\n  symbol: 1024\n  repair: 40\n  passes: 3\n  max_mbps: 120\n  pass_gap: 0.5\n",
    );
    let _out = start_output(&cfg, &KEY);
    let (_s, uplink) = start_input(&cfg, &KEY);
    let ModuleKind::Folder(fc) = &cfg.modules[0].kind else {
        panic!()
    };
    let flink = ModuleLink::new(uplink, &cfg.modules[0], Some(KEY.to_vec()), None);
    std::fs::create_dir_all(&fc.input).unwrap();
    let mut want = Vec::new();
    for (n, len) in [
        (0u32, 2_000_000usize),
        (1, 300_000),
        (2, 5_000),
        (3, 1_200_000),
    ] {
        let data = pattern(len, n + 10);
        std::fs::write(fc.input.join(format!("f{n}.bin")), &data).unwrap();
        want.push((format!("f{n}.bin"), data));
    }
    let (ready, _) = dyode::fsafe::scan_ready_files(&fc.input, 0.0);
    assert_eq!(folder::send_batch("files", fc, &flink, &ready), 4);
    for (name, data) in &want {
        wait_for(name, 60, || {
            std::fs::read(fc.output.join(name)).is_ok_and(|b| &b == data)
        });
    }
    let (p, l) = (
        relay.passed.load(Ordering::Relaxed),
        relay.dropped.load(Ordering::Relaxed),
    );
    assert!(
        l * 100 / (p + l) >= 5,
        "the channel really lost packets: {l} of {}",
        p + l
    );
}

/// Hostile and unlucky cases on one pair of boxes:
/// - a receiver restart resumes a partial object from its .part file;
/// - recorded traffic replayed after a restart delivers nothing twice;
/// - a key holder's poisoned symbols cost time, not integrity;
/// - after the sender restarts, its old boot's traffic is refused.
#[test]
fn restart_resume_and_replays() {
    let d = TempDir::new();
    let listen: SocketAddr = format!("127.0.0.1:{}", free_port()).parse().unwrap();
    // Start with nothing getting through.
    let blocked = Channel {
        p: 1.0,
        r: 0.0,
        fade_ms: 0,
        fade_every_ms: 0,
    };
    let (relay, relay_addr) = Relay::start(listen, blocked);
    // One chunk per window and a slow link: chunks complete one by one,
    // so the receiver can be stopped with the object half collected.
    let extra = "  chunk: 65536\n  symbol: 512\n  interleave: 1\n  passes: 4\n  pass_gap: 1.0\n  max_mbps: 2\n";
    let cfg = config(d.path(), relay_addr, listen, extra);
    let out = start_output(&cfg, &KEY);
    let (_s, uplink) = start_input(&cfg, &KEY);
    let ModuleKind::Folder(fc) = &cfg.modules[0].kind else {
        panic!()
    };
    let flink = ModuleLink::new(
        Arc::clone(&uplink),
        &cfg.modules[0],
        Some(KEY.to_vec()),
        None,
    );
    std::fs::create_dir_all(&fc.input).unwrap();
    let data = pattern(1_000_000, 77); // 16 chunks
    std::fs::write(fc.input.join("big.bin"), &data).unwrap();
    relay.record.store(true, Ordering::Relaxed);
    *relay.channel.lock().unwrap() = CLEAN;
    let (ready, _) = dyode::fsafe::scan_ready_files(&fc.input, 0.0);
    assert_eq!(folder::send_batch("files", fc, &flink, &ready), 1);
    wait_for("a partial object", 20, || {
        out.receiver.collecting().iter().any(|c| c.1 >= 3)
    });
    // Block the link and "crash" the receiver mid-object.
    *relay.channel.lock().unwrap() = blocked;
    out.receiver.stop();
    std::thread::sleep(Duration::from_millis(800));
    let held = out.receiver.collecting();
    drop(out);
    std::thread::sleep(Duration::from_millis(300));
    assert!(!fc.output.join("big.bin").exists());

    // Restart: the partial object comes back from disk.
    let out = start_output(&cfg, &KEY);
    let resumed = out.receiver.collecting();
    assert_eq!(resumed.len(), 1, "resumed {resumed:?}, held {held:?}");
    assert!(resumed[0].1 >= 3);
    // A key holder injects junk symbols for every chunk: each poisoned
    // chunk fails its seal and is collected again.
    let poison_stop = Arc::new(AtomicBool::new(false));
    {
        let stop = Arc::clone(&poison_stop);
        let recorded: Vec<Vec<u8>> = relay.recorded.lock().unwrap().clone();
        std::thread::spawn(move || {
            let keys = dyode::air::keys::Keys::derive(&KEY);
            let out = UdpSocket::bind("127.0.0.1:0").unwrap();
            let mut pkt = Vec::new();
            for p in recorded.iter().step_by(7) {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let (mut h, sym) = dyode::air::packet::decode(&keys, p, 512).unwrap();
                h.payload_id[3] ^= 0x55; // a different symbol id...
                let junk: Vec<u8> = sym.iter().map(|b| b ^ 0xa5).collect(); // ...with junk in it
                dyode::air::packet::encode(&keys, &h, &junk, &mut pkt);
                let _ = out.send_to(&pkt, listen);
                std::thread::sleep(Duration::from_micros(200));
            }
        });
    }
    *relay.channel.lock().unwrap() = Channel {
        p: 0.05,
        r: 0.5,
        ..CLEAN
    };
    wait_for("the file after the restart", 60, || {
        std::fs::read(fc.output.join("big.bin")).is_ok_and(|b| b == data)
    });
    poison_stop.store(true, Ordering::Relaxed);
    relay.record.store(false, Ordering::Relaxed);
    let bad_seals = counter("air_chunks_bad_seal");
    assert!(
        bad_seals > 0,
        "the poisoned symbols were never decoded into a chunk"
    );

    // Replay everything recorded into a restarted receiver: nothing new.
    *relay.channel.lock().unwrap() = blocked;
    std::fs::remove_file(fc.output.join("big.bin")).unwrap();
    out.receiver.stop();
    std::thread::sleep(Duration::from_millis(800));
    drop(out);
    std::thread::sleep(Duration::from_millis(300));
    let out = start_output(&cfg, &KEY);
    relay.replay(listen);
    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        !fc.output.join("big.bin").exists(),
        "a replay after a restart delivered the file again"
    );
    assert!(out.receiver.collecting().is_empty());

    // A new sender boot (the input box restarted) is accepted, and then
    // the old boot's recording is refused even for an object id it never
    // delivered.
    let (_s2, up2) = start_input(&cfg, &KEY);
    *relay.channel.lock().unwrap() = CLEAN;
    let mlink = ModuleLink::new(up2, &cfg.modules[1], Some(KEY.to_vec()), None);
    mlink
        .send_message(b"", br#"{"seq":1,"registers":{"0":[1,2,3,4]}}"#)
        .unwrap();
    wait_for("the new boot's Modbus update", 10, || {
        out.mirror.handle_pdu(&[3, 0, 0, 0, 1]) == vec![3, 2, 0, 1]
    });
    let before = out.modbus_delivered.load(Ordering::Relaxed);
    *relay.channel.lock().unwrap() = blocked;
    relay.replay(listen);
    std::thread::sleep(Duration::from_millis(1500));
    assert!(out.receiver.collecting().is_empty(), "old boot collected");
    assert_eq!(out.modbus_delivered.load(Ordering::Relaxed), before);
    assert!(!fc.output.join("big.bin").exists());
    assert!(out.refusals.0.lock().unwrap().is_empty());
}
