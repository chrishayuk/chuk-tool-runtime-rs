//! A minimal stdio MCP server exposing an `echo` tool, spawned by the
//! end-to-end test. Modelled on `chuk-mcp`'s demo server.

use chuk_mcp::server::McpServer;
use serde_json::{json, Value};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut server = McpServer::new("chuk-tool-runtime-e2e-echo", env!("CARGO_PKG_VERSION"), None);

    server.register_tool(
        "echo",
        json!({
            "type": "object",
            "properties": {"text": {"type": "string"}},
        }),
        "Echo back the provided text",
        |args| async move {
            let text = args.get("text").and_then(Value::as_str).unwrap_or("");
            Ok(json!(format!("echo: {text}")))
        },
    );

    if let Err(e) = server.run_stdio().await {
        eprintln!("echo server error: {e}");
        std::process::exit(1);
    }
}
