//! The DYODE envelope: what travels inside every lidi transfer.
//!
//! lidi moves a byte stream reliably *or not at all*: a RaptorQ block it
//! cannot rebuild aborts every transfer in progress, with no signal to the
//! sender.  So DYODE never keeps a long-lived stream open through lidi.
//! Each Modbus update, log batch, screen frame and file is its own lidi
//! transfer, wrapped in an envelope that is self-validating and signed:
//!
//! ```text
//!   header   magic "DYL1" | version u8 | kind u8 | endpoint u16
//!            | module tag [8] | boot id [8] | counter u64
//!            | body length u64 | meta length u32 | meta (JSON)
//!   body     <body length> bytes
//!   trailer  SHA-256(body) [32] | HMAC-SHA256(key, header || SHA-256(body)) [32]
//! ```
//!
//! Integers are big-endian.  The receiver knows a transfer is complete and
//! genuine only when the whole body *and* a valid trailer arrived; anything
//! cut short by a lidi abort is discarded and reported.  This replaces the
//! Python version's DYO1 datagram framing (chunking, CRC32, reassembly) and
//! its DYA1 seal, and -- because each file carries its own signed name,
//! size and hash -- the separate folder manifests too.
//!
//! The module tag (see `config::Module::tag`) makes two boxes with
//! different config.yaml files refuse each other's traffic instead of
//! routing it to the wrong module.

use crate::auth::{Refusal, Sealer, TAG_LEN, Verifier};
use crate::ratelimit::TokenBucket;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub const MAGIC: &[u8; 4] = b"DYL1";
pub const VERSION: u8 = 1;
pub const FIXED_HEADER: usize = 4 + 1 + 1 + 2 + 8 + 8 + 8 + 8 + 4;
pub const TRAILER: usize = 32 + TAG_LEN;
pub const MAX_META: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    /// A whole message held in memory (Modbus, logs, screen, batch summary).
    Message = 1,
    /// A file streamed to disk.
    File = 2,
}

impl Kind {
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Kind::Message),
            2 => Some(Kind::File),
            _ => None,
        }
    }
}

/// A parsed header.  Nothing in it is trustworthy until the trailer has
/// been verified -- `Inbound::open_body` must only use it for limits.
#[derive(Clone, Debug)]
pub struct Header {
    pub kind: Kind,
    pub endpoint: u16,
    pub module_tag: [u8; 8],
    pub boot: [u8; 8],
    pub counter: u64,
    pub body_len: u64,
    pub meta: Vec<u8>,
    /// The header exactly as received, for the HMAC.
    pub raw: Vec<u8>,
}

impl Header {
    /// Parse the meta JSON (empty meta = null).
    pub fn meta_json(&self) -> Option<serde_json::Value> {
        if self.meta.is_empty() {
            return Some(serde_json::Value::Null);
        }
        serde_json::from_slice(&self.meta).ok()
    }
}

/// Build a header; returns its bytes.
pub fn encode_header(
    sealer: &Sealer,
    kind: Kind,
    endpoint: u16,
    module_tag: [u8; 8],
    body_len: u64,
    meta: &[u8],
) -> Vec<u8> {
    assert!(meta.len() <= MAX_META, "envelope meta too large");
    let mut h = Vec::with_capacity(FIXED_HEADER + meta.len());
    h.extend_from_slice(MAGIC);
    h.push(VERSION);
    h.push(kind as u8);
    h.extend_from_slice(&endpoint.to_be_bytes());
    h.extend_from_slice(&module_tag);
    h.extend_from_slice(&sealer.boot());
    h.extend_from_slice(&sealer.next_counter().to_be_bytes());
    h.extend_from_slice(&body_len.to_be_bytes());
    h.extend_from_slice(&(meta.len() as u32).to_be_bytes());
    h.extend_from_slice(meta);
    h
}

fn trailer(sealer: &Sealer, header: &[u8], digest: &[u8; 32]) -> [u8; TRAILER] {
    let tag = sealer.sign(&[header, digest]);
    let mut t = [0u8; TRAILER];
    t[..32].copy_from_slice(digest);
    t[32..].copy_from_slice(&tag);
    t
}

/// A complete in-memory message envelope.
pub fn message(
    sealer: &Sealer,
    endpoint: u16,
    module_tag: [u8; 8],
    meta: &[u8],
    body: &[u8],
) -> Vec<u8> {
    let header = encode_header(
        sealer,
        Kind::Message,
        endpoint,
        module_tag,
        body.len() as u64,
        meta,
    );
    let digest: [u8; 32] = Sha256::digest(body).into();
    let mut out = header.clone();
    out.extend_from_slice(body);
    out.extend_from_slice(&trailer(sealer, &header, &digest));
    out
}

/// How a file transfer ended, as seen by the sender.
pub type FileOutcome = Result<[u8; 32], String>;

enum SrcState {
    Header(usize),
    Body,
    Trailer([u8; TRAILER], usize),
    Done,
}

/// A `Read` that produces header + file + trailer, hashing the file as it
/// goes, so the bytes that are hashed are exactly the bytes that are sent
/// (read once, from the descriptor that was checked).  The outcome --
/// the SHA-256, or why it failed -- is reported on `done`.
pub struct FileSource {
    sealer: Arc<Sealer>,
    header: Vec<u8>,
    file: File,
    remaining: u64,
    hasher: Sha256,
    state: SrcState,
    pacers: Vec<Arc<TokenBucket>>,
    done: Option<crossbeam_channel::Sender<FileOutcome>>,
}

impl FileSource {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        sealer: Arc<Sealer>,
        endpoint: u16,
        module_tag: [u8; 8],
        meta: &[u8],
        file: File,
        size: u64,
        pacers: Vec<Arc<TokenBucket>>,
        done: crossbeam_channel::Sender<FileOutcome>,
    ) -> Self {
        let header = encode_header(&sealer, Kind::File, endpoint, module_tag, size, meta);
        Self {
            sealer,
            header,
            file,
            remaining: size,
            hasher: Sha256::new(),
            state: SrcState::Header(0),
            pacers,
            done: Some(done),
        }
    }

    fn finish(&mut self, outcome: FileOutcome) {
        if let Some(tx) = self.done.take() {
            let _ = tx.send(outcome);
        }
    }

    fn pace(&self, n: usize) {
        for p in &self.pacers {
            p.take(n);
        }
    }
}

impl Read for FileSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            match &mut self.state {
                SrcState::Header(pos) => {
                    let n = (self.header.len() - *pos).min(buf.len());
                    buf[..n].copy_from_slice(&self.header[*pos..*pos + n]);
                    *pos += n;
                    if *pos == self.header.len() {
                        self.state = SrcState::Body;
                    }
                    return Ok(n);
                }
                SrcState::Body => {
                    if self.remaining == 0 {
                        let digest: [u8; 32] = std::mem::take(&mut self.hasher).finalize().into();
                        self.state =
                            SrcState::Trailer(trailer(&self.sealer, &self.header, &digest), 0);
                        continue;
                    }
                    let want = buf.len().min(self.remaining.min(1 << 20) as usize);
                    let n = match self.file.read(&mut buf[..want]) {
                        Ok(n) => n,
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(e) => {
                            self.finish(Err(format!("read error: {e}")));
                            return Err(e);
                        }
                    };
                    if n == 0 {
                        // Shrunk while being sent: the receiver must not
                        // get a short file, so make lidi abort it.
                        let msg = format!(
                            "file shrank while being sent ({} bytes missing)",
                            self.remaining
                        );
                        self.finish(Err(msg.clone()));
                        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, msg));
                    }
                    self.hasher.update(&buf[..n]);
                    self.remaining -= n as u64;
                    self.pace(n);
                    return Ok(n);
                }
                SrcState::Trailer(t, pos) => {
                    let n = (TRAILER - *pos).min(buf.len());
                    buf[..n].copy_from_slice(&t[*pos..*pos + n]);
                    *pos += n;
                    if *pos == TRAILER {
                        let digest: [u8; 32] = t[..32].try_into().unwrap();
                        self.state = SrcState::Done;
                        self.finish(Ok(digest));
                    }
                    return Ok(n);
                }
                SrcState::Done => return Ok(0),
            }
        }
    }
}

impl Drop for FileSource {
    fn drop(&mut self) {
        self.finish(Err("transfer abandoned before the end".into()));
    }
}

/// A `Read` over bytes already in memory, optionally paced.
pub struct MemSource {
    data: Vec<u8>,
    pos: usize,
    pacers: Vec<Arc<TokenBucket>>,
}

impl MemSource {
    pub fn new(data: Vec<u8>, pacers: Vec<Arc<TokenBucket>>) -> Self {
        Self {
            data,
            pos: 0,
            pacers,
        }
    }
}

impl Read for MemSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = (self.data.len() - self.pos).min(buf.len());
        buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        for p in &self.pacers {
            p.take(n);
        }
        Ok(n)
    }
}

// --------------------------------------------------------------------------
// Receiving
// --------------------------------------------------------------------------

/// Where a body is written.
pub enum Body {
    Memory(Vec<u8>),
    File { file: File, path: PathBuf },
}

impl Body {
    fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        match self {
            Body::Memory(v) => {
                v.extend_from_slice(data);
                Ok(())
            }
            Body::File { file, .. } => file.write_all(data),
        }
    }
}

/// Why a transfer was not delivered.
#[derive(Debug, Clone)]
pub enum Failure {
    /// Cut short: lidi lost a block and aborted it, or the sender stopped.
    Incomplete {
        received: u64,
        expected: Option<u64>,
    },
    /// The signature or the replay check failed.
    Refused(Refusal),
    /// Not a DYODE envelope, or not one for this module.
    Malformed(String),
    /// The module refused it (too large, unexpected kind, disk error...).
    Rejected(String),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::Incomplete {
                received,
                expected: Some(e),
            } => {
                write!(
                    f,
                    "incomplete transfer ({received} of {e} body bytes; lost in the diode or sender stopped)"
                )
            }
            Failure::Incomplete {
                received,
                expected: None,
            } => {
                write!(
                    f,
                    "incomplete transfer ({received} bytes, header never completed)"
                )
            }
            Failure::Refused(Refusal::BadSignature) => {
                f.write_str("not signed with this box's key")
            }
            Failure::Refused(Refusal::Replayed) => f.write_str("replayed"),
            Failure::Malformed(m) => write!(f, "malformed: {m}"),
            Failure::Rejected(m) => write!(f, "rejected: {m}"),
        }
    }
}

/// A module's receiving side.  Called from lidi's client threads.
pub trait Inbound: Send + Sync {
    fn module_name(&self) -> &str;
    /// The header is structurally valid but NOT yet authenticated: decide
    /// only on kind and size.  Return where the body should go.
    fn open_body(&self, hdr: &Header) -> Result<Body, String>;
    /// A complete, verified envelope.
    fn deliver(&self, hdr: Header, body: Body, digest: [u8; 32]);
    /// Anything else.  `hdr` and `body` are given when they got that far
    /// (the header is unverified: use it for diagnostics only).
    fn failed(&self, hdr: Option<&Header>, body: Option<Body>, why: Failure);
}

/// What the receiving side needs to know about one endpoint.
pub struct Registration {
    pub endpoint: u16,
    pub module_tag: [u8; 8],
    pub inbound: Arc<dyn Inbound>,
    pub verifier: Mutex<Verifier>,
}

enum ParseState {
    Head(Vec<u8>),
    Body {
        hdr: Header,
        body: Body,
        hasher: Sha256,
        remaining: u64,
    },
    Trailer {
        hdr: Header,
        body: Body,
        digest: [u8; 32],
        buf: Vec<u8>,
    },
    Done {
        hdr: Header,
        body: Body,
        digest: [u8; 32],
        trailer: Vec<u8>,
    },
    Failed,
    Delivered,
}

/// The lidi output client: parses one envelope as it streams in, and
/// delivers or reports it when lidi drops it at the end of the transfer.
pub struct Parser {
    reg: Arc<Registration>,
    state: ParseState,
    received: u64,
}

impl Parser {
    pub fn new(reg: Arc<Registration>) -> Self {
        Self {
            reg,
            state: ParseState::Head(Vec::with_capacity(FIXED_HEADER)),
            received: 0,
        }
    }

    fn fail(&mut self, hdr: Option<&Header>, body: Option<Body>, why: Failure) -> io::Error {
        let msg = why.to_string();
        self.reg.inbound.failed(hdr, body, why);
        self.state = ParseState::Failed;
        io::Error::new(io::ErrorKind::InvalidData, msg)
    }

    #[allow(clippy::type_complexity)]
    fn parse_fixed(
        b: &[u8],
        want_endpoint: u16,
        want_tag: [u8; 8],
    ) -> Result<(Kind, u16, [u8; 8], [u8; 8], u64, u64, usize), String> {
        if &b[..4] != MAGIC {
            return Err("not a DYODE envelope (bad magic)".into());
        }
        if b[4] != VERSION {
            return Err(format!("unsupported envelope version {}", b[4]));
        }
        let kind = Kind::from_u8(b[5]).ok_or_else(|| format!("unknown envelope kind {}", b[5]))?;
        let endpoint = u16::from_be_bytes([b[6], b[7]]);
        let tag: [u8; 8] = b[8..16].try_into().unwrap();
        let boot: [u8; 8] = b[16..24].try_into().unwrap();
        let counter = u64::from_be_bytes(b[24..32].try_into().unwrap());
        let body_len = u64::from_be_bytes(b[32..40].try_into().unwrap());
        let meta_len = u32::from_be_bytes(b[40..44].try_into().unwrap()) as usize;
        if endpoint != want_endpoint {
            return Err(format!(
                "envelope for endpoint {endpoint} arrived on endpoint {want_endpoint}"
            ));
        }
        if tag != want_tag {
            return Err("sent by a different module: are both config.yaml files identical?".into());
        }
        if meta_len > MAX_META {
            return Err(format!("meta too large ({meta_len} bytes)"));
        }
        Ok((kind, endpoint, tag, boot, counter, body_len, meta_len))
    }

    fn feed(&mut self, mut data: &[u8]) -> io::Result<()> {
        let (want_endpoint, want_tag) = (self.reg.endpoint, self.reg.module_tag);
        while !data.is_empty() {
            match &mut self.state {
                ParseState::Head(buf) => {
                    let meta_len =
                        |b: &[u8]| u32::from_be_bytes(b[40..44].try_into().unwrap()) as usize;
                    let target = if buf.len() < FIXED_HEADER {
                        FIXED_HEADER
                    } else {
                        FIXED_HEADER + meta_len(buf)
                    };
                    let n = (target - buf.len()).min(data.len());
                    buf.extend_from_slice(&data[..n]);
                    data = &data[n..];
                    if buf.len() < target {
                        continue;
                    }
                    if target == FIXED_HEADER {
                        // Validate as soon as the fixed part is in, before
                        // trusting meta_len to size anything.
                        if let Err(e) = Self::parse_fixed(buf, want_endpoint, want_tag) {
                            return Err(self.fail(None, None, Failure::Malformed(e)));
                        }
                        if meta_len(buf) > 0 {
                            continue;
                        }
                    }
                    // Header complete.
                    let raw = std::mem::take(buf);
                    let (kind, endpoint, module_tag, boot, counter, body_len, _) =
                        match Self::parse_fixed(&raw, want_endpoint, want_tag) {
                            Ok(v) => v,
                            Err(e) => return Err(self.fail(None, None, Failure::Malformed(e))),
                        };
                    let hdr = Header {
                        kind,
                        endpoint,
                        module_tag,
                        boot,
                        counter,
                        body_len,
                        meta: raw[FIXED_HEADER..].to_vec(),
                        raw,
                    };
                    match self.reg.inbound.open_body(&hdr) {
                        Ok(body) => {
                            self.state = ParseState::Body {
                                hdr,
                                body,
                                hasher: Sha256::new(),
                                remaining: body_len,
                            };
                        }
                        Err(e) => return Err(self.fail(Some(&hdr), None, Failure::Rejected(e))),
                    }
                }
                ParseState::Body {
                    body,
                    hasher,
                    remaining,
                    ..
                } => {
                    let n = (*remaining).min(data.len() as u64) as usize;
                    if n > 0 {
                        hasher.update(&data[..n]);
                        if let Err(e) = body.write_all(&data[..n]) {
                            let (h, b) =
                                match std::mem::replace(&mut self.state, ParseState::Failed) {
                                    ParseState::Body { hdr, body, .. } => (hdr, body),
                                    _ => unreachable!(),
                                };
                            return Err(self.fail(
                                Some(&h),
                                Some(b),
                                Failure::Rejected(format!("cannot store: {e}")),
                            ));
                        }
                        *remaining -= n as u64;
                        data = &data[n..];
                    }
                    if *remaining == 0 {
                        let ParseState::Body {
                            hdr, body, hasher, ..
                        } = std::mem::replace(&mut self.state, ParseState::Failed)
                        else {
                            unreachable!()
                        };
                        self.state = ParseState::Trailer {
                            hdr,
                            body,
                            digest: hasher.finalize().into(),
                            buf: Vec::new(),
                        };
                    }
                }
                ParseState::Trailer { buf, .. } => {
                    let n = (TRAILER - buf.len()).min(data.len());
                    buf.extend_from_slice(&data[..n]);
                    data = &data[n..];
                    if buf.len() == TRAILER {
                        let ParseState::Trailer {
                            hdr,
                            body,
                            digest,
                            buf,
                        } = std::mem::replace(&mut self.state, ParseState::Failed)
                        else {
                            unreachable!()
                        };
                        self.state = ParseState::Done {
                            hdr,
                            body,
                            digest,
                            trailer: buf,
                        };
                    }
                }
                ParseState::Done { .. } => {
                    let ParseState::Done { hdr, body, .. } =
                        std::mem::replace(&mut self.state, ParseState::Failed)
                    else {
                        unreachable!()
                    };
                    return Err(self.fail(
                        Some(&hdr),
                        Some(body),
                        Failure::Malformed("data after the trailer".into()),
                    ));
                }
                ParseState::Failed | ParseState::Delivered => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "transfer already refused",
                    ));
                }
            }
        }
        Ok(())
    }

    /// Settle the transfer: deliver it if complete and genuine, report it
    /// otherwise.  Called when lidi drops the client.
    fn settle(&mut self) {
        match std::mem::replace(&mut self.state, ParseState::Delivered) {
            ParseState::Done {
                hdr,
                body,
                digest,
                trailer,
            } => {
                if trailer[..32] != digest {
                    self.reg.inbound.failed(
                        Some(&hdr),
                        Some(body),
                        Failure::Malformed("body does not match its SHA-256".into()),
                    );
                    return;
                }
                let check = self.reg.verifier.lock().unwrap().check(
                    &[&hdr.raw, &digest],
                    &trailer[32..],
                    hdr.boot,
                    hdr.counter,
                );
                match check {
                    Ok(()) => self.reg.inbound.deliver(hdr, body, digest),
                    Err(r) => self
                        .reg
                        .inbound
                        .failed(Some(&hdr), Some(body), Failure::Refused(r)),
                }
            }
            ParseState::Head(buf) => {
                if !buf.is_empty() || self.received > 0 {
                    self.reg.inbound.failed(
                        None,
                        None,
                        Failure::Incomplete {
                            received: self.received,
                            expected: None,
                        },
                    );
                }
            }
            ParseState::Body {
                hdr,
                body,
                remaining,
                ..
            } => {
                let expected = hdr.body_len;
                self.reg.inbound.failed(
                    Some(&hdr),
                    Some(body),
                    Failure::Incomplete {
                        received: expected - remaining,
                        expected: Some(expected),
                    },
                );
            }
            ParseState::Trailer { hdr, body, .. } => {
                let expected = hdr.body_len;
                self.reg.inbound.failed(
                    Some(&hdr),
                    Some(body),
                    Failure::Incomplete {
                        received: expected,
                        expected: Some(expected),
                    },
                );
            }
            ParseState::Failed | ParseState::Delivered => {}
        }
    }
}

impl Write for Parser {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.received += data.len() as u64;
        self.feed(data)?;
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for Parser {
    fn drop(&mut self) {
        self.settle();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    type Got = Vec<(Kind, Vec<u8>, Vec<u8>)>;

    #[derive(Default)]
    struct Collect {
        got: StdMutex<Got>,
        bad: StdMutex<Vec<String>>,
    }

    impl Inbound for Collect {
        fn module_name(&self) -> &str {
            "test"
        }
        fn open_body(&self, hdr: &Header) -> Result<Body, String> {
            if hdr.body_len > 1 << 20 {
                return Err("too large".into());
            }
            Ok(Body::Memory(Vec::new()))
        }
        fn deliver(&self, hdr: Header, body: Body, _d: [u8; 32]) {
            let Body::Memory(b) = body else { panic!() };
            self.got.lock().unwrap().push((hdr.kind, hdr.meta, b));
        }
        fn failed(&self, _h: Option<&Header>, _b: Option<Body>, why: Failure) {
            self.bad.lock().unwrap().push(why.to_string());
        }
    }

    fn reg(key: Option<Vec<u8>>, c: Arc<Collect>) -> Arc<Registration> {
        Arc::new(Registration {
            endpoint: 3,
            module_tag: [9; 8],
            inbound: c,
            verifier: Mutex::new(Verifier::new(key)),
        })
    }

    fn feed_chunks(r: &Arc<Registration>, bytes: &[u8], chunk: usize) {
        let mut p = Parser::new(r.clone());
        for c in bytes.chunks(chunk) {
            if p.write(c).is_err() {
                break;
            }
        }
    }

    #[test]
    fn message_roundtrip_any_chunking() {
        let key = vec![1u8; 32];
        let sealer = Sealer::new(Some(key.clone()));
        let c = Arc::new(Collect::default());
        let r = reg(Some(key), c.clone());
        for chunk in [1, 7, 44, 1000] {
            let env = message(&sealer, 3, [9; 8], br#"{"a":1}"#, b"hello world");
            feed_chunks(&r, &env, chunk);
        }
        let got = c.got.lock().unwrap();
        assert_eq!(got.len(), 4);
        assert_eq!(got[0].2, b"hello world");
        assert_eq!(got[0].1, br#"{"a":1}"#);
        assert!(c.bad.lock().unwrap().is_empty());
    }

    #[test]
    fn truncated_wrong_key_and_replay_are_refused() {
        let key = vec![1u8; 32];
        let sealer = Sealer::new(Some(key.clone()));
        let c = Arc::new(Collect::default());
        let r = reg(Some(key), c.clone());
        let env = message(&sealer, 3, [9; 8], b"", b"payload");
        feed_chunks(&r, &env[..env.len() - 5], 16); // cut short
        feed_chunks(&r, &env, 16); // ok
        feed_chunks(&r, &env, 16); // replay
        let forged = message(
            &Sealer::new(Some(vec![2u8; 32])),
            3,
            [9; 8],
            b"",
            b"payload",
        );
        feed_chunks(&r, &forged, 16);
        let other_module = message(&sealer, 3, [8; 8], b"", b"payload");
        feed_chunks(&r, &other_module, 16);
        assert_eq!(c.got.lock().unwrap().len(), 1);
        let bad = c.bad.lock().unwrap();
        assert_eq!(bad.len(), 4, "{bad:?}");
        assert!(bad[0].contains("incomplete"));
        assert!(bad[1].contains("replayed"));
        assert!(bad[2].contains("key"));
        assert!(bad[3].contains("config.yaml"));
    }

    #[test]
    fn file_source_streams_and_reports() {
        let dir = crate::testutil::TempDir::new();
        let path = dir.path().join("f");
        let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &data).unwrap();
        let sealer = Arc::new(Sealer::new(None));
        let (tx, rx) = crossbeam_channel::bounded(1);
        let mut src = FileSource::new(
            sealer,
            3,
            [9; 8],
            b"{}",
            File::open(&path).unwrap(),
            data.len() as u64,
            vec![],
            tx,
        );
        let mut out = Vec::new();
        src.read_to_end(&mut out).unwrap();
        let digest = rx.recv().unwrap().unwrap();
        assert_eq!(digest, <[u8; 32]>::from(Sha256::digest(&data)));
        let c = Arc::new(Collect::default());
        let r = reg(None, c.clone());
        feed_chunks(&r, &out, 65536);
        assert_eq!(c.got.lock().unwrap()[0].2, data);
    }
}
