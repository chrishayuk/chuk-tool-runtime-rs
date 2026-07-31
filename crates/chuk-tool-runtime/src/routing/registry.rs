//! Tool → server registry with a first-wins collision policy.
//!
//! Ports `StreamManager._register_tools` from `chuk-tool-processor`: the first
//! server to advertise a tool name owns default (unpinned) routing for it. A
//! later server advertising the same name is recorded as a provider but does
//! **not** take over routing — a bare tool name is not a trust boundary, so a
//! second server must not be able to hijack a name an earlier one already owns.

use std::collections::HashMap;

/// Maps tool names to their owning server (first-wins) and tracks every server
/// that advertises each name.
#[derive(Debug, Default)]
pub struct ToolRegistry {
    owners: HashMap<String, String>,
    providers: HashMap<String, Vec<String>>,
}

impl ToolRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register that `server` advertises `tools`, applying the first-wins policy.
    ///
    /// The first server to advertise a name owns it; a later, different server is
    /// added to the provider list and logged, but the original owner is kept.
    /// Empty tool names are ignored.
    pub fn register(&mut self, server: &str, tools: &[String]) {
        for tool in tools {
            if tool.is_empty() {
                continue;
            }

            let provs = self.providers.entry(tool.clone()).or_default();
            if !provs.iter().any(|s| s == server) {
                provs.push(server.to_string());
            }

            match self.owners.get(tool) {
                None => {
                    self.owners.insert(tool.clone(), server.to_string());
                }
                Some(owner) if owner != server => {
                    tracing::warn!(
                        tool = %tool,
                        owner = %owner,
                        ignored = %server,
                        "tool-name collision: keeping first-wins owner; call the other server explicitly to reach it"
                    );
                }
                _ => {}
            }
        }
    }

    /// The server that owns default routing for `tool`, if any.
    pub fn owner(&self, tool: &str) -> Option<&str> {
        self.owners.get(tool).map(String::as_str)
    }

    /// Every server that advertised `tool`, in registration order (first owns).
    pub fn providers(&self, tool: &str) -> Vec<String> {
        self.providers.get(tool).cloned().unwrap_or_default()
    }

    /// Tool names advertised by more than one server, mapped to those servers.
    pub fn collisions(&self) -> HashMap<String, Vec<String>> {
        self.providers
            .iter()
            .filter(|(_, servers)| servers.len() > 1)
            .map(|(tool, servers)| (tool.clone(), servers.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn first_server_wins_the_name() {
        let mut reg = ToolRegistry::new();
        reg.register("trusted", &names(&["read_file", "search"]));
        reg.register("community", &names(&["read_file"]));

        assert_eq!(reg.owner("read_file"), Some("trusted"));
        assert_eq!(reg.providers("read_file"), vec!["trusted", "community"]);
        assert_eq!(reg.owner("search"), Some("trusted"));
    }

    #[test]
    fn collisions_lists_multi_provider_names_only() {
        let mut reg = ToolRegistry::new();
        reg.register("a", &names(&["shared", "only_a"]));
        reg.register("b", &names(&["shared"]));

        let collisions = reg.collisions();
        assert_eq!(collisions.get("shared"), Some(&vec!["a".into(), "b".into()]));
        assert!(!collisions.contains_key("only_a"));
    }

    #[test]
    fn re_registering_same_server_is_idempotent() {
        let mut reg = ToolRegistry::new();
        reg.register("a", &names(&["t"]));
        reg.register("a", &names(&["t"]));
        assert_eq!(reg.providers("t"), vec!["a"]); // not duplicated
        assert!(reg.collisions().is_empty());
    }

    #[test]
    fn empty_tool_names_are_ignored() {
        let mut reg = ToolRegistry::new();
        reg.register("a", &names(&["", "real"]));
        assert_eq!(reg.owner(""), None);
        assert_eq!(reg.owner("real"), Some("a"));
    }

    #[test]
    fn unknown_tool_has_no_owner_or_providers() {
        let reg = ToolRegistry::new();
        assert_eq!(reg.owner("nope"), None);
        assert!(reg.providers("nope").is_empty());
    }
}
