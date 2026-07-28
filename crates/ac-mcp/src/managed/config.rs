use std::collections::BTreeMap;

use indexmap::IndexMap;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// A local MCP server launched over stdio.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StdioConfig {
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<BTreeMap<String, String>>,
}

impl std::fmt::Debug for StdioConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StdioConfig")
            .field("command", &self.command)
            .field("arg_count", &self.args.as_ref().map(Vec::len))
            .field(
                "env_keys",
                &self.env.as_ref().map(|env| env.keys().collect::<Vec<_>>()),
            )
            .finish()
    }
}

/// Optional OAuth fields carried by a remote server definition.
///
/// Redirect defaults and callback presentation are deliberately absent: the
/// embedding application supplies those when it builds an interactive flow.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OAuthSettings {
    #[serde(rename = "clientId", skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(rename = "clientSecret", skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(rename = "redirectUri", skip_serializing_if = "Option::is_none")]
    pub redirect_uri: Option<String>,
    #[serde(rename = "callbackPort", skip_serializing_if = "Option::is_none")]
    pub callback_port: Option<i64>,
}

impl std::fmt::Debug for OAuthSettings {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OAuthSettings")
            .field("client_id", &self.client_id)
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "[REDACTED]"),
            )
            .field("scope", &self.scope)
            .field("redirect_uri", &self.redirect_uri)
            .field("callback_port", &self.callback_port)
            .finish()
    }
}

/// OAuth policy for a remote endpoint.
///
/// A missing `oauth` field and [`OAuthMode::Settings`] both permit OAuth.
/// The literal `false` is represented by [`OAuthMode::Disabled`].
#[derive(Clone, PartialEq, Eq)]
pub enum OAuthMode {
    Disabled,
    Settings(OAuthSettings),
}

impl std::fmt::Debug for OAuthMode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disabled => formatter.write_str("Disabled"),
            Self::Settings(settings) => formatter.debug_tuple("Settings").field(settings).finish(),
        }
    }
}

impl Serialize for OAuthMode {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Disabled => serializer.serialize_bool(false),
            Self::Settings(settings) => settings.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for OAuthMode {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Flag(bool),
            Settings(OAuthSettings),
        }

        match Repr::deserialize(deserializer)? {
            Repr::Flag(false) => Ok(Self::Disabled),
            Repr::Flag(true) => Err(D::Error::custom("oauth must be an object or false")),
            Repr::Settings(settings) => Ok(Self::Settings(settings)),
        }
    }
}

/// A remote MCP server reached over Streamable HTTP.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteConfig {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oauth: Option<OAuthMode>,
}

impl std::fmt::Debug for RemoteConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RemoteConfig")
            .field("url", &"[configured]")
            .field(
                "header_names",
                &self
                    .headers
                    .as_ref()
                    .map(|headers| headers.keys().collect::<Vec<_>>()),
            )
            .field("oauth", &self.oauth)
            .finish()
    }
}

impl RemoteConfig {
    pub fn oauth_enabled(&self) -> bool {
        !matches!(self.oauth, Some(OAuthMode::Disabled))
    }

    pub fn oauth_settings(&self) -> Option<&OAuthSettings> {
        match self.oauth.as_ref() {
            Some(OAuthMode::Settings(settings)) => Some(settings),
            _ => None,
        }
    }
}

/// One portable server definition: stdio XOR remote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ServerConfig {
    Remote(RemoteConfig),
    Stdio(StdioConfig),
}

impl ServerConfig {
    /// Validate constraints that serde cannot express.
    pub fn validate(&self) -> Result<(), &'static str> {
        match self {
            Self::Stdio(config) if config.command.is_empty() => Err("command must be non-empty"),
            Self::Remote(config) if config.url.is_empty() => Err("url must be non-empty"),
            _ => Ok(()),
        }
    }

    pub fn is_remote(&self) -> bool {
        matches!(self, Self::Remote(_))
    }

    pub fn remote(&self) -> Option<&RemoteConfig> {
        match self {
            Self::Remote(remote) => Some(remote),
            Self::Stdio(_) => None,
        }
    }

    /// Stable identity of the exact transport and authentication definition.
    pub fn fingerprint(&self) -> String {
        let encoded = serde_json::to_vec(self).expect("server config serializes");
        format!("sha256:{:x}", Sha256::digest(encoded))
    }
}

/// The portable `mcpServers` registry.
///
/// `IndexMap` intentionally preserves authored key order across reads,
/// mutations, status snapshots, and writes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    #[serde(rename = "mcpServers", default)]
    pub mcp_servers: IndexMap<String, ServerConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectedServer {
    pub server: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedConfig {
    pub config: Config,
    pub rejected: Vec<RejectedServer>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct RawConfigFile {
    #[serde(rename = "mcpServers", default)]
    pub(crate) mcp_servers: IndexMap<String, Value>,
}

pub(crate) fn parse_server_entries(
    entries: impl IntoIterator<Item = (String, Value)>,
) -> ParsedConfig {
    let mut parsed = ParsedConfig::default();
    for (name, value) in entries {
        match serde_json::from_value::<ServerConfig>(value) {
            Ok(config) => match config.validate() {
                Ok(()) => {
                    parsed.config.mcp_servers.insert(name, config);
                }
                Err(reason) => parsed.rejected.push(RejectedServer {
                    server: name,
                    reason: reason.to_string(),
                }),
            },
            Err(error) => parsed.rejected.push(RejectedServer {
                server: name,
                reason: error.to_string(),
            }),
        }
    }
    parsed
}

/// Parse an already-loaded value. File-backed parsing uses the original text
/// directly so it can retain object key order.
pub fn parse_config(raw: &Value) -> ParsedConfig {
    let Some(servers) = raw.get("mcpServers").and_then(Value::as_object) else {
        return ParsedConfig::default();
    };
    parse_server_entries(
        servers
            .iter()
            .map(|(name, value)| (name.clone(), value.clone())),
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn strict_entries_and_fingerprints_are_deterministic() {
        let parsed = parse_config(&json!({
            "mcpServers": {
                "stdio": { "command": "npx", "args": ["server"] },
                "remote": { "url": "https://example.test/mcp", "oauth": false },
                "both": { "command": "npx", "url": "https://example.test/mcp" },
                "empty": { "command": "" },
                "extra": { "command": "npx", "unknown": true }
            }
        }));
        assert_eq!(
            parsed.config.mcp_servers.keys().collect::<Vec<_>>(),
            ["remote", "stdio"]
        );
        assert_eq!(parsed.rejected.len(), 3);

        let config = parsed.config.mcp_servers["remote"].clone();
        assert_eq!(config.fingerprint(), config.clone().fingerprint());
        assert!(config.fingerprint().starts_with("sha256:"));
    }

    #[test]
    fn oauth_true_is_not_a_valid_policy() {
        let parsed = parse_config(&json!({
            "mcpServers": { "bad": { "url": "https://example.test", "oauth": true } }
        }));
        assert!(parsed.config.mcp_servers.is_empty());
        assert_eq!(parsed.rejected[0].server, "bad");
    }
}
