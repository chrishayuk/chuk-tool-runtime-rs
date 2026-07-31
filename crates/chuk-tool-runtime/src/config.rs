//! Policy configuration.
//!
//! Defaults mirror the Python `chuk-tool-processor` MCP middleware stack so the
//! two runtimes behave the same. Where the Python implementation was
//! inconsistent (retry substring matching was case-sensitive while the skip list
//! was case-insensitive) this core normalises both to case-insensitive, which is
//! the intended behaviour.

use std::time::Duration;

/// Error-message substrings that, by default, *should* trigger a retry.
pub const DEFAULT_RETRY_ON: &[&str] = &[
    "transport not initialized",
    "connection",
    "timeout",
    "refused",
    "reset",
    "closed",
];

/// Error-message substrings that must *never* be retried (checked first).
pub const DEFAULT_SKIP_ON: &[&str] = &[
    "oauth",
    "unauthorized",
    "authentication",
    "invalid_grant",
    "no server found",
];

/// Retry-with-backoff policy.
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Whether the layer is active.
    pub enabled: bool,
    /// Maximum number of *retries* after the initial attempt.
    pub max_retries: u32,
    /// Base backoff delay.
    pub base_delay: Duration,
    /// Backoff ceiling.
    pub max_delay: Duration,
    /// Multiply each delay by a random factor in `[0.5, 1.5)`.
    pub jitter: bool,
    /// Substrings that make an error retryable (empty = retry all errors).
    pub retry_on: Vec<String>,
    /// Substrings that make an error non-retryable (takes precedence).
    pub skip_on: Vec<String>,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_retries: 3,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(30),
            jitter: true,
            retry_on: DEFAULT_RETRY_ON.iter().map(|s| s.to_string()).collect(),
            skip_on: DEFAULT_SKIP_ON.iter().map(|s| s.to_string()).collect(),
        }
    }
}

impl RetryConfig {
    /// Whether an error with message `error` should be retried.
    ///
    /// `None` means the call succeeded, so there is nothing to retry.
    pub fn should_retry(&self, error: Option<&str>) -> bool {
        let Some(err) = error else {
            return false;
        };
        let lowered = err.to_lowercase();

        // Skip list wins.
        if self
            .skip_on
            .iter()
            .any(|p| lowered.contains(&p.to_lowercase()))
        {
            return false;
        }
        // No allow-list configured => every (non-skipped) error is retryable.
        if self.retry_on.is_empty() {
            return true;
        }
        self.retry_on
            .iter()
            .any(|p| lowered.contains(&p.to_lowercase()))
    }

    /// Exponential backoff for a 0-based `attempt`, *before* jitter:
    /// `min(base_delay * 2^attempt, max_delay)`.
    pub fn base_backoff(&self, attempt: u32) -> Duration {
        let factor = 2f64.powi(attempt as i32);
        let secs = (self.base_delay.as_secs_f64() * factor).min(self.max_delay.as_secs_f64());
        Duration::from_secs_f64(secs.max(0.0))
    }

    /// Backoff for `attempt`, applying jitter in `[0.5, 1.5)` when enabled.
    pub fn backoff(&self, attempt: u32) -> Duration {
        let base = self.base_backoff(attempt).as_secs_f64();
        if self.jitter {
            let jitter = 0.5 + rand::random::<f64>();
            Duration::from_secs_f64(base * jitter)
        } else {
            Duration::from_secs_f64(base)
        }
    }
}

/// Per-tool circuit-breaker policy.
///
/// State machine (per tool): `CLOSED → OPEN` once `failure_threshold` failures
/// accrue; `OPEN → HALF_OPEN` after `reset_timeout`; `HALF_OPEN → CLOSED` after
/// `success_threshold` probe successes, or back to `OPEN` on any probe failure.
/// At most `half_open_max_calls` probes run concurrently in `HALF_OPEN`.
#[derive(Debug, Clone)]
pub struct CircuitBreakerConfig {
    /// Whether the layer is active.
    pub enabled: bool,
    /// Consecutive-ish failures in `CLOSED` before opening.
    pub failure_threshold: u32,
    /// Probe successes in `HALF_OPEN` before closing.
    pub success_threshold: u32,
    /// How long to stay `OPEN` before allowing a `HALF_OPEN` probe.
    pub reset_timeout: Duration,
    /// Max concurrent probes allowed in `HALF_OPEN`.
    pub half_open_max_calls: u32,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            failure_threshold: 5,
            success_threshold: 2,
            reset_timeout: Duration::from_secs(60),
            half_open_max_calls: 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_with_jitter_stays_in_band() {
        let cfg = RetryConfig {
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(60),
            jitter: true,
            ..RetryConfig::default()
        };
        // attempt 0 => base 1s, jittered into [0.5s, 1.5s).
        for _ in 0..100 {
            let d = cfg.backoff(0).as_secs_f64();
            assert!((0.5..1.5).contains(&d), "jittered delay out of band: {d}");
        }
    }

    #[test]
    fn empty_retry_on_retries_all_non_skipped_errors() {
        let cfg = RetryConfig {
            retry_on: vec![],
            ..RetryConfig::default()
        };
        assert!(cfg.should_retry(Some("some totally unknown error")));
        // Skip list still wins even with an empty allow-list.
        assert!(!cfg.should_retry(Some("unauthorized")));
    }
}
