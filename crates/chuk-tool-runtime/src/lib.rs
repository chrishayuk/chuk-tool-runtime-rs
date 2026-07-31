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
pub mod routing;

pub use config::{CacheConfig, CircuitBreakerConfig, RateLimitConfig, RetryConfig};
pub use error::RuntimeError;
pub use invoker::ToolInvoker;
pub use layers::{
    CacheLayer, CircuitBreakerLayer, CircuitSnapshot, CircuitState, RateLimitLayer, RetryLayer,
    CIRCUIT_OPEN_PREFIX,
};
pub use outcome::ToolOutcome;
pub use routing::{Router, ToolRegistry, NO_SERVER_PREFIX};

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

/// A built tool-execution runtime: the assembled policy stack over a transport.
///
/// Returned by [`RuntimeBuilder::build`] / [`RuntimeBuilder::build_router`]. Call
/// tools through it with [`Runtime::call_tool`]; dropping it shuts the underlying
/// transport(s) down (RAII — for a stdio MCP server, closing our end signals it
/// to exit).
pub struct Runtime {
    inner: Box<dyn ToolInvoker>,
    router: Option<Arc<Router>>,
}

impl Runtime {
    /// Call `tool` with `args` through the full policy stack. The tool is routed
    /// to its first-wins owner.
    pub async fn call_tool(&self, tool: &str, args: Value, timeout: Option<Duration>) -> ToolOutcome {
        self.inner.call_tool(tool, args, timeout).await
    }

    /// Call `tool` on an explicitly pinned `server`, **bypassing the policy
    /// layers** (a direct, unambiguous call to that server). Only available when
    /// the runtime was built over a [`Router`] via [`RuntimeBuilder::build_router`];
    /// otherwise returns a failed outcome.
    pub async fn call_on(
        &self,
        server: &str,
        tool: &str,
        args: Value,
        timeout: Option<Duration>,
    ) -> ToolOutcome {
        match &self.router {
            Some(router) => router.call_on(server, tool, args, timeout).await,
            None => ToolOutcome::err(tool, "call_on requires a router-based runtime"),
        }
    }
}

#[async_trait]
impl ToolInvoker for Runtime {
    async fn call_tool(&self, tool: &str, args: Value, timeout: Option<Duration>) -> ToolOutcome {
        self.inner.call_tool(tool, args, timeout).await
    }
}

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
    rate_limit: Option<RateLimitConfig>,
    cache: Option<CacheConfig>,
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

    /// Enable the rate-limiting layer with `config`.
    pub fn with_rate_limit(mut self, config: RateLimitConfig) -> Self {
        self.rate_limit = Some(config);
        self
    }

    /// Enable the result-cache layer with `config` (outermost — a hit
    /// short-circuits every other layer).
    pub fn with_cache(mut self, config: CacheConfig) -> Self {
        self.cache = Some(config);
        self
    }

    /// Wrap `transport` with the configured layers, returning the assembled
    /// invoker.
    ///
    /// Layers are applied innermost-first so the runtime order matches the Python
    /// stacks: `cache → rate limiting → circuit breaker → retry → transport`.
    pub fn build<I>(self, transport: I) -> Runtime
    where
        I: ToolInvoker + 'static,
    {
        Runtime {
            inner: self.wrap(Box::new(transport)),
            router: None,
        }
    }

    /// Build over a [`Router`], keeping a handle so [`Runtime::call_on`] can issue
    /// pinned calls to a specific server.
    pub fn build_router(self, router: Router) -> Runtime {
        let router = Arc::new(router);
        let base: Box<dyn ToolInvoker> = Box::new(router.clone());
        Runtime {
            inner: self.wrap(base),
            router: Some(router),
        }
    }

    /// Wrap `invoker` innermost-first with the configured layers.
    fn wrap(self, mut invoker: Box<dyn ToolInvoker>) -> Box<dyn ToolInvoker> {
        // Retry closest to the transport...
        if let Some(cfg) = self.retry {
            invoker = Box::new(RetryLayer::new(invoker, cfg));
        }
        // ...circuit breaker above it (a fully-retried failure counts once)...
        if let Some(cfg) = self.circuit_breaker {
            invoker = Box::new(CircuitBreakerLayer::new(invoker, cfg));
        }
        // ...rate limiting above that...
        if let Some(cfg) = self.rate_limit {
            invoker = Box::new(RateLimitLayer::new(invoker, cfg));
        }
        // ...cache outermost (a hit short-circuits all of the above).
        if let Some(cfg) = self.cache {
            invoker = Box::new(CacheLayer::new(invoker, cfg));
        }
        invoker
    }
}
