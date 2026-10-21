//! Various sorts of processor state that are needed by multiple types of queue
//! workers.

use leaky_bucket::RateLimiter;

use crate::{costs::ModelCost, mem_limit::MemLimiter};

/// Shared processor state.
#[derive(Debug)]
pub struct CommonProcessorState {
    /// A rate limiter to control API request rate.
    pub rate_limiter: Option<RateLimiter>,

    /// A memory limiter for image loading.
    pub mem_limiter: MemLimiter,

    /// Model cost information, if available.
    pub model_cost: Option<ModelCost>,
}

impl CommonProcessorState {
    /// Wait on our rate limiter.
    pub async fn wait_on_rate_limiter(&self) {
        if let Some(rate_limiter) = self.rate_limiter.as_ref() {
            rate_limiter.acquire(1).await;
        }
    }
}
