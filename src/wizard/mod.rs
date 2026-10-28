//! `dyode setup`: the guided installer, in the spirit of DYODE (Python)
//! 1.5's curses setup.  It asks one thing per screen, Esc always goes
//! back, nothing is written before the review screen, and the result is
//! checked by the same loader the service uses.

pub mod model;
pub mod ui;

use crate::config::Side;
use model::{Draft, Env, KeyPlan, ModDraft, ModType, Variant};
use std::path::PathBuf;
use ui::{Line, Nav, Opt, Ui};

/// Answers given on the command line: their screens are skipped.
#[derive(Default, Clone)]
pub struct Preset {
    pub side: Option<Side>,
    pub variant: Option<Variant>,
    pub import: Option<PathBuf>,
}

pub enum Outcome {
    /// Written (or would be, in a dry run): what was done, then next steps.
    Written {
        done: Vec<String>,
        next: Vec<String>,
        draft: Box<Draft>,
    },
    Quit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Welcome,
    Variant,
    Side,
    Start,
    ImportPath,
    Name,
    Iface,
    IfaceManual,
    MyIp,
    Peer,
    PeerIp,
    PeerMac,
    PeerIface,
    Ports,
    PortsManual,
    Rate,
    SerialDev,
    SerialManual,
    Baud,
    Modules,
    Key,
    Api,
    ApiAddr,
    Unit,
    Review,
}

fn p(s: impl Into<String>) -> Vec<String> {
    vec![s.into()]
}

struct Wizard<'a, U: Ui> {
    ui: &'a mut U,
    env: &'a Env,
    preset: Preset,
    d: Draft,
    /// Set once the review has written everything.
    done: Option<Vec<String>>,
    key_asked: bool,
}

/// A made-up input box, for screenshots and tests: a diode card, a
/// second card carrying the default route, one unplugged.
pub fn demo_env(etc: PathBuf) -> Env {
    use crate::setup::Interface;
    let nic = |name: &str, mac: &str, carrier: bool, ip: Option<&str>| Interface {
        name: name.into(),
        mac: mac.into(),
        operstate: if carrier { "up".into() } else { "down".into() },
        carrier,
        virtual_: false,
        ipv4: ip.map(String::from),
    };
    Env {
        interfaces: vec![
            nic("eth0", "b8:27:eb:89:1e:f3", true, Some("10.0.1.1")),
            nic("eth1", "00:e0:4c:68:0a:12", false, None),
            nic("wlan0", "dc:a6:32:01:02:03", true, Some("192.168.1.23")),
        ],
        default_route: Some("wlan0".into()),
        cpus: 4,
        etc,
        serial_devices: vec!["/dev/serial0".into(), "/dev/ttyUSB0".into()],
        write_unit: true,
        binary: "/usr/local/bin/dyode".into(),
        dry_run: true,
    }
}

/// The installer's first question: where packages and the binary come
/// from.  Some("offline" | "online"), or None when the person leaves.
pub fn ask_install_mode<U: Ui>(
    ui: &mut U,
    bundle: bool,
    offline_default: bool,
) -> Option<&'static str> {
    let opts = [
        if bundle {
            Opt::new(
                "Offline",
                "only the files in this folder; nothing is downloaded",
            )
        } else {
            Opt::new("Offline", "this folder is not an offline bundle").warn(true)
        },
        Opt::new(
            "Online",
            "apt for OS packages; cargo builds dyode if needed",
        ),
    ];
    let body = vec![
        if bundle {
            "This folder is an offline bundle: it carries the dyode binaries and the OS packages for every supported system. \
             Offline checks every file against SHA256SUMS first."
                .to_string()
        } else {
            "This folder is not an offline bundle, so an offline install only works if nothing needs fetching \
             (a local build, no missing packages)."
                .to_string()
        },
        "Online needs this box to reach the Debian/Ubuntu mirrors. Boxes on the OT side usually cannot.".into(),
        String::new(),
        "The guided setup of DYODE itself follows once the software is installed.".into(),
    ];
    let def = usize::from(!offline_default);
    loop {
        match ui.choose("How should DYODE be installed?", &body, &opts, def) {
            Nav::Next(0) => {
                ui.finish();
                return Some("offline");
            }
            Nav::Next(_) => {
                ui.finish();
                return Some("online");
            }
            // The first screen: there is nothing to go back to.
            Nav::Back => continue,
            Nav::Quit => {
                ui.finish();
                return None;
            }
        }
    }
}

/// Run the wizard to the end (written) or until the person leaves.
pub fn run<U: Ui>(ui: &mut U, env: &Env, preset: Preset) -> Outcome {
    let mut d = Draft::new();
    d.write_unit = env.write_unit;
    let mut w = Wizard {
        ui,
        env,
        preset,
        d,
        done: None,
        key_asked: false,
    };
    w.run()
}

impl<U: Ui> Wizard<'_, U> {
    fn majors(&self) -> Vec<Step> {
        use Step::*;
        let mut v = vec![Welcome, Variant, Side, Start, Name];
        if self.d.variant == model::Variant::V1 {
            v.extend([Iface, MyIp, Peer, Ports, Rate]);
        } else {
            v.extend([SerialDev, Baud]);
        }
        v.push(Modules);
        if self.d.variant == model::Variant::V1 {
            v.push(Key);
            v.push(Api);
        }
        if self.env.write_unit {
            v.push(Unit);
        }
        v.push(Review);
        if self.preset.variant.is_some() {
            v.retain(|s| *s != Variant);
        }
        if self.preset.side.is_some() {
            v.retain(|s| *s != Side);
        }
        v
    }

    fn major_of(s: Step) -> Step {
        match s {
            Step::ImportPath => Step::Start,
            Step::ApiAddr => Step::Api,
            Step::IfaceManual => Step::Iface,
            Step::PeerIp | Step::PeerMac | Step::PeerIface => Step::Peer,
            Step::PortsManual => Step::Ports,
            Step::SerialManual => Step::SerialDev,
            other => other,
        }
    }

    fn run(&mut self) -> Outcome {
        let mut history: Vec<Step> = Vec::new();
        let mut cur = Step::Welcome;
        loop {
            let majors = self.majors();
            if let Some(n) = majors.iter().position(|s| *s == Self::major_of(cur)) {
                self.ui.step(n + 1, majors.len());
            }
            // A screen answered on the command line moves on by itself and
            // is not remembered, so Back skips over it too.
            let (nav, remember) = match self.skip(cur) {
                Some(next) => (Nav::Next(next), false),
                None => (self.screen(cur), true),
            };
            if self.done.is_some() {
                return self.finish();
            }
            match nav {
                Nav::Next(next) => {
                    if remember {
                        history.push(cur);
                    }
                    cur = next;
                }
                Nav::Back => {
                    if let Some(prev) = history.pop() {
                        cur = prev;
                    }
                }
                Nav::Quit => return Outcome::Quit,
            }
        }
    }

    fn skip(&mut self, s: Step) -> Option<Step> {
        match s {
            Step::Variant => self.preset.variant.map(|v| {
                self.d.variant = v;
                Step::Side
            }),
            Step::Side => self.preset.side.map(|v| {
                self.d.side = v;
                Step::Start
            }),
            Step::Unit if !self.env.write_unit => Some(Step::Review),
            Step::Api | Step::ApiAddr if self.d.variant == Variant::V2 => Some(Step::Unit),
            Step::PeerMac if self.d.side == Side::Out => Some(Step::PeerIface),
            _ => None,
        }
    }

    fn after_name(&self) -> Step {
        if self.d.variant == Variant::V1 {
            Step::Iface
        } else {
            Step::SerialDev
        }
    }

    fn after_modules(&self) -> Step {
        if self.d.variant == Variant::V1 {
            Step::Key
        } else {
            Step::Unit
        }
    }

    fn screen(&mut self, s: Step) -> Nav<Step> {
        let env = self.env;
        match s {
            Step::Welcome => {
                let existing = env.config_path();
                let mut body = vec![
                    "This sets up DYODE on this box, one question at a time.".into(),
                    String::new(),
                    "You will choose the side of the diode, the network card or serial line facing it, \
                     the lidi transport and the modules to carry. Nothing is written until the last screen, \
                     which shows the whole config.yaml first."
                        .into(),
                    String::new(),
                    "Both boxes use the SAME config.yaml: set up one box, then copy the file to the other \
                     (or import it there and only change what differs)."
                        .into(),
                ];
                if existing.exists() {
                    body.push(String::new());
                    body.push(format!(
                        "{} exists: you can start from it.",
                        existing.display()
                    ));
                }
                if env.dry_run {
                    body.push(String::new());
                    body.push("DRY RUN: nothing will be written.".into());
                }
                body.push(String::new());
                body.push("Esc goes back a screen at any time; Ctrl-C leaves.".into());
                match self.ui.info("Welcome to DYODE setup", &body) {
                    Nav::Back => Nav::Back,
                    other => other.map(|()| Step::Variant),
                }
            }
            Step::Variant => {
                let opts = [
                    Opt::new(
                        "DYODE v1",
                        "Ethernet diode: files, Modbus, logs, screens over lidi",
                    ),
                    Opt::new("DYODE v2", "serial link: Modbus only"),
                ];
                let def = usize::from(self.d.variant == Variant::V2);
                self.ui
                    .choose(
                        "Which DYODE?",
                        &p("Which kind of diode does this box sit on?"),
                        &opts,
                        def,
                    )
                    .map(|i| {
                        self.d.variant = if i == 0 { Variant::V1 } else { Variant::V2 };
                        Step::Side
                    })
            }
            Step::Side => {
                let opts = [
                    Opt::new("Input box", "on the protected (OT) side: sends"),
                    Opt::new("Output box", "on the IT side: receives"),
                ];
                let def = usize::from(self.d.side == Side::Out);
                self.ui
                    .choose(
                        "Which side is this box?",
                        &p("Data only ever flows from the input box to the output box."),
                        &opts,
                        def,
                    )
                    .map(|i| {
                        self.d.side = if i == 0 { Side::In } else { Side::Out };
                        Step::Start
                    })
            }
            Step::Start => {
                let existing = env.config_path();
                let mut opts = vec![Opt::new("Start a new configuration", "")];
                if existing.exists() {
                    opts.push(Opt::new(
                        format!("Edit {}", existing.display()),
                        "keep everything, change what you need",
                    ));
                }
                opts.push(Opt::new(
                    "Import a config.yaml",
                    "e.g. the other box's, from a USB stick",
                ));
                let def = if self.preset.import.is_some() {
                    opts.len() - 1
                } else {
                    usize::from(existing.exists() || self.d.imported_from.is_some())
                };
                let body = p(
                    "The other box's config.yaml is the best start: both boxes must agree on everything.",
                );
                match self.ui.choose("Starting point", &body, &opts, def) {
                    Nav::Next(0) => {
                        let (v, side, unit) = (self.d.variant, self.d.side, self.d.write_unit);
                        self.d = Draft::new();
                        (self.d.variant, self.d.side, self.d.write_unit) = (v, side, unit);
                        Nav::Next(Step::Name)
                    }
                    Nav::Next(1) if existing.exists() => self.import(&existing),
                    Nav::Next(_) => Nav::Next(Step::ImportPath),
                    Nav::Back => Nav::Back,
                    Nav::Quit => Nav::Quit,
                }
            }
            Step::ImportPath => {
                let def = self
                    .preset
                    .import
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                let check = |s: &str| -> Result<(), String> {
                    let path = PathBuf::from(s);
                    if !path.is_file() {
                        return Err(format!("no file at {s}"));
                    }
                    Draft::import(&path).map(|_| ())
                };
                match self.ui.text(
                    "Import a config.yaml",
                    &p("The file is checked by DYODE's own loader before it is used. The key beside it (dyode.key) can be copied too."),
                    "Path to config.yaml",
                    &def,
                    &check,
                ) {
                    Nav::Next(path) => self.import(&PathBuf::from(path)),
                    Nav::Back => Nav::Back,
                    Nav::Quit => Nav::Quit,
                }
            }
            Step::Name => {
                let def = self.d.name.clone();
                self.ui
                    .text(
                        "Configuration name",
                        &p("A name for this diode, shown in the logs of both boxes."),
                        "Name",
                        &def,
                        &model::check_name,
                    )
                    .map(|v| {
                        self.d.name = v;
                        self.after_name()
                    })
            }
            Step::Iface => {
                let me = self.d.my_iface().to_string();
                let mut opts: Vec<Opt> = env
                    .interfaces
                    .iter()
                    .map(|i| {
                        let mut desc = format!(
                            "{:<17}  {:<7}  {}",
                            i.mac,
                            if i.carrier { "link up" } else { "no link" },
                            i.ipv4.as_deref().unwrap_or("no IPv4")
                        );
                        let route = env.default_route.as_deref() == Some(&i.name);
                        if route {
                            desc.push_str("  default route");
                        }
                        Opt::new(&i.name, desc).warn(route || !i.carrier)
                    })
                    .collect();
                opts.push(Opt::new("Other...", "type the interface name"));
                let def = env
                    .interfaces
                    .iter()
                    .position(|i| i.name == me)
                    .or_else(|| {
                        env.interfaces.iter().position(|i| {
                            i.carrier && env.default_route.as_deref() != Some(&i.name)
                        })
                    })
                    .unwrap_or(0);
                let what = if self.d.side == Side::In {
                    "sends through"
                } else {
                    "receives from"
                };
                let body = vec![
                    format!("Which network card is cabled to the diode? This box {what} it."),
                    "Red entries are probably wrong: no link, or the card that carries this box's default route.".into(),
                ];
                match self.ui.choose("Diode network card", &body, &opts, def) {
                    Nav::Next(i) if i < env.interfaces.len() => {
                        let iface = &env.interfaces[i];
                        self.d.set_my_iface(iface.name.clone());
                        if self.d.side == Side::Out && model::check_mac(&iface.mac).is_ok() {
                            self.d.out_mac = iface.mac.clone();
                        }
                        Nav::Next(Step::MyIp)
                    }
                    Nav::Next(_) => Nav::Next(Step::IfaceManual),
                    Nav::Back => Nav::Back,
                    Nav::Quit => Nav::Quit,
                }
            }
            Step::IfaceManual => {
                let def = self.d.my_iface().to_string();
                self.ui
                    .text(
                        "Diode network card",
                        &p("The interface name, as `ip link` shows it (eth0, enp1s0, ...). It may not exist yet."),
                        "Interface",
                        &def,
                        &model::check_name,
                    )
                    .map(|v| {
                        self.d.set_my_iface(v);
                        Step::MyIp
                    })
            }
            Step::MyIp => {
                let def = self.d.my_ip().to_string();
                let iface = self.d.my_iface().to_string();
                let now = env
                    .iface(&iface)
                    .and_then(|i| i.ipv4.clone())
                    .unwrap_or_else(|| "none".into());
                let body = vec![
                    format!("This box's IPv4 address on {iface} (it has: {now})."),
                    "Both diode cards sit on one small private network of their own, e.g. 10.0.1.1 and 10.0.1.2. \
                     DYODE does not set the address: the network settings must."
                        .into(),
                ];
                self.ui
                    .text(
                        "This box's address",
                        &body,
                        "IPv4 address",
                        &def,
                        &model::check_ipv4,
                    )
                    .map(|v| {
                        self.d.set_my_ip(v);
                        Step::Peer
                    })
            }
            Step::Peer => {
                let other = if self.d.side == Side::In {
                    "output"
                } else {
                    "input"
                };
                let known = self.d.imported_from.is_some();
                let mut body = vec![format!(
                    "The same config.yaml goes on both boxes, so it holds the {other} box's details too. \
                     On that box, `dyode interfaces` lists its cards and MACs."
                )];
                if known {
                    body.push(String::new());
                    body.push(format!(
                        "From the imported file: {} on {}",
                        self.d.peer_ip(),
                        self.d.peer_iface()
                    ));
                    if self.d.side == Side::In {
                        body.push(format!(
                            "  MAC {}",
                            if self.d.out_mac.is_empty() {
                                "NOT SET"
                            } else {
                                &self.d.out_mac
                            }
                        ));
                    }
                    return self
                        .ui
                        .confirm(&format!("The {other} box"), &body, "Keep", "Change", true)
                        .map(|keep| if keep { Step::Ports } else { Step::PeerIp });
                }
                self.ui
                    .info(&format!("The {other} box"), &body)
                    .map(|()| Step::PeerIp)
            }
            Step::PeerIp => {
                let def = self.d.peer_ip().to_string();
                let mine = self.d.my_ip().to_string();
                let check = move |s: &str| -> Result<(), String> {
                    model::check_ipv4(s)?;
                    if s == mine {
                        Err("that is this box's address".into())
                    } else {
                        Ok(())
                    }
                };
                self.ui
                    .text(
                        "The other box",
                        &p("Its IPv4 address on its diode card."),
                        "IPv4 address",
                        &def,
                        &check,
                    )
                    .map(|v| {
                        self.d.set_peer_ip(v);
                        Step::PeerMac
                    })
            }
            Step::PeerMac => {
                let def = self.d.out_mac.clone();
                let body = vec![
                    "The output box's MAC address on its diode card. A diode carries no ARP replies back, \
                     so the input box needs it for a static ARP entry."
                        .into(),
                    "Leave it empty only to fill it in later: nothing reaches the output box without it.".into(),
                ];
                self.ui
                    .text(
                        "The output box's MAC",
                        &body,
                        "MAC address",
                        &def,
                        &model::check_mac_opt,
                    )
                    .map(|v| {
                        self.d.out_mac = v.to_lowercase();
                        Step::PeerIface
                    })
            }
            Step::PeerIface => {
                let def = self.d.peer_iface().to_string();
                self.ui
                    .text(
                        "The other box",
                        &p("Its diode card's name (eth1, enp2s0, ...)."),
                        "Interface",
                        &def,
                        &model::check_name,
                    )
                    .map(|v| {
                        self.d.set_peer_iface(v);
                        Step::Ports
                    })
            }
            Step::Ports => {
                let sug = model::suggested_ports(env.cpus);
                let mut opts: Vec<Opt> = [1usize, 2, 3, 4]
                    .iter()
                    .map(|&n| {
                        let desc = if n == sug {
                            format!("suggested for {} CPUs", env.cpus)
                        } else {
                            String::new()
                        };
                        Opt::new(
                            format!(
                                "{n} port{}: {}",
                                if n > 1 { "s" } else { "" },
                                model::port_list(5000, n)
                            ),
                            desc,
                        )
                    })
                    .collect();
                opts.push(Opt::new("Other...", "type the ports"));
                let cur = model::split_list(&self.d.ports);
                let def = (1..=4)
                    .find(|&n| model::port_list(5000, n) == cur.join(", "))
                    .map_or(4, |n| n - 1);
                let def = if self.d.imported_from.is_none() && cur == ["5000"] {
                    sug - 1
                } else {
                    def
                };
                let body = vec![
                    "lidi sends through these UDP ports, with one RaptorQ encoding thread per port on the input box \
                     and one decoding thread per port on the output box. More ports, more throughput -- if both boxes have the cores."
                        .into(),
                    "Choose for the smaller of the two boxes. The output box's firewall opens exactly these.".into(),
                ];
                match self.ui.choose("lidi transport: ports", &body, &opts, def) {
                    Nav::Next(i) if i < 4 => {
                        self.d.ports = model::port_list(5000, i + 1);
                        Nav::Next(Step::Rate)
                    }
                    Nav::Next(_) => Nav::Next(Step::PortsManual),
                    Nav::Back => Nav::Back,
                    Nav::Quit => Nav::Quit,
                }
            }
            Step::PortsManual => {
                let def = self.d.ports.clone();
                self.ui
                    .text(
                        "lidi transport: ports",
                        &p("UDP ports, comma separated."),
                        "Ports",
                        &def,
                        &model::check_ports,
                    )
                    .map(|v| {
                        self.d.ports = model::split_list(&v).join(", ");
                        Step::Rate
                    })
            }
            Step::Rate => {
                let def = self.d.max_mbps.clone();
                let body = vec![
                    "A cap on everything sent, in Mbit/s (0 = none). lidi sends each block as one burst; \
                     a receiver, NIC or media converter that cannot absorb it drops packets, and a diode cannot ask again."
                        .into(),
                    "100 is safe on any link. Raise it only after measuring (CAVEATS.md).".into(),
                ];
                self.ui
                    .text(
                        "lidi transport: rate",
                        &body,
                        "Mbit/s",
                        &def,
                        &model::check_mbps,
                    )
                    .map(|v| {
                        self.d.max_mbps = v;
                        Step::Modules
                    })
            }
            Step::SerialDev => {
                let mut opts: Vec<Opt> =
                    env.serial_devices.iter().map(|d| Opt::new(d, "")).collect();
                opts.push(Opt::new("Other...", "type the device path"));
                let def = env
                    .serial_devices
                    .iter()
                    .position(|d| *d == self.d.serial_device)
                    .unwrap_or(0);
                match self.ui.choose(
                    "Serial line",
                    &p("Which serial device is wired to the other box?"),
                    &opts,
                    def,
                ) {
                    Nav::Next(i) if i < env.serial_devices.len() => {
                        self.d.serial_device = env.serial_devices[i].clone();
                        Nav::Next(Step::Baud)
                    }
                    Nav::Next(_) => Nav::Next(Step::SerialManual),
                    Nav::Back => Nav::Back,
                    Nav::Quit => Nav::Quit,
                }
            }
            Step::SerialManual => {
                let def = self.d.serial_device.clone();
                self.ui
                    .text(
                        "Serial line",
                        &p("The device path, e.g. /dev/ttyUSB0."),
                        "Device",
                        &def,
                        &model::check_abs_path,
                    )
                    .map(|v| {
                        self.d.serial_device = v;
                        Step::Baud
                    })
            }
            Step::Baud => {
                let rates = ["9600", "19200", "38400", "57600", "115200"];
                let opts: Vec<Opt> = rates
                    .iter()
                    .map(|r| Opt::new(*r, if *r == "57600" { "default" } else { "" }))
                    .collect();
                let def = rates
                    .iter()
                    .position(|r| *r == self.d.baudrate)
                    .unwrap_or(3);
                self.ui
                    .choose(
                        "Serial speed",
                        &p("Both boxes must use the same speed."),
                        &opts,
                        def,
                    )
                    .map(|i| {
                        self.d.baudrate = rates[i].into();
                        Step::Modules
                    })
            }
            Step::Modules => self.modules(),
            Step::Key => self.key(),
            Step::Api => {
                let side = self.d.side.as_str();
                if self.d.api_listen.is_empty() {
                    if let Some((_, l)) = self.d.api_imported.iter().find(|(s, _)| s == side) {
                        self.d.api_listen = l.clone();
                    }
                }
                let opts = [
                    Opt::new("Off", "no API on this box"),
                    Opt::new("On", "HTTPS on the management network card, with tokens"),
                ];
                let body = vec![
                    "The local API serves this box's status and Prometheus metrics, for Prometheus or Zabbix. \
                     On the output box it covers the input box too, from the status it sends through the diode."
                        .into(),
                    "It listens on the management network card, never the diode card. A TLS certificate and a \
                     first token are created when you write the configuration."
                        .into(),
                ];
                let def = usize::from(!self.d.api_listen.is_empty());
                self.ui.choose("Local API", &body, &opts, def).map(|i| {
                    if i == 0 {
                        self.d.api_listen.clear();
                        Step::Unit
                    } else {
                        Step::ApiAddr
                    }
                })
            }
            Step::ApiAddr => {
                let me = self.d.my_iface().to_string();
                let my_ip = self.d.my_ip().to_string();
                let def = if self.d.api_listen.is_empty() {
                    env.interfaces
                        .iter()
                        .filter(|i| i.name != me)
                        .find_map(|i| {
                            i.ipv4
                                .as_deref()
                                .map(|a| a.split('/').next().unwrap_or(a).to_string())
                        })
                        .map_or_else(String::new, |a| format!("{a}:8443"))
                } else {
                    self.d.api_listen.clone()
                };
                let check = move |s: &str| -> Result<(), String> {
                    let a: std::net::SocketAddr = s
                        .parse()
                        .map_err(|_| "address:port, like 192.168.50.4:8443".to_string())?;
                    if a.ip().is_unspecified() {
                        return Err(
                            "one address, not 0.0.0.0: that would include the diode card".into(),
                        );
                    }
                    if a.ip().to_string() == my_ip {
                        return Err(
                            "that is this box's diode address; use the management card's".into(),
                        );
                    }
                    Ok(())
                };
                let body = vec![
                    format!(
                        "The address and port to listen on. This box's diode card is {me}; the API must not use it."
                    ),
                    "Prometheus and Zabbix connect here.".into(),
                ];
                self.ui
                    .text("Local API address", &body, "Address:port", &def, &check)
                    .map(|v| {
                        self.d.api_listen = v;
                        Step::Unit
                    })
            }
            Step::Unit => {
                let unit = crate::setup::unit_name(self.d.side);
                let body = vec![
                    format!(
                        "Write /etc/systemd/system/{unit}, so DYODE starts at boot once the diode card is up?"
                    ),
                    "install.sh does this as well; say No if you manage units yourself.".into(),
                ];
                self.ui
                    .confirm("Service", &body, "Yes", "No", self.d.write_unit)
                    .map(|y| {
                        self.d.write_unit = y;
                        Step::Review
                    })
            }
            Step::Review => self.review(),
        }
    }

    fn import(&mut self, path: &std::path::Path) -> Nav<Step> {
        match Draft::import(path) {
            Ok(mut d) => {
                d.variant = self.d.variant;
                d.side = self.d.side;
                d.write_unit = self.d.write_unit;
                self.d = d;
                Nav::Next(Step::Name)
            }
            Err(e) => {
                let _ = self
                    .ui
                    .notice("Cannot use that file", &[format!("{}:", path.display()), e]);
                Nav::Next(Step::ImportPath)
            }
        }
    }

    // ----------------------------------------------------------------------
    // Modules
    // ----------------------------------------------------------------------

    fn modules(&mut self) -> Nav<Step> {
        let mut def = 0;
        loop {
            let n = self.d.modules.len();
            let mut opts: Vec<Opt> = self
                .d
                .modules
                .iter()
                .map(|m| {
                    let off = self.d.variant == Variant::V2 && m.ty != ModType::Modbus;
                    Opt::new(&m.name, format!("{:<6} {}", m.ty.name(), m.summary())).warn(off)
                })
                .collect();
            opts.push(Opt::new("Add a module", ""));
            if n > 0 {
                opts.push(Opt::new("Remove a module", ""));
            }
            opts.push(Opt::new("Done", "continue"));
            let body = vec![
                "What should cross the diode? Each module is one flow; pick one to change it.".into(),
                "Their order is part of the protocol: keep it the same on both boxes (it is, with the same file).".into(),
            ];
            let def_i = if n > 0 && def == 0 {
                opts.len() - 1
            } else {
                def.min(opts.len() - 1)
            };
            match self.ui.choose("Modules", &body, &opts, def_i) {
                Nav::Back => return Nav::Back,
                Nav::Quit => return Nav::Quit,
                Nav::Next(i) if i < n => {
                    if let Nav::Quit = self.edit_module(i) {
                        return Nav::Quit;
                    }
                    def = i;
                }
                Nav::Next(i) if i == n => match self.add_module() {
                    Nav::Quit => return Nav::Quit,
                    _ => def = 0,
                },
                Nav::Next(i) if n > 0 && i == n + 1 => match self.remove_module() {
                    Nav::Quit => return Nav::Quit,
                    _ => def = 0,
                },
                Nav::Next(_) => {
                    if n == 0 {
                        let _ = self.ui.notice(
                            "Modules",
                            &p("Add at least one module: DYODE has nothing to do without one."),
                        );
                        continue;
                    }
                    return Nav::Next(self.after_modules());
                }
            }
        }
    }

    fn add_module(&mut self) -> Nav<()> {
        let types: Vec<ModType> = if self.d.variant == Variant::V2 {
            vec![ModType::Modbus]
        } else {
            ModType::ALL.to_vec()
        };
        loop {
            let ty = if types.len() == 1 {
                types[0]
            } else {
                let opts: Vec<Opt> = types
                    .iter()
                    .map(|t| Opt::new(t.name(), t.describe()))
                    .collect();
                match self
                    .ui
                    .choose("Add a module", &p("What kind of module?"), &opts, 0)
                {
                    Nav::Next(i) => types[i],
                    Nav::Back => return Nav::Back,
                    Nav::Quit => return Nav::Quit,
                }
            };
            let count = self.d.modules.iter().filter(|m| m.ty == ty).count();
            let mut name = format!(
                "{}{} {}",
                ty.name()[..1].to_uppercase(),
                &ty.name()[1..],
                count + 1
            );
            while !self.d.module_name_free(&name, None) {
                name.push('+');
            }
            let taken: Vec<String> = self.d.modules.iter().map(|m| m.name.clone()).collect();
            let check = move |s: &str| -> Result<(), String> {
                model::check_name(s)?;
                if taken.iter().any(|t| t == s) {
                    Err("another module has that name".into())
                } else {
                    Ok(())
                }
            };
            let name = match self.ui.text(
                &format!("New {} module", ty.name()),
                &p("Its name; both boxes match modules by name and type."),
                "Name",
                &name,
                &check,
            ) {
                Nav::Next(v) => v,
                Nav::Back if types.len() == 1 => return Nav::Back,
                Nav::Back => continue,
                Nav::Quit => return Nav::Quit,
            };
            let mut m = ModDraft::new(&name, ty);
            for f in model::fields(ty) {
                m.set(f.key, f.default, f.kind);
            }
            self.d.modules.push(m);
            let i = self.d.modules.len() - 1;
            match self.edit_fields(i) {
                Nav::Next(()) => return Nav::Next(()),
                Nav::Quit => return Nav::Quit,
                Nav::Back => {
                    self.d.modules.pop();
                    // back to the name
                    if types.len() == 1 {
                        return Nav::Back;
                    }
                }
            }
        }
    }

    fn edit_module(&mut self, i: usize) -> Nav<()> {
        let m = &self.d.modules[i];
        let opts = [
            Opt::new("Change its settings", m.summary()),
            Opt::new("Advanced options", "timings, caps, formats"),
            Opt::new("Rename it", m.name.clone()),
            Opt::new("Move it up", "changes its endpoint number"),
        ];
        let title = format!("{} ({})", m.name, m.ty.name());
        match self.ui.choose(
            &title,
            &p("What do you want to do with this module?"),
            &opts,
            0,
        ) {
            Nav::Next(0) => match self.edit_fields(i) {
                Nav::Quit => Nav::Quit,
                _ => Nav::Next(()),
            },
            Nav::Next(1) => match self.advanced(i) {
                Nav::Quit => Nav::Quit,
                _ => Nav::Next(()),
            },
            Nav::Next(2) => {
                let taken: Vec<String> = self
                    .d
                    .modules
                    .iter()
                    .enumerate()
                    .filter(|(j, _)| *j != i)
                    .map(|(_, m)| m.name.clone())
                    .collect();
                let check = move |s: &str| -> Result<(), String> {
                    model::check_name(s)?;
                    if taken.iter().any(|t| t == s) {
                        Err("another module has that name".into())
                    } else {
                        Ok(())
                    }
                };
                let cur = self.d.modules[i].name.clone();
                match self.ui.text(
                    "Rename",
                    &p("A module's name is part of its identity: rename it on both boxes."),
                    "Name",
                    &cur,
                    &check,
                ) {
                    Nav::Next(v) => {
                        self.d.modules[i].name = v;
                        Nav::Next(())
                    }
                    Nav::Back => Nav::Back,
                    Nav::Quit => Nav::Quit,
                }
            }
            Nav::Next(_) => {
                if i > 0 {
                    self.d.modules.swap(i, i - 1);
                }
                Nav::Next(())
            }
            Nav::Back => Nav::Back,
            Nav::Quit => Nav::Quit,
        }
    }

    /// One screen per field, then the advanced options; Back on the first
    /// field returns Back.
    fn edit_fields(&mut self, i: usize) -> Nav<()> {
        let ty = self.d.modules[i].ty;
        let fields = model::fields(ty);
        let mut k = 0;
        loop {
            while k < fields.len() {
                let f = &fields[k];
                let m = &self.d.modules[i];
                let cur = m.get(f.key);
                let title = format!("{} ({}) {}/{}", m.name, ty.name(), k + 1, fields.len());
                let check = f.check;
                match self.ui.text(&title, &p(f.help), f.label, &cur, &check) {
                    Nav::Next(v) => {
                        self.d.modules[i].set(f.key, &v, f.kind);
                        k += 1;
                    }
                    Nav::Back if k == 0 => return Nav::Back,
                    Nav::Back => k -= 1,
                    Nav::Quit => return Nav::Quit,
                }
            }
            match self.advanced(i) {
                Nav::Next(()) => {}
                Nav::Back => {
                    k = fields.len() - 1;
                    continue;
                }
                Nav::Quit => return Nav::Quit,
            }
            // A module DYODE would refuse is caught here, not at the end.
            let mut one = self.d.clone();
            one.modules = vec![self.d.modules[i].clone()];
            match one.validate(self.env) {
                Ok(_) => return Nav::Next(()),
                Err(e) => {
                    let _ = self.ui.notice("This module needs a fix", &[e]);
                    k = 0;
                }
            }
        }
    }

    /// The options the main screens skip, as a list: Done (first, so Enter
    /// skips it all) and one entry per option with its value or default.
    fn advanced(&mut self, i: usize) -> Nav<()> {
        let ty = self.d.modules[i].ty;
        let fields = model::advanced(ty);
        let mut def = 0;
        loop {
            let m = &self.d.modules[i];
            let mut opts = vec![Opt::new("Done", "keep these and continue")];
            let mut changed = 0;
            for f in &fields {
                let v = m.get(f.key);
                let desc = if v.is_empty() {
                    format!("default ({})", f.default)
                } else {
                    changed += 1;
                    format!("= {v}")
                };
                opts.push(Opt::new(f.label, desc));
            }
            let body = vec![
                format!(
                    "Optional settings for {:?}. Most installations keep the defaults; {changed} of {} changed.",
                    m.name,
                    fields.len()
                ),
                "Pick one to change it, or Done.".into(),
            ];
            let title = format!("{} ({}): advanced", m.name, ty.name());
            let k = match self.ui.choose(&title, &body, &opts, def) {
                Nav::Next(0) => return Nav::Next(()),
                Nav::Next(k) => k - 1,
                Nav::Back => return Nav::Back,
                Nav::Quit => return Nav::Quit,
            };
            let f = &fields[k];
            let cur = self.d.modules[i].get(f.key);
            let check_f = f.check;
            let check = move |s: &str| if s.is_empty() { Ok(()) } else { check_f(s) };
            let help = vec![
                f.help.to_string(),
                format!("Leave it empty for the default ({}).", f.default),
            ];
            match self.ui.text(&title, &help, f.label, &cur, &check) {
                Nav::Next(v) => {
                    self.d.modules[i].set(f.key, &v, f.kind);
                    def = 0; // back on Done: change one thing, press Enter twice
                }
                Nav::Back => def = k + 1,
                Nav::Quit => return Nav::Quit,
            }
        }
    }

    fn remove_module(&mut self) -> Nav<()> {
        let mut opts: Vec<Opt> = self
            .d
            .modules
            .iter()
            .map(|m| Opt::new(&m.name, m.ty.name()))
            .collect();
        opts.push(Opt::new("Cancel", ""));
        match self
            .ui
            .choose("Remove a module", &p("Which one?"), &opts, opts.len() - 1)
        {
            Nav::Next(i) if i < self.d.modules.len() => {
                let name = self.d.modules[i].name.clone();
                match self.ui.confirm(
                    "Remove a module",
                    &p(format!("Remove {name:?}?")),
                    "Remove",
                    "Keep",
                    false,
                ) {
                    Nav::Next(true) => {
                        self.d.modules.remove(i);
                        Nav::Next(())
                    }
                    Nav::Quit => Nav::Quit,
                    _ => Nav::Back,
                }
            }
            Nav::Quit => Nav::Quit,
            _ => Nav::Back,
        }
    }

    // ----------------------------------------------------------------------
    // Key, review, end
    // ----------------------------------------------------------------------

    fn key(&mut self) -> Nav<Step> {
        let env = self.env;
        let path = self.d.key_path(env);
        let mut plans = Vec::new();
        let mut opts = Vec::new();
        if path.is_file() {
            plans.push(KeyPlan::Keep);
            opts.push(Opt::new(
                "Keep the key",
                format!("already at {}", path.display()),
            ));
        } else {
            if let Some(src) = self.d.import_key(env) {
                plans.push(KeyPlan::Copy);
                opts.push(Opt::new(
                    "Copy the imported key",
                    format!("from {}", src.display()),
                ));
            }
            plans.push(KeyPlan::Create);
            opts.push(Opt::new(
                "Create a new key",
                "on ONE box only, then copy it over",
            ));
        }
        plans.push(KeyPlan::Later);
        opts.push(Opt::new("Later", "I will copy the key from the other box"));
        // A new configuration: this is the first box, so make the key here.
        // An imported one: the other box has the key; copy it if it came along.
        let want = match (
            self.d.imported_from.is_some(),
            self.d.import_key(env).is_some(),
        ) {
            (_, true) => KeyPlan::Copy,
            (true, false) => KeyPlan::Later,
            (false, false) => KeyPlan::Create,
        };
        let want = if self.key_asked { self.d.key } else { want };
        self.key_asked = true;
        let def = plans.iter().position(|k| *k == want).unwrap_or(0);
        let body = vec![
            format!(
                "Everything that crosses the diode is signed with a shared key: the SAME {} on both boxes. \
                 Treat it like a password.",
                path.file_name().unwrap_or_default().to_string_lossy()
            ),
            "If the other box already has one, choose Later and copy that one here.".into(),
        ];
        self.ui.choose("Message key", &body, &opts, def).map(|i| {
            self.d.key = plans[i];
            Step::Api
        })
    }

    fn review(&mut self) -> Nav<Step> {
        let env = self.env;
        let d = &self.d;
        let mut lines = Vec::new();
        let side = if d.side == Side::In {
            "input"
        } else {
            "output"
        };
        let variant = if d.variant == Variant::V1 {
            "v1 (Ethernet, lidi)"
        } else {
            "v2 (serial)"
        };
        lines.push(Line::Head(format!(
            "{} -- {side} box, DYODE {variant}",
            d.name
        )));
        if d.variant == Variant::V1 {
            lines.push(Line::Text(format!(
                "input  box: {} on {}",
                d.in_ip, d.in_iface
            )));
            let mac = if d.out_mac.is_empty() {
                "MAC NOT SET".into()
            } else {
                format!("MAC {}", d.out_mac)
            };
            lines.push(Line::Text(format!(
                "output box: {} on {}, {mac}",
                d.out_ip, d.out_iface
            )));
            lines.push(Line::Text(format!(
                "lidi: ports {}, cap {} Mbit/s",
                d.ports, d.max_mbps
            )));
        } else {
            lines.push(Line::Text(format!(
                "serial: {} at {} baud",
                d.serial_device, d.baudrate
            )));
        }
        if d.variant == Variant::V1 {
            lines.push(Line::Text(if d.api_listen.is_empty() {
                "API: off".to_string()
            } else {
                format!("API: https://{}", d.api_listen)
            }));
        }
        for (i, m) in d.modules.iter().enumerate() {
            lines.push(Line::Text(format!(
                "module {i}: {} ({}) {}",
                m.name,
                m.ty.name(),
                m.summary()
            )));
        }
        let valid = d.validate(env);
        lines.push(Line::Text(String::new()));
        if let Err(e) = &valid {
            lines.push(Line::Head("Must be fixed before writing".into()));
            lines.push(Line::Warn(e.clone()));
            lines.push(Line::Text(String::new()));
        }
        let warns = d.warnings(env);
        if !warns.is_empty() {
            lines.push(Line::Head(format!("Check ({})", warns.len())));
            lines.extend(warns.into_iter().map(Line::Warn));
            lines.push(Line::Text(String::new()));
        }
        lines.push(Line::Head(if env.dry_run {
            "Would do (dry run)".into()
        } else {
            "Will do".into()
        }));
        lines.extend(
            d.plan(env)
                .into_iter()
                .map(|l| Line::Text(format!("- {l}"))),
        );
        lines.push(Line::Text(String::new()));
        lines.push(Line::Head("config.yaml".into()));
        lines.extend(d.to_yaml().lines().map(|l| Line::Code(l.to_string())));
        let accept = if valid.is_err() {
            "Back"
        } else if env.dry_run {
            "Finish"
        } else {
            "Write"
        };
        match self.ui.review("Review", &lines, accept) {
            Nav::Next(()) if valid.is_err() => Nav::Back,
            Nav::Next(()) => match model::apply(&self.d, env) {
                Ok(done) => {
                    self.done = Some(done);
                    Nav::Next(Step::Review)
                }
                Err(e) => {
                    let _ = self.ui.notice(
                        "Could not write",
                        &[
                            e,
                            String::new(),
                            "Nothing after this point was changed.".into(),
                        ],
                    );
                    Nav::Next(Step::Review)
                }
            },
            other => other.map(|()| Step::Review),
        }
    }

    fn finish(&mut self) -> Outcome {
        let next = model::next_steps(&self.d, self.env);
        let mut body = self.done.take().unwrap_or_default();
        body.push(String::new());
        body.push("Next:".into());
        body.extend(next.iter().map(|s| format!("  {s}")));
        if self.ui.full_screen() {
            let _ = self.ui.notice(
                if self.env.dry_run {
                    "Done (dry run)"
                } else {
                    "Done"
                },
                &body,
            );
        }
        self.ui.finish();
        body.truncate(body.len() - next.len() - 2);
        Outcome::Written {
            done: body,
            next,
            draft: Box::new(self.d.clone()),
        }
    }
}

impl<T> Nav<T> {
    pub fn map<V>(self, f: impl FnOnce(T) -> V) -> Nav<V> {
        match self {
            Nav::Next(t) => Nav::Next(f(t)),
            Nav::Back => Nav::Back,
            Nav::Quit => Nav::Quit,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::{Virtual, parse_keys};
    use ui::DialogUi;

    fn run_script(script: &str, etc: &std::path::Path, preset: Preset) -> (Outcome, Vec<String>) {
        let mut env = demo_env(etc.to_path_buf());
        env.dry_run = false;
        env.write_unit = false;
        let mut ui = DialogUi::new(Virtual::new(80, 24, parse_keys(script)));
        let out = run(&mut ui, &env, preset);
        // When the script runs out the wizard asks "Leave setup?": drop that.
        let frames = ui
            .backend
            .frames
            .into_iter()
            .filter(|f| !f.contains("Leave setup?"))
            .collect();
        (out, frames)
    }

    // welcome, v1, input, new, name, eth0, ip, peer info, peer ip, peer mac,
    // peer iface, ports (suggested), rate, add folder (type, name, in, out),
    // done, key: create, review: write, done.
    // The folder's advanced screen: settle (second option) changed to 5, then Done.
    const FULL: &str = "enter enter enter enter ctrl-u 'Line 3' enter enter enter enter enter \
                        'b8:27:eb:b1:ff:ab' enter enter enter enter enter enter enter enter enter \
                        down down enter '5' enter enter \
                        enter enter enter enter enter";

    #[test]
    fn full_run_writes_a_config_dyode_accepts() {
        let dir = crate::testutil::TempDir::new();
        let (out, _) = run_script(FULL, dir.path(), Preset::default());
        let Outcome::Written { draft, .. } = out else {
            panic!("did not finish")
        };
        assert_eq!(draft.name, "Line 3");
        let cfg = crate::config::load(&dir.path().join("config.yaml")).unwrap();
        assert_eq!(cfg.lidi.ports, vec![5000, 5001, 5002]);
        assert_eq!(cfg.network.out_mac.as_deref(), Some("b8:27:eb:b1:ff:ab"));
        let crate::config::ModuleKind::Folder(f) = &cfg.modules[0].kind else {
            panic!("not a folder")
        };
        assert_eq!(f.settle, 5.0);
        assert!(dir.path().join("dyode.key").is_file());
    }

    #[test]
    fn api_on_writes_listen_cert_and_token() {
        if std::process::Command::new("openssl")
            .arg("version")
            .output()
            .is_err()
        {
            return;
        }
        let dir = crate::testutil::TempDir::new();
        // As FULL, but "On" at the API screen, then an address on wlan0's
        // network (the demo box's management card).
        let script = "enter enter enter enter ctrl-u 'Line 3' enter enter enter enter enter \
                      'b8:27:eb:b1:ff:ab' enter enter enter enter enter enter enter enter enter \
                      enter enter enter down enter ctrl-u '192.168.1.23:8443' enter enter enter enter";
        let (out, frames) = run_script(script, dir.path(), Preset::default());
        let Outcome::Written { done, .. } = out else {
            panic!("did not finish: {}", frames.last().unwrap())
        };
        let cfg = crate::config::load(&dir.path().join("config.yaml")).unwrap();
        let api = cfg.api.input.expect("api.in");
        assert_eq!(api.listen.to_string(), "192.168.1.23:8443");
        assert!(dir.path().join("api.crt").is_file() && dir.path().join("api.key").is_file());
        let line = done
            .iter()
            .find(|l| l.contains("shown only now"))
            .expect("token line");
        let token = line.rsplit(' ').next().unwrap();
        let tokens = crate::apitoken::list(&dir.path().join("api-tokens")).unwrap();
        assert_eq!(
            crate::apitoken::verify(&tokens, token).unwrap().name,
            "monitoring"
        );
    }

    #[test]
    fn esc_goes_back_and_bad_input_is_refused() {
        let dir = crate::testutil::TempDir::new();
        // To the name, Esc back to the start screen, forward again; a bad
        // address is refused with a message, then corrected.
        let script = "enter enter enter enter esc enter enter enter ctrl-u '10.0.1' enter ctrl-u '10.0.1.1' enter";
        let (out, frames) = run_script(script, dir.path(), Preset::default());
        assert!(matches!(out, Outcome::Quit));
        assert!(
            frames.iter().any(|f| f.contains("! an IPv4 address")),
            "no error shown"
        );
        assert!(
            frames.last().unwrap().contains("The output box"),
            "{}",
            frames.last().unwrap()
        );
        assert!(!dir.path().join("config.yaml").exists());
    }

    #[test]
    fn install_mode_question() {
        let mut ui = DialogUi::new(Virtual::new(80, 24, parse_keys("down enter")));
        assert_eq!(ask_install_mode(&mut ui, true, true), Some("online"));
        assert!(ui.backend.frames[0].contains("How should DYODE be installed?"));
        let mut ui = DialogUi::new(Virtual::new(80, 24, parse_keys("enter")));
        assert_eq!(ask_install_mode(&mut ui, false, false), Some("online"));
        // Esc stays on this first screen; running out of keys is Ctrl-C twice.
        let mut ui = DialogUi::new(Virtual::new(80, 24, parse_keys("esc")));
        assert_eq!(ask_install_mode(&mut ui, true, true), None);
    }

    #[test]
    fn preset_side_skips_its_screen() {
        let dir = crate::testutil::TempDir::new();
        let preset = Preset {
            side: Some(Side::Out),
            variant: Some(Variant::V1),
            import: None,
        };
        let (_, frames) = run_script("enter", dir.path(), preset);
        assert!(frames.last().unwrap().contains("Starting point"));
        assert!(frames.last().unwrap().contains("step 2 of"));
    }

    #[test]
    fn plain_questions_work_from_a_pipe() {
        let dir = crate::testutil::TempDir::new();
        let mut env = demo_env(dir.path().to_path_buf());
        env.dry_run = false;
        env.write_unit = false;
        // v1, out side: the out box's MAC comes from its own card.
        let answers =
            "\n1\n2\n1\nPiped\n1\n10.0.1.2\n\n\n\n\n\n1\n1\n\n/srv/a\n/srv/b\n\n\n1\n\n\n";
        let mut out = Vec::new();
        let res = run(
            &mut ui::PlainUi::new(answers.as_bytes(), &mut out),
            &env,
            Preset::default(),
        );
        let text = String::from_utf8_lossy(&out).to_string();
        assert!(matches!(res, Outcome::Written { .. }), "{text}");
        let cfg = crate::config::load(&dir.path().join("config.yaml")).unwrap();
        assert_eq!(cfg.network.out_mac.as_deref(), Some("b8:27:eb:89:1e:f3"));
        assert_eq!(cfg.config_name.as_deref(), Some("Piped"));
    }
}
