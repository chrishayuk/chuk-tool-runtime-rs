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
mod seatbelt;
pub mod wire;

pub use broker::{Broker, BrokerConfig, BrokerSession};
pub use seatbelt::{SeatbeltBackend, DEFAULT_DENY_READ_PATHS};

/// Per-run paths handed to a backend when it builds its launch wrapper.
#[derive(Debug, Clone)]
pub struct LaunchCtx {
    /// The guest's writable staging directory.
    pub workdir: PathBuf,
    /// Directory holding the broker's unix socket (the only network the guest gets).
    pub socket_dir: PathBuf,
}

/// How to launch a command under OS isolation.
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
