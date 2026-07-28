use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use serde_json::{Map, Value, json};

use super::store::{StoreError, write_private_atomic};
use crate::oauth::{ClientCredentials, OAuthFlowStore, OAuthStoreError, OAuthTokens};

#[derive(Clone, Default, PartialEq)]
pub struct TokenRecord {
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    pub expires_at: Option<f64>,
    pub scope: Option<String>,
}

impl std::fmt::Debug for TokenRecord {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TokenRecord")
            .field(
                "access_token",
                &self.access_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("expires_at", &self.expires_at)
            .field("scope", &self.scope)
            .finish()
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CredentialEntry {
    pub tokens: Option<TokenRecord>,
    pub server_url: Option<String>,
    pub config_fingerprint: Option<String>,
}

/// Exact durable server identity a credential may authorize.
#[derive(Clone, PartialEq, Eq)]
pub struct CredentialBinding {
    pub server_url: String,
    pub config_fingerprint: String,
}

impl CredentialBinding {
    pub fn new(server_url: impl Into<String>, config_fingerprint: impl Into<String>) -> Self {
        Self {
            server_url: server_url.into(),
            config_fingerprint: config_fingerprint.into(),
        }
    }
}

impl std::fmt::Debug for CredentialBinding {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CredentialBinding")
            .field("server_url", &"[configured]")
            .field("config_fingerprint", &self.config_fingerprint)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialState {
    NeedsAuth,
    Authenticated,
    Expired,
}

/// Semantic credential seam used by the managed control plane.
pub trait CredentialStore: Send + Sync + 'static {
    type FlowStore: OAuthFlowStore + Send + Sync + 'static;

    /// Claim an unscoped URL-only entry for one durable current definition.
    ///
    /// A host may call this explicitly against registered configuration when
    /// adopting fingerprint scoping. Candidate probes, ordinary startup, and
    /// stale snapshots must remain read-only and never call it.
    fn claim_unscoped<'a>(
        &'a self,
        _server: &'a str,
        _binding: &'a CredentialBinding,
    ) -> BoxFuture<'a, Result<bool, StoreError>> {
        Box::pin(async { Ok(false) })
    }

    fn bearer<'a>(
        &'a self,
        server: &'a str,
        binding: &'a CredentialBinding,
    ) -> BoxFuture<'a, Result<Option<String>, StoreError>>;

    fn state_at<'a>(
        &'a self,
        server: &'a str,
        binding: &'a CredentialBinding,
        now_epoch_seconds: f64,
    ) -> BoxFuture<'a, Result<CredentialState, StoreError>>;

    /// Forget credentials and invalidate every flow store created before this
    /// mutation, even when no file entry currently exists.
    fn remove<'a>(&'a self, server: &'a str) -> BoxFuture<'a, Result<(), StoreError>>;

    /// Bind one OAuth flow to the current per-server generation.
    fn oauth_flow(self: Arc<Self>, server: String, binding: CredentialBinding) -> Self::FlowStore;
}

/// Stock atomic JSON credential store (mode `0600` on Unix).
///
/// Construct one instance per path. Its lock serializes all read-modify-write
/// operations and owns generation invalidation for interactive flows.
pub struct FileCredentialStore {
    path: PathBuf,
    mutation_state: Arc<Mutex<HashMap<String, u64>>>,
}

impl std::fmt::Debug for FileCredentialStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FileCredentialStore")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl FileCredentialStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            mutation_state: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Exposed so a host can put the secret file on its sandbox deny-read
    /// list.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Missing, unreadable, malformed, or non-object files read as empty.
    pub fn read_all(&self) -> Map<String, Value> {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return Map::new();
        };
        match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(entries)) => entries,
            _ => Map::new(),
        }
    }

    fn read_all_for_update(&self) -> io::Result<Map<String, Value>> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Map::new()),
            Err(error) => return Err(error),
        };
        match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(entries)) => Ok(entries),
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "MCP credential store must contain a JSON object",
            )),
            Err(error) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid MCP credential-store JSON: {error}"),
            )),
        }
    }

    pub fn get(&self, server: &str) -> Option<CredentialEntry> {
        self.read_all().get(server).map(entry_view)
    }

    pub fn get_for_binding(
        &self,
        server: &str,
        binding: &CredentialBinding,
    ) -> Option<CredentialEntry> {
        self.get(server).filter(|entry| {
            entry.server_url.as_deref() == Some(binding.server_url.as_str())
                && entry.config_fingerprint.as_deref() == Some(binding.config_fingerprint.as_str())
        })
    }

    /// Claim an unscoped URL-only entry once under the same lock used by
    /// removals and OAuth commits.
    pub(super) fn claim_unscoped_for_binding(
        &self,
        server: &str,
        binding: &CredentialBinding,
    ) -> io::Result<bool> {
        let _state = self
            .mutation_state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut entries = self.read_all_for_update()?;
        let Some(raw) = entries.get(server) else {
            return Ok(false);
        };
        let entry = entry_view(raw);
        if entry.server_url.as_deref() != Some(binding.server_url.as_str()) {
            return Ok(false);
        }
        if entry.config_fingerprint.is_some() {
            return Ok(false);
        }

        let Some(Value::Object(mut raw_entry)) = entries.remove(server) else {
            return Ok(false);
        };
        raw_entry.insert(
            "configFingerprint".to_string(),
            json!(binding.config_fingerprint),
        );
        entries.insert(server.to_string(), Value::Object(raw_entry));
        self.write_all(entries)?;
        Ok(true)
    }

    pub fn bearer_for(&self, server: &str, binding: &CredentialBinding) -> Option<String> {
        self.get_for_binding(server, binding)
            .and_then(|entry| entry.tokens)
            .and_then(|tokens| tokens.access_token)
    }

    pub fn state_for(
        &self,
        server: &str,
        binding: &CredentialBinding,
        now_epoch_seconds: f64,
    ) -> CredentialState {
        match self
            .get_for_binding(server, binding)
            .and_then(|entry| entry.tokens)
            .and_then(|tokens| tokens.access_token.is_some().then_some(tokens))
        {
            None => CredentialState::NeedsAuth,
            Some(tokens)
                if tokens
                    .expires_at
                    .is_some_and(|expires_at| expires_at < now_epoch_seconds) =>
            {
                CredentialState::Expired
            }
            Some(_) => CredentialState::Authenticated,
        }
    }

    pub fn authorization_generation(&self, server: &str) -> u64 {
        *self
            .mutation_state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(server)
            .unwrap_or(&0)
    }

    fn write_all(&self, entries: Map<String, Value>) -> io::Result<()> {
        let mut text =
            serde_json::to_string_pretty(&Value::Object(entries)).expect("credentials serialize");
        text.push('\n');
        write_private_atomic(&self.path, text.as_bytes())
    }

    fn generation_error(server: &str) -> io::Error {
        io::Error::new(
            io::ErrorKind::Interrupted,
            format!("OAuth authorization for `{server}` was superseded"),
        )
    }

    fn update_field_at_generation(
        &self,
        server: &str,
        generation: u64,
        field: &str,
        value: Value,
        server_url: Option<&str>,
    ) -> io::Result<()> {
        let state = self
            .mutation_state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if state.get(server).copied().unwrap_or(0) != generation {
            return Err(Self::generation_error(server));
        }
        let mut entries = self.read_all_for_update()?;
        let mut entry = match entries.get(server) {
            Some(Value::Object(entry)) => entry.clone(),
            _ => Map::new(),
        };
        entry.insert(field.to_string(), value);
        if let Some(server_url) = server_url {
            entry.insert("serverUrl".to_string(), json!(server_url));
        }
        entries.insert(server.to_string(), Value::Object(entry));
        self.write_all(entries)
    }

    fn oauth_state(&self, server: &str) -> Option<String> {
        self.read_all()
            .get(server)?
            .get("oauthState")?
            .as_str()
            .map(str::to_string)
    }

    fn clear_pending_at_generation(&self, server: &str, generation: u64) -> io::Result<()> {
        let state = self
            .mutation_state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if state.get(server).copied().unwrap_or(0) != generation {
            return Err(Self::generation_error(server));
        }
        let mut entries = self.read_all_for_update()?;
        let Some(Value::Object(existing)) = entries.get(server) else {
            return Ok(());
        };
        if !existing.contains_key("oauthState") && !existing.contains_key("codeVerifier") {
            return Ok(());
        }
        let mut entry = existing.clone();
        entry.remove("oauthState");
        entry.remove("codeVerifier");
        entries.insert(server.to_string(), Value::Object(entry));
        self.write_all(entries)
    }

    fn commit_at_generation(
        &self,
        server: &str,
        generation: u64,
        server_url: &str,
        config_fingerprint: &str,
        tokens: &OAuthTokens,
        client_info: &ClientCredentials,
    ) -> io::Result<()> {
        let state = self
            .mutation_state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if state.get(server).copied().unwrap_or(0) != generation {
            return Err(Self::generation_error(server));
        }

        let mut token_object = Map::new();
        token_object.insert("accessToken".into(), json!(tokens.access_token));
        if let Some(refresh_token) = &tokens.refresh_token {
            token_object.insert("refreshToken".into(), json!(refresh_token));
        }
        if let Some(expires_at) = tokens.expires_at {
            token_object.insert("expiresAt".into(), json!(expires_at));
        }
        if let Some(scope) = &tokens.scope {
            token_object.insert("scope".into(), json!(scope));
        }

        let mut entry = Map::new();
        entry.insert("tokens".into(), Value::Object(token_object));
        if client_info.from_registration {
            let mut client = Map::new();
            client.insert("clientId".into(), json!(client_info.client_id));
            if let Some(client_secret) = &client_info.client_secret {
                client.insert("clientSecret".into(), json!(client_secret));
            }
            if let Some(issued_at) = client_info.client_id_issued_at {
                client.insert("clientIdIssuedAt".into(), json!(issued_at));
            }
            if let Some(expires_at) = client_info.client_secret_expires_at {
                client.insert("clientSecretExpiresAt".into(), json!(expires_at));
            }
            entry.insert("clientInfo".into(), Value::Object(client));
        }
        entry.insert("serverUrl".into(), json!(server_url));
        entry.insert("configFingerprint".into(), json!(config_fingerprint));

        let mut entries = self.read_all_for_update()?;
        entries.insert(server.to_string(), Value::Object(entry));
        self.write_all(entries)
    }

    fn remove_sync(&self, server: &str) -> io::Result<()> {
        let mut state = self
            .mutation_state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let generation = state.entry(server.to_string()).or_insert(0);
        *generation = generation.wrapping_add(1);
        let mut entries = self.read_all_for_update()?;
        if entries.remove(server).is_none() {
            return Ok(());
        }
        self.write_all(entries)
    }
}

fn non_empty_string(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn entry_view(raw: &Value) -> CredentialEntry {
    let tokens = raw
        .get("tokens")
        .and_then(Value::as_object)
        .map(|tokens| TokenRecord {
            access_token: non_empty_string(tokens.get("accessToken")),
            refresh_token: non_empty_string(tokens.get("refreshToken")),
            expires_at: tokens.get("expiresAt").and_then(Value::as_f64),
            scope: non_empty_string(tokens.get("scope")),
        });
    CredentialEntry {
        tokens,
        server_url: raw
            .get("serverUrl")
            .and_then(Value::as_str)
            .map(str::to_string),
        config_fingerprint: raw
            .get("configFingerprint")
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

fn task_error(error: tokio::task::JoinError) -> StoreError {
    StoreError::Task(format!("MCP credential-store task failed: {error}"))
}

impl CredentialStore for FileCredentialStore {
    type FlowStore = FileOAuthFlowStore;

    fn claim_unscoped<'a>(
        &'a self,
        server: &'a str,
        binding: &'a CredentialBinding,
    ) -> BoxFuture<'a, Result<bool, StoreError>> {
        let server = server.to_string();
        let binding = binding.clone();
        let store = FileCredentialStore {
            path: self.path.clone(),
            mutation_state: self.mutation_state.clone(),
        };
        Box::pin(async move {
            tokio::task::spawn_blocking(move || store.claim_unscoped_for_binding(&server, &binding))
                .await
                .map_err(task_error)?
                .map_err(StoreError::from)
        })
    }

    fn bearer<'a>(
        &'a self,
        server: &'a str,
        binding: &'a CredentialBinding,
    ) -> BoxFuture<'a, Result<Option<String>, StoreError>> {
        let server = server.to_string();
        let binding = binding.clone();
        let path = self.path.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                FileCredentialStore::new(path).bearer_for(&server, &binding)
            })
            .await
            .map_err(task_error)
        })
    }

    fn state_at<'a>(
        &'a self,
        server: &'a str,
        binding: &'a CredentialBinding,
        now_epoch_seconds: f64,
    ) -> BoxFuture<'a, Result<CredentialState, StoreError>> {
        let server = server.to_string();
        let binding = binding.clone();
        let path = self.path.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                FileCredentialStore::new(path).state_for(&server, &binding, now_epoch_seconds)
            })
            .await
            .map_err(task_error)
        })
    }

    fn remove<'a>(&'a self, server: &'a str) -> BoxFuture<'a, Result<(), StoreError>> {
        let server = server.to_string();
        let store = FileCredentialStore {
            path: self.path.clone(),
            mutation_state: self.mutation_state.clone(),
        };
        Box::pin(async move {
            tokio::task::spawn_blocking(move || store.remove_sync(&server))
                .await
                .map_err(task_error)?
                .map_err(StoreError::from)
        })
    }

    fn oauth_flow(self: Arc<Self>, server: String, binding: CredentialBinding) -> Self::FlowStore {
        let generation = self.authorization_generation(&server);
        FileOAuthFlowStore {
            credentials: self,
            server,
            binding,
            generation,
        }
    }
}

/// Generation-bound adapter for [`crate::oauth::OAuthCoordinator`].
pub struct FileOAuthFlowStore {
    credentials: Arc<FileCredentialStore>,
    server: String,
    binding: CredentialBinding,
    generation: u64,
}

fn oauth_store_error(error: io::Error) -> OAuthStoreError {
    if error.kind() == io::ErrorKind::Interrupted {
        OAuthStoreError::superseded(error.to_string())
    } else {
        OAuthStoreError::failed(error.to_string())
    }
}

fn oauth_task_error(error: tokio::task::JoinError) -> OAuthStoreError {
    OAuthStoreError::failed(format!("OAuth credential-store task failed: {error}"))
}

impl OAuthFlowStore for FileOAuthFlowStore {
    fn bearer<'a>(
        &'a self,
        server_url: &'a str,
    ) -> BoxFuture<'a, Result<Option<String>, OAuthStoreError>> {
        if server_url != self.binding.server_url {
            return Box::pin(async { Ok(None) });
        }
        let credentials = self.credentials.clone();
        let server = self.server.clone();
        let binding = self.binding.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || credentials.bearer_for(&server, &binding))
                .await
                .map_err(oauth_task_error)
        })
    }

    fn state(&self) -> BoxFuture<'_, Result<Option<String>, OAuthStoreError>> {
        let credentials = self.credentials.clone();
        let server = self.server.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || credentials.oauth_state(&server))
                .await
                .map_err(oauth_task_error)
        })
    }

    fn persist_state<'a>(&'a self, state: &'a str) -> BoxFuture<'a, Result<(), OAuthStoreError>> {
        let credentials = self.credentials.clone();
        let server = self.server.clone();
        let generation = self.generation;
        let state = state.to_string();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                credentials.update_field_at_generation(
                    &server,
                    generation,
                    "oauthState",
                    json!(state),
                    None,
                )
            })
            .await
            .map_err(oauth_task_error)?
            .map_err(oauth_store_error)
        })
    }

    fn persist_verifier<'a>(
        &'a self,
        verifier: &'a str,
    ) -> BoxFuture<'a, Result<(), OAuthStoreError>> {
        let credentials = self.credentials.clone();
        let server = self.server.clone();
        let generation = self.generation;
        let verifier = verifier.to_string();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                credentials.update_field_at_generation(
                    &server,
                    generation,
                    "codeVerifier",
                    json!(verifier),
                    None,
                )
            })
            .await
            .map_err(oauth_task_error)?
            .map_err(oauth_store_error)
        })
    }

    fn clear_pending(&self) -> BoxFuture<'_, Result<(), OAuthStoreError>> {
        let credentials = self.credentials.clone();
        let server = self.server.clone();
        let generation = self.generation;
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                credentials.clear_pending_at_generation(&server, generation)
            })
            .await
            .map_err(oauth_task_error)?
            .map_err(oauth_store_error)
        })
    }

    fn commit<'a>(
        &'a self,
        server_url: &'a str,
        tokens: &'a OAuthTokens,
        credentials: &'a ClientCredentials,
    ) -> BoxFuture<'a, Result<(), OAuthStoreError>> {
        let store = self.credentials.clone();
        let server = self.server.clone();
        let generation = self.generation;
        let server_url = server_url.to_string();
        let binding = self.binding.clone();
        let tokens = tokens.clone();
        let credentials = credentials.clone();
        Box::pin(async move {
            if server_url != binding.server_url {
                return Err(OAuthStoreError::failed(
                    "OAuth server URL changed while authorization was in progress",
                ));
            }
            tokio::task::spawn_blocking(move || {
                store.commit_at_generation(
                    &server,
                    generation,
                    &server_url,
                    &binding.config_fingerprint,
                    &tokens,
                    &credentials,
                )
            })
            .await
            .map_err(oauth_task_error)?
            .map_err(oauth_store_error)
        })
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[tokio::test]
    async fn credentials_are_url_bound_generation_safe_and_private_on_unix() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mcp-auth.json");
        std::fs::write(
            &path,
            r#"{"server":{"serverUrl":"https://one.test","configFingerprint":"fp-one","tokens":{"accessToken":"secret"}}}"#,
        )
        .unwrap();
        let store = Arc::new(FileCredentialStore::new(&path));
        let binding = CredentialBinding::new("https://one.test", "fp-one");
        assert_eq!(
            store.bearer_for("server", &binding).as_deref(),
            Some("secret")
        );
        assert!(
            store
                .bearer_for(
                    "server",
                    &CredentialBinding::new("https://one.test", "fp-two")
                )
                .is_none()
        );

        let stale = store
            .clone()
            .oauth_flow("server".to_string(), binding.clone());
        store.remove("server").await.unwrap();
        assert!(
            stale
                .persist_state("must-not-return")
                .await
                .unwrap_err()
                .is_superseded()
        );
        assert!(store.get("server").is_none());

        let fresh = store.clone().oauth_flow("server".to_string(), binding);
        fresh.persist_state("fresh").await.unwrap();
        assert_eq!(fresh.state().await.unwrap().as_deref(), Some("fresh"));

        #[cfg(unix)]
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[tokio::test]
    async fn unscoped_url_only_credentials_are_claimed_once() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mcp-auth.json");
        std::fs::write(
            &path,
            r#"{"server":{"serverUrl":"https://one.test","tokens":{"accessToken":"legacy"},"unknown":"preserved"}}"#,
        )
        .unwrap();
        let store = FileCredentialStore::new(&path);
        let first = CredentialBinding::new("https://one.test", "fp-one");

        assert!(store.claim_unscoped("server", &first).await.unwrap());
        assert_eq!(
            store.bearer("server", &first).await.unwrap().as_deref(),
            Some("legacy")
        );
        let saved = store.read_all();
        assert_eq!(
            saved["server"]["configFingerprint"].as_str(),
            Some("fp-one")
        );
        assert_eq!(saved["server"]["unknown"].as_str(), Some("preserved"));

        let later = CredentialBinding::new("https://one.test", "fp-two");
        assert!(store.bearer("server", &later).await.unwrap().is_none());
        assert_eq!(
            store.state_at("server", &later, 0.0).await.unwrap(),
            CredentialState::NeedsAuth
        );
        assert_eq!(
            store.read_all()["server"]["configFingerprint"].as_str(),
            Some("fp-one")
        );
    }

    #[tokio::test]
    async fn token_object_without_an_access_token_needs_auth() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mcp-auth.json");
        std::fs::write(
            &path,
            r#"{"server":{"serverUrl":"https://one.test","configFingerprint":"fp-one","tokens":{}}}"#,
        )
        .unwrap();
        let store = FileCredentialStore::new(&path);
        let binding = CredentialBinding::new("https://one.test", "fp-one");
        assert!(store.bearer("server", &binding).await.unwrap().is_none());
        assert_eq!(
            store.state_at("server", &binding, 0.0).await.unwrap(),
            CredentialState::NeedsAuth
        );
    }

    #[tokio::test]
    async fn mutations_refuse_to_replace_a_malformed_store() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mcp-auth.json");
        let malformed = b"{ definitely not valid json";
        std::fs::write(&path, malformed).unwrap();
        let store = Arc::new(FileCredentialStore::new(&path));
        let binding = CredentialBinding::new("https://one.test", "fp-one");

        assert!(store.bearer("server", &binding).await.unwrap().is_none());
        let flow = store.clone().oauth_flow("server".to_string(), binding);
        assert!(flow.persist_state("state").await.is_err());
        assert!(store.remove("server").await.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), malformed);
    }
}
