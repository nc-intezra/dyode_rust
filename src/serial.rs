//! DYODE v2 ("light"): Modbus over a serial line through an optocoupler.
//!
//! lidi needs a UDP link, so the v2 hardware keeps DYODE's own framing:
//! one message per line, `<crc32 as 8 hex digits> <space> <JSON> <\n>`.
//! Compact JSON never contains a raw newline, so '\n' is a reliable
//! delimiter.  One thread owns the serial port and multiplexes every
//! Modbus module over it, tagged by module name.  Messages are not signed
//! on this link (as in the Python version): it is a point-to-point wire.

use crate::config::{Config, ModbusConfig, SerialConfig};
use crate::logging::FailureLog;
use crate::modbus::{Mirror, PlcPoller};
use serde_json::Value;
use std::collections::HashMap;
use std::ffi::CString;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub fn crc32(data: &[u8]) -> u32 {
    let mut h = crc32fast::Hasher::new();
    h.update(data);
    h.finalize()
}

pub fn encode(obj: &Value) -> Vec<u8> {
    let body = serde_json::to_vec(obj).expect("JSON");
    let mut out = format!("{:08x} ", crc32(&body)).into_bytes();
    out.extend_from_slice(&body);
    out.push(b'\n');
    out
}

/// Decode one line (with or without its newline); None if damaged.
pub fn decode(line: &[u8]) -> Option<Value> {
    let line = line.trim_ascii();
    if line.len() < 10 || line[8] != b' ' {
        return None;
    }
    let crc = u32::from_str_radix(std::str::from_utf8(&line[..8]).ok()?, 16).ok()?;
    let body = &line[9..];
    if crc32(body) != crc {
        return None;
    }
    serde_json::from_slice(body).ok()
}

/// Accumulate raw serial bytes and yield complete lines.
pub struct LineBuffer {
    buf: Vec<u8>,
    max_line: usize,
    pub overflows: u64,
}

impl LineBuffer {
    pub fn new(max_line: usize) -> Self {
        Self {
            buf: Vec::new(),
            max_line,
            overflows: 0,
        }
    }

    pub fn feed(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        self.buf.extend_from_slice(data);
        let mut lines = Vec::new();
        while let Some(i) = self.buf.iter().position(|&b| b == b'\n') {
            lines.push(self.buf[..i].to_vec());
            self.buf.drain(..=i);
        }
        if self.buf.len() > self.max_line {
            // Garbage with no newline: discard rather than grow forever.
            self.buf.clear();
            self.overflows += 1;
        }
        lines
    }
}

fn baud_constant(baud: u32) -> Option<libc::speed_t> {
    Some(match baud {
        1200 => libc::B1200,
        2400 => libc::B2400,
        4800 => libc::B4800,
        9600 => libc::B9600,
        19200 => libc::B19200,
        38400 => libc::B38400,
        57600 => libc::B57600,
        115_200 => libc::B115200,
        230_400 => libc::B230400,
        460_800 => libc::B460800,
        921_600 => libc::B921600,
        _ => return None,
    })
}

/// Open the port raw, 8N1, with a read timeout (tenths of a second).
pub fn open_port(cfg: &SerialConfig, timeout_ds: u8) -> Result<File, String> {
    let speed = baud_constant(cfg.baudrate)
        .ok_or_else(|| format!("unsupported baud rate {}", cfg.baudrate))?;
    let c = CString::new(cfg.device.as_bytes()).map_err(|_| "bad device name".to_string())?;
    // SAFETY: valid C string.
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(format!(
            "cannot open {}: {}",
            cfg.device,
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: fd is ours.
    let file = unsafe { File::from_raw_fd(fd) };
    let mut tio: libc::termios = unsafe { std::mem::zeroed() };
    // SAFETY: valid fd and termios pointer.
    unsafe {
        if libc::tcgetattr(fd, &mut tio) != 0 {
            return Err(format!(
                "{} is not a serial port: {}",
                cfg.device,
                std::io::Error::last_os_error()
            ));
        }
        libc::cfmakeraw(&mut tio);
        tio.c_cflag |= libc::CLOCAL | libc::CREAD;
        tio.c_cflag &= !(libc::CSTOPB | libc::PARENB | libc::CRTSCTS);
        tio.c_cc[libc::VMIN] = 0;
        tio.c_cc[libc::VTIME] = timeout_ds;
        libc::cfsetispeed(&mut tio, speed);
        libc::cfsetospeed(&mut tio, speed);
        if libc::tcsetattr(file.as_raw_fd(), libc::TCSANOW, &tio) != 0 {
            return Err(format!(
                "cannot configure {}: {}",
                cfg.device,
                std::io::Error::last_os_error()
            ));
        }
    }
    Ok(file)
}

/// v2 input: poll every Modbus module, write to the serial link.
pub fn run_input(modules: &[(String, ModbusConfig)], cfg: &Config) -> Result<(), String> {
    let mut pollers: Vec<PlcPoller> = modules.iter().map(|(n, c)| PlcPoller::new(n, c)).collect();
    let interval = modules
        .iter()
        .map(|(_, c)| c.interval)
        .fold(f64::INFINITY, f64::min);
    let serial = &cfg.serial;
    // 8N1 = 10 bits on the wire per byte.
    let budget = f64::from(serial.baudrate) / 10.0 * interval;
    let mut link = FailureLog::new(format!("serial port {}", serial.device));
    let mut statuses: HashMap<String, FailureLog> = pollers
        .iter()
        .map(|p| {
            (
                p.name.clone(),
                FailureLog::new(format!("module {:?}", p.name)),
            )
        })
        .collect();
    let mut port: Option<File> = None;
    log::info!(
        "polling {} module(s) every {interval}s, writing to {} @ {} baud",
        pollers.len(),
        serial.device,
        serial.baudrate
    );
    loop {
        let started = Instant::now();
        if port.is_none() {
            match open_port(serial, 5) {
                Ok(p) => {
                    link.ok();
                    port = Some(p);
                }
                Err(e) => link.fail(e),
            }
        }
        let mut sent = 0usize;
        for p in &mut pollers {
            let frame = match p.poll() {
                Ok(msg) => {
                    statuses.get_mut(&p.name).unwrap().ok();
                    encode(&msg)
                }
                Err(e) => {
                    statuses.get_mut(&p.name).unwrap().fail(e);
                    continue;
                }
            };
            let Some(f) = port.as_mut() else { continue };
            match f.write_all(&frame).and_then(|()| f.flush()) {
                Ok(()) => sent += frame.len(),
                Err(e) => {
                    link.fail(e);
                    port = None;
                }
            }
        }
        if sent as f64 > 0.8 * budget {
            log::warn!(
                "sending {sent} bytes per {interval}s uses >80% of the serial bandwidth; raise 'interval' or reduce ranges"
            );
        }
        std::thread::sleep(Duration::from_secs_f64(interval).saturating_sub(started.elapsed()));
    }
}

/// v2 output: one serial reader feeding every module's mirror.
pub fn run_reader(
    mirrors: &HashMap<String, Arc<Mirror>>,
    cfg: &SerialConfig,
) -> Result<(), String> {
    let mut link = FailureLog::new(format!("serial port {}", cfg.device));
    let mut buf = LineBuffer::new(4 * 1024 * 1024);
    loop {
        let mut port = match open_port(cfg, 2) {
            Ok(p) => {
                link.ok();
                p
            }
            Err(e) => {
                link.fail(e);
                std::thread::sleep(Duration::from_secs(5));
                continue;
            }
        };
        let mut chunk = [0u8; 4096];
        loop {
            let n = match port.read(&mut chunk) {
                Ok(n) => n,
                Err(e) => {
                    link.fail(e);
                    break;
                }
            };
            for line in buf.feed(&chunk[..n]) {
                let Some(msg) = decode(&line) else {
                    log::warn!("serial: damaged frame discarded ({} bytes)", line.len());
                    continue;
                };
                let name = msg.get("module").and_then(Value::as_str).unwrap_or("");
                match mirrors.get(name) {
                    Some(m) => {
                        m.apply(&msg, None);
                    }
                    None => log::warn!("serial: update for unknown module {name:?} ignored"),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn framing() {
        let msg = json!({"module": "a", "seq": 1});
        let f = encode(&msg);
        assert_eq!(decode(&f), Some(msg.clone()));
        let mut bad = f.clone();
        bad[12] ^= 1;
        assert_eq!(decode(&bad), None);
        let mut lb = LineBuffer::new(100);
        let mut joined = f.clone();
        joined.extend_from_slice(&f);
        let (a, b) = joined.split_at(15);
        assert!(lb.feed(a).is_empty());
        assert_eq!(lb.feed(b).len(), 2);
        assert!(lb.feed(&[b'x'; 200]).is_empty());
        assert_eq!(lb.overflows, 1);
    }

    #[test]
    fn python_crc_compat() {
        // zlib.crc32(b'{"a":1}') == 0x561bacaf in Python.
        assert_eq!(crc32(br#"{"a":1}"#), 0x561b_acaf);
    }
}
