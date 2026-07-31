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
use std::time::Duration;

use chuk_tool_runtime::ToolInvoker;
use serde_json::{json, Value};

use crate::broker::{Broker, BrokerConfig};
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
        }
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

    /// Whether the backend can run here.
    pub fn is_available(&self) -> bool {
        self.backend.is_available()
    }

    /// Execute `code` in the sandbox, bounded by `timeout`.
    pub async fn run(&self, code: &str, timeout: Duration) -> std::io::Result<IsolatedResult> {
        let workdir = tempfile::tempdir()?;
        // Short socket path (unix sun_path is length-limited); /tmp is in the
        // Seatbelt write roots.
        let socket = PathBuf::from("/tmp").join(format!("ctr-{:016x}.sock", rand::random::<u64>()));
        let token = format!("{:016x}{:016x}", rand::random::<u64>(), rand::random::<u64>());

        let guest_path = workdir.path().join("guest.py");
        std::fs::write(&guest_path, GUEST_PY)?;

        let tools: Vec<String> = self
            .allowed_tools
            .as_ref()
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default();
        let job = json!({
            "code": code,
            "endpoint": socket.to_string_lossy(),
            "token": token,
            "namespace": self.namespace,
            "tools": tools,
        });
        let job_path = workdir.path().join("job.json");
        std::fs::write(&job_path, serde_json::to_vec(&job)?)?;

        let broker = Broker::bind(
            &socket,
            self.invoker.clone(),
            BrokerConfig {
                token,
                namespace: self.namespace.clone(),
                allowed_tools: self.allowed_tools.clone(),
            },
        )
        .await?;

        let ctx = LaunchCtx {
            workdir: workdir.path().to_path_buf(),
            socket_dir: PathBuf::from("/tmp"),
        };
        let mut argv = self.backend.wrapper_argv(&ctx);
        argv.push(self.python.clone());
        argv.push(guest_path.to_string_lossy().into_owned());
        argv.push(job_path.to_string_lossy().into_owned());

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

        let result = match tokio::time::timeout(timeout, run).await {
            Ok(Ok((session, output))) => IsolatedResult {
                value: session.result,
                tool_calls: session.tool_calls,
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
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
    use crate::SeatbeltBackend;
    use async_trait::async_trait;
    use chuk_tool_runtime::ToolOutcome;

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
        let code = r#"r = await echo(text="hi")
return r["echoed"]["text"]"#;
        let result = runner.run(code, Duration::from_secs(30)).await.unwrap();

        assert!(!result.timed_out, "stderr: {}", result.stderr);
        assert!(result.exit_ok, "guest failed; stderr: {}", result.stderr);
        assert_eq!(result.value, Some(json!("hi")));
        assert_eq!(result.tool_calls, 1);
    }
}
