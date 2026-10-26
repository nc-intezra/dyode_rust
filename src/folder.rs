//! Folder transfers (module type `folder`).
//!
//! Input side: watch `in`, and send every file once it has settled, each
//! as its own lidi transfer.  The file is opened once without following
//! links, and hashed while it is sent, so the bytes signed are the bytes
//! read.  Sent files are retired to `.dyode_sent` for `keep_sent_hours`,
//! because a diode gives no proof of delivery.
//!
//! Output side: each transfer streams into the staging folder (never into
//! `out`, where file shares would hand out partial files).  When the
//! envelope's trailer proves the file complete and genuine, it is moved
//! into `out` without following links.
//!
//! After each batch the input side sends a signed summary listing the
//! files and their SHA-256.  The output side matches it against what it
//! stored and, after `batch_timeout` with nothing arriving, names every
//! file that never came -- the same `file_missing` / `batch_received`
//! records the Python version wrote, so its log tooling still works.
//!
//! What changed from udpcast: files no longer need a manifest sent ahead,
//! a gap between files, or an "autostart" grace period, and several
//! modules can transfer at once.  What did not change: there is no return
//! path, so a file lidi cannot rebuild is reported and must be re-sent (by
//! copying it back from `.dyode_sent`).

use crate::config::{FolderConfig, Module};
use crate::envelope::{Body, Failure, Header, Inbound, Kind};
use crate::fsafe::{self, PlaceError};
use crate::hex;
use crate::logging::Event;
use crate::transport::ModuleLink;
use crate::watch::ChangeWatcher;
use serde_json::{Value, json};
use std::collections::{HashSet, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

/// Files per batch summary; the rest wait for the next pass.
pub const MAX_BATCH_FILES: usize = 256;
pub const SENT_DIR: &str = ".dyode_sent";
const MAX_SUMMARY_BYTES: u64 = 16 * 1024 * 1024;

fn new_batch_id() -> String {
    let mut b = [0u8; 16];
    crate::auth::random_bytes(&mut b);
    hex::encode(b)
}

fn short(id: &str) -> &str {
    &id[..id.len().min(8)]
}

// --------------------------------------------------------------------------
// Input side
// --------------------------------------------------------------------------

/// Move a file out of the watched folder once it has been transmitted.
/// Keeping it under .dyode_sent makes a failed transfer recoverable: copy
/// it back into the watched folder and it is sent again.
pub fn retire_sent_file(root: &Path, rel: &str, path: &Path, keep_sent_hours: u64) {
    if keep_sent_hours == 0 {
        if let Err(e) = fs::remove_file(path) {
            log::error!("sent {rel} but could not delete it: {e}");
        }
        return;
    }
    let dest = root.join(SENT_DIR).join(rel);
    let res = dest
        .parent()
        .map_or(Ok(()), fs::create_dir_all)
        .and_then(|()| fs::rename(path, &dest));
    if let Err(e) = res {
        log::error!("sent {rel} but could not retire it to {SENT_DIR}: {e}");
        let _ = fs::remove_file(path);
    }
}

/// Delete retired files older than keep_sent_hours.
pub fn prune_sent(root: &Path, keep_sent_hours: u64) {
    if keep_sent_hours == 0 {
        return;
    }
    let base = root.join(SENT_DIR);
    let cutoff = SystemTime::now() - Duration::from_secs(keep_sent_hours * 3600);
    fn walk(dir: &Path, base: &Path, cutoff: SystemTime) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(md) = fs::symlink_metadata(&p) else {
                continue;
            };
            if md.is_dir() {
                walk(&p, base, cutoff);
            } else if md.modified().map(|m| m < cutoff).unwrap_or(false) {
                let _ = fs::remove_file(&p);
            }
        }
        if dir != base {
            let _ = fs::remove_dir(dir); // only succeeds when empty
        }
    }
    if base.is_dir() {
        walk(&base, &base, cutoff);
    }
}

/// Send one batch.  Returns the number of files sent.
pub fn send_batch(
    module: &str,
    fc: &FolderConfig,
    link: &ModuleLink,
    files: &[(String, PathBuf)],
) -> usize {
    let batch = new_batch_id();
    let started = Instant::now();
    let mut entries = Vec::new();
    let mut sent_bytes = 0u64;
    let mut failed = 0usize;
    let todo = &files[..files.len().min(MAX_BATCH_FILES)];
    for (rel, path) in todo {
        let Some((file, size, mtime)) = fsafe::open_ready_file(&fc.input, rel) else {
            log::warn!(
                "skipped {rel}: not a regular file any more (replaced by a link, or removed since the scan)"
            );
            Event::new("file_skipped")
                .module(module)
                .per_file()
                .field("path", rel.as_str())
                .field("reason", "not_regular_file")
                .emit();
            continue;
        };
        let mtime = mtime
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0.0, |d| d.as_secs_f64());
        let meta = json!({"path": rel, "size": size, "mtime": mtime, "batch": batch});
        log::info!("sending {rel} ({size} bytes)");
        let outcome = link
            .send_file(meta.to_string().as_bytes(), file, size)
            .and_then(|rx| {
                rx.recv()
                    .map_err(|_| "lidi dropped the transfer".to_string())
                    .and_then(|r| r)
            });
        match outcome {
            Ok(digest) => {
                retire_sent_file(&fc.input, rel, path, fc.keep_sent_hours);
                sent_bytes += size;
                let digest = hex::encode(digest);
                Event::new("file_sent")
                    .module(module)
                    .per_file()
                    .field("batch", short(&batch))
                    .field("path", rel.as_str())
                    .field("bytes", size)
                    .field("sha256", digest.as_str())
                    .emit();
                entries.push(json!({"path": rel, "size": size, "sha256": digest}));
            }
            Err(e) => {
                failed = todo.len() - entries.len();
                log::error!(
                    "sending {rel} failed ({e}); aborting batch, {failed} file(s) left for the next pass"
                );
                Event::new("file_failed")
                    .module(module)
                    .per_file()
                    .field("path", rel.as_str())
                    .field("reason", e)
                    .emit();
                break;
            }
        }
    }
    if entries.is_empty() {
        return 0;
    }
    let summary = json!({"magic": "dyode-batch-v2", "batch": batch, "files": entries});
    if let Err(e) = link.send_message(br#"{"kind":"batch"}"#, summary.to_string().as_bytes()) {
        log::error!("could not send the summary of batch {}: {e}", short(&batch));
    }
    let elapsed = started.elapsed().as_secs_f64();
    let mbps = if elapsed > 0.0 {
        Some((sent_bytes as f64 * 8.0 / elapsed / 1e6 * 10.0).round() / 10.0)
    } else {
        None
    };
    Event::new("batch_sent")
        .module(module)
        .field("batch", short(&batch))
        .field("files", entries.len())
        .field("files_failed", failed)
        .field("bytes", sent_bytes)
        .field("duration_s", (elapsed * 1000.0).round() / 1000.0)
        .opt_field("mbps", mbps)
        .emit();
    entries.len()
}

/// Input agent: watch `in` and send files as they settle.  Runs forever.
pub fn run_input(module: &Module, fc: &FolderConfig, link: &ModuleLink) -> Result<(), String> {
    fs::create_dir_all(&fc.input)
        .map_err(|e| format!("cannot create {}: {e}", fc.input.display()))?;
    if !fc.ignored.is_empty() {
        log::info!(
            "module {:?}: ignoring udpcast-only setting(s) {} (lidi needs none of them)",
            module.name,
            fc.ignored.join(", ")
        );
    }
    let watcher = ChangeWatcher::new(&fc.input, true);
    log::info!(
        "module {:?}: watching {} (endpoint {}, {}, sent files kept {})",
        module.name,
        fc.input.display(),
        module.index,
        fc.bitrate
            .map_or_else(|| "no rate cap".to_string(), |b| format!("{b} Mbit/s")),
        if fc.keep_sent_hours > 0 {
            format!("{}h", fc.keep_sent_hours)
        } else {
            "not at all".into()
        }
    );
    loop {
        // Files already there are picked up at start-up too.
        let (ready, waiting) = fsafe::scan_ready_files(&fc.input, fc.settle);
        if !ready.is_empty() {
            if send_batch(&module.name, fc, link, &ready) == 0 {
                // Nothing went out (lidi down, files vanished): do not spin.
                std::thread::sleep(Duration::from_secs(1));
            }
            continue;
        }
        prune_sent(&fc.input, fc.keep_sent_hours);
        let wait = if waiting > 0 {
            fc.settle.max(0.2)
        } else {
            fc.rescan
        };
        watcher.wait(Duration::from_secs_f64(wait));
    }
}

// --------------------------------------------------------------------------
// Output side
// --------------------------------------------------------------------------

/// Where incoming transfers are written before they are verified.  Never
/// inside `out`; by default a sibling, so the final rename stays atomic.
pub fn staging_dir(fc: &FolderConfig) -> PathBuf {
    if let Some(s) = &fc.staging {
        return s.clone();
    }
    let out = fc.output.to_string_lossy();
    PathBuf::from(format!("{}.incoming", out.trim_end_matches('/')))
}

struct Entry {
    path: String,
    size: u64,
    sha256: String,
}

struct Batch {
    id: String,
    started: Instant,
    expected: usize,
    stored: usize,
    bytes: u64,
    outstanding: Vec<Entry>,
}

struct State {
    batches: VecDeque<Batch>,
    /// (path, sha256) stored recently, for summaries that arrive after
    /// their files (lidi delivers transfers concurrently).
    recent: VecDeque<(String, String, u64, Instant)>,
    seen_batches: VecDeque<String>,
    last_activity: Instant,
}

const RETAIN_BATCHES: usize = 16;
const REMEMBER: usize = 4096;
const RECENT_FOR: Duration = Duration::from_secs(3600);

pub struct FolderInbound {
    name: String,
    out_dir: PathBuf,
    staging: PathBuf,
    batch_timeout: f64,
    state: Mutex<State>,
}

impl FolderInbound {
    pub fn new(module: &Module, fc: &FolderConfig) -> Result<Self, String> {
        let staging = staging_dir(fc);
        fs::create_dir_all(&fc.output)
            .map_err(|e| format!("cannot create {}: {e}", fc.output.display()))?;
        fs::create_dir_all(&staging)
            .map_err(|e| format!("cannot create {}: {e}", staging.display()))?;
        if crate::config::path_within(&staging, &fc.output) {
            log::warn!(
                "staging folder {} is inside the output folder: anything serving {} will see partial files",
                staging.display(),
                fc.output.display()
            );
        }
        // Clear leftovers from a previous run.
        if let Ok(rd) = fs::read_dir(&staging) {
            for e in rd.flatten() {
                if e.file_name().to_string_lossy().starts_with(".dyode-") {
                    let _ = fs::remove_file(e.path());
                }
            }
        }
        if !fc.ignored.is_empty() {
            log::info!(
                "module {:?}: ignoring udpcast-only setting(s) {}",
                module.name,
                fc.ignored.join(", ")
            );
        }
        log::info!(
            "module {:?}: receiving into {} via {} (endpoint {})",
            module.name,
            fc.output.display(),
            staging.display(),
            module.index
        );
        Ok(Self {
            name: module.name.clone(),
            out_dir: fc.output.clone(),
            staging,
            batch_timeout: fc.batch_timeout,
            state: Mutex::new(State {
                batches: VecDeque::new(),
                recent: VecDeque::new(),
                seen_batches: VecDeque::new(),
                last_activity: Instant::now(),
            }),
        })
    }

    fn summarize(&self, b: &Batch, reason: &str) {
        for e in &b.outstanding {
            log::warn!(
                "file {} never arrived (batch {}, {reason})",
                e.path,
                short(&b.id)
            );
            Event::new("file_missing")
                .module(&self.name)
                .per_file()
                .field("batch", short(&b.id))
                .field("path", e.path.as_str())
                .field("bytes", e.size)
                .field("reason", reason)
                .emit();
        }
        Event::new("batch_received")
            .module(&self.name)
            .field("batch", short(&b.id))
            .field("files", b.expected)
            .field("files_stored", b.stored)
            .field("files_rejected", 0)
            .field("files_missing", b.outstanding.len())
            .field("bytes", b.bytes)
            .field("reason", reason)
            .field(
                "duration_s",
                (b.started.elapsed().as_secs_f64() * 1000.0).round() / 1000.0,
            )
            .emit();
        if !b.outstanding.is_empty() {
            log::warn!(
                "batch {} closed with {} file(s) never received ({reason})",
                short(&b.id),
                b.outstanding.len()
            );
        }
    }

    /// Newest write time of anything in the staging folder: a transfer in
    /// progress keeps every open batch alive however long it takes.
    fn staging_busy(&self) -> bool {
        let Ok(rd) = fs::read_dir(&self.staging) else {
            return false;
        };
        let limit = Duration::from_secs_f64(self.batch_timeout.max(1.0));
        rd.flatten().any(|e| {
            e.metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|m| m.elapsed().ok())
                .is_some_and(|age| age < limit)
        })
    }

    /// Close batches that have gone quiet.  Run periodically.
    pub fn sweep(&self) {
        if self.batch_timeout <= 0.0 {
            return;
        }
        let busy = self.staging_busy();
        let mut st = self.state.lock().unwrap();
        if busy {
            st.last_activity = Instant::now();
            return;
        }
        if st.last_activity.elapsed().as_secs_f64() < self.batch_timeout {
            return;
        }
        while let Some(b) = st.batches.pop_front() {
            self.summarize(&b, "timeout");
        }
        let now = Instant::now();
        while st
            .recent
            .front()
            .is_some_and(|r| now.duration_since(r.3) > RECENT_FOR)
        {
            st.recent.pop_front();
        }
    }

    /// Run the sweeper for ever.
    pub fn run_sweeper(&self) -> Result<(), String> {
        let every = Duration::from_secs_f64(self.batch_timeout.clamp(1.0, 30.0));
        loop {
            std::thread::sleep(every);
            self.sweep();
        }
    }

    fn record_stored(&self, path: &str, sha: &str, size: u64) {
        let mut st = self.state.lock().unwrap();
        st.last_activity = Instant::now();
        // Claim an outstanding entry in an open batch, oldest first.
        let mut done = None;
        for (i, b) in st.batches.iter_mut().enumerate() {
            if let Some(pos) = b
                .outstanding
                .iter()
                .position(|e| e.path == path && e.sha256 == sha)
            {
                b.outstanding.remove(pos);
                b.stored += 1;
                b.bytes += size;
                if b.outstanding.is_empty() {
                    done = Some(i);
                }
                break;
            }
        }
        if let Some(i) = done {
            let b = st.batches.remove(i).unwrap();
            self.summarize(&b, "complete");
            return;
        }
        st.recent
            .push_back((path.to_string(), sha.to_string(), size, Instant::now()));
        while st.recent.len() > REMEMBER {
            st.recent.pop_front();
        }
    }

    fn handle_summary(&self, body: &[u8]) {
        let Ok(v) = serde_json::from_slice::<Value>(body) else {
            log::error!("module {:?}: undecodable batch summary", self.name);
            return;
        };
        let (Some(id), Some(files)) = (
            v.get("batch").and_then(Value::as_str),
            v.get("files").and_then(Value::as_array),
        ) else {
            log::error!("module {:?}: malformed batch summary", self.name);
            return;
        };
        let mut st = self.state.lock().unwrap();
        st.last_activity = Instant::now();
        if st.seen_batches.iter().any(|s| s == id) {
            log::debug!("batch {} summary seen before; ignored", short(id));
            return;
        }
        st.seen_batches.push_back(id.to_string());
        while st.seen_batches.len() > REMEMBER {
            st.seen_batches.pop_front();
        }
        let mut b = Batch {
            id: id.to_string(),
            started: Instant::now(),
            expected: files.len(),
            stored: 0,
            bytes: 0,
            outstanding: Vec::new(),
        };
        for f in files {
            let (Some(path), Some(size), Some(sha)) = (
                f.get("path").and_then(Value::as_str),
                f.get("size").and_then(Value::as_u64),
                f.get("sha256").and_then(Value::as_str),
            ) else {
                continue;
            };
            if let Some(pos) = st.recent.iter().position(|r| r.0 == path && r.1 == sha) {
                let r = st.recent.remove(pos).unwrap();
                b.stored += 1;
                b.bytes += r.2;
            } else {
                b.outstanding.push(Entry {
                    path: path.into(),
                    size,
                    sha256: sha.into(),
                });
            }
        }
        log::info!(
            "batch {} announced: {} file(s), {} already stored",
            short(id),
            b.expected,
            b.stored
        );
        Event::new("manifest_received")
            .module(&self.name)
            .field("batch", short(id))
            .field("files", b.expected)
            .emit();
        // A re-sent file supersedes the outstanding entry in older batches.
        let paths: HashSet<String> = files
            .iter()
            .filter_map(|f| f.get("path").and_then(Value::as_str))
            .map(String::from)
            .collect();
        let mut closed = Vec::new();
        for (i, older) in st.batches.iter_mut().enumerate() {
            older.outstanding.retain(|e| !paths.contains(&e.path));
            if older.outstanding.is_empty() {
                closed.push(i);
            }
        }
        for i in closed.into_iter().rev() {
            let o = st.batches.remove(i).unwrap();
            self.summarize(&o, "superseded");
        }
        if b.outstanding.is_empty() {
            self.summarize(&b, "complete");
        } else {
            st.batches.push_back(b);
            while st.batches.len() > RETAIN_BATCHES {
                let o = st.batches.pop_front().unwrap();
                self.summarize(&o, "evicted");
            }
        }
    }

    fn store(&self, hdr: &Header, file: fs::File, staged: &Path, digest: &[u8; 32]) {
        let meta = hdr.meta_json().unwrap_or(Value::Null);
        let path = meta
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let mtime = meta.get("mtime").and_then(Value::as_f64);
        let size = hdr.body_len;
        let reject = |reason: &str, msg: String| {
            log::error!("{msg}");
            Event::new("file_rejected")
                .module(&self.name)
                .per_file()
                .field("path", path.as_str())
                .field("reason", reason)
                .field("bytes", size)
                .emit();
            let _ = fs::remove_file(staged);
        };
        if fsafe::safe_parts(&path).is_none() {
            return reject(
                "unsafe_path",
                format!("unsafe path {path:?} in a signed transfer; file discarded"),
            );
        }
        if let Some(m) = mtime {
            fsafe::set_mtime(&file, m);
        }
        drop(file);
        match fsafe::place_file(&self.out_dir, &path, staged) {
            Ok(dest) => {
                let sha = hex::encode(digest);
                log::info!("file {path} available at {}", dest.display());
                Event::new("file_stored")
                    .module(&self.name)
                    .per_file()
                    .opt_field(
                        "batch",
                        meta.get("batch")
                            .and_then(Value::as_str)
                            .map(|b| short(b).to_string()),
                    )
                    .field("path", path.as_str())
                    .field("bytes", size)
                    .field("sha256", sha.as_str())
                    .emit();
                self.record_stored(&path, &sha, size);
            }
            Err(PlaceError::Unsafe) => reject(
                "unsafe_destination",
                format!(
                    "refused to store {path:?}: a folder on its path is a symlink or not a folder"
                ),
            ),
            Err(PlaceError::Io(e)) => reject("io_error", format!("could not store {path:?}: {e}")),
        }
    }
}

impl Inbound for FolderInbound {
    fn module_name(&self) -> &str {
        &self.name
    }

    fn open_body(&self, hdr: &Header) -> Result<Body, String> {
        self.state.lock().unwrap().last_activity = Instant::now();
        match hdr.kind {
            Kind::Message => {
                if hdr.body_len > MAX_SUMMARY_BYTES {
                    return Err(format!("batch summary too large ({} bytes)", hdr.body_len));
                }
                Ok(Body::Memory(Vec::new()))
            }
            Kind::File => {
                let name = format!(
                    ".dyode-{:08x}{:08x}",
                    crate::auth::random_u32(),
                    crate::auth::random_u32()
                );
                let path = self.staging.join(name);
                let file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(&path)
                    .map_err(|e| {
                        format!(
                            "cannot create a staging file in {}: {e}",
                            self.staging.display()
                        )
                    })?;
                Ok(Body::File { file, path })
            }
        }
    }

    fn deliver(&self, hdr: Header, body: Body, digest: [u8; 32]) {
        match body {
            Body::Memory(b) => self.handle_summary(&b),
            Body::File { mut file, path } => {
                let _ = file.flush();
                self.store(&hdr, file, &path, &digest);
            }
        }
    }

    fn failed(&self, hdr: Option<&Header>, body: Option<Body>, why: Failure) {
        if let Some(Body::File { path, .. }) = &body {
            let _ = fs::remove_file(path);
        }
        // The header is not authenticated: the name is a hint only.
        let hinted = hdr
            .filter(|h| h.kind == Kind::File)
            .and_then(Header::meta_json)
            .and_then(|m| m.get("path").and_then(Value::as_str).map(String::from));
        let what = match (&hinted, hdr.map(|h| h.kind)) {
            (Some(p), _) => format!("file {p:?} (unverified name)"),
            (None, Some(Kind::Message)) => "a batch summary".to_string(),
            _ => "a transfer".to_string(),
        };
        log::error!("module {:?}: {what} was not received: {why}", self.name);
        let reason = match &why {
            Failure::Incomplete { .. } => "incomplete",
            Failure::Refused(crate::auth::Refusal::BadSignature) => "bad_signature",
            Failure::Refused(crate::auth::Refusal::Replayed) => "replayed",
            Failure::Malformed(_) => "malformed",
            Failure::Rejected(_) => "rejected",
        };
        Event::new("file_rejected")
            .module(&self.name)
            .per_file()
            .opt_field("path_unverified", hinted)
            .field("reason", reason)
            .field("detail", why.to_string())
            .emit();
    }
}
