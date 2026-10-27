//! The `dyode` command.

use dyode::config::{self, Side};
use dyode::{auth, logging, logs_tool, runtime, setup, timefmt, transport};
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
        return;
    }
    match auth::load_key(&cfg.auth) {
        Ok(Some(_)) => println!("  key: {} OK", cfg.auth.key_file.display()),
        Ok(None) => println!("  key: none (auth.required is false: NOT authenticated)"),
        Err(e) => {
            println!("  key: {e}");
            exit(1);
        }
    }
    if let Err(e) = transport::new_receiver_config_check(&cfg) {
        println!("  lidi: {e}");
        exit(1);
    }
    if side.as_deref() == Some("in") && cfg.network.out_mac.is_none() {
        println!(
            "  warning: dyode_out.mac is not set: the input box cannot reach the output box through a diode"
        );
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
    // Hidden: run a key script against a fake box and save every screen.
    let demo = args.value(&["--demo-keys"]);
    let demo_out = args.value(&["--demo-out"]);
    let demo_size = args.value(&["--demo-size"]);
    args.done();

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
