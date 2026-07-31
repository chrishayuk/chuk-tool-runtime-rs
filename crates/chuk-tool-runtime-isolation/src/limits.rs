//! Resource ceilings for a single isolated execution.
//!
//! Enforced across the stack: the wall-clock timeout by the runner; CPU / memory
//! / process caps by the guest via POSIX `setrlimit` (best-effort defence in
//! depth); the tool-call count by the broker; and captured-output size by the
//! runner. Defaults suit a short orchestration snippet.

use std::time::Duration;

/// Hard wall-clock ceiling.
pub const DEFAULT_WALL_TIMEOUT: Duration = Duration::from_secs(30);
/// CPU-seconds ceiling (guards busy loops that don't trip the wall clock).
pub const DEFAULT_CPU_SECONDS: u64 = 15;
/// Address-space ceiling.
pub const DEFAULT_MEMORY_BYTES: u64 = 256 * 1024 * 1024;
/// Captured stdout/stderr cap.
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 64 * 1024;
/// Broker tool-call cap.
pub const DEFAULT_MAX_TOOL_CALLS: usize = 100;
/// Process/thread cap.
pub const DEFAULT_MAX_PROCESSES: u64 = 64;

/// Resource ceilings for one isolated run.
#[derive(Debug, Clone)]
pub struct IsolationLimits {
    /// Hard wall-clock ceiling; the runner kills the guest when exceeded.
    pub wall_timeout: Duration,
    /// CPU-seconds ceiling (`RLIMIT_CPU`); `None` disables.
    pub cpu_seconds: Option<u64>,
    /// Address-space / memory ceiling in bytes (`RLIMIT_AS`); `None` disables.
    pub memory_bytes: Option<u64>,
    /// Maximum captured stdout/stderr bytes kept; the rest is truncated.
    pub max_output_bytes: usize,
    /// Maximum tool calls the guest may make through the broker.
    pub max_tool_calls: usize,
    /// Maximum processes/threads the guest may spawn (`RLIMIT_NPROC`); `None` disables.
    pub max_processes: Option<u64>,
}

impl Default for IsolationLimits {
    fn default() -> Self {
        Self {
            wall_timeout: DEFAULT_WALL_TIMEOUT,
            cpu_seconds: Some(DEFAULT_CPU_SECONDS),
            memory_bytes: Some(DEFAULT_MEMORY_BYTES),
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            max_tool_calls: DEFAULT_MAX_TOOL_CALLS,
            max_processes: Some(DEFAULT_MAX_PROCESSES),
        }
    }
}

impl IsolationLimits {
    /// Set the wall-clock ceiling. (Builder.)
    pub fn with_wall_timeout(mut self, timeout: Duration) -> Self {
        self.wall_timeout = timeout;
        self
    }
    /// Set the CPU-seconds ceiling. (Builder.)
    pub fn with_cpu_seconds(mut self, seconds: u64) -> Self {
        self.cpu_seconds = Some(seconds);
        self
    }
    /// Set the memory ceiling in bytes. (Builder.)
    pub fn with_memory_bytes(mut self, bytes: u64) -> Self {
        self.memory_bytes = Some(bytes);
        self
    }
    /// Set the maximum tool calls. (Builder.)
    pub fn with_max_tool_calls(mut self, calls: usize) -> Self {
        self.max_tool_calls = calls;
        self
    }

    /// The `{cpu_seconds, memory_bytes, max_processes}` map handed to the guest,
    /// which applies them with `setrlimit`.
    pub(crate) fn guest_json(&self) -> serde_json::Value {
        serde_json::json!({
            "cpu_seconds": self.cpu_seconds,
            "memory_bytes": self.memory_bytes,
            "max_processes": self.max_processes,
        })
    }
}

/// Truncate `text` to at most `max` bytes, at a UTF-8 char boundary.
pub(crate) fn truncate_utf8(mut text: String, max: usize) -> String {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_named_constants() {
        let l = IsolationLimits::default();
        assert_eq!(l.wall_timeout, DEFAULT_WALL_TIMEOUT);
        assert_eq!(l.cpu_seconds, Some(DEFAULT_CPU_SECONDS));
        assert_eq!(l.max_tool_calls, DEFAULT_MAX_TOOL_CALLS);
    }

    #[test]
    fn builders_override_fields() {
        let l = IsolationLimits::default()
            .with_wall_timeout(Duration::from_secs(1))
            .with_cpu_seconds(2)
            .with_memory_bytes(1024)
            .with_max_tool_calls(3);
        assert_eq!(l.wall_timeout, Duration::from_secs(1));
        assert_eq!(l.cpu_seconds, Some(2));
        assert_eq!(l.memory_bytes, Some(1024));
        assert_eq!(l.max_tool_calls, 3);
    }

    #[test]
    fn truncate_respects_byte_cap_and_char_boundaries() {
        assert_eq!(truncate_utf8("hello".to_string(), 10), "hello");
        assert_eq!(truncate_utf8("hello".to_string(), 3), "hel");
        // "é" is 2 bytes; a cap of 1 must not split it.
        assert_eq!(truncate_utf8("é".to_string(), 1), "");
    }
}
