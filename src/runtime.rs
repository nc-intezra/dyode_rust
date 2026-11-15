//! `dyode in` and `dyode out`: start the transport (lidi, or the air
//! broadcast), then every configured module under supervision.

use crate::config::{Config, ModuleKind, Side, TransportKind};
use crate::envelope::Inbound;
use crate::transport::{self, Lifecycle, ModuleLink, Uplink};
use crate::{auth, folder, logstream, modbus, netutil, screen, serial, status, supervisor};
use std::collections::HashMap;
use std::sync::Arc;

fn banner(cfg: &Config, side: Side) {
    crate::stats::init(cfg, side.as_str());
    log::info!(
        "DYODE {} ({} side), configuration {:?}, version {}, dated {}",
        env!("CARGO_PKG_VERSION"),
        side.as_str(),
        cfg.config_name.as_deref().unwrap_or("?"),
        cfg.config_version.as_deref().unwrap_or("?"),
        cfg.config_date.as_deref().unwrap_or("?")
    );
    if cfg.transport == TransportKind::Air {
        let a = &cfg.air;
        log::info!(
            "air: {} byte symbols, {} byte chunks, repair {}%, interleave {}, {} pass(es) {}s apart, {} Mbit/s",
            a.symbol,
            a.chunk,
            a.repair,
            a.interleave,
            a.passes,
            a.pass_gap,
            a.max_mbps
        );
        return;
    }
    let l = &cfg.lidi;
    log::info!(
        "lidi: ports {:?}, MTU {}, block {} bytes, repair {}%, heartbeat {}s, {} simultaneous transfers",
        l.ports,
        l.mtu,
        l.block,
        l.repair,
        l.heartbeat,
        cfg.max_clients()
    );
}

fn key_or_die(cfg: &Config) -> Option<Vec<u8>> {
    match auth::load_key(&cfg.auth) {
        Ok(k) => k,
        Err(e) => {
            log::error!("configuration error: {e}");
            std::process::exit(1);
        }
    }
}

/// The key, which the air transport cannot do without: its packets are
/// signed and its chunks encrypted with keys derived from it.
fn air_key_or_die(key: &Option<Vec<u8>>) -> Vec<u8> {
    match key {
        Some(k) => k.clone(),
        None => {
            log::error!(
                "transport: air needs the key (auth.key_file): every packet is signed and every chunk encrypted with it"
            );
            std::process::exit(1);
        }
    }
}

/// Start every module's input thread and the status reporter.
fn start_inputs(cfg: &Arc<Config>, uplink: &Arc<Uplink>, key: &Option<Vec<u8>>) {
    for m in &cfg.modules {
        let mbps = match &m.kind {
            ModuleKind::Folder(f) => f.bitrate,
            _ => None,
        };
        let link = Arc::new(ModuleLink::new(Arc::clone(uplink), m, key.clone(), mbps));
        let module = m.clone();
        let task: supervisor::Task = Box::new(move || match &module.kind {
            ModuleKind::Folder(c) => folder::run_input(&module, c, &link),
            ModuleKind::Modbus(c) => modbus::run_input(&module, c, &link),
            ModuleKind::Screen(c) => screen::run_input(&module, c, &link),
            ModuleKind::Logs(c) => logstream::run_input(&module, c, &link),
        });
        supervisor::spawn(&m.name, task);
    }
    let status_link = ModuleLink::for_endpoint(
        Arc::clone(uplink),
        transport::status_endpoint(cfg),
        transport::status_tag(),
        key.clone(),
        None,
    );
    let c = Arc::clone(cfg);
    supervisor::spawn(
        "status",
        Box::new(move || status::run_reporter(&c, Some(&status_link))),
    );
}

/// The input side over the air transport.
fn run_in_air(cfg: Config) -> ! {
    log::info!(
        "input: broadcasting to {} (the radio's input); nothing is ever received",
        cfg.air.to
    );
    let key = key_or_die(&cfg);
    let k = air_key_or_die(&key);
    let endpoints: Vec<(u16, [u8; 8])> = cfg
        .modules
        .iter()
        .map(|m| (m.index, m.tag()))
        .chain(std::iter::once((
            transport::status_endpoint(&cfg),
            transport::status_tag(),
        )))
        .collect();
    let sender = match crate::air::AirSender::new(&cfg, &k, &endpoints) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            log::error!("{e}");
            std::process::exit(1);
        }
    };
    let s = Arc::clone(&sender);
    supervisor::spawn("air-carousel", Box::new(move || s.run()));
    let cfg = Arc::new(cfg);
    let uplink = Arc::new(Uplink::air(sender));
    start_inputs(&cfg, &uplink, &key);
    start_api(&cfg, Side::In);
    supervisor::park_forever()
}

/// The input side.  Never returns.
pub fn run_in(cfg: Config) -> ! {
    banner(&cfg, Side::In);
    if cfg.transport == TransportKind::Air {
        run_in_air(cfg);
    }
    let net = cfg.network.clone();
    log::info!(
        "input {} ({}) -> output {} ({})",
        net.in_ip,
        net.in_interface,
        net.out_ip,
        net.out_mac.as_deref().unwrap_or("MAC not set")
    );
    let key = key_or_die(&cfg);

    // Refuse to come up pretending to work.  Without the ARP entry every
    // transfer is silently discarded by our own kernel, so a service that
    // starts before the diode NIC is configured would look healthy and
    // move nothing.  Exiting lets systemd retry until the NIC is ready.
    if net.out_mac.is_some() && !netutil::set_static_arp(&net) {
        log::error!(
            "cannot set the static ARP entry for {} on {}; refusing to start, because nothing would reach the output side",
            net.out_ip,
            net.in_interface
        );
        std::process::exit(1);
    }

    let sender = match transport::new_sender(&cfg) {
        Ok(s) => &*Box::leak(Box::new(s)),
        Err(e) => {
            log::error!("{e}");
            std::process::exit(1);
        }
    };
    let cfg = Arc::new(cfg);
    std::thread::scope(|scope| {
        if let Err(e) = sender.start(scope) {
            log::error!("cannot start lidi: {e}");
            std::process::exit(1);
        }
        let uplink = Arc::new(Uplink::new(sender, &cfg));
        if net.out_mac.is_some() {
            let n = net.clone();
            supervisor::spawn("arp-keeper", Box::new(move || netutil::run_arp_keeper(&n)));
        }
        start_inputs(&cfg, &uplink, &key);
        start_api(&cfg, Side::In);
        supervisor::park_forever()
    })
}

/// The output side.  Never returns.
pub fn run_out(cfg: Config) -> ! {
    banner(&cfg, Side::Out);
    let net = cfg.network.clone();
    let key = key_or_die(&cfg);
    if cfg.transport == TransportKind::Air {
        let k = air_key_or_die(&key);
        let lifecycle = output_modules(&cfg, &key);
        let receiver = match crate::air::AirReceiver::new(&cfg, &k, &lifecycle) {
            Ok(r) => r,
            Err(e) => {
                log::error!("{e}");
                std::process::exit(1);
            }
        };
        supervisor::spawn("air-receive", Box::new(move || receiver.run()));
        let cfg = Arc::new(cfg);
        let c = Arc::clone(&cfg);
        supervisor::spawn("status", Box::new(move || status::run_reporter(&c, None)));
        start_api(&cfg, Side::Out);
        supervisor::park_forever()
    }
    log::info!(
        "receiving from {} on {} ({})",
        net.in_ip,
        net.out_ip,
        net.out_interface
    );
    log::info!(
        "lidi listens on {} UDP {:?}; the diode-port firewall (install.sh) keeps other interfaces out",
        net.out_ip,
        cfg.lidi.ports
    );
    let lifecycle = output_modules(&cfg, &key);
    let receiver = match transport::new_receiver(&cfg, lifecycle) {
        Ok(r) => &*Box::leak(Box::new(r)),
        Err(e) => {
            log::error!("{e}");
            std::process::exit(1);
        }
    };
    let cfg = Arc::new(cfg);
    let c = Arc::clone(&cfg);
    supervisor::spawn("status", Box::new(move || status::run_reporter(&c, None)));
    start_api(&cfg, Side::Out);
    std::thread::scope(|scope| {
        if let Err(e) = receiver.start(scope) {
            log::error!("cannot start lidi: {e}");
            std::process::exit(1);
        }
        supervisor::park_forever()
    })
}

/// Every module's output handler (and its threads), registered by
/// endpoint, plus the status channel.
fn output_modules(cfg: &Config, key: &Option<Vec<u8>>) -> Lifecycle {
    let net = cfg.network.clone();
    let mut lifecycle = Lifecycle::new();
    for m in &cfg.modules {
        let inbound: Arc<dyn Inbound> = match &m.kind {
            ModuleKind::Folder(c) => {
                let fi = match folder::FolderInbound::new(m, c) {
                    Ok(f) => Arc::new(f),
                    Err(e) => {
                        log::error!("module {:?}: {e}", m.name);
                        std::process::exit(1);
                    }
                };
                let sweeper = Arc::clone(&fi);
                supervisor::spawn(
                    &format!("{}-sweep", m.name),
                    Box::new(move || sweeper.run_sweeper()),
                );
                fi
            }
            ModuleKind::Modbus(c) => {
                let mirror = Arc::new(modbus::Mirror::new(&m.name, c));
                let srv = Arc::clone(&mirror);
                supervisor::spawn(&m.name, Box::new(move || srv.serve()));
                let st = Arc::clone(&mirror);
                supervisor::spawn(
                    &format!("{}-stale", m.name),
                    Box::new(move || st.watch_staleness()),
                );
                Arc::new(modbus::MirrorInbound::new(mirror))
            }
            ModuleKind::Screen(c) => {
                let store = Arc::new(screen::FrameStore::default());
                let (module, c2, n2, s2) = (m.clone(), c.clone(), net.clone(), Arc::clone(&store));
                supervisor::spawn(
                    &m.name,
                    Box::new(move || screen::run_http(&module, &c2, &n2, Arc::clone(&s2))),
                );
                Arc::new(screen::ScreenInbound::new(&m.name, store))
            }
            ModuleKind::Logs(c) => {
                let (tx, rx) = crossbeam_channel::bounded(1024);
                let inbound = Arc::new(logstream::LogsInbound::new(&m.name, tx));
                let (module, c2, ib) = (m.clone(), c.clone(), Arc::clone(&inbound));
                supervisor::spawn(
                    &m.name,
                    Box::new(move || logstream::run_output(&module, &c2, &rx, &ib)),
                );
                inbound
            }
        };
        lifecycle.register(m, key.clone(), inbound);
        if matches!(m.kind, ModuleKind::Folder(_)) {
            // Its work happens in lidi's threads; there is no module
            // thread to report it running.
            crate::stats::module_started(&m.name);
        }
    }
    lifecycle.register_endpoint(
        transport::status_endpoint(cfg),
        transport::status_tag(),
        key.clone(),
        Arc::new(status::StatusInbound::new()),
    );
    lifecycle
}

/// The local API, when configured for this side (and built in).
fn start_api(cfg: &Arc<Config>, side: Side) {
    let Some(api) = cfg.api.for_side(side).cloned() else {
        return;
    };
    #[cfg(feature = "api")]
    {
        let c = Arc::clone(cfg);
        supervisor::spawn("api", Box::new(move || crate::api::serve(&c, side, &api)));
    }
    #[cfg(not(feature = "api"))]
    {
        let _ = api;
        log::error!(
            "an api: section is configured, but this dyode was built without the API (cargo feature \"api\")"
        );
    }
}

fn modbus_modules(cfg: &Config) -> Vec<(String, crate::config::ModbusConfig)> {
    let mut out = Vec::new();
    for m in &cfg.modules {
        match &m.kind {
            ModuleKind::Modbus(c) => out.push((m.name.clone(), c.clone())),
            other => log::warn!(
                "module {:?}: type {:?} is not supported by DYODE v2; ignored",
                m.name,
                other.type_name()
            ),
        }
    }
    if out.is_empty() {
        log::error!("no modbus modules configured");
        std::process::exit(1);
    }
    out
}

/// DYODE v2 input: Modbus over the serial line.  Never returns.
pub fn run_serial_in(cfg: Config) -> ! {
    banner(&cfg, Side::In);
    let modules = modbus_modules(&cfg);
    let cfg = Arc::new(cfg);
    supervisor::spawn(
        "modbus-serial-in",
        Box::new(move || serial::run_input(&modules, &cfg)),
    );
    supervisor::park_forever()
}

/// DYODE v2 output.  Never returns.
pub fn run_serial_out(cfg: Config) -> ! {
    banner(&cfg, Side::Out);
    let mut mirrors = HashMap::new();
    for (name, c) in modbus_modules(&cfg) {
        let mirror = Arc::new(modbus::Mirror::new(&name, &c));
        let srv = Arc::clone(&mirror);
        supervisor::spawn(&name, Box::new(move || srv.serve()));
        let st = Arc::clone(&mirror);
        supervisor::spawn(
            &format!("{name}-stale"),
            Box::new(move || st.watch_staleness()),
        );
        mirrors.insert(name, mirror);
    }
    let serial_cfg = cfg.serial.clone();
    let mirrors = Arc::new(mirrors);
    supervisor::spawn(
        "serial-reader",
        Box::new(move || serial::run_reader(&mirrors, &serial_cfg)),
    );
    supervisor::park_forever()
}
