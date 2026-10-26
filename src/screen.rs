//! Screen sharing (module type `screen`).
//!
//! Input side: watch a folder where screenshots (JPEG) are written -- by
//! screenshot.ps1, say -- and send the newest one, at most `max_fps` per
//! second, each frame one lidi transfer.
//! Output side: serve the latest frame over HTTP as an MJPEG stream
//! (`http://<it-address>:8080/`) and as a still (`/screen.jpg`).

use crate::config::{Module, Network, ScreenConfig};
use crate::envelope::{Body, Failure, Header, Inbound, Kind};
use crate::transport::ModuleLink;
use crate::watch::ChangeWatcher;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime};

pub const JPEG_MAGIC: &[u8] = b"\xff\xd8";
const BOUNDARY: &str = "jpgboundary";
const MAX_FRAME: u64 = 32 * 1024 * 1024;

const PAGE: &[u8] = b"<!DOCTYPE html>
<html><head><meta charset=\"utf-8\"><title>DYODE screen</title>
<style>body{margin:0;background:#111}img{display:block;max-width:100%;margin:auto}</style>
</head><body><img src=\"/screen.mjpg\" alt=\"screen\"></body></html>
";

/// (path, mtime, size) of the newest JPEG in `folder`, links skipped.
pub fn newest_image(folder: &Path) -> Option<(PathBuf, SystemTime, u64)> {
    let mut best: Option<(PathBuf, SystemTime, u64)> = None;
    for e in std::fs::read_dir(folder).ok()?.flatten() {
        let name = e.file_name().to_string_lossy().to_lowercase();
        if !(name.ends_with(".jpg") || name.ends_with(".jpeg")) {
            continue;
        }
        let Ok(md) = std::fs::symlink_metadata(e.path()) else {
            continue;
        };
        if !md.file_type().is_file() {
            continue;
        }
        let Ok(m) = md.modified() else { continue };
        if best.as_ref().is_none_or(|b| m > b.1) {
            best = Some((e.path(), m, md.len()));
        }
    }
    best
}

/// Input agent: send each new screenshot, at most max_fps per second.
pub fn run_input(module: &Module, cfg: &ScreenConfig, link: &ModuleLink) -> Result<(), String> {
    std::fs::create_dir_all(&cfg.input)
        .map_err(|e| format!("cannot create {}: {e}", cfg.input.display()))?;
    let watcher = ChangeWatcher::new(&cfg.input, false);
    let min_gap = Duration::from_secs_f64(1.0 / cfg.max_fps);
    let mut last: Option<(SystemTime, u64)> = None;
    log::info!(
        "module {:?}: watching {} for screenshots (endpoint {})",
        module.name,
        cfg.input.display(),
        module.index
    );
    loop {
        watcher.wait(Duration::from_secs(1));
        let Some((path, mtime, size)) = newest_image(&cfg.input) else {
            continue;
        };
        if last == Some((mtime, size)) {
            continue;
        }
        let data = match crate::fsafe::open_ready_file(
            path.parent().unwrap_or(Path::new(".")),
            &path.file_name().unwrap().to_string_lossy(),
        ) {
            Some((mut f, _, _)) => {
                let mut v = Vec::new();
                match std::io::Read::read_to_end(&mut f, &mut v) {
                    Ok(_) => v,
                    Err(e) => {
                        log::debug!("cannot read {}: {e}", path.display());
                        continue;
                    }
                }
            }
            None => continue,
        };
        if !data.starts_with(JPEG_MAGIC) {
            log::debug!("{} is not a complete JPEG yet; skipped", path.display());
            continue;
        }
        if let Err(e) = link.send_message(b"", &data) {
            log::warn!("module {:?}: {e}", module.name);
        }
        last = Some((mtime, size));
        std::thread::sleep(min_gap);
    }
}

/// Latest frame, shared between the lidi receiver and HTTP viewers.
#[derive(Default)]
pub struct FrameStore {
    inner: Mutex<(u64, Option<Arc<Vec<u8>>>)>,
    cond: Condvar,
}

impl FrameStore {
    pub fn put(&self, frame: Vec<u8>) {
        let mut g = self.inner.lock().unwrap();
        g.0 += 1;
        g.1 = Some(Arc::new(frame));
        self.cond.notify_all();
    }

    /// A frame newer than `seen`, or None after `timeout`.
    pub fn wait_newer(&self, seen: u64, timeout: Duration) -> (u64, Option<Arc<Vec<u8>>>) {
        let g = self.inner.lock().unwrap();
        let (g, _) = self
            .cond
            .wait_timeout_while(g, timeout, |g| g.0 == seen)
            .unwrap();
        if g.0 == seen {
            (seen, None)
        } else {
            (g.0, g.1.clone())
        }
    }
}

pub struct ScreenInbound {
    pub name: String,
    pub store: Arc<FrameStore>,
    refusals: crate::logging::RefusalLog,
}

impl ScreenInbound {
    pub fn new(name: &str, store: Arc<FrameStore>) -> Self {
        Self {
            name: name.to_string(),
            store,
            refusals: crate::logging::RefusalLog::new(name),
        }
    }
}

impl Inbound for ScreenInbound {
    fn module_name(&self) -> &str {
        &self.name
    }

    fn open_body(&self, hdr: &Header) -> Result<Body, String> {
        if hdr.kind != Kind::Message || hdr.body_len > MAX_FRAME {
            return Err("not a screen frame".into());
        }
        Ok(Body::Memory(Vec::with_capacity(hdr.body_len as usize)))
    }

    fn deliver(&self, _hdr: Header, body: Body, _digest: [u8; 32]) {
        let Body::Memory(b) = body else { return };
        if b.starts_with(JPEG_MAGIC) {
            self.store.put(b);
        } else {
            log::warn!(
                "module {:?}: received a frame that is not a JPEG; ignored",
                self.name
            );
        }
    }

    fn failed(&self, _hdr: Option<&Header>, _body: Option<Body>, why: Failure) {
        match why {
            Failure::Incomplete { .. } => {
                log::debug!("module {:?}: a frame was lost in the diode", self.name)
            }
            other => self.refusals.refused(&other),
        }
    }
}

fn respond(s: &mut TcpStream, code: &str, ctype: &str, body: &[u8]) -> std::io::Result<()> {
    write!(
        s,
        "HTTP/1.1 {code}\r\nServer: DYODE\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    s.write_all(body)
}

fn serve_viewer(
    mut s: TcpStream,
    store: &FrameStore,
    streams: &AtomicUsize,
    max_streams: usize,
) -> std::io::Result<()> {
    s.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut line = String::new();
    {
        let mut r = BufReader::new(&s);
        r.read_line(&mut line)?;
        // Skip the headers.
        let mut h = String::new();
        while r.read_line(&mut h)? > 2 {
            h.clear();
        }
    }
    let mut parts = line.split_whitespace();
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    if method != "GET" {
        return respond(
            &mut s,
            "405 Method Not Allowed",
            "text/plain",
            b"GET only\n",
        );
    }
    let path = target.split('?').next().unwrap_or("");
    match path {
        "/" | "/index.html" => respond(&mut s, "200 OK", "text/html; charset=utf-8", PAGE),
        "/screen.jpg" => match store.wait_newer(u64::MAX, Duration::ZERO).1 {
            Some(f) => respond(&mut s, "200 OK", "image/jpeg", &f),
            None => respond(
                &mut s,
                "503 Service Unavailable",
                "text/plain",
                b"no frame received yet\n",
            ),
        },
        p if p.ends_with(".mjpg") => {
            // Each viewer holds a thread for as long as it watches: cap them.
            if streams.fetch_add(1, Ordering::SeqCst) >= max_streams {
                streams.fetch_sub(1, Ordering::SeqCst);
                log::warn!(
                    "viewer {:?} refused: {max_streams} streams already open (max_streams)",
                    s.peer_addr().ok()
                );
                return respond(
                    &mut s,
                    "503 Service Unavailable",
                    "text/plain",
                    b"too many viewers\n",
                );
            }
            let res = (|| -> std::io::Result<()> {
                write!(
                    s,
                    "HTTP/1.1 200 OK\r\nServer: DYODE\r\nContent-Type: multipart/x-mixed-replace; boundary={BOUNDARY}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n"
                )?;
                let mut seen = u64::MAX;
                loop {
                    let (id, frame) = store.wait_newer(seen, Duration::from_secs(5));
                    seen = id;
                    let Some(frame) = frame else { continue };
                    write!(
                        s,
                        "--{BOUNDARY}\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
                        frame.len()
                    )?;
                    s.write_all(&frame)?;
                    s.write_all(b"\r\n")?;
                    s.flush()?;
                }
            })();
            streams.fetch_sub(1, Ordering::SeqCst);
            res
        }
        _ => respond(&mut s, "404 Not Found", "text/plain", b"not found\n"),
    }
}

/// Output agent: the viewer page.  Runs forever.
pub fn run_http(
    module: &Module,
    cfg: &ScreenConfig,
    net: &Network,
    store: Arc<FrameStore>,
) -> Result<(), String> {
    let bind = cfg
        .http_bind
        .unwrap_or_else(|| crate::netutil::default_it_address(net));
    let listener = TcpListener::bind((bind, cfg.http_port))
        .map_err(|e| format!("cannot listen on {bind}:{}: {e}", cfg.http_port))?;
    log::info!(
        "module {:?}: screen available at http://{bind}:{}/",
        module.name,
        cfg.http_port
    );
    let streams = Arc::new(AtomicUsize::new(0));
    // Viewers that are not streaming also get a thread; bound those too.
    let busy = Arc::new(AtomicUsize::new(0));
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        if busy.load(Ordering::Relaxed) >= cfg.max_streams + 16 {
            continue;
        }
        busy.fetch_add(1, Ordering::Relaxed);
        let (store, streams, held, max) = (
            Arc::clone(&store),
            Arc::clone(&streams),
            Arc::clone(&busy),
            cfg.max_streams,
        );
        let ok = std::thread::Builder::new()
            .name("screen-http".into())
            .spawn(move || {
                if let Err(e) = serve_viewer(conn, &store, &streams, max) {
                    log::debug!("viewer disconnected: {e}");
                }
                held.fetch_sub(1, Ordering::Relaxed);
            });
        if ok.is_err() {
            busy.fetch_sub(1, Ordering::Relaxed);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newest_jpeg_wins() {
        let d = crate::testutil::TempDir::new();
        std::fs::write(d.path().join("a.jpg"), b"\xff\xd8a").unwrap();
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(d.path().join("b.JPEG"), b"\xff\xd8b").unwrap();
        std::fs::write(d.path().join("c.png"), b"x").unwrap();
        let (p, _, _) = newest_image(d.path()).unwrap();
        assert!(p.ends_with("b.JPEG"));
    }

    #[test]
    fn frame_store_wait() {
        let s = FrameStore::default();
        assert!(s.wait_newer(u64::MAX, Duration::ZERO).1.is_none());
        s.put(b"\xff\xd8x".to_vec());
        let (id, f) = s.wait_newer(u64::MAX, Duration::ZERO);
        assert_eq!(f.unwrap().as_slice(), b"\xff\xd8x");
        assert!(s.wait_newer(id, Duration::from_millis(10)).1.is_none());
    }
}
