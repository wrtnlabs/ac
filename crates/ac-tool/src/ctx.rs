use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::SystemTime;

use tokio_util::sync::CancellationToken;

use crate::policy::PathPolicy;

/// What every tool receives. One ToolCtx per run; tools share it.
pub struct ToolCtx {
    pub policy: Arc<dyn PathPolicy>,
    pub extensions: Extensions,
    pub file_times: FileTimes,
    pub cancel: CancellationToken,
}

impl ToolCtx {
    pub fn new(policy: Arc<dyn PathPolicy>) -> Self {
        Self {
            policy,
            extensions: Extensions::default(),
            file_times: FileTimes::default(),
            cancel: CancellationToken::new(),
        }
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
