//! Support for specifying rate limits for calling various APIs.

use std::{fmt, str::FromStr, time::Duration};

use leaky_bucket::RateLimiter;

use crate::prelude::*;

/// The period over which the rate limit is applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RateLimitPeriod {
    /// Per second.
    Second,
    /// Per minute.
    Minute,
}

impl RateLimitPeriod {
    /// Convert this period to a number of seconds.
    pub fn to_duration(self) -> Duration {
        match self {
            RateLimitPeriod::Second => Duration::from_secs(1),
            RateLimitPeriod::Minute => Duration::from_secs(60),
        }
    }
}

impl fmt::Display for RateLimitPeriod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RateLimitPeriod::Second => write!(f, "s"),
            RateLimitPeriod::Minute => write!(f, "m"),
        }
    }
}

impl FromStr for RateLimitPeriod {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "s" => Ok(RateLimitPeriod::Second),
            "m" => Ok(RateLimitPeriod::Minute),
            _ => Err(anyhow!("Unsupported rate limit period: {:?}", s)),
        }
    }
}

/// A rate limit for an API.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RateLimit {
    /// The maximum number of requests allowed in the period.
    pub max_requests: usize,
    /// The period over which the rate limit is applied.
    pub per_period: RateLimitPeriod,
}

impl RateLimit {
    /// Create a new [`RateLimit`].
    pub fn new(max_requests: usize, per_period: RateLimitPeriod) -> Self {
        Self {
            max_requests,
            per_period,
        }
    }

    /// Create a [`RateLimiter`] for this rate limit.
    pub fn to_rate_limiter(&self) -> RateLimiter {
        // We only refill once every `interval` seconds, so for
        // `RateLimitPeriod::Minute`, we may want to do some math to calculate
        // per-second limits instead. For now, just start with a full bucket and
        // hope the user doesn't call `prompt-scaler` twice in rapid succession.
        RateLimiter::builder()
            .initial(self.max_requests)
            .refill(self.max_requests)
            .max(self.max_requests)
            .interval(self.per_period.to_duration())
            .build()
    }
}

impl fmt::Display for RateLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.max_requests, self.per_period)
    }
}

impl FromStr for RateLimit {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let parse = |s: &str| -> Result<_> {
            let mut parts = s.splitn(2, '/');
            let max_requests = parts
                .next()
                .ok_or_else(|| anyhow!("Missing max requests"))?
                .parse::<usize>()?;
            let per_period = parts
                .next()
                .ok_or_else(|| anyhow!("Missing period"))?
                .parse::<RateLimitPeriod>()?;
            Ok(Self {
                max_requests,
                per_period,
            })
        };
        parse(s).with_context(|| format!("Failed to parse rate limit: {:?}", s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse() {
        let rate_limit = RateLimit::from_str("10/s").unwrap();
        assert_eq!(rate_limit.max_requests, 10);
        assert_eq!(rate_limit.per_period, RateLimitPeriod::Second);

        let rate_limit = RateLimit::from_str("5/m").unwrap();
        assert_eq!(rate_limit.max_requests, 5);
        assert_eq!(rate_limit.per_period, RateLimitPeriod::Minute);
    }

    #[test]
    fn test_failed_parse() {
        assert!(RateLimit::from_str("10/invalid").is_err());
        assert!(RateLimit::from_str("invalid").is_err());
    }

    #[test]
    fn test_display() {
        let rate_limit = RateLimit::from_str("10/s").unwrap();
        assert_eq!(rate_limit.to_string(), "10/s");

        let rate_limit = RateLimit::from_str("5/m").unwrap();
        assert_eq!(rate_limit.to_string(), "5/m");
    }
}
