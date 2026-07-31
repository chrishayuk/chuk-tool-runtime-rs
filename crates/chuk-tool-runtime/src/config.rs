//! Policy configuration.
//!
//! Defaults mirror the Python `chuk-tool-processor` MCP middleware stack so the
//! two runtimes behave the same. Where the Python implementation was
//! inconsistent (retry substring matching was case-sensitive while the skip list
//! was case-insensitive) this core normalises both to case-insensitive, which is
//! the intended behaviour.

use std::collections::{HashMap, HashSet};
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

    /// Set the maximum number of retries. (Builder.)
    pub fn with_max_retries(mut self, n: u32) -> Self {
        self.max_retries = n;
        self
    }
    /// Set the base backoff delay. (Builder.)
    pub fn with_base_delay(mut self, delay: Duration) -> Self {
        self.base_delay = delay;
        self
    }
    /// Set the backoff ceiling. (Builder.)
    pub fn with_max_delay(mut self, delay: Duration) -> Self {
        self.max_delay = delay;
        self
    }
    /// Enable or disable jitter. (Builder.)
    pub fn with_jitter(mut self, on: bool) -> Self {
        self.jitter = on;
        self
    }
    /// Replace the retry-on substring list. (Builder.)
    pub fn with_retry_on(mut self, patterns: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.retry_on = patterns.into_iter().map(Into::into).collect();
        self
    }
    /// Replace the skip (never-retry) substring list. (Builder.)
    pub fn with_skip_on(mut self, patterns: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.skip_on = patterns.into_iter().map(Into::into).collect();
        self
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

impl CircuitBreakerConfig {
    /// Set the failures-before-open threshold. (Builder.)
    pub fn with_failure_threshold(mut self, n: u32) -> Self {
        self.failure_threshold = n;
        self
    }
    /// Set the probe successes needed to close. (Builder.)
    pub fn with_success_threshold(mut self, n: u32) -> Self {
        self.success_threshold = n;
        self
    }
    /// Set how long to stay open before a probe. (Builder.)
    pub fn with_reset_timeout(mut self, timeout: Duration) -> Self {
        self.reset_timeout = timeout;
        self
    }
    /// Set the max concurrent half-open probes. (Builder.)
    pub fn with_half_open_max_calls(mut self, n: u32) -> Self {
        self.half_open_max_calls = n;
        self
    }
}

/// Sliding-window rate-limiting policy.
///
/// A **global** window (`global_limit` requests per `global_period` across all
/// tools) and independent **per-tool** windows. The layer *waits* until a slot
/// frees rather than rejecting. Rate limiting is opt-in: you enable it by adding
/// the layer via the builder.
#[derive(Debug, Clone)]
pub struct RateLimitConfig {
    /// Whether the layer is active.
    pub enabled: bool,
    /// Max requests per `global_period` across all tools (`None` = no global cap).
    pub global_limit: Option<u32>,
    /// The global window length.
    pub global_period: Duration,
    /// Independent `tool -> (limit, period)` windows.
    pub per_tool: HashMap<String, (u32, Duration)>,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            global_limit: Some(100),
            global_period: Duration::from_secs(60),
            per_tool: HashMap::new(),
        }
    }
}

impl RateLimitConfig {
    /// Set the global window (`limit` requests per `period`). (Builder.)
    pub fn with_global(mut self, limit: u32, period: Duration) -> Self {
        self.global_limit = Some(limit);
        self.global_period = period;
        self
    }
    /// Remove the global cap (per-tool windows still apply). (Builder.)
    pub fn without_global_limit(mut self) -> Self {
        self.global_limit = None;
        self
    }
    /// Add or replace a per-tool window. (Builder; chainable per tool.)
    pub fn with_tool_limit(mut self, tool: impl Into<String>, limit: u32, period: Duration) -> Self {
        self.per_tool.insert(tool.into(), (limit, period));
        self
    }
}

/// Result-caching policy.
///
/// Caching is **opt-in per tool**: only tools in `cacheable_tools` are cached,
/// and only successful results (errors are never cached). Entries are keyed on
/// the tool name plus a canonical rendering of the arguments.
#[derive(Debug, Clone, Default)]
pub struct CacheConfig {
    /// Tools whose successful results should be cached (empty = cache nothing).
    pub cacheable_tools: HashSet<String>,
    /// Default entry lifetime (`None` = never expires).
    pub default_ttl: Option<Duration>,
    /// Per-tool lifetime overrides.
    pub per_tool_ttl: HashMap<String, Duration>,
}

impl CacheConfig {
    /// The TTL to apply to `tool` (per-tool override, else the default).
    pub fn ttl_for(&self, tool: &str) -> Option<Duration> {
        self.per_tool_ttl.get(tool).copied().or(self.default_ttl)
    }

    /// Whether results for `tool` should be cached.
    pub fn is_cacheable(&self, tool: &str) -> bool {
        self.cacheable_tools.contains(tool)
    }

    /// Mark a tool cacheable. (Builder; chainable per tool.)
    pub fn cacheable_tool(mut self, tool: impl Into<String>) -> Self {
        self.cacheable_tools.insert(tool.into());
        self
    }
    /// Set the default entry lifetime. (Builder.)
    pub fn with_default_ttl(mut self, ttl: Duration) -> Self {
        self.default_ttl = Some(ttl);
        self
    }
    /// Set a per-tool lifetime override. (Builder; chainable per tool.)
    pub fn with_tool_ttl(mut self, tool: impl Into<String>, ttl: Duration) -> Self {
        self.per_tool_ttl.insert(tool.into(), ttl);
        self
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

    #[test]
    fn retry_builders_chain() {
        let cfg = RetryConfig::default()
            .with_max_retries(7)
            .with_base_delay(Duration::from_millis(200))
            .with_max_delay(Duration::from_secs(5))
            .with_jitter(false)
            .with_retry_on(["boom"])
            .with_skip_on(["nope"]);
        assert_eq!(cfg.max_retries, 7);
        assert_eq!(cfg.base_delay, Duration::from_millis(200));
        assert!(!cfg.jitter);
        assert!(cfg.should_retry(Some("boom happened")));
        assert!(!cfg.should_retry(Some("nope not this")));
    }

    #[test]
    fn circuit_breaker_builders_chain() {
        let cfg = CircuitBreakerConfig::default()
            .with_failure_threshold(2)
            .with_success_threshold(1)
            .with_reset_timeout(Duration::from_secs(30))
            .with_half_open_max_calls(3);
        assert_eq!(cfg.failure_threshold, 2);
        assert_eq!(cfg.success_threshold, 1);
        assert_eq!(cfg.reset_timeout, Duration::from_secs(30));
        assert_eq!(cfg.half_open_max_calls, 3);
    }

    #[test]
    fn rate_limit_builders_chain() {
        let cfg = RateLimitConfig::default()
            .with_global(10, Duration::from_secs(1))
            .with_tool_limit("slow", 1, Duration::from_secs(60));
        assert_eq!(cfg.global_limit, Some(10));
        assert_eq!(cfg.per_tool.get("slow"), Some(&(1, Duration::from_secs(60))));

        let none = RateLimitConfig::default().without_global_limit();
        assert_eq!(none.global_limit, None);
    }

    #[test]
    fn cache_builders_chain() {
        let cfg = CacheConfig::default()
            .cacheable_tool("a")
            .cacheable_tool("b")
            .with_default_ttl(Duration::from_secs(300))
            .with_tool_ttl("a", Duration::from_secs(10));
        assert!(cfg.is_cacheable("a") && cfg.is_cacheable("b"));
        assert_eq!(cfg.ttl_for("a"), Some(Duration::from_secs(10))); // per-tool override
        assert_eq!(cfg.ttl_for("b"), Some(Duration::from_secs(300))); // default
    }
}
