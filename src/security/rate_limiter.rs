use std::sync::Mutex;
use std::time::{Duration, Instant};

struct Bucket {
    tokens: f64,
    updated_at: Instant,
}

pub struct RateLimiter {
    max_rps: u64,
    bucket: Mutex<Bucket>,
}

pub fn retry_after_ms(wait: Duration) -> u128 {
    wait.as_nanos().div_ceil(1_000_000).max(1)
}

impl RateLimiter {
    pub fn new(max_rps: u64) -> Self {
        Self {
            max_rps,
            bucket: Mutex::new(Bucket {
                tokens: max_rps as f64,
                updated_at: Instant::now(),
            }),
        }
    }

    pub fn check(&self) -> Result<(), Duration> {
        self.check_at(Instant::now())
    }

    fn check_at(&self, now: Instant) -> Result<(), Duration> {
        if self.max_rps == 0 {
            return Err(Duration::from_secs(1));
        }
        let mut bucket = self
            .bucket
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let elapsed = now
            .saturating_duration_since(bucket.updated_at)
            .as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * self.max_rps as f64).min(self.max_rps as f64);
        bucket.updated_at = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(())
        } else {
            Err(Duration::from_secs_f64(
                (1.0 - bucket.tokens) / self.max_rps as f64,
            ))
        }
    }
}

lazy_static::lazy_static! {
    pub static ref GLOBAL_LIMITER: RateLimiter = RateLimiter::new(50);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_bucket_refills_smoothly_and_reports_wait() {
        let limiter = RateLimiter::new(50);
        let start = Instant::now();
        for _ in 0..50 {
            assert!(limiter.check_at(start).is_ok());
        }
        assert!(limiter.check_at(start).unwrap_err().as_millis() >= 19);
        assert!(limiter.check_at(start + Duration::from_millis(10)).is_err());
        assert!(limiter.check_at(start + Duration::from_millis(20)).is_ok());
        assert!(limiter.check_at(start + Duration::from_millis(20)).is_err());
        assert_eq!(retry_after_ms(Duration::from_micros(19_001)), 20);
        assert_eq!(retry_after_ms(Duration::from_nanos(1)), 1);
    }
}
