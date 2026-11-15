//! `transport: air` -- a one-way broadcast transport for radio links.
//!
//! The input box splits each DYL1 envelope (one message or one file, the
//! same bytes lidi would carry) into chunks, seals each chunk with
//! ChaCha20-Poly1305, turns each sealed chunk into one RaptorQ source block,
//! and repeats the object on a carousel of fresh repair symbols, one symbol
//! per signed UDP datagram (DYB1, see [`packet`]).  The datagrams go to a
//! one-way radio such as wfb-ng's `udp_direct_tx`.
//!
//! Each output box collects whatever symbols reach it, decodes and opens
//! each chunk, and hands the reassembled envelope to the same envelope
//! parser and module handlers lidi feeds.  Nothing is ever sent back:
//! receivers can join late, stay silent, and be any number.
//!
//! See docs/AIR.md for the design, the defaults and their reasons.

pub mod keys;
pub mod manifest;
pub mod packet;
pub mod receiver;
pub mod schedule;
pub mod sender;

use raptorq::{EncodingPacket, ObjectTransmissionInformation, SourceBlockEncoder};
use std::time::{SystemTime, UNIX_EPOCH};

pub use receiver::AirReceiver;
pub use sender::AirSender;

/// Unix seconds now.
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// A sortable boot id: the start time in the high 32 bits, random low
/// bits.  A receiver refuses every boot older than the newest it has
/// accepted (design correction 2), so recorded traffic from an earlier run
/// of the sender cannot be replayed after the receiver restarts.
pub fn new_boot(not_before: u64) -> u64 {
    let secs = unix_now().min(u64::from(u32::MAX));
    let mut r = [0u8; 4];
    crate::auth::random_bytes(&mut r);
    let b = (secs << 32) | u64::from(u32::from_be_bytes(r));
    b.max(not_before)
}

pub fn boot_secs(boot: u64) -> u64 {
    boot >> 32
}

/// `len` rounded up to whole symbols: the RaptorQ block API wants it.
pub fn padded(len: usize, symbol: u16) -> usize {
    len.div_ceil(usize::from(symbol)).max(1) * usize::from(symbol)
}

/// Source symbols of a sealed chunk of `len` bytes.
pub fn symbols(len: usize, symbol: u16) -> u32 {
    (padded(len, symbol) / usize::from(symbol)) as u32
}

pub fn oti(padded_len: usize, symbol: u16) -> ObjectTransmissionInformation {
    ObjectTransmissionInformation::new(padded_len as u64, symbol, 1, 1, 1)
}

/// The RaptorQ encoder of one sealed chunk.
pub fn encoder(sealed: &[u8], symbol: u16) -> SourceBlockEncoder {
    let mut data = sealed.to_vec();
    data.resize(padded(sealed.len(), symbol), 0);
    SourceBlockEncoder::new(0, &oti(data.len(), symbol), &data)
}

/// The packets of one pass of one chunk, in sending order.
pub fn pass_packets(
    enc: &SourceBlockEncoder,
    k: u32,
    plan: schedule::PassPlan,
) -> Vec<EncodingPacket> {
    let mut v = if plan.source {
        enc.source_packets()
    } else {
        Vec::new()
    };
    debug_assert!(!plan.source || v.len() == k as usize);
    if plan.repair_count > 0 {
        v.extend(enc.repair_packets(plan.repair_start, plan.repair_count));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use raptorq::SourceBlockDecoder;

    #[test]
    fn boots_sort_by_time() {
        let a = new_boot(0);
        assert!(boot_secs(a) + 5 > unix_now());
        assert_eq!(new_boot(u64::MAX), u64::MAX, "never below the last boot");
    }

    /// A chunk comes back from any K of its symbols, source or repair, in
    /// any order, from any pass.
    #[test]
    fn chunk_decodes_from_later_pass_only() {
        let t = 64u16;
        let sealed: Vec<u8> = (0..1000u32).map(|i| (i * 31 % 251) as u8).collect();
        let k = symbols(sealed.len(), t);
        assert_eq!(k, 16);
        let enc = encoder(&sealed, t);
        let mut dec = SourceBlockDecoder::new(
            0,
            &oti(padded(sealed.len(), t), t),
            padded(sealed.len(), t) as u64,
        );
        // pass 3 only, every other packet lost but enough left
        let p3 = pass_packets(&enc, k, schedule::pass_plan(k, 20, 3));
        let mut out = None;
        for p in p3.into_iter().rev() {
            if let Some(d) = dec.decode(std::iter::once(p)) {
                out = Some(d);
                break;
            }
        }
        let mut out = out.expect("decodes");
        out.truncate(sealed.len());
        assert_eq!(out, sealed);
    }
}
