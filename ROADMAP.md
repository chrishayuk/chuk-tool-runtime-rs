# chuk-tool-runtime-rs — Roadmap & Status

Pickup notes: where the project is, what's next, and how to run it. For the
*why* behind the architecture see [DESIGN.md](./DESIGN.md).

Last updated: 2026-07-31 (branch `main`, HEAD `5a27805`).

## The goal

A Rust reimplementation of `chuk-tool-processor`'s execution/policy value, so a
mostly-Rust `mcp-cli` (with a thin `uv` wrapper) becomes possible. One fast,
correct core, consumed natively by Rust and via thin PyO3 bindings from Python —
mirroring the `chuk-mcp` → `chuk-mcp-rs` relationship.

Standing quality bar for every change: **modular, decoupled, good folder/file
structure, no large files, ≥90% test coverage per file, no magic strings/numbers.**

## Crates

| Crate | Purpose | Publish |
|---|---|---|
| `chuk-tool-runtime` | Core policy layers + routing (no MCP dep) | yes |
| `chuk-tool-runtime-mcp` | `McpInvoker` adapter over `chuk-mcp` (path dep) | yes |
| `chuk-tool-runtime-python` | PyO3 bindings, module `chuk_tool_runtime_rs` | yes |
| `chuk-tool-runtime-isolation` | OS sandboxing (Seatbelt/Local/Bubblewrap) + broker | yes |
| `chuk-tool-runtime-e2e` | End-to-end stdio echo-server + integration test | no |

## Status

### Phase A — MCP plumbing + policy + bindings ✅ DONE
- [x] `ToolInvoker` seam, `ToolOutcome`, `RuntimeBuilder`
- [x] Policy layers: **retry**, **circuit breaker**, **rate limit**, **cache** — each tested
- [x] **Routing**: `ToolRegistry` (first-wins) + `Router` with `call_on` pinning
- [x] **MCP transport adapter** (`McpInvoker`), stdio + HTTP
- [x] **End-to-end** stdio echo-server + full-stack test
- [x] **PyO3 bindings**: `connect_stdio` / `connect_http` / `connect`; typed `ToolResult`; `async with` + `close`; per-tool config; type stubs
- [x] **Config builders** (`with_*` setters + per-tool adders); pinned `call_on`

### Phase B — Isolation / sandboxing ✅ MOSTLY DONE
- [x] Wire protocol (`wire.rs`) — length-prefixed JSON, 8MB frame cap
- [x] `Broker` (host, unix socket) — token auth, host-authoritative namespace, allowlist, tool-call cap
- [x] Guest bootstrap (`resources/guest.py`) — stdlib-only, async tool proxies
- [x] `IsolatedRunner` — orchestrates broker + sandboxed guest
- [x] Backends: **Seatbelt** (macOS, end-to-end verified), **Local** (no-op, gated by `allow_no_isolation`), **Bubblewrap** (Linux; argv unit-tested)
- [x] **Resource limits** — `IsolationLimits` (wall / CPU-s / memory / pids / tool-call / output caps), enforced across runner + broker + guest `setrlimit`. Defaults mirror ctp.
- [ ] **Docker backend** — DEFERRED until a Docker-daemon CI can cover pull/run/remove to the 90% bar. The trait's `*_guest` / `python_exe` / `extra_env` hooks are already shaped for it.

### Phase C — Execution engine + tool models ⏳ NOT STARTED
- [ ] Tool result / call models (ctp `ToolResult` / `ToolCall` parity)
- [ ] Execution engine tying policy + routing + isolation into one entry point

### Phase D — Discovery / parsers ⏳ NOT STARTED
- [ ] Tool-call parsers (extract calls from LLM output)
- [ ] Discovery / search over available tools

### Phase E — Back mcp-cli's ToolManager with the Rust runtime ⏳ NOT STARTED
- Branch `feat/rust-runtime-backend` in **mcp-cli** (gradual — mcp-cli is popular)
- Opt-in flag; keep ctp for `ToolProcessor`/models/discovery; keep `ToolManager` public API stable
- mcp-cli uses **none** of ctp's isolation — its tools are remote MCP tools

## Suggested next step

**Expose `IsolatedRunner` to Python** (extends Phase A bindings into Phase B) *or*
start **Phase C** (execution engine + models). The isolation stack is proven
end-to-end on macOS; the natural follow-ons are making it callable from Python
and giving it a unified execution entry point.

## Dev loop

```bash
# Build / test the whole workspace (PyO3 crate excluded via default-members)
cargo test
cargo clippy --all-targets -- -D warnings

# Per-file coverage (quality gate: every file ≥90%)
cargo llvm-cov -p chuk-tool-runtime-isolation

# Python bindings (mixed maturin layout)
cd crates/chuk-tool-runtime-python && maturin develop
```

Notes:
- Isolation tests that need a sandbox/python **skip** gracefully where unavailable
  (Seatbelt e2e is macOS-only; Bubblewrap argv is unit-tested, not run, off Linux).
- The MCP adapter uses a **path dependency** on sibling `chuk-mcp-rs/crates/chuk-mcp`
  (not yet on crates.io) — check it out as a sibling to build that crate.
- Commits use `--signoff` (DCO); Block Secrets / IBM Vault Radar guards them.
