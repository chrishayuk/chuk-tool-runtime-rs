//! # chuk-tool-runtime
//!
//! A language-agnostic **tool-execution policy layer** — retry, circuit breaker,
//! rate limiting, caching, and collision-safe routing — that sits *above* an MCP
//! transport (`chuk-mcp-rs`) and *below* a client (a Rust `mcp-cli`, or the
//! Python `chuk-tool-processor` via bindings).
//!
//! Everything is built on one seam, [`ToolInvoker`]: the innermost invoker is the
//! transport; each policy is a layer that both implements and wraps a
//! `ToolInvoker`, so a stack is just nested invokers. Use [`RuntimeBuilder`] to
//! assemble a stack in the correct order.
//!
//! ```
//! use chuk_tool_runtime::{RuntimeBuilder, RetryConfig, ToolInvoker, ToolOutcome};
//! use async_trait::async_trait;
//! use serde_json::Value;
//! use std::time::Duration;
//!
//! struct Transport;
//! #[async_trait]
//! impl ToolInvoker for Transport {
//!     async fn call_tool(&self, tool: &str, _a: Value, _t: Option<Duration>) -> ToolOutcome {
//!         ToolOutcome::ok(tool, serde_json::json!({"echoed": true}))
//!     }
//! }
//!
//! # async fn run() {
//! let runtime = RuntimeBuilder::new()
//!     .with_retry(RetryConfig::default())
//!     .build(Transport);
//! let out = runtime.call_tool("echo", Value::Null, None).await;
//! assert!(out.success);
//! # }
//! ```

pub mod config;
pub mod error;
pub mod invoker;
pub mod layers;
pub mod outcome;

pub use config::{CircuitBreakerConfig, RetryConfig};
pub use error::RuntimeError;
pub use invoker::ToolInvoker;
pub use layers::{
    CircuitBreakerLayer, CircuitSnapshot, CircuitState, RetryLayer, CIRCUIT_OPEN_PREFIX,
};
pub use outcome::ToolOutcome;

/// Assembles a policy stack over a transport invoker.
///
/// Layers are applied innermost-first so the runtime order matches the Python
/// middleware stack: `rate limiting → circuit breaker → retry → transport`
/// (only retry is wired today; the rest are tracked in DESIGN.md and slot in
/// here without changing the seam).
#[derive(Default)]
pub struct RuntimeBuilder {
    retry: Option<RetryConfig>,
    circuit_breaker: Option<CircuitBreakerConfig>,
}

impl RuntimeBuilder {
    /// A builder with no layers.
    pub fn new() -> Self {
        Self::default()
    }

    /// Enable the retry layer with `config`.
    pub fn with_retry(mut self, config: RetryConfig) -> Self {
        self.retry = Some(config);
        self
    }

    /// Enable the circuit-breaker layer with `config` (sits above retry).
    pub fn with_circuit_breaker(mut self, config: CircuitBreakerConfig) -> Self {
        self.circuit_breaker = Some(config);
        self
    }

    /// Wrap `transport` with the configured layers, returning the assembled
    /// invoker.
    ///
    /// Layers are applied innermost-first so the runtime order matches the Python
    /// middleware stack: `circuit breaker → retry → transport` (rate limiting will
    /// wrap outermost once added).
    pub fn build<I>(self, transport: I) -> Box<dyn ToolInvoker>
    where
        I: ToolInvoker + 'static,
    {
        let mut invoker: Box<dyn ToolInvoker> = Box::new(transport);
        // Retry closest to the transport...
        if let Some(cfg) = self.retry {
            invoker = Box::new(RetryLayer::new(invoker, cfg));
        }
        // ...circuit breaker above it (a fully-retried failure counts once).
        if let Some(cfg) = self.circuit_breaker {
            invoker = Box::new(CircuitBreakerLayer::new(invoker, cfg));
        }
        invoker
    }
}
