//! Local backend — runs the guest directly, **with no isolation**.
//!
//! For development and testing only (and cross-platform runner tests where no OS
//! sandbox is available). The [`crate::IsolatedRunner`] refuses it unless
//! `allow_no_isolation` is set, so it can't be used to run untrusted code by
//! accident.

use crate::{LaunchCtx, SandboxBackend};

/// A no-op backend: the guest runs as an ordinary child process.
#[derive(Debug, Default)]
pub struct LocalBackend;

impl SandboxBackend for LocalBackend {
    fn name(&self) -> &str {
        "local"
    }

    fn provides_isolation(&self) -> bool {
        false
    }

    fn is_available(&self) -> bool {
        true
    }

    fn wrapper_argv(&self, _ctx: &LaunchCtx) -> Vec<String> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn is_a_non_isolating_passthrough() {
        let b = LocalBackend;
        assert_eq!(b.name(), "local");
        assert!(!b.provides_isolation());
        assert!(b.is_available());
        let ctx = LaunchCtx {
            workdir: PathBuf::from("/w"),
            endpoint: PathBuf::from("/w/s.sock"),
        };
        assert!(b.wrapper_argv(&ctx).is_empty());
        // Identity mapping by default.
        assert_eq!(b.workdir_guest(&ctx), ctx.workdir);
        assert_eq!(b.endpoint_guest(&ctx), ctx.endpoint);
        assert!(b.python_exe().is_none());
    }
}
