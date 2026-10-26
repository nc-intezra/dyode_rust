//! Message authentication: a shared key, HMAC-SHA256, and replay refusal.
//!
//! Both boxes hold the same random key (`dyode.key`, hex, beside
//! config.yaml).  Every envelope sent through the diode -- Modbus update,
//! log batch, screen frame, file -- is signed with it, so nothing that
//! merely reaches the output box's diode ports (from the IT network, say)
//! can pass for data from the input box.
//!
//! The key file format is unchanged from the Python version, so an existing
//! dyode.key keeps working.  The boot id is random per sender stream and
//! the counter rises with every envelope, so a captured envelope cannot be
//! replayed into the same stream; reordering within `window` is tolerated
//! (lidi delivers concurrent transfers in any order, and log batches are
//! sent more than once).

use crate::config::{AuthConfig, ConfigError};
use crate::hex;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::collections::{HashSet, VecDeque};
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

pub const KEY_BYTES: usize = 32;
pub const TAG_LEN: usize = 32;
type HmacSha256 = Hmac<Sha256>;

/// Fill `buf` from the OS random source.
pub fn random_bytes(buf: &mut [u8]) {
    getrandom::fill(buf).expect("the OS random number generator failed");
}

pub fn random_u32() -> u32 {
    let mut b = [0u8; 4];
    random_bytes(&mut b);
    u32::from_be_bytes(b)
}

/// Write a new random key (hex) to `path`, mode 0600.  Refuses to
/// overwrite: replacing the key on one box only breaks the link.
pub fn generate_key_file(path: &Path) -> std::io::Result<()> {
    let mut key = [0u8; KEY_BYTES];
    random_bytes(&mut key);
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    writeln!(f, "{}", hex::encode(key))?;
    f.sync_all()
}

/// The key bytes from a hex key file.
pub fn read_key_file(path: &Path) -> Result<Vec<u8>, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        ConfigError(format!(
            "cannot read authentication key {}: {e}",
            path.display()
        ))
    })?;
    let key = hex::decode(text.trim())
        .ok_or_else(|| ConfigError(format!("authentication key {} is not hex", path.display())))?;
    if key.len() < 16 {
        return Err(ConfigError(format!(
            "authentication key {} is too short (need at least 16 bytes)",
            path.display()
        )));
    }
    Ok(key)
}

/// The key for this config, or None when authentication is off.
///
/// A missing key with `auth.required` (the default) is a configuration
/// error: running unsigned would silently undo the protection.
pub fn load_key(auth: &AuthConfig) -> Result<Option<Vec<u8>>, ConfigError> {
    if !auth.key_file.exists() {
        if auth.required {
            return Err(ConfigError(format!(
                "no message authentication key at {}. Create one on ONE box with \
                 'dyode new-key' and copy it to the other box beside config.yaml \
                 (or set auth.required: false to run unauthenticated)",
                auth.key_file.display()
            )));
        }
        log::warn!(
            "auth.required is false and {} is missing: messages are NOT authenticated",
            auth.key_file.display()
        );
        return Ok(None);
    }
    read_key_file(&auth.key_file).map(Some)
}

/// HMAC-SHA256 over the concatenation of `parts`.
pub fn tag(key: &[u8], parts: &[&[u8]]) -> [u8; TAG_LEN] {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    for p in parts {
        mac.update(p);
    }
    mac.finalize().into_bytes().into()
}

/// Constant-time check of a tag over `parts`.
pub fn verify_tag(key: &[u8], parts: &[&[u8]], expected: &[u8]) -> bool {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    for p in parts {
        mac.update(p);
    }
    mac.verify_slice(expected).is_ok()
}

/// The signing side of one stream: a random boot id and a counter.
pub struct Sealer {
    key: Option<Vec<u8>>,
    boot: [u8; 8],
    counter: AtomicU64,
}

impl Sealer {
    pub fn new(key: Option<Vec<u8>>) -> Self {
        let mut boot = [0u8; 8];
        random_bytes(&mut boot);
        Self {
            key,
            boot,
            counter: AtomicU64::new(0),
        }
    }

    pub fn boot(&self) -> [u8; 8] {
        self.boot
    }

    pub fn next_counter(&self) -> u64 {
        self.counter.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// The tag over `parts`, or zeros when running unauthenticated.
    pub fn sign(&self, parts: &[&[u8]]) -> [u8; TAG_LEN] {
        match &self.key {
            Some(k) => tag(k, parts),
            None => [0u8; TAG_LEN],
        }
    }

    pub fn authenticated(&self) -> bool {
        self.key.is_some()
    }
}

/// Why an envelope was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    BadSignature,
    Replayed,
}

struct Stream {
    boot: [u8; 8],
    highest: u64,
    recent: HashSet<u64>,
}

/// The checking side: verifies tags and refuses replays.
pub struct Verifier {
    key: Option<Vec<u8>>,
    window: u64,
    max_streams: usize,
    streams: VecDeque<Stream>,
    pub rejected: u64,
}

impl Verifier {
    pub fn new(key: Option<Vec<u8>>) -> Self {
        Self {
            key,
            window: 4096,
            max_streams: 8,
            streams: VecDeque::new(),
            rejected: 0,
        }
    }

    pub fn authenticated(&self) -> bool {
        self.key.is_some()
    }

    /// Check the tag and the (boot, counter) pair.  Without a key every
    /// envelope is accepted (auth.required: false), replays included.
    pub fn check(
        &mut self,
        parts: &[&[u8]],
        tag: &[u8],
        boot: [u8; 8],
        counter: u64,
    ) -> Result<(), Refusal> {
        let Some(key) = &self.key else { return Ok(()) };
        if !verify_tag(key, parts, tag) {
            self.rejected += 1;
            return Err(Refusal::BadSignature);
        }
        let pos = self.streams.iter().position(|s| s.boot == boot);
        let mut stream = match pos {
            Some(i) => self.streams.remove(i).unwrap(),
            None => Stream {
                boot,
                highest: 0,
                recent: HashSet::new(),
            },
        };
        let too_old = counter.saturating_add(self.window) <= stream.highest;
        if too_old || stream.recent.contains(&counter) {
            self.rejected += 1;
            self.streams.push_back(stream);
            return Err(Refusal::Replayed);
        }
        stream.recent.insert(counter);
        if counter > stream.highest {
            stream.highest = counter;
            let floor = counter.saturating_sub(self.window);
            stream.recent.retain(|c| *c > floor);
        }
        self.streams.push_back(stream);
        while self.streams.len() > self.max_streams {
            self.streams.pop_front();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_rfc4231_case2() {
        let t = tag(b"Jefe", &[b"what do ya want ", b"for nothing?"]);
        assert_eq!(
            hex::encode(t),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn replay_window() {
        let key = vec![7u8; 32];
        let s = Sealer::new(Some(key.clone()));
        let mut v = Verifier::new(Some(key));
        let boot = s.boot();
        let msg = |c: u64| s.sign(&[b"x", &c.to_be_bytes()]);
        for c in [1u64, 3, 2] {
            assert_eq!(v.check(&[b"x", &c.to_be_bytes()], &msg(c), boot, c), Ok(()));
        }
        assert_eq!(
            v.check(&[b"x", &2u64.to_be_bytes()], &msg(2), boot, 2),
            Err(Refusal::Replayed)
        );
        assert_eq!(
            v.check(&[b"y"], &msg(9), boot, 9),
            Err(Refusal::BadSignature)
        );
        // Far behind the window.
        let hi = 10_000u64;
        v.check(&[b"x", &hi.to_be_bytes()], &msg(hi), boot, hi)
            .unwrap();
        assert_eq!(
            v.check(&[b"x", &4u64.to_be_bytes()], &msg(4), boot, 4),
            Err(Refusal::Replayed)
        );
    }

    #[test]
    fn key_file_roundtrip() {
        let dir = crate::testutil::TempDir::new();
        let p = dir.path().join("dyode.key");
        generate_key_file(&p).unwrap();
        assert!(generate_key_file(&p).is_err(), "must refuse to overwrite");
        assert_eq!(read_key_file(&p).unwrap().len(), KEY_BYTES);
        std::fs::write(dir.path().join("short"), "abcd\n").unwrap();
        assert!(read_key_file(&dir.path().join("short")).is_err());
    }
}
