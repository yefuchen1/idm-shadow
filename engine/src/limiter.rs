//! 限速：令牌桶。
//!
//! 全局一个桶、单任务各一个桶。下载线程写盘前先向桶里取令牌，
//! 桶里没有就等。这样限速对所有连接同时生效，而不是每条连接各限各的。

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 令牌桶。`limit` 为 `None` 表示不限速。
pub struct RateLimiter {
    inner: Mutex<Inner>,
}

struct Inner {
    limit: Option<u64>,
    tokens: f64,
    last: Instant,
}

impl RateLimiter {
    pub fn new(limit: Option<u64>) -> Self {
        Self {
            inner: Mutex::new(Inner {
                limit,
                tokens: limit.map(|l| l as f64).unwrap_or(0.0),
                last: Instant::now(),
            }),
        }
    }

    pub fn limit(&self) -> Option<u64> {
        self.inner.lock().unwrap().limit
    }

    /// 运行时改限速值。
    pub fn set_limit(&self, limit: Option<u64>) {
        let mut inner = self.inner.lock().unwrap();
        inner.limit = limit;
        if let Some(l) = limit {
            inner.tokens = inner.tokens.min(l as f64);
        }
    }

    /// 等待直到可以取走 `n` 字节的配额。
    pub async fn acquire(&self, n: u64) {
        loop {
            let wait = {
                let mut inner = self.inner.lock().unwrap();
                let Some(limit) = inner.limit else {
                    return;
                };
                if limit == 0 {
                    // 0 表示暂停传输，但保留可恢复的能力
                    drop(inner);
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    continue;
                }

                let now = Instant::now();
                let elapsed = now.duration_since(inner.last).as_secs_f64();
                inner.last = now;
                inner.tokens = (inner.tokens + elapsed * limit as f64).min(limit as f64);

                if inner.tokens >= n as f64 {
                    inner.tokens -= n as f64;
                    return;
                }

                let missing = n as f64 - inner.tokens;
                let secs = missing / limit as f64;
                Duration::from_secs_f64(secs.clamp(0.01, 1.0))
            };
            tokio::time::sleep(wait).await;
        }
    }

    /// 同步取配额，供非 async 场景使用（会阻塞）。
    pub fn try_acquire(&self, n: u64) -> bool {
        let mut inner = self.inner.lock().unwrap();
        let Some(limit) = inner.limit else {
            return true;
        };
        if limit == 0 {
            return false;
        }
        let now = Instant::now();
        let elapsed = now.duration_since(inner.last).as_secs_f64();
        inner.last = now;
        inner.tokens = (inner.tokens + elapsed * limit as f64).min(limit as f64);
        if inner.tokens >= n as f64 {
            inner.tokens -= n as f64;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlimited_always_acquires() {
        let limiter = RateLimiter::new(None);
        assert!(limiter.try_acquire(1_000_000));
    }

    #[test]
    fn zero_limit_blocks() {
        let limiter = RateLimiter::new(Some(0));
        assert!(!limiter.try_acquire(1));
    }

    #[tokio::test]
    async fn limited_bucket_delays() {
        // 1000 B/s，先拿走 500，剩下的要等
        let limiter = RateLimiter::new(Some(1000));
        assert!(limiter.try_acquire(500));
        assert!(!limiter.try_acquire(900));

        let start = Instant::now();
        limiter.acquire(300).await;
        assert!(start.elapsed() >= Duration::from_millis(200));
    }

    #[tokio::test]
    async fn set_limit_applies_immediately() {
        let limiter = RateLimiter::new(None);
        assert!(limiter.try_acquire(10_000));
        limiter.set_limit(Some(100));
        assert!(!limiter.try_acquire(10_000));
    }
}
