//! # chuk-tool-runtime-mcp
//!
//! MCP transport adapter for [`chuk_tool_runtime`]: an [`McpInvoker`] wraps a
//! `chuk-mcp` client and implements [`ToolInvoker`], mapping a `tools/call`
//! result to a [`ToolOutcome`]. Put one per server behind a
//! [`chuk_tool_runtime::Router`], then wrap that in the policy layers.
//!
//! The adapter is generic over the small [`McpToolClient`] trait (implemented
//! for `chuk_mcp::McpClient`), so the result-mapping logic is unit-testable
//! without a live server.

use std::time::Duration;

use async_trait::async_trait;
use chuk_mcp::protocol::messages::tools::{Tool, ToolResult};
use chuk_mcp::{Connect, McpClient, McpError};
use chuk_tool_runtime::{Router, ToolInvoker, ToolOutcome};
use serde_json::{json, Value};

/// The slice of a `chuk-mcp` client the adapter needs. Implemented for
/// [`McpClient`]; also lets tests inject a mock.
#[async_trait]
pub trait McpToolClient: Send + Sync {
    /// Call a tool with JSON-object arguments.
    async fn call_tool(&self, name: &str, arguments: Value) -> Result<ToolResult, McpError>;
    /// List the tools the server advertises.
    async fn list_tools(&self) -> Result<Vec<Tool>, McpError>;
}

#[async_trait]
impl McpToolClient for McpClient {
    async fn call_tool(&self, name: &str, arguments: Value) -> Result<ToolResult, McpError> {
        McpClient::call_tool(self, name, arguments).await
    }
    async fn list_tools(&self) -> Result<Vec<Tool>, McpError> {
        McpClient::list_tools(self).await
    }
}

/// A [`ToolInvoker`] backed by an MCP client.
pub struct McpInvoker<C> {
    client: C,
}

impl<C: McpToolClient> McpInvoker<C> {
    /// Wrap an MCP client (or anything implementing [`McpToolClient`]).
    pub fn new(client: C) -> Self {
        Self { client }
    }

    /// The names of the tools this server advertises — handy for registering a
    /// server with a [`chuk_tool_runtime::Router`].
    pub async fn tool_names(&self) -> Result<Vec<String>, McpError> {
        Ok(self
            .client
            .list_tools()
            .await?
            .into_iter()
            .map(|tool| tool.name)
            .collect())
    }

    /// Borrow the wrapped client.
    pub fn client(&self) -> &C {
        &self.client
    }
}

#[async_trait]
impl<C: McpToolClient> ToolInvoker for McpInvoker<C> {
    async fn call_tool(&self, tool: &str, args: Value, timeout: Option<Duration>) -> ToolOutcome {
        // The MCP layer requires object arguments; a bare `null` means "no args".
        let args = if args.is_null() { json!({}) } else { args };

        let call = self.client.call_tool(tool, args);
        let result = match timeout {
            Some(t) => match tokio::time::timeout(t, call).await {
                Ok(result) => result,
                Err(_) => return ToolOutcome::err(tool, format!("tool '{tool}' timed out")),
            },
            None => call.await,
        };

        match result {
            Ok(tool_result) => to_outcome(tool, tool_result),
            Err(err) => ToolOutcome::err(tool, err.to_string()),
        }
    }
}

/// Connect to a stdio MCP server (era-detecting) and wrap it as an [`McpInvoker`].
///
/// ```no_run
/// # async fn run() -> Result<(), chuk_mcp::McpError> {
/// let invoker = chuk_tool_runtime_mcp::connect_stdio("mcp-server", ["--flag"]).await?;
/// let names = invoker.tool_names().await?;
/// # Ok(()) }
/// ```
pub async fn connect_stdio(
    command: impl Into<String>,
    args: impl IntoIterator<Item = impl Into<String>>,
) -> Result<McpInvoker<McpClient>, McpError> {
    let client = Connect::to_command(command, args).connect().await?;
    Ok(McpInvoker::new(client))
}

/// Extension for wiring MCP servers into a [`Router`] in one call.
#[async_trait]
pub trait RouterMcpExt {
    /// Connect to a stdio MCP server, discover its tools, and register it under
    /// `server` — the [`Router`] equivalent of the manual connect/discover/add
    /// dance.
    async fn add_mcp_stdio(
        &mut self,
        server: &str,
        command: String,
        args: Vec<String>,
    ) -> Result<(), McpError>;
}

#[async_trait]
impl RouterMcpExt for Router {
    async fn add_mcp_stdio(
        &mut self,
        server: &str,
        command: String,
        args: Vec<String>,
    ) -> Result<(), McpError> {
        let invoker = connect_stdio(command, args).await?;
        let names = invoker.tool_names().await?;
        self.register_tools(server, &names);
        self.add_server(server, Box::new(invoker));
        Ok(())
    }
}

/// Map an MCP `ToolResult` onto a runtime [`ToolOutcome`].
fn to_outcome(tool: &str, result: ToolResult) -> ToolOutcome {
    if result.is_error {
        ToolOutcome::err(tool, error_text(&result.content))
    } else {
        // Preserve the full result (content + resultType + meta) as the payload.
        let value = serde_json::to_value(&result).unwrap_or(Value::Null);
        ToolOutcome::ok(tool, value)
    }
}

/// Extract a human-readable message from error content blocks, falling back to
/// the raw JSON when there are no `text` fields.
fn error_text(content: &[Value]) -> String {
    let texts: Vec<&str> = content
        .iter()
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect();
    if texts.is_empty() {
        serde_json::to_string(content).unwrap_or_else(|_| "tool error".to_string())
    } else {
        texts.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    enum Behavior {
        Ok(ToolResult),
        Err(String),
        Slow(ToolResult),
    }

    struct Mock {
        behavior: Behavior,
        last_args: Mutex<Option<Value>>,
    }
    impl Mock {
        fn new(behavior: Behavior) -> Self {
            Self {
                behavior,
                last_args: Mutex::new(None),
            }
        }
    }

    fn tool_result(json: Value) -> ToolResult {
        serde_json::from_value(json).expect("valid ToolResult json")
    }

    #[async_trait]
    impl McpToolClient for Mock {
        async fn call_tool(&self, _name: &str, arguments: Value) -> Result<ToolResult, McpError> {
            *self.last_args.lock().unwrap() = Some(arguments);
            match &self.behavior {
                Behavior::Ok(tr) => Ok(tr.clone()),
                Behavior::Err(msg) => Err(McpError::validation(msg.clone())),
                Behavior::Slow(tr) => {
                    tokio::time::sleep(Duration::from_secs(10)).await;
                    Ok(tr.clone())
                }
            }
        }
        async fn list_tools(&self) -> Result<Vec<Tool>, McpError> {
            Ok(vec![
                serde_json::from_value(json!({"name": "echo", "inputSchema": {}})).unwrap(),
            ])
        }
    }

    #[tokio::test]
    async fn maps_success_result_to_ok() {
        let mock = Mock::new(Behavior::Ok(tool_result(
            json!({"content": [{"type": "text", "text": "hi"}], "isError": false}),
        )));
        let invoker = McpInvoker::new(mock);
        let out = invoker.call_tool("echo", json!({"q": 1}), None).await;
        assert!(out.success && !out.from_cache);
        assert_eq!(out.result.unwrap()["content"][0]["text"], "hi");
    }

    #[tokio::test]
    async fn maps_tool_error_to_err_with_text() {
        let mock = Mock::new(Behavior::Ok(tool_result(
            json!({"content": [{"type": "text", "text": "bad input"}], "isError": true}),
        )));
        let invoker = McpInvoker::new(mock);
        let out = invoker.call_tool("echo", json!({}), None).await;
        assert!(out.is_error());
        assert_eq!(out.error.unwrap(), "bad input");
    }

    #[tokio::test]
    async fn error_text_falls_back_to_raw_json() {
        let mock = Mock::new(Behavior::Ok(tool_result(
            json!({"content": [{"code": 42}], "isError": true}),
        )));
        let out = McpInvoker::new(mock).call_tool("echo", json!({}), None).await;
        assert!(out.error.unwrap().contains("42")); // no text field → raw json
    }

    #[tokio::test]
    async fn maps_transport_error_to_err() {
        let mock = Mock::new(Behavior::Err("network down".into()));
        let out = McpInvoker::new(mock).call_tool("echo", json!({}), None).await;
        assert!(out.is_error());
        assert!(out.error.unwrap().contains("network down"));
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_returns_error() {
        let mock = Mock::new(Behavior::Slow(tool_result(
            json!({"content": [], "isError": false}),
        )));
        let out = McpInvoker::new(mock)
            .call_tool("echo", json!({}), Some(Duration::from_secs(1)))
            .await;
        assert!(out.is_error());
        assert!(out.error.unwrap().contains("timed out"));
    }

    #[tokio::test]
    async fn null_args_become_empty_object() {
        let mock = Mock::new(Behavior::Ok(tool_result(json!({"content": [], "isError": false}))));
        let invoker = McpInvoker::new(mock);
        invoker.call_tool("echo", Value::Null, None).await;
        assert_eq!(*invoker.client().last_args.lock().unwrap(), Some(json!({})));
    }

    #[tokio::test]
    async fn tool_names_lists_advertised_tools() {
        let mock = Mock::new(Behavior::Ok(tool_result(json!({"content": [], "isError": false}))));
        let invoker = McpInvoker::new(mock);
        assert_eq!(invoker.tool_names().await.unwrap(), vec!["echo".to_string()]);
    }
}
