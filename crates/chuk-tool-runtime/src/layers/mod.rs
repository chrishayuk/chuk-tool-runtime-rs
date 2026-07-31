//! Policy layers. Each both implements and wraps a [`crate::invoker::ToolInvoker`],
//! so they compose by nesting. Build order (innermost → outermost) mirrors the
//! Python middleware stack: transport → retry → circuit breaker → rate limiting.

mod retry;

pub use retry::RetryLayer;

// Planned layers (see DESIGN.md):
// mod circuit_breaker;  pub use circuit_breaker::CircuitBreakerLayer;
// mod rate_limit;       pub use rate_limit::RateLimitLayer;
// mod cache;            pub use cache::CacheLayer;
