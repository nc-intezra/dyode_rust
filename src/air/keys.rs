//! Keys of the air transport, all derived from dyode.key with HKDF-SHA256.
//!
//! - `k_pkt` signs every packet (a 16-byte truncated HMAC-SHA256 tag), so
//!   a receiver drops anything not sent by a key holder before it costs a
//!   RaptorQ decode.
//! - `k_enc` is never used directly: each sender boot gets its own
//!   ChaCha20-Poly1305 key, HKDF(k_enc, salt = boot id).  The nonce is the
//!   object id and chunk index, unique within a boot, so no nonce repeats
//!   under one key.  (The design sketch named XChaCha20 with the boot in
//!   the nonce; ring has no XChaCha20, and a per-boot key gives the same
//!   guarantee.)
//! - The key id is two bytes that tell receivers on a shared channel which
//!   packets are meant for their key, before any HMAC is computed.

use hmac::{Hmac, Mac};
use ring::aead::{self, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::hkdf;
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

pub const TAG_LEN: usize = 16;
/// Bytes AEAD adds to each chunk.
pub const SEAL_LEN: usize = 16;

struct Len(usize);

impl hkdf::KeyType for Len {
    fn len(&self) -> usize {
        self.0
    }
}

fn expand(salt: &[u8], ikm: &[u8], info: &[u8], out: &mut [u8]) {
    hkdf::Salt::new(hkdf::HKDF_SHA256, salt)
        .extract(ikm)
        .expand(&[info], Len(out.len()))
        .expect("HKDF output length is valid")
        .fill(out)
        .expect("HKDF output length is valid");
}

pub struct Keys {
    pkt: [u8; 32],
    enc: [u8; 32],
    pub key_id: u16,
}

impl Keys {
    pub fn derive(master: &[u8]) -> Self {
        let salt = b"dyode-air-v1";
        let mut pkt = [0u8; 32];
        let mut enc = [0u8; 32];
        let mut id = [0u8; 2];
        expand(salt, master, b"packet tag", &mut pkt);
        expand(salt, master, b"chunk encryption", &mut enc);
        expand(salt, master, b"key id", &mut id);
        Self {
            pkt,
            enc,
            key_id: u16::from_be_bytes(id),
        }
    }

    fn mac(&self, header: &[u8], symbol: &[u8]) -> HmacSha256 {
        let mut m = HmacSha256::new_from_slice(&self.pkt).expect("any key length");
        m.update(header);
        m.update(symbol);
        m
    }

    pub fn tag(&self, header: &[u8], symbol: &[u8]) -> [u8; TAG_LEN] {
        let full = self.mac(header, symbol).finalize().into_bytes();
        let mut t = [0u8; TAG_LEN];
        t.copy_from_slice(&full[..TAG_LEN]);
        t
    }

    /// Constant-time check of a truncated tag.
    pub fn verify(&self, header: &[u8], symbol: &[u8], tag: &[u8]) -> bool {
        tag.len() == TAG_LEN && self.mac(header, symbol).verify_truncated_left(tag).is_ok()
    }

    pub fn boot_key(&self, boot: u64) -> BootKey {
        let mut k = [0u8; 32];
        expand(
            &boot.to_be_bytes(),
            &self.enc,
            b"dyode-air boot key",
            &mut k,
        );
        let key = UnboundKey::new(&aead::CHACHA20_POLY1305, &k).expect("32-byte key");
        BootKey {
            key: LessSafeKey::new(key),
            boot,
        }
    }
}

/// One sender boot's chunk key.
pub struct BootKey {
    key: LessSafeKey,
    pub boot: u64,
}

/// What a chunk is bound to.  The chunk count is deliberately absent: a
/// receiver decrypts chunks before the manifest tells it how many there
/// are (design correction 1); the sealed manifest fixes the count and the
/// whole-object SHA-256 catches truncation.
pub struct ChunkId {
    pub endpoint: u16,
    pub module_tag: [u8; 8],
    pub object: u64,
    pub index: u32,
}

impl BootKey {
    fn nonce_aad(&self, id: &ChunkId) -> (Nonce, [u8; 30]) {
        let mut n = [0u8; 12];
        n[..8].copy_from_slice(&id.object.to_be_bytes());
        n[8..].copy_from_slice(&id.index.to_be_bytes());
        let mut aad = [0u8; 30];
        aad[..4].copy_from_slice(b"DYB1");
        aad[4..6].copy_from_slice(&id.endpoint.to_be_bytes());
        aad[6..14].copy_from_slice(&id.module_tag);
        aad[14..22].copy_from_slice(&self.boot.to_be_bytes());
        aad[22..30].copy_from_slice(&id.object.to_be_bytes());
        // the index is in the nonce; a chunk opened under the wrong index
        // fails like any other forgery
        (Nonce::assume_unique_for_key(n), aad)
    }

    pub fn seal(&self, id: &ChunkId, mut plain: Vec<u8>) -> Vec<u8> {
        let (nonce, aad) = self.nonce_aad(id);
        self.key
            .seal_in_place_append_tag(nonce, Aad::from(&aad[..]), &mut plain)
            .expect("ChaCha20-Poly1305 seals any length used here");
        plain
    }

    pub fn open(&self, id: &ChunkId, mut sealed: Vec<u8>) -> Option<Vec<u8>> {
        let (nonce, aad) = self.nonce_aad(id);
        let n = self
            .key
            .open_in_place(nonce, Aad::from(&aad[..]), &mut sealed)
            .ok()?
            .len();
        sealed.truncate(n);
        Some(sealed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(index: u32) -> ChunkId {
        ChunkId {
            endpoint: 3,
            module_tag: *b"modtag!!",
            object: 77,
            index,
        }
    }

    #[test]
    fn seal_open_and_tamper() {
        let k = Keys::derive(&[9u8; 32]);
        let bk = k.boot_key(0x1234);
        let sealed = bk.seal(&id(5), b"hello air".to_vec());
        assert_eq!(sealed.len(), 9 + SEAL_LEN);
        assert_eq!(bk.open(&id(5), sealed.clone()).unwrap(), b"hello air");
        // wrong index, wrong boot, flipped bit, other key
        assert!(bk.open(&id(6), sealed.clone()).is_none());
        assert!(k.boot_key(0x1235).open(&id(5), sealed.clone()).is_none());
        let mut bad = sealed.clone();
        bad[0] ^= 1;
        assert!(bk.open(&id(5), bad).is_none());
        let other = Keys::derive(&[8u8; 32]).boot_key(0x1234);
        assert!(other.open(&id(5), sealed).is_none());
    }

    #[test]
    fn packet_tags() {
        let k = Keys::derive(&[1u8; 32]);
        let t = k.tag(b"head", b"symbol");
        assert!(k.verify(b"head", b"symbol", &t));
        assert!(!k.verify(b"head", b"symbol!", &t));
        assert!(!k.verify(b"head", b"symbol", &t[..15]));
        assert_ne!(Keys::derive(&[2u8; 32]).key_id, k.key_id);
    }
}
