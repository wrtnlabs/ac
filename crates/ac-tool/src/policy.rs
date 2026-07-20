use std::path::{Component, Path, PathBuf};

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
    /// Base directory for resolving relative paths (and for display).
    fn root(&self) -> &Path;
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
    fn root(&self) -> &Path {
        &self.root
    }

    fn resolve_read(&self, path: &Path) -> Result<PathBuf, PolicyError> {
        self.resolve(path)
    }

    fn resolve_write(&self, path: &Path) -> Result<PathBuf, PolicyError> {
        self.resolve(path)
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
}
