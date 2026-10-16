//! Timelock policy for the follower (counter-party) leg.
//!
//! Whoever holds the secret can wait until the last moment of the follower's
//! lock to reveal it. The follower then needs `safety_window` to see the secret
//! and redeem on the other chain before that lock expires, so:
//!
//! ```text
//! follower_expiry + safety_window <= initiator_expiry
//! ```

#[derive(Clone, Copy, Debug)]
pub struct TimelockPolicy {
    pub safety_window: u64,
    pub min_follower_lock: u64,
}

impl Default for TimelockPolicy {
    fn default() -> Self {
        Self { safety_window: 2 * 3600, min_follower_lock: 30 * 60 }
    }
}

impl TimelockPolicy {
    /// Expiry for the follower's lock, or `None` if it would be unsafe or too short.
    pub fn follower_expiry(&self, initiator_expiry: u64, now: u64) -> Option<u64> {
        let expiry = initiator_expiry.checked_sub(self.safety_window)?;
        (expiry.saturating_sub(now) >= self.min_follower_lock).then_some(expiry)
    }

    pub fn is_safe(&self, follower_expiry: u64, initiator_expiry: u64) -> bool {
        follower_expiry.saturating_add(self.safety_window) <= initiator_expiry
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: TimelockPolicy = TimelockPolicy { safety_window: 7200, min_follower_lock: 1800 };

    #[test]
    fn leaves_safety_window() {
        assert_eq!(P.follower_expiry(100_000, 0), Some(100_000 - 7200));
        assert!(P.is_safe(100_000 - 7200, 100_000));
        assert!(!P.is_safe(100_000 - 7199, 100_000));
    }

    #[test]
    fn rejects_short_locks() {
        assert_eq!(P.follower_expiry(1_000 + 7200 + 1799, 1_000), None);
        assert!(P.follower_expiry(1_000 + 7200 + 1800, 1_000).is_some());
        assert_eq!(P.follower_expiry(10, 0), None);
    }
}
