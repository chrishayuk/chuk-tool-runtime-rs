//! End-to-end: connect to a real stdio MCP server, route it, apply the policy
//! layers, and drive tool calls through the lot — using the one-call
//! `Router::add_mcp_stdio` convenience.

use chuk_tool_runtime::{CacheConfig, RetryConfig, Router, RuntimeBuilder};
use chuk_tool_runtime_mcp::RouterMcpExt;
use serde_json::json;

#[tokio::test]
async fn echo_over_the_full_stack() {
    // 1. Connect + discover + register the spawned Rust echo server in one call.
    let mut router = Router::new();
    router
        .add_mcp_stdio(
            "echo-server",
            env!("CARGO_BIN_EXE_mcp-echo-server").to_string(),
            vec![],
        )
        .await
        .expect("add echo server");
    assert_eq!(router.server_for_tool("echo"), Some("echo-server"));

    // 2. Wrap the router in the policy layers.
    let cache = CacheConfig {
        cacheable_tools: ["echo".to_string()].into_iter().collect(),
        ..Default::default()
    };
    let runtime = RuntimeBuilder::new()
        .with_retry(RetryConfig::default())
        .with_cache(cache)
        .build(router);

    // 3. Call the tool through the whole stack.
    let out = runtime.call_tool("echo", json!({"text": "hi"}), None).await;
    assert!(out.success, "call failed: {:?}", out.error);
    assert!(!out.from_cache);
    let rendered = serde_json::to_string(&out.result).unwrap();
    assert!(rendered.contains("echo: hi"), "unexpected result: {rendered}");

    // 4. Repeat call is served from the cache (never reaches the server).
    let cached = runtime.call_tool("echo", json!({"text": "hi"}), None).await;
    assert!(cached.from_cache);
    assert_eq!(cached.result, out.result);

    // 5. An unknown tool is a routing error; into_result() surfaces it.
    let miss = runtime.call_tool("does_not_exist", json!({}), None).await;
    assert!(miss.into_result().is_err());
}
