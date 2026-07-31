# chuk-tool-runtime-rs

A language-agnostic **tool-execution policy layer** in Rust — retry, circuit
breaker, rate limiting, caching, and collision-safe routing — that sits *above*
the MCP protocol core ([`chuk-mcp-rs`](https://github.com/IBM/chuk-mcp-rs)) and
*below* a client (a Rust `mcp-cli`, or the Python
[`chuk-tool-processor`](https://github.com/IBM/chuk-tool-processor) via PyO3
bindings).

It's the Rust counterpart to the execution-policy value of `chuk-tool-processor`,
extracted so the same core can be shared by native-Rust and Python consumers —
the same "one core, thin bindings" split as `chuk-mcp` → `chuk-mcp-rs`.

See **[DESIGN.md](DESIGN.md)** for the architecture, the extraction line
(what's Rust vs what stays Python), and the roadmap.

## Layout

```
crates/chuk-tool-runtime          # the core policy crate (no MCP dependency)
crates/chuk-tool-runtime-mcp      # MCP transport adapter (McpInvoker over a chuk-mcp client)
crates/chuk-tool-runtime-python   # PyO3 bindings (module: chuk_tool_runtime_rs)
crates/chuk-tool-runtime-e2e      # echo-server bin + full-stack test (publish = false)
```

The MCP adapter, bindings, and e2e crates path-depend on the local `chuk-mcp-rs`
(unpublished); the **core** crate does not.

## From Python

```python
import asyncio, chuk_tool_runtime_rs as rt

async def main():
    async with await rt.connect_stdio(
        "./target/debug/mcp-echo-server",
        retry=rt.RetryConfig(max_retries=2),
        cache=rt.CacheConfig(cacheable_tools=["echo"]),
    ) as runtime:
        print(runtime.tools)                       # -> ['echo']
        out = await runtime.call_tool("echo", {"text": "hi"})
        print(out.success, out.result, out.from_cache)   # typed ToolResult

asyncio.run(main())
```

Ships type stubs (`py.typed`), so editors get full autocomplete. Leaving the
`async with` (or calling `await runtime.close()`) shuts the server down.

Build the extension with `maturin develop -m crates/chuk-tool-runtime-python/Cargo.toml`.

## Quickstart

```rust
use chuk_tool_runtime::{RuntimeBuilder, RetryConfig};

let runtime = RuntimeBuilder::new()
    .with_retry(RetryConfig::default())
    .build(transport); // any `ToolInvoker` (e.g. an MCP tools/call adapter)

let outcome = runtime.call_tool("search", args, None).await;
```

## Develop

```sh
cargo build
cargo test
```

## Status

Early. The `ToolInvoker` seam, `ToolOutcome`, `RuntimeBuilder`, and the **retry**
layer are implemented and tested; circuit breaker, rate limiting, caching,
routing, the `chuk-mcp-rs` transport adapter, and the PyO3 bindings are next (see
DESIGN.md).

## License

Apache-2.0
