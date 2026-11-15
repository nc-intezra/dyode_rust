//! Chunk 0 of every object: what the object is, sealed like the data.

pub const LEN: usize = 75;
const VERSION: u8 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    pub endpoint: u16,
    pub module_tag: [u8; 8],
    /// Bytes of the object (one DYL1 envelope).
    pub total_len: u64,
    /// Plaintext bytes per data chunk (the last may be shorter).
    pub chunk_size: u32,
    pub chunk_count: u32,
    /// SHA-256 of the whole object.
    pub sha256: [u8; 32],
    /// Unix seconds, sender clock.
    pub created: u64,
    /// Unix seconds after which a receiver drops the object.
    pub expiry: u64,
}

impl Manifest {
    pub fn chunks_for(total_len: u64, chunk_size: u32) -> u32 {
        total_len.div_ceil(u64::from(chunk_size)).max(1) as u32
    }

    /// Plaintext length of data chunk `index` (1-based).
    pub fn chunk_plain_len(&self, index: u32) -> u64 {
        let size = u64::from(self.chunk_size);
        let start = u64::from(index - 1) * size;
        self.total_len.saturating_sub(start).min(size)
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(LEN);
        v.push(VERSION);
        v.extend_from_slice(&self.endpoint.to_be_bytes());
        v.extend_from_slice(&self.module_tag);
        v.extend_from_slice(&self.total_len.to_be_bytes());
        v.extend_from_slice(&self.chunk_size.to_be_bytes());
        v.extend_from_slice(&self.chunk_count.to_be_bytes());
        v.extend_from_slice(&self.sha256);
        v.extend_from_slice(&self.created.to_be_bytes());
        v.extend_from_slice(&self.expiry.to_be_bytes());
        debug_assert_eq!(v.len(), LEN);
        v
    }

    /// Parse, and check that the fields agree with each other.
    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() != LEN || b[0] != VERSION {
            return None;
        }
        let u64_at = |i: usize| u64::from_be_bytes(b[i..i + 8].try_into().unwrap());
        let u32_at = |i: usize| u32::from_be_bytes(b[i..i + 4].try_into().unwrap());
        let m = Self {
            endpoint: u16::from_be_bytes([b[1], b[2]]),
            module_tag: b[3..11].try_into().unwrap(),
            total_len: u64_at(11),
            chunk_size: u32_at(19),
            chunk_count: u32_at(23),
            sha256: b[27..59].try_into().unwrap(),
            created: u64_at(59),
            expiry: u64_at(67),
        };
        let ok = m.chunk_size > 0
            && m.total_len > 0
            && m.chunk_count == Self::chunks_for(m.total_len, m.chunk_size);
        ok.then_some(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_consistency() {
        let m = Manifest {
            endpoint: 1,
            module_tag: [7; 8],
            total_len: 2_500_000,
            chunk_size: 1 << 20,
            chunk_count: 3,
            sha256: [1; 32],
            created: 1000,
            expiry: 2000,
        };
        let b = m.encode();
        assert_eq!(Manifest::decode(&b).unwrap(), m);
        assert_eq!(m.chunk_plain_len(1), 1 << 20);
        assert_eq!(m.chunk_plain_len(3), 2_500_000 - 2 * (1 << 20));
        let mut wrong = m.clone();
        wrong.chunk_count = 4;
        assert!(Manifest::decode(&wrong.encode()).is_none());
        assert!(Manifest::decode(&b[..74]).is_none());
    }
}
