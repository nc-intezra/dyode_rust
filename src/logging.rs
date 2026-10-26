//! Logging: a human log and a JSON event log.
//!
//! * stderr, always, so `journalctl -u dyode-in` still shows everything;
//! * `<dir>/dyode.log`, human-readable, every module;
//! * `<dir>/transfer.jsonl`, one JSON object per line (schema v1, the same
//!   records the Python version wrote), for transfers and log-stream stats.
//!
//! Rotation is logrotate's job.  Both files are re-opened when logrotate
//! moves them away (the same idea as Python's WatchedFileHandler): the path
//! is re-checked at most once a second and re-opened when its inode changed.
//!
//! The embedded lidi engines log through the same `log` facade.  Their
//! records are filtered separately (`lidi.log_level`, default `warn`),
//! because at `info` lidi logs every transfer, and DYODE makes one transfer
//! per Modbus poll.

use crate::config::LoggingConfig;
use crate::timefmt;
use log::{Level, LevelFilter, Log, Metadata, Record};
use serde_json::{Map, Value};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

pub const EVENT_SCHEMA_VERSION: u64 = 1;
const LOG_DIR_MODE: u32 = 0o750;
const RECHECK: Duration = Duration::from_secs(1);

/// An append-only file that follows logrotate.
struct WatchedFile {
    path: PathBuf,
    file: File,
    id: (u64, u64),
    checked: Instant,
}

impl WatchedFile {
    fn open(path: &Path) -> std::io::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o640)
            .open(path)?;
        let md = file.metadata()?;
        Ok(Self {
            path: path.to_path_buf(),
            file,
            id: (md.dev(), md.ino()),
            checked: Instant::now(),
        })
    }

    fn write_line(&mut self, line: &[u8]) {
        if self.checked.elapsed() >= RECHECK {
            self.checked = Instant::now();
            let current = fs::metadata(&self.path).ok().map(|m| (m.dev(), m.ino()));
            if current != Some(self.id) {
                if let Ok(fresh) = WatchedFile::open(&self.path) {
                    *self = fresh;
                }
            }
        }
        // One write() per record: O_APPEND keeps concurrent writers from
        // interleaving inside a line.
        let _ = self.file.write_all(line);
    }
}

struct State {
    level: LevelFilter,
    lidi_level: LevelFilter,
    human: Option<WatchedFile>,
}

struct DyodeLogger {
    state: RwLock<State>,
}

impl DyodeLogger {
    fn is_lidi(target: &str) -> bool {
        target.starts_with("lidi")
    }
}

impl Log for DyodeLogger {
    fn enabled(&self, md: &Metadata) -> bool {
        let st = self.state.read().unwrap();
        if Self::is_lidi(md.target()) {
            md.level() <= st.lidi_level
        } else {
            md.level() <= st.level
        }
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let thread = std::thread::current();
        let tname = thread.name().unwrap_or("main");
        let level = match record.level() {
            Level::Error => "ERROR",
            Level::Warn => "WARNING",
            Level::Info => "INFO",
            Level::Debug => "DEBUG",
            Level::Trace => "TRACE",
        };
        let prefix = if Self::is_lidi(record.target()) {
            "lidi: "
        } else {
            ""
        };
        let line = format!(
            "{} {:<7} [{}] {}{}\n",
            timefmt::local_log_stamp(),
            level,
            tname,
            prefix,
            record.args()
        );
        let _ = std::io::stderr().write_all(line.as_bytes());
        let mut st = self.state.write().unwrap();
        if let Some(h) = st.human.as_mut() {
            h.write_line(line.as_bytes());
        }
    }

    fn flush(&self) {
        let _ = std::io::stderr().flush();
    }
}

static LOGGER: OnceLock<DyodeLogger> = OnceLock::new();

struct EventState {
    file: Option<WatchedFile>,
    side: Option<String>,
    per_file_events: bool,
}

static EVENTS: Mutex<EventState> = Mutex::new(EventState {
    file: None,
    side: None,
    per_file_events: true,
});

/// Install the logger (stderr only).  Safe to call more than once.
pub fn init(level: LevelFilter) {
    let logger = LOGGER.get_or_init(|| DyodeLogger {
        state: RwLock::new(State {
            level,
            lidi_level: LevelFilter::Warn,
            human: None,
        }),
    });
    if log::set_logger(logger).is_ok() {
        log::set_max_level(LevelFilter::Trace);
    }
    logger.state.write().unwrap().level = level;
}

/// Add the log directory once the configuration is known.
///
/// If the directory cannot be used (not root, read-only /var) DYODE still
/// starts: it logs a warning and stays on stderr.
pub fn configure(cfg: &LoggingConfig, side: &str, lidi_level: LevelFilter) {
    let logger = LOGGER.get().expect("logging::init first");
    {
        let mut st = logger.state.write().unwrap();
        st.lidi_level = lidi_level;
        st.human = None;
    }
    {
        let mut ev = EVENTS.lock().unwrap();
        ev.side = Some(side.to_string());
        ev.per_file_events = cfg.per_file_events;
        ev.file = None;
    }
    let Some(dir) = cfg.dir.as_ref() else { return };
    let opened = (|| -> std::io::Result<(WatchedFile, WatchedFile)> {
        std::os::unix::fs::DirBuilderExt::mode(fs::DirBuilder::new().recursive(true), LOG_DIR_MODE)
            .create(dir)?;
        let human = WatchedFile::open(&dir.join(&cfg.human))?;
        let events = WatchedFile::open(&dir.join(&cfg.json))?;
        Ok((human, events))
    })();
    match opened {
        Ok((human, events)) => {
            logger.state.write().unwrap().human = Some(human);
            EVENTS.lock().unwrap().file = Some(events);
        }
        Err(e) => {
            log::warn!(
                "cannot write to {} ({e}); logging to stderr only",
                dir.display()
            );
        }
    }
}

/// One record for transfer.jsonl.
///
/// ```ignore
/// Event::new("file_sent").module(name).per_file().field("path", rel).emit();
/// ```
pub struct Event {
    name: &'static str,
    module: Option<String>,
    per_file: bool,
    fields: Map<String, Value>,
}

impl Event {
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            module: None,
            per_file: false,
            fields: Map::new(),
        }
    }

    pub fn module(mut self, module: &str) -> Self {
        self.module = Some(module.to_string());
        self
    }

    /// High-volume per-file records, suppressed by `per_file_events: false`.
    pub fn per_file(mut self) -> Self {
        self.per_file = true;
        self
    }

    pub fn field(mut self, key: &str, value: impl Into<Value>) -> Self {
        let v = value.into();
        if !v.is_null() {
            self.fields.insert(key.to_string(), v);
        }
        self
    }

    pub fn opt_field<T: Into<Value>>(self, key: &str, value: Option<T>) -> Self {
        match value {
            Some(v) => self.field(key, v),
            None => self,
        }
    }

    pub fn fields(mut self, map: Map<String, Value>) -> Self {
        for (k, v) in map {
            if !v.is_null() {
                self.fields.insert(k, v);
            }
        }
        self
    }

    pub fn emit(self) {
        let mut ev = EVENTS.lock().unwrap();
        if self.per_file && !ev.per_file_events {
            return;
        }
        let side = ev.side.clone();
        let Some(file) = ev.file.as_mut() else { return };
        let mut rec = Map::new();
        rec.insert("v".into(), EVENT_SCHEMA_VERSION.into());
        rec.insert("ts".into(), timefmt::utc_now().into());
        rec.insert("event".into(), self.name.into());
        if let Some(s) = side {
            rec.insert("side".into(), s.into());
        }
        if let Some(m) = self.module {
            rec.insert("module".into(), m.into());
        }
        for (k, v) in self.fields {
            rec.insert(k, v);
        }
        if let Ok(mut line) = serde_json::to_vec(&Value::Object(rec)) {
            line.push(b'\n');
            file.write_line(&line);
        }
    }
}

/// Log the first failure loudly, repeats quietly, and the recovery.
pub struct FailureLog {
    what: String,
    failing: bool,
}

impl FailureLog {
    pub fn new(what: impl Into<String>) -> Self {
        Self {
            what: what.into(),
            failing: false,
        }
    }

    pub fn fail(&mut self, err: impl std::fmt::Display) {
        if self.failing {
            log::debug!("{} (still failing): {err}", self.what);
        } else {
            log::warn!("{}: {err}", self.what);
            self.failing = true;
        }
    }

    pub fn ok(&mut self) {
        if self.failing {
            log::info!("{}: recovered", self.what);
            self.failing = false;
        }
    }
}

/// Rate-limited warning helper: true at most once per `every`.
pub struct Every {
    every: Duration,
    last: Option<Instant>,
}

impl Every {
    pub const fn new(every: Duration) -> Self {
        Self { every, last: None }
    }

    pub fn due(&mut self) -> bool {
        match self.last {
            Some(t) if t.elapsed() < self.every => false,
            _ => {
                self.last = Some(Instant::now());
                true
            }
        }
    }
}

/// Count refused envelopes and say so at most once a minute, like the
/// Python version's unseal(): a flood of forged traffic must not flood the
/// log too.
pub struct RefusalLog {
    module: String,
    state: std::sync::Mutex<(u64, Option<Instant>)>,
}

impl RefusalLog {
    pub fn new(module: &str) -> Self {
        Self {
            module: module.to_string(),
            state: std::sync::Mutex::new((0, None)),
        }
    }

    pub fn refused(&self, why: &dyn std::fmt::Display) {
        let mut st = self.state.lock().unwrap();
        st.0 += 1;
        if st.1.is_none_or(|t| t.elapsed() >= Duration::from_secs(60)) {
            log::warn!(
                "module {:?}: {} message(s) refused ({why})",
                self.module,
                st.0
            );
            *st = (0, Some(Instant::now()));
        }
    }
}
