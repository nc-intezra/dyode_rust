//! `dyode check`, as data: printed by the command and served by the API's
//! `/v1/check`.

use crate::config::{Config, Side};
use serde::Serialize;
use std::path::PathBuf;

#[derive(Debug, Default, Serialize)]
pub struct Report {
    /// Things that are fine and worth saying.
    pub notes: Vec<String>,
    /// DYODE will not work like this.
    pub errors: Vec<String>,
    /// Probably wrong, or worth improving.
    pub warnings: Vec<String>,
}

/// Check what can be checked without starting anything.  `side` limits the
/// unit and API checks to one box; `serial` means DYODE v2 (no key, no lidi).
pub fn report(cfg: &Config, side: Option<&str>, serial: bool) -> Report {
    let mut r = Report::default();
    unit_checks(cfg, side, serial, &mut r);
    if serial {
        return r;
    }
    match crate::auth::load_key(&cfg.auth) {
        Ok(Some(_)) => r
            .notes
            .push(format!("key: {} OK", cfg.auth.key_file.display())),
        Ok(None) => r
            .warnings
            .push("no key: auth.required is false, so nothing is authenticated".into()),
        Err(e) => r.errors.push(format!("key: {e}")),
    }
    if let Err(e) = crate::transport::new_receiver_config_check(cfg) {
        r.errors.push(format!("lidi: {e}"));
    }
    if side == Some("in") && cfg.network.out_mac.is_none() {
        r.warnings.insert(
            0,
            "dyode_out.mac is not set: the input box cannot reach the output box through a diode"
                .into(),
        );
    }
    r.warnings.extend(throughput_hints(cfg));
    api_checks(cfg, side, &mut r);
    r
}

fn sides(side: Option<&str>) -> Vec<Side> {
    match side {
        Some("in") => vec![Side::In],
        Some("out") => vec![Side::Out],
        _ => vec![Side::In, Side::Out],
    }
}

/// Compare the installed systemd unit(s) with config.yaml.
fn unit_checks(cfg: &Config, side: Option<&str>, serial: bool, r: &mut Report) {
    let dir = PathBuf::from(
        std::env::var("DYODE_SYSTEMD_DIR").unwrap_or_else(|_| "/etc/systemd/system".into()),
    );
    for s in sides(side) {
        let path = dir.join(crate::setup::unit_name(s));
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let found = crate::setup::check_unit(&text, s, cfg, serial);
                if found.is_empty() {
                    r.notes.push(format!("unit: {} matches", path.display()));
                }
                r.warnings.extend(found);
            }
            Err(_) if side.is_some() => r.warnings.push(format!(
                "{} is not installed: run install.sh (or dyode setup) to create it",
                path.display()
            )),
            Err(_) => {}
        }
    }
}

/// Suggestions for using the box's cores, never errors.
fn throughput_hints(cfg: &Config) -> Vec<String> {
    let mut w = Vec::new();
    let cpus = std::thread::available_parallelism().map_or(1, usize::from);
    let suggested = crate::wizard::model::suggested_ports(cpus);
    if cfg.lidi.ports.len() < suggested {
        let first = cfg.lidi.ports.first().copied().unwrap_or(5000);
        w.push(format!(
            "lidi uses {} port(s) on a {cpus}-CPU box: one RaptorQ thread per port.  For more throughput set \
             lidi.ports: [{}] -- in the config of BOTH boxes, and only if the other box has the cores too",
            cfg.lidi.ports.len(),
            crate::wizard::model::port_list(first, suggested)
        ));
    }
    if let Some(m) = cfg.lidi.max_clients {
        let need = cfg.concurrent_transfers() + 1;
        if m < need {
            w.push(format!(
                "lidi.max_clients is {m}, but the modules and the status channel can have {need} transfers \
                 in flight (parallel_files included): transfers will wait for each other.  Remove \
                 max_clients to get {} automatically",
                cfg.concurrent_transfers() + 3
            ));
        }
    }
    w
}

/// The `api:` section for the box(es) being checked.
fn api_checks(cfg: &Config, side: Option<&str>, r: &mut Report) {
    for s in sides(side) {
        let Some(a) = cfg.api.for_side(s) else {
            continue;
        };
        let name = s.as_str();
        if !cfg!(feature = "api") {
            r.errors.push(format!(
                "api.{name} is configured, but this dyode was built without the API"
            ));
        }
        match &a.tls {
            Some((cert, key)) => {
                for (what, p) in [("certificate", cert), ("key", key)] {
                    if !p.is_file() {
                        r.errors.push(format!(
                            "api.{name}: TLS {what} {} is missing (dyode api-cert makes one)",
                            p.display()
                        ));
                    }
                }
            }
            None => r
                .notes
                .push(format!("api.{name}: plain HTTP on {}", a.listen)),
        }
        match crate::apitoken::list(&a.tokens) {
            Ok(t) if t.is_empty() => r.warnings.push(format!(
                "api.{name}: no tokens in {}: only /v1/health answers (dyode api-token add NAME)",
                a.tokens.display()
            )),
            Ok(t) => r.notes.push(format!(
                "api.{name}: listens on {}, {} token(s)",
                a.listen,
                t.len()
            )),
            Err(e) => r.errors.push(format!("api.{name}: {e}")),
        }
    }
}
