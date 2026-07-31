//! Sliding-window rate-limiting layer (outermost policy).
//!
//! Enforces a global window and independent per-tool windows. When a window is
//! full the call *waits* until the oldest request in the window ages out, then
//! proceeds — it does not reject. Global limit is checked before the per-tool
//! one, matching the Python `RateLimiter`.

use std::collections::{HashMap, VecDeque};

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};

use crate::config::RateLimitConfig;
use crate::invoker::ToolInvoker;
use crate::outcome::ToolOutcome;

/// A single sliding window: at most `limit` acquisitions per `period`.
struct Window {
    limit: u32,
    period: Duration,
    stamps: Mutex<VecDeque<Instant>>,
}

impl Window {
    fn new(limit: u32, period: Duration) -> Self {
        Self {
            limit,
            period,
            stamps: Mutex::new(VecDeque::new()),
        }
    }

    /// Block until a slot is free, then record this acquisition.
    async fn acquire(&self) {
        loop {
            let wait = {
                let mut stamps = self.stamps.lock().await;
                let now = Instant::now();

                // Drop timestamps that have aged out of the window.
                while let Some(&front) = stamps.front() {
                    if now.saturating_duration_since(front) >= self.period {
                        stamps.pop_front();
                    } else {
                        break;
                    }
                }

                if (stamps.len() as u32) < self.limit {
                    stamps.push_back(now);
                    return;
                }

                // Full: wait until the oldest request leaves the window.
                let oldest = *stamps.front().expect("window is full so it is non-empty");
                (oldest + self.period).saturating_duration_since(now)
            };

            if wait.is_zero() {
                continue; // oldest just expired; re-prune on the next pass
            }
            tokio::time::sleep(wait).await;
        }
    }
}

/// Wraps an inner invoker, throttling calls to a global and per-tool rate.
pub struct RateLimitLayer<I> {
    inner: I,
    enabled: bool,
    global: Option<Window>,
    per_tool: HashMap<String, Window>,
}

impl<I> RateLimitLayer<I> {
    /// Wrap `inner` with the given rate-limit policy.
    pub fn new(inner: I, config: RateLimitConfig) -> Self {
        let global = config
            .global_limit
            .map(|limit| Window::new(limit, config.global_period));
        let per_tool = config
            .per_tool
            .into_iter()
            .map(|(tool, (limit, period))| (tool, Window::new(limit, period)))
            .collect();
        Self {
            inner,
            enabled: config.enabled,
            global,
            per_tool,
        }
    }
}

#[async_trait]
impl<I: ToolInvoker> ToolInvoker for RateLimitLayer<I> {
    async fn call_tool(&self, tool: &str, args: Value, timeout: Option<Duration>) -> ToolOutcome {
        if !self.enabled {
            return self.inner.call_tool(tool, args, timeout).await;
        }
        if let Some(global) = &self.global {
            global.acquire().await;
        }
        if let Some(window) = self.per_tool.get(tool) {
            window.acquire().await;
        }
        self.inner.call_tool(tool, args, timeout).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Always-ok invoker that counts calls.
    struct Mock {
        calls: AtomicUsize,
    }
    impl Mock {
        fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
            }
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }
    #[async_trait]
    impl ToolInvoker for Mock {
        async fn call_tool(&self, tool: &str, _a: Value, _t: Option<Duration>) -> ToolOutcome {
            self.calls.fetch_add(1, Ordering::SeqCst);
            ToolOutcome::ok(tool, Value::Null)
        }
    }

    fn global_cfg(limit: u32, period_secs: u64) -> RateLimitConfig {
        RateLimitConfig {
            enabled: true,
            global_limit: Some(limit),
            global_period: Duration::from_secs(period_secs),
            per_tool: HashMap::new(),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn under_limit_passes_through() {
        let layer = RateLimitLayer::new(Mock::new(), global_cfg(5, 60));
        for _ in 0..3 {
            assert!(layer.call_tool("x", Value::Null, None).await.success);
        }
        assert_eq!(layer.inner.calls(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn over_limit_blocks_until_window_slides() {
        let layer = Arc::new(RateLimitLayer::new(Mock::new(), global_cfg(1, 60)));

        // First call fills the window.
        assert!(layer.call_tool("x", Value::Null, None).await.success);

        // Second call must wait for the window to slide.
        let l = layer.clone();
        let handle = tokio::spawn(async move { l.call_tool("x", Value::Null, None).await });
        tokio::task::yield_now().await;
        assert!(!handle.is_finished(), "second call should be blocked");
        assert_eq!(layer.inner.calls(), 1);

        tokio::time::advance(Duration::from_secs(60)).await;
        assert!(handle.await.unwrap().success);
        assert_eq!(layer.inner.calls(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn per_tool_windows_are_independent() {
        let mut per_tool = HashMap::new();
        per_tool.insert("a".to_string(), (1u32, Duration::from_secs(60)));
        let cfg = RateLimitConfig {
            enabled: true,
            global_limit: None, // isolate per-tool behaviour
            global_period: Duration::from_secs(60),
            per_tool,
        };
        let layer = Arc::new(RateLimitLayer::new(Mock::new(), cfg));

        // "a" is capped at 1/60s.
        assert!(layer.call_tool("a", Value::Null, None).await.success);
        let l = layer.clone();
        let a2 = tokio::spawn(async move { l.call_tool("a", Value::Null, None).await });
        tokio::task::yield_now().await;
        assert!(!a2.is_finished(), "second 'a' should be blocked");

        // "b" has no limit and proceeds immediately.
        assert!(layer.call_tool("b", Value::Null, None).await.success);

        tokio::time::advance(Duration::from_secs(60)).await;
        assert!(a2.await.unwrap().success);
    }

    #[tokio::test(start_paused = true)]
    async fn disabled_passes_through() {
        let mut cfg = global_cfg(1, 60);
        cfg.enabled = false;
        let layer = RateLimitLayer::new(Mock::new(), cfg);
        for _ in 0..5 {
            assert!(layer.call_tool("x", Value::Null, None).await.success);
        }
        assert_eq!(layer.inner.calls(), 5); // never throttled
    }
}
