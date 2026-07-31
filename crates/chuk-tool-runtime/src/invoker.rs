//! The core abstraction: something that can invoke a tool by name.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use crate::outcome::ToolOutcome;

/// Invokes a tool by name and returns its [`ToolOutcome`].
///
/// This is the seam the whole runtime is built on. The innermost invoker is the
/// transport (e.g. an MCP `tools/call` backed by `chuk-mcp-rs`); every policy
/// layer (retry, circuit breaker, rate limiting, …) both *implements* and
/// *wraps* a `ToolInvoker`, so layers compose by nesting.
///
/// Implementations must not panic: transport/infra failures are returned as a
/// failed [`ToolOutcome`], never as an unwind.
#[async_trait]
pub trait ToolInvoker: Send + Sync {
    /// Invoke `tool` with `args`, optionally bounded by `timeout`.
    async fn call_tool(&self, tool: &str, args: Value, timeout: Option<Duration>) -> ToolOutcome;
}

/// Delegating impl so a boxed invoker is itself a `ToolInvoker` — lets the
/// builder stack heterogeneous layers behind a single `Box<dyn ToolInvoker>`.
#[async_trait]
impl ToolInvoker for Box<dyn ToolInvoker> {
    async fn call_tool(&self, tool: &str, args: Value, timeout: Option<Duration>) -> ToolOutcome {
        (**self).call_tool(tool, args, timeout).await
    }
}

/// Delegating impl for shared invokers — lets a stack be built over an
/// `Arc<Router>` while a handle to the same router is kept for pinned calls.
#[async_trait]
impl<T: ToolInvoker + ?Sized> ToolInvoker for std::sync::Arc<T> {
    async fn call_tool(&self, tool: &str, args: Value, timeout: Option<Duration>) -> ToolOutcome {
        (**self).call_tool(tool, args, timeout).await
    }
}
