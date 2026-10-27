//! End to end through real, embedded lidi engines over loopback UDP:
//! a folder transfer, a Modbus update, and a forged sender.

use dyode::config::{self, ModuleKind};
use dyode::envelope::{Body, Failure, Header, Inbound};
use dyode::testutil::TempDir;
use dyode::transport::{self, Lifecycle, ModuleLink, Uplink};
use dyode::{folder, modbus};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

#[derive(Default)]
struct Refusals(Mutex<Vec<String>>);

/// Wraps a mirror to also record refusals.
struct Recording(modbus::MirrorInbound, Arc<Refusals>);

impl Inbound for Recording {
    fn module_name(&self) -> &str {
        self.0.module_name()
    }
    fn open_body(&self, hdr: &Header) -> Result<Body, String> {
        self.0.open_body(hdr)
    }
    fn deliver(&self, hdr: Header, body: Body, digest: [u8; 32]) {
        self.0.deliver(hdr, body, digest);
    }
    fn failed(&self, hdr: Option<&Header>, body: Option<Body>, why: Failure) {
        self.1.0.lock().unwrap().push(why.to_string());
        self.0.failed(hdr, body, why);
    }
}

#[test]
fn folder_and_modbus_through_lidi() {
    let d = TempDir::new();
    let port = 20000 + (dyode::auth::random_u32() % 20000) as u16;
    let mb_port = port + 1;
    let yaml = format!(
        "dyode_in: {{ip: 127.0.0.1, interface: lo}}\n\
         dyode_out: {{ip: 127.0.0.1, interface: lo}}\n\
         lidi: {{ports: [{port}], heartbeat: 1}}\n\
         logging: {{dir: ''}}\n\
         modules:\n  files:\n    type: folder\n    in: {d}/in\n    out: {d}/out\n\
         \x20 plc:\n    type: modbus\n    ip: 127.0.0.1\n    port_out: {mb_port}\n    bind_out: 127.0.0.1\n    registers: [0-4]\n",
        d = d.path().display()
    );
    let cfg = config::parse(&yaml, Path::new("config.yaml"), d.path()).unwrap();
    let key = Some(vec![42u8; 32]);
    let (ModuleKind::Folder(fc), ModuleKind::Modbus(mc)) =
        (&cfg.modules[0].kind, &cfg.modules[1].kind)
    else {
        panic!()
    };

    // Output side.
    let fin = Arc::new(folder::FolderInbound::new(&cfg.modules[0], fc).unwrap());
    let mirror = Arc::new(modbus::Mirror::new("plc", mc));
    let refusals = Arc::new(Refusals::default());
    let mut lc = Lifecycle::new();
    lc.register(&cfg.modules[0], key.clone(), fin.clone());
    lc.register(
        &cfg.modules[1],
        key.clone(),
        Arc::new(Recording(
            modbus::MirrorInbound::new(Arc::clone(&mirror)),
            Arc::clone(&refusals),
        )),
    );
    let receiver: &'static transport::LidiReceiver =
        Box::leak(Box::new(transport::new_receiver(&cfg, lc).unwrap()));
    std::thread::spawn(move || {
        std::thread::scope(|s| {
            if let Err(e) = receiver.start(s) {
                panic!("lidi receiver: {e}");
            }
            loop {
                std::thread::park();
            }
        })
    });

    // Input side.
    let sender: &'static transport::LidiSender =
        Box::leak(Box::new(transport::new_sender(&cfg).unwrap()));
    std::thread::spawn(move || {
        std::thread::scope(|s| {
            if let Err(e) = sender.start(s) {
                panic!("lidi sender: {e}");
            }
            loop {
                std::thread::park();
            }
        })
    });
    std::thread::sleep(Duration::from_millis(300));
    let uplink = Arc::new(Uplink::new(sender, &cfg));
    let flink = ModuleLink::new(Arc::clone(&uplink), &cfg.modules[0], key.clone(), None);
    let mlink = ModuleLink::new(Arc::clone(&uplink), &cfg.modules[1], key.clone(), None);

    // A file, including a sub-folder, larger than one RaptorQ block.
    std::fs::create_dir_all(fc.input.join("sub")).unwrap();
    let data: Vec<u8> = (0..1_000_000u32).map(|i| (i * 7 % 256) as u8).collect();
    std::fs::write(fc.input.join("sub/data.bin"), &data).unwrap();
    std::fs::write(fc.input.join("small.txt"), b"hello").unwrap();
    let (ready, _) = dyode::fsafe::scan_ready_files(&fc.input, 0.0);
    assert_eq!(folder::send_batch("files", fc, &flink, &ready), 2);
    let out = fc.output.join("sub/data.bin");
    wait_for("the file", 20, || {
        std::fs::read(&out).map(|b| b == data).unwrap_or(false)
    });
    assert_eq!(
        std::fs::read(fc.output.join("small.txt")).unwrap(),
        b"hello"
    );
    assert!(
        fc.input.join(".dyode_sent/sub/data.bin").exists(),
        "retired, not deleted"
    );

    // A Modbus update.
    mlink
        .send_message(b"", br#"{"seq":1,"registers":{"0":[11,22,33,44]}}"#)
        .unwrap();
    wait_for("the Modbus update", 10, || {
        mirror.handle_pdu(&[3, 0, 0, 0, 4]) == vec![3, 8, 0, 11, 0, 22, 0, 33, 0, 44]
    });

    // A forged sender (other key) is refused, and the values stay.
    let forged = ModuleLink::new(
        Arc::clone(&uplink),
        &cfg.modules[1],
        Some(vec![1u8; 32]),
        None,
    );
    forged
        .send_message(b"", br#"{"seq":2,"registers":{"0":[666,666,666,666]}}"#)
        .unwrap();
    wait_for("the refusal", 10, || {
        refusals
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.contains("not signed"))
    });
    assert_eq!(mirror.handle_pdu(&[3, 0, 0, 0, 1]), vec![3, 2, 0, 11]);
}

/// parallel_files: several files of one batch in flight at once, over two
/// lidi ports, all arriving intact and all retired.
#[test]
fn folder_parallel_files_through_lidi() {
    let d = TempDir::new();
    let port = 20000 + (dyode::auth::random_u32() % 20000) as u16;
    let yaml = format!(
        "dyode_in: {{ip: 127.0.0.1, interface: lo}}\n\
         dyode_out: {{ip: 127.0.0.1, interface: lo}}\n\
         lidi: {{ports: [{port}, {p2}], heartbeat: 1}}\n\
         logging: {{dir: ''}}\n\
         modules:\n  files:\n    type: folder\n    in: {d}/in\n    out: {d}/out\n    parallel_files: 4\n",
        p2 = port + 1,
        d = d.path().display()
    );
    let cfg = config::parse(&yaml, Path::new("config.yaml"), d.path()).unwrap();
    assert_eq!(cfg.max_clients(), 6, "4 files + summary + heartbeat");
    let key = Some(vec![7u8; 32]);
    let ModuleKind::Folder(fc) = &cfg.modules[0].kind else {
        panic!()
    };
    let fin = Arc::new(folder::FolderInbound::new(&cfg.modules[0], fc).unwrap());
    let mut lc = Lifecycle::new();
    lc.register(&cfg.modules[0], key.clone(), fin);
    let receiver: &'static transport::LidiReceiver =
        Box::leak(Box::new(transport::new_receiver(&cfg, lc).unwrap()));
    std::thread::spawn(move || {
        std::thread::scope(|s| {
            if let Err(e) = receiver.start(s) {
                panic!("lidi receiver: {e}");
            }
            loop {
                std::thread::park();
            }
        })
    });
    let sender: &'static transport::LidiSender =
        Box::leak(Box::new(transport::new_sender(&cfg).unwrap()));
    std::thread::spawn(move || {
        std::thread::scope(|s| {
            if let Err(e) = sender.start(s) {
                panic!("lidi sender: {e}");
            }
            loop {
                std::thread::park();
            }
        })
    });
    std::thread::sleep(Duration::from_millis(300));
    let uplink = Arc::new(Uplink::new(sender, &cfg));
    let flink = ModuleLink::new(uplink, &cfg.modules[0], key, None);

    std::fs::create_dir_all(&fc.input).unwrap();
    let mut want = Vec::new();
    for n in 0..9u32 {
        let data: Vec<u8> = (0..300_000u32)
            .map(|i| ((i * 13 + n * 31) % 251) as u8)
            .collect();
        std::fs::write(fc.input.join(format!("f{n}.bin")), &data).unwrap();
        want.push((format!("f{n}.bin"), data));
    }
    let (ready, _) = dyode::fsafe::scan_ready_files(&fc.input, 0.0);
    assert_eq!(folder::send_batch("files", fc, &flink, &ready), 9);
    for (name, data) in &want {
        let out = fc.output.join(name);
        wait_for(name, 20, || {
            std::fs::read(&out).map(|b| &b == data).unwrap_or(false)
        });
        assert!(fc.input.join(".dyode_sent").join(name).exists());
    }
}
