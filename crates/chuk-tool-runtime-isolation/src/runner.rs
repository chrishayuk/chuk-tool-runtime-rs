//! `IsolatedRunner` — run untrusted code in a sandbox with brokered tool access.
//!
//! Ties the pieces together: generate a token + socket, bind the [`Broker`],
//! spawn a guest under a [`SandboxBackend`] (`<sandbox> python guest.py job.json`),
//! and let the guest execute the code and call host tools back through the broker.
//! Everything except the brokered tool channel is denied by the backend.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use chuk_tool_runtime::ToolInvoker;
use serde_json::{json, Value};

use crate::broker::{Broker, BrokerConfig};
use crate::limits::{truncate_utf8, IsolationLimits};
use crate::{LaunchCtx, SandboxBackend};

/// The guest bootstrap, embedded and written into each run's work dir.
const GUEST_PY: &str = include_str!("../resources/guest.py");

/// Outcome of an isolated run.
#[derive(Debug, Clone)]
pub struct IsolatedResult {
    /// The code's return value (via the broker `result`), if it completed.
    pub value: Option<Value>,
    /// How many host tool calls the guest made.
    pub tool_calls: usize,
    /// Captured stdout / stderr of the guest.
    pub stdout: String,
    pub stderr: String,
    /// Whether the run hit the wall-clock timeout.
    pub timed_out: bool,
    /// Whether the guest process exited successfully.
    pub exit_ok: bool,
}

/// Runs code inside a sandbox backend with brokered tool access.
pub struct IsolatedRunner {
    backend: Box<dyn SandboxBackend>,
    invoker: Arc<dyn ToolInvoker>,
    namespace: String,
    allowed_tools: Option<HashSet<String>>,
    python: String,
    allow_no_isolation: bool,
    limits: IsolationLimits,
}

impl IsolatedRunner {
    /// Build a runner over `backend`, brokering tool calls to `invoker`.
    pub fn new(backend: Box<dyn SandboxBackend>, invoker: Arc<dyn ToolInvoker>) -> Self {
        Self {
            backend,
            invoker,
            namespace: "default".to_string(),
            allowed_tools: None,
            python: python_exe(),
            allow_no_isolation: false,
            limits: IsolationLimits::default(),
        }
    }

    /// Set the resource ceilings for the run (default: [`IsolationLimits::default`]).
    pub fn limits(mut self, limits: IsolationLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Pin brokered tools to `namespace` (default: `"default"`).
    pub fn namespace(mut self, namespace: impl Into<String>) -> Self {
        self.namespace = namespace.into();
        self
    }

    /// Restrict the guest to these tool names (also what it's told it can call).
    pub fn allowed_tools(mut self, tools: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.allowed_tools = Some(tools.into_iter().map(Into::into).collect());
        self
    }

    /// Permit a non-isolating backend (e.g. [`crate::LocalBackend`]). Required to
    /// use one — a non-isolating backend must not run untrusted code by accident.
    pub fn allow_no_isolation(mut self, allow: bool) -> Self {
        self.allow_no_isolation = allow;
        self
    }

    /// Whether the backend can run here.
    pub fn is_available(&self) -> bool {
        self.backend.is_available()
    }

    /// Execute `code` in the sandbox, bounded by the configured [`IsolationLimits`].
    pub async fn run(&self, code: &str) -> std::io::Result<IsolatedResult> {
        if !self.backend.provides_isolation() && !self.allow_no_isolation {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "backend '{}' provides no isolation; set allow_no_isolation(true) to use it",
                    self.backend.name()
                ),
            ));
        }

        let workdir = tempfile::tempdir()?;
        // Short socket path (unix sun_path is length-limited); /tmp is in the
        // backends' write roots.
        let socket = PathBuf::from("/tmp").join(format!("ctr-{:016x}.sock", rand::random::<u64>()));
        let token = format!("{:016x}{:016x}", rand::random::<u64>(), rand::random::<u64>());

        let ctx = LaunchCtx {
            workdir: workdir.path().to_path_buf(),
            endpoint: socket.clone(),
        };

        // Files are written on the host; the guest reads them at its own paths
        // (identity for same-fs backends, mounted for containers).
        std::fs::write(workdir.path().join("guest.py"), GUEST_PY)?;
        let tools: Vec<String> = self
            .allowed_tools
            .as_ref()
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default();
        let job = json!({
            "code": code,
            "endpoint": self.backend.endpoint_guest(&ctx).to_string_lossy(),
            "token": token,
            "namespace": self.namespace,
            "tools": tools,
            "limits": self.limits.guest_json(),
        });
        std::fs::write(workdir.path().join("job.json"), serde_json::to_vec(&job)?)?;

        let broker = Broker::bind(
            &socket,
            self.invoker.clone(),
            BrokerConfig {
                token,
                namespace: self.namespace.clone(),
                allowed_tools: self.allowed_tools.clone(),
                max_tool_calls: self.limits.max_tool_calls,
            },
        )
        .await?;

        let guest_dir = self.backend.workdir_guest(&ctx);
        let python = self.backend.python_exe().unwrap_or_else(|| self.python.clone());
        let mut argv = self.backend.wrapper_argv(&ctx);
        argv.push(python);
        argv.push(guest_dir.join("guest.py").to_string_lossy().into_owned());
        argv.push(guest_dir.join("job.json").to_string_lossy().into_owned());

        let (program, rest) = argv.split_first().expect("wrapper argv is non-empty");
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(rest)
            .current_dir(workdir.path())
            .kill_on_drop(true)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in self.backend.extra_env() {
            cmd.env(key, value);
        }
        let child = cmd.spawn()?;

        // Serve the guest and collect its output concurrently.
        let run = async {
            let session = broker.serve_one().await?;
            let output = child.wait_with_output().await?;
            Ok::<_, std::io::Error>((session, output))
        };

        let cap = self.limits.max_output_bytes;
        let result = match tokio::time::timeout(self.limits.wall_timeout, run).await {
            Ok(Ok((session, output))) => IsolatedResult {
                value: session.result,
                tool_calls: session.tool_calls,
                stdout: truncate_utf8(String::from_utf8_lossy(&output.stdout).into_owned(), cap),
                stderr: truncate_utf8(String::from_utf8_lossy(&output.stderr).into_owned(), cap),
                timed_out: false,
                exit_ok: output.status.success(),
            },
            Ok(Err(e)) => {
                let _ = std::fs::remove_file(&socket);
                return Err(e);
            }
            Err(_elapsed) => IsolatedResult {
                value: None,
                tool_calls: 0,
                stdout: String::new(),
                stderr: "guest run timed out".to_string(),
                timed_out: true,
                exit_ok: false,
            },
        };
        let _ = std::fs::remove_file(&socket);
        Ok(result)
    }
}

/// Resolve a `python3` for the guest (absolute path preferred).
fn python_exe() -> String {
    for candidate in ["/usr/bin/python3", "/opt/homebrew/bin/python3", "/usr/local/bin/python3"] {
        if Path::new(candidate).exists() {
            return candidate.to_string();
        }
    }
    "python3".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LocalBackend, SeatbeltBackend};
    use async_trait::async_trait;
    use chuk_tool_runtime::ToolOutcome;
    use std::time::Duration;

    struct HostTools;
    #[async_trait]
    impl ToolInvoker for HostTools {
        async fn call_tool(&self, tool: &str, args: Value, _t: Option<Duration>) -> ToolOutcome {
            ToolOutcome::ok(tool, json!({"echoed": args}))
        }
    }

    fn have_python() -> bool {
        ["/usr/bin/python3", "/opt/homebrew/bin/python3", "/usr/local/bin/python3"]
            .iter()
            .any(|p| Path::new(p).exists())
    }

    const ECHO_CODE: &str = "r = await echo(text=\"hi\")\nreturn r[\"echoed\"][\"text\"]";

    #[tokio::test]
    async fn refuses_non_isolating_backend_without_opt_in() {
        // LocalBackend provides no isolation; without allow_no_isolation the run
        // is rejected before anything is spawned.
        let runner = IsolatedRunner::new(Box::new(LocalBackend), Arc::new(HostTools));
        let err = runner.run("return 1").await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[tokio::test]
    async fn times_out_a_hanging_guest() {
        if !have_python() {
            return;
        }
        let runner = IsolatedRunner::new(Box::new(LocalBackend), Arc::new(HostTools))
            .allow_no_isolation(true)
            .limits(IsolationLimits::default().with_wall_timeout(Duration::from_millis(500)));
        let result = runner
            .run("import asyncio\nawait asyncio.sleep(30)\nreturn 1")
            .await
            .unwrap();
        assert!(result.timed_out);
    }

    #[tokio::test]
    async fn local_backend_runs_the_full_broker_path() {
        if !have_python() {
            return;
        }
        // No OS sandbox — exercises the runner + broker + guest cross-platform.
        let runner = IsolatedRunner::new(Box::new(LocalBackend), Arc::new(HostTools))
            .allow_no_isolation(true)
            .allowed_tools(["echo"]);
        let result = runner.run(ECHO_CODE).await.unwrap();
        assert!(!result.timed_out, "stderr: {}", result.stderr);
        assert!(result.exit_ok, "stderr: {}", result.stderr);
        assert_eq!(result.value, Some(json!("hi")));
        assert_eq!(result.tool_calls, 1);
    }

    #[tokio::test]
    async fn caps_captured_output_to_the_limit() {
        if !have_python() {
            return;
        }
        // The guest prints far more than the cap; the runner keeps only `cap` bytes.
        let cap = 32;
        let runner = IsolatedRunner::new(Box::new(LocalBackend), Arc::new(HostTools))
            .allow_no_isolation(true)
            .limits(IsolationLimits { max_output_bytes: cap, ..IsolationLimits::default() });
        let result = runner.run("print('x' * 10000)\nreturn 1").await.unwrap();
        assert!(result.exit_ok, "stderr: {}", result.stderr);
        assert!(result.stdout.len() <= cap, "stdout was {} bytes", result.stdout.len());
    }

    #[tokio::test]
    async fn a_cpu_bound_guest_is_killed_by_the_cpu_limit() {
        if !have_python() {
            return;
        }
        // A tight busy loop burns CPU without tripping the (generous) wall clock;
        // the guest's RLIMIT_CPU takes it down instead. It sends no result.
        let runner = IsolatedRunner::new(Box::new(LocalBackend), Arc::new(HostTools))
            .allow_no_isolation(true)
            .limits(
                IsolationLimits::default()
                    .with_cpu_seconds(1)
                    .with_wall_timeout(Duration::from_secs(30)),
            );
        let result = runner.run("while True:\n    pass").await.unwrap();
        assert!(!result.timed_out, "should die by CPU limit, not wall clock");
        assert!(!result.exit_ok);
        assert_eq!(result.value, None);
    }

    #[cfg_attr(not(target_os = "macos"), ignore)]
    #[tokio::test]
    async fn end_to_end_sandboxed_code_calls_a_host_tool() {
        let backend = SeatbeltBackend::default();
        if !backend.is_available() || !have_python() {
            return; // skip where we can't actually sandbox or run python
        }
        let runner = IsolatedRunner::new(Box::new(backend), Arc::new(HostTools))
            .namespace("default")
            .allowed_tools(["echo"]);

        // Untrusted code calls the brokered host tool and returns a value.
        let result = runner.run(ECHO_CODE).await.unwrap();

        assert!(!result.timed_out, "stderr: {}", result.stderr);
        assert!(result.exit_ok, "guest failed; stderr: {}", result.stderr);
        assert_eq!(result.value, Some(json!("hi")));
        assert_eq!(result.tool_calls, 1);
    }
}
