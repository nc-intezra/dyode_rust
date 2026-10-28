//! DYODE: a low-cost DIY data diode for ICS -- Rust port, with ANSSI's
//! lidi (RaptorQ forward error correction) as the diode transport.
//!
//! The input box runs `dyode in`, the output box `dyode out`; both read the
//! same config.yaml.  See README.md and docs/ARCHITECTURE.md.

pub mod auth;
pub mod config;
pub mod envelope;
pub mod folder;
pub mod fsafe;
pub mod hex;
pub mod logging;
pub mod logs_tool;
pub mod logstream;
pub mod modbus;
pub mod netutil;
pub mod probe;
pub mod ratelimit;
pub mod runtime;
pub mod screen;
pub mod serial;
pub mod setup;
pub mod supervisor;
pub mod timefmt;
pub mod transport;
pub mod tui;
pub mod watch;
pub mod wizard;

#[doc(hidden)]
pub mod testutil;
