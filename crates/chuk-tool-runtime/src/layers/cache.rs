//! Result-caching layer (outermost policy).
//!
//! A hit short-circuits every inner layer, so this wraps outermost. Caching is
//! opt-in per tool and only successful results are stored (errors are never
//! cached), matching `chuk-tool-processor`'s `CachingToolExecutor`.

use std::collections::HashMap;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};

use crate::config::CacheConfig;
use crate::invoker::ToolInvoker;
use crate::outcome::ToolOutcome;

/// Canonical string form of arguments, used as the per-tool cache sub-key.
///
/// `serde_json`'s default `Map` is a `BTreeMap`, so serialization is key-sorted
/// and stable across argument orderings (equivalent to Python's
/// `json.dumps(..., sort_keys=True)`).
fn args_key(args: &Value) -> String {
    serde_json::to_string(args).unwrap_or_default()
}

struct Entry {
    value: Value,
    expires_at: Option<Instant>,
}

/// Wraps an inner invoker, serving cached successful results for opted-in tools.
pub struct CacheLayer<I> {
    inner: I,
    config: CacheConfig,
    store: Mutex<HashMap<(String, String), Entry>>,
}

impl<I> CacheLayer<I> {
    /// Wrap `inner` with the given cache policy.
    pub fn new(inner: I, config: CacheConfig) -> Self {
        Self {
            inner,
            config,
            store: Mutex::new(HashMap::new()),
        }
    }

    /// Number of live (unexpired) entries — for diagnostics/tests.
    pub async fn len(&self) -> usize {
        let now = Instant::now();
        let store = self.store.lock().await;
        store
            .values()
            .filter(|e| e.expires_at.map(|t| now < t).unwrap_or(true))
            .count()
    }

    /// Whether the cache holds no live entries.
    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }
}

#[async_trait]
impl<I: ToolInvoker> ToolInvoker for CacheLayer<I> {
    async fn call_tool(&self, tool: &str, args: Value, timeout: Option<Duration>) -> ToolOutcome {
        if !self.config.is_cacheable(tool) {
            return self.inner.call_tool(tool, args, timeout).await;
        }

        let key = (tool.to_string(), args_key(&args));

        // Lookup (evicting an expired entry on the way).
        {
            let mut store = self.store.lock().await;
            if let Some(entry) = store.get(&key) {
                let expired = entry.expires_at.map(|t| Instant::now() >= t).unwrap_or(false);
                if expired {
                    store.remove(&key);
                } else {
                    let mut hit = ToolOutcome::ok(tool, entry.value.clone());
                    hit.from_cache = true;
                    return hit;
                }
            }
        }

        let outcome = self.inner.call_tool(tool, args, timeout).await;

        // Only successful results are cached.
        if outcome.success {
            if let Some(value) = &outcome.result {
                let expires_at = self.config.ttl_for(tool).map(|ttl| Instant::now() + ttl);
                self.store.lock().await.insert(
                    key,
                    Entry {
                        value: value.clone(),
                        expires_at,
                    },
                );
            }
        }

        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Invoker that returns `{"n": <call#>}` on success, or an error, counting calls.
    struct Mock {
        calls: AtomicUsize,
        fail: bool,
    }
    impl Mock {
        fn ok() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                fail: false,
            }
        }
        fn failing() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                fail: true,
            }
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }
    #[async_trait]
    impl ToolInvoker for Mock {
        async fn call_tool(&self, tool: &str, _a: Value, _t: Option<Duration>) -> ToolOutcome {
            let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if self.fail {
                ToolOutcome::err(tool, "boom")
            } else {
                ToolOutcome::ok(tool, serde_json::json!({ "n": n }))
            }
        }
    }

    fn cfg_for(tools: &[&str], ttl: Option<Duration>) -> CacheConfig {
        CacheConfig {
            cacheable_tools: tools.iter().map(|s| s.to_string()).collect::<HashSet<_>>(),
            default_ttl: ttl,
            per_tool_ttl: HashMap::new(),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn caches_successful_result() {
        let layer = CacheLayer::new(Mock::ok(), cfg_for(&["x"], Some(Duration::from_secs(300))));

        let first = layer.call_tool("x", Value::Null, None).await;
        assert!(first.success && !first.from_cache);
        assert_eq!(first.result, Some(serde_json::json!({"n": 1})));

        let second = layer.call_tool("x", Value::Null, None).await;
        assert!(second.from_cache);
        assert_eq!(second.result, Some(serde_json::json!({"n": 1}))); // same value, not n:2
        assert_eq!(layer.inner.calls(), 1); // inner only hit once
    }

    #[tokio::test(start_paused = true)]
    async fn errors_are_not_cached() {
        let layer = CacheLayer::new(Mock::failing(), cfg_for(&["x"], None));
        layer.call_tool("x", Value::Null, None).await;
        layer.call_tool("x", Value::Null, None).await;
        assert_eq!(layer.inner.calls(), 2);
        assert!(layer.is_empty().await);
    }

    #[tokio::test(start_paused = true)]
    async fn entries_expire_after_ttl() {
        let layer = CacheLayer::new(Mock::ok(), cfg_for(&["x"], Some(Duration::from_secs(10))));
        layer.call_tool("x", Value::Null, None).await;
        assert_eq!(layer.inner.calls(), 1);

        tokio::time::advance(Duration::from_secs(10)).await;

        let after = layer.call_tool("x", Value::Null, None).await;
        assert!(!after.from_cache); // expired → recomputed
        assert_eq!(layer.inner.calls(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn non_cacheable_tool_passes_through() {
        let layer = CacheLayer::new(Mock::ok(), cfg_for(&["x"], None));
        let a = layer.call_tool("y", Value::Null, None).await;
        let b = layer.call_tool("y", Value::Null, None).await;
        assert!(!a.from_cache && !b.from_cache);
        assert_eq!(layer.inner.calls(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn distinct_args_are_distinct_entries() {
        let layer = CacheLayer::new(Mock::ok(), cfg_for(&["x"], None));
        let a = layer.call_tool("x", serde_json::json!({"q": 1}), None).await;
        let b = layer.call_tool("x", serde_json::json!({"q": 2}), None).await;
        assert_eq!(a.result, Some(serde_json::json!({"n": 1})));
        assert_eq!(b.result, Some(serde_json::json!({"n": 2})));

        // Re-issuing the first args hits the cache (still n:1), no new inner call.
        let a2 = layer.call_tool("x", serde_json::json!({"q": 1}), None).await;
        assert!(a2.from_cache);
        assert_eq!(a2.result, Some(serde_json::json!({"n": 1})));
        assert_eq!(layer.inner.calls(), 2);
        assert_eq!(layer.len().await, 2);
    }

    #[test]
    fn args_key_is_order_independent() {
        let a = args_key(&serde_json::json!({"a": 1, "b": 2}));
        let b = args_key(&serde_json::json!({"b": 2, "a": 1}));
        assert_eq!(a, b);
    }
}
