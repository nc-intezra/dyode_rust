//! Setup helpers (the parts of dyode_setup.py that are not interactive
//! prompts): listing network interfaces with their MAC addresses, and
//! writing the systemd unit.

use crate::config::Side;
use std::fs;
use std::path::Path;

pub struct Interface {
    pub name: String,
    pub mac: String,
    pub operstate: String,
    pub carrier: bool,
    pub virtual_: bool,
    pub ipv4: Option<String>,
}

fn read(path: &Path) -> String {
    fs::read_to_string(path)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Network interfaces from /sys/class/net (virtual ones skipped unless asked).
pub fn list_interfaces(sys_root: &Path, include_virtual: bool) -> Vec<Interface> {
    let addrs = crate::netutil::interface_addresses();
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir(sys_root) else {
        return out;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let dir = e.path();
        let virtual_ = fs::read_link(&dir)
            .map(|l| l.to_string_lossy().contains("/virtual/"))
            .unwrap_or(false);
        if name == "lo" || (virtual_ && !include_virtual) {
            continue;
        }
        out.push(Interface {
            mac: read(&dir.join("address")),
            operstate: read(&dir.join("operstate")),
            carrier: read(&dir.join("carrier")) == "1",
            virtual_,
            ipv4: addrs.get(&name).map(ToString::to_string),
            name,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// systemd-escape(1) for a path component used in a unit name.  A VLAN
/// ('eth0.100') or an altname with a dash has to be escaped, or the .device
/// unit never matches and the dependency silently does nothing.
pub fn systemd_escape(text: &str) -> String {
    let mut out = String::new();
    for (i, ch) in text.chars().enumerate() {
        if ch == '/' {
            out.push('-');
        } else if ch.is_ascii_alphanumeric() || ch == '_' || (ch == '.' && i > 0) {
            out.push(ch);
        } else {
            let mut buf = [0u8; 4];
            for b in ch.encode_utf8(&mut buf).bytes() {
                out.push_str(&format!("\\x{b:02x}"));
            }
        }
    }
    out
}

pub fn device_unit(interface: &str) -> String {
    format!(
        "sys-subsystem-net-devices-{}.device",
        systemd_escape(interface)
    )
}

/// Shell that waits for the interface to have an IPv4 address.  The diode
/// NIC is configured by hand, so at boot this service can easily start
/// before that address exists.
pub fn wait_for_address_cmd(interface: &str, seconds: u32) -> String {
    format!(
        "/bin/sh -c 'for i in $(seq 1 {seconds}); do ip -4 addr show dev {interface} 2>/dev/null | grep -q \"inet \" && exit 0; sleep 1; done; echo \"{interface} still has no IPv4 address after {seconds}s\" >&2; exit 1'"
    )
}

pub fn unit_name(side: Side) -> String {
    format!("dyode-{}.service", side.as_str())
}

/// The systemd unit for one side.  `command` is `in`/`out` (v1, lidi) or
/// `serial-in`/`serial-out` (v2).
pub fn systemd_unit_text(
    side: Side,
    command: &str,
    workdir: &Path,
    binary: &Path,
    interface: Option<&str>,
) -> String {
    let reason = match side {
        Side::In => "the static ARP entry",
        Side::Out => "binding the Modbus server to port 502",
    };
    let word = match side {
        Side::In => "input (sending)",
        Side::Out => "output (receiving)",
    };
    let mut l = vec![
        "[Unit]".to_string(),
        format!("Description=DYODE {word} side ({command})"),
        "After=network-online.target".into(),
        "Wants=network-online.target".into(),
    ];
    if let Some(i) = interface {
        l.push("# Stop and restart with the diode NIC, and do not start without it.".into());
        l.push(format!("After={}", device_unit(i)));
        l.push(format!("BindsTo={}", device_unit(i)));
    }
    l.push(
        "# A diode may sit behind a network that takes long to come back: never give up.".into(),
    );
    l.push("StartLimitIntervalSec=0".into());
    l.push(String::new());
    l.push("[Service]".into());
    l.push(format!("WorkingDirectory={}", workdir.display()));
    if let Some(i) = interface {
        l.push(format!("ExecStartPre={}", wait_for_address_cmd(i, 60)));
    }
    l.push(format!(
        "ExecStart={} {command} --config {}/config.yaml",
        binary.display(),
        workdir.display()
    ));
    l.extend([
        "Restart=always".to_string(),
        "RestartSec=5".into(),
        "StandardInput=null".into(),
        "# Creates /var/log/dyode-transfer with the right mode before start.".into(),
        "LogsDirectory=dyode-transfer".into(),
        "LogsDirectoryMode=0750".into(),
        "# /run/dyode: the status file read by 'dyode status'.".into(),
        "RuntimeDirectory=dyode".into(),
        "RuntimeDirectoryMode=0750".into(),
        format!("# root is needed for {reason}."),
        "User=root".into(),
        String::new(),
        "[Install]".into(),
        "WantedBy=multi-user.target".into(),
    ]);
    if let Some(i) = interface {
        // BindsTo only propagates the STOP when the NIC disappears; this is
        // what brings DYODE back when it reappears.
        l.push(format!("WantedBy={}", device_unit(i)));
    }
    l.push(String::new());
    l.join("\n")
}

/// What is wrong between an installed unit and config.yaml, in plain
/// words.  `unit` is the unit file's text; `serial` says DYODE v2.
pub fn check_unit(
    unit: &str,
    side: Side,
    cfg: &crate::config::Config,
    serial: bool,
) -> Vec<String> {
    let mut w = Vec::new();
    let name = unit_name(side);
    let bound: Option<&str> = unit
        .lines()
        .find_map(|l| l.trim().strip_prefix("BindsTo="))
        .map(str::trim);
    let exec = unit
        .lines()
        .find_map(|l| l.trim().strip_prefix("ExecStart="))
        .unwrap_or("");
    let words: Vec<&str> = exec.split_whitespace().collect();
    let command = words.get(1).copied().unwrap_or("");
    let want_cmd = match (side, serial) {
        (Side::In, false) => "in",
        (Side::Out, false) => "out",
        (Side::In, true) => "serial-in",
        (Side::Out, true) => "serial-out",
    };
    if !command.is_empty() && command != want_cmd {
        w.push(format!(
            "{name} runs 'dyode {command}', but this is the {want_cmd} side"
        ));
    }
    if !serial {
        let iface = match side {
            Side::In => &cfg.network.in_interface,
            Side::Out => &cfg.network.out_interface,
        };
        let want = device_unit(iface);
        match bound {
            Some(b) if b == want => {}
            Some(b) => {
                let shown = b
                    .strip_prefix("sys-subsystem-net-devices-")
                    .and_then(|x| x.strip_suffix(".device"))
                    .unwrap_or(b);
                w.push(format!(
                    "{name} waits for the network card {shown}, but config.yaml says {iface}: \
                     it will never start (or start on the wrong card).  Regenerate it: \
                     sudo dyode systemd-unit --side {} > /etc/systemd/system/{name}, or re-run install.sh",
                    side.as_str()
                ));
            }
            None => w.push(format!("{name} is not tied to the diode card {iface} (no BindsTo=): it may start before the card is up")),
        }
    }
    if let Some(i) = words.iter().position(|w| *w == "--config" || *w == "-c") {
        if let Some(p) = words.get(i + 1) {
            let unit_cfg = std::fs::canonicalize(p).ok();
            let this = std::fs::canonicalize(&cfg.path).ok();
            if unit_cfg.is_some() && this.is_some() && unit_cfg != this {
                w.push(format!("{name} uses {p}, not {}", cfg.path.display()));
            } else if unit_cfg.is_none() {
                w.push(format!("{name} uses {p}, which does not exist"));
            }
        }
    }
    w
}

pub struct ApiCert {
    pub cert: std::path::PathBuf,
    pub key: std::path::PathBuf,
    pub names: Vec<String>,
    /// As openssl prints it: "sha256 Fingerprint=AB:CD:...".
    pub fingerprint: String,
}

/// A self-signed TLS certificate for the local API, made with openssl
/// (present on every supported system): api.crt and api.key in `dir`.
pub fn make_api_cert(
    dir: &Path,
    name: &str,
    ips: &[String],
    days: u32,
    force: bool,
) -> Result<ApiCert, String> {
    use std::process::{Command, Stdio};
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-.".contains(c))
    {
        return Err(format!("{name:?} is not a host name"));
    }
    for ip in ips {
        if ip.parse::<std::net::IpAddr>().is_err() {
            return Err(format!("{ip:?} is not an IP address"));
        }
    }
    let (crt, key) = (dir.join("api.crt"), dir.join("api.key"));
    if !force && (crt.exists() || key.exists()) {
        return Err(format!(
            "{} or {} exists; --force replaces them",
            crt.display(),
            key.display()
        ));
    }
    fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let mut names = vec![format!("DNS:{name}")];
    names.extend(ips.iter().map(|i| format!("IP:{i}")));
    let out = Command::new("openssl")
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
        .arg(&key)
        .arg("-out")
        .arg(&crt)
        .args([
            "-days",
            &days.to_string(),
            "-subj",
            &format!("/CN={name}/O=DYODE"),
        ])
        .args(["-addext", &format!("subjectAltName={}", names.join(","))])
        .args(["-addext", "basicConstraints=critical,CA:FALSE"])
        .args(["-addext", "keyUsage=critical,digitalSignature"])
        .args(["-addext", "extendedKeyUsage=serverAuth"])
        .stdout(Stdio::null())
        .output()
        .map_err(|e| {
            format!("cannot run openssl ({e}): install it, or use a certificate from your CA")
        })?;
    if !out.status.success() {
        return Err(format!(
            "openssl failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(&key, fs::Permissions::from_mode(0o600));
    let fingerprint = Command::new("openssl")
        .args(["x509", "-noout", "-fingerprint", "-sha256", "-in"])
        .arg(&crt)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    Ok(ApiCert {
        cert: crt,
        key,
        names,
        fingerprint,
    })
}

/// Copy a key file after checking it, readable by root only.
pub fn install_key_copy(source: &Path, dest: &Path) -> Result<(), String> {
    crate::auth::read_key_file(source).map_err(|e| e.to_string())?;
    let text = fs::read(source).map_err(|e| e.to_string())?;
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dest)
        .map_err(|e| format!("cannot create {}: {e}", dest.display()))?;
    f.write_all(&text).map_err(|e| e.to_string())
}

/// nftables rules keeping the diode ports for the input box only (D-02).
///
/// Linux accepts a packet for any of its own addresses on any interface, so
/// a host on the IT network could otherwise send to the output box's lidi
/// ports through its IT-facing NIC.  Every envelope is signed, but lidi
/// itself is not: forged lidi packets with a new session id make lidi abort
/// the transfers in flight, and a flood costs CPU.  These rules drop any
/// datagram for a diode port unless it arrives on the diode NIC from
/// dyode_in.ip.  Nothing else on the box is touched.
pub fn firewall_rules(cfg: &crate::config::Config) -> Result<String, String> {
    let iface = &cfg.network.out_interface;
    if iface.is_empty()
        || iface.len() > 15
        || !iface
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_.:-".contains(c))
    {
        return Err(format!("unsafe interface name {iface:?}"));
    }
    let mut ports = cfg.lidi.ports.clone();
    ports.sort_unstable();
    let plist = ports
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let in_ip = cfg.network.in_ip;
    let api_rule = cfg.api.output.as_ref().map_or(String::new(), |a| {
        format!(
            "\t\ttcp dport {} iifname \"{iface}\" counter drop comment \"DYODE: the API never on the diode NIC\"\n",
            a.listen.port()
        )
    });
    Ok(format!(
        "# Managed by DYODE install.sh (rewritten on each run; --no-firewall to skip).\n\
         # Diode ports {plist}: accepted only on {iface} from the input box {in_ip}.\n\
         # Everything else on this box is untouched.  See CAVEATS.md.\n\
         table inet dyode\n\
         delete table inet dyode\n\
         table inet dyode {{\n\
         \tchain input {{\n\
         \t\ttype filter hook input priority -10; policy accept;\n\
         \t\tmeta nfproto ipv6 udp dport {{ {plist} }} counter drop comment \"DYODE: diode ports are IPv4 only\"\n\
         \t\tudp dport {{ {plist} }} iifname != \"{iface}\" counter drop comment \"DYODE: diode ports only on the diode NIC\"\n\
         \t\tudp dport {{ {plist} }} ip saddr != {in_ip} counter drop comment \"DYODE: diode ports only from the input box\"\n\
         {api_rule}\
         \t}}\n\
         }}\n"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// packaging/systemd/reference/ must stay what the code generates.
    #[test]
    fn reference_units_match_the_generator() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        for (file, side, cmd, iface) in [
            ("dyode-in.service", Side::In, "in", Some("eth0")),
            ("dyode-out.service", Side::Out, "out", Some("eth1")),
            ("dyode-in-serial.service", Side::In, "serial-in", None),
            ("dyode-out-serial.service", Side::Out, "serial-out", None),
        ] {
            let text =
                fs::read_to_string(root.join("packaging/systemd/reference").join(file)).unwrap();
            let body: String = text
                .lines()
                .skip_while(|l| l.starts_with("# "))
                .map(|l| format!("{l}\n"))
                .collect();
            let want = systemd_unit_text(
                side,
                cmd,
                Path::new("/etc/dyode"),
                Path::new("/usr/local/bin/dyode"),
                iface,
            );
            assert_eq!(
                body, want,
                "{file} is stale: regenerate it with dyode systemd-unit"
            );
        }
    }

    #[test]
    fn unit_checks() {
        let d = crate::testutil::TempDir::new();
        let cfg_path = d.path().join("config.yaml");
        fs::write(
            &cfg_path,
            "dyode_out: {interface: enp2s0}\nmodules:\n  a: {type: folder, in: /a, out: /b}\n",
        )
        .unwrap();
        let cfg = crate::config::load(&cfg_path).unwrap();
        let unit = |iface: Option<&str>, cmd: &str| {
            systemd_unit_text(
                Side::Out,
                cmd,
                d.path(),
                Path::new("/usr/local/bin/dyode"),
                iface,
            )
        };
        assert!(check_unit(&unit(Some("enp2s0"), "out"), Side::Out, &cfg, false).is_empty());
        let w = check_unit(&unit(Some("eth1"), "out"), Side::Out, &cfg, false);
        assert!(
            w.len() == 1 && w[0].contains("eth1") && w[0].contains("enp2s0"),
            "{w:?}"
        );
        let w = check_unit(&unit(None, "out"), Side::Out, &cfg, false);
        assert!(w[0].contains("not tied"), "{w:?}");
        let w = check_unit(&unit(None, "serial-out"), Side::Out, &cfg, false);
        assert!(w.iter().any(|l| l.contains("serial-out")), "{w:?}");
        assert!(check_unit(&unit(None, "serial-out"), Side::Out, &cfg, true).is_empty());
    }

    #[test]
    fn escape() {
        assert_eq!(systemd_escape("eth0"), "eth0");
        assert_eq!(systemd_escape("eth0.100"), "eth0.100");
        assert_eq!(systemd_escape("enp1s0-foo"), "enp1s0\\x2dfoo");
        assert_eq!(device_unit("eth1"), "sys-subsystem-net-devices-eth1.device");
    }

    #[test]
    fn firewall() {
        let t = "dyode_in: {ip: 10.0.1.1}\ndyode_out: {ip: 10.0.1.2, interface: eth1}\nlidi: {ports: [5001, 5000]}\nmodules:\n  m: {type: modbus, ip: 1.2.3.4, registers: [0-1]}\n";
        let cfg = crate::config::parse(t, Path::new("c"), Path::new("/")).unwrap();
        let r = firewall_rules(&cfg).unwrap();
        assert!(r.contains("udp dport { 5000, 5001 } iifname != \"eth1\" counter drop"));
        assert!(r.contains("ip saddr != 10.0.1.1"));
    }

    #[test]
    fn unit() {
        let t = systemd_unit_text(
            Side::In,
            "in",
            Path::new("/opt/dyode"),
            Path::new("/usr/local/bin/dyode"),
            Some("eth0"),
        );
        assert!(t.contains("ExecStart=/usr/local/bin/dyode in --config /opt/dyode/config.yaml"));
        assert!(t.contains("BindsTo=sys-subsystem-net-devices-eth0.device"));
        assert!(t.contains("WantedBy=sys-subsystem-net-devices-eth0.device"));
    }
}
