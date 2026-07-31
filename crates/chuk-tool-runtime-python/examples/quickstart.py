"""Quickstart: drive the Rust tool runtime from Python against a stdio MCP server.

Build the extension into your venv first:

    maturin develop -m crates/chuk-tool-runtime-python/Cargo.toml

Then run against any stdio MCP server command, e.g. the workspace echo server:

    cargo build -p chuk-tool-runtime-e2e --bin mcp-echo-server
    python quickstart.py ./target/debug/mcp-echo-server
"""

import asyncio
import sys

import chuk_tool_runtime_rs as rt


async def main(server_command: str) -> None:
    # Connect (era-detecting) and wrap the server in the policy layers.
    runtime = await rt.connect_stdio(
        server_command,
        retry=rt.RetryConfig(max_retries=2),
        cache=rt.CacheConfig(cacheable_tools=["echo"], default_ttl=300.0),
    )

    out = await runtime.call_tool("echo", {"text": "hello"})
    print("success:", out["success"], "| result:", out["result"])

    cached = await runtime.call_tool("echo", {"text": "hello"})
    print("second call from_cache:", cached["from_cache"])


if __name__ == "__main__":
    if len(sys.argv) < 2:
        raise SystemExit("usage: python quickstart.py <stdio-mcp-server-command>")
    asyncio.run(main(sys.argv[1]))
