# chuk-tool-runtime-rs — Design

A language-agnostic **tool-execution policy layer** in Rust. It sits between the
MCP protocol core and a client:

```
chuk-mcp-rs                     protocol / transport                (exists)
  └─ chuk-tool-runtime(-rs)     execution policy: retry, circuit
                                breaker, rate limit, cache, routing  (this repo)
       ├─ mcp-cli (Rust)        native consumer
       └─ chuk-tool-processor   Python: PyO3 bindings over this core
                                + Python tool registration/execution
```

This mirrors the `chuk-mcp` → `chuk-mcp-rs` relationship: one fast, correct core,
consumed natively by Rust and via thin bindings from Python.

## Why this layer exists

`chuk-mcp-rs` speaks the protocol; it deliberately has no execution *policy*. The
Python `chuk-tool-processor` grew a lot of valuable policy — retry, circuit
breaker, rate limiting, caching, deadlines, collision-safe routing — but bundled
it with Python-specific tool execution. This crate extracts the **policy** (which
is pure logic + I/O and needs no Python) so it can be shared.

### The extraction line: "does it need to run a Python object?"

- **Goes to Rust (this crate)** — retry/backoff, circuit-breaker state machine,
  token-bucket rate limiting, result caching, deadline/timeout budgeting,
  collision-safe routing (first-wins), tool-call parsing, MCP-call orchestration.
  None of this depends on the language a tool is written in.
- **Stays in Python `chuk-tool-processor`** — registering Python callables as
  tools, `ValidatedTool`/Pydantic, `InProcessStrategy` executing Python
  functions, LangChain interop. The Python package becomes *bindings over this
  core* plus these Python-only surfaces.
- **Not in scope** — isolation of untrusted **Python** code (Seatbelt/Docker/
  Bubblewrap guests). That is a server/agent concern tied to the Python guest and
  stays in `chuk-tool-processor`. (Sandboxing *native* code could be added later
  if a Rust consumer needs it, but it is not a port of the Python isolation.)

For a **client** (mcp-cli) the case is especially clean: its tools are *remote*
MCP tools, so there is never a local Python callable to run — it wants exactly
the part of ctp that ports.

## Core abstraction

Everything is built on one seam:

```rust
#[async_trait]
pub trait ToolInvoker: Send + Sync {
    async fn call_tool(&self, tool: &str, args: Value, timeout: Option<Duration>) -> ToolOutcome;
}
```

- The **innermost** invoker is the transport (an MCP `tools/call` backed by
  `chuk-mcp-rs`).
- Each **policy layer** both implements and wraps a `ToolInvoker`, so a stack is
  just nested invokers. `RuntimeBuilder` assembles them.
- Failures are returned as a failed `ToolOutcome` (`success = false` + `error`),
  never panics — mirroring the Python transport that returns `{isError, error}`.

### Layer order (outermost → innermost)

Matches the Python middleware stack exactly:

```
rate limiting → circuit breaker → retry → transport
```

`RuntimeBuilder::build` applies them innermost-first so retry sits closest to the
transport.

## Behavioural parity with `chuk-tool-processor`

Defaults come straight from the Python MCP middleware (`mcp/middleware.py`):

| Policy | Defaults |
|---|---|
| Retry | `max_retries=3`, `base_delay=1s`, `max_delay=30s`, jitter `[0.5,1.5)`, backoff `min(base·2^attempt, max_delay)` |
| Retry classify | retry on `connection/timeout/refused/reset/closed/"transport not initialized"`; **skip** (never retry) `oauth/unauthorized/authentication/invalid_grant/"no server found"` — skip wins |
| Circuit breaker | `failure_threshold=5`, `success_threshold=2`, `reset_timeout=60s`, `half_open_max_calls=1`, per-tool |
| Rate limiting | off by default; `global=100/60s`, optional per-tool `(count, period)` |

Intentional deviation: Python matched retry substrings case-sensitively but the
skip list case-insensitively; this core normalises **both** to case-insensitive.

## Status

- [x] `ToolInvoker` seam, `ToolOutcome`, `RuntimeBuilder`
- [x] **Retry** layer (backoff, jitter, deadline-capped, error classification) — tested
- [x] **Circuit breaker** layer (per-tool 3-state machine, diagnostics snapshot) — tested
- [x] **Rate limiting** layer (sliding window, global + per-tool, waits) — tested
- [x] **Cache** layer (opt-in per tool, canonical-args key, TTL, success-only) — tested
- [x] **Routing**: `ToolRegistry` (first-wins) + `Router` dispatcher with `call_on` pinning — tested
- [x] **`chuk-mcp-rs` transport adapter** — `crates/chuk-tool-runtime-mcp`: `McpInvoker` (a `ToolInvoker` over a `chuk-mcp` client) — tested
- [x] **End-to-end** — `crates/chuk-tool-runtime-e2e` (`publish = false`): a stdio echo-server bin + a test that connects, routes, wraps with retry+cache, and drives the full stack
- [x] **`chuk-tool-runtime-python`** (PyO3 bindings) — `connect_stdio` / `connect_http` / multi-server `connect`; typed `ToolResult`; `async with` + `close`; per-tool config; ships type stubs. Transport+policy stay Rust, only results cross to Python.
- [x] **HTTP transport** (`connect_http` / `Router::add_mcp_http`) and **config builders** (`with_*` setters + per-tool adders)

## On wrapping `chuk-tool-processor` over this runtime

Deliberately **not** doing this in the near term. ctp's retry/breaker/rate-limit/
cache wrappers are already *shared* across its local-tool path and its MCP path,
so replacing only the MCP path forks the policy into two implementations, and
replacing the local path would round-trip Rust→Python per call for tools that are
inherently Python. ctp works today. The runtime's real consumers are a Rust
`mcp-cli` and new Python code that wants the Rust runtime directly (the bindings).
ctp can adopt this later, selectively (its MCP path only), if exact behavioural
parity with `mcp-cli` becomes worth the fork — but that's a low-ROI change today.

> **Note:** the MCP adapter currently uses a **path dependency** on the local
> `chuk-mcp-rs/crates/chuk-mcp` (that crate isn't published to crates.io yet), so
> building `chuk-tool-runtime-mcp` needs `chuk-mcp-rs` checked out as a sibling.
> The **core** crate has no such dependency. Switch to a version/git dep once
> `chuk-mcp` is published.
- [ ] Diagnostics/status surface (mirrors `MiddlewareStatus`)

## Build order rationale

Retry first: it is the innermost layer, fully self-contained, and the thing a
client needs on day one. Circuit breaker and rate limiting are per-tool stateful
layers that slot in above it without touching the seam. The transport adapter and
PyO3 bindings come once the policy layers are proven, so both `mcp-cli` and
`chuk-tool-processor` can adopt the same core.
