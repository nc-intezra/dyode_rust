//! `dyode in` and `dyode out`: start lidi, then every configured module
//! under supervision.

use crate::config::{Config, ModuleKind, Side};
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

/// The input side.  Never returns.
pub fn run_in(cfg: Config) -> ! {
    banner(&cfg, Side::In);
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
        for m in &cfg.modules {
            let mbps = match &m.kind {
                ModuleKind::Folder(f) => f.bitrate,
                _ => None,
            };
            let link = Arc::new(ModuleLink::new(Arc::clone(&uplink), m, key.clone(), mbps));
            let module = m.clone();
            let task: supervisor::Task = Box::new(move || match &module.kind {
                ModuleKind::Folder(c) => folder::run_input(&module, c, &link),
                ModuleKind::Modbus(c) => modbus::run_input(&module, c, &link),
                ModuleKind::Screen(c) => screen::run_input(&module, c, &link),
                ModuleKind::Logs(c) => logstream::run_input(&module, c, &link),
            });
            supervisor::spawn(&m.name, task);
        }
        if net.out_mac.is_some() {
            let n = net.clone();
            supervisor::spawn("arp-keeper", Box::new(move || netutil::run_arp_keeper(&n)));
        }
        let status_link = ModuleLink::for_endpoint(
            Arc::clone(&uplink),
            transport::status_endpoint(&cfg),
            transport::status_tag(),
            key.clone(),
            None,
        );
        let c = Arc::clone(&cfg);
        supervisor::spawn(
            "status",
            Box::new(move || status::run_reporter(&c, Some(&status_link))),
        );
        start_api(&cfg, Side::In);
        supervisor::park_forever()
    })
}

/// The output side.  Never returns.
pub fn run_out(cfg: Config) -> ! {
    banner(&cfg, Side::Out);
    let net = cfg.network.clone();
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
    let key = key_or_die(&cfg);
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
        transport::status_endpoint(&cfg),
        transport::status_tag(),
        key.clone(),
        Arc::new(status::StatusInbound::new()),
    );
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
