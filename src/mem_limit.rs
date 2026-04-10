//! A permit system to limit total RAM usage by a subsystem.
//!
//! We use this to provide backpressure when loading images, so that we don't
//! run out of RAM when processing 100s of large images at once.

use std::{fmt, str::FromStr, sync::Arc, time::Duration};

use bytesize::ByteSize;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::{prelude::*, timeouts::WithTimeout};

/// A reasonable deadlock timeout for acquiring memory permits.
///
/// Currently, deadlocks _shouldn't_ be able to happen, and it's possible in
/// some cases that drivers might block for a while waiting for RAM. So we set
/// this _very_ high, as a last resort.
pub const MEM_PERMIT_DEADLOCK_TIMEOUT: Duration = Duration::from_mins(15);

/// A memory limit parsed from a CLI string like `"2G"`, `"500M"`, or `"4096k"`.
///
/// This is analogous to [`crate::rate_limit::RateLimit`]: it's a
/// cloneable/serializable config value that lives in CLI options, and can be
/// converted to a runtime [`MemLimiter`] via [`MemLimit::to_mem_limiter`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemLimit(ByteSize);

impl MemLimit {
    /// Construct the runtime [`MemLimiter`] from this limit.
    pub fn to_mem_limiter(&self, acquire_timeout: Option<Duration>) -> MemLimiter {
        MemLimiter::byte_limit(self.0.as_u64() as usize, acquire_timeout)
    }
}

impl FromStr for MemLimit {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let byte_size = s
            .parse::<ByteSize>()
            .map_err(|e| anyhow!("Failed to parse memory limit: {e}"))?;
        Ok(Self(byte_size))
    }
}

impl fmt::Display for MemLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Scaling factor for converting between bytes and the internal kilobyte units used by
/// our semaphore.
const KB_SCALE_FACTOR: usize = 1024;

/// A permit to use a certain amount of RAM. This is an opaque handle which is
/// returned when acquiring a permit, and released when dropped. Acquiring a
/// permit is a blocking async operation, but releasing a permit is done using
/// ordinary sync drop.
#[derive(Debug)]
pub struct MemPermit {
    _permit: OwnedSemaphorePermit,
}

/// Manages a limit on total RAM usage, and provides [`MemPermit`]s to acquire
/// permits to use RAM. This is used to limit RAM usage when loading images,
/// providing backpressure if a we would otherwise load too many large images at
/// once.
#[derive(Debug)]
pub struct MemLimiter {
    /// Total RAM limit in bytes. This is used to check for code that tries to
    /// acquire permits for more RAM than the total limit, which is always an
    /// error.
    ram_limit_bytes: usize,

    /// Timeout for acquiring a permit, if any. This is used to catch deadlocks.
    acquire_timeout: Option<Duration>,

    /// Async semaphore used to manage the permits. One important detail: We
    /// need to handle more than 4GB of memory, so this internal semaphore
    /// counts in _kilobytes_ of memory (rounded up), not bytes.
    semaphore: Arc<Semaphore>,
}

impl MemLimiter {
    /// Create a new [`MemLimit`] with the given total RAM limit in bytes.
    pub fn byte_limit(ram_limit_bytes: usize, acquire_timeout: Option<Duration>) -> Self {
        let total_ram_limit_kb = ram_limit_bytes.div_ceil(KB_SCALE_FACTOR);
        Self {
            ram_limit_bytes,
            acquire_timeout,
            semaphore: Arc::new(Semaphore::new(total_ram_limit_kb)),
        }
    }

    /// Create a unlimited [`MemLimit`] with no RAM limit. (Or rather, an
    /// extremely high RAM limit which should never be hit in practice.)
    pub fn unlimited() -> Self {
        Self {
            ram_limit_bytes: u32::MAX as usize * KB_SCALE_FACTOR,
            acquire_timeout: None,
            semaphore: Arc::new(Semaphore::new(u32::MAX as usize)),
        }
    }

    /// Convert bytes to permits, rounding up. Returns an error if the requested
    /// amount of RAM exceeds the total limit.
    fn bytes_to_permits(&self, ram_amount_bytes: usize) -> Result<u32> {
        if ram_amount_bytes > self.ram_limit_bytes {
            return Err(anyhow!(
                "Requested RAM amount of {} bytes exceeds total limit of {} bytes",
                ram_amount_bytes,
                self.ram_limit_bytes
            ));
        }
        let ram_amount_kb = u32::try_from(ram_amount_bytes.div_ceil(KB_SCALE_FACTOR))
            .map_err(|_| {
                anyhow!(
                    "Requested RAM amount of {} bytes is too large",
                    ram_amount_bytes
                )
            })?;
        Ok(ram_amount_kb)
    }

    /// Return the number of bytes currently available for acquisition.
    ///
    /// Because permits are tracked in KB internally, this value is always a
    /// multiple of 1024.
    #[allow(dead_code)]
    pub fn available_bytes(&self) -> usize {
        self.semaphore.available_permits() * KB_SCALE_FACTOR
    }

    /// Acquire a [`MemPermit`] for the given amount of RAM in bytes. This is an
    /// async operation which will wait until enough RAM is available to acquire
    /// the permit.
    ///
    /// IMPORTANT DEADLOCK WARNING: A given thread or task must _never_ hold
    /// more than one `MemPermit` at a time, or you may get deadlocks. To avoid
    /// this, release any previous permits held by a thread or task before
    /// acquiring a new one. (Note that our timeout can help detect such
    /// deadlocks, and limit the failure to a specific task instead of the whole
    /// process, but it's still best to avoid them in the first place.)
    pub async fn acquire(&self, ram_amount_bytes: usize) -> Result<MemPermit> {
        let ram_amount_kb = self.bytes_to_permits(ram_amount_bytes)?;
        let permit = self
            .semaphore
            .clone()
            .acquire_many_owned(ram_amount_kb)
            .with_timeout(self.acquire_timeout)
            .await
            .context("Failed to acquire memory usage permit")?;
        Ok(MemPermit { _permit: permit })
    }

    // This likely isn't needed unless we try to load images inside of handlebars templates,
    // but it's sufficiently non-obvious we want to keep the code as proof of concept.
    //
    // /// A synchronous, blocking version of [`Self::acquire`]. This is for use in
    // /// contexts like Handlebars, where we don't have async support. This will
    // /// block the current thread until the permit is acquired.
    // ///
    // /// IMPORTANT DEADLOCK WARNING: See [`Self::acquire`].
    // pub fn acquire_blocking(&self, ram_amount_bytes: usize) -> Result<MemPermit> {
    //     let handle = Handle::current();
    //     handle.block_on(self.acquire(ram_amount_bytes))
    // }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mem_limit_parses_various_formats() {
        let limit = "2GB".parse::<MemLimit>().unwrap();
        assert_eq!(limit.0.as_u64(), 2_000_000_000);

        let limit = "2GiB".parse::<MemLimit>().unwrap();
        assert_eq!(limit.0.as_u64(), 2 * 1024 * 1024 * 1024);

        let limit = "500MB".parse::<MemLimit>().unwrap();
        assert_eq!(limit.0.as_u64(), 500_000_000);

        let limit = "4096KiB".parse::<MemLimit>().unwrap();
        assert_eq!(limit.0.as_u64(), 4096 * 1024);
    }

    #[test]
    fn mem_limit_rejects_invalid_strings() {
        assert!("not_a_size".parse::<MemLimit>().is_err());
        assert!("".parse::<MemLimit>().is_err());
    }

    #[test]
    fn mem_limit_to_mem_limiter_produces_working_limiter() {
        let limit = "1MiB".parse::<MemLimit>().unwrap();
        let limiter = limit.to_mem_limiter(None);
        // Should be able to acquire up to the limit (1 MiB = 1048576 bytes).
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let permit = limiter.acquire(1024 * 1024).await.unwrap();
            drop(permit);
        });
    }

    #[tokio::test]
    async fn acquire_and_release() {
        let limiter = MemLimiter::byte_limit(2048, None);
        let permit1 = limiter.acquire(1024).await.unwrap();
        let permit2 = limiter.acquire(1024).await.unwrap();
        // All capacity used. Drop one permit to free space.
        drop(permit1);
        let _permit3 = limiter.acquire(1024).await.unwrap();
        drop(permit2);
    }

    #[tokio::test]
    async fn unlimited_allows_large_acquires() {
        let limiter = MemLimiter::unlimited();
        let _permit = limiter.acquire(1024 * 1024 * 1024).await.unwrap();
    }

    #[tokio::test]
    async fn acquire_exceeding_limit_returns_error() {
        let limiter = MemLimiter::byte_limit(1024, None);
        let result = limiter.acquire(2048).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("exceeds total limit")
        );
    }

    #[tokio::test]
    async fn acquire_rounds_up_to_kb() {
        let limiter = MemLimiter::byte_limit(2048, None);
        assert_eq!(limiter.available_bytes(), 2048);
        // Acquiring 1 byte should consume 1 KB internally.
        let permit = limiter.acquire(1).await.unwrap();
        assert_eq!(limiter.available_bytes(), 1024);
        // Releasing should restore full capacity.
        drop(permit);
        assert_eq!(limiter.available_bytes(), 2048);
    }

    #[tokio::test]
    async fn timeout_fires_when_exhausted() {
        let limiter = MemLimiter::byte_limit(1024, Some(Duration::from_millis(50)));
        // Hold a permit that uses all capacity.
        let _permit = limiter.acquire(1024).await.unwrap();
        // Trying to acquire more should time out.
        let result = limiter.acquire(1).await;
        assert!(result.is_err());
    }
}
