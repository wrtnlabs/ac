//! Race-safe filesystem access for paths already authorized by `ac-tool`.
//!
//! On Unix every component is opened relative to a held directory descriptor
//! with `O_NOFOLLOW`; missing write parents are created with `mkdirat` and
//! immediately reopened the same way. A concurrent rename can make an
//! operation fail, but cannot redirect it through a planted symlink.
//!
//! Non-Unix targets retain the old path-based behavior. `AuthorizedPath`
//! remains the boundary there, but the standard library does not expose the
//! descriptor-relative primitives needed for the Unix hardening. Keep this
//! fallback explicit until AC has a Windows handle-relative implementation.

use std::fs::File;
use std::io;
use std::path::Path;

use ac_tool::AuthorizedPath;

pub(crate) struct RootedPath {
    authorized: AuthorizedPath,
}

impl RootedPath {
    pub(crate) fn new(authorized: AuthorizedPath) -> Self {
        Self { authorized }
    }

    pub(crate) fn path(&self) -> &Path {
        self.authorized.path()
    }

    pub(crate) fn open_read(&self) -> io::Result<File> {
        imp::open_read(&self.authorized)
    }

    pub(crate) fn open_existing_write(&self) -> io::Result<File> {
        imp::open_existing_write(&self.authorized)
    }

    pub(crate) fn create_new(&self) -> io::Result<File> {
        imp::create_new(&self.authorized)
    }

    pub(crate) fn ensure_dir(&self) -> io::Result<()> {
        imp::ensure_dir(&self.authorized)
    }

    pub(crate) fn atomic_replace(&self, bytes: &[u8]) -> io::Result<()> {
        imp::atomic_replace(&self.authorized, bytes)
    }

    #[cfg(unix)]
    pub(crate) fn open_dir(&self) -> io::Result<File> {
        imp::open_dir(&self.authorized)
    }
}

#[cfg(unix)]
mod imp {
    use std::ffi::OsStr;
    use std::fs::File;
    use std::io::{self, Write as _};
    use std::os::fd::OwnedFd;
    use std::path::{Component, Path};

    use ac_tool::AuthorizedPath;
    use rustix::fs::{AtFlags, Mode, OFlags, mkdirat, open, openat, renameat, unlinkat};

    const DIR_FLAGS: OFlags = OFlags::RDONLY
        .union(OFlags::DIRECTORY)
        .union(OFlags::CLOEXEC)
        .union(OFlags::NOFOLLOW);

    fn components(path: &Path) -> io::Result<Vec<&OsStr>> {
        path.components()
            .map(|component| match component {
                Component::Normal(name) => Ok(name),
                _ => Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "authorized relative path contains a non-normal component: {}",
                        path.display()
                    ),
                )),
            })
            .collect()
    }

    fn open_absolute_dir(path: &Path) -> io::Result<OwnedFd> {
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("authorization root is not absolute: {}", path.display()),
            ));
        }
        let mut dir = open(Path::new("/"), DIR_FLAGS, Mode::empty()).map_err(io::Error::from)?;
        for component in path.components() {
            match component {
                Component::RootDir => {}
                Component::Normal(name) => {
                    dir = openat(&dir, name, DIR_FLAGS, Mode::empty()).map_err(io::Error::from)?;
                }
                Component::CurDir => {}
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "authorization root contains an unsupported component: {}",
                            path.display()
                        ),
                    ));
                }
            }
        }
        Ok(dir)
    }

    fn open_child_dir(parent: &OwnedFd, name: &OsStr, create: bool) -> io::Result<OwnedFd> {
        match openat(parent, name, DIR_FLAGS, Mode::empty()) {
            Ok(dir) => Ok(dir),
            Err(error) if create && error == rustix::io::Errno::NOENT => {
                match mkdirat(parent, name, Mode::from_raw_mode(0o777)) {
                    Ok(()) => {}
                    // Another writer may have created the entry. Reopening it
                    // with O_NOFOLLOW below decides whether it is a directory.
                    Err(error) if error == rustix::io::Errno::EXIST => {}
                    Err(error) => return Err(io::Error::from(error)),
                }
                openat(parent, name, DIR_FLAGS, Mode::empty()).map_err(io::Error::from)
            }
            Err(error) => Err(io::Error::from(error)),
        }
    }

    fn open_parent(authorized: &AuthorizedPath, create: bool) -> io::Result<(OwnedFd, &OsStr)> {
        let parts = components(authorized.relative())?;
        let (leaf, parents) = parts.split_last().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "path names an authorization root: {}",
                    authorized.path().display()
                ),
            )
        })?;
        let mut dir = open_absolute_dir(authorized.root())?;
        for component in parents {
            dir = open_child_dir(&dir, component, create)?;
        }
        Ok((dir, leaf))
    }

    pub(super) fn open_read(authorized: &AuthorizedPath) -> io::Result<File> {
        if authorized.relative().as_os_str().is_empty() {
            return open_absolute_dir(authorized.root()).map(File::from);
        }
        let (parent, leaf) = open_parent(authorized, false)?;
        openat(
            &parent,
            leaf,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map(File::from)
        .map_err(io::Error::from)
    }

    pub(super) fn open_existing_write(authorized: &AuthorizedPath) -> io::Result<File> {
        let (parent, leaf) = open_parent(authorized, false)?;
        openat(
            &parent,
            leaf,
            OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map(File::from)
        .map_err(io::Error::from)
    }

    pub(super) fn create_new(authorized: &AuthorizedPath) -> io::Result<File> {
        let (parent, leaf) = open_parent(authorized, true)?;
        openat(
            &parent,
            leaf,
            OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::from_raw_mode(0o666),
        )
        .map(File::from)
        .map_err(io::Error::from)
    }

    pub(super) fn ensure_dir(authorized: &AuthorizedPath) -> io::Result<()> {
        let mut dir = open_absolute_dir(authorized.root())?;
        for component in components(authorized.relative())? {
            dir = open_child_dir(&dir, component, true)?;
        }
        Ok(())
    }

    pub(super) fn atomic_replace(authorized: &AuthorizedPath, bytes: &[u8]) -> io::Result<()> {
        let (parent, leaf) = open_parent(authorized, false)?;
        let tmp_name = format!(".ac-tmp-{}", uuid::Uuid::new_v4().simple());
        let mut tmp = openat(
            &parent,
            tmp_name.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::from_raw_mode(0o666),
        )
        .map(File::from)
        .map_err(io::Error::from)?;
        let result = (|| {
            tmp.write_all(bytes)?;
            tmp.flush()?;
            renameat(&parent, tmp_name.as_str(), &parent, leaf).map_err(io::Error::from)
        })();
        if result.is_err() {
            let _ = unlinkat(&parent, tmp_name.as_str(), AtFlags::empty());
        }
        result
    }

    pub(super) fn open_dir(authorized: &AuthorizedPath) -> io::Result<File> {
        if authorized.relative().as_os_str().is_empty() {
            return open_absolute_dir(authorized.root()).map(File::from);
        }
        let mut dir = open_absolute_dir(authorized.root())?;
        for component in components(authorized.relative())? {
            dir = open_child_dir(&dir, component, false)?;
        }
        Ok(File::from(dir))
    }
}

#[cfg(not(unix))]
mod imp {
    use std::fs::{File, OpenOptions};
    use std::io::{self, Write as _};

    use ac_tool::AuthorizedPath;

    pub(super) fn open_read(authorized: &AuthorizedPath) -> io::Result<File> {
        File::open(authorized.path())
    }

    pub(super) fn open_existing_write(authorized: &AuthorizedPath) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(authorized.path())
    }

    pub(super) fn create_new(authorized: &AuthorizedPath) -> io::Result<File> {
        if let Some(parent) = authorized.path().parent() {
            std::fs::create_dir_all(parent)?;
        }
        OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(authorized.path())
    }

    pub(super) fn ensure_dir(authorized: &AuthorizedPath) -> io::Result<()> {
        std::fs::create_dir_all(authorized.path())
    }

    pub(super) fn atomic_replace(authorized: &AuthorizedPath, bytes: &[u8]) -> io::Result<()> {
        let parent = authorized
            .path()
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "target has no parent"))?;
        let tmp = parent.join(format!(".ac-tmp-{}", uuid::Uuid::new_v4().simple()));
        let result = (|| {
            let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
            file.write_all(bytes)?;
            file.flush()?;
            std::fs::rename(&tmp, authorized.path())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(tmp);
        }
        result
    }
}

#[cfg(all(test, not(unix)))]
mod fallback_tests {
    use super::*;

    #[test]
    fn documented_non_unix_fallback_preserves_basic_read_and_create() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let existing = root.join("existing.txt");
        std::fs::write(&existing, b"inside").unwrap();

        let readable = RootedPath::new(AuthorizedPath::new(root.clone(), existing).unwrap());
        assert_eq!(
            std::fs::read(readable.open_read().unwrap()).unwrap(),
            b"inside"
        );

        let created = root.join("nested").join("new.txt");
        let writable = RootedPath::new(AuthorizedPath::new(root, created.clone()).unwrap());
        std::io::Write::write_all(&mut writable.create_new().unwrap(), b"new").unwrap();
        assert_eq!(std::fs::read(created).unwrap(), b"new");
    }
}
