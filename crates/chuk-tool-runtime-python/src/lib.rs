//! PyO3 bindings for `chuk-tool-runtime`.
//!
//! Exposes the Rust tool-execution runtime to Python as `chuk_tool_runtime_rs`:
//! connect to a stdio MCP server, wrap it in the policy layers (retry, circuit
//! breaker, rate limiting, cache) over first-wins routing, and
//! `await runtime.call_tool(...)`. The transport and all policy stay in Rust;
//! only the typed result crosses to Python.

use std::sync::Arc;
use std::time::Duration;

use pyo3::prelude::*;
use pyo3_async_runtimes::tokio::future_into_py;
use pythonize::{depythonize, pythonize};
use serde_json::Value;

use chuk_tool_runtime::{
    CacheConfig, CircuitBreakerConfig, RateLimitConfig, RetryConfig, Router, RuntimeBuilder,
    ToolInvoker, ToolOutcome,
};

fn py_err(msg: impl Into<String>) -> PyErr {
    pyo3::exceptions::PyRuntimeError::new_err(msg.into())
}

fn py_to_json(obj: &Bound<'_, PyAny>) -> PyResult<Value> {
    Ok(depythonize(obj)?)
}

fn json_to_py(py: Python<'_>, value: &Value) -> PyResult<Py<PyAny>> {
    Ok(pythonize(py, value)?.unbind())
}

// --------------------------------------------------------------------------- #
//  Result
// --------------------------------------------------------------------------- #

/// The typed result of a tool call.
#[pyclass(name = "ToolResult")]
struct PyToolResult {
    /// The tool that was called.
    #[pyo3(get)]
    tool: String,
    /// Whether the call ultimately succeeded.
    #[pyo3(get)]
    success: bool,
    result: Option<Value>,
    /// Error message when `success` is `False`.
    #[pyo3(get)]
    error: Option<String>,
    /// Number of invocation attempts made.
    #[pyo3(get)]
    attempts: u32,
    /// Whether the result was served from the cache.
    #[pyo3(get)]
    from_cache: bool,
}

impl PyToolResult {
    fn from_outcome(outcome: ToolOutcome) -> Self {
        Self {
            tool: outcome.tool,
            success: outcome.success,
            result: outcome.result,
            error: outcome.error,
            attempts: outcome.attempts,
            from_cache: outcome.from_cache,
        }
    }
}

#[pymethods]
impl PyToolResult {
    /// The tool's result payload (or `None`).
    #[getter]
    fn result(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        match &self.result {
            Some(value) => json_to_py(py, value),
            None => Ok(py.None()),
        }
    }

    fn __repr__(&self) -> String {
        let pybool = |b: bool| if b { "True" } else { "False" };
        format!(
            "ToolResult(tool={:?}, success={}, from_cache={}, attempts={})",
            self.tool,
            pybool(self.success),
            pybool(self.from_cache),
            self.attempts
        )
    }
}

// --------------------------------------------------------------------------- #
//  Config classes
// --------------------------------------------------------------------------- #

/// Retry-with-backoff policy.
#[pyclass(name = "RetryConfig", from_py_object)]
#[derive(Clone)]
struct PyRetryConfig {
    inner: RetryConfig,
}

#[pymethods]
impl PyRetryConfig {
    #[new]
    #[pyo3(signature = (max_retries=3, base_delay=1.0, max_delay=30.0, jitter=true))]
    fn new(max_retries: u32, base_delay: f64, max_delay: f64, jitter: bool) -> Self {
        Self {
            inner: RetryConfig {
                max_retries,
                base_delay: Duration::from_secs_f64(base_delay),
                max_delay: Duration::from_secs_f64(max_delay),
                jitter,
                ..RetryConfig::default()
            },
        }
    }
}

/// Per-tool circuit-breaker policy.
#[pyclass(name = "CircuitBreakerConfig", from_py_object)]
#[derive(Clone)]
struct PyCircuitBreakerConfig {
    inner: CircuitBreakerConfig,
}

#[pymethods]
impl PyCircuitBreakerConfig {
    #[new]
    #[pyo3(signature = (failure_threshold=5, success_threshold=2, reset_timeout=60.0, half_open_max_calls=1))]
    fn new(
        failure_threshold: u32,
        success_threshold: u32,
        reset_timeout: f64,
        half_open_max_calls: u32,
    ) -> Self {
        Self {
            inner: CircuitBreakerConfig {
                failure_threshold,
                success_threshold,
                reset_timeout: Duration::from_secs_f64(reset_timeout),
                half_open_max_calls,
                enabled: true,
            },
        }
    }
}

/// Global sliding-window rate limiting.
#[pyclass(name = "RateLimitConfig", from_py_object)]
#[derive(Clone)]
struct PyRateLimitConfig {
    inner: RateLimitConfig,
}

#[pymethods]
impl PyRateLimitConfig {
    #[new]
    #[pyo3(signature = (global_limit=100, global_period=60.0))]
    fn new(global_limit: u32, global_period: f64) -> Self {
        Self {
            inner: RateLimitConfig {
                enabled: true,
                global_limit: Some(global_limit),
                global_period: Duration::from_secs_f64(global_period),
                ..RateLimitConfig::default()
            },
        }
    }
}

/// Opt-in result caching.
#[pyclass(name = "CacheConfig", from_py_object)]
#[derive(Clone)]
struct PyCacheConfig {
    inner: CacheConfig,
}

#[pymethods]
impl PyCacheConfig {
    #[new]
    #[pyo3(signature = (cacheable_tools, default_ttl=None))]
    fn new(cacheable_tools: Vec<String>, default_ttl: Option<f64>) -> Self {
        Self {
            inner: CacheConfig {
                cacheable_tools: cacheable_tools.into_iter().collect(),
                default_ttl: default_ttl.map(Duration::from_secs_f64),
                ..CacheConfig::default()
            },
        }
    }
}

// --------------------------------------------------------------------------- #
//  Runtime
// --------------------------------------------------------------------------- #

/// A built tool-execution runtime over an MCP server.
///
/// Supports `async with` for deterministic shutdown; `close()` (or leaving the
/// context) drops the connection, which signals the server to exit.
#[pyclass]
struct Runtime {
    inner: Option<Arc<dyn ToolInvoker>>,
    tool_names: Vec<String>,
}

impl Runtime {
    fn invoker(&self) -> PyResult<Arc<dyn ToolInvoker>> {
        self.inner
            .clone()
            .ok_or_else(|| py_err("runtime is closed"))
    }
}

#[pymethods]
impl Runtime {
    /// The names of the tools available through this runtime.
    #[getter]
    fn tools(&self) -> Vec<String> {
        self.tool_names.clone()
    }

    /// Call a tool through the policy stack, returning a [`ToolResult`].
    #[pyo3(signature = (name, arguments=None, timeout=None))]
    fn call_tool<'py>(
        &self,
        py: Python<'py>,
        name: String,
        arguments: Option<Bound<'py, PyAny>>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.invoker()?;
        let args = match arguments {
            Some(obj) => py_to_json(&obj)?,
            None => Value::Null,
        };
        let timeout = timeout.map(Duration::from_secs_f64);
        future_into_py(py, async move {
            let outcome = inner.call_tool(&name, args, timeout).await;
            Ok(PyToolResult::from_outcome(outcome))
        })
    }

    /// Shut the runtime down, dropping the connection to the server.
    fn close<'py>(&mut self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        self.inner = None;
        future_into_py(py, async move { Ok(()) })
    }

    fn __aenter__<'py>(slf: Py<Self>, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        future_into_py(py, async move { Ok(slf) })
    }

    #[pyo3(signature = (_exc_type=None, _exc_value=None, _traceback=None))]
    fn __aexit__<'py>(
        &mut self,
        py: Python<'py>,
        _exc_type: Option<Bound<'py, PyAny>>,
        _exc_value: Option<Bound<'py, PyAny>>,
        _traceback: Option<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.inner = None;
        future_into_py(py, async move { Ok(false) }) // don't suppress exceptions
    }
}

/// Connect to a stdio MCP server (era-detecting), then build a runtime that
/// wraps it with the given policy layers over first-wins routing.
#[pyfunction]
#[pyo3(signature = (command, args=None, retry=None, circuit_breaker=None, rate_limit=None, cache=None))]
fn connect_stdio<'py>(
    py: Python<'py>,
    command: String,
    args: Option<Vec<String>>,
    retry: Option<PyRetryConfig>,
    circuit_breaker: Option<PyCircuitBreakerConfig>,
    rate_limit: Option<PyRateLimitConfig>,
    cache: Option<PyCacheConfig>,
) -> PyResult<Bound<'py, PyAny>> {
    let args = args.unwrap_or_default();
    let retry = retry.map(|c| c.inner);
    let circuit_breaker = circuit_breaker.map(|c| c.inner);
    let rate_limit = rate_limit.map(|c| c.inner);
    let cache = cache.map(|c| c.inner);

    future_into_py(py, async move {
        let invoker = chuk_tool_runtime_mcp::connect_stdio(command, args)
            .await
            .map_err(|e| py_err(format!("connect failed: {e}")))?;
        let names = invoker
            .tool_names()
            .await
            .map_err(|e| py_err(format!("tool discovery failed: {e}")))?;

        let mut router = Router::new();
        router.register_tools("default", &names);
        router.add_server("default", Box::new(invoker));

        let mut builder = RuntimeBuilder::new();
        if let Some(cfg) = retry {
            builder = builder.with_retry(cfg);
        }
        if let Some(cfg) = circuit_breaker {
            builder = builder.with_circuit_breaker(cfg);
        }
        if let Some(cfg) = rate_limit {
            builder = builder.with_rate_limit(cfg);
        }
        if let Some(cfg) = cache {
            builder = builder.with_cache(cfg);
        }

        let stack: Arc<dyn ToolInvoker> = Arc::new(builder.build(router));
        Ok(Runtime {
            inner: Some(stack),
            tool_names: names,
        })
    })
}

#[pymodule]
fn _native(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyToolResult>()?;
    m.add_class::<PyRetryConfig>()?;
    m.add_class::<PyCircuitBreakerConfig>()?;
    m.add_class::<PyRateLimitConfig>()?;
    m.add_class::<PyCacheConfig>()?;
    m.add_class::<Runtime>()?;
    m.add_function(wrap_pyfunction!(connect_stdio, m)?)?;
    Ok(())
}
