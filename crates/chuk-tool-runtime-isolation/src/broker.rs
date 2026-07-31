//! Host-side tool broker.
//!
//! The sandboxed guest connects over a unix socket and speaks the [`wire`]
//! protocol: `hello` (token auth), `list_tools`, `call_tool` (run a tool **on
//! the host** and return JSON), and `result` (the run's final value). The broker
//! enforces the token, a host-authoritative namespace, and an optional tool
//! allowlist — the guest is untrusted, so *everything* privileged is decided
//! here, not by the guest.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chuk_tool_runtime::ToolInvoker;
use serde_json::{json, Value};
use tokio::net::UnixListener;

use crate::wire;

/// Broker policy: who the guest is and what it may call.
#[derive(Debug, Clone)]
pub struct BrokerConfig {
    /// Shared secret the guest must present in `hello`.
    pub token: String,
    /// The host-authoritative namespace. A guest that supplies a different one is
    /// rejected.
    pub namespace: String,
    /// Tools the guest may call. `None` allows all; entries may be bare (`"name"`)
    /// or namespace-qualified (`"ns.name"`).
    pub allowed_tools: Option<HashSet<String>>,
    /// Maximum successful tool calls the guest may make before the broker refuses
    /// further ones.
    pub max_tool_calls: usize,
}

impl BrokerConfig {
    fn is_allowed(&self, name: &str) -> bool {
        match &self.allowed_tools {
            None => true,
            Some(set) => {
                set.contains(name) || set.contains(&format!("{}.{}", self.namespace, name))
            }
        }
    }
}

/// What a single guest session produced.
#[derive(Debug, Default)]
pub struct BrokerSession {
    /// The run's final value (`result` message), if the guest sent one.
    pub result: Option<Value>,
    /// How many tool calls the guest made.
    pub tool_calls: usize,
    /// Whether the guest passed the `hello` handshake.
    pub authorized: bool,
}

/// A bound broker, ready to serve one guest connection.
pub struct Broker {
    listener: UnixListener,
    path: PathBuf,
    invoker: Arc<dyn ToolInvoker>,
    config: BrokerConfig,
}

impl Broker {
    /// Bind the broker's unix socket at `path`.
    pub async fn bind(
        path: impl Into<PathBuf>,
        invoker: Arc<dyn ToolInvoker>,
        config: BrokerConfig,
    ) -> std::io::Result<Self> {
        let path = path.into();
        let listener = UnixListener::bind(&path)?;
        Ok(Self {
            listener,
            path,
            invoker,
            config,
        })
    }

    /// The socket path the guest connects to.
    pub fn endpoint(&self) -> &Path {
        &self.path
    }

    /// Accept one guest connection and drive it until it disconnects or sends its
    /// `result`. Returns what the session produced.
    pub async fn serve_one(&self) -> std::io::Result<BrokerSession> {
        let (stream, _) = self.listener.accept().await?;
        let (mut reader, mut writer) = stream.into_split();
        let mut session = BrokerSession::default();

        // Handshake.
        let hello = wire::recv(&mut reader).await?;
        let id = hello.get(wire::KEY_ID).cloned().unwrap_or(Value::Null);
        let ok_method = hello.get(wire::KEY_METHOD).and_then(Value::as_str) == Some(wire::METHOD_HELLO);
        let ok_token = hello.get(wire::KEY_TOKEN).and_then(Value::as_str) == Some(self.config.token.as_str());
        if !(ok_method && ok_token) {
            let _ = wire::send(&mut writer, &err(&id, "unauthorized")).await;
            return Ok(session);
        }
        session.authorized = true;
        wire::send(&mut writer, &json!({wire::KEY_ID: id, wire::KEY_OK: true})).await?;

        // Request loop (sequential dispatch).
        loop {
            let msg = match wire::recv(&mut reader).await {
                Ok(msg) => msg,
                Err(_) => break, // guest disconnected
            };
            let id = msg.get(wire::KEY_ID).cloned().unwrap_or(Value::Null);
            let method = msg.get(wire::KEY_METHOD).and_then(Value::as_str).unwrap_or("");

            match method {
                wire::METHOD_LIST_TOOLS => {
                    wire::send(&mut writer, &ok(&id, json!(self.list_tools()))).await?;
                }
                wire::METHOD_CALL_TOOL => {
                    // No privileged work once the run has produced its result.
                    if session.result.is_some() {
                        wire::send(&mut writer, &err(&id, "run already completed")).await?;
                        continue;
                    }
                    // Enforce the host-set tool-call ceiling.
                    if session.tool_calls >= self.config.max_tool_calls {
                        wire::send(&mut writer, &err(&id, "tool call limit exceeded")).await?;
                        continue;
                    }
                    let params = msg.get(wire::KEY_PARAMS).cloned().unwrap_or_else(|| json!({}));
                    match self.call_tool(&params).await {
                        Ok(value) => {
                            session.tool_calls += 1;
                            wire::send(&mut writer, &ok(&id, value)).await?;
                        }
                        Err(e) => wire::send(&mut writer, &err(&id, &e)).await?,
                    }
                }
                wire::METHOD_RESULT => {
                    if session.result.is_none() {
                        session.result = Some(
                            msg.get(wire::KEY_PARAMS)
                                .and_then(|p| p.get(wire::KEY_VALUE))
                                .cloned()
                                .unwrap_or(Value::Null),
                        );
                    }
                    wire::send(&mut writer, &json!({wire::KEY_ID: id, wire::KEY_OK: true})).await?;
                }
                other => {
                    wire::send(&mut writer, &err(&id, &format!("unknown method: {other}"))).await?;
                }
            }
        }
        Ok(session)
    }

    /// Names the guest is told it may call (the allowlist, or empty when the host
    /// pins none — the runtime can't enumerate an open set).
    fn list_tools(&self) -> Vec<String> {
        match &self.config.allowed_tools {
            Some(set) => set.iter().cloned().collect(),
            None => Vec::new(),
        }
    }

    async fn call_tool(&self, params: &Value) -> Result<Value, String> {
        let name = params
            .get(wire::KEY_NAME)
            .and_then(Value::as_str)
            .ok_or("missing tool name")?;

        // The host namespace is authoritative: reject a mismatching guest one.
        if let Some(ns) = params.get(wire::KEY_NAMESPACE).and_then(Value::as_str) {
            if ns != self.config.namespace {
                return Err(format!("namespace not permitted: {ns}"));
            }
        }
        if !self.config.is_allowed(name) {
            return Err(format!("tool not permitted: {name}"));
        }

        let args = params.get(wire::KEY_ARGUMENTS).cloned().unwrap_or_else(|| json!({}));
        let outcome = self.invoker.call_tool(name, args, None).await;
        if outcome.success {
            Ok(outcome.result.unwrap_or(Value::Null))
        } else {
            Err(outcome.error.unwrap_or_else(|| "tool error".to_string()))
        }
    }
}

fn ok(id: &Value, value: Value) -> Value {
    json!({wire::KEY_ID: id, wire::KEY_OK: true, wire::KEY_VALUE: value})
}

fn err(id: &Value, error: &str) -> Value {
    json!({wire::KEY_ID: id, wire::KEY_OK: false, wire::KEY_ERROR: error})
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use chuk_tool_runtime::ToolOutcome;
    use std::time::Duration;
    use tokio::net::UnixStream;

    /// Host tool executor for tests: echoes args back for "echo", fails "boom".
    struct HostTools;
    #[async_trait]
    impl ToolInvoker for HostTools {
        async fn call_tool(&self, tool: &str, args: Value, _t: Option<Duration>) -> ToolOutcome {
            match tool {
                "boom" => ToolOutcome::err(tool, "kaboom"),
                _ => ToolOutcome::ok(tool, json!({"echoed": args})),
            }
        }
    }

    fn cfg(allowed: Option<&[&str]>) -> BrokerConfig {
        cfg_with_cap(allowed, usize::MAX)
    }

    fn cfg_with_cap(allowed: Option<&[&str]>, max_tool_calls: usize) -> BrokerConfig {
        BrokerConfig {
            token: "secret".into(),
            namespace: "default".into(),
            allowed_tools: allowed.map(|a| a.iter().map(|s| s.to_string()).collect()),
            max_tool_calls,
        }
    }

    async fn bind(cfg: BrokerConfig) -> (Broker, PathBuf, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("broker.sock");
        let broker = Broker::bind(path.clone(), Arc::new(HostTools), cfg).await.unwrap();
        (broker, path, dir)
    }

    #[tokio::test]
    async fn happy_path_call_and_result() {
        let (broker, path, _dir) = bind(cfg(Some(&["echo"]))).await;
        let server = tokio::spawn(async move { broker.serve_one().await.unwrap() });

        let mut c = UnixStream::connect(&path).await.unwrap();
        wire::send(&mut c, &json!({"id": 1, "method": "hello", "token": "secret"})).await.unwrap();
        assert_eq!(wire::recv(&mut c).await.unwrap()["ok"], json!(true));

        wire::send(&mut c, &json!({"id": 2, "method": "call_tool",
            "params": {"name": "echo", "arguments": {"x": 1}}})).await.unwrap();
        let reply = wire::recv(&mut c).await.unwrap();
        assert_eq!(reply["ok"], json!(true));
        assert_eq!(reply["value"], json!({"echoed": {"x": 1}}));

        wire::send(&mut c, &json!({"id": 3, "method": "result", "params": {"value": 42}})).await.unwrap();
        assert_eq!(wire::recv(&mut c).await.unwrap()["ok"], json!(true));
        drop(c);

        let session = server.await.unwrap();
        assert!(session.authorized);
        assert_eq!(session.tool_calls, 1);
        assert_eq!(session.result, Some(json!(42)));
    }

    #[tokio::test]
    async fn rejects_bad_token() {
        let (broker, path, _dir) = bind(cfg(None)).await;
        let server = tokio::spawn(async move { broker.serve_one().await.unwrap() });
        let mut c = UnixStream::connect(&path).await.unwrap();
        wire::send(&mut c, &json!({"id": 1, "method": "hello", "token": "wrong"})).await.unwrap();
        let reply = wire::recv(&mut c).await.unwrap();
        assert_eq!(reply["ok"], json!(false));
        assert_eq!(reply["error"], json!("unauthorized"));
        drop(c);
        assert!(!server.await.unwrap().authorized);
    }

    #[tokio::test]
    async fn enforces_allowlist_and_tool_errors() {
        let (broker, path, _dir) = bind(cfg(Some(&["echo"]))).await;
        let server = tokio::spawn(async move { broker.serve_one().await.unwrap() });
        let mut c = UnixStream::connect(&path).await.unwrap();
        wire::send(&mut c, &json!({"id": 1, "method": "hello", "token": "secret"})).await.unwrap();
        wire::recv(&mut c).await.unwrap();

        // not on the allowlist
        wire::send(&mut c, &json!({"id": 2, "method": "call_tool", "params": {"name": "boom"}})).await.unwrap();
        let reply = wire::recv(&mut c).await.unwrap();
        assert_eq!(reply["ok"], json!(false));
        assert!(reply["error"].as_str().unwrap().contains("not permitted"));

        // a wrong namespace is rejected even for an allowed tool
        wire::send(&mut c, &json!({"id": 3, "method": "call_tool",
            "params": {"name": "echo", "namespace": "evil"}})).await.unwrap();
        assert!(wire::recv(&mut c).await.unwrap()["error"].as_str().unwrap().contains("namespace"));
        drop(c);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn enforces_the_tool_call_ceiling() {
        let (broker, path, _dir) = bind(cfg_with_cap(None, 1)).await;
        let server = tokio::spawn(async move { broker.serve_one().await.unwrap() });
        let mut c = UnixStream::connect(&path).await.unwrap();
        wire::send(&mut c, &json!({"id": 1, "method": "hello", "token": "secret"})).await.unwrap();
        wire::recv(&mut c).await.unwrap();

        // First call is within budget.
        wire::send(&mut c, &json!({"id": 2, "method": "call_tool", "params": {"name": "echo"}})).await.unwrap();
        assert_eq!(wire::recv(&mut c).await.unwrap()["ok"], json!(true));

        // Second call exceeds the ceiling and is refused.
        wire::send(&mut c, &json!({"id": 3, "method": "call_tool", "params": {"name": "echo"}})).await.unwrap();
        let reply = wire::recv(&mut c).await.unwrap();
        assert_eq!(reply["ok"], json!(false));
        assert!(reply["error"].as_str().unwrap().contains("limit"));
        drop(c);
        assert_eq!(server.await.unwrap().tool_calls, 1);
    }

    #[tokio::test]
    async fn tool_failure_becomes_error_reply() {
        let (broker, path, _dir) = bind(cfg(None)).await; // all allowed
        let server = tokio::spawn(async move { broker.serve_one().await.unwrap() });
        let mut c = UnixStream::connect(&path).await.unwrap();
        wire::send(&mut c, &json!({"id": 1, "method": "hello", "token": "secret"})).await.unwrap();
        wire::recv(&mut c).await.unwrap();
        wire::send(&mut c, &json!({"id": 2, "method": "call_tool", "params": {"name": "boom"}})).await.unwrap();
        let reply = wire::recv(&mut c).await.unwrap();
        assert_eq!(reply["ok"], json!(false));
        assert_eq!(reply["error"], json!("kaboom"));
        drop(c);
        server.await.unwrap();
    }
}
