//! Client-side token bucket mirroring Hardcover's limits (burst 10, 60/min).
//!
//! We run slightly under the server's burst (8) so other clients of the same
//! account (the limit is per user, not per token) leave us some headroom.

use std::sync::Mutex;
use std::time::{Duration, Instant};

pub(crate) const DEFAULT_BURST: u32 = 8;
pub(crate) const DEFAULT_REFILL_PER_SEC: f64 = 1.0;

#[derive(Debug)]
pub(crate) struct TokenBucket {
    capacity: f64,
    refill_per_sec: f64,
    state: Mutex<State>,
}

#[derive(Debug)]
struct State {
    /// May go negative: callers reserve tokens up front and then sleep off the debt,
    /// which queues concurrent callers fairly without holding the lock while sleeping.
    tokens: f64,
    last: Instant,
}

impl TokenBucket {
    pub fn new(capacity: u32, refill_per_sec: f64) -> Self {
        Self::new_at(capacity, refill_per_sec, Instant::now())
    }

    fn new_at(capacity: u32, refill_per_sec: f64, now: Instant) -> Self {
        assert!(capacity > 0 && refill_per_sec > 0.0);
        Self {
            capacity: f64::from(capacity),
            refill_per_sec,
            state: Mutex::new(State {
                tokens: f64::from(capacity),
                last: now,
            }),
        }
    }

    fn refill(&self, s: &mut State, now: Instant) {
        let elapsed = now.saturating_duration_since(s.last).as_secs_f64();
        s.tokens = (s.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        s.last = now;
    }

    /// Takes `cost` tokens and returns how long the caller must wait before sending.
    pub fn reserve_at(&self, cost: u32, now: Instant) -> Duration {
        let cost = f64::from(cost).min(self.capacity);
        let mut s = self.state.lock().unwrap_or_else(|p| p.into_inner());
        self.refill(&mut s, now);
        s.tokens -= cost;
        if s.tokens >= 0.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(-s.tokens / self.refill_per_sec)
        }
    }

    /// Blocks until `cost` tokens are available, then consumes them.
    pub fn acquire(&self, cost: u32) {
        let wait = self.reserve_at(cost, Instant::now());
        if !wait.is_zero() {
            std::thread::sleep(wait);
        }
    }

    /// The server said 429: empty the bucket and push the next slot `retry_after` out.
    pub fn penalize_at(&self, retry_after: Option<Duration>, now: Instant) {
        let mut s = self.state.lock().unwrap_or_else(|p| p.into_inner());
        self.refill(&mut s, now);
        let debt = retry_after.map_or(0.0, |d| d.as_secs_f64() * self.refill_per_sec);
        s.tokens = s.tokens.min(0.0) - debt;
    }

    pub fn penalize(&self, retry_after: Option<Duration>) {
        self.penalize_at(retry_after, Instant::now());
    }
}

impl Default for TokenBucket {
    fn default() -> Self {
        Self::new(DEFAULT_BURST, DEFAULT_REFILL_PER_SEC)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(s: f64) -> Duration {
        Duration::from_secs_f64(s)
    }

    #[test]
    fn burst_then_one_per_second() {
        let t0 = Instant::now();
        let b = TokenBucket::new_at(8, 1.0, t0);
        for _ in 0..8 {
            assert_eq!(b.reserve_at(1, t0), Duration::ZERO);
        }
        // 9th and 10th wait 1s and 2s respectively.
        assert_eq!(b.reserve_at(1, t0), secs(1.0));
        assert_eq!(b.reserve_at(1, t0), secs(2.0));
    }

    #[test]
    fn refills_over_time_up_to_capacity() {
        let t0 = Instant::now();
        let b = TokenBucket::new_at(8, 1.0, t0);
        for _ in 0..8 {
            b.reserve_at(1, t0);
        }
        // 3 seconds later 3 tokens are back.
        let t3 = t0 + secs(3.0);
        for _ in 0..3 {
            assert_eq!(b.reserve_at(1, t3), Duration::ZERO);
        }
        assert!(b.reserve_at(1, t3) > Duration::ZERO);
        // A long idle period never exceeds the capacity.
        let b = TokenBucket::new_at(8, 1.0, t0);
        let later = t0 + secs(3600.0);
        for _ in 0..8 {
            assert_eq!(b.reserve_at(1, later), Duration::ZERO);
        }
        assert_eq!(b.reserve_at(1, later), secs(1.0));
    }

    #[test]
    fn cost_is_capped_at_capacity() {
        let t0 = Instant::now();
        let b = TokenBucket::new_at(8, 1.0, t0);
        assert_eq!(b.reserve_at(100, t0), Duration::ZERO);
        assert_eq!(b.reserve_at(1, t0), secs(1.0));
    }

    #[test]
    fn penalize_respects_retry_after() {
        let t0 = Instant::now();
        let b = TokenBucket::new_at(8, 1.0, t0);
        b.penalize_at(Some(secs(5.0)), t0);
        assert_eq!(b.reserve_at(1, t0), secs(6.0));
        let b = TokenBucket::new_at(8, 1.0, t0);
        b.penalize_at(None, t0);
        assert_eq!(b.reserve_at(1, t0), secs(1.0));
    }

    #[test]
    fn acquire_blocks_for_the_debt() {
        let b = TokenBucket::new(1, 20.0);
        let start = Instant::now();
        b.acquire(1);
        b.acquire(1); // must wait ~50ms
        assert!(start.elapsed() >= Duration::from_millis(45));
    }
}
