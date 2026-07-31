//! macOS Seatbelt backend — runs the guest under `sandbox-exec`.

use std::path::Path;

use crate::{expand_home, real, LaunchCtx, SandboxBackend};

/// Well-known secret locations denied to the guest by default (hardening).
pub const DEFAULT_DENY_READ_PATHS: &[&str] = &[
    "~/.ssh",
    "~/.aws",
    "~/.config/gcloud",
    "~/.kube",
    "~/.gnupg",
    "~/.docker",
    "~/.netrc",
    "~/.git-credentials",
    "~/Library/Keychains",
    "~/Library/Application Support/com.apple.TCC",
];

/// Quote a path for an SBPL string literal.
fn q(path: &str) -> String {
    path.replace('\\', "\\\\").replace('"', "\\\"")
}

/// OS-sandboxed guest via macOS `sandbox-exec`.
pub struct SeatbeltBackend {
    deny_read_paths: Vec<String>,
}

impl Default for SeatbeltBackend {
    fn default() -> Self {
        Self::new(DEFAULT_DENY_READ_PATHS.iter().map(|p| p.to_string()), std::iter::empty())
    }
}

impl SeatbeltBackend {
    /// Build with an explicit deny-read base set plus extra denied paths.
    /// `~` is expanded to `$HOME`.
    pub fn new(
        deny_read_paths: impl IntoIterator<Item = String>,
        add_deny_read_paths: impl IntoIterator<Item = String>,
    ) -> Self {
        let deny_read_paths = deny_read_paths
            .into_iter()
            .chain(add_deny_read_paths)
            .map(|p| expand_home(&p))
            .collect();
        Self { deny_read_paths }
    }

    /// The generated SBPL profile for a run.
    fn profile(&self, ctx: &LaunchCtx) -> String {
        let write_roots = [
            real(&ctx.workdir),
            real(&ctx.socket_dir),
            real(Path::new("/private/tmp")),
            real(Path::new("/tmp")),
        ];

        let mut lines = vec![
            "(version 1)".to_string(),
            "(deny default)".to_string(),
            "(allow process-fork)".to_string(),
            "(allow process-exec*)".to_string(),
            "(allow sysctl-read)".to_string(),
            "(allow mach-lookup)".to_string(),
            "(allow signal (target self))".to_string(),
            // Reads: broad (dyld/CPython abort if a needed lib read is denied)...
            "(allow file-read*)".to_string(),
        ];
        // ...but deny the configured secret paths.
        for p in &self.deny_read_paths {
            lines.push(format!("(deny file-read* (subpath \"{}\"))", q(p)));
        }
        // Writes: only the staging dir, socket dir, tmp, and /dev/null.
        for root in &write_roots {
            lines.push(format!("(allow file-write* (subpath \"{}\"))", q(root)));
        }
        lines.push("(allow file-write* (literal \"/dev/null\"))".to_string());
        // Network: only the unix broker socket. Inet stays denied by default.
        lines.push("(allow network-outbound (remote unix-socket))".to_string());

        let mut profile = lines.join("\n");
        profile.push('\n');
        profile
    }
}

impl SandboxBackend for SeatbeltBackend {
    fn name(&self) -> &str {
        "seatbelt"
    }

    fn provides_isolation(&self) -> bool {
        true
    }

    fn is_available(&self) -> bool {
        cfg!(target_os = "macos") && Path::new("/usr/bin/sandbox-exec").exists()
    }

    fn wrapper_argv(&self, ctx: &LaunchCtx) -> Vec<String> {
        vec![
            "sandbox-exec".to_string(),
            "-p".to_string(),
            self.profile(ctx),
        ]
    }

    fn extra_env(&self) -> Vec<(String, String)> {
        // Avoid .pyc writes next to (read-only) stdlib modules.
        vec![("PYTHONDONTWRITEBYTECODE".to_string(), "1".to_string())]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run_sandboxed;

    fn ctx_in(dir: &Path) -> LaunchCtx {
        LaunchCtx {
            workdir: dir.to_path_buf(),
            socket_dir: dir.to_path_buf(),
        }
    }

    fn sh(script: String) -> Vec<String> {
        vec!["/bin/sh".to_string(), "-c".to_string(), script]
    }

    #[test]
    fn profile_denies_default_and_allows_broker_socket() {
        let dir = std::env::temp_dir();
        let profile = SeatbeltBackend::default().profile(&ctx_in(&dir));
        assert!(profile.contains("(deny default)"));
        assert!(profile.contains("(allow network-outbound (remote unix-socket))"));
        assert!(profile.contains(".ssh")); // a default secret path is denied
    }

    #[cfg_attr(not(target_os = "macos"), ignore)]
    #[tokio::test]
    async fn runs_and_allows_workdir_writes() {
        let backend = SeatbeltBackend::default();
        if !backend.is_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(dir.path());
        let target = dir.path().join("out.txt");
        let out = run_sandboxed(
            &backend,
            &ctx,
            &sh(format!("echo hello > '{}'", target.display())),
        )
        .await
        .unwrap();
        assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(std::fs::read_to_string(&target).unwrap().trim(), "hello");
    }

    #[cfg_attr(not(target_os = "macos"), ignore)]
    #[tokio::test]
    async fn denies_writes_outside_the_sandbox() {
        let backend = SeatbeltBackend::default();
        if !backend.is_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(dir.path());
        let home = std::env::var("HOME").unwrap();
        let probe = format!("{home}/.chuk_sbx_probe_test");
        let _ = std::fs::remove_file(&probe);

        let out = run_sandboxed(&backend, &ctx, &sh(format!("echo x > '{probe}'")))
            .await
            .unwrap();

        let exists = Path::new(&probe).exists();
        let _ = std::fs::remove_file(&probe);
        assert!(!out.status.success(), "write to $HOME must be denied by the sandbox");
        assert!(!exists, "no file should have been created outside the sandbox");
    }
}
