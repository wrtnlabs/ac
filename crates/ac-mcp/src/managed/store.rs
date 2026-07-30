use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use futures::future::BoxFuture;
use serde::Serialize;
use serde_json::Value;

use super::catalog::{CatalogCache, CatalogEntry, CatalogServer};
use super::config::{Config, ParsedConfig, RawConfigFile, parse_server_entries};

#[derive(Debug)]
pub enum StoreError {
    Io(io::Error),
    Task(String),
    Other(String),
}

impl StoreError {
    pub fn other(message: impl Into<String>) -> Self {
        Self::Other(message.into())
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => error.fmt(formatter),
            Self::Task(message) | Self::Other(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Task(_) | Self::Other(_) => None,
        }
    }
}

impl From<io::Error> for StoreError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Persistence seam for the portable server registry and offline catalog.
pub trait StateStore: Send + Sync + 'static {
    fn load_config(&self) -> BoxFuture<'_, Result<ParsedConfig, StoreError>>;
    fn save_config<'a>(&'a self, config: &'a Config) -> BoxFuture<'a, Result<(), StoreError>>;
    fn load_catalog(&self) -> BoxFuture<'_, Result<CatalogCache, StoreError>>;
    fn save_catalog<'a>(
        &'a self,
        catalog: &'a CatalogCache,
    ) -> BoxFuture<'a, Result<(), StoreError>>;
}

/// Stock two-file state store.
#[derive(Debug, Clone)]
pub struct FileStateStore {
    config_path: PathBuf,
    catalog_path: PathBuf,
}

impl FileStateStore {
    pub fn new(config_path: impl Into<PathBuf>, catalog_path: impl Into<PathBuf>) -> Self {
        Self {
            config_path: config_path.into(),
            catalog_path: catalog_path.into(),
        }
    }

    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    pub fn catalog_path(&self) -> &Path {
        &self.catalog_path
    }

    /// Tolerant boot read. A missing file is empty; malformed or unreadable
    /// files produce a rejection row so read-modify-write callers can refuse
    /// destructive updates.
    pub fn load_config_sync(&self) -> ParsedConfig {
        let text = match std::fs::read_to_string(&self.config_path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return ParsedConfig::default();
            }
            Err(error) => {
                return ParsedConfig {
                    config: Config::default(),
                    rejected: vec![super::config::RejectedServer {
                        server: "*".to_string(),
                        reason: format!("could not read server registry: {error}"),
                    }],
                };
            }
        };
        match serde_json::from_str::<RawConfigFile>(&text) {
            Ok(raw) => parse_server_entries(raw.mcp_servers),
            Err(error) => ParsedConfig {
                config: Config::default(),
                rejected: vec![super::config::RejectedServer {
                    server: "*".to_string(),
                    reason: format!("invalid server registry: {error}"),
                }],
            },
        }
    }

    pub fn save_config_sync(&self, config: &Config) -> Result<(), StoreError> {
        let mut text =
            serde_json::to_string_pretty(config).expect("portable MCP server registry serializes");
        text.push('\n');
        write_private_atomic(&self.config_path, text.as_bytes()).map_err(StoreError::from)
    }

    /// Tolerant version-aware catalog read.
    pub fn load_catalog_sync(&self) -> CatalogCache {
        let Ok(text) = std::fs::read_to_string(&self.catalog_path) else {
            return CatalogCache::default();
        };
        let Ok(raw) = serde_json::from_str::<Value>(&text) else {
            return CatalogCache::default();
        };
        let entries = raw
            .get("entries")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|entry| serde_json::from_value::<CatalogEntry>(entry.clone()).ok())
            .collect();
        let servers = if raw.get("version").and_then(Value::as_u64) == Some(2) {
            raw.get("servers")
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
                .filter_map(|(name, identity)| {
                    serde_json::from_value::<CatalogServer>(identity.clone())
                        .ok()
                        .map(|identity| (name.clone(), identity))
                })
                .collect()
        } else {
            Default::default()
        };
        CatalogCache { servers, entries }
    }

    pub fn save_catalog_sync(&self, catalog: &CatalogCache) -> Result<(), StoreError> {
        #[derive(Serialize)]
        struct CatalogFile<'a> {
            version: u32,
            servers: &'a std::collections::BTreeMap<String, CatalogServer>,
            entries: &'a [CatalogEntry],
        }

        let file = CatalogFile {
            version: 2,
            servers: &catalog.servers,
            entries: &catalog.entries,
        };
        let text = serde_json::to_string_pretty(&file).expect("offline MCP catalog serializes");
        write_private_atomic(&self.catalog_path, text.as_bytes()).map_err(StoreError::from)
    }
}

fn task_error(error: tokio::task::JoinError) -> StoreError {
    StoreError::Task(format!("MCP state-store task failed: {error}"))
}

impl StateStore for FileStateStore {
    fn load_config(&self) -> BoxFuture<'_, Result<ParsedConfig, StoreError>> {
        let store = self.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || store.load_config_sync())
                .await
                .map_err(task_error)
        })
    }

    fn save_config<'a>(&'a self, config: &'a Config) -> BoxFuture<'a, Result<(), StoreError>> {
        let store = self.clone();
        let config = config.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || store.save_config_sync(&config))
                .await
                .map_err(task_error)?
        })
    }

    fn load_catalog(&self) -> BoxFuture<'_, Result<CatalogCache, StoreError>> {
        let store = self.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || store.load_catalog_sync())
                .await
                .map_err(task_error)
        })
    }

    fn save_catalog<'a>(
        &'a self,
        catalog: &'a CatalogCache,
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        let store = self.clone();
        let catalog = catalog.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || store.save_catalog_sync(&catalog))
                .await
                .map_err(task_error)?
        })
    }
}

pub(crate) fn write_private_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    use std::io::Write;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temp_directory = private_temp_dir(path);
    std::fs::create_dir_all(&temp_directory)?;
    let metadata = std::fs::symlink_metadata(&temp_directory)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "MCP private temporary path must be a directory, not a symlink",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temp_directory, std::fs::Permissions::from_mode(0o700))?;
    }
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    let temp = temp_directory.join(format!("{file_name}.tmp.{}", uuid::Uuid::new_v4()));

    #[cfg(unix)]
    let mut file = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?
    };
    #[cfg(not(unix))]
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;

    file.write_all(contents)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    // Persist both the contents and the final inode metadata before publishing
    // the temporary file through rename.
    file.sync_all()?;
    drop(file);

    if let Err(error) = replace_file_atomic(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(error);
    }

    sync_containing_directory(path)?;
    Ok(())
}

fn containing_directory(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

#[cfg(unix)]
fn sync_containing_directory(path: &Path) -> io::Result<()> {
    std::fs::File::open(containing_directory(path))?.sync_all()
}

#[cfg(windows)]
fn sync_containing_directory(_path: &Path) -> io::Result<()> {
    // `replace_file_atomic` uses `MOVEFILE_WRITE_THROUGH`, which is the
    // Windows durability primitive for the replacement.
    Ok(())
}

#[cfg(all(not(unix), not(windows)))]
fn sync_containing_directory(_path: &Path) -> io::Result<()> {
    // No portable directory-fsync primitive exists on other std targets.
    Ok(())
}

pub(super) fn private_temp_dir(path: &Path) -> PathBuf {
    containing_directory(path).join(".ac-mcp-tmp")
}

#[cfg(not(windows))]
fn replace_file_atomic(source: &Path, destination: &Path) -> io::Result<()> {
    std::fs::rename(source, destination)
}

#[cfg(windows)]
fn replace_file_atomic(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;

    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let source: Vec<u16> = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: both pointers reference NUL-terminated buffers that remain
    // alive for the call. MoveFileExW performs the replacement atomically
    // within one volume and REPLACE_EXISTING supplies Unix rename semantics.
    let replaced = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if replaced == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::managed::config::{ServerConfig, StdioConfig};
    use indexmap::IndexMap;

    #[test]
    fn containing_directory_handles_bare_relative_and_absolute_paths() {
        assert_eq!(containing_directory(Path::new("mcp.json")), Path::new("."));
        assert_eq!(
            containing_directory(Path::new("state/mcp.json")),
            Path::new("state")
        );

        #[cfg(unix)]
        assert_eq!(
            containing_directory(Path::new("/state/mcp.json")),
            Path::new("/state")
        );
    }

    #[test]
    fn files_are_tolerant_ordered_and_private_on_unix() {
        let directory = tempfile::tempdir().unwrap();
        let store = FileStateStore::new(
            directory.path().join("mcp.json"),
            directory.path().join("mcp-catalog.json"),
        );
        assert!(store.load_config_sync().config.mcp_servers.is_empty());

        std::fs::write(
            store.config_path(),
            r#"{"mcpServers":{"second":{"command":"b"},"first":{"command":"a"}}}"#,
        )
        .unwrap();
        let parsed = store.load_config_sync();
        assert_eq!(
            parsed.config.mcp_servers.keys().collect::<Vec<_>>(),
            ["second", "first"]
        );
        store.save_config_sync(&parsed.config).unwrap();
        let temp_directory = private_temp_dir(store.config_path());
        assert!(temp_directory.is_dir());
        assert!(std::fs::read_dir(&temp_directory).unwrap().next().is_none());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(store.config_path())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(&temp_directory)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }

        std::fs::write(store.catalog_path(), "{broken").unwrap();
        assert_eq!(store.load_catalog_sync(), CatalogCache::default());

        let config = Config {
            mcp_servers: IndexMap::from([(
                "one".into(),
                ServerConfig::Stdio(StdioConfig {
                    command: "command".into(),
                    args: None,
                    env: None,
                    env_vars: None,
                    cwd: None,
                }),
            )]),
        };
        store.save_config_sync(&config).unwrap();
        assert_eq!(store.load_config_sync().config, config);
    }
}
