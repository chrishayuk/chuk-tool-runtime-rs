//! Runtime error type (for construction/configuration failures; tool-call
//! failures flow through [`crate::outcome::ToolOutcome`], not this).

/// Errors raised while building or configuring the runtime.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    /// A configuration value was invalid.
    #[error("invalid configuration: {0}")]
    Config(String),
}
