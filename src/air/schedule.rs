//! The carousel's arithmetic, kept free of I/O so it can be tested.

use std::ops::Range;

/// Data chunks `1..=n` grouped into interleaving windows.
///
/// Every window holds at least `depth` chunks (design correction 10): a
/// short tail window would be wiped by one fade.  The remainder is spread
/// over the full windows instead, so windows hold `depth` to `2*depth - 1`
/// chunks.  Fewer than `depth` chunks make one window.
pub fn windows(n: u32, depth: u32) -> Vec<Range<u32>> {
    let depth = depth.max(1);
    if n == 0 {
        return Vec::new();
    }
    let count = (n / depth).max(1);
    let (base, extra) = (n / count, n % count);
    let mut out = Vec::with_capacity(count as usize);
    let mut start = 1;
    for i in 0..count {
        let len = base + u32::from(i < extra);
        out.push(start..start + len);
        start += len;
    }
    out
}

/// What one pass sends of a chunk of `k` source symbols.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PassPlan {
    /// Send the `k` source symbols (pass 1 only).
    pub source: bool,
    /// First repair symbol index (0 = the first repair symbol).
    pub repair_start: u32,
    pub repair_count: u32,
}

impl PassPlan {
    pub fn packets(&self, k: u32) -> u32 {
        if self.source {
            k + self.repair_count
        } else {
            self.repair_count
        }
    }
}

/// Repair symbols of the first pass: `repair` percent of `k`, rounded up.
pub fn first_repair(k: u32, repair_pct: u16) -> u32 {
    (u64::from(k) * u64::from(repair_pct)).div_ceil(100) as u32
}

/// RaptorQ encoding symbol ids are 24 bits; the extended block size K' is
/// at most 56403 above K.
const ESI_LIMIT: u64 = 1 << 24;
const MAX_EXTENDED: u64 = 56_403;

/// Pass `pass` (1-based) of a chunk of `k` source symbols.
///
/// Pass 1 sends the source symbols and `repair`% repair symbols.  Every
/// later pass sends K·(1+r) *new* repair symbols (design correction 4), so
/// a receiver that missed pass 1 entirely can still finish from any one
/// later pass.  Symbol ids wrap long before the 24-bit limit, which only a
/// `latest` or `until` object repeated thousands of times reaches.
pub fn pass_plan(k: u32, repair_pct: u16, pass: u32) -> PassPlan {
    let r1 = first_repair(k, repair_pct);
    if pass <= 1 {
        return PassPlan {
            source: true,
            repair_start: 0,
            repair_count: r1,
        };
    }
    let later = k + r1;
    let raw = u64::from(r1) + u64::from(pass - 2) * u64::from(later);
    let room = ESI_LIMIT - MAX_EXTENDED - u64::from(later);
    PassPlan {
        source: false,
        repair_start: (raw % room) as u32,
        repair_count: later,
    }
}

/// Manifest symbols in a pass of `data_packets` packets: at least 8
/// (design correction 11, so a late joiner of a small object still gets
/// one), and one per 128 packets on large objects.
pub fn manifest_symbols(data_packets: u64) -> u32 {
    (data_packets / 128).max(8).min(u64::from(u32::MAX)) as u32
}

/// Spreads `m` manifest packets evenly over `p` data packets.
#[derive(Clone, Debug)]
pub struct Spacer {
    p: u64,
    m: u64,
    emitted: u64,
    sent: u64,
}

impl Spacer {
    pub fn new(data_packets: u64, manifest_packets: u32) -> Self {
        Self {
            p: data_packets.max(1),
            m: u64::from(manifest_packets),
            emitted: 0,
            sent: 0,
        }
    }

    /// Call before each data packet: how many manifest packets go first.
    pub fn before_data(&mut self) -> u32 {
        let mut n = 0;
        while self.sent < self.m && self.sent * self.p <= self.emitted * self.m {
            self.sent += 1;
            n += 1;
        }
        self.emitted += 1;
        n
    }

    /// At the end of the pass: manifest packets still owed.
    pub fn remaining(&mut self) -> u32 {
        let n = self.m - self.sent;
        self.sent = self.m;
        n as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_are_balanced_never_short() {
        assert!(windows(0, 16).is_empty());
        assert_eq!(windows(5, 16), vec![1..6]);
        assert_eq!(windows(16, 16), vec![1..17]);
        // 33 chunks: two windows of 17 and 16, not 16 + 16 + 1
        assert_eq!(windows(33, 16), vec![1..18, 18..34]);
        for n in 1..400 {
            let w = windows(n, 16);
            assert_eq!(w.first().unwrap().start, 1);
            assert_eq!(w.last().unwrap().end, n + 1);
            for (a, b) in w.iter().zip(w.iter().skip(1)) {
                assert_eq!(a.end, b.start);
            }
            if n >= 16 {
                assert!(
                    w.iter().all(|r| r.len() >= 16 && r.len() < 32),
                    "{n}: {w:?}"
                );
            }
        }
    }

    #[test]
    fn later_passes_send_fresh_repair() {
        let k = 820;
        let p1 = pass_plan(k, 20, 1);
        assert_eq!(
            p1,
            PassPlan {
                source: true,
                repair_start: 0,
                repair_count: 164
            }
        );
        assert_eq!(p1.packets(k), 984);
        let p2 = pass_plan(k, 20, 2);
        assert_eq!(
            p2,
            PassPlan {
                source: false,
                repair_start: 164,
                repair_count: 984
            }
        );
        let p3 = pass_plan(k, 20, 3);
        assert_eq!(p3.repair_start, 164 + 984);
        // a tiny object still gets repair in every pass
        assert_eq!(pass_plan(1, 20, 1).repair_count, 1);
        assert_eq!(pass_plan(1, 20, 5).repair_count, 2);
        // ids stay valid for ever
        let far = pass_plan(56_000, 400, 1_000_000);
        assert!(u64::from(far.repair_start) + u64::from(far.repair_count) + 56_403 < 1 << 24);
    }

    #[test]
    fn manifest_spread() {
        assert_eq!(manifest_symbols(3), 8);
        assert_eq!(manifest_symbols(128 * 100), 100);
        for (p, m) in [(3u64, 8u32), (1000, 8), (12800, 100)] {
            let mut s = Spacer::new(p, m);
            let mut at = Vec::new();
            for i in 0..p {
                for _ in 0..s.before_data() {
                    at.push(i);
                }
            }
            let rest = s.remaining();
            assert_eq!(at.len() as u32 + rest, m);
            assert_eq!(at[0], 0, "a manifest symbol leads the pass");
            if p >= u64::from(m) {
                assert_eq!(rest, 0);
                // evenly: gaps differ by at most one
                let gaps: Vec<u64> = at.windows(2).map(|w| w[1] - w[0]).collect();
                let (lo, hi) = (gaps.iter().min().unwrap(), gaps.iter().max().unwrap());
                assert!(hi - lo <= 1, "{gaps:?}");
            }
        }
    }
}
