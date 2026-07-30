//! Host-neutral managed MCP control plane.
//!
//! The low-level [`crate::McpConnection`] API remains available for custom
//! integrations. This module supplies the repeated load-bearing pattern:
//! portable ordered configuration, identity-bound offline catalog, lazy
//! dialers, atomic credential persistence (mode `0600` on Unix), status,
//! refresh/backfill, derived-name migration, symbolic environment-backed
//! remote headers, and mutation ordering. Hosts inject locations, environment
//! values, OAuth presentation/identity, and any RPC or UI projection.

mod catalog;
mod config;
mod connection;
mod credentials;
mod store;

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use ac_tool::ToolRegistry;
use serde::Serialize;
use tokio::sync::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;

pub use catalog::{
    CatalogCache, CatalogEntry, CatalogNamePolicy, CatalogSearchMetadata, CatalogServer,
    entries_from_specs, entries_from_specs_with, qualified_tool_name, sanitize_segment,
};
pub use config::{
    Config, OAuthMode, OAuthSettings, ParsedConfig, RejectedServer, RemoteConfig, ServerConfig,
    ServerConfigError, StdioConfig, parse_config,
};
pub use connection::{
    ConnectError, ConnectionPolicy, StderrMode, connect, connection_name, dialer, enumerate,
    select_environment,
};
pub use credentials::{
    CredentialBinding, CredentialEntry, CredentialState, CredentialStore, FileCredentialStore,
    FileOAuthFlowStore, TokenRecord,
};
pub use store::{FileStateStore, StateStore, StoreError};

use crate::oauth::{
    ClientMetadata, InteractiveOAuthConfig, OAuthCoordinator, OAuthEndpointPolicy,
    OAuthEnumerateError, OAuthEnumerator,
};
use crate::oauth_callback::PageCopy;
use crate::{
    CachedToolSpec, McpDialer, McpError, RegisterOptions, RegisteredTools,
    register_cached as register_cached_tools,
};

#[derive(Debug, thiserror::Error)]
pub enum ManagedError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Mcp(#[from] McpError),
    #[error("MCP server '{0}' is not registered")]
    UnknownServer(String),
    #[error("invalid MCP server definition: {0}")]
    InvalidConfig(String),
    #[error("MCP server registry contains rejected entries: {0}")]
    RejectedConfig(String),
}

fn require_clean_config(parsed: ParsedConfig) -> Result<Config, ManagedError> {
    if parsed.rejected.is_empty() {
        return Ok(parsed.config);
    }
    Err(ManagedError::RejectedConfig(
        parsed
            .rejected
            .iter()
            .map(|entry| entry.server.as_str())
            .collect::<Vec<_>>()
            .join(", "),
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ServerState {
    Pending,
    Cached,
    Failed,
    NeedsAuth,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerStatus {
    pub server: String,
    pub state: ServerState,
    pub tool_count: u64,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
struct StatusRecord {
    server: String,
    config_fingerprint: String,
    state: ServerState,
    tool_count: u64,
    error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthStatus {
    pub server: String,
    pub state: CredentialState,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CatalogServerSnapshot {
    pub server: String,
    pub config: ServerConfig,
    pub config_fingerprint: String,
    pub tools: Vec<CatalogEntry>,
}

/// One coherent config-bound view for registry mounting or search indexing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CatalogSnapshot {
    pub servers: Vec<CatalogServerSnapshot>,
}

impl CatalogSnapshot {
    pub fn entries(&self) -> impl Iterator<Item = &CatalogEntry> {
        self.servers.iter().flat_map(|server| server.tools.iter())
    }

    pub fn search_metadata(&self) -> Vec<CatalogSearchMetadata> {
        self.entries().map(CatalogEntry::search_metadata).collect()
    }
}

fn credential_binding(definition: &ServerConfig) -> Option<CredentialBinding> {
    definition
        .remote()
        .map(|remote| CredentialBinding::new(remote.url.clone(), definition.fingerprint()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshOutcome {
    Cached { tool_count: usize },
    Failed { error: String, needs_auth: bool },
    Stale,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshReport {
    pub server: String,
    pub outcome: RefreshOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpsertResult {
    pub overwritten: bool,
    pub refresh: RefreshOutcome,
}

#[derive(Debug, Clone)]
pub enum ProbeResult {
    Reachable { tools: Vec<CachedToolSpec> },
    Failed { error: String, needs_auth: bool },
}

#[derive(Debug)]
pub struct CatalogRegistration {
    pub server: String,
    pub tools: RegisteredTools,
}

#[derive(Debug)]
pub struct MountResult {
    /// Successfully registered MCP tool names to pass to
    /// `ac_runtime::ConditionalToolsHook`.
    pub gated_names: BTreeSet<String>,
    /// Per-server registration/skipping account.
    pub registrations: Vec<CatalogRegistration>,
    /// Outcome of installing AC's stock search tool.
    pub search: SearchMount,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchMount {
    NotRequested,
    EmptyCatalog,
    Installed,
    SkippedCollision,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthenticationResult {
    pub tool_count: usize,
}

/// Host-owned OAuth identity and presentation.
///
/// Endpoint, enabled state, requested scope, and configured client
/// credentials are derived from the durable remote server definition.
#[derive(Debug, Clone)]
pub struct OAuthHostPolicy {
    /// Fully resolved loopback URI. A host may use its own default callback
    /// path and map any config shorthand before calling the manager.
    pub redirect_uri: String,
    pub discovery_metadata: ClientMetadata,
    pub registration_metadata: ClientMetadata,
    /// Cross-origin authorization-server origins the host explicitly trusts.
    /// Empty keeps OAuth discovery on the configured MCP origin.
    pub endpoint_policy: OAuthEndpointPolicy,
    pub page_copy: PageCopy,
}

#[derive(Debug, thiserror::Error)]
pub enum AuthenticationError {
    #[error("MCP server '{0}' is not registered")]
    UnknownServer(String),
    #[error("MCP server '{0}' is not a remote server")]
    NotRemote(String),
    #[error("OAuth is disabled for this server")]
    Disabled(String),
    #[error("OAuth failed: {0}")]
    Flow(String),
    #[error("MCP server '{0}' changed while authentication was in progress")]
    Changed(String),
    #[error(transparent)]
    Managed(#[from] ManagedError),
}

/// Explicit stock-store locations. Hosts choose every path and can use the
/// same values to deny secret/control-file reads in their sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedPaths {
    pub config: PathBuf,
    pub catalog: PathBuf,
    pub credentials: PathBuf,
}

impl ManagedPaths {
    pub fn new(
        config: impl Into<PathBuf>,
        catalog: impl Into<PathBuf>,
        credentials: impl Into<PathBuf>,
    ) -> Self {
        Self {
            config: config.into(),
            catalog: catalog.into(),
            credentials: credentials.into(),
        }
    }

    /// Every stock control-plane path a host should protect from agent/tool
    /// reads and writes.
    pub fn control_paths(&self) -> Vec<PathBuf> {
        let files = [
            self.config.clone(),
            self.catalog.clone(),
            self.credentials.clone(),
        ];
        let mut paths = files.to_vec();
        for file in &files {
            let temporary = store::private_temp_dir(file);
            if !paths.contains(&temporary) {
                paths.push(temporary);
            }
        }
        paths
    }
}

/// Managed MCP runtime over host-selected persistence implementations.
pub struct ManagedMcp<S: StateStore, C: CredentialStore> {
    state: Arc<S>,
    credentials: Arc<C>,
    policy: ConnectionPolicy,
    name_policy: CatalogNamePolicy,
    oauth: Arc<OAuthCoordinator>,
    statuses: RwLock<Vec<StatusRecord>>,
    config_lock: Mutex<()>,
    catalog_lock: Mutex<()>,
    background: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl<S: StateStore, C: CredentialStore> std::fmt::Debug for ManagedMcp<S, C> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedMcp")
            .field("policy", &self.policy)
            .field("name_policy", &self.name_policy)
            .finish_non_exhaustive()
    }
}

impl<S: StateStore, C: CredentialStore> ManagedMcp<S, C> {
    /// Construct from arbitrary async stores and prune stale catalog rows.
    pub async fn new(
        state: S,
        credentials: C,
        policy: ConnectionPolicy,
    ) -> Result<Self, ManagedError> {
        let state = Arc::new(state);
        let parsed = state.load_config().await?;
        let config = parsed.config;
        let credentials = Arc::new(credentials);
        let mut catalog = state.load_catalog().await?;
        if parsed.rejected.is_empty() && catalog.retain_current(&config) {
            // Cleanup is hygiene, not a boot prerequisite. A stale or
            // malformed cache already contributes no current tools.
            let _ = state.save_catalog(&catalog).await;
        }
        Ok(Self::from_loaded(
            state,
            credentials,
            policy,
            &config,
            &catalog,
        ))
    }

    fn from_loaded(
        state: Arc<S>,
        credentials: Arc<C>,
        policy: ConnectionPolicy,
        config: &Config,
        catalog: &CatalogCache,
    ) -> Self {
        let statuses = statuses_from_catalog(config, catalog);
        Self {
            state,
            credentials,
            policy,
            name_policy: CatalogNamePolicy::default(),
            oauth: Arc::new(OAuthCoordinator::new()),
            statuses: RwLock::new(statuses),
            config_lock: Mutex::new(()),
            catalog_lock: Mutex::new(()),
            background: std::sync::Mutex::new(Vec::new()),
        }
    }

    pub fn state_store(&self) -> Arc<S> {
        self.state.clone()
    }

    pub fn credential_store(&self) -> Arc<C> {
        self.credentials.clone()
    }

    pub fn connection_policy(&self) -> &ConnectionPolicy {
        &self.policy
    }

    /// Select the deterministic name mapping used for future enumerations.
    ///
    /// Existing rows remain verbatim unless the host explicitly calls
    /// [`requalify_catalog`](Self::requalify_catalog).
    pub fn with_catalog_name_policy(mut self, policy: CatalogNamePolicy) -> Self {
        self.name_policy = policy;
        self
    }

    pub fn catalog_name_policy(&self) -> &CatalogNamePolicy {
        &self.name_policy
    }

    /// Rewrite every current derived-catalog tool name through the selected
    /// name policy, without dialing a server or changing its cached schema.
    ///
    /// This is an explicit migration seam for hosts changing a public tool
    /// name contract. It is idempotent, prunes stale catalog identities using
    /// the same config snapshot, and returns the number of renamed entries.
    pub async fn requalify_catalog(&self) -> Result<usize, ManagedError> {
        let _config_guard = self.config_lock.lock().await;
        let config = require_clean_config(self.state.load_config().await?)?;
        let _catalog_guard = self.catalog_lock.lock().await;
        let mut catalog = self.state.load_catalog().await?;
        let pruned = catalog.retain_current(&config);
        let mut renamed = 0;
        for entry in &mut catalog.entries {
            let qualified = self
                .name_policy
                .qualified_name(&entry.server, &entry.tool_name);
            if entry.qualified_name != qualified {
                entry.qualified_name = qualified;
                renamed += 1;
            }
        }
        if pruned || renamed > 0 {
            self.state.save_catalog(&catalog).await?;
        }
        Ok(renamed)
    }

    pub fn oauth_coordinator(&self) -> Arc<OAuthCoordinator> {
        self.oauth.clone()
    }

    /// Create a flow store bound to one exact durable server definition.
    ///
    /// This low-level entry point is useful for custom OAuth coordination.
    /// Prefer [`authenticate_registered_with`](Self::authenticate_registered_with)
    /// when the manager should take and enforce the configuration snapshot.
    pub fn oauth_store(
        &self,
        server: impl Into<String>,
        binding: CredentialBinding,
    ) -> C::FlowStore {
        self.credentials.clone().oauth_flow(server.into(), binding)
    }

    pub fn cancel_authentication(&self, server: &str) {
        self.oauth.cancel_interactive_authentication(server);
    }

    pub async fn shutdown(&self) {
        let tasks = {
            let mut tasks = self
                .background
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            tasks.drain(..).collect::<Vec<_>>()
        };
        for task in &tasks {
            task.abort();
        }
        for task in tasks {
            let _ = task.await;
        }
        self.oauth.shutdown().await;
    }

    pub async fn config(&self) -> Result<ParsedConfig, ManagedError> {
        Ok(self.state.load_config().await?)
    }

    /// Explicitly bind URL-only credentials to the current durable server
    /// definitions.
    ///
    /// This is an opt-in data migration. Ordinary bearer reads and candidate
    /// probes are strict and never mutate credentials. Hosts should call this
    /// only when adopting fingerprint scoping for an existing credential
    /// store whose prior rows intentionally lacked a fingerprint.
    pub async fn claim_unscoped_credentials(&self) -> Result<usize, ManagedError> {
        let _config_guard = self.config_lock.lock().await;
        let config = require_clean_config(self.state.load_config().await?)?;
        let mut claimed = 0;
        for (server, definition) in &config.mcp_servers {
            if let Some(binding) = credential_binding(definition)
                && self.credentials.claim_unscoped(server, &binding).await?
            {
                claimed += 1;
            }
        }
        Ok(claimed)
    }

    pub async fn status(&self) -> Result<Vec<ServerStatus>, ManagedError> {
        let config = self.state.load_config().await?.config;
        let records = self.statuses.read().await;
        Ok(config
            .mcp_servers
            .iter()
            .map(|(server, definition)| {
                let fingerprint = definition.fingerprint();
                match records.iter().find(|record| {
                    record.server == *server && record.config_fingerprint == fingerprint
                }) {
                    Some(record) => ServerStatus {
                        server: server.clone(),
                        state: record.state,
                        tool_count: record.tool_count,
                        error: record.error.clone(),
                    },
                    None => ServerStatus {
                        server: server.clone(),
                        state: ServerState::Pending,
                        tool_count: 0,
                        error: None,
                    },
                }
            })
            .collect())
    }

    pub async fn auth_status(&self) -> Result<Vec<AuthStatus>, ManagedError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs_f64())
            .unwrap_or(0.0);
        let config = self.state.load_config().await?.config;
        let mut statuses = Vec::new();
        for (server, definition) in &config.mcp_servers {
            let ServerConfig::Remote(remote) = definition else {
                continue;
            };
            if !remote.oauth_enabled() {
                continue;
            }
            let binding = credential_binding(definition)
                .expect("remote definitions always have a credential binding");
            statuses.push(AuthStatus {
                server: server.clone(),
                state: self.credentials.state_at(server, &binding, now).await?,
            });
        }
        Ok(statuses)
    }

    pub async fn catalog_snapshot(&self) -> Result<CatalogSnapshot, ManagedError> {
        let config = self.state.load_config().await?.config;
        let catalog = self.state.load_catalog().await?;
        let servers = config
            .mcp_servers
            .iter()
            .filter(|(server, definition)| catalog.server_is_current(server, definition))
            .map(|(server, definition)| CatalogServerSnapshot {
                server: server.clone(),
                config: definition.clone(),
                config_fingerprint: definition.fingerprint(),
                tools: catalog
                    .entries
                    .iter()
                    .filter(|entry| entry.server == *server)
                    .cloned()
                    .collect(),
            })
            .collect();
        Ok(CatalogSnapshot { servers })
    }

    /// Connectivity check with no persistence mutation.
    pub async fn probe(
        &self,
        server: &str,
        definition: &ServerConfig,
    ) -> Result<ProbeResult, ManagedError> {
        let bearer = self.bearer_for(server, definition).await?;
        Ok(
            match enumerate(server, definition, bearer.clone(), &self.policy).await {
                Ok(tools) => ProbeResult::Reachable { tools },
                Err(error) => ProbeResult::Failed {
                    needs_auth: definition
                        .remote()
                        .is_some_and(|remote| remote.oauth_enabled())
                        && error.needs_auth(),
                    error: connection::redact_error_message(
                        definition,
                        &bearer.iter().cloned().collect::<Vec<_>>(),
                        &error.to_string(),
                    ),
                },
            },
        )
    }

    pub async fn probe_registered(&self, server: &str) -> Result<ProbeResult, ManagedError> {
        let config = self.state.load_config().await?.config;
        let definition = config
            .mcp_servers
            .get(server)
            .ok_or_else(|| ManagedError::UnknownServer(server.to_string()))?;
        self.probe(server, definition).await
    }

    pub async fn upsert(
        &self,
        server: impl Into<String>,
        definition: ServerConfig,
    ) -> Result<UpsertResult, ManagedError> {
        definition
            .validate()
            .map_err(|reason| ManagedError::InvalidConfig(reason.to_string()))?;
        let server = server.into();
        let overwritten = {
            let _config_guard = self.config_lock.lock().await;
            let mut config = require_clean_config(self.state.load_config().await?)?;
            let overwritten = config.mcp_servers.contains_key(&server);
            let definition_changed = config
                .mcp_servers
                .get(&server)
                .is_none_or(|current| current.fingerprint() != definition.fingerprint());
            if definition_changed {
                self.oauth.cancel_interactive_authentication(&server);
                // Credentials are deleted before config mutation so a failed
                // save or crash cannot leave an orphan secret that an
                // identical later definition silently reuses. A save failure
                // deliberately leaves the visible old server needing auth.
                self.credentials.remove(&server).await?;
            }
            config
                .mcp_servers
                .insert(server.clone(), definition.clone());
            self.state.save_config(&config).await?;

            let _catalog_guard = self.catalog_lock.lock().await;
            let mut catalog = self.state.load_catalog().await?;
            catalog.retain_current(&config);
            catalog.remove_server(&server);
            self.state.save_catalog(&catalog).await?;
            self.statuses
                .write()
                .await
                .retain(|status| status.server != server);
            overwritten
        };

        let refresh = self.refresh_definition(&server, &definition).await?;
        Ok(UpsertResult {
            overwritten,
            refresh,
        })
    }

    /// Remove credentials first, then durable config and cached tools.
    ///
    /// Security takes precedence over availability here: a config-save
    /// failure may leave the visible server requiring reauthentication, but
    /// a crash or credential-delete failure can never leave an orphan token
    /// that an identical later definition silently reuses.
    pub async fn remove(&self, server: &str) -> Result<bool, ManagedError> {
        let _config_guard = self.config_lock.lock().await;
        let mut config = require_clean_config(self.state.load_config().await?)?;
        self.oauth.cancel_interactive_authentication(server);
        self.credentials.remove(server).await?;
        let removed = config.mcp_servers.shift_remove(server).is_some();
        self.state.save_config(&config).await?;

        let _catalog_guard = self.catalog_lock.lock().await;
        let mut catalog = self.state.load_catalog().await?;
        catalog.retain_current(&config);
        catalog.remove_server(server);
        self.state.save_catalog(&catalog).await?;
        self.statuses
            .write()
            .await
            .retain(|status| status.server != server);
        Ok(removed)
    }

    pub async fn refresh(&self, server: &str) -> Result<RefreshOutcome, ManagedError> {
        let config = self.state.load_config().await?.config;
        let definition = config
            .mcp_servers
            .get(server)
            .cloned()
            .ok_or_else(|| ManagedError::UnknownServer(server.to_string()))?;
        self.refresh_definition(server, &definition).await
    }

    async fn refresh_definition(
        &self,
        server: &str,
        definition: &ServerConfig,
    ) -> Result<RefreshOutcome, ManagedError> {
        let bearer = self.bearer_for(server, definition).await?;
        match enumerate(server, definition, bearer.clone(), &self.policy).await {
            Ok(specs) => {
                let tool_count = specs.len();
                if self.commit_enumeration(server, definition, specs).await? {
                    Ok(RefreshOutcome::Cached { tool_count })
                } else {
                    Ok(RefreshOutcome::Stale)
                }
            }
            Err(error) => {
                let needs_auth = definition
                    .remote()
                    .is_some_and(|remote| remote.oauth_enabled())
                    && error.needs_auth();
                let message = connection::redact_error_message(
                    definition,
                    &bearer.iter().cloned().collect::<Vec<_>>(),
                    &error.to_string(),
                );
                if self
                    .mark_failure(server, definition, &message, needs_auth)
                    .await?
                {
                    Ok(RefreshOutcome::Failed {
                        error: message,
                        needs_auth,
                    })
                } else {
                    Ok(RefreshOutcome::Stale)
                }
            }
        }
    }

    /// Commit externally enumerated specs (for example, the result of an
    /// interactive OAuth flow) only if the definition is still current.
    pub async fn commit_enumeration(
        &self,
        server: &str,
        enumerated_definition: &ServerConfig,
        specs: Vec<CachedToolSpec>,
    ) -> Result<bool, ManagedError> {
        crate::validate_cached_catalog(server, &specs)?;
        let entries = entries_from_specs_with(server, specs, |server, tool| {
            self.name_policy.qualified_name(server, tool)
        });
        let tool_count = entries.len() as u64;
        let _config_guard = self.config_lock.lock().await;
        let config = require_clean_config(self.state.load_config().await?)?;
        let Some(current) = config.mcp_servers.get(server) else {
            return Ok(false);
        };
        if current.fingerprint() != enumerated_definition.fingerprint() {
            return Ok(false);
        }

        let _catalog_guard = self.catalog_lock.lock().await;
        let mut catalog = self.state.load_catalog().await?;
        catalog.retain_current(&config);
        catalog.replace_server(server, enumerated_definition, entries);
        self.state.save_catalog(&catalog).await?;
        self.set_status(StatusRecord {
            server: server.to_string(),
            config_fingerprint: enumerated_definition.fingerprint(),
            state: ServerState::Cached,
            tool_count,
            error: None,
        })
        .await;
        Ok(true)
    }

    async fn mark_failure(
        &self,
        server: &str,
        attempted_definition: &ServerConfig,
        error: &str,
        needs_auth: bool,
    ) -> Result<bool, ManagedError> {
        let _config_guard = self.config_lock.lock().await;
        let config = self.state.load_config().await?.config;
        if config
            .mcp_servers
            .get(server)
            .is_none_or(|current| current.fingerprint() != attempted_definition.fingerprint())
        {
            return Ok(false);
        }
        self.set_status(StatusRecord {
            server: server.to_string(),
            config_fingerprint: attempted_definition.fingerprint(),
            state: if needs_auth {
                ServerState::NeedsAuth
            } else {
                ServerState::Failed
            },
            tool_count: 0,
            error: Some(error.to_string()),
        })
        .await;
        Ok(true)
    }

    async fn set_status(&self, record: StatusRecord) {
        let mut statuses = self.statuses.write().await;
        statuses.retain(|status| status.server != record.server);
        statuses.push(record);
    }

    /// Enumerate every configured server lacking a current catalog identity.
    pub async fn backfill(&self) -> Result<Vec<RefreshReport>, ManagedError> {
        let config = self.state.load_config().await?.config;
        let catalog = self.state.load_catalog().await?;
        let known = catalog.current_server_names(&config);
        let mut reports = Vec::new();
        for (server, definition) in &config.mcp_servers {
            if known.contains(server) {
                continue;
            }
            reports.push(RefreshReport {
                server: server.clone(),
                outcome: self.refresh_definition(server, definition).await?,
            });
        }
        Ok(reports)
    }

    /// Start an offline-first backfill owned by this manager.
    ///
    /// [`shutdown`](Self::shutdown) aborts and joins the task. The returned
    /// handle permits an individual cancellation; call [`backfill`](Self::backfill)
    /// directly when the result report is needed.
    pub fn spawn_backfill(self: &Arc<Self>) -> tokio::task::AbortHandle {
        self.spawn_backfill_with(|_| {})
    }

    /// Start an offline-first backfill and observe its terminal report.
    ///
    /// The observer runs once inside the owned task unless the task is
    /// cancelled. This keeps error/reporting policy in the host without
    /// requiring it to own a second task lifecycle.
    pub fn spawn_backfill_with<F>(self: &Arc<Self>, observer: F) -> tokio::task::AbortHandle
    where
        F: FnOnce(Result<Vec<RefreshReport>, ManagedError>) + Send + 'static,
    {
        let managed = self.clone();
        let task = tokio::spawn(async move {
            observer(managed.backfill().await);
        });
        let abort = task.abort_handle();
        self.background
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(task);
        abort
    }

    async fn bearer_for(
        &self,
        server: &str,
        definition: &ServerConfig,
    ) -> Result<Option<String>, ManagedError> {
        match definition {
            ServerConfig::Remote(remote) if remote.oauth_enabled() => {
                let binding = credential_binding(definition)
                    .expect("remote definitions always have a credential binding");
                Ok(self.credentials.bearer(server, &binding).await?)
            }
            ServerConfig::Remote(_) | ServerConfig::Stdio(_) => Ok(None),
        }
    }

    pub async fn dialers(&self) -> Result<HashMap<String, McpDialer>, ManagedError> {
        let config = self.state.load_config().await?.config;
        let mut dialers = HashMap::new();
        for (server, definition) in &config.mcp_servers {
            let bearer = self.bearer_for(server, definition).await?;
            dialers.insert(
                server.clone(),
                dialer(server, definition, bearer, &self.policy),
            );
        }
        Ok(dialers)
    }

    pub async fn register_cached(
        &self,
        registry: &mut ToolRegistry,
        options: &RegisterOptions,
    ) -> Result<Vec<CatalogRegistration>, ManagedError> {
        self.register_cached_with(registry, options, |_, _| {})
            .await
    }

    /// Register current catalog entries without dialing. `transform` may
    /// decorate the presentation description only; names, schemas,
    /// capabilities, and transport identity remain catalog-owned.
    pub async fn register_cached_with<F>(
        &self,
        registry: &mut ToolRegistry,
        options: &RegisterOptions,
        transform: F,
    ) -> Result<Vec<CatalogRegistration>, ManagedError>
    where
        F: FnMut(&CatalogEntry, &mut String) + Send,
    {
        let snapshot = self.catalog_snapshot().await?;
        self.register_snapshot_with(&snapshot, registry, options, transform)
            .await
    }

    /// Register one caller-held catalog snapshot without reloading config or
    /// catalog state.
    pub async fn register_snapshot(
        &self,
        snapshot: &CatalogSnapshot,
        registry: &mut ToolRegistry,
        options: &RegisterOptions,
    ) -> Result<Vec<CatalogRegistration>, ManagedError> {
        self.register_snapshot_with(snapshot, registry, options, |_, _| {})
            .await
    }

    /// [`register_snapshot`](Self::register_snapshot) with a host description
    /// projection applied to every cached spec.
    pub async fn register_snapshot_with<F>(
        &self,
        snapshot: &CatalogSnapshot,
        registry: &mut ToolRegistry,
        options: &RegisterOptions,
        mut transform: F,
    ) -> Result<Vec<CatalogRegistration>, ManagedError>
    where
        F: FnMut(&CatalogEntry, &mut String) + Send,
    {
        Ok(self
            .mount_snapshot_inner(snapshot, registry, options, &mut transform, false)
            .await?
            .registrations)
    }

    /// Mount current cached tools plus AC's stock `tool_search`.
    ///
    /// The returned names are exactly the successfully registered latent
    /// tools. The host passes that set to its generic
    /// `ac_runtime::ConditionalToolsHook`; no MCP-specific hook or search
    /// choreography belongs in the embedding.
    pub async fn mount_cached(
        &self,
        registry: &mut ToolRegistry,
        options: &RegisterOptions,
    ) -> Result<MountResult, ManagedError> {
        self.mount_cached_with(registry, options, |_, _| {}).await
    }

    /// [`mount_cached`](Self::mount_cached) with the same host projection hook
    /// as [`register_cached_with`](Self::register_cached_with). Search entries
    /// use the transformed descriptions, so presentation context is
    /// consistent between discovery and invocation.
    pub async fn mount_cached_with<F>(
        &self,
        registry: &mut ToolRegistry,
        options: &RegisterOptions,
        transform: F,
    ) -> Result<MountResult, ManagedError>
    where
        F: FnMut(&CatalogEntry, &mut String) + Send,
    {
        let snapshot = self.catalog_snapshot().await?;
        self.mount_snapshot_with(&snapshot, registry, options, transform)
            .await
    }

    /// Mount one caller-held catalog snapshot plus AC's stock `tool_search`
    /// without reloading config or catalog state.
    pub async fn mount_snapshot(
        &self,
        snapshot: &CatalogSnapshot,
        registry: &mut ToolRegistry,
        options: &RegisterOptions,
    ) -> Result<MountResult, ManagedError> {
        self.mount_snapshot_with(snapshot, registry, options, |_, _| {})
            .await
    }

    /// [`mount_snapshot`](Self::mount_snapshot) with a host projection hook.
    pub async fn mount_snapshot_with<F>(
        &self,
        snapshot: &CatalogSnapshot,
        registry: &mut ToolRegistry,
        options: &RegisterOptions,
        mut transform: F,
    ) -> Result<MountResult, ManagedError>
    where
        F: FnMut(&CatalogEntry, &mut String) + Send,
    {
        self.mount_snapshot_inner(snapshot, registry, options, &mut transform, true)
            .await
    }

    async fn mount_snapshot_inner(
        &self,
        snapshot: &CatalogSnapshot,
        registry: &mut ToolRegistry,
        options: &RegisterOptions,
        transform: &mut (dyn FnMut(&CatalogEntry, &mut String) + Send),
        install_search: bool,
    ) -> Result<MountResult, ManagedError> {
        let mut registrations = Vec::new();
        let mut gated_names = BTreeSet::new();
        let mut search_entries = HashMap::new();
        let mut search_order = Vec::new();
        for mounted in &snapshot.servers {
            let bearer = self.bearer_for(&mounted.server, &mounted.config).await?;
            let dialer = dialer(&mounted.server, &mounted.config, bearer, &self.policy);
            let mut server_search_entries = HashMap::new();
            let specs: Vec<_> = mounted
                .tools
                .iter()
                .map(|entry| {
                    let mut spec = entry.as_cached_spec();
                    let mut description = spec.description.take().unwrap_or_default();
                    transform(entry, &mut description);
                    spec.description = Some(description);
                    if install_search {
                        let registry_name = spec
                            .registry_name
                            .clone()
                            .unwrap_or_else(|| entry.qualified_name.clone());
                        server_search_entries
                            .entry(registry_name.clone())
                            .or_insert_with(|| {
                                ac_tools::ToolSearchEntry::new(
                                    registry_name,
                                    spec.description.clone().unwrap_or_default(),
                                )
                                .with_keywords(format!("{} {}", entry.server, entry.tool_name))
                            });
                    }
                    spec
                })
                .collect();
            let tools = register_cached_tools(
                registry,
                &connection_name(&mounted.server),
                &specs,
                dialer,
                options,
            )?;
            for name in &tools.registered {
                if let Some(entry) = server_search_entries.remove(name)
                    && !search_entries.contains_key(name)
                {
                    search_order.push(name.clone());
                    search_entries.insert(name.clone(), entry);
                }
            }
            gated_names.extend(tools.registered.iter().cloned());
            registrations.push(CatalogRegistration {
                server: mounted.server.clone(),
                tools,
            });
        }
        let search = if !install_search {
            SearchMount::NotRequested
        } else if gated_names.is_empty() {
            SearchMount::EmptyCatalog
        } else if registry.contains(ac_tools::TOOL_SEARCH_NAME) {
            SearchMount::SkippedCollision
        } else {
            let catalog = search_order
                .into_iter()
                .filter(|name| gated_names.contains(name))
                .filter_map(|name| search_entries.remove(&name))
                .collect::<Vec<_>>();
            registry.register(ac_tools::ToolSearch::new(Arc::new(catalog)));
            SearchMount::Installed
        };
        Ok(MountResult {
            gated_names,
            registrations,
            search,
        })
    }

    /// Low-level coordinator access for a custom enumerator.
    ///
    /// This method deliberately does not attach the result to the managed
    /// catalog because AC cannot prove a caller-supplied enumerator used the
    /// registered definition. Prefer [`authenticate_registered`](Self::authenticate_registered).
    pub async fn coordinate_authentication(
        &self,
        server: &str,
        flow: &InteractiveOAuthConfig,
        enumerator: &dyn OAuthEnumerator<CachedToolSpec>,
        on_open_url: &(dyn Fn(String) + Send + Sync),
        cancel: &CancellationToken,
    ) -> Result<Vec<CachedToolSpec>, String> {
        let (definition, store) = self
            .authentication_snapshot(server)
            .await
            .map_err(|error| error.to_string())?;
        let Some(remote) = definition.remote() else {
            return Err(format!("MCP server '{server}' is not a remote server"));
        };
        if flow.server_url != remote.url {
            return Err(format!(
                "MCP server '{server}' changed before authentication started"
            ));
        }
        self.oauth
            .authenticate_interactive(server, flow, &store, enumerator, on_open_url, cancel)
            .await
            .map_err(|message| connection::redact_error_message(&definition, &[], &message))
    }

    /// Interactive OAuth using this manager's one config-to-connection path.
    ///
    /// The caller supplies only host policy/presentation in `flow`; stored
    /// bearer probing, authenticated re-enumeration, connection timeout, and
    /// catalog commit are all the same mechanisms used by probe/refresh and
    /// lazy tool calls.
    pub async fn authenticate_configured(
        &self,
        server: &str,
        flow: &InteractiveOAuthConfig,
        on_open_url: &(dyn Fn(String) + Send + Sync),
        cancel: &CancellationToken,
    ) -> Result<AuthenticationResult, String> {
        let host = OAuthHostPolicy {
            redirect_uri: flow.redirect_uri.clone(),
            discovery_metadata: flow.discovery_metadata.clone(),
            registration_metadata: flow.registration_metadata.clone(),
            endpoint_policy: flow.endpoint_policy.clone(),
            page_copy: flow.page_copy.clone(),
        };
        self.authenticate_registered(server, &host, on_open_url, cancel)
            .await
            .map_err(|error| error.to_string())
    }

    /// Preferred high-level authentication entry point. Only host identity,
    /// callback presentation, and the resolved redirect URI are injected;
    /// every security-bearing OAuth field comes from the registered server.
    pub async fn authenticate_registered(
        &self,
        server: &str,
        host: &OAuthHostPolicy,
        on_open_url: &(dyn Fn(String) + Send + Sync),
        cancel: &CancellationToken,
    ) -> Result<AuthenticationResult, AuthenticationError> {
        let (definition, store) = self.authentication_snapshot(server).await?;
        self.authenticate_definition(server, definition, store, host, on_open_url, cancel)
            .await
    }

    /// Authenticate one registered server while deriving host policy from
    /// the same exact definition snapshot used for OAuth and catalog commit.
    ///
    /// Use this when redirect selection or other presentation policy depends
    /// on the remote definition. It prevents a concurrent config edit from
    /// pairing policy derived from one definition with credentials for
    /// another.
    pub async fn authenticate_registered_with<F>(
        &self,
        server: &str,
        host_policy: F,
        on_open_url: &(dyn Fn(String) + Send + Sync),
        cancel: &CancellationToken,
    ) -> Result<AuthenticationResult, AuthenticationError>
    where
        F: FnOnce(&RemoteConfig) -> OAuthHostPolicy + Send,
    {
        let (definition, store) = self.authentication_snapshot(server).await?;
        let ServerConfig::Remote(remote) = &definition else {
            return Err(AuthenticationError::NotRemote(server.to_string()));
        };
        let host = host_policy(remote);
        self.authenticate_definition(server, definition, store, &host, on_open_url, cancel)
            .await
    }

    /// Atomically capture a current definition and the credential generation
    /// authorized to act for it. Mutations use the same config lock, so a
    /// remove/overwrite either precedes this snapshot or invalidates its flow
    /// store before any later commit.
    async fn authentication_snapshot(
        &self,
        server: &str,
    ) -> Result<(ServerConfig, C::FlowStore), AuthenticationError> {
        let _config_guard = self.config_lock.lock().await;
        let definition =
            require_clean_config(self.state.load_config().await.map_err(ManagedError::from)?)?
                .mcp_servers
                .get(server)
                .cloned()
                .ok_or_else(|| AuthenticationError::UnknownServer(server.to_string()))?;
        let ServerConfig::Remote(remote) = &definition else {
            return Err(AuthenticationError::NotRemote(server.to_string()));
        };
        if !remote.oauth_enabled() {
            return Err(AuthenticationError::Disabled(server.to_string()));
        }
        let binding = credential_binding(&definition)
            .expect("remote definitions always have a credential binding");
        let store = self.oauth_store(server, binding);
        Ok((definition, store))
    }

    async fn authenticate_definition(
        &self,
        server: &str,
        definition: ServerConfig,
        store: C::FlowStore,
        host: &OAuthHostPolicy,
        on_open_url: &(dyn Fn(String) + Send + Sync),
        cancel: &CancellationToken,
    ) -> Result<AuthenticationResult, AuthenticationError> {
        let ServerConfig::Remote(remote) = &definition else {
            return Err(AuthenticationError::NotRemote(server.to_string()));
        };
        if !remote.oauth_enabled() {
            return Err(AuthenticationError::Disabled(server.to_string()));
        }
        let settings = remote.oauth_settings();
        let flow = InteractiveOAuthConfig {
            enabled: true,
            server_url: remote.url.clone(),
            redirect_uri: host.redirect_uri.clone(),
            scope: settings.and_then(|settings| settings.scope.clone()),
            client_id: settings.and_then(|settings| settings.client_id.clone()),
            client_secret: settings.and_then(|settings| settings.client_secret.clone()),
            endpoint_policy: host.endpoint_policy.clone(),
            discovery_metadata: host.discovery_metadata.clone(),
            registration_metadata: host.registration_metadata.clone(),
            page_copy: host.page_copy.clone(),
        };
        let enumerator = ConfiguredOAuthEnumerator {
            server: server.to_string(),
            definition: definition.clone(),
            policy: self.policy.clone(),
        };
        let specs = self
            .oauth
            .authenticate_interactive(server, &flow, &store, &enumerator, on_open_url, cancel)
            .await
            .map_err(|message| {
                AuthenticationError::Flow(connection::redact_error_message(
                    &definition,
                    &[],
                    &message,
                ))
            })?;
        let tool_count = specs.len();
        if !self.commit_enumeration(server, &definition, specs).await? {
            return Err(AuthenticationError::Changed(server.to_string()));
        }
        Ok(AuthenticationResult { tool_count })
    }
}

struct ConfiguredOAuthEnumerator {
    server: String,
    definition: ServerConfig,
    policy: ConnectionPolicy,
}

impl OAuthEnumerator<CachedToolSpec> for ConfiguredOAuthEnumerator {
    fn enumerate(
        &self,
        bearer: Option<String>,
    ) -> futures::future::BoxFuture<'_, Result<Vec<CachedToolSpec>, OAuthEnumerateError>> {
        Box::pin(async move {
            enumerate(&self.server, &self.definition, bearer.clone(), &self.policy)
                .await
                .map_err(|error| OAuthEnumerateError {
                    needs_auth: error.needs_auth(),
                    message: connection::redact_error_message(
                        &self.definition,
                        &bearer.iter().cloned().collect::<Vec<_>>(),
                        &error.to_string(),
                    ),
                })
        })
    }
}

impl ManagedMcp<FileStateStore, FileCredentialStore> {
    /// Synchronous boot constructor for the stock file stores.
    ///
    /// Reads are small local control files and tolerant; operational network
    /// and process work remains async. This avoids requiring an embedding
    /// already inside Tokio to manufacture a nested executor during startup.
    pub fn open(paths: ManagedPaths, policy: ConnectionPolicy) -> Self {
        let state = Arc::new(FileStateStore::new(&paths.config, &paths.catalog));
        let credentials = Arc::new(FileCredentialStore::new(&paths.credentials));
        let parsed = state.load_config_sync();
        let config = parsed.config;
        let mut catalog = state.load_catalog_sync();
        if parsed.rejected.is_empty() && catalog.retain_current(&config) {
            // Best effort: an unwritable stale cache must not prevent boot.
            let _ = state.save_catalog_sync(&catalog);
        }
        Self::from_loaded(state, credentials, policy, &config, &catalog)
    }

    pub fn paths(&self) -> ManagedPaths {
        ManagedPaths {
            config: self.state.config_path().to_path_buf(),
            catalog: self.state.catalog_path().to_path_buf(),
            credentials: self.credentials.path().to_path_buf(),
        }
    }

    /// Synchronous stock-file variant of
    /// [`claim_unscoped_credentials`](Self::claim_unscoped_credentials).
    ///
    /// Call immediately after [`open`](Self::open), before sharing the
    /// manager with concurrent tasks.
    pub fn claim_unscoped_credentials_sync(&self) -> Result<usize, ManagedError> {
        let config = require_clean_config(self.state.load_config_sync())?;
        let mut claimed = 0;
        for (server, definition) in &config.mcp_servers {
            if let Some(binding) = credential_binding(definition)
                && self
                    .credentials
                    .claim_unscoped_for_binding(server, &binding)
                    .map_err(StoreError::from)?
            {
                claimed += 1;
            }
        }
        Ok(claimed)
    }
}

fn statuses_from_catalog(config: &Config, catalog: &CatalogCache) -> Vec<StatusRecord> {
    let counts = catalog
        .entries
        .iter()
        .fold(HashMap::<&str, u64>::new(), |mut counts, entry| {
            *counts.entry(entry.server.as_str()).or_insert(0) += 1;
            counts
        });
    config
        .mcp_servers
        .iter()
        .filter(|(server, definition)| catalog.server_is_current(server, definition))
        .map(|(server, definition)| StatusRecord {
            server: server.clone(),
            config_fingerprint: definition.fingerprint(),
            state: ServerState::Cached,
            tool_count: counts.get(server.as_str()).copied().unwrap_or(0),
            error: None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use futures::future::BoxFuture;
    use serde_json::json;

    use super::*;
    use crate::oauth::{ClientCredentials, OAuthFlowStore, OAuthTokens};

    fn paths(directory: &tempfile::TempDir) -> ManagedPaths {
        ManagedPaths::new(
            directory.path().join("mcp.json"),
            directory.path().join("mcp-catalog.json"),
            directory.path().join("mcp-auth.json"),
        )
    }

    fn missing_server() -> ServerConfig {
        ServerConfig::Stdio(StdioConfig {
            command: "/definitely/not/an/mcp/server".into(),
            args: None,
            env: Some(BTreeMap::from([("KEY".into(), "value".into())])),
            env_vars: None,
            cwd: None,
        })
    }

    fn oauth_server(scope: &str) -> ServerConfig {
        ServerConfig::Remote(RemoteConfig {
            url: "not-an-http-url".into(),
            headers: None,
            env_headers: None,
            bearer_token_env_var: None,
            oauth: Some(OAuthMode::Settings(OAuthSettings {
                scope: Some(scope.into()),
                ..Default::default()
            })),
        })
    }

    fn tokens(access_token: &str) -> OAuthTokens {
        OAuthTokens {
            access_token: access_token.into(),
            refresh_token: None,
            expires_at: None,
            scope: None,
        }
    }

    fn client_credentials() -> ClientCredentials {
        ClientCredentials {
            client_id: "client".into(),
            client_secret: None,
            client_id_issued_at: None,
            client_secret_expires_at: None,
            from_registration: false,
        }
    }

    struct FailingSaveState {
        config: Config,
    }

    impl StateStore for FailingSaveState {
        fn load_config(&self) -> BoxFuture<'_, Result<ParsedConfig, StoreError>> {
            let config = self.config.clone();
            Box::pin(async move {
                Ok(ParsedConfig {
                    config,
                    rejected: Vec::new(),
                })
            })
        }

        fn save_config<'a>(&'a self, _config: &'a Config) -> BoxFuture<'a, Result<(), StoreError>> {
            Box::pin(async { Err(StoreError::other("synthetic config write failure")) })
        }

        fn load_catalog(&self) -> BoxFuture<'_, Result<CatalogCache, StoreError>> {
            Box::pin(async { Ok(CatalogCache::default()) })
        }

        fn save_catalog<'a>(
            &'a self,
            _catalog: &'a CatalogCache,
        ) -> BoxFuture<'a, Result<(), StoreError>> {
            Box::pin(async { Ok(()) })
        }
    }

    #[test]
    fn managed_paths_include_deduplicated_private_temp_directories() {
        let directory = tempfile::tempdir().unwrap();
        let paths = paths(&directory);
        let controls = paths.control_paths();
        assert_eq!(controls.len(), 4);
        assert!(controls.contains(&paths.config));
        assert!(controls.contains(&paths.catalog));
        assert!(controls.contains(&paths.credentials));
        assert!(controls.contains(&directory.path().join(".ac-mcp-tmp")));
    }

    #[tokio::test]
    async fn upsert_failure_is_saved_and_statused_then_remove_cleans_it() {
        let directory = tempfile::tempdir().unwrap();
        let managed = ManagedMcp::open(paths(&directory), ConnectionPolicy::default());
        let added = managed.upsert("broken", missing_server()).await.unwrap();
        assert!(!added.overwritten);
        assert!(matches!(
            added.refresh,
            RefreshOutcome::Failed {
                needs_auth: false,
                ..
            }
        ));
        assert_eq!(
            managed.status().await.unwrap()[0].state,
            ServerState::Failed
        );
        assert!(managed.remove("broken").await.unwrap());
        assert!(managed.status().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn probe_refresh_and_status_never_surface_the_configured_url() {
        let directory = tempfile::tempdir().unwrap();
        let managed = ManagedMcp::open(paths(&directory), ConnectionPolicy::default());
        let configured_url = "not-http?token=url-secret";
        let definition = ServerConfig::Remote(RemoteConfig {
            url: configured_url.into(),
            headers: Some(BTreeMap::from([(
                "Authorization".into(),
                "Bearer header-secret".into(),
            )])),
            env_headers: None,
            bearer_token_env_var: None,
            oauth: None,
        });

        let probe = managed.probe("private-remote", &definition).await.unwrap();
        let ProbeResult::Failed {
            error: probe_error, ..
        } = probe
        else {
            panic!("invalid URL must fail probing");
        };
        assert!(!probe_error.contains(configured_url), "{probe_error}");
        assert!(!probe_error.contains("url-secret"), "{probe_error}");
        assert!(!probe_error.contains("header-secret"), "{probe_error}");

        let added = managed.upsert("private-remote", definition).await.unwrap();
        let RefreshOutcome::Failed {
            error: refresh_error,
            ..
        } = added.refresh
        else {
            panic!("invalid URL must fail refresh");
        };
        assert!(!refresh_error.contains(configured_url), "{refresh_error}");
        let status_error = managed.status().await.unwrap()[0].error.clone().unwrap();
        assert_eq!(status_error, refresh_error);
    }

    #[tokio::test]
    async fn commit_mount_and_search_are_connection_free() {
        let directory = tempfile::tempdir().unwrap();
        let paths = paths(&directory);
        let state = FileStateStore::new(&paths.config, &paths.catalog);
        let definition = missing_server();
        state
            .save_config_sync(&Config {
                mcp_servers: indexmap::IndexMap::from([("server".into(), definition.clone())]),
            })
            .unwrap();
        let managed = ManagedMcp::open(paths, ConnectionPolicy::default());
        assert!(
            managed
                .commit_enumeration(
                    "server",
                    &definition,
                    vec![CachedToolSpec {
                        name: "search".into(),
                        registry_name: None,
                        description: Some("Search records".into()),
                        input_schema: json!({ "type": "object" }),
                        read_only_hint: Some(true),
                    }],
                )
                .await
                .unwrap()
        );

        let snapshot = managed.catalog_snapshot().await.unwrap();
        assert_eq!(snapshot.search_metadata()[0].id, "mcp__server__search");
        state.save_config_sync(&Config::default()).unwrap();
        let mut registry = ToolRegistry::new();
        let mounted = managed
            .mount_snapshot(&snapshot, &mut registry, &RegisterOptions::default())
            .await
            .unwrap();
        assert_eq!(
            mounted.registrations[0].tools.registered,
            ["mcp__server__search"]
        );
        assert_eq!(
            mounted.gated_names,
            BTreeSet::from(["mcp__server__search".to_string()])
        );
        assert_eq!(mounted.search, SearchMount::Installed);
        assert!(registry.contains("mcp__server__search"));
        assert!(registry.contains(ac_tools::TOOL_SEARCH_NAME));

        let mut collision_registry = ToolRegistry::new();
        collision_registry.register(ac_tools::ToolSearch::new(Arc::new(Vec::new())));
        let collision = managed
            .mount_snapshot(
                &snapshot,
                &mut collision_registry,
                &RegisterOptions::default(),
            )
            .await
            .unwrap();
        assert_eq!(collision.search, SearchMount::SkippedCollision);
        assert!(collision_registry.contains("mcp__server__search"));
    }

    #[tokio::test]
    async fn custom_catalog_name_policy_feeds_the_single_commit_path() {
        let directory = tempfile::tempdir().unwrap();
        let paths = paths(&directory);
        let state = FileStateStore::new(&paths.config, &paths.catalog);
        let definition = missing_server();
        state
            .save_config_sync(&Config {
                mcp_servers: indexmap::IndexMap::from([("server".into(), definition.clone())]),
            })
            .unwrap();
        let managed = ManagedMcp::open(paths, ConnectionPolicy::default())
            .with_catalog_name_policy(CatalogNamePolicy::new(|server, tool| {
                format!("legacy__{server}__{tool}")
            }));
        assert!(
            managed
                .commit_enumeration(
                    "server",
                    &definition,
                    vec![CachedToolSpec {
                        name: "search".into(),
                        registry_name: None,
                        description: None,
                        input_schema: json!({ "type": "object" }),
                        read_only_hint: None,
                    }],
                )
                .await
                .unwrap()
        );
        assert_eq!(
            managed
                .catalog_snapshot()
                .await
                .unwrap()
                .entries()
                .next()
                .unwrap()
                .qualified_name,
            "legacy__server__search"
        );
    }

    #[tokio::test]
    async fn explicit_requalification_migrates_existing_catalog_names() {
        let directory = tempfile::tempdir().unwrap();
        let paths = paths(&directory);
        let state = FileStateStore::new(&paths.config, &paths.catalog);
        let definition = missing_server();
        state
            .save_config_sync(&Config {
                mcp_servers: indexmap::IndexMap::from([("server-name".into(), definition.clone())]),
            })
            .unwrap();
        let mut catalog = CatalogCache::default();
        catalog.replace_server(
            "server-name",
            &definition,
            vec![CatalogEntry {
                server: "server-name".into(),
                tool_name: "search/tool".into(),
                qualified_name: "legacy__server_name__search_tool".into(),
                description: "search".into(),
                input_schema: json!({ "type": "object" }),
                read_only_hint: Some(true),
            }],
        );
        state.save_catalog_sync(&catalog).unwrap();

        let managed = ManagedMcp::open(paths, ConnectionPolicy::default());
        assert_eq!(managed.requalify_catalog().await.unwrap(), 1);
        let entry = managed
            .catalog_snapshot()
            .await
            .unwrap()
            .entries()
            .next()
            .unwrap()
            .clone();
        assert_eq!(
            entry.qualified_name,
            qualified_tool_name("server-name", "search/tool")
        );
        assert_eq!(entry.description, "search");
        assert_eq!(entry.read_only_hint, Some(true));
        assert_eq!(managed.requalify_catalog().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn stale_enumeration_cannot_attach_to_an_overwrite() {
        let directory = tempfile::tempdir().unwrap();
        let paths = paths(&directory);
        let state = FileStateStore::new(&paths.config, &paths.catalog);
        let old = ServerConfig::Stdio(StdioConfig {
            command: "old".into(),
            args: None,
            env: None,
            env_vars: None,
            cwd: None,
        });
        let new = ServerConfig::Stdio(StdioConfig {
            command: "new".into(),
            args: None,
            env: None,
            env_vars: None,
            cwd: None,
        });
        state
            .save_config_sync(&Config {
                mcp_servers: indexmap::IndexMap::from([("server".into(), new)]),
            })
            .unwrap();
        let managed = ManagedMcp::open(paths, ConnectionPolicy::default());
        assert!(
            !managed
                .commit_enumeration("server", &old, Vec::new())
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn stale_refresh_failure_is_reported_as_stale() {
        let directory = tempfile::tempdir().unwrap();
        let paths = paths(&directory);
        let state = FileStateStore::new(&paths.config, &paths.catalog);
        let old = oauth_server("old");
        let new = oauth_server("new");
        state
            .save_config_sync(&Config {
                mcp_servers: indexmap::IndexMap::from([("server".into(), new)]),
            })
            .unwrap();
        let managed = ManagedMcp::open(paths, ConnectionPolicy::default());
        assert_eq!(
            managed.refresh_definition("server", &old).await.unwrap(),
            RefreshOutcome::Stale
        );
    }

    #[tokio::test]
    async fn direct_config_edit_cannot_rebind_a_stale_oauth_commit() {
        let directory = tempfile::tempdir().unwrap();
        let paths = paths(&directory);
        let state = FileStateStore::new(&paths.config, &paths.catalog);
        let old = oauth_server("old");
        state
            .save_config_sync(&Config {
                mcp_servers: indexmap::IndexMap::from([("server".into(), old.clone())]),
            })
            .unwrap();
        let managed = ManagedMcp::open(paths, ConnectionPolicy::default());
        let old_binding = CredentialBinding::new("not-an-http-url", old.fingerprint());
        let stale = managed.oauth_store("server", old_binding.clone());

        let new = oauth_server("new");
        state
            .save_config_sync(&Config {
                mcp_servers: indexmap::IndexMap::from([("server".into(), new.clone())]),
            })
            .unwrap();
        stale
            .commit(
                "not-an-http-url",
                &tokens("old-token"),
                &client_credentials(),
            )
            .await
            .unwrap();

        assert_eq!(
            managed
                .credential_store()
                .bearer("server", &old_binding)
                .await
                .unwrap()
                .as_deref(),
            Some("old-token")
        );
        let new_binding = CredentialBinding::new("not-an-http-url", new.fingerprint());
        assert!(
            managed
                .credential_store()
                .bearer("server", &new_binding)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            managed.auth_status().await.unwrap()[0].state,
            CredentialState::NeedsAuth
        );
    }

    #[tokio::test]
    async fn explicit_unscoped_claim_is_durable_and_candidate_probe_cannot_steal_it() {
        let directory = tempfile::tempdir().unwrap();
        let paths = paths(&directory);
        let state = FileStateStore::new(&paths.config, &paths.catalog);
        let current = oauth_server("current");
        state
            .save_config_sync(&Config {
                mcp_servers: indexmap::IndexMap::from([("server".into(), current.clone())]),
            })
            .unwrap();
        std::fs::write(
            &paths.credentials,
            r#"{"server":{"serverUrl":"not-an-http-url","tokens":{"accessToken":"legacy"}}}"#,
        )
        .unwrap();

        let managed = ManagedMcp::open(paths, ConnectionPolicy::default());
        assert_eq!(
            managed.auth_status().await.unwrap()[0].state,
            CredentialState::NeedsAuth
        );
        let candidate = oauth_server("candidate");
        assert!(matches!(
            managed.probe("server", &candidate).await.unwrap(),
            ProbeResult::Failed { .. }
        ));
        assert_eq!(
            managed
                .credential_store()
                .get("server")
                .unwrap()
                .config_fingerprint
                .as_deref(),
            None
        );
        assert_eq!(managed.claim_unscoped_credentials_sync().unwrap(), 1);
        let current_fingerprint = current.fingerprint();
        assert_eq!(
            managed
                .credential_store()
                .get("server")
                .unwrap()
                .config_fingerprint
                .as_deref(),
            Some(current_fingerprint.as_str())
        );
        assert_eq!(
            managed.auth_status().await.unwrap()[0].state,
            CredentialState::Authenticated
        );
    }

    #[tokio::test]
    async fn changed_upsert_invalidates_an_in_flight_oauth_generation() {
        let directory = tempfile::tempdir().unwrap();
        let paths = paths(&directory);
        let state = FileStateStore::new(&paths.config, &paths.catalog);
        let old = oauth_server("old");
        state
            .save_config_sync(&Config {
                mcp_servers: indexmap::IndexMap::from([("server".into(), old.clone())]),
            })
            .unwrap();
        let managed = ManagedMcp::open(paths, ConnectionPolicy::default());
        let stale = managed.oauth_store(
            "server",
            CredentialBinding::new("not-an-http-url", old.fingerprint()),
        );

        let result = managed.upsert("server", oauth_server("new")).await.unwrap();
        assert!(result.overwritten);
        assert!(
            stale
                .persist_state("must-not-return")
                .await
                .unwrap_err()
                .is_superseded()
        );
        assert!(managed.credential_store().get("server").is_none());
    }

    #[tokio::test]
    async fn auth_snapshot_captured_before_remove_cannot_commit_an_orphan() {
        let directory = tempfile::tempdir().unwrap();
        let paths = paths(&directory);
        let state = FileStateStore::new(&paths.config, &paths.catalog);
        let definition = oauth_server("old");
        state
            .save_config_sync(&Config {
                mcp_servers: indexmap::IndexMap::from([("server".into(), definition)]),
            })
            .unwrap();
        let managed = ManagedMcp::open(paths, ConnectionPolicy::default());
        let (_, stale) = managed.authentication_snapshot("server").await.unwrap();

        assert!(managed.remove("server").await.unwrap());
        assert!(
            stale
                .commit(
                    "not-an-http-url",
                    &tokens("must-not-return"),
                    &client_credentials(),
                )
                .await
                .unwrap_err()
                .is_superseded()
        );
        assert!(managed.credential_store().get("server").is_none());
    }

    #[tokio::test]
    async fn changed_upsert_deletes_credentials_even_if_config_save_fails() {
        let directory = tempfile::tempdir().unwrap();
        let credentials_path = directory.path().join("mcp-auth.json");
        let old = oauth_server("old");
        let old_binding = CredentialBinding::new("not-an-http-url", old.fingerprint());
        std::fs::write(
            &credentials_path,
            format!(
                r#"{{"server":{{"serverUrl":"not-an-http-url","configFingerprint":"{}","tokens":{{"accessToken":"old-token"}}}}}}"#,
                old_binding.config_fingerprint
            ),
        )
        .unwrap();
        let managed = ManagedMcp::new(
            FailingSaveState {
                config: Config {
                    mcp_servers: indexmap::IndexMap::from([("server".into(), old)]),
                },
            },
            FileCredentialStore::new(&credentials_path),
            ConnectionPolicy::default(),
        )
        .await
        .unwrap();

        let error = managed
            .upsert("server", oauth_server("new"))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("synthetic config write failure"));
        assert!(managed.credential_store().get("server").is_none());
        assert_eq!(
            managed.auth_status().await.unwrap()[0].state,
            CredentialState::NeedsAuth
        );
    }

    #[tokio::test]
    async fn malformed_registry_blocks_mutations_without_wiping_files() {
        let directory = tempfile::tempdir().unwrap();
        let paths = paths(&directory);
        let malformed = b"{ definitely not valid json";
        std::fs::write(&paths.config, malformed).unwrap();
        let credentials = br#"{"server":{"serverUrl":"not-an-http-url","configFingerprint":"fp","tokens":{"accessToken":"secret"}}}"#;
        std::fs::write(&paths.credentials, credentials).unwrap();
        let managed = ManagedMcp::open(paths.clone(), ConnectionPolicy::default());

        assert!(matches!(
            managed.upsert("server", oauth_server("new")).await,
            Err(ManagedError::RejectedConfig(_))
        ));
        assert_eq!(std::fs::read(&paths.config).unwrap(), malformed);
        assert_eq!(std::fs::read(&paths.credentials).unwrap(), credentials);

        assert!(matches!(
            managed.remove("server").await,
            Err(ManagedError::RejectedConfig(_))
        ));
        assert_eq!(std::fs::read(&paths.config).unwrap(), malformed);
        assert_eq!(std::fs::read(&paths.credentials).unwrap(), credentials);
    }

    #[tokio::test]
    async fn high_level_authentication_honors_the_durable_disabled_policy() {
        let directory = tempfile::tempdir().unwrap();
        let paths = paths(&directory);
        FileStateStore::new(&paths.config, &paths.catalog)
            .save_config_sync(&Config {
                mcp_servers: indexmap::IndexMap::from([(
                    "remote".into(),
                    ServerConfig::Remote(RemoteConfig {
                        url: "https://example.test/mcp".into(),
                        headers: None,
                        env_headers: None,
                        bearer_token_env_var: None,
                        oauth: Some(OAuthMode::Disabled),
                    }),
                )]),
            })
            .unwrap();
        let managed = ManagedMcp::open(paths, ConnectionPolicy::default());
        let host = OAuthHostPolicy {
            redirect_uri: "http://127.0.0.1:12345/callback".into(),
            discovery_metadata: ClientMetadata::new("test", "1"),
            registration_metadata: ClientMetadata::new("test", "1"),
            endpoint_policy: OAuthEndpointPolicy::default(),
            page_copy: PageCopy {
                success_title: "done".into(),
                success_body: "done".into(),
                error_title: "error".into(),
            },
        };
        let error = managed
            .authenticate_registered("remote", &host, &|_| {}, &CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(error, AuthenticationError::Disabled(server) if server == "remote"));
    }

    #[tokio::test]
    async fn host_policy_factory_and_authentication_share_one_definition_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        let paths = paths(&directory);
        let state = FileStateStore::new(&paths.config, &paths.catalog);
        let old = ServerConfig::Remote(RemoteConfig {
            url: "https://old.example.test/mcp".into(),
            headers: None,
            env_headers: None,
            bearer_token_env_var: None,
            oauth: Some(OAuthMode::Settings(OAuthSettings::default())),
        });
        state
            .save_config_sync(&Config {
                mcp_servers: indexmap::IndexMap::from([("remote".into(), old)]),
            })
            .unwrap();
        let managed = ManagedMcp::open(paths, ConnectionPolicy::default());
        let replacement = oauth_server("new");
        let cancel = CancellationToken::new();
        cancel.cancel();
        let error = managed
            .authenticate_registered_with(
                "remote",
                |remote| {
                    assert_eq!(remote.url, "https://old.example.test/mcp");
                    state
                        .save_config_sync(&Config {
                            mcp_servers: indexmap::IndexMap::from([("remote".into(), replacement)]),
                        })
                        .unwrap();
                    OAuthHostPolicy {
                        redirect_uri: "http://127.0.0.1:12345/callback".into(),
                        discovery_metadata: ClientMetadata::new("test", "1"),
                        registration_metadata: ClientMetadata::new("test", "1"),
                        endpoint_policy: OAuthEndpointPolicy::default(),
                        page_copy: PageCopy {
                            success_title: "done".into(),
                            success_body: "done".into(),
                            error_title: "error".into(),
                        },
                    }
                },
                &|_| {},
                &cancel,
            )
            .await
            .unwrap_err();
        assert!(matches!(error, AuthenticationError::Flow(_)));
    }
}
