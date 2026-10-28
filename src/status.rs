//! The status channel: every `lidi.heartbeat` seconds each box writes its
//! status to /run/dyode, and the input box also sends it through the diode
//! on its own lidi endpoint (the one after the last module), signed like
//! everything else.  The output box keeps the last one it received, so its
//! API can serve both boxes while nothing on the IT side ever reaches the
//! input box.

use crate::config::Config;
use crate::envelope::{Body, Failure, Header, Inbound, Kind};
use crate::transport::ModuleLink;
use std::time::Duration;

/// A status message is a few KB; refuse anything absurd before reading it.
const MAX_STATUS: u64 = 1024 * 1024;

/// How often status is taken and sent, and when the input box counts as
/// silent on the output box.
pub fn interval(cfg: &Config) -> f64 {
    (cfg.lidi.heartbeat as f64).clamp(1.0, 60.0)
}

pub fn stale_after(cfg: &Config) -> f64 {
    interval(cfg) * 3.0
}

/// Input box: write the status file and send the status through the diode,
/// for ever.  Send errors are logged once and retried at the next tick.
pub fn run_reporter(cfg: &Config, link: Option<&ModuleLink>) -> Result<(), String> {
    let every = Duration::from_secs_f64(interval(cfg));
    let stale = stale_after(cfg);
    let side = if link.is_some() { "in" } else { "out" };
    let mut file_err = crate::logging::FailureLog::new(format!(
        "writing {}",
        crate::stats::status_path(side).display()
    ));
    let mut send_err = crate::logging::FailureLog::new("sending status through the diode");
    loop {
        match crate::stats::write_status_file(side, stale) {
            Ok(()) => file_err.ok(),
            Err(e) => file_err.fail(e),
        }
        if let Some(link) = link {
            let body = serde_json::to_vec(&crate::stats::snapshot()).unwrap_or_default();
            match link.send_message(br#"{"kind":"status"}"#, &body) {
                Ok(()) => send_err.ok(),
                Err(e) => send_err.fail(e),
            }
        }
        std::thread::sleep(every);
    }
}

/// Output box: the receiving end of the status channel.
pub struct StatusInbound(crate::logging::RefusalLog);

impl StatusInbound {
    pub fn new() -> Self {
        Self(crate::logging::RefusalLog::new("dyode-status"))
    }
}

impl Default for StatusInbound {
    fn default() -> Self {
        Self::new()
    }
}

impl Inbound for StatusInbound {
    fn module_name(&self) -> &str {
        "dyode-status"
    }

    fn open_body(&self, hdr: &Header) -> Result<Body, String> {
        if hdr.kind != Kind::Message || hdr.body_len > MAX_STATUS {
            return Err("not a status message".into());
        }
        Ok(Body::Memory(Vec::with_capacity(hdr.body_len as usize)))
    }

    fn deliver(&self, _hdr: Header, body: Body, _digest: [u8; 32]) {
        let Body::Memory(b) = body else { return };
        match serde_json::from_slice::<serde_json::Value>(&b) {
            Ok(v) if v["side"] == "in" => crate::stats::set_peer(v),
            Ok(_) => log::warn!("status message from something that is not an input box; ignored"),
            Err(_) => log::warn!("undecodable status message; ignored"),
        }
    }

    fn failed(&self, _hdr: Option<&Header>, _body: Option<Body>, why: Failure) {
        match why {
            // The next one comes in a few seconds.
            Failure::Incomplete { .. } => log::debug!("a status message was lost in the diode"),
            other => self.0.refused(&other),
        }
    }
}
