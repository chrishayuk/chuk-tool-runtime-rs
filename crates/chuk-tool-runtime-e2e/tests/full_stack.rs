//! End-to-end: connect to a real stdio MCP server, wrap it as a `ToolInvoker`,
//! route it, apply the policy layers, and drive tool calls through the lot.

use chuk_tool_runtime::{CacheConfig, RetryConfig, Router, RuntimeBuilder, ToolInvoker};
use chuk_tool_runtime_mcp::McpInvoker;
use serde_json::json;

#[tokio::test]
async fn echo_over_the_full_stack() {
    // 1. Connect to the spawned Rust echo server (era-detecting connect).
    let client = chuk_mcp::Connect::to_command(env!("CARGO_BIN_EXE_mcp-echo-server"), Vec::<String>::new())
        .connect()
        .await
        .expect("connect to echo server");
    let invoker = McpInvoker::new(client);

    // 2. Discover its tools and register the server in a Router.
    let tools = invoker.tool_names().await.expect("list tools");
    assert!(tools.contains(&"echo".to_string()), "server advertised: {tools:?}");

    let mut router = Router::new();
    router.register_tools("echo-server", &tools);
    router.add_server("echo-server", Box::new(invoker));

    // 3. Wrap the router in the policy layers.
    let cache = CacheConfig {
        cacheable_tools: ["echo".to_string()].into_iter().collect(),
        ..Default::default()
    };
    let runtime = RuntimeBuilder::new()
        .with_retry(RetryConfig::default())
        .with_cache(cache)
        .build(router);

    // 4. Call the tool through the whole stack.
    let out = runtime.call_tool("echo", json!({"text": "hi"}), None).await;
    assert!(out.success, "call failed: {:?}", out.error);
    assert!(!out.from_cache);
    let rendered = serde_json::to_string(&out.result).unwrap();
    assert!(rendered.contains("echo: hi"), "unexpected result: {rendered}");

    // 5. Repeat call is served from the cache (never reaches the server).
    let cached = runtime.call_tool("echo", json!({"text": "hi"}), None).await;
    assert!(cached.from_cache);
    assert_eq!(cached.result, out.result);

    // 6. An unknown tool is a routing error.
    let miss = runtime.call_tool("does_not_exist", json!({}), None).await;
    assert!(miss.is_error());
}
