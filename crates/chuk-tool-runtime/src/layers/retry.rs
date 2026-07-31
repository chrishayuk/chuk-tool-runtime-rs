//! Retry-with-exponential-backoff layer.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::Value;

use crate::config::RetryConfig;
use crate::invoker::ToolInvoker;
use crate::outcome::ToolOutcome;

/// Wraps an inner invoker, retrying retryable failures with backoff.
///
/// The number of attempts made is recorded in [`ToolOutcome::attempts`]. When a
/// `timeout` is supplied it is treated as an overall deadline: backoff sleeps are
/// capped to the remaining budget, and no retry is started once it is exhausted.
pub struct RetryLayer<I> {
    inner: I,
    config: RetryConfig,
}

impl<I> RetryLayer<I> {
    /// Wrap `inner` with the given retry policy.
    pub fn new(inner: I, config: RetryConfig) -> Self {
        Self { inner, config }
    }
}

#[async_trait]
impl<I: ToolInvoker> ToolInvoker for RetryLayer<I> {
    async fn call_tool(&self, tool: &str, args: Value, timeout: Option<Duration>) -> ToolOutcome {
        if !self.config.enabled {
            return self.inner.call_tool(tool, args, timeout).await;
        }

        let deadline = timeout.map(|t| Instant::now() + t);
        let mut attempt: u32 = 0;

        loop {
            let mut outcome = self.inner.call_tool(tool, args.clone(), timeout).await;
            outcome.attempts = attempt + 1;

            if outcome.success {
                return outcome;
            }

            let has_budget = attempt < self.config.max_retries;
            if !has_budget || !self.config.should_retry(outcome.error.as_deref()) {
                return outcome;
            }

            let mut delay = self.config.backoff(attempt);
            if let Some(dl) = deadline {
                let remaining = dl.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return outcome; // out of time; return the last failure
                }
                delay = delay.min(remaining);
            }
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            attempt += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// Invoker that returns scripted outcomes and counts calls.
    struct Mock {
        responses: Mutex<VecDeque<ToolOutcome>>,
        calls: AtomicUsize,
    }

    impl Mock {
        fn new(responses: Vec<ToolOutcome>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
                calls: AtomicUsize::new(0),
            }
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl ToolInvoker for Mock {
        async fn call_tool(
            &self,
            tool: &str,
            _args: Value,
            _timeout: Option<Duration>,
        ) -> ToolOutcome {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| ToolOutcome::err(tool, "mock exhausted"))
        }
    }

    /// Zero-delay, deterministic config for fast tests.
    fn cfg(max_retries: u32) -> RetryConfig {
        RetryConfig {
            max_retries,
            base_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
            jitter: false,
            ..RetryConfig::default()
        }
    }

    #[tokio::test]
    async fn retries_then_succeeds() {
        let mock = Mock::new(vec![
            ToolOutcome::err("echo", "connection reset"),
            ToolOutcome::err("echo", "timeout"),
            ToolOutcome::ok("echo", serde_json::json!({"ok": true})),
        ]);
        let layer = RetryLayer::new(mock, cfg(3));
        let out = layer.call_tool("echo", Value::Null, None).await;
        assert!(out.success);
        assert_eq!(out.attempts, 3);
        assert_eq!(layer.inner.calls(), 3);
    }

    #[tokio::test]
    async fn does_not_retry_non_retryable() {
        let mock = Mock::new(vec![ToolOutcome::err("echo", "401 Unauthorized")]);
        let layer = RetryLayer::new(mock, cfg(3));
        let out = layer.call_tool("echo", Value::Null, None).await;
        assert!(out.is_error());
        assert_eq!(out.attempts, 1);
        assert_eq!(layer.inner.calls(), 1); // no retries on a skip-listed error
    }

    #[tokio::test]
    async fn exhausts_budget_on_persistent_failure() {
        let mock = Mock::new(vec![
            ToolOutcome::err("echo", "connection refused"),
            ToolOutcome::err("echo", "connection refused"),
            ToolOutcome::err("echo", "connection refused"),
        ]);
        let layer = RetryLayer::new(mock, cfg(2)); // 1 initial + 2 retries = 3 tries
        let out = layer.call_tool("echo", Value::Null, None).await;
        assert!(out.is_error());
        assert_eq!(out.attempts, 3);
        assert_eq!(layer.inner.calls(), 3);
    }

    #[tokio::test]
    async fn disabled_passes_through() {
        let mock = Mock::new(vec![ToolOutcome::err("echo", "timeout")]);
        let mut c = cfg(3);
        c.enabled = false;
        let layer = RetryLayer::new(mock, c);
        let out = layer.call_tool("echo", Value::Null, None).await;
        assert!(out.is_error());
        assert_eq!(layer.inner.calls(), 1);
    }

    #[test]
    fn should_retry_skip_precedence() {
        let c = RetryConfig::default();
        assert!(c.should_retry(Some("connection reset by peer")));
        assert!(!c.should_retry(Some("OAuth token expired"))); // skip wins (case-insensitive)
        assert!(!c.should_retry(Some("some unrelated error")));
        assert!(!c.should_retry(None));
    }

    #[test]
    fn base_backoff_is_exponential_and_capped() {
        let c = RetryConfig {
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(10),
            jitter: false,
            ..RetryConfig::default()
        };
        assert_eq!(c.base_backoff(0), Duration::from_secs(1));
        assert_eq!(c.base_backoff(1), Duration::from_secs(2));
        assert_eq!(c.base_backoff(2), Duration::from_secs(4));
        assert_eq!(c.base_backoff(10), Duration::from_secs(10)); // capped
    }
}
