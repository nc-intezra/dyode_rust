//! Hex encoding (the key file format, and digests in the event log).

pub fn encode(bytes: impl AsRef<[u8]>) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let b = bytes.as_ref();
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push(DIGITS[usize::from(x >> 4)] as char);
        s.push(DIGITS[usize::from(x & 15)] as char);
    }
    s
}

/// None if `text` is not an even number of hex digits.
pub fn decode(text: &str) -> Option<Vec<u8>> {
    let t = text.as_bytes();
    if t.len() % 2 != 0 {
        return None;
    }
    let val = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    t.chunks(2)
        .map(|p| Some((val(p[0])? << 4) | val(p[1])?))
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn roundtrip() {
        assert_eq!(super::encode([0u8, 0xab, 0xff]), "00abff");
        assert_eq!(super::decode("00ABff").unwrap(), vec![0, 0xab, 0xff]);
        assert!(super::decode("0g").is_none());
        assert!(super::decode("abc").is_none());
    }
}
