use embassy_time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenBucket {
    tokens: u8,
    capacity: u8,
    rate_per_sec: u8,
    last_refill: Instant,
    violations: u8,
}

impl TokenBucket {
    pub const fn new(capacity: u8, rate_per_sec: u8) -> Self {
        Self {
            tokens: capacity,
            capacity,
            rate_per_sec,
            last_refill: Instant::from_ticks(0),
            violations: 0,
        }
    }

    pub fn try_consume(&mut self, now: Instant) -> bool {
        if let Some(elapsed) = now.checked_duration_since(self.last_refill) {
            let elapsed_secs = elapsed.as_secs();
            if elapsed_secs > 0 {
                let refill = (elapsed_secs.saturating_mul(self.rate_per_sec as u64))
                    .min(u8::MAX as u64) as u8;
                self.tokens = self.tokens.saturating_add(refill).min(self.capacity);
                self.last_refill = now;
            }
        }

        if self.tokens > 0 {
            self.tokens -= 1;
            self.violations = 0;
            true
        } else {
            self.violations = self.violations.saturating_add(1);
            false
        }
    }

    pub fn reset_violations(&mut self) {
        self.violations = 0;
    }

    pub const fn violations(&self) -> u8 {
        self.violations
    }
}

impl Default for TokenBucket {
    fn default() -> Self {
        Self::new(0, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::TokenBucket;
    use embassy_time::Instant;

    #[test]
    fn starts_with_full_capacity_and_allows_initial_burst() {
        let mut bucket = TokenBucket::new(20, 10);
        let t0 = Instant::from_secs(0);

        for _ in 0..20 {
            assert!(bucket.try_consume(t0));
        }

        assert_eq!(bucket.tokens, 0);
        assert_eq!(bucket.violations(), 0);
    }

    #[test]
    fn twenty_first_consume_fails_and_increments_violations() {
        let mut bucket = TokenBucket::new(20, 10);
        let t0 = Instant::from_secs(0);

        for _ in 0..20 {
            assert!(bucket.try_consume(t0));
        }

        assert!(!bucket.try_consume(t0));
        assert_eq!(bucket.violations(), 1);
    }

    #[test]
    fn one_second_refills_rate_tokens() {
        let mut bucket = TokenBucket::new(20, 10);
        let t0 = Instant::from_secs(0);

        for _ in 0..20 {
            assert!(bucket.try_consume(t0));
        }

        assert!(bucket.try_consume(Instant::from_secs(1)));
        assert_eq!(bucket.tokens, 9);
        assert_eq!(bucket.violations(), 0);
    }

    #[test]
    fn refill_does_not_exceed_capacity() {
        let mut bucket = TokenBucket::new(20, 10);
        let t0 = Instant::from_secs(0);

        for _ in 0..5 {
            assert!(bucket.try_consume(t0));
        }

        assert!(bucket.try_consume(Instant::from_secs(10)));
        assert_eq!(bucket.tokens, 19);
    }

    #[test]
    fn fifty_consecutive_violations_are_counted() {
        let mut bucket = TokenBucket::new(1, 0);
        let t0 = Instant::from_secs(0);

        assert!(bucket.try_consume(t0));
        for _ in 0..50 {
            assert!(!bucket.try_consume(t0));
        }

        assert_eq!(bucket.violations(), 50);
    }

    #[test]
    fn successful_consume_resets_violations() {
        let mut bucket = TokenBucket::new(1, 1);
        let t0 = Instant::from_secs(0);

        assert!(bucket.try_consume(t0));
        assert!(!bucket.try_consume(t0));
        assert!(!bucket.try_consume(t0));
        assert_eq!(bucket.violations(), 2);

        assert!(bucket.try_consume(Instant::from_secs(1)));
        assert_eq!(bucket.violations(), 0);
    }

    #[test]
    fn reset_violations_clears_counter() {
        let mut bucket = TokenBucket::new(1, 0);
        let t0 = Instant::from_secs(0);

        assert!(bucket.try_consume(t0));
        assert!(!bucket.try_consume(t0));
        assert_eq!(bucket.violations(), 1);

        bucket.reset_violations();
        assert_eq!(bucket.violations(), 0);
    }
}
