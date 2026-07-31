//! Per-tool circuit-breaker layer.
//!
//! Sits *above* retry (a fully-retried failure counts as one failure) and short-
//! circuits calls to a tool that keeps failing, giving it time to recover.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};

use crate::config::CircuitBreakerConfig;
use crate::invoker::ToolInvoker;
use crate::outcome::ToolOutcome;

/// Error-message prefix used when a call is short-circuited by an open breaker.
pub const CIRCUIT_OPEN_PREFIX: &str = "circuit breaker open";

/// Circuit-breaker state for a single tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    /// Calls pass through.
    Closed,
    /// Calls are short-circuited.
    Open,
    /// A limited number of probe calls are allowed.
    HalfOpen,
}

/// A point-in-time view of one tool's breaker, for diagnostics.
#[derive(Debug, Clone)]
pub struct CircuitSnapshot {
    pub state: CircuitState,
    pub failure_count: u32,
    pub success_count: u32,
    /// Time until an `OPEN` breaker will admit a probe (`None` unless `OPEN`).
    pub time_until_half_open: Option<Duration>,
}

#[derive(Debug)]
struct Breaker {
    state: CircuitState,
    failure_count: u32,
    success_count: u32,
    half_open_calls: u32,
    opened_at: Option<Instant>,
}

impl Breaker {
    fn new() -> Self {
        Self {
            state: CircuitState::Closed,
            failure_count: 0,
            success_count: 0,
            half_open_calls: 0,
            opened_at: None,
        }
    }

    fn open(&mut self) {
        self.state = CircuitState::Open;
        self.opened_at = Some(Instant::now());
    }

    /// Try to admit a call, transitioning `OPEN → HALF_OPEN` once the reset
    /// window has elapsed. Returns whether the call is allowed through.
    fn try_acquire(&mut self, cfg: &CircuitBreakerConfig) -> bool {
        match self.state {
            CircuitState::Closed => true,
            CircuitState::HalfOpen => {
                if self.half_open_calls < cfg.half_open_max_calls {
                    self.half_open_calls += 1;
                    true
                } else {
                    false
                }
            }
            CircuitState::Open => {
                let ready = self
                    .opened_at
                    .map(|t| t.elapsed() >= cfg.reset_timeout)
                    .unwrap_or(false);
                if ready {
                    self.state = CircuitState::HalfOpen;
                    self.half_open_calls = 1; // this call takes the first probe slot
                    self.success_count = 0;
                    true
                } else {
                    false
                }
            }
        }
    }

    fn record_success(&mut self, cfg: &CircuitBreakerConfig) {
        if self.state == CircuitState::HalfOpen {
            self.success_count += 1;
            if self.success_count >= cfg.success_threshold {
                self.state = CircuitState::Closed;
                self.failure_count = 0;
                self.success_count = 0;
                self.half_open_calls = 0;
                self.opened_at = None;
            }
        } else {
            // CLOSED: a success clears accrued failures.
            self.failure_count = 0;
        }
    }

    fn record_failure(&mut self, cfg: &CircuitBreakerConfig) {
        self.failure_count += 1;
        match self.state {
            CircuitState::Closed => {
                if self.failure_count >= cfg.failure_threshold {
                    self.open();
                }
            }
            CircuitState::HalfOpen => {
                // Probe failed → straight back to OPEN, restarting the window.
                self.success_count = 0;
                self.half_open_calls = 0;
                self.open();
            }
            CircuitState::Open => {}
        }
    }

    /// Release a `HALF_OPEN` probe slot after a call completes (no-op once the
    /// breaker has since transitioned out of `HALF_OPEN`).
    fn release_probe(&mut self) {
        if self.state == CircuitState::HalfOpen {
            self.half_open_calls = self.half_open_calls.saturating_sub(1);
        }
    }

    fn snapshot(&self, cfg: &CircuitBreakerConfig) -> CircuitSnapshot {
        let time_until_half_open = if self.state == CircuitState::Open {
            self.opened_at
                .map(|t| cfg.reset_timeout.saturating_sub(t.elapsed()))
        } else {
            None
        };
        CircuitSnapshot {
            state: self.state,
            failure_count: self.failure_count,
            success_count: self.success_count,
            time_until_half_open,
        }
    }
}

/// Wraps an inner invoker with a per-tool circuit breaker.
pub struct CircuitBreakerLayer<I> {
    inner: I,
    config: CircuitBreakerConfig,
    breakers: Mutex<HashMap<String, Arc<Mutex<Breaker>>>>,
}

impl<I> CircuitBreakerLayer<I> {
    /// Wrap `inner` with the given circuit-breaker policy.
    pub fn new(inner: I, config: CircuitBreakerConfig) -> Self {
        Self {
            inner,
            config,
            breakers: Mutex::new(HashMap::new()),
        }
    }

    async fn breaker_for(&self, tool: &str) -> Arc<Mutex<Breaker>> {
        let mut map = self.breakers.lock().await;
        map.entry(tool.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(Breaker::new())))
            .clone()
    }

    /// Diagnostics snapshot of the breaker for `tool`, if one exists yet.
    pub async fn snapshot(&self, tool: &str) -> Option<CircuitSnapshot> {
        let cell = {
            let map = self.breakers.lock().await;
            map.get(tool).cloned()
        }?;
        let breaker = cell.lock().await;
        Some(breaker.snapshot(&self.config))
    }
}

#[async_trait]
impl<I: ToolInvoker> ToolInvoker for CircuitBreakerLayer<I> {
    async fn call_tool(&self, tool: &str, args: Value, timeout: Option<Duration>) -> ToolOutcome {
        if !self.config.enabled {
            return self.inner.call_tool(tool, args, timeout).await;
        }

        let cell = self.breaker_for(tool).await;

        let allowed = {
            let mut breaker = cell.lock().await;
            breaker.try_acquire(&self.config)
        };
        if !allowed {
            return ToolOutcome::err(tool, format!("{CIRCUIT_OPEN_PREFIX} for '{tool}'"));
        }

        let outcome = self.inner.call_tool(tool, args, timeout).await;

        {
            let mut breaker = cell.lock().await;
            if outcome.success {
                breaker.record_success(&self.config);
            } else {
                breaker.record_failure(&self.config);
            }
            breaker.release_probe();
        }

        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashSet, VecDeque};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex as StdMutex;

    struct Mock {
        calls: AtomicUsize,
        script: StdMutex<VecDeque<bool>>, // success flags consumed first
        default_ok: bool,
        fail_tools: HashSet<String>, // if non-empty, decide purely by tool name
    }

    impl Mock {
        fn always_fail() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                script: StdMutex::new(VecDeque::new()),
                default_ok: false,
                fail_tools: HashSet::new(),
            }
        }
        fn scripted(flags: Vec<bool>, default_ok: bool) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                script: StdMutex::new(flags.into()),
                default_ok,
                fail_tools: HashSet::new(),
            }
        }
        fn fail_only(tools: &[&str]) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                script: StdMutex::new(VecDeque::new()),
                default_ok: true,
                fail_tools: tools.iter().map(|s| s.to_string()).collect(),
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
            let ok = if !self.fail_tools.is_empty() {
                !self.fail_tools.contains(tool)
            } else {
                self.script.lock().unwrap().pop_front().unwrap_or(self.default_ok)
            };
            if ok {
                ToolOutcome::ok(tool, Value::Null)
            } else {
                ToolOutcome::err(tool, "boom")
            }
        }
    }

    fn cfg() -> CircuitBreakerConfig {
        CircuitBreakerConfig {
            failure_threshold: 3,
            success_threshold: 2,
            reset_timeout: Duration::from_secs(60),
            half_open_max_calls: 1,
            enabled: true,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn opens_after_threshold_and_blocks() {
        let layer = CircuitBreakerLayer::new(Mock::always_fail(), cfg());
        for _ in 0..3 {
            let o = layer.call_tool("x", Value::Null, None).await;
            assert!(o.is_error());
        }
        assert_eq!(layer.snapshot("x").await.unwrap().state, CircuitState::Open);

        // Now blocked: the call is short-circuited and never reaches the inner.
        let before = layer.inner.calls();
        let o = layer.call_tool("x", Value::Null, None).await;
        assert!(o.is_error());
        assert!(o.error.unwrap().contains(CIRCUIT_OPEN_PREFIX));
        assert_eq!(layer.inner.calls(), before, "blocked call must not hit inner");
    }

    #[tokio::test(start_paused = true)]
    async fn recovers_through_half_open_to_closed() {
        // Fail twice-below-threshold? No: open needs 3. Fail 3, then succeed on probe.
        let layer = CircuitBreakerLayer::new(Mock::scripted(vec![false, false, false], true), cfg());
        for _ in 0..3 {
            layer.call_tool("x", Value::Null, None).await;
        }
        assert_eq!(layer.snapshot("x").await.unwrap().state, CircuitState::Open);

        tokio::time::advance(Duration::from_secs(60)).await;

        // success_threshold = 2 probes to close.
        let p1 = layer.call_tool("x", Value::Null, None).await;
        assert!(p1.success);
        assert_eq!(layer.snapshot("x").await.unwrap().state, CircuitState::HalfOpen);
        let p2 = layer.call_tool("x", Value::Null, None).await;
        assert!(p2.success);
        assert_eq!(layer.snapshot("x").await.unwrap().state, CircuitState::Closed);
    }

    #[tokio::test(start_paused = true)]
    async fn half_open_failure_reopens() {
        let layer = CircuitBreakerLayer::new(Mock::always_fail(), cfg());
        for _ in 0..3 {
            layer.call_tool("x", Value::Null, None).await;
        }
        tokio::time::advance(Duration::from_secs(60)).await;

        // Probe runs (allowed) and fails → back to OPEN.
        let probe = layer.call_tool("x", Value::Null, None).await;
        assert!(probe.is_error());
        assert_eq!(layer.snapshot("x").await.unwrap().state, CircuitState::Open);

        // Immediately blocked again.
        let before = layer.inner.calls();
        layer.call_tool("x", Value::Null, None).await;
        assert_eq!(layer.inner.calls(), before);
    }

    #[tokio::test(start_paused = true)]
    async fn breakers_are_per_tool() {
        let layer = CircuitBreakerLayer::new(Mock::fail_only(&["a"]), cfg());
        for _ in 0..3 {
            layer.call_tool("a", Value::Null, None).await;
        }
        assert_eq!(layer.snapshot("a").await.unwrap().state, CircuitState::Open);

        // "b" is unaffected and keeps succeeding.
        let o = layer.call_tool("b", Value::Null, None).await;
        assert!(o.success);
        assert_eq!(layer.snapshot("b").await.unwrap().state, CircuitState::Closed);
    }

    #[tokio::test(start_paused = true)]
    async fn disabled_passes_through() {
        let mut c = cfg();
        c.enabled = false;
        let layer = CircuitBreakerLayer::new(Mock::always_fail(), c);
        for _ in 0..5 {
            layer.call_tool("x", Value::Null, None).await;
        }
        // Never trips; every call reached the inner.
        assert_eq!(layer.inner.calls(), 5);
        assert!(layer.snapshot("x").await.is_none());
    }
}
