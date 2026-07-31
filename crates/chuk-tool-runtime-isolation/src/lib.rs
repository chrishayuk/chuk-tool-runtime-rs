//! OS-level sandbox backends for running untrusted guest processes.
//!
//! A [`SandboxBackend`] describes *how* to launch a command under isolation; a
//! [`LaunchCtx`] carries the per-run paths (the guest's work dir and the broker
//! socket dir). [`run_sandboxed`] launches a command wrapped by a backend and
//! captures its output.
//!
//! The first backend is [`SeatbeltBackend`] (macOS `sandbox-exec`). It generates
//! an SBPL profile that denies everything by default and then grants only what a
//! short guest run needs — **no outbound network** (except the unix broker
//! socket) and **no filesystem writes** outside the work/socket/tmp dirs. Read
//! confinement is a best-effort denylist of well-known secret directories
//! (a strict read allowlist breaks the interpreter/dyld), mirroring
//! `chuk-tool-processor`'s Seatbelt backend.

use std::path::{Path, PathBuf};
use std::process::Output;

mod broker;
mod bubblewrap;
mod limits;
mod local;
mod runner;
mod seatbelt;
pub mod wire;

pub use broker::{Broker, BrokerConfig, BrokerSession};
pub use bubblewrap::BubblewrapBackend;
pub use limits::{
    IsolationLimits, DEFAULT_CPU_SECONDS, DEFAULT_MAX_OUTPUT_BYTES, DEFAULT_MAX_PROCESSES,
    DEFAULT_MAX_TOOL_CALLS, DEFAULT_MEMORY_BYTES, DEFAULT_WALL_TIMEOUT,
};
pub use local::LocalBackend;
pub use runner::{IsolatedResult, IsolatedRunner};
pub use seatbelt::{SeatbeltBackend, DEFAULT_DENY_READ_PATHS};

/// Per-run paths handed to a backend when it builds its launch wrapper.
#[derive(Debug, Clone)]
pub struct LaunchCtx {
    /// The guest's writable staging directory (holds `guest.py` + `job.json`).
    pub workdir: PathBuf,
    /// The broker's unix socket path — the only network the guest gets.
    pub endpoint: PathBuf,
}

/// How to launch a guest command under OS isolation.
///
/// The identity mapping (guest paths == host paths, host python) is the default;
/// container backends override the `*_guest` methods and [`python_exe`].
pub trait SandboxBackend: Send + Sync {
    /// Short identifier, e.g. `"seatbelt"`.
    fn name(&self) -> &str;

    /// Whether this backend actually confines the guest (vs. a no-op local run).
    fn provides_isolation(&self) -> bool;

    /// Whether this backend can run on the current host.
    fn is_available(&self) -> bool;

    /// The wrapper argv prepended to the guest command (e.g.
    /// `["sandbox-exec", "-p", <profile>]`). Empty for a no-op backend.
    fn wrapper_argv(&self, ctx: &LaunchCtx) -> Vec<String>;

    /// Where the guest sees the work dir (default: identity — same as host).
    fn workdir_guest(&self, ctx: &LaunchCtx) -> PathBuf {
        ctx.workdir.clone()
    }

    /// Where the guest reaches the broker socket (default: identity).
    fn endpoint_guest(&self, ctx: &LaunchCtx) -> PathBuf {
        ctx.endpoint.clone()
    }

    /// Python executable for the guest (`None` = the runner's host python; a
    /// container backend returns its in-image interpreter).
    fn python_exe(&self) -> Option<String> {
        None
    }

    /// Extra environment variables to set for the guest.
    fn extra_env(&self) -> Vec<(String, String)> {
        Vec::new()
    }
}

/// Launch `command` (argv) under `backend` in `ctx`, capturing its output.
///
/// The final argv is `backend.wrapper_argv(ctx)` followed by `command`, run with
/// the working directory set to `ctx.workdir` and the backend's extra env applied.
pub async fn run_sandboxed(
    backend: &dyn SandboxBackend,
    ctx: &LaunchCtx,
    command: &[String],
) -> std::io::Result<Output> {
    let mut argv = backend.wrapper_argv(ctx);
    argv.extend_from_slice(command);

    let (program, rest) = argv
        .split_first()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty command"))?;

    let mut cmd = tokio::process::Command::new(program);
    cmd.args(rest).current_dir(&ctx.workdir);
    for (key, value) in backend.extra_env() {
        cmd.env(key, value);
    }
    cmd.output().await
}

/// Expand a leading `~` to `$HOME`; leave other paths unchanged.
pub(crate) fn expand_home(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}{rest}");
        }
    }
    path.to_string()
}

/// Absolute, symlink-resolved form of an existing path (falls back to the input).
pub(crate) fn real(path: &Path) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_home_handles_tilde_and_absolute() {
        if let Ok(home) = std::env::var("HOME") {
            assert_eq!(expand_home("~/a/b"), format!("{home}/a/b"));
        }
        assert_eq!(expand_home("/abs/path"), "/abs/path");
        assert_eq!(expand_home("relative"), "relative");
    }

    #[test]
    fn real_falls_back_when_path_missing() {
        assert_eq!(real(Path::new("/no/such/path/xyz")), "/no/such/path/xyz");
    }

    #[tokio::test]
    async fn run_sandboxed_rejects_an_empty_command() {
        let ctx = LaunchCtx {
            workdir: std::env::temp_dir(),
            endpoint: std::env::temp_dir().join("s.sock"),
        };
        let err = run_sandboxed(&crate::LocalBackend, &ctx, &[]).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }
}
