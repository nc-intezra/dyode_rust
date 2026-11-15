//! The DYB1 datagram: one RaptorQ symbol of one chunk of one object.
//!
//! ```text
//!  0  magic "DYB1"        4
//!  4  version (1)         1
//!  5  flags               1   bit 0: manifest chunk
//!  6  key id              2
//!  8  endpoint            2   the module (its index in config.yaml)
//! 10  boot                8   sender start: unix seconds << 32 | random
//! 18  object id           8   counts up from 1 within a boot
//! 26  chunk index         4   0 = manifest, 1.. = data
//! 30  chunk length        4   sealed bytes of the chunk (before padding)
//! 34  payload id          4   RaptorQ SBN + ESI
//! 38  symbol              T
//! 38+T tag               16   HMAC-SHA256(k_pkt, header || symbol)[..16]
//! ```
//!
//! 54 bytes of overhead per packet; all integers big-endian.

use super::keys::{Keys, TAG_LEN};

pub const MAGIC: &[u8; 4] = b"DYB1";
pub const VERSION: u8 = 1;
pub const HEADER_LEN: usize = 38;
pub const OVERHEAD: usize = HEADER_LEN + TAG_LEN;
pub const FLAG_MANIFEST: u8 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub flags: u8,
    pub key_id: u16,
    pub endpoint: u16,
    pub boot: u64,
    pub object: u64,
    pub chunk: u32,
    pub chunk_len: u32,
    pub payload_id: [u8; 4],
}

impl Header {
    pub fn to_bytes(&self) -> [u8; HEADER_LEN] {
        let mut h = [0u8; HEADER_LEN];
        h[..4].copy_from_slice(MAGIC);
        h[4] = VERSION;
        h[5] = self.flags;
        h[6..8].copy_from_slice(&self.key_id.to_be_bytes());
        h[8..10].copy_from_slice(&self.endpoint.to_be_bytes());
        h[10..18].copy_from_slice(&self.boot.to_be_bytes());
        h[18..26].copy_from_slice(&self.object.to_be_bytes());
        h[26..30].copy_from_slice(&self.chunk.to_be_bytes());
        h[30..34].copy_from_slice(&self.chunk_len.to_be_bytes());
        h[34..38].copy_from_slice(&self.payload_id);
        h
    }

    fn from_bytes(h: &[u8]) -> Self {
        let u16_at = |i: usize| u16::from_be_bytes([h[i], h[i + 1]]);
        let u32_at = |i: usize| u32::from_be_bytes(h[i..i + 4].try_into().unwrap());
        let u64_at = |i: usize| u64::from_be_bytes(h[i..i + 8].try_into().unwrap());
        Self {
            flags: h[5],
            key_id: u16_at(6),
            endpoint: u16_at(8),
            boot: u64_at(10),
            object: u64_at(18),
            chunk: u32_at(26),
            chunk_len: u32_at(30),
            payload_id: h[34..38].try_into().unwrap(),
        }
    }
}

/// Why a datagram was dropped before its contents were looked at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bad {
    /// Not a DYB1 packet of the configured symbol size.
    Format,
    /// A DYB1 packet for another key (another link on the same channel).
    OtherKey,
    /// The right key id, but the tag is wrong: damaged or forged.
    Tag,
}

impl Bad {
    pub fn counter(self) -> &'static str {
        match self {
            Bad::Format => "air_packets_malformed",
            Bad::OtherKey => "air_packets_other_key",
            Bad::Tag => "air_packets_bad_tag",
        }
    }
}

pub fn encode(keys: &Keys, h: &Header, symbol: &[u8], out: &mut Vec<u8>) {
    out.clear();
    let head = h.to_bytes();
    out.extend_from_slice(&head);
    out.extend_from_slice(symbol);
    out.extend_from_slice(&keys.tag(&head, symbol));
}

/// Check and split one datagram.  `symbol` is the configured symbol size:
/// both boxes read it from the same config.yaml.
pub fn decode<'a>(keys: &Keys, buf: &'a [u8], symbol: usize) -> Result<(Header, &'a [u8]), Bad> {
    if buf.len() != OVERHEAD + symbol || &buf[..4] != MAGIC || buf[4] != VERSION {
        return Err(Bad::Format);
    }
    let h = Header::from_bytes(&buf[..HEADER_LEN]);
    if h.key_id != keys.key_id {
        return Err(Bad::OtherKey);
    }
    let sym = &buf[HEADER_LEN..HEADER_LEN + symbol];
    if !keys.verify(&buf[..HEADER_LEN], sym, &buf[HEADER_LEN + symbol..]) {
        return Err(Bad::Tag);
    }
    Ok((h, sym))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_rejections() {
        let k = Keys::derive(&[5u8; 32]);
        let h = Header {
            flags: FLAG_MANIFEST,
            key_id: k.key_id,
            endpoint: 2,
            boot: 0x0102_0304_0506_0708,
            object: 9,
            chunk: 0,
            chunk_len: 91,
            payload_id: [0, 0, 0, 1],
        };
        let sym = vec![0xabu8; 64];
        let mut pkt = Vec::new();
        encode(&k, &h, &sym, &mut pkt);
        assert_eq!(pkt.len(), 64 + OVERHEAD);
        assert_eq!(OVERHEAD, 54);
        let (h2, s2) = decode(&k, &pkt, 64).unwrap();
        assert_eq!(h2, h);
        assert_eq!(s2, &sym[..]);
        assert_eq!(decode(&k, &pkt, 63), Err(Bad::Format));
        let mut bad = pkt.clone();
        bad[20] ^= 1; // object id
        assert_eq!(decode(&k, &bad, 64), Err(Bad::Tag));
        let mut bad = pkt.clone();
        bad[50] ^= 1; // symbol
        assert_eq!(decode(&k, &bad, 64), Err(Bad::Tag));
        let other = Keys::derive(&[6u8; 32]);
        assert_eq!(decode(&other, &pkt, 64), Err(Bad::OtherKey));
    }
}
