//! The `dyode` command.

use dyode::config::{self, Side};
use dyode::{auth, logging, logs_tool, probe, runtime, setup, timefmt, transport};
use std::path::{Path, PathBuf};
use std::process::exit;

const USAGE: &str = "\
DYODE -- a DIY data diode, with lidi (ANSSI) as its transport

Usage: dyode <command> [options]

Running (both boxes read the same config.yaml):
  in            input side (DYODE v1: sends through lidi)
  out           output side (DYODE v1: receives through lidi)
  serial-in     input side of DYODE v2 (Modbus over a serial line)
  serial-out    output side of DYODE v2
    -c, --config FILE      config.yaml (default: ./config.yaml, beside the
                           binary, then /etc/dyode/config.yaml)
    --log-level LEVEL      DEBUG, INFO (default), WARNING or ERROR

Setting up:
  setup         guided setup in dialog boxes (plain questions when not on a
                terminal): writes config.yaml, the key and the systemd unit.
                [--side in|out] [--variant v1|v2] [--etc DIR] [--plain]
                [--import-config FILE] [--dry-run] [--no-unit]
  new-key       create the shared message key (once, on ONE box; copy it to
                the other):  [-c config.yaml] [--key-file FILE]
  check         validate config.yaml and the key:  [-c FILE] [--side in|out]
                [--serial] (DYODE v2: no key)
  lidi-config   print the lidi parameters derived from config.yaml  [-c FILE]
  interfaces    list network interfaces with their MAC addresses  [--all]
  firewall-rules  print the output box's nftables rules (diode ports only
                from the input box, only on the diode NIC)  [-c FILE]
  systemd-unit  print a systemd unit:  --side in|out [--serial]
                [--workdir DIR] [--binary PATH] [--interface IFACE] [-c FILE]

Measuring a link (trial runs; see docs/PROBE.md):
  probe send    numbered, timestamped UDP at a fixed rate, one thread and
                port per stream:  --to HOST:PORT [--streams N]
                [--mbps X | --pps N] [--size BYTES] [--duration S | --count N]
                [--burst N] [--bind IP] [--sndbuf BYTES] [--json FILE]
  probe recv    loss, burst lengths, Gilbert-Elliott fit (netem line), delay,
                jitter and RaptorQ what-if per stream:  [--listen IP:PORT]
                [--streams N] [--idle S] [--duration S] [--report S]
                [--reorder-ms MS] [--fec-sim T:R,...|none] [--rcvbuf BYTES]
                [--keep] [--log FILE] [--json FILE] [--quiet]
  probe analyze LOG  replay a recv --log file  [--reorder-ms MS]
                [--fec-sim T:R,...] [--json FILE]

Logs:
  logs --archive [--dir DIR] [--keep-weeks N] [--dry-run]
  logs --stats   [--dir DIR] [--json NAME]

  version, help
";

struct Args {
    items: Vec<String>,
}

impl Args {
    fn flag(&mut self, names: &[&str]) -> bool {
        let before = self.items.len();
        self.items.retain(|a| !names.contains(&a.as_str()));
        self.items.len() != before
    }

    fn value(&mut self, names: &[&str]) -> Option<String> {
        let mut i = 0;
        while i < self.items.len() {
            let a = &self.items[i];
            for n in names {
                if a == n {
                    if i + 1 >= self.items.len() {
                        die_usage(&format!("{n} needs a value"));
                    }
                    let v = self.items.remove(i + 1);
                    self.items.remove(i);
                    return Some(v);
                }
                if let Some(v) = a.strip_prefix(&format!("{n}=")) {
                    let v = v.to_string();
                    self.items.remove(i);
                    return Some(v);
                }
            }
            i += 1;
        }
        None
    }

    fn done(&self) {
        if let Some(a) = self.items.first() {
            die_usage(&format!("unexpected argument {a:?}"));
        }
    }
}

fn die_usage(msg: &str) -> ! {
    eprintln!("dyode: {msg}\n\n{USAGE}");
    exit(2)
}

fn level(text: Option<String>) -> log::LevelFilter {
    match text.as_deref().map(str::to_uppercase).as_deref() {
        None | Some("INFO") => log::LevelFilter::Info,
        Some("DEBUG") => log::LevelFilter::Debug,
        Some("TRACE") => log::LevelFilter::Trace,
        Some("WARNING" | "WARN") => log::LevelFilter::Warn,
        Some("ERROR") => log::LevelFilter::Error,
        Some(other) => die_usage(&format!("unknown log level {other:?}")),
    }
}

fn load(explicit: Option<String>) -> config::Config {
    let path = config::find_config(explicit.as_deref().map(Path::new)).unwrap_or_else(|e| {
        log::error!("configuration error: {e}");
        exit(1)
    });
    config::load(&path).unwrap_or_else(|e| {
        log::error!("configuration error: {e}");
        exit(1)
    })
}

fn run(mut args: Args, side: Side, serial: bool) -> ! {
    let lvl = level(args.value(&["--log-level"]));
    let cfg_path = args.value(&["-c", "--config"]);
    args.done();
    logging::init(lvl);
    let cfg = load(cfg_path);
    logging::configure(&cfg.logging, side.as_str(), cfg.lidi.log_level);
    match (side, serial) {
        (Side::In, false) => runtime::run_in(cfg),
        (Side::Out, false) => runtime::run_out(cfg),
        (Side::In, true) => runtime::run_serial_in(cfg),
        (Side::Out, true) => runtime::run_serial_out(cfg),
    }
}

fn cmd_new_key(mut args: Args) {
    let key_file = args.value(&["--key-file"]);
    let cfg_path = args.value(&["-c", "--config"]);
    args.done();
    let path = match key_file {
        Some(k) => PathBuf::from(k),
        None => {
            let p = config::find_config(cfg_path.as_deref().map(Path::new)).ok();
            match p.and_then(|p| config::load(&p).ok()) {
                Some(cfg) => cfg.auth.key_file,
                None => PathBuf::from("dyode.key"),
            }
        }
    };
    match auth::generate_key_file(&path) {
        Ok(()) => {
            println!("created {}", path.display());
            println!("Copy it to the other box, beside its config.yaml (chmod 600).");
            println!("Both boxes need the SAME key: treat it like a password.");
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            eprintln!(
                "{} already exists; not replacing it (a new key on one box only breaks the link)",
                path.display()
            );
            exit(1);
        }
        Err(e) => {
            eprintln!("cannot create {}: {e}", path.display());
            exit(1);
        }
    }
}

fn cmd_check(mut args: Args) {
    let cfg_path = args.value(&["-c", "--config"]);
    let side = args.value(&["--side"]);
    let serial = args.flag(&["--serial"]);
    args.done();
    logging::init(log::LevelFilter::Warn);
    let cfg = load(cfg_path);
    println!("{}: OK", cfg.path.display());
    println!(
        "  network: in {} ({}) -> out {} ({}), out MAC {}",
        cfg.network.in_ip,
        cfg.network.in_interface,
        cfg.network.out_ip,
        cfg.network.out_interface,
        cfg.network.out_mac.as_deref().unwrap_or("NOT SET")
    );
    println!(
        "  lidi: ports {:?}, MTU {}, block {}, repair {}%, max_clients {}",
        cfg.lidi.ports,
        cfg.lidi.mtu,
        cfg.lidi.block,
        cfg.lidi.repair,
        cfg.max_clients()
    );
    for m in &cfg.modules {
        let mut ignored: Vec<String> = match &m.kind {
            config::ModuleKind::Folder(f) => f.ignored.clone(),
            _ => Vec::new(),
        };
        if let Some(p) = m.legacy_port {
            ignored.insert(0, format!("port {p}"));
        }
        let note = if ignored.is_empty() {
            String::new()
        } else {
            format!(" (ignored: {})", ignored.join(", "))
        };
        println!(
            "  endpoint {:>2}: {:<7} {:?}{note}",
            m.index,
            m.kind.type_name(),
            m.name
        );
    }
    if serial {
        println!(
            "  serial: {} @ {} baud (v2: no key needed)",
            cfg.serial.device, cfg.serial.baudrate
        );
    }
    let r = dyode::check::report(&cfg, side.as_deref(), serial);
    for n in &r.notes {
        println!("  {n}");
    }
    for e in &r.errors {
        println!("  error: {e}");
    }
    for w in &r.warnings {
        println!("  warning: {w}");
    }
    if !r.errors.is_empty() {
        exit(1);
    }
}

fn cmd_unit(mut args: Args) {
    let side = match args.value(&["--side"]).as_deref() {
        Some("in") => Side::In,
        Some("out") => Side::Out,
        _ => die_usage("systemd-unit needs --side in|out"),
    };
    let serial = args.flag(&["--serial"]);
    let workdir = args
        .value(&["--workdir"])
        .unwrap_or_else(|| "/etc/dyode".into());
    let binary = args.value(&["--binary"]).unwrap_or_else(|| {
        std::env::current_exe().map_or_else(
            |_| "/usr/local/bin/dyode".into(),
            |p| p.display().to_string(),
        )
    });
    let mut iface = args.value(&["--interface"]);
    let cfg_path = args.value(&["-c", "--config"]);
    args.done();
    if iface.is_none() && !serial {
        if let Some(cfg) = config::find_config(cfg_path.as_deref().map(Path::new))
            .ok()
            .and_then(|p| config::load(&p).ok())
        {
            iface = Some(match side {
                Side::In => cfg.network.in_interface,
                Side::Out => cfg.network.out_interface,
            });
        }
    }
    let command = match (side, serial) {
        (Side::In, false) => "in",
        (Side::Out, false) => "out",
        (Side::In, true) => "serial-in",
        (Side::Out, true) => "serial-out",
    };
    print!(
        "# /etc/systemd/system/{}\n{}",
        setup::unit_name(side),
        setup::systemd_unit_text(
            side,
            command,
            Path::new(&workdir),
            Path::new(&binary),
            iface.as_deref()
        )
    );
}

/// Can we create or change files in `dir` (or where it would be created)?
fn writable(dir: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let mut p = dir;
    while !p.exists() {
        match p.parent() {
            Some(q) => p = q,
            None => return false,
        }
    }
    let Ok(c) = std::ffi::CString::new(p.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: a valid C string.
    unsafe { libc::access(c.as_ptr(), libc::W_OK) == 0 }
}

fn cmd_setup(mut args: Args) {
    use dyode::tui;
    use dyode::wizard::{self, model, ui};
    let plain = args.flag(&["--plain"]);
    let dry_run = args.flag(&["--dry-run"]);
    let no_unit = args.flag(&["--no-unit"]);
    let etc = PathBuf::from(
        args.value(&["--etc"])
            .unwrap_or_else(|| "/etc/dyode".into()),
    );
    let side = match args.value(&["--side"]).as_deref() {
        None => None,
        Some("in") => Some(Side::In),
        Some("out") => Some(Side::Out),
        Some(o) => die_usage(&format!("--side must be in or out, not {o:?}")),
    };
    let variant = match args.value(&["--variant"]).as_deref() {
        None => None,
        Some("v1") => Some(model::Variant::V1),
        Some("v2") => Some(model::Variant::V2),
        Some(o) => die_usage(&format!("--variant must be v1 or v2, not {o:?}")),
    };
    let import = args.value(&["--import-config"]).map(PathBuf::from);
    let preset = wizard::Preset {
        side,
        variant,
        import,
    };
    // Used by install.sh: ask offline/online before anything is installed,
    // in the same dialog look, and write the answer to a file.
    let ask_mode = args.value(&["--ask-install-mode"]);
    let mode_default = args.value(&["--default"]);
    let bundle = args.flag(&["--bundle"]);
    // Hidden: run a key script against a fake box and save every screen.
    let demo = args.value(&["--demo-keys"]);
    let demo_out = args.value(&["--demo-out"]);
    let demo_size = args.value(&["--demo-size"]);
    args.done();

    if let Some(answer) = ask_mode {
        let offline_default = mode_default.as_deref() != Some("online");
        let script = demo.clone();
        let res = match (script, plain) {
            (Some(keys), _) => {
                let mut dui = ui::DialogUi::new(tui::Virtual::new(80, 24, tui::parse_keys(&keys)));
                let r = wizard::ask_install_mode(&mut dui, bundle, offline_default);
                if let Some(out) = &demo_out {
                    let body: String = dui
                        .backend
                        .shots
                        .iter()
                        .map(|(_, f)| format!("{f}\n"))
                        .collect();
                    let _ = std::fs::write(out, body);
                }
                r
            }
            (None, false) if tui::Terminal::open().is_ok() => {
                let t = tui::Terminal::open().expect("terminal");
                wizard::ask_install_mode(&mut ui::DialogUi::new(t), bundle, offline_default)
            }
            _ => {
                let stdin = std::io::stdin();
                wizard::ask_install_mode(
                    &mut ui::PlainUi::new(stdin.lock(), std::io::stderr()),
                    bundle,
                    offline_default,
                )
            }
        };
        match res {
            Some(mode) => {
                if let Err(e) = std::fs::write(&answer, format!("{mode}\n")) {
                    eprintln!("cannot write {answer}: {e}");
                    exit(1);
                }
            }
            None => exit(3),
        }
        return;
    }

    if let Some(script) = demo {
        let (w, h) = demo_size
            .and_then(|s| {
                s.split_once('x')
                    .and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?)))
            })
            .unwrap_or((80, 24));
        let mut env = wizard::demo_env(etc);
        env.dry_run = true;
        env.write_unit = !no_unit;
        let mut dui = ui::DialogUi::new(tui::Virtual::new(w, h, tui::parse_keys(&script)));
        let out = wizard::run(&mut dui, &env, preset);
        let frames = &dui.backend.shots;
        let path = demo_out.unwrap_or_else(|| "setup-demo.html".into());
        let body: String = frames.iter().map(|(_, f)| format!("{f}\n")).collect();
        std::fs::write(&path, body).unwrap_or_else(|e| {
            eprintln!("cannot write {path}: {e}");
            exit(1)
        });
        println!(
            "{} screens to {path} ({})",
            frames.len(),
            if matches!(out, wizard::Outcome::Quit) {
                "ended before writing"
            } else {
                "reached the end"
            }
        );
        return;
    }

    if !dry_run && !writable(&etc) {
        eprintln!(
            "dyode setup writes to {} and /etc/systemd: run it with sudo (or try --dry-run).",
            etc.display()
        );
        exit(1);
    }
    let mut env = model::Env::real(etc, dry_run);
    if no_unit {
        env.write_unit = false;
    }
    let term = if plain {
        None
    } else {
        tui::Terminal::open().ok()
    };
    let out = match term {
        Some(t) => wizard::run(&mut ui::DialogUi::new(t), &env, preset),
        None => {
            let stdin = std::io::stdin();
            wizard::run(
                &mut ui::PlainUi::new(stdin.lock(), std::io::stdout()),
                &env,
                preset,
            )
        }
    };
    match out {
        wizard::Outcome::Quit => {
            eprintln!("setup left; nothing was written.");
            exit(3);
        }
        wizard::Outcome::Written { done, next, .. } => {
            for l in done {
                println!("{l}");
            }
            println!("\nNext:");
            for l in next {
                println!("  {l}");
            }
        }
    }
}

fn cmd_status(mut args: Args) {
    let json = args.flag(&["--json"]);
    let side = args.value(&["--side"]);
    args.done();
    let sides: Vec<&str> = match side.as_deref() {
        Some("in") => vec!["in"],
        Some("out") => vec!["out"],
        None => vec!["in", "out"],
        Some(o) => die_usage(&format!("--side must be in or out, not {o:?}")),
    };
    let mut found = false;
    for s in sides {
        let path = dyode::stats::status_path(s);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        found = true;
        let age = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .map_or(0, |d| d.as_secs());
        if json {
            println!("{text}");
            continue;
        }
        match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(v) => {
                print!("{}", dyode::stats::describe(&v));
                if age > 90 {
                    println!(
                        "warning: {} is {age}s old: is dyode-{s} running?",
                        path.display()
                    );
                }
            }
            Err(e) => eprintln!("{}: {e}", path.display()),
        }
    }
    if !found {
        eprintln!(
            "no status in {}: DYODE is not running on this box (or not as root)",
            dyode::stats::run_dir().display()
        );
        exit(1);
    }
}

/// The token file of the side asked for, or of the only side configured.
fn tokens_path(cfg: &config::Config, side: Option<String>) -> PathBuf {
    let pick = |s: Side| cfg.api.for_side(s).map(|a| a.tokens.clone());
    let found = match side.as_deref() {
        Some("in") => pick(Side::In),
        Some("out") => pick(Side::Out),
        None => pick(Side::Out).or_else(|| pick(Side::In)),
        Some(o) => die_usage(&format!("--side must be in or out, not {o:?}")),
    };
    found.unwrap_or_else(|| {
        cfg.path
            .canonicalize()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("api-tokens")))
            .unwrap_or_else(|| PathBuf::from("api-tokens"))
    })
}

fn cmd_api_token(mut args: Args) {
    let cfg_path = args.value(&["-c", "--config"]);
    let side = args.value(&["--side"]);
    let scopes: Vec<String> = args
        .value(&["--scope"])
        .unwrap_or_else(|| "read".into())
        .split(',')
        .map(|s| s.trim().to_string())
        .collect();
    if args.items.is_empty() {
        die_usage("api-token needs add NAME, list or revoke NAME");
    }
    let action = args.items.remove(0);
    let name = (!args.items.is_empty()).then(|| args.items.remove(0));
    args.done();
    logging::init(log::LevelFilter::Warn);
    let cfg = load(cfg_path);
    let path = tokens_path(&cfg, side);
    match (action.as_str(), name) {
        ("add", Some(n)) => match dyode::apitoken::add(&path, &n, &scopes) {
            Ok(token) => {
                println!("{token}");
                eprintln!(
                    "token {n:?} ({}) added to {}.  It is shown only now: store it in the client's secrets.",
                    scopes.join(","),
                    path.display()
                );
            }
            Err(e) => {
                eprintln!("{e}");
                exit(1);
            }
        },
        ("list", None) => match dyode::apitoken::list(&path) {
            Ok(t) if t.is_empty() => println!("no tokens in {}", path.display()),
            Ok(t) => {
                for tok in t {
                    println!("{:<20} {}", tok.name, tok.scopes.join(","));
                }
            }
            Err(e) => {
                eprintln!("{e}");
                exit(1);
            }
        },
        ("revoke", Some(n)) => match dyode::apitoken::revoke(&path, &n) {
            Ok(true) => println!("revoked {n:?} in {}", path.display()),
            Ok(false) => {
                eprintln!("no token named {n:?} in {}", path.display());
                exit(1);
            }
            Err(e) => {
                eprintln!("{e}");
                exit(1);
            }
        },
        _ => die_usage("api-token needs add NAME, list or revoke NAME"),
    }
}

fn cmd_api_cert(mut args: Args) {
    let dir = PathBuf::from(
        args.value(&["--dir"])
            .unwrap_or_else(|| "/etc/dyode".into()),
    );
    let mut ips = Vec::new();
    while let Some(ip) = args.value(&["--ip"]) {
        if ip.parse::<std::net::IpAddr>().is_err() {
            die_usage(&format!("--ip {ip:?} is not an IP address"));
        }
        ips.push(ip);
    }
    let name = args.value(&["--name"]).unwrap_or_else(|| {
        std::fs::read_to_string("/etc/hostname")
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "dyode".into())
    });
    let days = args.value(&["--days"]).unwrap_or_else(|| "825".into());
    if days.parse::<u32>().map_or(true, |d| d == 0) {
        die_usage("--days needs a positive number");
    }
    let force = args.flag(&["--force"]);
    args.done();
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-.".contains(c))
    {
        die_usage("--name must be a host name");
    }
    match setup::make_api_cert(&dir, &name, &ips, days.parse().unwrap_or(825), force) {
        Ok(c) => {
            println!(
                "wrote {} and {} (valid {days} days, {})",
                c.cert.display(),
                c.key.display(),
                c.names.join(", ")
            );
            println!("{}", c.fingerprint);
            println!(
                "Give clients this certificate (or pin the fingerprint), then set in config.yaml:"
            );
            println!(
                "  api: {{<side>: {{listen: ADDRESS:8443, tls: {{cert: api.crt, key: api.key}}}}}}"
            );
        }
        Err(e) => {
            eprintln!("{e}");
            exit(1);
        }
    }
}

fn cmd_interfaces(mut args: Args) {
    let all = args.flag(&["--all"]);
    args.done();
    let list = setup::list_interfaces(Path::new("/sys/class/net"), all);
    if list.is_empty() {
        println!("no network interfaces found");
        return;
    }
    println!(
        "{:<16} {:<18} {:<8} {:<8} IPv4",
        "interface", "MAC", "state", "carrier"
    );
    for i in list {
        println!(
            "{:<16} {:<18} {:<8} {:<8} {}{}",
            i.name,
            i.mac,
            i.operstate,
            if i.carrier { "yes" } else { "no" },
            i.ipv4.unwrap_or_default(),
            if i.virtual_ { " (virtual)" } else { "" }
        );
    }
}

fn cmd_logs(mut args: Args) {
    let do_archive = args.flag(&["--archive"]);
    let do_stats = args.flag(&["--stats"]);
    let dry_run = args.flag(&["--dry-run"]);
    let dir = PathBuf::from(
        args.value(&["--dir"])
            .unwrap_or_else(|| logs_tool::DEFAULT_DIR.into()),
    );
    let keep = args
        .value(&["--keep-weeks"])
        .map(|v| {
            v.parse::<i64>()
                .unwrap_or_else(|_| die_usage("--keep-weeks needs a number"))
        })
        .unwrap_or(logs_tool::DEFAULT_KEEP_WEEKS);
    let json_name = args
        .value(&["--json"])
        .unwrap_or_else(|| "transfer.jsonl".into());
    args.done();
    if !do_archive && !do_stats {
        die_usage("choose --archive or --stats");
    }
    if !dir.is_dir() {
        eprintln!("no such directory: {}", dir.display());
        exit(1);
    }
    if do_archive {
        let live = ["dyode.log", json_name.as_str()];
        if let Err(e) = logs_tool::archive(
            &dir,
            &live,
            keep,
            timefmt::Date::today_utc(),
            dry_run,
            &mut std::io::stdout(),
        ) {
            eprintln!("archive failed: {e}");
            exit(1);
        }
    }
    if do_stats {
        let p = dir.join(&json_name);
        match logs_tool::summarize(&p) {
            Ok(s) => logs_tool::print_stats(&p, &s),
            Err(e) => {
                eprintln!("no event log at {} ({e})", p.display());
                exit(1);
            }
        }
    }
}

fn num<T: std::str::FromStr>(name: &str, v: Option<String>) -> Option<T> {
    v.map(|t| {
        t.parse::<T>()
            .unwrap_or_else(|_| die_usage(&format!("{name}: bad value {t:?}")))
    })
}

fn secs(name: &str, v: Option<String>) -> Option<std::time::Duration> {
    num::<f64>(name, v).map(|s| {
        if !(s.is_finite() && s >= 0.0) {
            die_usage(&format!("{name} must be zero or more seconds"));
        }
        std::time::Duration::from_secs_f64(s)
    })
}

fn sock_addr(name: &str, text: &str) -> std::net::SocketAddr {
    use std::net::ToSocketAddrs;
    text.to_socket_addrs()
        .ok()
        .and_then(|mut a| a.next())
        .unwrap_or_else(|| die_usage(&format!("{name}: {text:?} is not HOST:PORT")))
}

fn fec_list(v: Option<String>) -> Vec<probe::FecSpec> {
    match v.as_deref() {
        None => probe::FecSpec::defaults(),
        Some("none") => Vec::new(),
        Some(t) => probe::FecSpec::parse_list(t).unwrap_or_else(|e| die_usage(&e)),
    }
}

fn write_json(path: Option<String>, value: &serde_json::Value) {
    if let Some(p) = path {
        let text = serde_json::to_string_pretty(value).unwrap_or_default();
        if let Err(e) = std::fs::write(&p, text + "\n") {
            eprintln!("cannot write {p}: {e}");
            exit(1);
        }
        println!("wrote {p}");
    }
}

fn cmd_probe(mut args: Args) {
    if args.items.is_empty() {
        die_usage("probe needs send, recv or analyze");
    }
    let sub = args.items.remove(0);
    match sub.as_str() {
        "send" => probe_send(args),
        "recv" | "receive" => probe_recv(args),
        "analyze" | "analyse" => probe_analyze(args),
        other => die_usage(&format!(
            "unknown probe command {other:?} (send, recv or analyze)"
        )),
    }
}

fn probe_send(mut args: Args) {
    let to = args
        .value(&["--to"])
        .unwrap_or_else(|| die_usage("probe send needs --to HOST:PORT"));
    let to = sock_addr("--to", &to);
    let bind = num::<std::net::IpAddr>("--bind", args.value(&["--bind"]));
    let streams = num::<u16>("--streams", args.value(&["--streams"]))
        .unwrap_or(1)
        .max(1);
    let size = num::<usize>("--size", args.value(&["--size"])).unwrap_or(1316);
    let mbps = num::<f64>("--mbps", args.value(&["--mbps"]));
    let pps = num::<f64>("--pps", args.value(&["--pps"]));
    let count = num::<u64>("--count", args.value(&["--count"]));
    let duration = secs("--duration", args.value(&["--duration"]));
    let burst = num::<u32>("--burst", args.value(&["--burst"]))
        .unwrap_or(1)
        .max(1);
    let sndbuf = num::<usize>("--sndbuf", args.value(&["--sndbuf"]));
    let json = args.value(&["--json"]);
    args.done();
    if mbps.is_some() && pps.is_some() {
        die_usage("give --mbps or --pps, not both");
    }
    if count.is_some() && duration.is_some() {
        die_usage("give --duration or --count, not both");
    }
    let total_pps = match (mbps, pps) {
        (_, Some(p)) => p,
        (m, None) => m.unwrap_or(10.0) * 1e6 / 8.0 / size as f64,
    };
    let duration = if count.is_none() {
        Some(duration.unwrap_or(std::time::Duration::from_secs(10)))
    } else {
        None
    };
    let opts = probe::SendOptions {
        to,
        bind,
        streams,
        pps: total_pps / f64::from(streams),
        size,
        duration,
        count,
        burst,
        sndbuf,
    };
    let last = to.port() as u32 + u32::from(streams) - 1;
    let ports = if streams > 1 {
        format!("{}-{last}", to.port())
    } else {
        to.port().to_string()
    };
    let how_long = match (count, duration) {
        (Some(c), _) => format!("{c} packets per stream"),
        (None, Some(d)) => format!("{:.1} s", d.as_secs_f64()),
        _ => String::new(),
    };
    println!(
        "probe: {streams} stream(s) to {}:{ports}, {size}-byte packets, {:.1} pkt/s per stream ({:.3} Mbit/s total), {how_long}",
        to.ip(),
        opts.pps,
        total_pps * size as f64 * 8.0 / 1e6
    );
    probe::catch_ctrl_c();
    match probe::run_send(&opts) {
        Ok(reports) => {
            for r in &reports {
                println!(
                    "stream {}  (run {}): sent {} in {:.2} s = {:.1} pkt/s, {:.3} Mbit/s; send errors {}; worst lateness {:.2} ms",
                    r.stream, r.run_id, r.sent, r.seconds, r.pps, r.mbps, r.errors, r.max_late_ms
                );
            }
            if reports.iter().any(|r| r.max_late_ms > 50.0) {
                println!(
                    "note: the sender fell behind its schedule; use more --streams or a lower rate"
                );
            }
            write_json(
                json,
                &serde_json::json!({"tool": "dyode probe", "kind": "send", "reports": reports}),
            );
        }
        Err(e) => {
            eprintln!("probe send: {e}");
            exit(1);
        }
    }
}

fn probe_recv(mut args: Args) {
    let listen = sock_addr(
        "--listen",
        &args
            .value(&["--listen"])
            .unwrap_or_else(|| "0.0.0.0:7000".into()),
    );
    let opts = probe::RecvOptions {
        listen,
        streams: num::<u16>("--streams", args.value(&["--streams"]))
            .unwrap_or(1)
            .max(1),
        idle: secs("--idle", args.value(&["--idle"])).unwrap_or(std::time::Duration::from_secs(10)),
        duration: secs("--duration", args.value(&["--duration"])),
        report_every: secs("--report", args.value(&["--report"]))
            .unwrap_or(std::time::Duration::from_secs(1)),
        reorder_ms: num::<f64>("--reorder-ms", args.value(&["--reorder-ms"]))
            .unwrap_or(500.0)
            .max(0.0),
        fec: fec_list(args.value(&["--fec-sim"])),
        rcvbuf: num::<usize>("--rcvbuf", args.value(&["--rcvbuf"])).unwrap_or(32 << 20),
        keep: args.flag(&["--keep"]),
        quiet: args.flag(&["--quiet", "-q"]),
    };
    let log = args.value(&["--log"]);
    let json = args.value(&["--json"]);
    args.done();
    let last = u32::from(listen.port()) + u32::from(opts.streams) - 1;
    println!(
        "probe: listening on {}:{}{}; stop with Ctrl-C",
        listen.ip(),
        listen.port(),
        if opts.streams > 1 {
            format!("-{last}")
        } else {
            String::new()
        }
    );
    probe::catch_ctrl_c();
    match probe::run_recv(&opts, log.as_deref().map(Path::new)) {
        Ok(res) => {
            for p in &res.ports {
                if p.rcvbuf < opts.rcvbuf {
                    println!(
                        "note: port {} got a {} KiB receive buffer of {} KiB asked; raise net.core.rmem_max or run as root",
                        p.port,
                        p.rcvbuf / 1024,
                        opts.rcvbuf / 1024
                    );
                }
                if p.corrupt > 0 || p.foreign > 0 {
                    println!(
                        "port {}: {} damaged probe datagrams (CRC), {} foreign datagrams ignored",
                        p.port, p.corrupt, p.foreign
                    );
                }
            }
            if res.reports.is_empty() {
                println!("no probe packets received");
            } else if res.reports.len() > 1 {
                println!("{}", probe::totals_line(&res.reports));
            }
            write_json(
                json,
                &probe::reports_json("recv", &res.reports, Some(&res.ports)),
            );
        }
        Err(e) => {
            eprintln!("probe recv: {e}");
            exit(1);
        }
    }
}

fn probe_analyze(mut args: Args) {
    let reorder = num::<f64>("--reorder-ms", args.value(&["--reorder-ms"]))
        .unwrap_or(500.0)
        .max(0.0);
    let fec = fec_list(args.value(&["--fec-sim"]));
    let json = args.value(&["--json"]);
    if args.items.len() != 1 {
        die_usage("probe analyze needs one log file (from probe recv --log)");
    }
    let file = args.items.remove(0);
    match probe::analyze_log(Path::new(&file), reorder, &fec) {
        Ok(reports) => {
            for r in &reports {
                print!("{}", r.render());
            }
            if reports.len() > 1 {
                println!("{}", probe::totals_line(&reports));
            }
            write_json(json, &probe::reports_json("analyze", &reports, None));
        }
        Err(e) => {
            eprintln!("probe analyze: {e}");
            exit(1);
        }
    }
}

fn main() {
    let mut argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() {
        die_usage("no command given");
    }
    let cmd = argv.remove(0);
    let mut args = Args { items: argv };
    match cmd.as_str() {
        "in" => run(args, Side::In, false),
        "out" => run(args, Side::Out, false),
        "serial-in" => run(args, Side::In, true),
        "serial-out" => run(args, Side::Out, true),
        "new-key" => cmd_new_key(args),
        "check" => cmd_check(args),
        "lidi-config" => {
            let p = args.value(&["-c", "--config"]);
            args.done();
            logging::init(log::LevelFilter::Warn);
            print!("{}", transport::lidi_toml(&load(p)));
        }
        "interfaces" => cmd_interfaces(args),
        "setup" => cmd_setup(args),
        "status" => cmd_status(args),
        "api-token" => cmd_api_token(args),
        "api-cert" => cmd_api_cert(args),
        "firewall-rules" => {
            let p = args.value(&["-c", "--config"]);
            let iface_only = args.flag(&["--interface"]);
            args.done();
            logging::init(log::LevelFilter::Warn);
            let cfg = load(p);
            if iface_only {
                println!("{}", cfg.network.out_interface);
            } else {
                match setup::firewall_rules(&cfg) {
                    Ok(r) => print!("{r}"),
                    Err(e) => {
                        eprintln!("error: {e}");
                        exit(1);
                    }
                }
            }
        }
        "systemd-unit" => cmd_unit(args),
        "logs" => cmd_logs(args),
        "probe" => cmd_probe(args),
        "version" | "--version" | "-V" => {
            println!(
                "dyode {} (lidi 3.0.0, {})",
                env!("CARGO_PKG_VERSION"),
                lidi_command_utils::ALLOCATOR_NAME
            )
        }
        "help" | "--help" | "-h" => print!("{USAGE}"),
        other => die_usage(&format!("unknown command {other:?}")),
    }
}
