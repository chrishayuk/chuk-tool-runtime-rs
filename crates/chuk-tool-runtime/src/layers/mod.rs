//! Policy layers. Each both implements and wraps a [`crate::invoker::ToolInvoker`],
//! so they compose by nesting. Build order (innermost → outermost) mirrors the
//! Python middleware stack: transport → retry → circuit breaker → rate limiting.

mod circuit_breaker;
mod retry;

pub use circuit_breaker::{CircuitBreakerLayer, CircuitSnapshot, CircuitState, CIRCUIT_OPEN_PREFIX};
pub use retry::RetryLayer;

// Planned layers (see DESIGN.md):
// mod rate_limit;  pub use rate_limit::RateLimitLayer;
// mod cache;       pub use cache::CacheLayer;
