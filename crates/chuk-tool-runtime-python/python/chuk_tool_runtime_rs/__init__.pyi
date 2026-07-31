"""Type stubs for chuk_tool_runtime_rs (the Rust tool runtime)."""

from typing import Any, Optional, Sequence

class ToolResult:
    """The typed result of a tool call."""

    tool: str
    success: bool
    result: Any
    error: Optional[str]
    attempts: int
    from_cache: bool
    def __repr__(self) -> str: ...

class RetryConfig:
    def __init__(
        self,
        max_retries: int = 3,
        base_delay: float = 1.0,
        max_delay: float = 30.0,
        jitter: bool = True,
    ) -> None: ...

class CircuitBreakerConfig:
    def __init__(
        self,
        failure_threshold: int = 5,
        success_threshold: int = 2,
        reset_timeout: float = 60.0,
        half_open_max_calls: int = 1,
    ) -> None: ...

class RateLimitConfig:
    def __init__(self, global_limit: int = 100, global_period: float = 60.0) -> None: ...

class CacheConfig:
    def __init__(
        self, cacheable_tools: Sequence[str], default_ttl: Optional[float] = None
    ) -> None: ...

class Runtime:
    """A built tool-execution runtime over an MCP server."""

    @property
    def tools(self) -> list[str]: ...
    async def call_tool(
        self,
        name: str,
        arguments: Optional[dict[str, Any]] = None,
        timeout: Optional[float] = None,
    ) -> ToolResult: ...
    async def close(self) -> None: ...
    async def __aenter__(self) -> "Runtime": ...
    async def __aexit__(self, exc_type: Any, exc_value: Any, traceback: Any) -> bool: ...

async def connect_stdio(
    command: str,
    args: Optional[Sequence[str]] = None,
    retry: Optional[RetryConfig] = None,
    circuit_breaker: Optional[CircuitBreakerConfig] = None,
    rate_limit: Optional[RateLimitConfig] = None,
    cache: Optional[CacheConfig] = None,
) -> Runtime: ...
