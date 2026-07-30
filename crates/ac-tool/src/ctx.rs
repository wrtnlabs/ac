use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio_util::sync::CancellationToken;

use crate::agent::AgentSpawner;
use crate::policy::PathPolicy;
use crate::sandbox::SandboxLauncher;

/// What every tool receives.
///
/// A run owns one base context. The runtime creates a shallow
/// [`for_invocation`](Self::for_invocation) clone for every dispatched tool
/// call: run-scoped state stays shared, while the provider's exact call id is
/// carried only by that invocation. This makes identity available to generic
/// tools without observation-order side channels.
#[derive(Clone)]
pub struct ToolCtx {
    pub policy: Arc<dyn PathPolicy>,
    /// The OS-sandbox seam. `None` means no launcher is installed — a tool
    /// that runs external processes must then decide for itself whether to run
    /// unsandboxed (and say so) or refuse. Install one with
    /// [`with_sandbox`](ToolCtx::with_sandbox).
    pub sandbox: Option<Arc<dyn SandboxLauncher>>,
    /// The sub-agent seam ([docs/ac-subagents.md]). `None` means delegation is
    /// unavailable here — a `task`-style tool must refuse as data. A CHILD ctx
    /// has this `None` by construction: that absence IS the recursion guard.
    /// Install one with [`with_spawner`](ToolCtx::with_spawner).
    pub spawner: Option<Arc<dyn AgentSpawner>>,
    pub extensions: Arc<Extensions>,
    pub file_times: Arc<FileTimes>,
    pub locks: Arc<PathLocks>,
    pub cancel: CancellationToken,
    tool_call_id: Option<String>,
}

impl ToolCtx {
    pub fn new(policy: Arc<dyn PathPolicy>) -> Self {
        Self {
            policy,
            sandbox: None,
            spawner: None,
            extensions: Arc::new(Extensions::default()),
            file_times: Arc::new(FileTimes::default()),
            locks: Arc::new(PathLocks::default()),
            cancel: CancellationToken::new(),
            tool_call_id: None,
        }
    }

    /// Install an OS-sandbox launcher (builder-style, before the ctx is shared
    /// behind an `Arc`).
    pub fn with_sandbox(mut self, sandbox: Arc<dyn SandboxLauncher>) -> Self {
        self.sandbox = Some(sandbox);
        self
    }

    /// Install a sub-agent spawner (builder-style). A child context is built
    /// *without* this call — the omission is the structural recursion guard.
    pub fn with_spawner(mut self, spawner: Arc<dyn AgentSpawner>) -> Self {
        self.spawner = Some(spawner);
        self
    }

    /// Use `cancel` as this context's cancellation token (builder-style). A
    /// child context is built with a token *derived from* the parent's
    /// (`parent.cancel.child_token()`) so cancel flows down but never up.
    pub fn with_cancel(mut self, cancel: CancellationToken) -> Self {
        self.cancel = cancel;
        self
    }

    /// Clone this run context for one concrete provider tool invocation.
    ///
    /// Policy, extensions, freshness state, path locks, cancellation, sandbox,
    /// and spawner are shared with the run. Only `tool_call_id` is
    /// invocation-local, so concurrent calls cannot overwrite or reorder one
    /// another's identity.
    pub fn for_invocation(&self, tool_call_id: impl Into<String>) -> Self {
        let mut invocation = self.clone();
        invocation.tool_call_id = Some(tool_call_id.into());
        invocation
    }

    /// The provider-assigned id of the tool call currently being dispatched.
    ///
    /// This is `None` on a run's base context and on tools invoked directly by
    /// a host without going through the runtime dispatcher.
    pub fn tool_call_id(&self) -> Option<&str> {
        self.tool_call_id.as_deref()
    }
}

/// Per-path async mutex map. When a turn runs several mutating tools
/// concurrently, a read-modify-write on the same file would otherwise race and
/// lose an update; a tool that holds `locks.lock(path)` across its
/// read→modify→write is serialized against any other holder of the same path.
#[derive(Default)]
pub struct PathLocks(Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>);

impl PathLocks {
    /// Acquire the lock for `path`, awaiting any concurrent holder. The returned
    /// guard serializes same-path writers; distinct paths never contend.
    pub async fn lock(&self, path: &Path) -> tokio::sync::OwnedMutexGuard<()> {
        let mutex = {
            let mut map = self.0.lock().expect("path-locks lock poisoned");
            map.entry(path.to_path_buf())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        mutex.lock_owned().await
    }
}

/// Typed extension slot: host tools carry host state through the kit's ctx
/// without the kit knowing the types (and without ToolCtx ever freezing into
/// a god-struct).
#[derive(Default)]
pub struct Extensions(RwLock<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>);

impl Extensions {
    pub fn insert<T: Send + Sync + 'static>(&self, value: T) {
        self.0
            .write()
            .expect("extensions lock poisoned")
            .insert(TypeId::of::<T>(), Arc::new(value));
    }

    pub fn get<T: Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        self.0
            .read()
            .expect("extensions lock poisoned")
            .get(&TypeId::of::<T>())
            .cloned()
            .and_then(|any| any.downcast::<T>().ok())
    }
}

/// Metadata captured when a file is read.
///
/// Size composes with mtime so filesystems whose timestamp resolution is
/// coarse still catch a same-tick external rewrite that changed the length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileSnapshot {
    pub mtime: SystemTime,
    pub size: u64,
}

#[derive(Debug, Clone)]
struct ReadStamp {
    observed: FileSnapshot,
    read_at: SystemTime,
}

/// Why an overwrite failed the shared read-before-write gate.
///
/// The display strings are stable, actionable agent guidance rather than host
/// policy. Keeping them here prevents every host from rebuilding a second
/// freshness ledger merely to preserve useful errors.
#[derive(Debug, Clone)]
pub enum FileTimeError {
    NeverRead {
        path: PathBuf,
    },
    Stale {
        path: PathBuf,
        last_modified: SystemTime,
        last_read: SystemTime,
    },
}

impl fmt::Display for FileTimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NeverRead { path } => write!(
                f,
                "You must read file {} before overwriting it. Use the Read tool first",
                path.display()
            ),
            Self::Stale {
                path,
                last_modified,
                last_read,
            } => write!(
                f,
                "File {} has been modified since it was last read.\nLast modification: {}\nLast read: {}\n\nPlease read the file again before modifying it.",
                path.display(),
                iso8601_ms(*last_modified),
                iso8601_ms(*last_read),
            ),
        }
    }
}

impl std::error::Error for FileTimeError {}

/// One per-run freshness ledger, owned by [`ToolCtx`].
///
/// Hosts must use this ledger rather than installing an app-specific tracker
/// in `extensions`: the built-in file tools and host adapters then share the
/// same reads, locks, and overwrite decisions.
pub struct FileTimes {
    bypass: AtomicBool,
    stamps: Mutex<HashMap<PathBuf, ReadStamp>>,
}

impl Default for FileTimes {
    fn default() -> Self {
        Self {
            bypass: AtomicBool::new(false),
            stamps: Mutex::new(HashMap::new()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteCheck {
    /// Target does not exist — free to create.
    New,
    /// Read this run and unchanged since.
    Fresh,
    /// Exists but was never read this run.
    NeverRead,
    /// Read this run, but modified on disk since that read.
    Stale,
}

impl FileTimes {
    /// Enable or disable read-before-write checks for this run.
    ///
    /// The setting is runtime policy; AC deliberately does not read a
    /// host-specific environment variable itself.
    pub fn set_bypass(&self, bypass: bool) {
        self.bypass.store(bypass, Ordering::Relaxed);
    }

    pub fn is_bypassed(&self) -> bool {
        self.bypass.load(Ordering::Relaxed)
    }

    pub fn stamp(&self, path: PathBuf, mtime: SystemTime) {
        self.stamp_snapshot(
            path,
            FileSnapshot {
                mtime,
                // Unknown to older callers. A zero sentinel makes
                // `check_write` retain its historical mtime-only behavior;
                // new callers should use `stamp_with_size`.
                size: 0,
            },
        );
    }

    pub fn stamp_with_size(&self, path: PathBuf, mtime: SystemTime, size: u64) {
        self.stamp_snapshot(path, FileSnapshot { mtime, size });
    }

    pub fn stamp_snapshot(&self, path: PathBuf, observed: FileSnapshot) {
        if self.is_bypassed() {
            return;
        }
        self.stamps
            .lock()
            .expect("file-times lock poisoned")
            .insert(
                path,
                ReadStamp {
                    observed,
                    read_at: SystemTime::now(),
                },
            );
    }

    pub fn check_write(&self, path: &Path, current_mtime: Option<SystemTime>) -> WriteCheck {
        if self.is_bypassed() {
            return if current_mtime.is_some() {
                WriteCheck::Fresh
            } else {
                WriteCheck::New
            };
        }
        let map = self.stamps.lock().expect("file-times lock poisoned");
        match (map.get(path), current_mtime) {
            (_, None) => WriteCheck::New,
            (None, Some(_)) => WriteCheck::NeverRead,
            (Some(read), Some(current)) => {
                if read.observed.mtime == current {
                    WriteCheck::Fresh
                } else {
                    WriteCheck::Stale
                }
            }
        }
    }

    /// Validate an impending overwrite against the last successful read.
    ///
    /// `None` means a new target and always succeeds. Existing targets require
    /// a stamp whose mtime and, when known, size still match.
    pub fn assert_write(
        &self,
        path: &Path,
        current: Option<FileSnapshot>,
    ) -> Result<(), FileTimeError> {
        if self.is_bypassed() || current.is_none() {
            return Ok(());
        }
        let current = current.expect("checked above");
        let stamp = self
            .stamps
            .lock()
            .expect("file-times lock poisoned")
            .get(path)
            .cloned()
            .ok_or_else(|| FileTimeError::NeverRead {
                path: path.to_path_buf(),
            })?;
        let size_matches = stamp.observed.size == 0 || stamp.observed.size == current.size;
        if stamp.observed.mtime == current.mtime && size_matches {
            Ok(())
        } else {
            Err(FileTimeError::Stale {
                path: path.to_path_buf(),
                last_modified: current.mtime,
                last_read: stamp.read_at,
            })
        }
    }
}

/// Lexically resolve `requested` against `base` without touching the
/// filesystem. Absolute requests are normalized as-is; relative requests are
/// joined to `base`; `.` is dropped and `..` pops one component.
///
/// File tools use this to key a path consistently before it exists. Hosts may
/// also use it for lexical display and policy adapters.
pub fn file_time_key(base: &Path, requested: &str) -> PathBuf {
    let requested = Path::new(requested);
    let path = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        base.join(requested)
    };
    lexical_normalize(&path)
}

pub fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// `Date.prototype.toISOString` shape, used by the shared freshness error.
pub fn iso8601_ms(t: SystemTime) -> String {
    let ms = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let (days, rem_ms) = (ms.div_euclid(86_400_000), ms.rem_euclid(86_400_000));
    let (year, month, day) = civil_from_days(days);
    let (h, m, s, milli) = (
        rem_ms / 3_600_000,
        rem_ms / 60_000 % 60,
        rem_ms / 1000 % 60,
        rem_ms % 1000,
    );
    format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}.{milli:03}Z")
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SubtreePolicy;

    #[test]
    fn extensions_roundtrip() {
        struct HostState(u32);
        let extensions = Extensions::default();
        assert!(extensions.get::<HostState>().is_none());
        extensions.insert(HostState(7));
        assert_eq!(extensions.get::<HostState>().unwrap().0, 7);
    }

    #[test]
    fn invocation_clones_share_run_state_but_isolate_call_identity() {
        struct HostState(u32);

        let policy = Arc::new(SubtreePolicy::new("/tmp").unwrap());
        let ctx = ToolCtx::new(policy);
        ctx.extensions.insert(HostState(7));
        let first = ctx.for_invocation("call_1");
        let second = ctx.for_invocation("call_2");

        assert_eq!(ctx.tool_call_id(), None);
        assert_eq!(first.tool_call_id(), Some("call_1"));
        assert_eq!(second.tool_call_id(), Some("call_2"));
        assert_eq!(first.extensions.get::<HostState>().unwrap().0, 7);
        assert!(Arc::ptr_eq(&first.extensions, &second.extensions));
        assert!(Arc::ptr_eq(&first.file_times, &second.file_times));
        assert!(Arc::ptr_eq(&first.locks, &second.locks));
    }

    #[test]
    fn write_check_lifecycle() {
        let times = FileTimes::default();
        let path = PathBuf::from("/x/y.txt");
        let t0 = SystemTime::UNIX_EPOCH;
        let t1 = t0 + std::time::Duration::from_secs(1);

        assert_eq!(times.check_write(&path, None), WriteCheck::New);
        assert_eq!(times.check_write(&path, Some(t0)), WriteCheck::NeverRead);
        times.stamp(path.clone(), t0);
        assert_eq!(times.check_write(&path, Some(t0)), WriteCheck::Fresh);
        assert_eq!(times.check_write(&path, Some(t1)), WriteCheck::Stale);
    }

    #[test]
    fn lexical_file_key_normalizes_relative_and_absolute_paths() {
        let base = Path::new("/ws/project");
        assert_eq!(
            file_time_key(base, "./assets/../notes.txt"),
            PathBuf::from("/ws/project/notes.txt")
        );
        assert_eq!(
            file_time_key(base, "../sibling.txt"),
            PathBuf::from("/ws/sibling.txt")
        );
        assert_eq!(
            file_time_key(base, "/absolute/x"),
            PathBuf::from("/absolute/x")
        );
    }
}
