//! Bandwidth throttling using token bucket algorithm.
//!
//! Provides a thread-safe token bucket implementation to limit
//! the rate of data transfer (bytes per second).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use std::sync::Arc;

/// Token bucket for bandwidth throttling.
///
/// This implementation uses an atomic counter for the available tokens
/// and tracks the last refill time to add tokens proportionally to elapsed time.
#[derive(Debug)]
pub struct Throttle {
    /// Maximum bytes allowed per second
    max_bytes_per_sec: AtomicU64,
    /// Current available tokens (bytes)
    tokens: AtomicU64,
    /// Last time tokens were refilled
    last_refill: parking_lot::Mutex<Instant>,
    /// Minimum burst size (tokens always available)
    burst_size: u64,
}

impl Throttle {
    /// Create a new throttle with the given bytes/second limit.
    ///
    /// # Arguments
    /// * `max_bytes_per_sec` - Maximum throughput in bytes per second. If 0, no throttling is applied.
    /// * `burst_size` - Initial burst allowance (tokens available immediately).
    pub fn new(max_bytes_per_sec: u64, burst_size: u64) -> Self {
        let now = Instant::now();
        Self {
            max_bytes_per_sec: AtomicU64::new(max_bytes_per_sec),
            tokens: AtomicU64::new(burst_size),
            last_refill: parking_lot::Mutex::new(now),
            burst_size,
        }
    }

    /// Create an unlimited throttle (no throttling applied).
    pub fn unlimited() -> Self {
        Self::new(0, 0)
    }

    /// Update the maximum bytes per second limit.
    pub fn set_limit(&self, max_bytes_per_sec: u64) {
        self.max_bytes_per_sec.store(max_bytes_per_sec, Ordering::Relaxed);
    }

    /// Get the current limit in bytes per second.
    pub fn get_limit(&self) -> u64 {
        self.max_bytes_per_sec.load(Ordering::Relaxed)
    }

    /// Check if throttling is enabled.
    pub fn is_limited(&self) -> bool {
        self.max_bytes_per_sec.load(Ordering::Relaxed) > 0
    }

    /// Refill tokens based on elapsed time.
    fn refill(&self) {
        let max = self.max_bytes_per_sec.load(Ordering::Relaxed);
        if max == 0 {
            return; // Unlimited
        }

        let mut last_refill = self.last_refill.lock();
        let now = Instant::now();
        let elapsed = now.duration_since(*last_refill);
        
        // Calculate tokens to add based on elapsed time
        let tokens_to_add = (elapsed.as_secs_f64() * max as f64) as u64;
        
        if tokens_to_add > 0 {
            // Cap tokens at max + burst_size to prevent accumulation
            let max_tokens = max + self.burst_size;
            let current = self.tokens.load(Ordering::Relaxed);
            let new_tokens = std::cmp::min(current + tokens_to_add, max_tokens);
            self.tokens.store(new_tokens, Ordering::Relaxed);
            *last_refill = now;
        }
    }

    /// Try to consume the specified number of bytes.
    ///
    /// Returns `true` if the bytes were consumed immediately,
    /// `false` if we need to wait (not enough tokens).
    pub fn try_consume(&self, bytes: u64) -> bool {
        let max = self.max_bytes_per_sec.load(Ordering::Relaxed);
        if max == 0 {
            return true; // Unlimited
        }

        self.refill();

        let mut current = self.tokens.load(Ordering::Relaxed);
        loop {
            if current >= bytes {
                match self.tokens.compare_exchange_weak(
                    current,
                    current - bytes,
                    Ordering::SeqCst,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return true,
                    Err(actual) => current = actual,
                }
            } else {
                return false;
            }
        }
    }

    /// Consume the specified number of bytes, waiting if necessary.
    ///
    /// This method will block until enough tokens are available.
    pub fn consume(&self, bytes: u64) {
        let max = self.max_bytes_per_sec.load(Ordering::Relaxed);
        if max == 0 {
            return; // Unlimited
        }

        // If bytes requested is larger than max, we need to handle it in chunks
        if bytes > max {
            // Consume in chunks of max
            let mut remaining = bytes;
            while remaining > 0 {
                let chunk = std::cmp::min(remaining, max);
                self.consume_chunk(chunk);
                remaining -= chunk;
            }
        } else {
            self.consume_chunk(bytes);
        }
    }

    /// Consume a chunk of bytes (<= max_bytes_per_sec).
    fn consume_chunk(&self, bytes: u64) {
        loop {
            self.refill();
            
            let current = self.tokens.load(Ordering::Relaxed);
            if current >= bytes {
                match self.tokens.compare_exchange_weak(
                    current,
                    current - bytes,
                    Ordering::SeqCst,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return,
                    Err(_) => continue, // Retry on CAS failure
                }
            } else {
                // Calculate how long to wait for enough tokens
                let needed = bytes - current;
                let max = self.max_bytes_per_sec.load(Ordering::Relaxed);
                
                // Time to wait in seconds
                let wait_time_secs = needed as f64 / max as f64;
                let wait_duration = Duration::from_secs_f64(wait_time_secs);
                
                // Sleep for a portion of the wait time (to allow for concurrent updates)
                std::thread::sleep(std::cmp::min(wait_duration, Duration::from_millis(50)));
            }
        }
    }
}

/// Shared throttle handle that can be cloned and shared across threads.
#[derive(Debug, Clone)]
pub struct ThrottleHandle {
    inner: Arc<Throttle>,
}

impl ThrottleHandle {
    /// Create a new throttle handle.
    pub fn new(max_bytes_per_sec: u64, burst_size: u64) -> Self {
        Self {
            inner: Arc::new(Throttle::new(max_bytes_per_sec, burst_size)),
        }
    }

    /// Create an unlimited throttle handle.
    pub fn unlimited() -> Self {
        Self {
            inner: Arc::new(Throttle::unlimited()),
        }
    }

    /// Consume bytes, waiting if necessary.
    pub fn consume(&self, bytes: u64) {
        self.inner.consume(bytes);
    }

    /// Try to consume bytes without waiting.
    pub fn try_consume(&self, bytes: u64) -> bool {
        self.inner.try_consume(bytes)
    }

    /// Update the limit dynamically.
    pub fn set_limit(&self, max_bytes_per_sec: u64) {
        self.inner.set_limit(max_bytes_per_sec);
    }

    /// Get the current limit.
    pub fn get_limit(&self) -> u64 {
        self.inner.get_limit()
    }

    /// Check if throttling is active.
    pub fn is_limited(&self) -> bool {
        self.inner.is_limited()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Instant;

    #[test]
    fn test_unlimited_throttle() {
        let throttle = ThrottleHandle::unlimited();
        assert!(!throttle.is_limited());
        
        let start = Instant::now();
        throttle.consume(1_000_000_000); // Should return immediately
        let elapsed = start.elapsed();
        
        assert!(elapsed.as_millis() < 10, "Unlimited throttle should not delay");
    }

    #[test]
    fn test_try_consume_success() {
        let throttle = ThrottleHandle::new(1000, 1000); // 1KB/s with 1KB burst
        
        // Should succeed immediately due to burst
        assert!(throttle.try_consume(500));
        assert!(throttle.try_consume(500));
        
        // Should fail now (no tokens left)
        assert!(!throttle.try_consume(1));
    }

    #[test]
    fn test_try_consume_failure_and_refill() {
        let throttle = ThrottleHandle::new(1000, 100); // 1KB/s with 100B burst
        
        // Consume all burst tokens
        assert!(throttle.try_consume(100));
        assert!(!throttle.try_consume(1));
        
        // Wait for refill (100ms should give us ~100 tokens at 1000B/s)
        thread::sleep(Duration::from_millis(110));
        
        // Should have tokens again
        assert!(throttle.try_consume(50));
    }

    #[test]
    fn test_consume_blocks() {
        let throttle = ThrottleHandle::new(1000, 0); // 1KB/s, no burst
        
        let start = Instant::now();
        throttle.consume(500); // Should take ~500ms
        let elapsed = start.elapsed();
        
        // Allow some variance (at least 400ms)
        assert!(elapsed.as_millis() >= 400, 
            "Expected ~500ms delay, got {:?}", elapsed);
        assert!(elapsed.as_millis() < 700, 
            "Delay too long: {:?}", elapsed);
    }

    #[test]
    fn test_dynamic_limit_change() {
        let throttle = ThrottleHandle::new(1000, 1000); // 1KB/s
        
        // Consume burst
        throttle.consume(1000);
        
        // Change limit to 2KB/s
        throttle.set_limit(2000);
        
        // Wait a bit and check we get tokens at new rate
        thread::sleep(Duration::from_millis(100));
        
        // At 2KB/s, 100ms should give ~200 tokens
        assert!(throttle.try_consume(150));
    }

    #[test]
    fn test_large_consumption() {
        let throttle = ThrottleHandle::new(1000, 0); // 1KB/s
        
        let start = Instant::now();
        throttle.consume(3000); // 3KB should take ~3 seconds
        let elapsed = start.elapsed();
        
        // Allow variance but should be around 3 seconds
        assert!(elapsed.as_secs() >= 2, 
            "Expected ~3s delay, got {:?}", elapsed);
        assert!(elapsed.as_secs() < 5, 
            "Delay too long: {:?}", elapsed);
    }
}
