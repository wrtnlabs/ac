use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::{CachedToolSpec, HttpOptions, McpConnection, McpDialer, McpError};

use super::config::{ServerConfig, StdioConfig};

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
    pub connect_timeout: Duration,
    pub discovery_timeout: Duration,
    pub stdio_stderr: StderrMode,
}

impl std::fmt::Debug for ConnectionPolicy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConnectionPolicy")
            .field("stdio_env_keys", &self.stdio_env.keys().collect::<Vec<_>>())
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

    fn into_mcp(self, server: &str) -> McpError {
        match self {
            Self::Mcp(error) => error,
            Self::ConnectTimeout(timeout) => McpError::Connect {
                server: connection_name(server),
                message: format!("connect timed out after {timeout:?}"),
            },
            Self::DiscoveryTimeout(timeout) => McpError::Connect {
                server: connection_name(server),
                message: format!("tool discovery timed out after {timeout:?}"),
            },
        }
    }
}

fn command_from(config: &StdioConfig, policy: &ConnectionPolicy) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(&config.command);
    if let Some(args) = &config.args {
        command.args(args);
    }
    command.env_clear();
    command.envs(policy.stdio_env.iter());
    if let Some(environment) = &config.env {
        command.envs(environment);
    }
    match policy.stdio_stderr {
        StderrMode::Inherit => {
            command.stderr(Stdio::inherit());
        }
        StderrMode::Null => {
            command.stderr(Stdio::null());
        }
    }
    command
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
            McpConnection::connect_command(connection_name, command_from(config, policy)).await
        }
        ServerConfig::Remote(config) => {
            let options = HttpOptions {
                bearer_token: bearer_token.filter(|_| config.oauth_enabled()),
                headers: config
                    .headers
                    .as_ref()
                    .map(|headers| {
                        headers
                            .iter()
                            .map(|(name, value)| (name.clone(), value.clone()))
                            .collect()
                    })
                    .unwrap_or_default(),
            };
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
            connect(&name, &config, bearer_token, &policy)
                .await
                .map_err(|error| error.into_mcp(&name))
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

    #[tokio::test]
    async fn spawn_failure_is_typed() {
        let config = ServerConfig::Stdio(StdioConfig {
            command: "/definitely/not/an/mcp/server".into(),
            args: None,
            env: None,
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
