use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, RwLock};

#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    #[error("path escapes the permitted root: {0}")]
    Outside(String),
    #[error("access denied: {0}")]
    Denied(String),
    #[error("invalid path: {0}")]
    Invalid(String),
}

/// The containment seam. Built-in tools never decide where they may act — the
/// host does, by implementing this. Implementations must be symlink-safe:
/// resolve what exists on disk, not just the lexical path.
pub trait PathPolicy: Send + Sync {
    /// Base directory for resolving relative paths (and for display). Owned,
    /// not borrowed — a policy whose target can be swapped at runtime (see
    /// [`SwapPolicy`]) cannot lend a reference into itself.
    fn root(&self) -> PathBuf;
    fn resolve_read(&self, path: &Path) -> Result<PathBuf, PolicyError>;
    fn resolve_write(&self, path: &Path) -> Result<PathBuf, PolicyError>;
}

/// The generic-host policy: reads and writes confined to one directory
/// subtree. Symlink-safe — the deepest existing ancestor is canonicalized
/// before the containment check, so a symlink pointing outside the root is
/// rejected even though its lexical path looks contained.
pub struct SubtreePolicy {
    root: PathBuf,
}

impl SubtreePolicy {
    pub fn new(root: impl AsRef<Path>) -> std::io::Result<Self> {
        Ok(Self {
            root: root.as_ref().canonicalize()?,
        })
    }

    fn resolve(&self, path: &Path) -> Result<PathBuf, PolicyError> {
        let joined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };

        // Fold `.`/`..` lexically first so the ancestor walk below never sees
        // them; the containment verdict comes from the canonicalized result,
        // so this cannot loosen the check.
        let normalized = normalize_lexically(&joined)
            .ok_or_else(|| PolicyError::Invalid(joined.display().to_string()))?;

        // Split into deepest existing ancestor (canonicalized, so symlinks in
        // it are resolved) + the not-yet-existing tail.
        let mut existing = normalized.clone();
        let mut tail: Vec<std::ffi::OsString> = Vec::new();
        loop {
            if existing.exists() {
                break;
            }
            match (existing.file_name(), existing.parent()) {
                (Some(name), Some(parent)) => {
                    tail.push(name.to_os_string());
                    existing = parent.to_path_buf();
                }
                _ => return Err(PolicyError::Invalid(joined.display().to_string())),
            }
        }
        let mut resolved = existing
            .canonicalize()
            .map_err(|e| PolicyError::Invalid(format!("{}: {e}", existing.display())))?;
        for component in tail.iter().rev() {
            resolved.push(component);
        }

        if !resolved.starts_with(&self.root) {
            return Err(PolicyError::Outside(joined.display().to_string()));
        }
        Ok(resolved)
    }
}

impl PathPolicy for SubtreePolicy {
    fn root(&self) -> PathBuf {
        self.root.clone()
    }

    fn resolve_read(&self, path: &Path) -> Result<PathBuf, PolicyError> {
        self.resolve(path)
    }

    fn resolve_write(&self, path: &Path) -> Result<PathBuf, PolicyError> {
        self.resolve(path)
    }
}

/// Combinator: reads delegate to the inner policy, writes are always denied.
/// Symlink safety is preserved because resolution itself is delegated. The
/// denial message is model-facing data — it tells the model writes are not
/// permitted *yet*, the shape a host wants while some precondition (its own
/// choosing) is still unmet.
pub struct ReadOnlyPolicy {
    inner: Arc<dyn PathPolicy>,
}

impl ReadOnlyPolicy {
    pub fn new(inner: Arc<dyn PathPolicy>) -> Self {
        Self { inner }
    }
}

impl PathPolicy for ReadOnlyPolicy {
    fn root(&self) -> PathBuf {
        self.inner.root()
    }

    fn resolve_read(&self, path: &Path) -> Result<PathBuf, PolicyError> {
        self.inner.resolve_read(path)
    }

    fn resolve_write(&self, path: &Path) -> Result<PathBuf, PolicyError> {
        Err(PolicyError::Denied(format!(
            "writes are not permitted yet: {}",
            path.display()
        )))
    }
}

/// Combinator: reads *contained* by one policy, writes by another — e.g. read
/// a whole tree, write only one subtree of it. There is a single resolution
/// base: every relative path, read or write, joins against the write policy's
/// root (the directory the agent acts in), so one relative name always denotes
/// one file — a write of `out.txt` and a read of `out.txt` hit the same path.
/// The wider read tree is reached with `..` or absolute paths, which the read
/// policy's *containment* then judges. Symlink safety is preserved because
/// each side delegates resolution to its inner policy.
pub struct SplitPolicy {
    pub read: Arc<dyn PathPolicy>,
    pub write: Arc<dyn PathPolicy>,
}

impl SplitPolicy {
    fn rebase(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.write.root().join(path)
        }
    }
}

impl PathPolicy for SplitPolicy {
    /// The write policy's root — the single base every relative path (read
    /// AND write) resolves against.
    fn root(&self) -> PathBuf {
        self.write.root()
    }

    fn resolve_read(&self, path: &Path) -> Result<PathBuf, PolicyError> {
        self.read.resolve_read(&self.rebase(path))
    }

    fn resolve_write(&self, path: &Path) -> Result<PathBuf, PolicyError> {
        self.write.resolve_write(&self.rebase(path))
    }
}

/// Combinator: a policy whose target can be replaced mid-run. A host keeps an
/// `Arc<SwapPolicy>` and installs that same `Arc` as the ToolCtx's
/// `Arc<dyn PathPolicy>`; a host tool can then [`swap`](SwapPolicy::swap)
/// containment (say, from [`ReadOnlyPolicy`] to a chosen write subtree) with
/// zero runtime changes — every tool sees the new policy on its next resolve.
/// Symlink safety is preserved because resolution delegates to the current
/// inner policy.
pub struct SwapPolicy {
    current: RwLock<Arc<dyn PathPolicy>>,
}

impl SwapPolicy {
    pub fn new(initial: Arc<dyn PathPolicy>) -> Self {
        Self {
            current: RwLock::new(initial),
        }
    }

    pub fn swap(&self, next: Arc<dyn PathPolicy>) {
        *self.current.write().expect("swap-policy lock poisoned") = next;
    }

    pub fn current(&self) -> Arc<dyn PathPolicy> {
        self.current
            .read()
            .expect("swap-policy lock poisoned")
            .clone()
    }
}

// Each method clones the current Arc out of the lock and delegates — the guard
// is never held across the delegated call, so a slow resolve cannot block a
// concurrent swap (or vice versa).
impl PathPolicy for SwapPolicy {
    fn root(&self) -> PathBuf {
        self.current().root()
    }

    fn resolve_read(&self, path: &Path) -> Result<PathBuf, PolicyError> {
        self.current().resolve_read(path)
    }

    fn resolve_write(&self, path: &Path) -> Result<PathBuf, PolicyError> {
        self.current().resolve_write(path)
    }
}

fn normalize_lexically(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> (tempfile::TempDir, SubtreePolicy) {
        let dir = tempfile::tempdir().unwrap();
        let policy = SubtreePolicy::new(dir.path()).unwrap();
        (dir, policy)
    }

    #[test]
    fn relative_paths_resolve_inside_root() {
        let (_dir, policy) = policy();
        let resolved = policy
            .resolve_write(Path::new("new/nested/file.txt"))
            .unwrap();
        assert!(resolved.starts_with(policy.root()));
        assert!(resolved.ends_with("new/nested/file.txt"));
    }

    #[test]
    fn parent_escape_is_rejected() {
        let (_dir, policy) = policy();
        assert!(matches!(
            policy.resolve_write(Path::new("../outside.txt")),
            Err(PolicyError::Outside(_))
        ));
        assert!(matches!(
            policy.resolve_write(Path::new("missing/../../outside.txt")),
            Err(PolicyError::Outside(_))
        ));
    }

    #[test]
    fn absolute_outside_is_rejected() {
        let (_dir, policy) = policy();
        assert!(matches!(
            policy.resolve_read(Path::new("/etc/hosts")),
            Err(PolicyError::Outside(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_rejected() {
        let (dir, policy) = policy();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
        assert!(matches!(
            policy.resolve_write(Path::new("link/file.txt")),
            Err(PolicyError::Outside(_))
        ));
    }

    #[test]
    fn read_only_permits_reads_denies_writes() {
        let (_dir, inner) = policy();
        let root = inner.root();
        let read_only = ReadOnlyPolicy::new(Arc::new(inner));

        let resolved = read_only.resolve_read(Path::new("file.txt")).unwrap();
        assert!(resolved.starts_with(&root));
        assert_eq!(read_only.root(), root);
        assert!(matches!(
            read_only.resolve_write(Path::new("file.txt")),
            Err(PolicyError::Denied(_))
        ));
    }

    #[test]
    fn split_routes_read_and_write_to_different_subtrees() {
        let parent = tempfile::tempdir().unwrap();
        std::fs::create_dir(parent.path().join("inner")).unwrap();
        let read = Arc::new(SubtreePolicy::new(parent.path()).unwrap());
        let write = Arc::new(SubtreePolicy::new(parent.path().join("inner")).unwrap());
        let write_root = write.root();
        let split = SplitPolicy { read, write };

        assert_eq!(split.root(), write_root);
        // One relative name denotes ONE file: a read and a write of the same
        // relative path resolve to the same place (the write root).
        let read_at = split.resolve_read(Path::new("file.txt")).unwrap();
        let wrote_at = split.resolve_write(Path::new("file.txt")).unwrap();
        assert_eq!(read_at, wrote_at);
        assert!(wrote_at.starts_with(&write_root));
        // The wider read tree is reachable with `..` (and absolute paths)...
        let widened = split.resolve_read(Path::new("../sibling.txt")).unwrap();
        assert_eq!(
            widened,
            parent.path().canonicalize().unwrap().join("sibling.txt")
        );
        // ...but the same escape as a WRITE is out, relative or absolute.
        assert!(matches!(
            split.resolve_write(Path::new("../sibling.txt")),
            Err(PolicyError::Outside(_))
        ));
        assert!(matches!(
            split.resolve_write(&parent.path().join("sibling.txt")),
            Err(PolicyError::Outside(_))
        ));
    }

    #[test]
    fn swap_rebinds_the_policy_a_ctx_already_holds() {
        let (_dir, inner) = policy();
        let inner = Arc::new(inner);
        let swap = Arc::new(SwapPolicy::new(Arc::new(ReadOnlyPolicy::new(
            inner.clone(),
        ))));
        // The same Arc, coerced, is what a host installs in the ToolCtx.
        let ctx = crate::ToolCtx::new(swap.clone() as Arc<dyn PathPolicy>);

        assert!(matches!(
            ctx.policy.resolve_write(Path::new("file.txt")),
            Err(PolicyError::Denied(_))
        ));
        swap.swap(inner);
        assert!(ctx.policy.resolve_write(Path::new("file.txt")).is_ok());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_readers_during_swap_do_not_deadlock() {
        let (_dir, inner) = policy();
        let inner = Arc::new(inner);
        let swap = Arc::new(SwapPolicy::new(
            Arc::new(ReadOnlyPolicy::new(inner.clone())) as Arc<dyn PathPolicy>,
        ));

        let mut tasks = Vec::new();
        for _ in 0..4 {
            let swap = swap.clone();
            tasks.push(tokio::spawn(async move {
                for _ in 0..500 {
                    let _ = swap.resolve_read(Path::new("file.txt"));
                    let _ = swap.resolve_write(Path::new("file.txt"));
                }
            }));
        }
        let swapper = {
            let swap = swap.clone();
            let inner = inner.clone();
            tokio::spawn(async move {
                for i in 0..500 {
                    if i % 2 == 0 {
                        swap.swap(inner.clone());
                    } else {
                        swap.swap(Arc::new(ReadOnlyPolicy::new(inner.clone())));
                    }
                }
            })
        };
        tasks.push(swapper);
        for task in tasks {
            tokio::time::timeout(std::time::Duration::from_secs(10), task)
                .await
                .expect("swap contention must not deadlock")
                .unwrap();
        }
    }
}
