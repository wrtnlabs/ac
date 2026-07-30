use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::{CachedToolSpec, HttpOptions, McpConnection, McpDialer, McpError};

use super::config::{ServerConfig, StdioConfig};

type EnvironmentValueResolver = dyn Fn(&str) -> Option<String> + Send + Sync;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum StderrMode {
    Inherit,
    #[default]
    Null,
}

/// Host-injected process and connection policy.
///
/// The environment is a materialized map, not an allowlist: the host decides
/// what crosses the process boundary and AC never consults the ambient
/// environment on its own.
#[derive(Clone)]
pub struct ConnectionPolicy {
    pub stdio_env: BTreeMap<OsString, OsString>,
    environment_value: Option<Arc<EnvironmentValueResolver>>,
    pub connect_timeout: Duration,
    pub discovery_timeout: Duration,
    pub stdio_stderr: StderrMode,
}

impl std::fmt::Debug for ConnectionPolicy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConnectionPolicy")
            .field("stdio_env_keys", &self.stdio_env.keys().collect::<Vec<_>>())
            .field(
                "has_environment_value_resolver",
                &self.environment_value.is_some(),
            )
            .field("connect_timeout", &self.connect_timeout)
            .field("discovery_timeout", &self.discovery_timeout)
            .field("stdio_stderr", &self.stdio_stderr)
            .finish()
    }
}

impl Default for ConnectionPolicy {
    fn default() -> Self {
        Self {
            stdio_env: BTreeMap::new(),
            environment_value: None,
            connect_timeout: Duration::from_secs(15),
            discovery_timeout: Duration::from_secs(15),
            stdio_stderr: StderrMode::Null,
        }
    }
}

impl ConnectionPolicy {
    pub fn with_env(
        mut self,
        environment: impl IntoIterator<Item = (impl Into<OsString>, impl Into<OsString>)>,
    ) -> Self {
        self.stdio_env = environment
            .into_iter()
            .map(|(key, value)| (key.into(), value.into()))
            .collect();
        self
    }

    pub fn with_connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// Supply symbolic environment values for remote header and bearer
    /// references. AC never reads ambient process state on its own.
    pub fn with_environment_value_resolver(
        mut self,
        resolver: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        self.environment_value = Some(Arc::new(resolver));
        self
    }

    fn environment_value(&self, name: &str) -> Option<String> {
        self.environment_value
            .as_ref()
            .and_then(|resolver| resolver(name))
    }

    pub fn with_discovery_timeout(mut self, timeout: Duration) -> Self {
        self.discovery_timeout = timeout;
        self
    }
}

/// Normalize a display/config key into AC's unambiguous connection grammar.
pub fn connection_name(name: &str) -> String {
    let already_valid = !name.is_empty()
        && !name.contains("__")
        && !name.ends_with('_')
        && name.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '-' || character == '_'
        });
    if already_valid {
        return name.to_string();
    }

    let mut normalized: String = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect();
    while normalized.contains("__") {
        normalized = normalized.replace("__", "_-");
    }
    if normalized.ends_with('_') {
        normalized.pop();
        normalized.push('-');
    }
    if normalized.is_empty() {
        normalized.push_str("server");
    }
    let digest = Sha256::digest(name.as_bytes());
    let hash: String = digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("{normalized}-{hash}")
}

#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    #[error("MCP connect timed out after {}ms", .0.as_millis())]
    ConnectTimeout(Duration),
    #[error("MCP tool discovery timed out after {}ms", .0.as_millis())]
    DiscoveryTimeout(Duration),
    #[error(transparent)]
    Mcp(#[from] McpError),
}

impl ConnectError {
    pub fn needs_auth(&self) -> bool {
        matches!(self, Self::Mcp(McpError::Auth { .. }))
    }

    fn into_redacted_mcp(self, server: &str) -> McpError {
        let needs_auth = self.needs_auth();
        // Connection construction captures the exact environment/header values
        // it sends and McpConnection redacts with that one-shot snapshot.
        // Never invoke a host resolver again while surfacing the error: it may
        // rotate or intentionally be one-shot.
        let message = self.to_string();
        let server = connection_name(server);
        if needs_auth {
            McpError::Auth { server, message }
        } else {
            McpError::Connect { server, message }
        }
    }
}

fn command_from(
    config: &StdioConfig,
    policy: &ConnectionPolicy,
) -> (tokio::process::Command, Vec<String>) {
    let mut command = tokio::process::Command::new(&config.command);
    let mut secrets = Vec::new();
    if let Some(args) = &config.args {
        command.args(args);
    }
    command.env_clear();
    command.envs(policy.stdio_env.iter());
    let mut configured_environment = BTreeMap::new();
    for name in config.env_vars.iter().flatten() {
        if let Some(value) = policy.environment_value(name) {
            configured_environment.insert(name.clone(), value);
        }
    }
    if let Some(environment) = &config.env {
        configured_environment.extend(environment.clone());
    }
    secrets.extend(configured_environment.values().cloned());
    command.envs(configured_environment);
    if let Some(cwd) = &config.cwd {
        command.current_dir(cwd);
    }
    match policy.stdio_stderr {
        StderrMode::Inherit => {
            command.stderr(Stdio::inherit());
        }
        StderrMode::Null => {
            command.stderr(Stdio::null());
        }
    }
    (command, secrets)
}

fn http_options(
    config: &super::config::RemoteConfig,
    oauth_bearer: Option<String>,
    policy: &ConnectionPolicy,
) -> HttpOptions {
    let mut headers = config.headers.clone().unwrap_or_default();
    for (header, environment_key) in config.env_headers.iter().flatten() {
        if let Some(value) = policy.environment_value(environment_key) {
            headers.insert(header.clone(), value);
        }
    }
    let environment_bearer = config
        .bearer_token_env_var
        .as_deref()
        .and_then(|name| policy.environment_value(name));
    let has_authorization_header = headers
        .keys()
        .any(|header| header.eq_ignore_ascii_case("authorization"));
    HttpOptions {
        bearer_token: if has_authorization_header {
            None
        } else {
            environment_bearer.or_else(|| oauth_bearer.filter(|_| config.oauth_enabled()))
        },
        headers: headers.into_iter().collect(),
    }
}

pub(crate) fn redact_error_message(
    config: &ServerConfig,
    extra_secrets: &[String],
    message: &str,
) -> String {
    let mut secrets = Vec::new();
    match config {
        ServerConfig::Stdio(config) => {
            secrets.extend(config.env.iter().flatten().map(|(_, value)| value.clone()));
        }
        ServerConfig::Remote(config) => {
            if !config.url.is_empty() {
                secrets.push(config.url.clone());
            }
            if let Ok(url) = reqwest::Url::parse(&config.url) {
                secrets.push(url.as_str().to_string());
                if !url.username().is_empty() {
                    secrets.push(url.username().to_string());
                }
                if let Some(password) = url.password() {
                    secrets.push(password.to_string());
                }
                secrets.extend(url.query_pairs().map(|(_, value)| value.into_owned()));
                if let Some(fragment) = url.fragment() {
                    secrets.push(fragment.to_string());
                }
            }
            secrets.extend(
                config
                    .headers
                    .iter()
                    .flatten()
                    .map(|(_, value)| value.clone()),
            );
            if let Some(secret) = config
                .oauth_settings()
                .and_then(|settings| settings.client_secret.clone())
            {
                secrets.push(secret);
            }
        }
    }
    secrets.extend(extra_secrets.iter().cloned());
    let (mut redacted, truncated) =
        crate::redact_and_limit(message.to_string(), &secrets, crate::MAX_ERROR_BYTES);
    if truncated {
        redacted.push_str("\n[truncated: MCP error exceeded 32 KiB]");
    }
    redacted
}

async fn connect_unbounded(
    name: &str,
    config: &ServerConfig,
    bearer_token: Option<String>,
    policy: &ConnectionPolicy,
) -> Result<McpConnection, McpError> {
    let connection_name = connection_name(name);
    match config {
        ServerConfig::Stdio(config) => {
            let (command, secrets) = command_from(config, policy);
            McpConnection::connect_command_with_redaction(connection_name, command, secrets).await
        }
        ServerConfig::Remote(config) => {
            let options = http_options(config, bearer_token, policy);
            McpConnection::connect_http_with(connection_name, &config.url, options).await
        }
    }
}

/// Open one configured server under the host's connect deadline.
pub async fn connect(
    name: &str,
    config: &ServerConfig,
    bearer_token: Option<String>,
    policy: &ConnectionPolicy,
) -> Result<McpConnection, ConnectError> {
    tokio::time::timeout(
        policy.connect_timeout,
        connect_unbounded(name, config, bearer_token, policy),
    )
    .await
    .map_err(|_| ConnectError::ConnectTimeout(policy.connect_timeout))?
    .map_err(ConnectError::Mcp)
}

struct ConnectionGuard(McpConnection);

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.0.shutdown();
    }
}

async fn export_catalog_with_timeout(
    connection: McpConnection,
    timeout: Duration,
) -> Result<Vec<CachedToolSpec>, ConnectError> {
    let connection = ConnectionGuard(connection);
    tokio::time::timeout(timeout, connection.0.export_catalog())
        .await
        .map_err(|_| ConnectError::DiscoveryTimeout(timeout))?
        .map_err(ConnectError::Mcp)
}

/// Connect, list every tool, and shut down the transport.
pub async fn enumerate(
    name: &str,
    config: &ServerConfig,
    bearer_token: Option<String>,
    policy: &ConnectionPolicy,
) -> Result<Vec<CachedToolSpec>, ConnectError> {
    let connection = connect(name, config, bearer_token, policy).await?;
    export_catalog_with_timeout(connection, policy.discovery_timeout).await
}

/// Build a lazy dialer with exactly the same policy used by probes and
/// enumeration.
pub fn dialer(
    name: &str,
    config: &ServerConfig,
    bearer_token: Option<String>,
    policy: &ConnectionPolicy,
) -> McpDialer {
    let name = name.to_string();
    let config = config.clone();
    let bearer_token = bearer_token.clone();
    let policy = policy.clone();
    Arc::new(move || {
        let name = name.clone();
        let config = config.clone();
        let bearer_token = bearer_token.clone();
        let policy = policy.clone();
        Box::pin(async move {
            connect(&name, &config, bearer_token.clone(), &policy)
                .await
                .map_err(|error| error.into_redacted_mcp(&name))
        })
    })
}

/// Convenience for hosts materializing an inherited environment from a key
/// allowlist.
pub fn select_environment<'a>(
    keys: impl IntoIterator<Item = &'a OsStr>,
) -> BTreeMap<OsString, OsString> {
    keys.into_iter()
        .filter_map(|key| std::env::var_os(key).map(|value| (key.to_os_string(), value)))
        .collect()
}

#[cfg(test)]
mod tests {
    use rmcp::model::{
        ErrorData, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerInfo,
    };
    use rmcp::service::RequestContext;
    use rmcp::{RoleServer, ServerHandler, ServiceExt};

    use super::*;
    use crate::managed::RemoteConfig;

    #[derive(Clone)]
    struct HangingServer;

    impl ServerHandler for HangingServer {
        fn get_info(&self) -> ServerInfo {
            ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
        }

        async fn list_tools(
            &self,
            _request: Option<PaginatedRequestParams>,
            _context: RequestContext<RoleServer>,
        ) -> Result<ListToolsResult, ErrorData> {
            std::future::pending().await
        }
    }

    async fn hanging_connection() -> (McpConnection, McpConnection, tokio::task::JoinHandle<()>) {
        let (client_io, server_io) = tokio::io::duplex(1 << 16);
        let server = tokio::spawn(async move {
            let service = HangingServer
                .serve(server_io)
                .await
                .expect("server handshake");
            let _ = service.waiting().await;
        });
        let connection = McpConnection::connect("hanging", client_io)
            .await
            .expect("client handshake");
        let observer = McpConnection {
            service: connection.service.clone(),
            name: connection.name.clone(),
            redaction_secrets: connection.redaction_secrets.clone(),
        };
        (connection, observer, server)
    }

    #[test]
    fn connection_names_are_valid_and_stable() {
        assert_eq!(connection_name("linear"), "linear");
        assert!(connection_name("my server!").starts_with("my_server-"));
        assert!(connection_name("a__b").starts_with("a_-b-"));
        assert!(connection_name("tail_").starts_with("tail-"));
        assert!(connection_name("").starts_with("server-"));
        assert_ne!(connection_name("my server!"), connection_name("my_server-"));
        assert_ne!(connection_name("a__b"), connection_name("a_-b"));
        assert_ne!(connection_name("tail_"), connection_name("tail-"));
        assert_ne!(connection_name(""), connection_name("server"));
        for raw in ["a__b__", "___", "한글", "x y_z"] {
            let name = connection_name(raw);
            assert!(!name.contains("__"));
            assert!(!name.ends_with('_'));
            assert!(
                name.chars()
                    .all(|character| character.is_ascii_alphanumeric()
                        || character == '-'
                        || character == '_')
            );
        }
    }

    #[test]
    fn remote_environment_values_resolve_lazily_without_entering_config() {
        let config = RemoteConfig {
            url: "https://example.test/mcp".into(),
            headers: Some(BTreeMap::from([("X-Static".into(), "yes".into())])),
            env_headers: Some(BTreeMap::from([(
                "X-Dynamic".into(),
                "DYNAMIC_HEADER".into(),
            )])),
            bearer_token_env_var: Some("REMOTE_TOKEN".into()),
            oauth: None,
        };
        let policy =
            ConnectionPolicy::default().with_environment_value_resolver(|name| match name {
                "DYNAMIC_HEADER" => Some("runtime".into()),
                "REMOTE_TOKEN" => Some("secret".into()),
                _ => None,
            });
        let options = http_options(&config, Some("oauth".into()), &policy);

        assert!(options.headers.contains(&("X-Static".into(), "yes".into())));
        assert!(
            options
                .headers
                .contains(&("X-Dynamic".into(), "runtime".into()))
        );
        assert_eq!(options.bearer_token.as_deref(), Some("secret"));
        let serialized = serde_json::to_string(&config).unwrap();
        assert!(serialized.contains("DYNAMIC_HEADER"));
        assert!(!serialized.contains("runtime"));
        assert!(!serialized.contains("secret"));
    }

    #[test]
    fn configured_authorization_header_suppresses_environment_and_oauth_bearers() {
        let config = RemoteConfig {
            url: "https://example.test/mcp".into(),
            headers: Some(BTreeMap::from([(
                "authorization".into(),
                "Basic configured".into(),
            )])),
            env_headers: None,
            bearer_token_env_var: Some("REMOTE_TOKEN".into()),
            oauth: None,
        };
        let policy =
            ConnectionPolicy::default().with_environment_value_resolver(|name| match name {
                "REMOTE_TOKEN" => Some("environment-bearer".into()),
                _ => None,
            });

        let options = http_options(&config, Some("oauth-bearer".into()), &policy);

        assert_eq!(options.bearer_token, None);
        assert!(
            options
                .headers
                .contains(&("authorization".into(), "Basic configured".into()))
        );
    }

    #[test]
    fn environment_authorization_header_is_resolved_before_bearer_precedence() {
        let config = RemoteConfig {
            url: "https://example.test/mcp".into(),
            headers: None,
            env_headers: Some(BTreeMap::from([(
                "AUTHORIZATION".into(),
                "AUTH_HEADER".into(),
            )])),
            bearer_token_env_var: Some("REMOTE_TOKEN".into()),
            oauth: None,
        };
        let policy =
            ConnectionPolicy::default().with_environment_value_resolver(|name| match name {
                "AUTH_HEADER" => Some("Token configured".into()),
                "REMOTE_TOKEN" => Some("environment-bearer".into()),
                _ => None,
            });

        let options = http_options(&config, Some("oauth-bearer".into()), &policy);

        assert_eq!(options.bearer_token, None);
        assert!(
            options
                .headers
                .contains(&("AUTHORIZATION".into(), "Token configured".into()))
        );
    }

    #[test]
    fn surfaced_error_redaction_covers_urls_and_every_configured_secret_source() {
        let config = ServerConfig::Remote(RemoteConfig {
            url: "https://user:url-secret@example.test/mcp?token=query-secret".into(),
            headers: Some(BTreeMap::from([(
                "X-Api-Key".into(),
                "static-secret".into(),
            )])),
            env_headers: Some(BTreeMap::from([(
                "X-Dynamic".into(),
                "HEADER_SECRET".into(),
            )])),
            bearer_token_env_var: Some("ENV_BEARER".into()),
            oauth: Some(super::super::OAuthMode::Settings(
                super::super::OAuthSettings {
                    client_secret: Some("client-secret".into()),
                    ..Default::default()
                },
            )),
        });
        let message = "request to https://user:url-secret@example.test/mcp?token=query-secret \
                       exposed static-secret client-secret oauth-access-token";

        let redacted = redact_error_message(&config, &["oauth-access-token".to_string()], message);

        assert!(redacted.contains("[REDACTED]"));
        for secret in [
            "url-secret",
            "query-secret",
            "static-secret",
            "client-secret",
            "oauth-access-token",
        ] {
            assert!(!redacted.contains(secret), "{secret} leaked: {redacted}");
        }
    }

    #[test]
    fn stdio_environment_values_and_cwd_are_applied_lazily() {
        let config = StdioConfig {
            command: "server".into(),
            args: Some(vec!["--stdio".into()]),
            env: Some(BTreeMap::from([("OVERRIDE".into(), "literal".into())])),
            env_vars: Some(vec!["DYNAMIC_TOKEN".into(), "OVERRIDE".into()]),
            cwd: Some("/tmp/agent-core-mcp".into()),
        };
        let policy =
            ConnectionPolicy::default().with_environment_value_resolver(|name| match name {
                "DYNAMIC_TOKEN" => Some("runtime-secret".into()),
                "OVERRIDE" => Some("ambient-value".into()),
                _ => None,
            });
        let (command, secrets) = command_from(&config, &policy);
        let environment = command
            .as_std()
            .get_envs()
            .filter_map(|(name, value)| {
                Some((
                    name.to_string_lossy().into_owned(),
                    value?.to_string_lossy().into_owned(),
                ))
            })
            .collect::<BTreeMap<_, _>>();

        assert_eq!(
            environment.get("DYNAMIC_TOKEN").map(String::as_str),
            Some("runtime-secret")
        );
        assert_eq!(
            environment.get("OVERRIDE").map(String::as_str),
            Some("literal")
        );
        assert_eq!(
            command.as_std().get_current_dir(),
            Some(std::path::Path::new("/tmp/agent-core-mcp"))
        );
        assert!(secrets.contains(&"runtime-secret".to_string()));
        assert!(secrets.contains(&"literal".to_string()));
        assert!(!secrets.contains(&"ambient-value".to_string()));
        let serialized = serde_json::to_string(&config).unwrap();
        assert!(serialized.contains("DYNAMIC_TOKEN"));
        assert!(!serialized.contains("runtime-secret"));
        assert!(!serialized.contains("ambient-value"));
    }

    #[tokio::test]
    async fn spawn_failure_is_typed() {
        let config = ServerConfig::Stdio(StdioConfig {
            command: "/definitely/not/an/mcp/server".into(),
            args: None,
            env: None,
            env_vars: None,
            cwd: None,
        });
        let error = enumerate("missing", &config, None, &ConnectionPolicy::default())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("connect failed"));
    }

    #[tokio::test]
    async fn discovery_timeout_is_typed_and_shuts_down_connection() {
        let (connection, observer, server) = hanging_connection().await;
        let timeout = Duration::from_millis(10);
        let error = export_catalog_with_timeout(connection, timeout)
            .await
            .unwrap_err();
        assert!(matches!(error, ConnectError::DiscoveryTimeout(value) if value == timeout));
        assert_eq!(error.to_string(), "MCP tool discovery timed out after 10ms");
        assert!(observer.is_closed());
        server.abort();
    }

    #[tokio::test]
    async fn cancelling_discovery_shuts_down_connection() {
        let (connection, observer, server) = hanging_connection().await;
        let discovery = tokio::spawn(export_catalog_with_timeout(
            connection,
            Duration::from_secs(60),
        ));
        tokio::task::yield_now().await;
        discovery.abort();
        let _ = discovery.await;
        assert!(observer.is_closed());
        server.abort();
    }
}
