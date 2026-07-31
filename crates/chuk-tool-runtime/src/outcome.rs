//! The result currency that flows through every policy layer.

use serde_json::Value;

/// Outcome of a tool invocation.
///
/// Layers inspect and pass these through. A transport/infra failure is modelled
/// as `success = false` with an `error` message (mirroring the Python runtime,
/// where the innermost transport returns `{isError, error}` rather than raising),
/// so a single type covers both tool-reported and transport-level failures.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutcome {
    /// Tool name this outcome is for.
    pub tool: String,
    /// Whether the call ultimately succeeded.
    pub success: bool,
    /// Result payload on success.
    pub result: Option<Value>,
    /// Error message on failure.
    pub error: Option<String>,
    /// Number of invocation attempts made (>= 1); set by the retry layer.
    pub attempts: u32,
    /// Whether the result was served from a cache layer.
    pub from_cache: bool,
}

impl ToolOutcome {
    /// A successful outcome carrying `result`.
    pub fn ok(tool: impl Into<String>, result: Value) -> Self {
        Self {
            tool: tool.into(),
            success: true,
            result: Some(result),
            error: None,
            attempts: 1,
            from_cache: false,
        }
    }

    /// A failed outcome carrying an error message.
    pub fn err(tool: impl Into<String>, error: impl Into<String>) -> Self {
        Self {
            tool: tool.into(),
            success: false,
            result: None,
            error: Some(error.into()),
            attempts: 1,
            from_cache: false,
        }
    }

    /// `true` when the call did not succeed.
    pub fn is_error(&self) -> bool {
        !self.success
    }
}
