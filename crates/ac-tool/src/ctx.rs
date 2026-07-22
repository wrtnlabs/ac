use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::SystemTime;

use tokio_util::sync::CancellationToken;

use crate::agent::AgentSpawner;
use crate::policy::PathPolicy;
use crate::sandbox::SandboxLauncher;

/// What every tool receives. One ToolCtx per run; tools share it.
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
    pub extensions: Extensions,
    pub file_times: FileTimes,
    pub locks: PathLocks,
    pub cancel: CancellationToken,
}

impl ToolCtx {
    pub fn new(policy: Arc<dyn PathPolicy>) -> Self {
        Self {
            policy,
            sandbox: None,
            spawner: None,
            extensions: Extensions::default(),
            file_times: FileTimes::default(),
            locks: PathLocks::default(),
            cancel: CancellationToken::new(),
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

/// Per-run read-before-write ledger. `read`-style tools stamp the mtime they
/// saw; write-style tools check the stamp before overwriting an existing file.
#[derive(Default)]
pub struct FileTimes(Mutex<HashMap<PathBuf, SystemTime>>);

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
    pub fn stamp(&self, path: PathBuf, mtime: SystemTime) {
        self.0
            .lock()
            .expect("file-times lock poisoned")
            .insert(path, mtime);
    }

    pub fn check_write(&self, path: &Path, current_mtime: Option<SystemTime>) -> WriteCheck {
        let map = self.0.lock().expect("file-times lock poisoned");
        match (map.get(path), current_mtime) {
            (_, None) => WriteCheck::New,
            (None, Some(_)) => WriteCheck::NeverRead,
            (Some(read), Some(current)) => {
                if *read == current {
                    WriteCheck::Fresh
                } else {
                    WriteCheck::Stale
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_roundtrip() {
        struct HostState(u32);
        let extensions = Extensions::default();
        assert!(extensions.get::<HostState>().is_none());
        extensions.insert(HostState(7));
        assert_eq!(extensions.get::<HostState>().unwrap().0, 7);
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
}
