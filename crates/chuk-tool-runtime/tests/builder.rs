//! Integration tests for the public `RuntimeBuilder` API and layer composition.

use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use chuk_tool_runtime::{
    CircuitBreakerConfig, RateLimitConfig, RetryConfig, RuntimeBuilder, ToolInvoker, ToolOutcome,
    CIRCUIT_OPEN_PREFIX,
};
use serde_json::Value;

/// A transport that fails a fixed number of times, then succeeds.
struct Transport {
    remaining_failures: Mutex<u32>,
}

impl Transport {
    fn new(failures: u32) -> Self {
        Self {
            remaining_failures: Mutex::new(failures),
        }
    }
}

#[async_trait]
impl ToolInvoker for Transport {
    async fn call_tool(&self, tool: &str, _a: Value, _t: Option<Duration>) -> ToolOutcome {
        let mut left = self.remaining_failures.lock().unwrap();
        if *left > 0 {
            *left -= 1;
            ToolOutcome::err(tool, "connection reset")
        } else {
            ToolOutcome::ok(tool, serde_json::json!({"ok": true}))
        }
    }
}

fn fast_retry(max_retries: u32) -> RetryConfig {
    RetryConfig {
        max_retries,
        base_delay: Duration::ZERO,
        max_delay: Duration::ZERO,
        jitter: false,
        ..RetryConfig::default()
    }
}

#[tokio::test]
async fn empty_builder_is_passthrough() {
    let runtime = RuntimeBuilder::new().build(Transport::new(0));
    let out = runtime.call_tool("echo", Value::Null, None).await;
    assert!(out.success);
}

#[tokio::test]
async fn retry_layer_recovers_via_builder() {
    // Two transient failures, then success; retry should paper over them.
    let runtime = RuntimeBuilder::new()
        .with_retry(fast_retry(3))
        .build(Transport::new(2));
    let out = runtime.call_tool("echo", Value::Null, None).await;
    assert!(out.success);
    assert_eq!(out.attempts, 3);
}

#[tokio::test]
async fn circuit_breaker_wraps_retry_and_opens() {
    // Retry (1 retry => 2 tries) never succeeds because the transport always fails;
    // the breaker counts each fully-retried failure once and opens at threshold 2.
    let transport = Transport::new(u32::MAX); // always fails
    let runtime = RuntimeBuilder::new()
        .with_retry(fast_retry(1))
        .with_circuit_breaker(CircuitBreakerConfig {
            failure_threshold: 2,
            ..CircuitBreakerConfig::default()
        })
        .build(transport);

    // Two fully-retried failures open the breaker.
    assert!(runtime.call_tool("x", Value::Null, None).await.is_error());
    assert!(runtime.call_tool("x", Value::Null, None).await.is_error());

    // Third call is short-circuited by the open breaker.
    let out = runtime.call_tool("x", Value::Null, None).await;
    assert!(out.error.unwrap().contains(CIRCUIT_OPEN_PREFIX));
}

#[tokio::test]
async fn all_layers_compose() {
    // rate-limit → circuit-breaker → retry → transport, using default policies.
    let runtime = RuntimeBuilder::new()
        .with_retry(fast_retry(2))
        .with_circuit_breaker(CircuitBreakerConfig::default())
        .with_rate_limit(RateLimitConfig::default())
        .build(Transport::new(1)); // one transient failure, then success
    let out = runtime.call_tool("echo", Value::Null, None).await;
    assert!(out.success);
    assert_eq!(out.attempts, 2);
}
