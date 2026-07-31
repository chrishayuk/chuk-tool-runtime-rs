//! Linux bubblewrap backend — runs the guest in a namespace sandbox via `bwrap`.
//!
//! Gives the guest a read-only view of the system + interpreter, a private
//! `/tmp` tmpfs, fresh `/proc` and `/dev`, no network (unless allowed), and
//! bind-mounts only the broker socket dir for host tool access. Paths are bound
//! at their original locations, so guest-visible paths match host paths (the
//! default identity mapping). Requires `bwrap` on Linux.

use std::path::Path;

use crate::{LaunchCtx, SandboxBackend};

/// System roots the interpreter needs, bind-mounted read-only when present.
const SYSTEM_ROOTS: &[&str] = &["/usr", "/bin", "/sbin", "/lib", "/lib64", "/etc"];

/// Linux namespace-isolated guest via `bwrap`.
#[derive(Debug, Default)]
pub struct BubblewrapBackend {
    allow_network: bool,
}

impl BubblewrapBackend {
    /// Build a backend, optionally leaving the network namespace shared.
    pub fn new(allow_network: bool) -> Self {
        Self { allow_network }
    }
}

impl SandboxBackend for BubblewrapBackend {
    fn name(&self) -> &str {
        "bubblewrap"
    }

    fn provides_isolation(&self) -> bool {
        true
    }

    fn is_available(&self) -> bool {
        cfg!(target_os = "linux") && which("bwrap")
    }

    fn extra_env(&self) -> Vec<(String, String)> {
        vec![("PYTHONDONTWRITEBYTECODE".to_string(), "1".to_string())]
    }

    fn wrapper_argv(&self, ctx: &LaunchCtx) -> Vec<String> {
        let mut argv = str_vec(&[
            "bwrap",
            "--die-with-parent",
            "--new-session",
            "--unshare-user",
            "--unshare-pid",
            "--unshare-ipc",
        ]);
        if !self.allow_network {
            argv.push("--unshare-net".to_string());
        }
        // System + interpreter roots, read-only.
        for root in SYSTEM_ROOTS {
            if Path::new(root).exists() {
                argv.extend(str_vec(&["--ro-bind", root, root]));
            }
        }
        argv.extend(str_vec(&["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp"]));

        // Staging dir read-only; broker socket dir writable so the guest can connect().
        let workdir = ctx.workdir.to_string_lossy().into_owned();
        argv.extend(["--ro-bind".to_string(), workdir.clone(), workdir]);
        let socket_dir = ctx
            .endpoint
            .parent()
            .unwrap_or_else(|| Path::new("/tmp"))
            .to_string_lossy()
            .into_owned();
        argv.extend(["--bind".to_string(), socket_dir.clone(), socket_dir]);
        argv.extend(str_vec(&["--chdir", "/", "--"]));
        argv
    }
}

fn str_vec(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

/// Whether `bin` is on `PATH`.
fn which(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(bin).exists()))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn argv(allow_network: bool) -> Vec<String> {
        let ctx = LaunchCtx {
            workdir: PathBuf::from("/work"),
            endpoint: PathBuf::from("/sock/broker.sock"),
        };
        BubblewrapBackend::new(allow_network).wrapper_argv(&ctx)
    }

    fn window(argv: &[String], a: &str, b: &str) -> bool {
        argv.windows(2).any(|w| w[0] == a && w[1] == b)
    }

    #[test]
    fn no_network_by_default_and_mounts_workdir_ro() {
        let a = argv(false);
        assert_eq!(a[0], "bwrap");
        assert!(a.contains(&"--unshare-net".to_string()));
        assert!(window(&a, "--tmpfs", "/tmp"));
        assert!(window(&a, "--ro-bind", "/work")); // staging dir read-only
        assert!(window(&a, "--bind", "/sock")); // socket dir writable
        assert_eq!(a.last().unwrap(), "--");
    }

    #[test]
    fn allow_network_drops_unshare_net() {
        assert!(!argv(true).contains(&"--unshare-net".to_string()));
    }

    #[test]
    fn identity_and_metadata() {
        let b = BubblewrapBackend::default();
        assert_eq!(b.name(), "bubblewrap");
        assert!(b.provides_isolation());
        assert_eq!(b.extra_env(), vec![("PYTHONDONTWRITEBYTECODE".to_string(), "1".to_string())]);
        // not available off-Linux (this test host is macOS)
        assert_eq!(b.is_available(), cfg!(target_os = "linux") && which("bwrap"));
    }
}
