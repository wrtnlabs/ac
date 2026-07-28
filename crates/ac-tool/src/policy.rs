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

/// A policy-authorized absolute path together with the directory capability
/// that contains it.
///
/// Path policies still expose [`PathPolicy::resolve_read`] and
/// [`PathPolicy::resolve_write`] for display and compatibility. Filesystem
/// implementations should prefer the `authorize_*` variants: on Unix, AC's
/// stock tools open `root` first and traverse `relative` with descriptor-
/// relative, no-follow operations. That keeps the policy verdict attached to
/// the later I/O even if another process swaps a not-yet-existing parent for a
/// symlink between authorization and use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizedPath {
    root: PathBuf,
    path: PathBuf,
}

impl AuthorizedPath {
    pub fn new(root: PathBuf, path: PathBuf) -> Result<Self, PolicyError> {
        if !root.is_absolute() || !path.is_absolute() || !path.starts_with(&root) {
            return Err(PolicyError::Invalid(format!(
                "{} is not beneath authorization root {}",
                path.display(),
                root.display()
            )));
        }
        let relative = path.strip_prefix(&root).expect("starts_with checked above");
        if relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(PolicyError::Invalid(format!(
                "{} is not a normalized path beneath {}",
                path.display(),
                root.display()
            )));
        }
        Ok(Self { root, path })
    }

    /// Wrap a previously resolved absolute path. The filesystem root is the
    /// capability in this compatibility form; every component is still opened
    /// descriptor-relatively with no-follow semantics on Unix.
    pub fn from_resolved(path: impl Into<PathBuf>) -> Result<Self, PolicyError> {
        let path = path.into();
        let root = filesystem_root(&path)
            .ok_or_else(|| PolicyError::Invalid(path.display().to_string()))?;
        Self::new(root, path)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn relative(&self) -> &Path {
        self.path
            .strip_prefix(&self.root)
            .expect("AuthorizedPath validates containment")
    }
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

    /// Resolve a read and retain an authorization root for race-safe I/O.
    ///
    /// Custom policies get a safe compatibility default rooted at the
    /// filesystem root. Policies that know a narrower capability should
    /// override this, as AC's built-in combinators do.
    fn authorize_read(&self, path: &Path) -> Result<AuthorizedPath, PolicyError> {
        AuthorizedPath::from_resolved(self.resolve_read(path)?)
    }

    /// Resolve a write and retain an authorization root for race-safe I/O.
    fn authorize_write(&self, path: &Path) -> Result<AuthorizedPath, PolicyError> {
        AuthorizedPath::from_resolved(self.resolve_write(path)?)
    }
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

    /// Build a subtree from an identity that was already authorized by a
    /// parent capability. This intentionally does not canonicalize again:
    /// hosts use it when a bind transition must retain the exact path
    /// identity established by an earlier descriptor-relative operation.
    pub fn from_authorized_root(authorized: AuthorizedPath) -> Self {
        Self {
            root: authorized.path,
        }
    }

    fn resolve(&self, path: &Path) -> Result<PathBuf, PolicyError> {
        let joined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };

        let resolved = resolve_on_disk(&joined)?;

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

    fn authorize_read(&self, path: &Path) -> Result<AuthorizedPath, PolicyError> {
        AuthorizedPath::new(self.root.clone(), self.resolve(path)?)
    }

    fn authorize_write(&self, path: &Path) -> Result<AuthorizedPath, PolicyError> {
        AuthorizedPath::new(self.root.clone(), self.resolve(path)?)
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

    fn authorize_read(&self, path: &Path) -> Result<AuthorizedPath, PolicyError> {
        self.inner.authorize_read(path)
    }

    fn authorize_write(&self, path: &Path) -> Result<AuthorizedPath, PolicyError> {
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

    fn authorize_read(&self, path: &Path) -> Result<AuthorizedPath, PolicyError> {
        self.read.authorize_read(&self.rebase(path))
    }

    fn authorize_write(&self, path: &Path) -> Result<AuthorizedPath, PolicyError> {
        self.write.authorize_write(&self.rebase(path))
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

    fn authorize_read(&self, path: &Path) -> Result<AuthorizedPath, PolicyError> {
        self.current().authorize_read(path)
    }

    fn authorize_write(&self, path: &Path) -> Result<AuthorizedPath, PolicyError> {
        self.current().authorize_write(path)
    }
}

/// A shared, grow-only set of read-root grants. A host creates one, installs
/// it in a [`GrantedReadPolicy`], and grants the extra directories reads may
/// resolve into — statically at build time (e.g. the generic host granting
/// its skills roots) or from a host component that earns new read access
/// mid-run. Each grant is canonicalized when added and resolved
/// symlink-safely on use — a grant is a [`SubtreePolicy`] under the hood.
/// Grants only ever widen READS; there is deliberately no write variant.
#[derive(Default)]
pub struct ReadGrants {
    roots: RwLock<Vec<SubtreePolicy>>,
    files: RwLock<Vec<FileGrant>>,
}

/// A single-file grant: the canonicalized parent directory plus the file name.
/// Keeping the name un-resolved (only the parent is canonicalized) means the
/// grant names one directory ENTRY — it matches however the caller spells the
/// intermediate components (symlinks included) and never widens to siblings.
struct FileGrant {
    parent: PathBuf,
    name: std::ffi::OsString,
}

impl ReadGrants {
    pub fn new() -> Self {
        Self::default()
    }

    /// Grant read access under `dir`. The directory must exist (it is
    /// canonicalized here — granting a path that may later appear would let a
    /// symlink planted in the meantime redirect the grant).
    pub fn grant(&self, dir: impl AsRef<Path>) -> std::io::Result<()> {
        let policy = SubtreePolicy::new(dir)?;
        self.insert_root(policy);
        Ok(())
    }

    /// Grant a directory identity already established through a parent
    /// capability, without re-canonicalizing its mutable pathname.
    pub fn grant_authorized(&self, dir: AuthorizedPath) {
        self.insert_root(SubtreePolicy::from_authorized_root(dir));
    }

    fn insert_root(&self, policy: SubtreePolicy) {
        let mut roots = self.roots.write().expect("read-grants lock poisoned");
        if !roots.iter().any(|p| p.root == policy.root) {
            roots.push(policy);
        }
    }

    /// Grant read access to exactly one file — never its siblings, never a
    /// subtree. The parent directory must exist (it is canonicalized here, the
    /// same planted-symlink defense as [`grant`](ReadGrants::grant)); the file
    /// name is kept as spelled, so the grant denotes that directory entry.
    pub fn grant_file(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        let path = path.as_ref();
        let name = path
            .file_name()
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("not a file path: {}", path.display()),
                )
            })?
            .to_os_string();
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("file grant needs a parent directory: {}", path.display()),
                )
            })?
            .canonicalize()?;
        let mut files = self.files.write().expect("read-grants lock poisoned");
        if !files.iter().any(|g| g.parent == parent && g.name == name) {
            files.push(FileGrant { parent, name });
        }
        Ok(())
    }

    /// The canonicalized roots granted so far, in grant order.
    pub fn roots(&self) -> Vec<PathBuf> {
        self.roots
            .read()
            .expect("read-grants lock poisoned")
            .iter()
            .map(|p| p.root.clone())
            .collect()
    }

    fn authorize_read(&self, path: &Path) -> Option<AuthorizedPath> {
        {
            let roots = self.roots.read().expect("read-grants lock poisoned");
            if let Some(resolved) = roots.iter().find_map(|p| p.authorize_read(path).ok()) {
                return Some(resolved);
            }
        }
        // File grants: canonicalize the candidate's parent (so any spelling of
        // the intermediate components lands on the same directory) and compare
        // the entry name against each grant.
        let name = path.file_name()?;
        let parent = path.parent()?.canonicalize().ok()?;
        let files = self.files.read().expect("read-grants lock poisoned");
        files
            .iter()
            .find(|g| g.parent == parent && g.name == name)
            .and_then(|g| {
                // The grant denotes that directory entry, not whatever it may
                // link to: a symlinked entry would leak an arbitrary target
                // with zero containment. Resolve the leaf and require the real
                // file to still be this exact entry.
                let real = g.parent.join(&g.name).canonicalize().ok()?;
                if real.parent() == Some(g.parent.as_path())
                    && real.file_name() == Some(g.name.as_os_str())
                {
                    AuthorizedPath::new(g.parent.clone(), real).ok()
                } else {
                    None
                }
            })
    }
}

/// Combinator: reads that the inner policy denies fall back to a dynamic set
/// of [`ReadGrants`]; writes always go to the inner policy alone — a grant can
/// never widen write access. Relative paths keep resolving against the inner
/// policy's root (granted directories are reached by absolute path), so a
/// grant never changes what a relative name denotes. Symlink safety is
/// preserved on both sides: the inner policy resolves as before, and each
/// grant resolves through its own [`SubtreePolicy`].
pub struct GrantedReadPolicy {
    inner: Arc<dyn PathPolicy>,
    grants: Arc<ReadGrants>,
}

impl GrantedReadPolicy {
    pub fn new(inner: Arc<dyn PathPolicy>, grants: Arc<ReadGrants>) -> Self {
        Self { inner, grants }
    }
}

impl PathPolicy for GrantedReadPolicy {
    fn root(&self) -> PathBuf {
        self.inner.root()
    }

    fn resolve_read(&self, path: &Path) -> Result<PathBuf, PolicyError> {
        self.authorize_read(path).map(|authorized| authorized.path)
    }

    fn resolve_write(&self, path: &Path) -> Result<PathBuf, PolicyError> {
        self.inner.resolve_write(path)
    }

    fn authorize_read(&self, path: &Path) -> Result<AuthorizedPath, PolicyError> {
        match self.inner.authorize_read(path) {
            Ok(resolved) => Ok(resolved),
            Err(inner_err) => {
                if path.is_absolute()
                    && let Some(resolved) = self.grants.authorize_read(path)
                {
                    return Ok(resolved);
                }
                Err(inner_err)
            }
        }
    }

    fn authorize_write(&self, path: &Path) -> Result<AuthorizedPath, PolicyError> {
        self.inner.authorize_write(path)
    }
}

/// Combinator: mount other policies under virtual name prefixes. A RELATIVE
/// candidate whose leading path segment equals a mount's prefix (full-segment
/// match — `auxx/f` does not match mount `aux`) has the prefix stripped and
/// the remainder judged by that mount's policy, whose own containment applies
/// (`aux/../escape` is the mount's refusal, never a hop into another tree).
/// Everything else — absolute paths included — delegates to the inner policy
/// untouched, so a mount never shadows a real path. Lets a host expose side
/// trees under stable virtual names without them living inside the primary
/// root. Symlink safety is preserved because resolution is always delegated.
pub struct PrefixRemapPolicy {
    pub inner: Arc<dyn PathPolicy>,
    pub mounts: Vec<(String, Arc<dyn PathPolicy>)>,
}

impl PrefixRemapPolicy {
    fn route(&self, path: &Path) -> Option<(&dyn PathPolicy, PathBuf)> {
        if path.is_absolute() {
            return None;
        }
        // "aux/f" and "./aux/f" must denote the same file: `strip_prefix`
        // treats a leading `.` as a mismatch, which would silently route the
        // dotted spelling to the inner tree instead of the mount. Fold CurDir
        // components before matching.
        let folded: PathBuf = path
            .components()
            .filter(|c| !matches!(c, Component::CurDir))
            .collect();
        self.mounts.iter().find_map(|(prefix, policy)| {
            folded
                .strip_prefix(prefix)
                .ok()
                .map(|rest| (policy.as_ref(), rest.to_path_buf()))
        })
    }
}

impl PathPolicy for PrefixRemapPolicy {
    fn root(&self) -> PathBuf {
        self.inner.root()
    }

    fn resolve_read(&self, path: &Path) -> Result<PathBuf, PolicyError> {
        match self.route(path) {
            Some((mount, rest)) => mount.resolve_read(&rest),
            None => self.inner.resolve_read(path),
        }
    }

    fn resolve_write(&self, path: &Path) -> Result<PathBuf, PolicyError> {
        match self.route(path) {
            Some((mount, rest)) => mount.resolve_write(&rest),
            None => self.inner.resolve_write(path),
        }
    }

    fn authorize_read(&self, path: &Path) -> Result<AuthorizedPath, PolicyError> {
        match self.route(path) {
            Some((mount, rest)) => mount.authorize_read(&rest),
            None => self.inner.authorize_read(path),
        }
    }

    fn authorize_write(&self, path: &Path) -> Result<AuthorizedPath, PolicyError> {
        match self.route(path) {
            Some((mount, rest)) => mount.authorize_write(&rest),
            None => self.inner.authorize_write(path),
        }
    }
}

/// Combinator: a deny-list applied AFTER the inner policy resolves — the check
/// runs on the resolved real path, so a symlink that resolves into a denied
/// subtree is caught even though its lexical path looks clean. A denied entry
/// covers itself and its whole subtree. Reads and writes carry separate lists
/// (denying writes says nothing about reads, and vice versa). Entries are
/// host-supplied absolute paths, resolved against disk on every check so an
/// entry that is itself (or contains) a symlink keeps tracking its target.
pub struct DenyPolicy {
    pub inner: Arc<dyn PathPolicy>,
    pub deny_write: Vec<PathBuf>,
    pub deny_read: Vec<PathBuf>,
}

impl DenyPolicy {
    fn check(
        resolved: PathBuf,
        denies: &[PathBuf],
        candidate: &Path,
    ) -> Result<PathBuf, PolicyError> {
        for deny in denies {
            // Fail closed: a deny entry that cannot be resolved (bad spelling,
            // permission error on an ancestor) must refuse the access, not
            // silently drop out of the deny set — an unevaluable deny is not
            // an absent one.
            let denied = resolve_on_disk(deny).map_err(|e| {
                PolicyError::Denied(format!(
                    "deny entry {} could not be resolved (failing closed): {e}",
                    deny.display()
                ))
            })?;
            if resolved.starts_with(&denied) {
                return Err(PolicyError::Denied(format!(
                    "path is on the deny list: {}",
                    candidate.display()
                )));
            }
        }
        Ok(resolved)
    }

    fn check_authorized(
        authorized: AuthorizedPath,
        denies: &[PathBuf],
        candidate: &Path,
    ) -> Result<AuthorizedPath, PolicyError> {
        Self::check(authorized.path.clone(), denies, candidate)?;
        Ok(authorized)
    }
}

impl PathPolicy for DenyPolicy {
    fn root(&self) -> PathBuf {
        self.inner.root()
    }

    fn resolve_read(&self, path: &Path) -> Result<PathBuf, PolicyError> {
        Self::check(self.inner.resolve_read(path)?, &self.deny_read, path)
    }

    fn resolve_write(&self, path: &Path) -> Result<PathBuf, PolicyError> {
        Self::check(self.inner.resolve_write(path)?, &self.deny_write, path)
    }

    fn authorize_read(&self, path: &Path) -> Result<AuthorizedPath, PolicyError> {
        Self::check_authorized(self.inner.authorize_read(path)?, &self.deny_read, path)
    }

    fn authorize_write(&self, path: &Path) -> Result<AuthorizedPath, PolicyError> {
        Self::check_authorized(self.inner.authorize_write(path)?, &self.deny_write, path)
    }
}

fn filesystem_root(path: &Path) -> Option<PathBuf> {
    let mut components = path.components();
    match components.next()? {
        Component::Prefix(prefix) => {
            let mut root = PathBuf::from(prefix.as_os_str());
            if matches!(components.next(), Some(Component::RootDir)) {
                root.push(Component::RootDir.as_os_str());
                Some(root)
            } else {
                None
            }
        }
        Component::RootDir => Some(PathBuf::from(Component::RootDir.as_os_str())),
        _ => None,
    }
}

/// Resolve `path` against what exists on disk: canonicalize the deepest
/// existing ancestor (so symlinks in it are followed) and re-append the
/// not-yet-existing tail. `.`/`..` are folded lexically first so the ancestor
/// walk never sees them; any verdict a caller takes from the result rests on
/// the canonicalized ancestor, so the lexical fold cannot loosen a check.
fn resolve_on_disk(path: &Path) -> Result<PathBuf, PolicyError> {
    let normalized = normalize_lexically(path)
        .ok_or_else(|| PolicyError::Invalid(path.display().to_string()))?;

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
            _ => return Err(PolicyError::Invalid(path.display().to_string())),
        }
    }
    let mut resolved = existing
        .canonicalize()
        .map_err(|e| PolicyError::Invalid(format!("{}: {e}", existing.display())))?;
    for component in tail.iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
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
        let authorized = policy
            .authorize_write(Path::new("new/nested/file.txt"))
            .unwrap();
        assert_eq!(authorized.root(), policy.root());
        assert_eq!(authorized.path(), resolved);
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
        let widened_authorized = split.authorize_read(Path::new("../sibling.txt")).unwrap();
        assert_eq!(
            widened_authorized.root(),
            parent.path().canonicalize().unwrap()
        );
        assert_eq!(widened_authorized.path(), widened);
        assert_eq!(
            split.authorize_write(Path::new("file.txt")).unwrap().root(),
            write_root
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

    #[test]
    fn granted_reads_widen_reads_only_and_only_after_the_grant() {
        let (_dir, inner) = policy();
        let inner_root = inner.root();
        let grants = Arc::new(ReadGrants::new());
        let granted = GrantedReadPolicy::new(Arc::new(inner), grants.clone());

        let outside = tempfile::tempdir().unwrap();
        let outside_root = outside.path().canonicalize().unwrap();
        std::fs::write(outside_root.join("companion.md"), "data").unwrap();
        let companion = outside_root.join("companion.md");

        // Before the grant: denied, with the inner policy's own error.
        assert!(matches!(
            granted.resolve_read(&companion),
            Err(PolicyError::Outside(_))
        ));

        grants.grant(&outside_root).unwrap();
        assert_eq!(grants.roots(), vec![outside_root.clone()]);

        // After: the read resolves — but the same path as a WRITE stays denied.
        assert_eq!(granted.resolve_read(&companion).unwrap(), companion);
        let authorized = granted.authorize_read(&companion).unwrap();
        assert_eq!(authorized.root(), outside_root);
        assert_eq!(authorized.path(), companion);
        assert!(matches!(
            granted.resolve_write(&companion),
            Err(PolicyError::Outside(_))
        ));

        // Relative paths still denote inner-root files, grant or no grant.
        let relative = granted.resolve_read(Path::new("companion.md")).unwrap();
        assert!(relative.starts_with(&inner_root));

        // Granting the same root twice does not duplicate it.
        grants.grant(&outside_root).unwrap();
        assert_eq!(grants.roots().len(), 1);

        // A grant target must exist at grant time.
        assert!(grants.grant(outside_root.join("missing")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_inside_a_granted_root_cannot_escape_it() {
        let (_dir, inner) = policy();
        let grants = Arc::new(ReadGrants::new());
        let granted = GrantedReadPolicy::new(Arc::new(inner), grants.clone());

        let grant_dir = tempfile::tempdir().unwrap();
        let grant_root = grant_dir.path().canonicalize().unwrap();
        let secret_dir = tempfile::tempdir().unwrap();
        std::fs::write(secret_dir.path().join("secret"), "s").unwrap();
        std::os::unix::fs::symlink(secret_dir.path(), grant_root.join("link")).unwrap();
        grants.grant(&grant_root).unwrap();

        assert!(matches!(
            granted.resolve_read(&grant_root.join("link/secret")),
            Err(PolicyError::Outside(_))
        ));
    }

    #[test]
    fn prefix_remap_routes_reads_and_writes_through_a_mount() {
        let primary = tempfile::tempdir().unwrap();
        let mount_dir = tempfile::tempdir().unwrap();
        std::fs::write(mount_dir.path().join("f.txt"), "m").unwrap();
        let inner = Arc::new(SubtreePolicy::new(primary.path()).unwrap());
        let mount = Arc::new(SubtreePolicy::new(mount_dir.path()).unwrap());
        let mount_root = mount.root();
        let remap = PrefixRemapPolicy {
            inner: inner.clone(),
            mounts: vec![("aux".into(), mount)],
        };

        assert_eq!(remap.root(), inner.root());
        assert_eq!(
            remap.resolve_read(Path::new("aux/f.txt")).unwrap(),
            mount_root.join("f.txt")
        );
        assert_eq!(
            remap.authorize_read(Path::new("aux/f.txt")).unwrap().root(),
            mount_root
        );
        let wrote = remap.resolve_write(Path::new("aux/new.txt")).unwrap();
        assert!(wrote.starts_with(&mount_root));
        // The bare prefix denotes the mount root itself.
        assert_eq!(remap.resolve_read(Path::new("aux")).unwrap(), mount_root);
    }

    #[test]
    fn prefix_remap_matches_full_segments_only() {
        let primary = tempfile::tempdir().unwrap();
        let mount_dir = tempfile::tempdir().unwrap();
        let inner = Arc::new(SubtreePolicy::new(primary.path()).unwrap());
        let inner_root = inner.root();
        let mount = Arc::new(SubtreePolicy::new(mount_dir.path()).unwrap());
        let remap = PrefixRemapPolicy {
            inner,
            mounts: vec![("aux".into(), mount)],
        };

        // "auxx/f" must NOT match mount "aux" — it is an inner-tree path.
        let resolved = remap.resolve_write(Path::new("auxx/f.txt")).unwrap();
        assert!(resolved.starts_with(&inner_root));
    }

    #[test]
    fn prefix_remap_escape_via_a_mount_is_refused_by_the_mount() {
        let primary = tempfile::tempdir().unwrap();
        let mount_dir = tempfile::tempdir().unwrap();
        let inner = Arc::new(SubtreePolicy::new(primary.path()).unwrap());
        let mount = Arc::new(SubtreePolicy::new(mount_dir.path()).unwrap());
        let remap = PrefixRemapPolicy {
            inner,
            mounts: vec![("aux".into(), mount)],
        };

        assert!(matches!(
            remap.resolve_read(Path::new("aux/../outside.txt")),
            Err(PolicyError::Outside(_))
        ));
        assert!(matches!(
            remap.resolve_write(Path::new("aux/../outside.txt")),
            Err(PolicyError::Outside(_))
        ));
    }

    #[test]
    fn prefix_remap_absolute_paths_bypass_the_mounts() {
        let primary = tempfile::tempdir().unwrap();
        let mount_dir = tempfile::tempdir().unwrap();
        std::fs::write(mount_dir.path().join("f.txt"), "m").unwrap();
        let inner = Arc::new(SubtreePolicy::new(primary.path()).unwrap());
        let inner_root = inner.root();
        let mount = Arc::new(SubtreePolicy::new(mount_dir.path()).unwrap());
        let mount_root = mount.root();
        let remap = PrefixRemapPolicy {
            inner,
            mounts: vec![("aux".into(), mount)],
        };

        // Absolute inside the inner root: judged (and admitted) by inner.
        let resolved = remap.resolve_read(&inner_root.join("x.txt")).unwrap();
        assert!(resolved.starts_with(&inner_root));
        // Absolute inside the MOUNT's real root: still judged by inner — the
        // mount only exists under its virtual name.
        assert!(matches!(
            remap.resolve_read(&mount_root.join("f.txt")),
            Err(PolicyError::Outside(_))
        ));
    }

    #[test]
    fn deny_covers_the_entry_and_its_subtree_per_access_kind() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("secret")).unwrap();
        let inner = Arc::new(SubtreePolicy::new(dir.path()).unwrap());
        let deny = DenyPolicy {
            inner: inner.clone(),
            deny_write: vec![dir.path().join("secret")],
            deny_read: vec![],
        };

        assert_eq!(deny.root(), inner.root());
        // The entry itself and anything under it are write-denied...
        assert!(matches!(
            deny.resolve_write(Path::new("secret")),
            Err(PolicyError::Denied(_))
        ));
        assert!(matches!(
            deny.resolve_write(Path::new("secret/f.txt")),
            Err(PolicyError::Denied(_))
        ));
        // ...while reads (a separate list) and sibling writes pass through.
        assert!(deny.resolve_read(Path::new("secret/f.txt")).is_ok());
        assert!(deny.resolve_write(Path::new("ok.txt")).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn deny_catches_a_symlink_resolving_into_a_denied_subtree() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("secret")).unwrap();
        std::fs::write(dir.path().join("secret/f.txt"), "s").unwrap();
        std::os::unix::fs::symlink(dir.path().join("secret"), dir.path().join("link")).unwrap();
        let inner = Arc::new(SubtreePolicy::new(dir.path()).unwrap());
        let deny = DenyPolicy {
            inner,
            deny_write: vec![],
            deny_read: vec![dir.path().join("secret")],
        };

        // The lexical path never mentions the denied subtree; the resolved
        // real path does, and that is what the deny check sees.
        assert!(matches!(
            deny.resolve_read(Path::new("link/f.txt")),
            Err(PolicyError::Denied(_))
        ));
    }

    #[test]
    fn prefix_remap_dotted_spelling_routes_identically() {
        let primary = tempfile::tempdir().unwrap();
        let mount_dir = tempfile::tempdir().unwrap();
        std::fs::write(mount_dir.path().join("f.txt"), "m").unwrap();
        let inner = Arc::new(SubtreePolicy::new(primary.path()).unwrap());
        let mount = Arc::new(SubtreePolicy::new(mount_dir.path()).unwrap());
        let mount_root = mount.root();
        let remap = PrefixRemapPolicy {
            inner,
            mounts: vec![("aux".into(), mount)],
        };

        // One name, one file: a leading `./` must not reroute to the inner
        // tree (a model spelling difference would otherwise fork the mount).
        assert_eq!(
            remap.resolve_read(Path::new("./aux/f.txt")).unwrap(),
            remap.resolve_read(Path::new("aux/f.txt")).unwrap(),
        );
        let wrote = remap.resolve_write(Path::new("./aux/new.txt")).unwrap();
        assert!(wrote.starts_with(&mount_root));
    }

    #[test]
    fn deny_fails_closed_when_an_entry_cannot_be_resolved() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "x").unwrap();
        let inner = Arc::new(SubtreePolicy::new(dir.path()).unwrap());
        // `/..`-prefixed: lexical normalization pops past the root and the
        // entry cannot be resolved. It must refuse the access, not silently
        // drop out of the deny set.
        let bad_entry = Path::new("/..").join(
            dir.path()
                .strip_prefix("/")
                .unwrap_or(dir.path())
                .join("f.txt"),
        );
        let deny = DenyPolicy {
            inner,
            deny_write: vec![],
            deny_read: vec![bad_entry],
        };

        assert!(matches!(
            deny.resolve_read(Path::new("f.txt")),
            Err(PolicyError::Denied(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_file_grant_refuses_a_symlinked_entry() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("target.txt"), "secret").unwrap();
        let grant_dir = tempfile::tempdir().unwrap();
        std::fs::write(grant_dir.path().join("sibling.txt"), "s").unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("target.txt"),
            grant_dir.path().join("link.txt"),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            grant_dir.path().join("sibling.txt"),
            grant_dir.path().join("local-link.txt"),
        )
        .unwrap();
        let (_dir, inner) = policy();
        let grants = Arc::new(ReadGrants::new());
        grants
            .grant_file(grant_dir.path().join("link.txt"))
            .unwrap();
        grants
            .grant_file(grant_dir.path().join("local-link.txt"))
            .unwrap();
        let granted = GrantedReadPolicy::new(Arc::new(inner), grants);

        // The grant denotes the entry, not its target: a symlink out of the
        // directory (or onto an ungranted sibling) must not be followed.
        assert!(
            granted
                .resolve_read(&grant_dir.path().join("link.txt"))
                .is_err()
        );
        assert!(
            granted
                .resolve_read(&grant_dir.path().join("local-link.txt"))
                .is_err()
        );
    }

    #[test]
    fn a_file_grant_covers_exactly_that_file() {
        let (_dir, inner) = policy();
        let grants = Arc::new(ReadGrants::new());
        let granted = GrantedReadPolicy::new(Arc::new(inner), grants.clone());

        let outside = tempfile::tempdir().unwrap();
        let outside_root = outside.path().canonicalize().unwrap();
        std::fs::write(outside_root.join("f.txt"), "granted").unwrap();
        std::fs::write(outside_root.join("sibling.txt"), "not granted").unwrap();
        std::fs::create_dir(outside_root.join("sub")).unwrap();
        std::fs::write(outside_root.join("sub/inner.txt"), "not granted").unwrap();

        grants.grant_file(outside_root.join("f.txt")).unwrap();

        // Exactly the granted file resolves...
        assert_eq!(
            granted.resolve_read(&outside_root.join("f.txt")).unwrap(),
            outside_root.join("f.txt")
        );
        // ...its sibling does not, nor a file one level down.
        assert!(
            granted
                .resolve_read(&outside_root.join("sibling.txt"))
                .is_err()
        );
        assert!(
            granted
                .resolve_read(&outside_root.join("sub/inner.txt"))
                .is_err()
        );
        // A file grant never widens writes.
        assert!(granted.resolve_write(&outside_root.join("f.txt")).is_err());
        // The parent directory must exist at grant time.
        assert!(
            grants
                .grant_file(outside_root.join("missing/f.txt"))
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_file_grant_matches_through_symlinked_parent_components() {
        let (_dir, inner) = policy();
        let grants = Arc::new(ReadGrants::new());
        let granted = GrantedReadPolicy::new(Arc::new(inner), grants.clone());

        let real = tempfile::tempdir().unwrap();
        let real_root = real.path().canonicalize().unwrap();
        std::fs::write(real_root.join("f.txt"), "granted").unwrap();
        let link_holder = tempfile::tempdir().unwrap();
        let link = link_holder.path().join("link");
        std::os::unix::fs::symlink(&real_root, &link).unwrap();

        // Granted via the symlinked spelling; the canonical parent is stored.
        grants.grant_file(link.join("f.txt")).unwrap();

        // Both spellings resolve to the same real file...
        assert_eq!(
            granted.resolve_read(&link.join("f.txt")).unwrap(),
            real_root.join("f.txt")
        );
        assert_eq!(
            granted.resolve_read(&real_root.join("f.txt")).unwrap(),
            real_root.join("f.txt")
        );
        // ...and a sibling stays refused through either spelling.
        std::fs::write(real_root.join("sibling.txt"), "no").unwrap();
        assert!(granted.resolve_read(&link.join("sibling.txt")).is_err());
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
