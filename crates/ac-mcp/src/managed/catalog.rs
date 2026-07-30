use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::config::{Config, ServerConfig};
use crate::CachedToolSpec;

/// One tool in the connection-free catalog.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogEntry {
    pub server: String,
    #[serde(rename = "toolName")]
    pub tool_name: String,
    /// Provider-safe registry name, retained verbatim on read.
    #[serde(rename = "qualifiedName")]
    pub qualified_name: String,
    #[serde(default)]
    pub description: String,
    #[serde(rename = "inputSchema", default = "default_input_schema")]
    pub input_schema: Value,
    #[serde(
        rename = "readOnlyHint",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub read_only_hint: Option<bool>,
}

fn default_input_schema() -> Value {
    serde_json::json!({ "type": "object" })
}

impl CatalogEntry {
    pub fn as_cached_spec(&self) -> CachedToolSpec {
        CachedToolSpec {
            name: self.tool_name.clone(),
            registry_name: Some(self.qualified_name.clone()),
            description: Some(self.description.clone()),
            input_schema: self.input_schema.clone(),
            read_only_hint: self.read_only_hint,
        }
    }

    /// Neutral metadata suitable for a host's search/index projection.
    pub fn search_metadata(&self) -> CatalogSearchMetadata {
        CatalogSearchMetadata {
            id: self.qualified_name.clone(),
            server: self.server.clone(),
            tool_name: self.tool_name.clone(),
            description: self.description.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogSearchMetadata {
    pub id: String,
    pub server: String,
    pub tool_name: String,
    pub description: String,
}

/// Identity record for a successful enumeration, including zero-tool
/// servers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogServer {
    #[serde(rename = "configFingerprint")]
    pub config_fingerprint: String,
}

/// Version-2 offline catalog.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CatalogCache {
    pub servers: BTreeMap<String, CatalogServer>,
    pub entries: Vec<CatalogEntry>,
}

impl CatalogCache {
    pub fn server_is_current(&self, name: &str, config: &ServerConfig) -> bool {
        self.servers
            .get(name)
            .is_some_and(|identity| identity.config_fingerprint == config.fingerprint())
    }

    pub fn current_server_names(&self, config: &Config) -> BTreeSet<String> {
        config
            .mcp_servers
            .iter()
            .filter(|(name, server)| self.server_is_current(name, server))
            .map(|(name, _)| name.clone())
            .collect()
    }

    pub fn current_entries(&self, config: &Config) -> Vec<CatalogEntry> {
        self.entries
            .iter()
            .filter(|entry| {
                config
                    .mcp_servers
                    .get(&entry.server)
                    .is_some_and(|server| self.server_is_current(&entry.server, server))
            })
            .cloned()
            .collect()
    }

    /// Drop entries whose server was removed or whose definition changed.
    pub fn retain_current(&mut self, config: &Config) -> bool {
        let before_servers = self.servers.len();
        self.servers.retain(|name, identity| {
            config
                .mcp_servers
                .get(name)
                .is_some_and(|server| identity.config_fingerprint == server.fingerprint())
        });
        let before_entries = self.entries.len();
        self.entries
            .retain(|entry| self.servers.contains_key(&entry.server));
        before_servers != self.servers.len() || before_entries != self.entries.len()
    }

    pub fn remove_server(&mut self, server: &str) -> bool {
        let identity_removed = self.servers.remove(server).is_some();
        let before = self.entries.len();
        self.entries.retain(|entry| entry.server != server);
        identity_removed || before != self.entries.len()
    }

    pub fn replace_server(
        &mut self,
        server: &str,
        config: &ServerConfig,
        entries: Vec<CatalogEntry>,
    ) {
        self.remove_server(server);
        self.servers.insert(
            server.to_string(),
            CatalogServer {
                config_fingerprint: config.fingerprint(),
            },
        );
        self.entries.extend(entries);
    }
}

pub fn sanitize_segment(segment: &str) -> String {
    segment
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect()
}

/// Stable provider-safe name for a catalog tool.
///
/// Common safe names retain the readable `mcp__server__tool` shape. Inputs
/// whose sanitization/delimiters could alias another pair, and long inputs,
/// receive a suffix derived from both raw segments.
pub fn qualified_tool_name(server: &str, tool: &str) -> String {
    let base = format!(
        "mcp__{}__{}",
        sanitize_segment(server),
        sanitize_segment(tool)
    );
    let segment_is_direct = |segment: &str| {
        segment
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
            && !segment.contains("__")
    };
    let needs_hash = !segment_is_direct(server)
        || !segment_is_direct(tool)
        || server.ends_with('_')
        || base.len() > crate::MAX_TOOL_NAME_LEN;
    if !needs_hash {
        return base;
    }
    let digest = Sha256::digest(format!("{server} {tool}").as_bytes());
    let hash: String = digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let suffix = format!("__{hash}");
    let prefix_len = (crate::MAX_TOOL_NAME_LEN - suffix.len()).min(base.len());
    format!("{}{suffix}", &base[..prefix_len])
}

/// Host-neutral policy for names persisted in newly enumerated catalog rows.
///
/// The default is collision-safe. An embedding with an existing public-name
/// contract can inject its established deterministic mapping without
/// replacing any other managed catalog behavior.
type QualifyName = dyn Fn(&str, &str) -> String + Send + Sync;

#[derive(Clone)]
pub struct CatalogNamePolicy {
    qualify: Arc<QualifyName>,
}

impl std::fmt::Debug for CatalogNamePolicy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CatalogNamePolicy")
            .finish_non_exhaustive()
    }
}

impl Default for CatalogNamePolicy {
    fn default() -> Self {
        Self::new(qualified_tool_name)
    }
}

impl CatalogNamePolicy {
    pub fn new(qualify: impl Fn(&str, &str) -> String + Send + Sync + 'static) -> Self {
        Self {
            qualify: Arc::new(qualify),
        }
    }

    pub fn qualified_name(&self, server: &str, tool: &str) -> String {
        (self.qualify)(server, tool)
    }
}

pub fn entries_from_specs(server: &str, specs: Vec<CachedToolSpec>) -> Vec<CatalogEntry> {
    entries_from_specs_with(server, specs, qualified_tool_name)
}

pub fn entries_from_specs_with(
    server: &str,
    specs: Vec<CachedToolSpec>,
    mut qualify: impl FnMut(&str, &str) -> String,
) -> Vec<CatalogEntry> {
    specs
        .into_iter()
        .map(|spec| CatalogEntry {
            qualified_name: spec
                .registry_name
                .unwrap_or_else(|| qualify(server, &spec.name)),
            server: server.to_string(),
            tool_name: spec.name,
            description: spec.description.unwrap_or_default(),
            input_schema: spec.input_schema,
            read_only_hint: spec.read_only_hint,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use indexmap::IndexMap;
    use serde_json::json;

    use super::*;
    use crate::managed::config::StdioConfig;

    fn server(command: &str) -> ServerConfig {
        ServerConfig::Stdio(StdioConfig {
            command: command.to_string(),
            args: None,
            env: None,
            env_vars: None,
            cwd: None,
        })
    }

    #[test]
    fn cache_is_bound_to_the_exact_definition() {
        let one = server("one");
        let two = server("two");
        let entry = CatalogEntry {
            server: "s".into(),
            tool_name: "t".into(),
            qualified_name: "mcp__s__t".into(),
            description: String::new(),
            input_schema: json!({ "type": "object" }),
            read_only_hint: None,
        };
        let mut cache = CatalogCache::default();
        cache.replace_server("s", &one, vec![entry.clone()]);
        let current = Config {
            mcp_servers: IndexMap::from([("s".into(), one)]),
        };
        assert_eq!(cache.current_entries(&current), vec![entry]);

        let changed = Config {
            mcp_servers: IndexMap::from([("s".into(), two)]),
        };
        assert!(cache.retain_current(&changed));
        assert!(cache.entries.is_empty());
        assert!(cache.servers.is_empty());
    }

    #[test]
    fn names_are_provider_safe_and_deterministic() {
        assert_eq!(
            qualified_tool_name("server", "list_items"),
            "mcp__server__list_items"
        );
        assert_ne!(
            qualified_tool_name("server", "list-items"),
            qualified_tool_name("server", "list_items")
        );
        assert_ne!(
            qualified_tool_name("a-b", "tool"),
            qualified_tool_name("a_b", "tool")
        );
        let long = qualified_tool_name("server", &"x".repeat(100));
        assert!(long.len() <= crate::MAX_TOOL_NAME_LEN);
        assert_eq!(long, qualified_tool_name("server", &"x".repeat(100)));
        assert_ne!(long, qualified_tool_name("server", &"y".repeat(100)));
    }

    #[test]
    fn custom_name_policy_only_fills_unqualified_specs() {
        let entries = entries_from_specs_with(
            "server",
            vec![
                CachedToolSpec {
                    name: "one".into(),
                    registry_name: None,
                    description: None,
                    input_schema: json!({}),
                    read_only_hint: None,
                },
                CachedToolSpec {
                    name: "two".into(),
                    registry_name: Some("preserved".into()),
                    description: None,
                    input_schema: json!({}),
                    read_only_hint: None,
                },
            ],
            |server, tool| format!("custom__{server}__{tool}"),
        );
        assert_eq!(entries[0].qualified_name, "custom__server__one");
        assert_eq!(entries[1].qualified_name, "preserved");
    }
}
