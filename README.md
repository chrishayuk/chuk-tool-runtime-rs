# chuk-tool-runtime-rs

A language-agnostic **tool-execution runtime** in Rust. It wraps MCP tool calls
in production-grade policy — **retry, circuit breaker, rate limiting, caching,
and collision-safe multi-server routing** — and exposes the whole thing to both
Rust and Python.

```
cache → rate limit → circuit breaker → retry → routing → MCP server(s)
```

It's the Rust counterpart to the execution-policy value of
[`chuk-tool-processor`](https://github.com/IBM/chuk-tool-processor), extracted so
one core is shared by native-Rust consumers (e.g. a Rust `mcp-cli`) and Python —
the same "one core, thin bindings" split as `chuk-mcp` → `chuk-mcp-rs`.

---

## Getting started — Rust

Wire one or more MCP servers into a `Router`, then wrap it in the policy layers:

```rust
use std::time::Duration;
use chuk_tool_runtime::{RuntimeBuilder, RetryConfig, CacheConfig, Router};
use chuk_tool_runtime_mcp::RouterMcpExt; // add_mcp_stdio / add_mcp_http
use serde_json::json;

// inside an async fn:
let mut router = Router::new();
router.add_mcp_stdio("db", "mcp-server-sqlite".into(),
                     vec!["--db-path".into(), "app.db".into()]).await?;
router.add_mcp_http("web", "https://mcp.example.com/mcp".into()).await?;

let runtime = RuntimeBuilder::new()
    .with_retry(RetryConfig::default().with_max_retries(3))
    .with_cache(CacheConfig::default().cacheable_tool("query")
                                      .with_default_ttl(Duration::from_secs(300)))
    .build_router(router);

// routed first-wins across servers
let out = runtime.call_tool("query", json!({"sql": "select 1"}), None).await;
if out.success {
    println!("{:?}", out.result);
}

// reach a specific server explicitly (bypasses the policy layers)
let pinned = runtime.call_on("web", "search", json!({"q": "rust"}), None).await;
```

`call_tool` never panics on a tool failure — it returns a `ToolOutcome`
(`success`, `result`, `error`, `attempts`, `from_cache`); use `into_result()` if
you'd rather have a `Result<Value, String>` to `?` on.

Every policy layer implements one trait — `ToolInvoker` (`async fn call_tool`) —
so you can also wrap **any** transport (implement `ToolInvoker` yourself) or
compose layers directly. See [DESIGN.md](DESIGN.md) for the architecture and
[ROADMAP.md](ROADMAP.md) for current status and what's next.

---

## Getting started — Python

The Python package (`chuk_tool_runtime_rs`) gives you the same runtime with an
`async`/`await` API and full type hints. Build it into your environment (needs
[maturin](https://www.maturin.rs) and a checkout of `chuk-mcp-rs` as a sibling):

```sh
maturin develop -m crates/chuk-tool-runtime-python/Cargo.toml
```

```python
import asyncio
import chuk_tool_runtime_rs as rt

async def main():
    async with await rt.connect_stdio(
        "mcp-server-sqlite", args=["--db-path", "app.db"],
        retry=rt.RetryConfig(max_retries=3),
        cache=rt.CacheConfig(cacheable_tools=["query"]),
    ) as runtime:
        print(runtime.tools)                       # what's available

        result = await runtime.call_tool("query", {"sql": "select 1"})
        if result.success:
            print(result.result)                   # typed: .success / .result / .error
        print("from cache:", result.from_cache)

asyncio.run(main())
```

`call_tool` returns a typed `ToolResult` (never raises on a tool failure). Leaving
the `async with` (or `await runtime.close()`) shuts the server down.

**HTTP servers**, **several servers at once** (routed first-wins), and **pinned
calls**:

```python
runtime = await rt.connect_http("https://mcp.example.com/mcp")

runtime = await rt.connect(servers=[
    {"name": "db",  "command": "mcp-server-sqlite", "args": ["--db-path", "app.db"]},
    {"name": "web", "url": "https://mcp.example.com/mcp"},
])
await runtime.call_tool("query", {"sql": "..."})       # routed first-wins
await runtime.call_on("web", "search", {"q": "..."})    # pin to a specific server
```

**Tune the policies** (all optional; pass any subset):

```python
rt.RetryConfig(max_retries=3, base_delay=1.0, max_delay=30.0, jitter=True)
rt.CircuitBreakerConfig(failure_threshold=5, reset_timeout=60.0)
rt.RateLimitConfig(global_limit=100, global_period=60.0, per_tool={"slow": (5, 60.0)})
rt.CacheConfig(cacheable_tools=["query"], default_ttl=300.0, per_tool_ttl={"query": 30.0})
```

---

## Layout

```
crates/chuk-tool-runtime          # core policy (retry, breaker, rate-limit, cache, routing) — no MCP dep
crates/chuk-tool-runtime-mcp      # MCP transport adapter (McpInvoker over a chuk-mcp client)
crates/chuk-tool-runtime-python   # PyO3 bindings (module: chuk_tool_runtime_rs)
crates/chuk-tool-runtime-e2e      # echo-server bin + full-stack test (publish = false)
```

The MCP adapter, bindings, and e2e crates path-depend on the local `chuk-mcp-rs`
(unpublished); the **core** crate does not.

## Develop

```sh
cargo test      # core + adapter + e2e (spawns a real echo server)
cargo clippy --all-targets -- -D warnings
maturin develop -m crates/chuk-tool-runtime-python/Cargo.toml   # build the Python extension
```

## License

Apache-2.0
