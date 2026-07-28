//! Filesystem tools: `read_file`, `write_file`, `edit_file`, `list_files`.
//!
//! Every path first passes through the host [`PathPolicy`] (via `ctx.policy`);
//! these tools never touch a raw user path. `read_file` stamps the mtime it saw
//! into the per-run read-before-write ledger, and the write tools consult it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use ac_tool::{
    AuthorizedPath, Capability, FileSnapshot, FileTimeError, PolicyError, Tool, ToolCtx,
    ToolOutput, WriteObserver,
};
use futures::future::BoxFuture;
use serde::Deserialize;

use crate::edit_replace::{
    convert_to_line_ending, detect_line_ending, fuzzy_replace, normalize_line_endings,
};
use crate::rooted_fs::RootedPath;

/// Maximum bytes `read_file` returns; larger files are truncated with a note.
const READ_CAP: usize = 256 * 1024;

/// Render a resolved absolute path relative to the policy root for model-facing
/// output; falls back to the absolute path when it is not under the root.
fn rel(root: &Path, p: &Path) -> String {
    match p.strip_prefix(root) {
        Ok(r) if r.as_os_str().is_empty() => ".".to_string(),
        Ok(r) => r.display().to_string(),
        Err(_) => p.display().to_string(),
    }
}

fn mtime_of(meta: &std::fs::Metadata) -> Option<SystemTime> {
    meta.modified().ok()
}

fn mtime_ms(t: SystemTime) -> Option<u64> {
    t.duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
}

/// Full-precision milliseconds since the Unix epoch.
///
/// Some host protocols preserve fractional milliseconds while AC's stock tool
/// rounds to a `u64`; the mutation primitive carries both without making a
/// host repeat the write transaction.
pub fn mtime_ms_f64(t: SystemTime) -> f64 {
    match t.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => d.as_secs() as f64 * 1000.0 + f64::from(d.subsec_nanos()) / 1e6,
        Err(e) => {
            -(e.duration().as_secs() as f64 * 1000.0 + f64::from(e.duration().subsec_nanos()) / 1e6)
        }
    }
}

#[derive(Debug)]
pub enum FileMutationError {
    Io {
        operation: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    Policy(PolicyError),
    Freshness(FileTimeError),
    Observer(String),
}

impl std::fmt::Display for FileMutationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io {
                operation,
                path,
                source,
            } => write!(f, "cannot {operation} {}: {source}", path.display()),
            Self::Policy(error) => error.fmt(f),
            Self::Freshness(error) => error.fmt(f),
            Self::Observer(reason) => {
                write!(f, "write aborted: pre-overwrite snapshot failed: {reason}")
            }
        }
    }
}

impl std::error::Error for FileMutationError {}

#[derive(Debug, Clone)]
pub struct FileWriteResult {
    pub path: PathBuf,
    pub bytes: u64,
    pub mtime: SystemTime,
    pub mtime_ms: f64,
}

#[derive(Debug, Clone)]
pub enum FileCommit {
    Written(FileWriteResult),
    Conflict {
        path: PathBuf,
        expected_mtime_ms: f64,
        actual: Option<FileSnapshot>,
    },
}

#[derive(Debug, Clone, Copy)]
pub enum ExpectedMtime {
    /// Full-precision millisecond value.
    Exact(f64),
    /// Whole milliseconds as exposed by AC's stock `WriteFileInput`.
    Milliseconds(u64),
}

impl ExpectedMtime {
    fn value(self) -> f64 {
        match self {
            Self::Exact(value) => value,
            Self::Milliseconds(value) => value as f64,
        }
    }

    fn matches(self, actual: SystemTime) -> bool {
        match self {
            Self::Exact(expected) => mtime_ms_f64(actual) == expected,
            Self::Milliseconds(expected) => mtime_ms(actual) == Some(expected),
        }
    }
}

/// One AC-owned same-path file transaction.
///
/// The guard holds [`ToolCtx::locks`] from the first metadata read through the
/// eventual write. Host tools may inspect/transform the current bytes between
/// [`begin`](Self::begin) and [`commit`](Self::commit) without rebuilding the
/// lock, freshness, observer, mkdir, write, stat, and restamp sequence.
pub struct FileMutation {
    ctx: Arc<ToolCtx>,
    path: PathBuf,
    rooted: RootedPath,
    initial: Option<FileSnapshot>,
    _guard: tokio::sync::OwnedMutexGuard<()>,
}

impl FileMutation {
    pub async fn begin(ctx: Arc<ToolCtx>, path: PathBuf) -> Result<Self, FileMutationError> {
        let authorized = ctx
            .policy
            .authorize_write(&path)
            .map_err(FileMutationError::Policy)?;
        Self::begin_authorized(ctx, authorized).await
    }

    /// Begin a mutation from an authorization the host already obtained while
    /// applying domain-specific routing. This avoids resolving the pathname a
    /// second time (and potentially observing a different policy generation).
    pub async fn begin_authorized(
        ctx: Arc<ToolCtx>,
        authorized: AuthorizedPath,
    ) -> Result<Self, FileMutationError> {
        let path = authorized.path().to_path_buf();
        let guard = ctx.locks.lock(&path).await;
        let rooted = RootedPath::new(authorized);
        let initial = metadata_snapshot(&rooted)?;
        Ok(Self {
            ctx,
            path,
            rooted,
            initial,
            _guard: guard,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn initial(&self) -> Option<FileSnapshot> {
        self.initial
    }

    pub fn exists(&self) -> bool {
        self.initial.is_some()
    }

    pub fn assert_fresh(&self) -> Result<(), FileMutationError> {
        self.ctx
            .file_times
            .assert_write(&self.path, self.initial)
            .map_err(FileMutationError::Freshness)
    }

    pub async fn read(&self) -> Result<Vec<u8>, FileMutationError> {
        let file = self
            .rooted
            .open_read()
            .map_err(|source| FileMutationError::Io {
                operation: "read",
                path: self.path.clone(),
                source,
            })?;
        read_all(file)
            .await
            .map_err(|source| FileMutationError::Io {
                operation: "read",
                path: self.path.clone(),
                source,
            })
    }

    /// Commit the complete replacement bytes.
    ///
    /// When `expected_mtime_ms` is present its comparison happens before the
    /// final freshness assertion. This lets a host surface an optimistic
    /// editor conflict if the file changed after it performed an earlier
    /// `assert_fresh` + transform under the in-process lock.
    pub async fn commit(
        self,
        bytes: &[u8],
        expected_mtime_ms: Option<ExpectedMtime>,
    ) -> Result<FileCommit, FileMutationError> {
        let (file, current) = self.open_commit_target(expected_mtime_ms)?;
        if let Some(expected) = expected_mtime_ms {
            let matches = current
                .map(|snapshot| expected.matches(snapshot.mtime))
                .unwrap_or(false);
            if !matches {
                return Ok(FileCommit::Conflict {
                    path: self.path,
                    expected_mtime_ms: expected.value(),
                    actual: current,
                });
            }
        }

        self.ctx
            .file_times
            .assert_write(&self.path, current)
            .map_err(FileMutationError::Freshness)?;

        let file =
            file.expect("an absent commit target only survives to an expected-mtime conflict");
        let mut file = tokio::fs::File::from_std(file);
        if current.is_some() {
            use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};

            file.seek(std::io::SeekFrom::Start(0))
                .await
                .map_err(|source| FileMutationError::Io {
                    operation: "seek",
                    path: self.path.clone(),
                    source,
                })?;
            let mut prior = Vec::new();
            file.read_to_end(&mut prior)
                .await
                .map_err(|source| FileMutationError::Io {
                    operation: "read",
                    path: self.path.clone(),
                    source,
                })?;
            // Byte-identical rewrites do not create a history entry. Hosts
            // still get the normal write/result semantics.
            if prior != bytes
                && let Some(observer) = self.ctx.extensions.get::<Arc<dyn WriteObserver>>()
            {
                observer
                    .before_overwrite(&self.path, &prior)
                    .map_err(FileMutationError::Observer)?;
            }
        }

        use tokio::io::{AsyncSeekExt as _, AsyncWriteExt as _};

        file.set_len(0)
            .await
            .map_err(|source| FileMutationError::Io {
                operation: "truncate",
                path: self.path.clone(),
                source,
            })?;
        file.seek(std::io::SeekFrom::Start(0))
            .await
            .map_err(|source| FileMutationError::Io {
                operation: "seek",
                path: self.path.clone(),
                source,
            })?;
        file.write_all(bytes)
            .await
            .map_err(|source| FileMutationError::Io {
                operation: "write",
                path: self.path.clone(),
                source,
            })?;
        file.flush().await.map_err(|source| FileMutationError::Io {
            operation: "flush",
            path: self.path.clone(),
            source,
        })?;
        let metadata = file
            .metadata()
            .await
            .map_err(|source| FileMutationError::Io {
                operation: "stat",
                path: self.path.clone(),
                source,
            })?;
        let after = snapshot_from_metadata(&metadata);
        self.ctx.file_times.stamp_snapshot(self.path.clone(), after);
        Ok(FileCommit::Written(FileWriteResult {
            path: self.path,
            bytes: after.size,
            mtime: after.mtime,
            mtime_ms: mtime_ms_f64(after.mtime),
        }))
    }

    fn open_commit_target(
        &self,
        expected_mtime_ms: Option<ExpectedMtime>,
    ) -> Result<(Option<std::fs::File>, Option<FileSnapshot>), FileMutationError> {
        const MAX_CREATE_RACES: usize = 8;

        for _ in 0..MAX_CREATE_RACES {
            match self.rooted.open_existing_write() {
                Ok(file) => {
                    let metadata = file.metadata().map_err(|source| FileMutationError::Io {
                        operation: "stat",
                        path: self.path.clone(),
                        source,
                    })?;
                    return Ok((Some(file), Some(snapshot_from_metadata(&metadata))));
                }
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                    if expected_mtime_ms.is_some() {
                        // Preserve the old conflict behavior: do not create
                        // parent directories or an empty leaf before the
                        // optimistic guard has rejected an absent target.
                        return Ok((None, None));
                    }
                    self.ctx
                        .file_times
                        .assert_write(&self.path, None)
                        .map_err(FileMutationError::Freshness)?;
                    match self.rooted.create_new() {
                        Ok(file) => return Ok((Some(file), None)),
                        Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
                            continue;
                        }
                        Err(source) => {
                            return Err(FileMutationError::Io {
                                operation: "create",
                                path: self.path.clone(),
                                source,
                            });
                        }
                    }
                }
                Err(source) => {
                    return Err(FileMutationError::Io {
                        operation: "open for write",
                        path: self.path.clone(),
                        source,
                    });
                }
            }
        }
        Err(FileMutationError::Io {
            operation: "open for write after concurrent creates",
            path: self.path.clone(),
            source: std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "target kept changing while it was opened",
            ),
        })
    }
}

fn metadata_snapshot(path: &RootedPath) -> Result<Option<FileSnapshot>, FileMutationError> {
    match path.open_read() {
        Ok(file) => file
            .metadata()
            .map(|metadata| Some(snapshot_from_metadata(&metadata)))
            .map_err(|source| FileMutationError::Io {
                operation: "stat",
                path: path.path().to_path_buf(),
                source,
            }),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(FileMutationError::Io {
            operation: "stat",
            path: path.path().to_path_buf(),
            source,
        }),
    }
}

fn snapshot_from_metadata(metadata: &std::fs::Metadata) -> FileSnapshot {
    FileSnapshot {
        mtime: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        size: metadata.len(),
    }
}

async fn read_all(file: std::fs::File) -> std::io::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt as _;

    let mut file = tokio::fs::File::from_std(file);
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).await?;
    Ok(bytes)
}

/// Read a UTF-8 text file within the workspace and return its contents.
///
/// The file is recorded in the read-before-write ledger, which later lets
/// `write_file` and `edit_file` overwrite it. Files larger than 256 KiB are
/// truncated (a note is appended). Reading a directory or a missing file is a
/// tool error, not a crash.
#[derive(Deserialize, schemars::JsonSchema)]
pub struct ReadFileInput {
    /// Path to the file to read, relative to the workspace root (or absolute
    /// inside it).
    pub path: String,
}

/// Reads a text file so the model can inspect it before editing.
pub struct ReadFile;

impl Tool for ReadFile {
    type Input = ReadFileInput;

    fn name(&self) -> &'static str {
        "read_file"
    }

    fn description(&self) -> String {
        "Read a UTF-8 text file inside the workspace and return its contents. \
         Files over 256 KiB are truncated. Records the file so it can later be \
         overwritten with write_file/edit_file (read-before-write)."
            .into()
    }

    fn capability(&self) -> Capability {
        Capability::ReadOnly
    }

    fn run(
        self: Arc<Self>,
        input: Self::Input,
        ctx: Arc<ToolCtx>,
    ) -> BoxFuture<'static, ToolOutput> {
        Box::pin(async move {
            let authorized = match ctx.policy.authorize_read(Path::new(&input.path)) {
                Ok(p) => p,
                Err(e) => return ToolOutput::error(e.to_string()),
            };
            let resolved = authorized.path().to_path_buf();
            let rooted = RootedPath::new(authorized);

            let file = match rooted.open_read() {
                Ok(file) => file,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return ToolOutput::error(format!("file not found: {}", input.path));
                }
                Err(e) => return ToolOutput::error(format!("cannot open {}: {e}", input.path)),
            };
            let meta = match file.metadata() {
                Ok(metadata) => metadata,
                Err(e) => return ToolOutput::error(format!("cannot stat {}: {e}", input.path)),
            };
            if meta.is_dir() {
                return ToolOutput::error(format!("is a directory, not a file: {}", input.path));
            }

            let bytes = match read_capped(file, READ_CAP + 1).await {
                Ok(b) => b,
                Err(e) => return ToolOutput::error(format!("cannot read {}: {e}", input.path)),
            };
            let truncated = bytes.len() > READ_CAP;
            let slice = if truncated {
                &bytes[..READ_CAP]
            } else {
                &bytes[..]
            };
            let mut content = String::from_utf8_lossy(slice).into_owned();
            if truncated {
                content.push_str(&format!(
                    "\n\n[truncated: file exceeds {READ_CAP} bytes; showing the first {READ_CAP}]"
                ));
            }

            if let Some(mtime) = mtime_of(&meta) {
                ctx.file_times
                    .stamp_with_size(resolved.clone(), mtime, meta.len());
            }

            ToolOutput::ok(content)
        })
    }
}

async fn read_capped(file: std::fs::File, limit: usize) -> std::io::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let file = tokio::fs::File::from_std(file);
    let mut buf = Vec::new();
    file.take(limit as u64).read_to_end(&mut buf).await?;
    Ok(buf)
}

/// Bytes read from one policy-authorized file descriptor.
///
/// `size` is the descriptor's metadata size at open time. `data` is always
/// bounded by the caller's `max_bytes`; if the file grows while it is being
/// read the operation fails instead of returning an oversized buffer.
#[derive(Debug, Clone)]
pub struct ReadBytes {
    pub data: Vec<u8>,
    pub size: u64,
    pub mtime: SystemTime,
    pub mtime_ms: f64,
}

#[derive(Debug)]
pub enum ReadBytesError {
    Io {
        operation: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    NotFile {
        path: PathBuf,
    },
    TooLarge {
        path: PathBuf,
        bytes: u64,
        max_bytes: u64,
    },
}

impl std::fmt::Display for ReadBytesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io {
                operation,
                path,
                source,
            } => write!(f, "cannot {operation} {}: {source}", path.display()),
            Self::NotFile { path } => write!(f, "not a file: {}", path.display()),
            Self::TooLarge {
                bytes, max_bytes, ..
            } => write!(f, "file too large ({bytes} bytes > {max_bytes} limit)"),
        }
    }
}

impl std::error::Error for ReadBytesError {}

/// Open and read one already-authorized file through AC's descriptor-relative
/// no-follow traversal, enforcing the byte ceiling on the same descriptor
/// whose metadata was inspected.
pub async fn read_bytes_authorized(
    authorized: AuthorizedPath,
    max_bytes: u64,
) -> Result<ReadBytes, ReadBytesError> {
    let (path, file, metadata) = open_authorized_file(authorized)?;
    let file = tokio::fs::File::from_std(file);
    read_bytes_from_open_file(&path, file, metadata, max_bytes).await
}

/// Blocking counterpart to [`read_bytes_authorized`], for CPU-bound adapters
/// whose parsing/rendering already runs on a blocking worker.
pub fn read_bytes_authorized_blocking(
    authorized: AuthorizedPath,
    max_bytes: u64,
) -> Result<ReadBytes, ReadBytesError> {
    use std::io::Read as _;

    let (path, file, metadata) = open_authorized_file(authorized)?;
    ensure_readable_file(&path, &metadata, max_bytes)?;
    let read_limit = max_bytes.saturating_add(1);
    let initial_capacity = metadata.len().min(read_limit).min(8 * 1024) as usize;
    let mut data = Vec::with_capacity(initial_capacity);
    file.take(read_limit)
        .read_to_end(&mut data)
        .map_err(|source| ReadBytesError::Io {
            operation: "read",
            path: path.clone(),
            source,
        })?;
    finish_read_bytes(path, metadata, data, max_bytes)
}

/// Create an already-authorized directory and any missing descendants without
/// following symlinks in the path on Unix.
pub fn ensure_directory_authorized(authorized: AuthorizedPath) -> std::io::Result<()> {
    RootedPath::new(authorized).ensure_dir()
}

/// Atomically replace one already-authorized file with bytes written to a
/// unique same-directory temporary entry. Unix traversal and publication are
/// descriptor-relative and no-follow.
pub fn write_atomic_authorized(authorized: AuthorizedPath, bytes: &[u8]) -> std::io::Result<()> {
    RootedPath::new(authorized).atomic_replace(bytes)
}

fn open_authorized_file(
    authorized: AuthorizedPath,
) -> Result<(PathBuf, std::fs::File, std::fs::Metadata), ReadBytesError> {
    let path = authorized.path().to_path_buf();
    let rooted = RootedPath::new(authorized);
    let file = rooted.open_read().map_err(|source| ReadBytesError::Io {
        operation: "stat",
        path: path.clone(),
        source,
    })?;
    let metadata = file.metadata().map_err(|source| ReadBytesError::Io {
        operation: "stat",
        path: path.clone(),
        source,
    })?;
    Ok((path, file, metadata))
}

fn ensure_readable_file(
    path: &Path,
    metadata: &std::fs::Metadata,
    max_bytes: u64,
) -> Result<(), ReadBytesError> {
    if !metadata.is_file() {
        return Err(ReadBytesError::NotFile {
            path: path.to_path_buf(),
        });
    }
    if metadata.len() > max_bytes {
        return Err(ReadBytesError::TooLarge {
            path: path.to_path_buf(),
            bytes: metadata.len(),
            max_bytes,
        });
    }
    Ok(())
}

async fn read_bytes_from_open_file(
    path: &Path,
    file: tokio::fs::File,
    metadata: std::fs::Metadata,
    max_bytes: u64,
) -> Result<ReadBytes, ReadBytesError> {
    use tokio::io::AsyncReadExt as _;

    ensure_readable_file(path, &metadata, max_bytes)?;
    let read_limit = max_bytes.saturating_add(1);
    let initial_capacity = metadata.len().min(read_limit).min(8 * 1024) as usize;
    let mut data = Vec::with_capacity(initial_capacity);
    file.take(read_limit)
        .read_to_end(&mut data)
        .await
        .map_err(|source| ReadBytesError::Io {
            operation: "read",
            path: path.to_path_buf(),
            source,
        })?;
    finish_read_bytes(path.to_path_buf(), metadata, data, max_bytes)
}

fn finish_read_bytes(
    path: PathBuf,
    metadata: std::fs::Metadata,
    data: Vec<u8>,
    max_bytes: u64,
) -> Result<ReadBytes, ReadBytesError> {
    if data.len() as u64 > max_bytes {
        return Err(ReadBytesError::TooLarge {
            path,
            bytes: data.len() as u64,
            max_bytes,
        });
    }
    let mtime = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    Ok(ReadBytes {
        data,
        size: metadata.len(),
        mtime,
        mtime_ms: mtime_ms_f64(mtime),
    })
}

#[derive(Debug, Clone)]
pub struct ReadTextSlice {
    pub content: String,
    pub bytes: u64,
    pub mtime: SystemTime,
    pub mtime_ms: f64,
    pub total_lines: usize,
}

#[derive(Debug)]
pub enum ReadTextError {
    Io {
        operation: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    NotFile {
        path: PathBuf,
    },
    TooLarge {
        path: PathBuf,
        bytes: u64,
        max_bytes: u64,
    },
}

impl std::fmt::Display for ReadTextError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io {
                operation,
                path,
                source,
            } => write!(f, "cannot {operation} {}: {source}", path.display()),
            Self::NotFile { path } => write!(f, "not a file: {}", path.display()),
            Self::TooLarge {
                bytes, max_bytes, ..
            } => write!(f, "file too large ({bytes} bytes > {max_bytes} limit)"),
        }
    }
}

impl std::error::Error for ReadTextError {}

/// Stat, size-gate, read, lossy UTF-8 decode, and line-slice one resolved path.
///
/// Routing and model-facing envelopes remain host policy. This primitive owns
/// the filesystem mechanics so app adapters do not port another read loop.
pub async fn read_text_slice(
    path: &Path,
    offset: usize,
    limit: usize,
    max_bytes: u64,
) -> Result<ReadTextSlice, ReadTextError> {
    let authorized =
        AuthorizedPath::from_resolved(path.to_path_buf()).map_err(|error| ReadTextError::Io {
            operation: "authorize",
            path: path.to_path_buf(),
            source: std::io::Error::new(std::io::ErrorKind::InvalidInput, error.to_string()),
        })?;
    read_text_slice_authorized(authorized, offset, limit, max_bytes).await
}

pub async fn read_text_slice_authorized(
    authorized: AuthorizedPath,
    offset: usize,
    limit: usize,
    max_bytes: u64,
) -> Result<ReadTextSlice, ReadTextError> {
    let path = authorized.path().to_path_buf();
    let rooted = RootedPath::new(authorized);
    let file = rooted.open_read().map_err(|source| ReadTextError::Io {
        // Preserve the prior path-level error contract: opening a
        // missing/denied path is surfaced as a `stat` failure.
        operation: "stat",
        path: path.clone(),
        source,
    })?;
    let metadata = file.metadata().map_err(|source| ReadTextError::Io {
        operation: "stat",
        path: path.clone(),
        source,
    })?;
    let file = tokio::fs::File::from_std(file);
    read_text_slice_from_open_file(&path, file, metadata, offset, limit, max_bytes).await
}

async fn read_text_slice_from_open_file(
    path: &Path,
    file: tokio::fs::File,
    metadata: std::fs::Metadata,
    offset: usize,
    limit: usize,
    max_bytes: u64,
) -> Result<ReadTextSlice, ReadTextError> {
    use tokio::io::AsyncReadExt;

    if !metadata.is_file() {
        return Err(ReadTextError::NotFile {
            path: path.to_path_buf(),
        });
    }
    if metadata.len() > max_bytes {
        return Err(ReadTextError::TooLarge {
            path: path.to_path_buf(),
            bytes: metadata.len(),
            max_bytes,
        });
    }
    // The descriptor is the same one we inspected above. Read at most one byte
    // beyond the limit so a file that grows after metadata() (or a path that is
    // swapped after open()) cannot trigger an unbounded allocation.
    let read_limit = max_bytes.saturating_add(1);
    let initial_capacity = metadata.len().min(read_limit).min(8 * 1024) as usize;
    let mut raw = Vec::with_capacity(initial_capacity);
    file.take(read_limit)
        .read_to_end(&mut raw)
        .await
        .map_err(|source| ReadTextError::Io {
            operation: "read",
            path: path.to_path_buf(),
            source,
        })?;
    if raw.len() as u64 > max_bytes {
        return Err(ReadTextError::TooLarge {
            path: path.to_path_buf(),
            bytes: raw.len() as u64,
            max_bytes,
        });
    }
    let text = String::from_utf8_lossy(&raw).into_owned();
    let lines: Vec<&str> = text.split('\n').collect();
    let start = offset.saturating_sub(1);
    let content = lines
        .iter()
        .skip(start)
        .take(limit)
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    let mtime = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    Ok(ReadTextSlice {
        content,
        bytes: metadata.len(),
        mtime,
        mtime_ms: mtime_ms_f64(mtime),
        total_lines: lines.len(),
    })
}

/// Create or overwrite a file inside the workspace.
///
/// An existing file may only be overwritten if it was read this run (via
/// `read_file`) and has not changed on disk since — otherwise the write is
/// refused and you must read it first. Missing parent directories are created.
#[derive(Deserialize, schemars::JsonSchema)]
pub struct WriteFileInput {
    /// Destination path, relative to the workspace root (or absolute inside it).
    pub path: String,
    /// Full new contents as UTF-8 text. Set exactly one of this or
    /// `content_base64`.
    pub content: Option<String>,
    /// Full new contents as base64-encoded bytes, for binary files. Set
    /// exactly one of this or `content`.
    pub content_base64: Option<String>,
    /// Optional optimistic-concurrency guard: when set and the target's
    /// current modification time in milliseconds differs (or the target no
    /// longer exists), the write is refused before anything is written and a
    /// structured conflict (`kind: "conflict"`, carrying both mtimes) is
    /// returned so the caller can re-read and retry.
    pub expected_mtime_ms: Option<u64>,
}

/// Writes a file, enforcing read-before-write on existing files.
pub struct WriteFile;

impl Tool for WriteFile {
    type Input = WriteFileInput;

    fn name(&self) -> &'static str {
        "write_file"
    }

    fn description(&self) -> String {
        "Create a new file or overwrite an existing one inside the workspace. \
         Provide exactly one of 'content' (UTF-8 text) or 'content_base64' \
         (base64-encoded bytes, for binary files). An existing file must have \
         been read this run (read_file) and be unchanged on disk, or the write \
         is refused. Optionally pass 'expected_mtime_ms': if the target's \
         current mtime (ms) differs, the write is refused with a structured \
         conflict ({\"kind\":\"conflict\", ...} carrying both mtimes) so you \
         can re-read and retry. Parent directories are created as needed."
            .into()
    }

    fn capability(&self) -> Capability {
        Capability::Mutating
    }

    fn run(
        self: Arc<Self>,
        input: Self::Input,
        ctx: Arc<ToolCtx>,
    ) -> BoxFuture<'static, ToolOutput> {
        Box::pin(async move {
            use base64::Engine as _;
            let bytes: Vec<u8> = match (input.content, input.content_base64) {
                (Some(text), None) => text.into_bytes(),
                (None, Some(b64)) => {
                    match base64::engine::general_purpose::STANDARD.decode(b64.as_bytes()) {
                        Ok(b) => b,
                        Err(e) => return ToolOutput::error(format!("invalid content_base64: {e}")),
                    }
                }
                (Some(_), Some(_)) => {
                    return ToolOutput::error(
                        "set exactly one of content or content_base64, not both",
                    );
                }
                (None, None) => {
                    return ToolOutput::error("set exactly one of content or content_base64");
                }
            };

            let mutation = match FileMutation::begin(ctx.clone(), PathBuf::from(&input.path)).await
            {
                Ok(mutation) => mutation,
                Err(error) => return ToolOutput::error(error.to_string()),
            };
            // Preserve the built-in's historic ordering: an explicit
            // optimistic guard is checked before read-before-write.
            match mutation
                .commit(
                    &bytes,
                    input.expected_mtime_ms.map(ExpectedMtime::Milliseconds),
                )
                .await
            {
                Ok(FileCommit::Written(result)) => ToolOutput::ok(format!(
                    "wrote {} bytes to {}",
                    result.bytes,
                    rel(&ctx.policy.root(), &result.path)
                )),
                Ok(FileCommit::Conflict { actual, .. }) => ToolOutput::error(
                    serde_json::json!({
                        "kind": "conflict",
                        "expected_mtime_ms": input.expected_mtime_ms,
                        "actual_mtime_ms": actual.and_then(|snapshot| mtime_ms(snapshot.mtime)),
                    })
                    .to_string(),
                ),
                Err(error) => ToolOutput::error(error.to_string()),
            }
        })
    }
}

/// Replace one unambiguous occurrence of a string in an existing file.
///
/// The file must already have been read this run. The shared fuzzy cascade
/// tolerates common formatting drift but refuses missing, ambiguous, or
/// disproportionately large matches.
#[derive(Deserialize, schemars::JsonSchema)]
pub struct EditFileInput {
    /// Path to the file to edit, relative to the workspace root.
    pub path: String,
    /// The text to find, with enough surrounding context to be unambiguous.
    pub old_string: String,
    /// The text to replace it with.
    pub new_string: String,
    /// Replace every occurrence instead of requiring a unique match.
    pub replace_all: Option<bool>,
}

/// Makes a precise single-occurrence replacement in a file.
pub struct EditFile;

impl Tool for EditFile {
    type Input = EditFileInput;

    fn name(&self) -> &'static str {
        "edit_file"
    }

    fn description(&self) -> String {
        "Replace one occurrence of old_string with new_string in an existing \
         file (which must have been read this run). Matching tolerates common \
         whitespace, indentation, line-ending, and escaping drift while \
         refusing ambiguous or disproportionately large spans. Set \
         replace_all to replace every occurrence."
            .into()
    }

    fn capability(&self) -> Capability {
        Capability::Mutating
    }

    fn run(
        self: Arc<Self>,
        input: Self::Input,
        ctx: Arc<ToolCtx>,
    ) -> BoxFuture<'static, ToolOutput> {
        Box::pin(async move {
            if input.old_string.is_empty() {
                return ToolOutput::error("old_string must not be empty");
            }

            let mutation = match FileMutation::begin(ctx.clone(), PathBuf::from(&input.path)).await
            {
                Ok(mutation) => mutation,
                Err(error) => return ToolOutput::error(error.to_string()),
            };
            if !mutation.exists() {
                return ToolOutput::error(format!("file not found: {}", input.path));
            }
            if let Err(error) = mutation.assert_fresh() {
                return ToolOutput::error(error.to_string());
            }
            let current_snapshot = mutation.initial().expect("exists checked");
            let content = match mutation.read().await {
                Ok(bytes) => match String::from_utf8(bytes) {
                    Ok(content) => content,
                    Err(error) => {
                        return ToolOutput::error(format!(
                            "cannot read {} as UTF-8: {error}",
                            input.path
                        ));
                    }
                },
                Err(error) => return ToolOutput::error(error.to_string()),
            };

            let ending = detect_line_ending(&content);
            let old_string =
                convert_to_line_ending(&normalize_line_endings(&input.old_string), ending);
            let new_string =
                convert_to_line_ending(&normalize_line_endings(&input.new_string), ending);
            let updated = match fuzzy_replace(
                &content,
                &old_string,
                &new_string,
                input.replace_all.unwrap_or(false),
            ) {
                Ok(updated) => updated,
                Err(error) => return ToolOutput::error(error),
            };
            match mutation
                .commit(
                    updated.as_bytes(),
                    Some(ExpectedMtime::Exact(mtime_ms_f64(current_snapshot.mtime))),
                )
                .await
            {
                Ok(FileCommit::Written(result)) => {
                    ToolOutput::ok(format!("edited {}", rel(&ctx.policy.root(), &result.path)))
                }
                Ok(FileCommit::Conflict { .. }) => ToolOutput::error(
                    "file changed on disk while it was being edited; read it again",
                ),
                Err(error) => ToolOutput::error(error.to_string()),
            }
        })
    }
}

/// List the immediate entries of a directory inside the workspace.
///
/// Non-recursive. Directories are suffixed with `/`. Results are sorted.
#[derive(Deserialize, schemars::JsonSchema)]
pub struct ListFilesInput {
    /// Directory to list, relative to the workspace root. Defaults to the root.
    pub path: Option<String>,
}

/// Lists the direct children of a directory.
pub struct ListFiles;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectoryEntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

#[derive(Debug, Clone)]
pub struct DirectoryEntry {
    pub name: String,
    pub kind: DirectoryEntryKind,
    pub size_bytes: Option<u64>,
    pub symlink_target: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct DirectoryListing {
    pub entries: Vec<DirectoryEntry>,
    /// Number of entries after filtering but before clipping.
    pub total: usize,
    pub truncated: bool,
}

/// AC-owned one-level directory enumeration used by both the stock list tool
/// and host adapters with richer result envelopes.
pub async fn list_directory(
    path: &Path,
    skip_names: &[&str],
    max_entries: Option<usize>,
    sort: bool,
) -> std::io::Result<DirectoryListing> {
    let authorized = AuthorizedPath::from_resolved(path.to_path_buf()).map_err(|error| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, error.to_string())
    })?;
    list_directory_authorized(authorized, skip_names, max_entries, sort).await
}

pub async fn list_directory_authorized(
    authorized: AuthorizedPath,
    skip_names: &[&str],
    max_entries: Option<usize>,
    sort: bool,
) -> std::io::Result<DirectoryListing> {
    list_directory_authorized_blocking(authorized, skip_names, max_entries, sort)
}

/// Blocking counterpart to [`list_directory_authorized`].
pub fn list_directory_authorized_blocking(
    authorized: AuthorizedPath,
    skip_names: &[&str],
    max_entries: Option<usize>,
    sort: bool,
) -> std::io::Result<DirectoryListing> {
    let rooted = RootedPath::new(authorized);
    #[cfg(unix)]
    let mut entries = enumerate_directory(rooted.open_dir()?, skip_names)?;
    #[cfg(not(unix))]
    let mut entries = enumerate_directory(rooted.path(), skip_names)?;
    if sort {
        entries.sort_by(|left, right| left.name.cmp(&right.name));
    }
    let total = entries.len();
    let limit = max_entries.unwrap_or(total);
    entries.truncate(limit);
    Ok(DirectoryListing {
        entries,
        total,
        truncated: total > limit,
    })
}

#[cfg(unix)]
fn enumerate_directory(
    dir: std::fs::File,
    skip_names: &[&str],
) -> std::io::Result<Vec<DirectoryEntry>> {
    use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};

    use rustix::fs::{AtFlags, Dir, FileType, readlinkat, statat};

    let mut stream = Dir::read_from(&dir).map_err(std::io::Error::from)?;
    let mut entries = Vec::new();
    for entry in &mut stream {
        let entry = entry.map_err(std::io::Error::from)?;
        let name_bytes = entry.file_name().to_bytes();
        if name_bytes == b"." || name_bytes == b".." {
            continue;
        }
        let os_name = std::ffi::OsStr::from_bytes(name_bytes);
        let name = os_name.to_string_lossy().into_owned();
        if skip_names.contains(&name.as_str()) {
            continue;
        }
        let stat = statat(&dir, os_name, AtFlags::SYMLINK_NOFOLLOW).ok();
        let file_type = stat
            .as_ref()
            .map(|stat| FileType::from_raw_mode(stat.st_mode));
        let (kind, size_bytes, symlink_target) = match file_type {
            Some(FileType::Directory) => (DirectoryEntryKind::Directory, None, None),
            Some(FileType::Symlink) => (
                DirectoryEntryKind::Symlink,
                None,
                readlinkat(&dir, os_name, Vec::new())
                    .ok()
                    .map(|target| PathBuf::from(std::ffi::OsString::from_vec(target.into_bytes()))),
            ),
            Some(FileType::RegularFile) => (
                DirectoryEntryKind::File,
                stat.and_then(|stat| u64::try_from(stat.st_size).ok()),
                None,
            ),
            _ => (DirectoryEntryKind::Other, None, None),
        };
        entries.push(DirectoryEntry {
            name,
            kind,
            size_bytes,
            symlink_target,
        });
    }
    Ok(entries)
}

#[cfg(not(unix))]
fn enumerate_directory(path: &Path, skip_names: &[&str]) -> std::io::Result<Vec<DirectoryEntry>> {
    let dir = std::fs::read_dir(path)?;
    let mut entries = Vec::new();
    for entry in dir {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if skip_names.contains(&name.as_str()) {
            continue;
        }
        let child = entry.path();
        let file_type = entry.file_type().ok();
        let (kind, size_bytes, symlink_target) = match file_type {
            Some(file_type) if file_type.is_dir() => (DirectoryEntryKind::Directory, None, None),
            Some(file_type) if file_type.is_symlink() => (
                DirectoryEntryKind::Symlink,
                None,
                std::fs::read_link(&child).ok(),
            ),
            Some(file_type) if file_type.is_file() => (
                DirectoryEntryKind::File,
                std::fs::metadata(&child).ok().map(|meta| meta.len()),
                None,
            ),
            _ => (DirectoryEntryKind::Other, None, None),
        };
        entries.push(DirectoryEntry {
            name,
            kind,
            size_bytes,
            symlink_target,
        });
    }
    Ok(entries)
}

impl Tool for ListFiles {
    type Input = ListFilesInput;

    fn name(&self) -> &'static str {
        "list_files"
    }

    fn description(&self) -> String {
        "List the immediate entries of a directory inside the workspace \
         (non-recursive). Directories end with '/'. Defaults to the workspace \
         root."
            .into()
    }

    fn capability(&self) -> Capability {
        Capability::ReadOnly
    }

    fn run(
        self: Arc<Self>,
        input: Self::Input,
        ctx: Arc<ToolCtx>,
    ) -> BoxFuture<'static, ToolOutput> {
        Box::pin(async move {
            let path = input.path.unwrap_or_else(|| ".".to_string());
            let authorized = match ctx.policy.authorize_read(Path::new(&path)) {
                Ok(p) => p,
                Err(e) => return ToolOutput::error(e.to_string()),
            };

            let listing = match list_directory_authorized(authorized, &[], None, true).await {
                Ok(listing) => listing,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return ToolOutput::error(format!("directory not found: {path}"));
                }
                Err(e) => return ToolOutput::error(format!("cannot list {path}: {e}")),
            };
            let names: Vec<String> = listing
                .entries
                .into_iter()
                .map(|entry| {
                    if entry.kind == DirectoryEntryKind::Directory {
                        format!("{}/", entry.name)
                    } else {
                        entry.name
                    }
                })
                .collect();

            if names.is_empty() {
                ToolOutput::ok("(empty)")
            } else {
                ToolOutput::ok(names.join("\n"))
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ac_tool::{AuthorizedPath, PathPolicy, PolicyError, SubtreePolicy};
    use std::path::PathBuf;
    use std::sync::{Barrier, Mutex};

    fn ctx_in(dir: &Path) -> Arc<ToolCtx> {
        Arc::new(ToolCtx::new(Arc::new(SubtreePolicy::new(dir).unwrap())))
    }

    async fn read(ctx: &Arc<ToolCtx>, path: &str) -> ToolOutput {
        Arc::new(ReadFile)
            .run(ReadFileInput { path: path.into() }, ctx.clone())
            .await
    }

    async fn write(ctx: &Arc<ToolCtx>, input: WriteFileInput) -> ToolOutput {
        Arc::new(WriteFile).run(input, ctx.clone()).await
    }

    async fn list(ctx: &Arc<ToolCtx>, path: &str) -> ToolOutput {
        Arc::new(ListFiles)
            .run(
                ListFilesInput {
                    path: Some(path.into()),
                },
                ctx.clone(),
            )
            .await
    }

    async fn edit(ctx: &Arc<ToolCtx>, path: &str, old: &str, new: &str) -> ToolOutput {
        Arc::new(EditFile)
            .run(
                EditFileInput {
                    path: path.into(),
                    old_string: old.into(),
                    new_string: new.into(),
                    replace_all: None,
                },
                ctx.clone(),
            )
            .await
    }

    fn text_input(path: &str, content: &str) -> WriteFileInput {
        WriteFileInput {
            path: path.into(),
            content: Some(content.into()),
            content_base64: None,
            expected_mtime_ms: None,
        }
    }

    fn disk_mtime_ms(path: &Path) -> u64 {
        mtime_ms(std::fs::metadata(path).unwrap().modified().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn resolved_read_slice_owns_decode_and_line_window() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, b"one\ntwo\nthree").unwrap();
        let path = path.canonicalize().unwrap();
        let slice = read_text_slice(&path, 2, 1, 1024).await.unwrap();
        assert_eq!(slice.content, "two");
        assert_eq!(slice.total_lines, 3);
        assert_eq!(slice.bytes, 13);
        assert!(slice.mtime_ms.is_finite());
    }

    #[tokio::test]
    async fn authorized_binary_read_uses_capability_and_byte_ceiling() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("image.bin");
        std::fs::write(&path, b"pixels").unwrap();
        let authorized = AuthorizedPath::new(root.clone(), path.clone()).unwrap();

        let read = read_bytes_authorized(authorized.clone(), 6).await.unwrap();
        assert_eq!(read.data, b"pixels");
        assert_eq!(read.size, 6);

        let error = read_bytes_authorized(authorized.clone(), 5)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ReadBytesError::TooLarge {
                bytes: 6,
                max_bytes: 5,
                ..
            }
        ));

        let blocking = read_bytes_authorized_blocking(authorized, 6).unwrap();
        assert_eq!(blocking.data, b"pixels");
    }

    #[test]
    fn authorized_directory_create_and_atomic_replace_stay_under_capability() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let nested = root.join("project").join("assets");
        ensure_directory_authorized(AuthorizedPath::new(root.clone(), nested.clone()).unwrap())
            .unwrap();
        assert!(nested.is_dir());

        let file = nested.join("manifest.json");
        let authorized = AuthorizedPath::new(root, file.clone()).unwrap();
        write_atomic_authorized(authorized.clone(), b"one").unwrap();
        write_atomic_authorized(authorized, b"two").unwrap();
        assert_eq!(std::fs::read(file).unwrap(), b"two");
    }

    #[cfg(unix)]
    #[test]
    fn authorized_directory_create_refuses_a_symlinked_parent() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        symlink(outside.path(), root.join("project")).unwrap();
        let target = root.join("project").join("assets");
        let error =
            ensure_directory_authorized(AuthorizedPath::new(root, target).unwrap()).unwrap_err();
        assert!(
            error.raw_os_error() == Some(libc::ELOOP)
                || error.raw_os_error() == Some(libc::ENOTDIR),
            "unexpected error: {error}"
        );
        assert!(!outside.path().join("assets").exists());
    }

    #[tokio::test]
    async fn authorized_binary_read_caps_growth_after_metadata() {
        use std::io::Write as _;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("growing.bin");
        std::fs::write(&path, b"x").unwrap();
        let file = tokio::fs::File::open(&path).await.unwrap();
        let metadata = file.metadata().await.unwrap();

        let mut writer = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writer.write_all(&vec![b'y'; 4096]).unwrap();
        writer.flush().unwrap();

        let error = read_bytes_from_open_file(&path, file, metadata, 32)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ReadBytesError::TooLarge {
                bytes: 33,
                max_bytes: 32,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn resolved_read_slice_caps_growth_after_metadata() {
        use std::io::Write as _;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("growing.txt");
        std::fs::write(&path, b"x").unwrap();
        let file = tokio::fs::File::open(&path).await.unwrap();
        let metadata = file.metadata().await.unwrap();

        let mut writer = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writer.write_all(&vec![b'y'; 4096]).unwrap();
        writer.flush().unwrap();

        let error = read_text_slice_from_open_file(&path, file, metadata, 1, 10, 32)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ReadTextError::TooLarge {
                bytes: 33,
                max_bytes: 32,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn resolved_read_slice_keeps_the_opened_file_when_path_is_swapped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("swapped.txt");
        let moved = dir.path().join("original.txt");
        std::fs::write(&path, b"original").unwrap();
        let file = tokio::fs::File::open(&path).await.unwrap();
        let metadata = file.metadata().await.unwrap();

        std::fs::rename(&path, &moved).unwrap();
        std::fs::write(&path, b"replacement").unwrap();

        let slice = read_text_slice_from_open_file(&path, file, metadata, 1, 10, 1024)
            .await
            .unwrap();
        assert_eq!(slice.content, "original");
        assert_eq!(slice.bytes, 8);
    }

    #[tokio::test]
    async fn resolved_directory_listing_filters_and_clips() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "a").unwrap();
        std::fs::write(dir.path().join("b.txt"), "bb").unwrap();
        std::fs::write(dir.path().join(".skip"), "x").unwrap();
        let root = dir.path().canonicalize().unwrap();
        let listing = list_directory(&root, &[".skip"], Some(1), true)
            .await
            .unwrap();
        assert_eq!(listing.total, 2);
        assert!(listing.truncated);
        assert_eq!(listing.entries.len(), 1);
        assert_eq!(listing.entries[0].name, "a.txt");
        assert_eq!(listing.entries[0].kind, DirectoryEntryKind::File);
        assert_eq!(listing.entries[0].size_bytes, Some(1));
    }

    #[tokio::test]
    async fn expected_mtime_conflict_is_structured_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "v1").unwrap();
        let ctx = ctx_in(dir.path());
        read(&ctx, "a.txt").await;
        let expected = disk_mtime_ms(&path);

        // Move the on-disk mtime forward, as an outside writer would.
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        let f = std::fs::File::options().write(true).open(&path).unwrap();
        f.set_modified(later).unwrap();
        drop(f);
        let actual = disk_mtime_ms(&path);
        assert_ne!(expected, actual);

        let out = write(
            &ctx,
            WriteFileInput {
                expected_mtime_ms: Some(expected),
                ..text_input("a.txt", "v2")
            },
        )
        .await;
        assert!(out.is_error);
        let v: serde_json::Value = serde_json::from_str(&out.content).expect("structured conflict");
        assert_eq!(v["kind"], "conflict");
        assert_eq!(v["expected_mtime_ms"], expected);
        assert_eq!(v["actual_mtime_ms"], actual);
        // Refused BEFORE writing: the prior contents are intact.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "v1");
    }

    #[tokio::test]
    async fn matching_expected_mtime_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "v1").unwrap();
        let ctx = ctx_in(dir.path());
        read(&ctx, "a.txt").await;

        let out = write(
            &ctx,
            WriteFileInput {
                expected_mtime_ms: Some(disk_mtime_ms(&path)),
                ..text_input("a.txt", "v2")
            },
        )
        .await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "v2");
    }

    #[tokio::test]
    async fn content_base64_round_trips_bytes() {
        use base64::Engine as _;
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(dir.path());
        let bytes: Vec<u8> = vec![0, 255, 128, 10, 13, 0, 7];

        let out = write(
            &ctx,
            WriteFileInput {
                path: "bin.dat".into(),
                content: None,
                content_base64: Some(base64::engine::general_purpose::STANDARD.encode(&bytes)),
                expected_mtime_ms: None,
            },
        )
        .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("wrote 7 bytes"));
        assert_eq!(std::fs::read(dir.path().join("bin.dat")).unwrap(), bytes);

        let bad = write(
            &ctx,
            WriteFileInput {
                path: "bad.dat".into(),
                content: None,
                content_base64: Some("not base64!!".into()),
                expected_mtime_ms: None,
            },
        )
        .await;
        assert!(bad.is_error);
        assert!(bad.content.contains("content_base64"));
    }

    #[tokio::test]
    async fn exactly_one_content_form_is_required() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(dir.path());

        let both = write(
            &ctx,
            WriteFileInput {
                path: "x.txt".into(),
                content: Some("a".into()),
                content_base64: Some("YQ==".into()),
                expected_mtime_ms: None,
            },
        )
        .await;
        assert!(both.is_error);
        assert!(both.content.contains("exactly one"));

        let neither = write(
            &ctx,
            WriteFileInput {
                path: "x.txt".into(),
                content: None,
                content_base64: None,
                expected_mtime_ms: None,
            },
        )
        .await;
        assert!(neither.is_error);
        assert!(neither.content.contains("exactly one"));

        assert!(!dir.path().join("x.txt").exists());
    }

    #[derive(Default)]
    struct Recording(Mutex<Vec<(PathBuf, Vec<u8>)>>);

    impl WriteObserver for Recording {
        fn before_overwrite(&self, path: &Path, prior: &[u8]) -> Result<(), String> {
            self.0
                .lock()
                .unwrap()
                .push((path.to_path_buf(), prior.to_vec()));
            Ok(())
        }
    }

    struct Failing;

    impl WriteObserver for Failing {
        fn before_overwrite(&self, _path: &Path, _prior: &[u8]) -> Result<(), String> {
            Err("history store unavailable".into())
        }
    }

    #[tokio::test]
    async fn observer_sees_prior_bytes_on_overwrite_and_edit_but_not_creation() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "v1").unwrap();
        let ctx = ctx_in(dir.path());
        let recording = Arc::new(Recording::default());
        ctx.extensions
            .insert(recording.clone() as Arc<dyn WriteObserver>);

        read(&ctx, "a.txt").await;
        let overwrote = write(&ctx, text_input("a.txt", "v2")).await;
        assert!(!overwrote.is_error, "{}", overwrote.content);
        let edited = edit(&ctx, "a.txt", "v2", "v3").await;
        assert!(!edited.is_error, "{}", edited.content);
        // New-file creation must not invoke the observer.
        let created = write(&ctx, text_input("new.txt", "fresh")).await;
        assert!(!created.is_error, "{}", created.content);

        let seen = recording.0.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(seen[0].0.ends_with("a.txt"));
        assert_eq!(seen[0].1, b"v1");
        assert_eq!(seen[1].1, b"v2");
    }

    #[tokio::test]
    async fn failing_observer_aborts_write_and_edit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "v1").unwrap();
        let ctx = ctx_in(dir.path());
        ctx.extensions
            .insert(Arc::new(Failing) as Arc<dyn WriteObserver>);
        read(&ctx, "a.txt").await;

        let out = write(&ctx, text_input("a.txt", "v2")).await;
        assert!(out.is_error);
        assert!(out.content.contains("history store unavailable"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "v1");

        let out = edit(&ctx, "a.txt", "v1", "v2").await;
        assert!(out.is_error);
        assert!(out.content.contains("history store unavailable"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "v1");

        // A failing observer never blocks creating a NEW file.
        let created = write(&ctx, text_input("new.txt", "fresh")).await;
        assert!(!created.is_error, "{}", created.content);
    }

    #[cfg(unix)]
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum BarrierAccess {
        Read,
        Write,
    }

    /// A deterministic authorization/use race: the inner policy reaches its
    /// verdict, then a cooperating thread swaps an ancestor before the
    /// descriptor-relative filesystem operation starts.
    #[cfg(unix)]
    struct BarrierPolicy {
        inner: Arc<dyn PathPolicy>,
        access: BarrierAccess,
        authorized: Arc<Barrier>,
        swapped: Arc<Barrier>,
    }

    #[cfg(unix)]
    impl BarrierPolicy {
        fn pause(&self, access: BarrierAccess) {
            if self.access == access {
                self.authorized.wait();
                self.swapped.wait();
            }
        }
    }

    #[cfg(unix)]
    impl PathPolicy for BarrierPolicy {
        fn root(&self) -> PathBuf {
            self.inner.root()
        }

        fn resolve_read(&self, path: &Path) -> Result<PathBuf, PolicyError> {
            self.inner.resolve_read(path)
        }

        fn resolve_write(&self, path: &Path) -> Result<PathBuf, PolicyError> {
            self.inner.resolve_write(path)
        }

        fn authorize_read(&self, path: &Path) -> Result<AuthorizedPath, PolicyError> {
            let authorized = self.inner.authorize_read(path)?;
            self.pause(BarrierAccess::Read);
            Ok(authorized)
        }

        fn authorize_write(&self, path: &Path) -> Result<AuthorizedPath, PolicyError> {
            let authorized = self.inner.authorize_write(path)?;
            self.pause(BarrierAccess::Write);
            Ok(authorized)
        }
    }

    #[cfg(unix)]
    fn racing_ctx(
        root: &Path,
        access: BarrierAccess,
    ) -> (Arc<ToolCtx>, Arc<Barrier>, Arc<Barrier>) {
        let authorized = Arc::new(Barrier::new(2));
        let swapped = Arc::new(Barrier::new(2));
        let inner: Arc<dyn PathPolicy> = Arc::new(SubtreePolicy::new(root).unwrap());
        let policy = BarrierPolicy {
            inner,
            access,
            authorized: authorized.clone(),
            swapped: swapped.clone(),
        };
        (
            Arc::new(ToolCtx::new(Arc::new(policy))),
            authorized,
            swapped,
        )
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn read_cannot_follow_parent_symlink_swapped_after_authorization() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let parent = root.path().join("parent");
        std::fs::create_dir(&parent).unwrap();
        std::fs::write(parent.join("secret.txt"), "inside").unwrap();
        std::fs::write(outside.path().join("secret.txt"), "OUTSIDE SECRET").unwrap();

        let (ctx, authorized, swapped) = racing_ctx(root.path(), BarrierAccess::Read);
        let root_path = root.path().to_path_buf();
        let outside_path = outside.path().to_path_buf();
        let swapper = std::thread::spawn(move || {
            authorized.wait();
            std::fs::rename(root_path.join("parent"), root_path.join("old-parent")).unwrap();
            symlink(outside_path, root_path.join("parent")).unwrap();
            swapped.wait();
        });

        let output = read(&ctx, "parent/secret.txt").await;
        swapper.join().unwrap();
        assert!(
            output.is_error,
            "unexpected escaped read: {}",
            output.content
        );
        assert!(!output.content.contains("OUTSIDE SECRET"));
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_cannot_follow_parent_symlink_swapped_after_authorization() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let parent = root.path().join("parent");
        std::fs::create_dir(&parent).unwrap();
        std::fs::write(parent.join("inside.txt"), "inside").unwrap();
        std::fs::write(outside.path().join("outside.txt"), "outside").unwrap();

        let (ctx, authorized, swapped) = racing_ctx(root.path(), BarrierAccess::Read);
        let root_path = root.path().to_path_buf();
        let outside_path = outside.path().to_path_buf();
        let swapper = std::thread::spawn(move || {
            authorized.wait();
            std::fs::rename(root_path.join("parent"), root_path.join("old-parent")).unwrap();
            symlink(outside_path, root_path.join("parent")).unwrap();
            swapped.wait();
        });

        let output = list(&ctx, "parent").await;
        swapper.join().unwrap();
        assert!(
            output.is_error,
            "unexpected escaped list: {}",
            output.content
        );
        assert!(!output.content.contains("outside.txt"));
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn write_cannot_follow_absent_parent_swapped_after_authorization() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let (ctx, authorized, swapped) = racing_ctx(root.path(), BarrierAccess::Write);
        let root_path = root.path().to_path_buf();
        let outside_path = outside.path().to_path_buf();
        let swapper = std::thread::spawn(move || {
            authorized.wait();
            symlink(outside_path, root_path.join("new-parent")).unwrap();
            swapped.wait();
        });

        let output = write(&ctx, text_input("new-parent/pwn.txt", "escaped")).await;
        swapper.join().unwrap();
        assert!(
            output.is_error,
            "unexpected escaped write: {}",
            output.content
        );
        assert!(!outside.path().join("pwn.txt").exists());
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mutation_commit_cannot_follow_parent_symlink_appearing_after_begin() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let ctx = ctx_in(root.path());
        let mutation = FileMutation::begin(ctx, PathBuf::from("new-parent/pwn.txt"))
            .await
            .unwrap();
        assert!(!mutation.exists());

        let ready = Arc::new(Barrier::new(2));
        let swapped = Arc::new(Barrier::new(2));
        let root_path = root.path().to_path_buf();
        let outside_path = outside.path().to_path_buf();
        let ready_for_thread = ready.clone();
        let swapped_for_thread = swapped.clone();
        let swapper = std::thread::spawn(move || {
            ready_for_thread.wait();
            symlink(outside_path, root_path.join("new-parent")).unwrap();
            swapped_for_thread.wait();
        });
        ready.wait();
        swapped.wait();

        let error = mutation.commit(b"escaped", None).await.unwrap_err();
        swapper.join().unwrap();
        assert!(
            error.to_string().contains("create") || error.to_string().contains("write"),
            "{error}"
        );
        assert!(!outside.path().join("pwn.txt").exists());
    }
}
