//! Multi-server dispatcher.
//!
//! A `Router` holds one [`ToolInvoker`] per server (each typically an MCP
//! transport) plus a [`ToolRegistry`]. As a `ToolInvoker` it dispatches a tool
//! to its first-wins owner; [`Router::call_on`] dispatches to an explicitly
//! pinned server (the equivalent of `call_tool(..., server_name=...)`).
//!
//! It is the innermost invoker — the policy layers (retry, circuit breaker, rate
//! limiting, cache) wrap a `Router`.

use std::collections::HashMap;

use async_trait::async_trait;
use serde_json::Value;
use tokio::time::Duration;

use super::registry::ToolRegistry;
use crate::invoker::ToolInvoker;
use crate::outcome::ToolOutcome;

/// Error-message prefix used when a tool can't be routed to a server.
pub const NO_SERVER_PREFIX: &str = "no server found for tool";

/// Routes tool calls across multiple per-server invokers.
#[derive(Default)]
pub struct Router {
    servers: HashMap<String, Box<dyn ToolInvoker>>,
    registry: ToolRegistry,
}

impl Router {
    /// An empty router.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add (or replace) the invoker for `server`.
    pub fn add_server(&mut self, server: impl Into<String>, invoker: Box<dyn ToolInvoker>) -> &mut Self {
        self.servers.insert(server.into(), invoker);
        self
    }

    /// Record that `server` advertises `tools` (first-wins ownership).
    pub fn register_tools(&mut self, server: &str, tools: &[String]) -> &mut Self {
        self.registry.register(server, tools);
        self
    }

    /// The server that owns default routing for `tool`.
    pub fn server_for_tool(&self, tool: &str) -> Option<&str> {
        self.registry.owner(tool)
    }

    /// Every server advertising `tool`, in registration order.
    pub fn servers_for_tool(&self, tool: &str) -> Vec<String> {
        self.registry.providers(tool)
    }

    /// Tool names advertised by more than one server.
    pub fn collisions(&self) -> HashMap<String, Vec<String>> {
        self.registry.collisions()
    }

    /// Dispatch `tool` to an explicitly pinned `server`, bypassing ownership.
    pub async fn call_on(
        &self,
        server: &str,
        tool: &str,
        args: Value,
        timeout: Option<Duration>,
    ) -> ToolOutcome {
        match self.servers.get(server) {
            Some(invoker) => invoker.call_tool(tool, args, timeout).await,
            None => ToolOutcome::err(tool, format!("{NO_SERVER_PREFIX} '{tool}': unknown server '{server}'")),
        }
    }
}

#[async_trait]
impl ToolInvoker for Router {
    async fn call_tool(&self, tool: &str, args: Value, timeout: Option<Duration>) -> ToolOutcome {
        match self.registry.owner(tool) {
            Some(server) => match self.servers.get(server) {
                Some(invoker) => invoker.call_tool(tool, args, timeout).await,
                // Owner recorded but its transport was never added.
                None => ToolOutcome::err(tool, format!("{NO_SERVER_PREFIX} '{tool}'")),
            },
            None => ToolOutcome::err(tool, format!("{NO_SERVER_PREFIX} '{tool}'")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Invoker tagged with the server that owns it, so tests can see who handled a call.
    struct Tagged(&'static str);
    #[async_trait]
    impl ToolInvoker for Tagged {
        async fn call_tool(&self, tool: &str, _a: Value, _t: Option<Duration>) -> ToolOutcome {
            ToolOutcome::ok(tool, serde_json::json!({ "handled_by": self.0 }))
        }
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn two_server_router() -> Router {
        let mut r = Router::new();
        r.add_server("a", Box::new(Tagged("a")));
        r.add_server("b", Box::new(Tagged("b")));
        r.register_tools("a", &names(&["shared"]));
        r.register_tools("b", &names(&["shared", "only_b"]));
        r
    }

    #[tokio::test]
    async fn routes_to_first_wins_owner() {
        let r = two_server_router();
        let out = r.call_tool("shared", Value::Null, None).await;
        assert_eq!(out.result, Some(serde_json::json!({"handled_by": "a"})));
    }

    #[tokio::test]
    async fn routes_unique_tool_to_its_server() {
        let r = two_server_router();
        let out = r.call_tool("only_b", Value::Null, None).await;
        assert_eq!(out.result, Some(serde_json::json!({"handled_by": "b"})));
    }

    #[tokio::test]
    async fn unknown_tool_errors() {
        let r = two_server_router();
        let out = r.call_tool("ghost", Value::Null, None).await;
        assert!(out.is_error());
        assert!(out.error.unwrap().contains(NO_SERVER_PREFIX));
    }

    #[tokio::test]
    async fn call_on_pins_the_shadowed_server() {
        let r = two_server_router();
        // "shared" is owned by "a", but pin to "b" explicitly.
        let out = r.call_on("b", "shared", Value::Null, None).await;
        assert_eq!(out.result, Some(serde_json::json!({"handled_by": "b"})));
    }

    #[tokio::test]
    async fn call_on_unknown_server_errors() {
        let r = two_server_router();
        let out = r.call_on("nope", "shared", Value::Null, None).await;
        assert!(out.error.unwrap().contains("unknown server 'nope'"));
    }

    #[tokio::test]
    async fn owner_without_transport_errors() {
        let mut r = Router::new();
        r.register_tools("ghost_server", &names(&["t"])); // registered but no add_server
        let out = r.call_tool("t", Value::Null, None).await;
        assert!(out.error.unwrap().contains(NO_SERVER_PREFIX));
    }

    #[test]
    fn exposes_registry_queries() {
        let r = two_server_router();
        assert_eq!(r.server_for_tool("shared"), Some("a"));
        assert_eq!(r.servers_for_tool("shared"), vec!["a", "b"]);
        assert_eq!(r.collisions().get("shared"), Some(&vec!["a".into(), "b".into()]));
    }
}
