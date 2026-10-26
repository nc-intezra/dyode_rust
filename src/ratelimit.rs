//! Token buckets.  A diode has no flow control, so the only way not to
//! overrun the receiver (or starve another module of the shared link) is
//! for the sender to pace itself.

use std::sync::Mutex;
use std::time::{Duration, Instant};

struct Inner {
    rate: f64,
    capacity: f64,
    tokens: f64,
    last: Instant,
}

/// `rate` bytes per second, bursts up to `capacity` bytes.
pub struct TokenBucket {
    inner: Mutex<Inner>,
}

impl TokenBucket {
    pub fn new(rate_bytes_per_s: f64, burst_bytes: f64) -> Self {
        Self {
            inner: Mutex::new(Inner {
                rate: rate_bytes_per_s.max(1.0),
                capacity: burst_bytes.max(1.0),
                tokens: burst_bytes.max(1.0),
                last: Instant::now(),
            }),
        }
    }

    /// A bucket for `mbps` megabits per second with ~50 ms of burst (at
    /// least one 256 KiB read, so a lidi block is never split needlessly).
    pub fn from_mbps(mbps: f64) -> Self {
        let rate = mbps * 1e6 / 8.0;
        Self::new(rate, (rate * 0.05).max(256.0 * 1024.0))
    }

    fn refill(i: &mut Inner) {
        let now = Instant::now();
        i.tokens = (i.tokens + now.duration_since(i.last).as_secs_f64() * i.rate).min(i.capacity);
        i.last = now;
    }

    /// Take `amount` if available now.  A request larger than the burst
    /// could never be satisfied, so the bucket grows to fit it.
    pub fn try_take(&self, amount: f64) -> bool {
        let mut i = self.inner.lock().unwrap();
        if amount > i.capacity {
            i.capacity = amount;
        }
        Self::refill(&mut i);
        // The epsilon stops float rounding leaving the bucket a hair short forever.
        if i.tokens + 1e-6 >= amount {
            i.tokens = (i.tokens - amount).max(0.0);
            true
        } else {
            false
        }
    }

    /// Seconds until `amount` would be available.
    pub fn wait_time(&self, amount: f64) -> Duration {
        let mut i = self.inner.lock().unwrap();
        Self::refill(&mut i);
        Duration::from_secs_f64(((amount - i.tokens) / i.rate).max(0.001))
    }

    /// Block until `amount` bytes may be sent.  Large amounts are taken in
    /// burst-sized slices so pacing stays smooth.
    pub fn take(&self, amount: usize) {
        let mut left = amount as f64;
        while left > 0.0 {
            let cap = self.inner.lock().unwrap().capacity;
            let slice = left.min(cap);
            while !self.try_take(slice) {
                std::thread::sleep(self.wait_time(slice).min(Duration::from_millis(200)));
            }
            left -= slice;
        }
    }
}
