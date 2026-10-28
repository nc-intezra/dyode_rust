//! The status channel and the API end to end: the input box's status goes
//! through a real embedded lidi engine over loopback, and the output box's
//! API serves it over TLS to a token holder.
#![cfg(feature = "api")]

use dyode::config::{self, Side};
use dyode::testutil::TempDir;
use dyode::transport::{self, Lifecycle, ModuleLink, Uplink};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::Arc;
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

/// A self-signed certificate for 127.0.0.1, or None without openssl.
fn make_cert(dir: &Path) -> Option<()> {
    let ok = std::process::Command::new("openssl")
        .args([
            "req",
            "-x509",
            "-newkey",
            "ec",
            "-pkeyopt",
            "ec_paramgen_curve:P-256",
            "-nodes",
        ])
        .arg("-keyout")
        .arg(dir.join("api.key"))
        .arg("-out")
        .arg(dir.join("api.crt"))
        .args([
            "-days",
            "2",
            "-subj",
            "/CN=test",
            "-addext",
            "subjectAltName=IP:127.0.0.1",
        ])
        .args([
            "-addext",
            "basicConstraints=critical,CA:FALSE",
            "-addext",
            "keyUsage=critical,digitalSignature",
            "-addext",
            "extendedKeyUsage=serverAuth",
        ])
        .output()
        .ok()?
        .status
        .success();
    ok.then_some(())
}

fn get(port: u16, cert: &Path, path: &str, token: Option<&str>) -> (u16, String) {
    let mut roots = rustls::RootCertStore::empty();
    for c in CertificateDer::pem_file_iter(cert).unwrap() {
        roots.add(c.unwrap()).unwrap();
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let conn =
        rustls::ClientConnection::new(Arc::new(cfg), ServerName::try_from("127.0.0.1").unwrap())
            .unwrap();
    let sock = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let mut tls = rustls::StreamOwned::new(conn, sock);
    let auth = token.map_or(String::new(), |t| format!("Authorization: Bearer {t}\r\n"));
    write!(tls, "GET {path} HTTP/1.1\r\nHost: x\r\n{auth}\r\n").unwrap();
    let mut out = Vec::new();
    let _ = tls.read_to_end(&mut out);
    let text = String::from_utf8_lossy(&out).to_string();
    let code = text.get(9..12).and_then(|c| c.parse().ok()).unwrap_or(0);
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    (code, body)
}

#[test]
fn input_status_reaches_the_output_api() {
    let d = TempDir::new();
    if make_cert(d.path()).is_none() {
        eprintln!("openssl not available: skipped");
        return;
    }
    let port = 20000 + (dyode::auth::random_u32() % 20000) as u16;
    let api_port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let yaml = format!(
        "dyode_in: {{ip: 127.0.0.2, interface: lo}}\n\
         dyode_out: {{ip: 127.0.0.3, interface: lo}}\n\
         lidi: {{ports: [{port}], heartbeat: 1}}\n\
         logging: {{dir: ''}}\n\
         api:\n  out: {{listen: '127.0.0.1:{api_port}', tls: {{cert: api.crt, key: api.key}}}}\n\
         modules:\n  files: {{type: folder, in: {d}/in, out: {d}/out}}\n",
        d = d.path().display()
    );
    let mut cfg = config::parse(&yaml, &d.path().join("config.yaml"), d.path()).unwrap();
    // lidi over loopback: both ends on 127.0.0.1.
    cfg.network.in_ip = "127.0.0.1".parse().unwrap();
    cfg.network.out_ip = "127.0.0.1".parse().unwrap();
    let api = cfg.api.output.clone().expect("api.out");
    assert_eq!(
        cfg.max_clients(),
        4,
        "1 module + summary + heartbeat + status"
    );
    let key = Some(vec![9u8; 32]);
    dyode::stats::init(&cfg, "out");
    dyode::stats::module_started("files");

    // Output side: the folder module and the status channel.
    let config::ModuleKind::Folder(fc) = &cfg.modules[0].kind else {
        panic!()
    };
    let mut lc = Lifecycle::new();
    lc.register(
        &cfg.modules[0],
        key.clone(),
        Arc::new(dyode::folder::FolderInbound::new(&cfg.modules[0], fc).unwrap()),
    );
    lc.register_endpoint(
        transport::status_endpoint(&cfg),
        transport::status_tag(),
        key.clone(),
        Arc::new(dyode::status::StatusInbound::new()),
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

    // The input box's status, as the reporter would send it.
    let uplink = Arc::new(Uplink::new(sender, &cfg));
    let link = ModuleLink::for_endpoint(
        uplink,
        transport::status_endpoint(&cfg),
        transport::status_tag(),
        key.clone(),
        None,
    );
    let mut st = dyode::stats::snapshot();
    st["side"] = "in".into();
    st["static_arp_ok"] = true.into();
    link.send_message(br#"{"kind":"status"}"#, &serde_json::to_vec(&st).unwrap())
        .unwrap();
    wait_for("the status message", 10, || dyode::stats::peer().is_some());

    // A forged status (other key) is refused and changes nothing.
    let before = dyode::stats::peer().unwrap().received;
    let uplink2 = Arc::new(Uplink::new(sender, &cfg));
    let forged = ModuleLink::for_endpoint(
        uplink2,
        transport::status_endpoint(&cfg),
        transport::status_tag(),
        Some(vec![1u8; 32]),
        None,
    );
    forged
        .send_message(b"", br#"{"side":"in","version":"666"}"#)
        .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(dyode::stats::peer().unwrap().received, before);

    // The API, over TLS.
    let token = dyode::apitoken::add(&api.tokens, "prometheus", &["read".into()]).unwrap();
    let (c, k) = api.tls.clone().unwrap();
    let tls = dyode::api::tls_config(&c, &k).unwrap();
    let listener = TcpListener::bind(api.listen).unwrap();
    let cfg = Arc::new(cfg);
    let (cfg2, api2) = (Arc::clone(&cfg), api.clone());
    std::thread::spawn(move || {
        dyode::api::serve_listener(listener, Some(tls), cfg2, Side::Out, api2)
    });
    std::thread::sleep(Duration::from_millis(100));
    let cert = d.path().join("api.crt");

    let (code, body) = get(api_port, &cert, "/v1/health", None);
    assert_eq!(code, 200, "{body}");
    assert!(body.contains("\"side\": \"out\""), "{body}");

    let (code, _) = get(api_port, &cert, "/v1/status", None);
    assert_eq!(code, 401);
    let (code, _) = get(api_port, &cert, "/v1/status", Some("dyode_wrong"));
    assert_eq!(code, 401);

    let (code, body) = get(api_port, &cert, "/v1/status", Some(&token));
    assert_eq!(code, 200, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["boxes"]["in"]["side"], "in");
    assert_eq!(v["boxes"]["out"]["side"], "out");
    assert!(v["boxes"]["in"]["status_age_s"].as_f64().unwrap() < 10.0);
    assert_eq!(v["warnings"].as_array().unwrap().len(), 0, "{body}");

    let (code, body) = get(api_port, &cert, "/metrics", Some(&token));
    assert_eq!(code, 200);
    for want in [
        "dyode_status_age_seconds{side=\"in\"}",
        "dyode_config_match 1",
        "dyode_module_up{side=\"out\",module=\"files\",type=\"folder\"} 1",
        "dyode_static_arp_ok 1",
        "dyode_counter_total{side=\"out\",counter=\"api_refused\"} 2",
    ] {
        assert!(body.contains(want), "missing {want} in\n{body}");
    }

    let (code, body) = get(api_port, &cert, "/v1/check", Some(&token));
    assert_eq!(code, 200, "{body}");
    assert!(body.contains("\"notes\""), "{body}");

    let (code, body) = get(api_port, &cert, "/v1/openapi.json", None);
    assert_eq!(code, 200);
    assert!(body.contains("\"openapi\": \"3.1.0\""));

    let (code, _) = get(api_port, &cert, "/v1/nothing", Some(&token));
    assert_eq!(code, 404);
}
