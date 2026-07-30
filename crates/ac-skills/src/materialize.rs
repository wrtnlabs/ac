//! Bounded, symlink-safe projection of resolved skill trees.
//!
//! Some hosts cannot expose source skill roots directly. This module builds a
//! complete snapshot beside the destination, then publishes that snapshot as
//! one directory generation. Cooperative processes serialize with an advisory
//! lock. Linux and Apple platforms use an atomic directory exchange when
//! replacing an existing generation. Other platforms use a rollback-safe pair
//! of renames: files are never partially written, though the destination root
//! can be briefly unavailable between those renames. On Unix, regular-file
//! permission bits (including executability) are copied into every generation.

use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(unix)]
use std::ffi::CString;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use crate::Skill;

static NEXT_CONTROL_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy)]
pub struct MaterializeLimits {
    pub max_skills: usize,
    pub max_depth: usize,
    pub max_entries_per_skill: usize,
    pub max_file_bytes: u64,
    pub max_skill_bytes: u64,
    pub max_total_bytes: u64,
}

impl Default for MaterializeLimits {
    fn default() -> Self {
        Self {
            max_skills: 256,
            max_depth: 16,
            max_entries_per_skill: 4096,
            max_file_bytes: 16 * 1024 * 1024,
            max_skill_bytes: 64 * 1024 * 1024,
            max_total_bytes: 256 * 1024 * 1024,
        }
    }
}

/// Mirror resolved skill directories into `destination/<dir_name>/`.
///
/// The destination is derived state. Each call publishes one complete
/// generation, so rejected or removed skills do not leave stale projections.
/// Existing destination roots must be real directories: a symlink or any
/// other file type is rejected in place and is never removed.
///
/// Writers that use this function are serialized across threads and processes
/// by a persistent sibling lock file. The lock is advisory; unrelated writers
/// that ignore it remain outside this function's guarantees.
pub fn materialize_skill_trees(
    skills: &[Skill],
    destination: &Path,
    limits: MaterializeLimits,
) -> Vec<(String, std::io::Error)> {
    match materialize_locked(skills, destination, limits) {
        Ok(failures) => failures,
        Err(error) => vec![("skills".to_string(), error)],
    }
}

fn materialize_locked(
    skills: &[Skill],
    destination: &Path,
    limits: MaterializeLimits,
) -> std::io::Result<Vec<(String, std::io::Error)>> {
    let parent = destination_parent(destination)?;
    fs::create_dir_all(&parent)?;
    let lock = open_lock_file(destination)?;
    File::lock(&lock)?;

    let destination_existed = validate_destination_root(destination)?;
    let stage = create_control_dir(destination, "stage")?;
    let mut cleanup = CleanupPath::new(stage.clone());
    let mut failures = Vec::new();
    let mut materialized_names = HashSet::new();
    let mut total_bytes = 0_u64;

    for skill in skills.iter().take(limits.max_skills) {
        if !is_safe_dir_name(&skill.dir_name) {
            failures.push((
                skill.dir_name.clone(),
                invalid_skill_tree(format!(
                    "skill directory name {:?} is not one safe path component",
                    skill.dir_name
                )),
            ));
            continue;
        }
        if !materialized_names.insert(OsString::from(&skill.dir_name)) {
            failures.push((
                skill.dir_name.clone(),
                invalid_skill_tree(format!(
                    "duplicate materialized skill directory {:?}",
                    skill.dir_name
                )),
            ));
            continue;
        }

        let target = stage.join(&skill.dir_name);
        match copy_skill_tree(&skill.source_dir, &target, limits, total_bytes) {
            Ok(bytes) => total_bytes += bytes,
            Err(error) => {
                let _ = remove_path_no_follow(&target);
                failures.push((skill.dir_name.clone(), error));
            }
        }
    }
    if skills.len() > limits.max_skills {
        failures.push((
            "skills".to_string(),
            invalid_skill_tree(format!(
                "resolved skill count {} exceeds limit of {}",
                skills.len(),
                limits.max_skills,
            )),
        ));
    }

    // Revalidate after staging. This closes cooperative races and refuses an
    // externally substituted root rather than overwriting it.
    if validate_destination_root(destination)? != destination_existed {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "destination root changed while materializing skills",
        ));
    }
    publish_snapshot(&stage, destination, destination_existed)?;
    if let Err(error) = cleanup.cleanup_now() {
        failures.push(("skills".to_string(), error));
    }
    Ok(failures)
}

fn destination_parent(destination: &Path) -> std::io::Result<PathBuf> {
    if destination.file_name().is_none() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "skills destination must have a final path component",
        ));
    }
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    if parent.as_os_str().is_empty() {
        Ok(PathBuf::from("."))
    } else {
        Ok(parent.to_path_buf())
    }
}

fn open_lock_file(destination: &Path) -> std::io::Result<File> {
    let parent = destination_parent(destination)?;
    let mut name = OsString::from(".");
    name.push(
        destination
            .file_name()
            .expect("destination_parent validated the final component"),
    );
    name.push(".ac-skills.lock");
    let path = parent.join(name);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("skills lock path is not a regular file: {}", path.display()),
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    options.open(path)
}

/// Returns whether the destination currently exists.
fn validate_destination_root(destination: &Path) -> std::io::Result<bool> {
    match fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "skills destination root must not be a symlink: {}",
                destination.display()
            ),
        )),
        Ok(metadata) if !metadata.is_dir() => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "skills destination root is not a directory: {}",
                destination.display()
            ),
        )),
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn is_safe_dir_name(name: &str) -> bool {
    let mut components = Path::new(name).components();
    matches!(components.next(), Some(Component::Normal(component)) if component == name)
        && components.next().is_none()
}

fn create_control_dir(destination: &Path, kind: &str) -> std::io::Result<PathBuf> {
    let parent = destination_parent(destination)?;
    let base = destination
        .file_name()
        .expect("destination_parent validated the final component");
    for _ in 0..128 {
        let id = NEXT_CONTROL_ID.fetch_add(1, Ordering::Relaxed);
        let mut name = OsString::from(".");
        name.push(base);
        name.push(format!(".ac-skills-{kind}-{}-{id}", std::process::id()));
        let path = parent.join(name);
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not allocate a unique skills staging directory",
    ))
}

fn publish_snapshot(
    stage: &Path,
    destination: &Path,
    destination_existed: bool,
) -> std::io::Result<()> {
    if !destination_existed {
        return fs::rename(stage, destination);
    }
    if atomic_exchange(stage, destination)? {
        return Ok(());
    }

    // Portable fallback. The old generation is restored if publishing the new
    // one fails, but readers may observe a brief NotFound between renames.
    let backup = create_control_dir(destination, "backup")?;
    fs::remove_dir(&backup)?;
    fs::rename(destination, &backup)?;
    if let Err(publish_error) = fs::rename(stage, destination) {
        return match fs::rename(&backup, destination) {
            Ok(()) => Err(publish_error),
            Err(rollback_error) => Err(std::io::Error::other(format!(
                "cannot publish skills snapshot ({publish_error}); \
                 cannot restore prior snapshot ({rollback_error})"
            ))),
        };
    }
    remove_path_no_follow(&backup)
}

#[cfg(target_os = "linux")]
fn atomic_exchange(left: &Path, right: &Path) -> std::io::Result<bool> {
    let left = path_c_string(left)?;
    let right = path_c_string(right)?;
    // SAFETY: both C strings are live, NUL-terminated path buffers. renameat2
    // does not retain either pointer.
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            left.as_ptr(),
            libc::AT_FDCWD,
            right.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    exchange_result(result)
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn atomic_exchange(left: &Path, right: &Path) -> std::io::Result<bool> {
    let left = path_c_string(left)?;
    let right = path_c_string(right)?;
    // SAFETY: both C strings are live, NUL-terminated path buffers.
    // renamex_np does not retain either pointer.
    let result = unsafe { libc::renamex_np(left.as_ptr(), right.as_ptr(), libc::RENAME_SWAP) };
    exchange_result(result)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "ios")))]
fn atomic_exchange(_left: &Path, _right: &Path) -> std::io::Result<bool> {
    Ok(false)
}

#[cfg(unix)]
fn path_c_string(path: &Path) -> std::io::Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("path contains an interior NUL: {}", path.display()),
        )
    })
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "ios"))]
fn exchange_result(result: libc::c_int) -> std::io::Result<bool> {
    if result == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    let unsupported = error.raw_os_error().is_some_and(|code| {
        code == libc::ENOSYS
            || code == libc::EINVAL
            || code == libc::ENOTSUP
            || code == libc::EOPNOTSUPP
    });
    if unsupported { Ok(false) } else { Err(error) }
}

#[derive(Default)]
struct SkillTreeBudget {
    entries: usize,
    bytes: u64,
}

fn invalid_skill_tree(message: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message.into())
}

fn copy_skill_tree(
    root: &Path,
    destination: &Path,
    limits: MaterializeLimits,
    prior_total_bytes: u64,
) -> std::io::Result<u64> {
    let metadata = fs::symlink_metadata(root)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("skill root is not a real directory: {}", root.display()),
        ));
    }
    fs::create_dir(destination)?;
    let mut budget = SkillTreeBudget::default();
    copy_dir(root, destination, 0, limits, prior_total_bytes, &mut budget)?;
    Ok(budget.bytes)
}

fn copy_dir(
    source: &Path,
    destination: &Path,
    depth: usize,
    limits: MaterializeLimits,
    prior_total_bytes: u64,
    budget: &mut SkillTreeBudget,
) -> std::io::Result<()> {
    if depth > limits.max_depth {
        return Err(invalid_skill_tree(format!(
            "skill tree exceeds depth limit of {}",
            limits.max_depth
        )));
    }
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let metadata = fs::symlink_metadata(&source_path)?;
        if metadata.file_type().is_symlink() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "skill companion symlinks are not allowed: {}",
                    source_path.display()
                ),
            ));
        }
        budget.entries = budget
            .entries
            .checked_add(1)
            .ok_or_else(|| invalid_skill_tree("skill tree entry count overflow"))?;
        if budget.entries > limits.max_entries_per_skill {
            return Err(invalid_skill_tree(format!(
                "skill tree exceeds entry limit of {}",
                limits.max_entries_per_skill
            )));
        }

        let target = destination.join(entry.file_name());
        if metadata.is_dir() {
            fs::create_dir(&target)?;
            copy_dir(
                &source_path,
                &target,
                depth + 1,
                limits,
                prior_total_bytes,
                budget,
            )?;
        } else if metadata.is_file() {
            copy_file_bounded(
                &source_path,
                &target,
                &metadata,
                limits,
                prior_total_bytes,
                budget,
            )?;
        } else {
            return Err(invalid_skill_tree(format!(
                "unsupported skill companion file type: {}",
                source_path.display()
            )));
        }
    }
    Ok(())
}

fn copy_file_bounded(
    source_path: &Path,
    target: &Path,
    metadata: &fs::Metadata,
    limits: MaterializeLimits,
    prior_total_bytes: u64,
    budget: &mut SkillTreeBudget,
) -> std::io::Result<()> {
    let skill_remaining = limits.max_skill_bytes.saturating_sub(budget.bytes);
    let aggregate_used = prior_total_bytes
        .checked_add(budget.bytes)
        .ok_or_else(|| invalid_skill_tree("materialized skill byte count overflow"))?;
    let aggregate_remaining = limits.max_total_bytes.saturating_sub(aggregate_used);
    let allowed = limits
        .max_file_bytes
        .min(skill_remaining)
        .min(aggregate_remaining);
    if metadata.len() > allowed {
        return Err(invalid_skill_tree(format!(
            "skill file exceeds remaining byte limit of {allowed}: {}",
            source_path.display()
        )));
    }

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    let mut source = options.open(source_path)?;
    let mut target_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)?;
    let mut copied = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = source.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        copied = copied
            .checked_add(read as u64)
            .ok_or_else(|| invalid_skill_tree("skill file byte count overflow"))?;
        if copied > allowed {
            return Err(invalid_skill_tree(format!(
                "skill file grew beyond remaining byte limit of {allowed}: {}",
                source_path.display()
            )));
        }
        target_file.write_all(&buffer[..read])?;
    }
    target_file.flush()?;
    set_unix_permissions(target, metadata)?;
    budget.bytes = budget
        .bytes
        .checked_add(copied)
        .ok_or_else(|| invalid_skill_tree("skill tree byte count overflow"))?;
    Ok(())
}

#[cfg(unix)]
fn set_unix_permissions(path: &Path, metadata: &fs::Metadata) -> std::io::Result<()> {
    fs::set_permissions(
        path,
        fs::Permissions::from_mode(metadata.permissions().mode() & 0o7777),
    )
}

#[cfg(not(unix))]
fn set_unix_permissions(_path: &Path, _metadata: &fs::Metadata) -> std::io::Result<()> {
    Ok(())
}

/// Remove a derived path without following a symlink at any level.
fn remove_path_no_follow(path: &Path) -> std::io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return fs::remove_file(path);
    }
    // std's recursive remover explicitly does not follow directory symlinks;
    // use that hardened implementation instead of a path-by-path recursion.
    fs::remove_dir_all(path)
}

struct CleanupPath {
    path: Option<PathBuf>,
}

impl CleanupPath {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn cleanup_now(&mut self) -> std::io::Result<()> {
        let Some(path) = self.path.take() else {
            return Ok(());
        };
        remove_path_no_follow(&path)
    }
}

impl Drop for CleanupPath {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = remove_path_no_follow(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SkillLayer, SkillsResolver};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

    fn write_skill(root: &Path, name: &str, body: &str) {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: Test.\n---\n{body}"),
        )
        .unwrap();
    }

    fn resolve(root: &Path) -> Vec<Skill> {
        SkillsResolver::direct_children(vec![SkillLayer {
            name: "test".to_string(),
            root: root.to_path_buf(),
        }])
        .list()
        .skills
    }

    #[test]
    fn exact_snapshot_replaces_same_length_bytes_and_prunes_stale_entries() {
        let source = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        write_skill(source.path(), "alpha", "AAAA");
        fs::write(source.path().join("alpha/old.txt"), "old").unwrap();
        assert!(
            materialize_skill_trees(
                &resolve(source.path()),
                destination.path(),
                MaterializeLimits::default(),
            )
            .is_empty()
        );

        write_skill(source.path(), "alpha", "BBBB");
        fs::remove_file(source.path().join("alpha/old.txt")).unwrap();
        assert!(
            materialize_skill_trees(
                &resolve(source.path()),
                destination.path(),
                MaterializeLimits::default(),
            )
            .is_empty()
        );
        let projected = fs::read_to_string(destination.path().join("alpha/SKILL.md")).unwrap();
        assert!(projected.ends_with("BBBB"));
        assert!(!destination.path().join("alpha/old.txt").exists());
    }

    #[test]
    fn no_longer_resolved_skill_directories_are_pruned() {
        let source = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        write_skill(source.path(), "alpha", "one");
        write_skill(source.path(), "beta", "two");
        assert!(
            materialize_skill_trees(
                &resolve(source.path()),
                destination.path(),
                MaterializeLimits::default(),
            )
            .is_empty()
        );
        fs::remove_dir_all(source.path().join("alpha")).unwrap();
        assert!(
            materialize_skill_trees(
                &resolve(source.path()),
                destination.path(),
                MaterializeLimits::default(),
            )
            .is_empty()
        );
        assert!(!destination.path().join("alpha").exists());
        assert!(destination.path().join("beta/SKILL.md").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn companion_symlinks_are_rejected_and_leave_no_projection() {
        let source = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        write_skill(source.path(), "alpha", "body");
        fs::write(outside.path().join("secret"), "secret").unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret"),
            source.path().join("alpha/escape"),
        )
        .unwrap();
        let failures = materialize_skill_trees(
            &resolve(source.path()),
            destination.path(),
            MaterializeLimits::default(),
        );
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].0, "alpha");
        assert!(!destination.path().join("alpha").exists());
    }

    #[test]
    fn file_and_aggregate_limits_fail_closed() {
        let source = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        write_skill(source.path(), "alpha", "0123456789");
        let limits = MaterializeLimits {
            max_file_bytes: 8,
            max_skill_bytes: 32,
            max_total_bytes: 32,
            ..MaterializeLimits::default()
        };
        let failures = materialize_skill_trees(&resolve(source.path()), destination.path(), limits);
        assert_eq!(failures.len(), 1);
        assert!(!destination.path().join("alpha").exists());
    }

    #[cfg(unix)]
    #[test]
    fn destination_root_symlink_is_rejected_without_touching_its_target() {
        let source = tempfile::tempdir().unwrap();
        let parent = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        write_skill(source.path(), "alpha", "body");
        fs::write(outside.path().join("sentinel"), "keep").unwrap();
        let destination = parent.path().join("skills");
        std::os::unix::fs::symlink(outside.path(), &destination).unwrap();

        let failures = materialize_skill_trees(
            &resolve(source.path()),
            &destination,
            MaterializeLimits::default(),
        );
        assert_eq!(failures.len(), 1);
        assert!(
            fs::symlink_metadata(&destination)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read_to_string(outside.path().join("sentinel")).unwrap(),
            "keep"
        );
        assert!(!outside.path().join("alpha").exists());
    }

    #[test]
    fn destination_root_non_directory_is_rejected_in_place() {
        let source = tempfile::tempdir().unwrap();
        let parent = tempfile::tempdir().unwrap();
        write_skill(source.path(), "alpha", "body");
        let destination = parent.path().join("skills");
        fs::write(&destination, "keep").unwrap();
        let failures = materialize_skill_trees(
            &resolve(source.path()),
            &destination,
            MaterializeLimits::default(),
        );
        assert_eq!(failures.len(), 1);
        assert_eq!(fs::read_to_string(destination).unwrap(), "keep");
    }

    #[test]
    fn traversal_shaped_manual_dir_name_cannot_escape_destination() {
        let source = tempfile::tempdir().unwrap();
        let parent = tempfile::tempdir().unwrap();
        write_skill(source.path(), "alpha", "body");
        let mut malicious = resolve(source.path()).remove(0);
        malicious.dir_name = "../outside".to_string();
        let destination = parent.path().join("skills");
        fs::create_dir(&destination).unwrap();
        fs::write(parent.path().join("outside"), "keep").unwrap();

        let failures =
            materialize_skill_trees(&[malicious], &destination, MaterializeLimits::default());
        assert_eq!(failures.len(), 1);
        assert_eq!(
            fs::read_to_string(parent.path().join("outside")).unwrap(),
            "keep"
        );
    }

    #[cfg(unix)]
    #[test]
    fn stale_destination_symlink_is_unlinked_without_traversing_target() {
        let destination = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("sentinel"), "keep").unwrap();
        std::os::unix::fs::symlink(outside.path(), destination.path().join("stale")).unwrap();
        assert!(
            materialize_skill_trees(&[], destination.path(), MaterializeLimits::default(),)
                .is_empty()
        );
        assert_eq!(
            fs::read_to_string(outside.path().join("sentinel")).unwrap(),
            "keep"
        );
        assert!(!destination.path().join("stale").exists());
    }

    #[cfg(unix)]
    #[test]
    fn executable_mode_and_chmod_only_changes_are_preserved() {
        let source = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        write_skill(source.path(), "alpha", "body");
        let script = source.path().join("alpha/run.sh");
        fs::write(&script, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            materialize_skill_trees(
                &resolve(source.path()),
                destination.path(),
                MaterializeLimits::default(),
            )
            .is_empty()
        );
        let projected = destination.path().join("alpha/run.sh");
        assert_eq!(
            fs::metadata(&projected).unwrap().permissions().mode() & 0o777,
            0o755
        );

        fs::set_permissions(&script, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            materialize_skill_trees(
                &resolve(source.path()),
                destination.path(),
                MaterializeLimits::default(),
            )
            .is_empty()
        );
        assert_eq!(
            fs::metadata(projected).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }

    #[test]
    fn concurrent_writers_never_publish_truncated_files() {
        let source_a = tempfile::tempdir().unwrap();
        let source_b = tempfile::tempdir().unwrap();
        let parent = tempfile::tempdir().unwrap();
        let destination = parent.path().join("skills");
        let payload_a = vec![b'A'; 512 * 1024];
        let payload_b = vec![b'B'; 512 * 1024];
        write_skill(source_a.path(), "alpha", "A");
        write_skill(source_b.path(), "alpha", "B");
        fs::write(source_a.path().join("alpha/payload"), &payload_a).unwrap();
        fs::write(source_b.path().join("alpha/payload"), &payload_b).unwrap();
        let skills_a = Arc::new(resolve(source_a.path()));
        let skills_b = Arc::new(resolve(source_b.path()));
        assert!(
            materialize_skill_trees(&skills_a, &destination, MaterializeLimits::default(),)
                .is_empty()
        );

        let running = Arc::new(AtomicBool::new(true));
        let reader_path = destination.join("alpha/payload");
        let reader_running = Arc::clone(&running);
        let expected_a = payload_a.clone();
        let expected_b = payload_b.clone();
        let reader = std::thread::spawn(move || {
            while reader_running.load(AtomicOrdering::Acquire) {
                match fs::read(&reader_path) {
                    Ok(bytes) => assert!(bytes == expected_a || bytes == expected_b),
                    // The documented portable fallback can briefly remove the
                    // root; it still never exposes partial file contents.
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => panic!("unexpected reader error: {error}"),
                }
            }
        });

        let mut writers = Vec::new();
        for skills in [skills_a, skills_b] {
            let writer_destination = destination.clone();
            writers.push(std::thread::spawn(move || {
                for _ in 0..12 {
                    assert!(
                        materialize_skill_trees(
                            &skills,
                            &writer_destination,
                            MaterializeLimits::default(),
                        )
                        .is_empty()
                    );
                }
            }));
        }
        for writer in writers {
            writer.join().unwrap();
        }
        running.store(false, AtomicOrdering::Release);
        reader.join().unwrap();
    }
}
