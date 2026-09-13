//! Shared workspace traversal and file-open boundary.
use std::fs::File;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

use super::SecurityFilter;

pub fn max_source_bytes() -> u64 {
    std::env::var("LATTICE_MAX_INDEX_FILE_BYTES")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .unwrap_or(2 * 1024 * 1024)
}

/// Open each component relative to an already-open parent without following
/// symlinks. A rename cannot redirect a subsequent open outside that parent.
pub fn open_source(root: &Path, relative: &Path) -> io::Result<File> {
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "source path must be a normal repository-relative path",
        ));
    }
    let relative_text = relative.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "source path must be valid UTF-8",
        )
    })?;
    validate_ignore_policy(root, relative)?;
    let filter = SecurityFilter::new(root);
    if filter.is_excluded(relative_text) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "source is excluded by workspace policy",
        ));
    }
    #[cfg(unix)]
    let file = {
        use std::ffi::CString;
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::os::unix::ffi::OsStrExt;
        let canonical_root = root.canonicalize()?;
        let root_name = CString::new(canonical_root.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in workspace root"))?;
        // SAFETY: root_name is NUL terminated and open returns a fresh descriptor.
        let root_fd = unsafe {
            libc::open(
                root_name.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            )
        };
        if root_fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful open returns a fresh owned descriptor.
        let mut parent = unsafe { File::from_raw_fd(root_fd) };
        let mut parts = relative.components().peekable();
        while let Some(part) = parts.next() {
            let name = CString::new(part.as_os_str().as_bytes())
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in source path"))?;
            let flags = libc::O_RDONLY
                | libc::O_CLOEXEC
                | libc::O_NOFOLLOW
                | if parts.peek().is_some() {
                    libc::O_DIRECTORY
                } else {
                    libc::O_NONBLOCK
                };
            // SAFETY: parent owns a live directory descriptor and name is NUL terminated.
            let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: successful openat returns a fresh owned descriptor.
            parent = unsafe { File::from_raw_fd(fd) };
        }
        parent
    };
    #[cfg(not(unix))]
    let file = {
        let canonical_root = root.canonicalize()?;
        let mut path = canonical_root.clone();
        for part in relative.components() {
            path.push(part);
            if std::fs::symlink_metadata(&path)?.file_type().is_symlink() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "source symlinks are excluded",
                ));
            }
        }
        if !path.canonicalize()?.starts_with(canonical_root) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "source is outside workspace",
            ));
        }
        File::open(path)?
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "source is not a regular file",
        ));
    }
    if metadata.len() > max_source_bytes() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "source exceeds configured byte limit",
        ));
    }
    Ok(file)
}

pub fn read_source(root: &Path, relative: &Path) -> io::Result<String> {
    let file = open_source(root, relative)?;
    let mut bytes = Vec::new();
    file.take(max_source_bytes().saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_source_bytes() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "source grew beyond configured byte limit",
        ));
    }
    String::from_utf8(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Apply the same lexical, ignore, and language policy used by traversal and
/// race-safe reads to an event path before it reaches the watcher pipeline.
pub fn allows_source_path(root: &Path, relative: &Path) -> bool {
    let Some(relative_text) = relative.to_str() else {
        return false;
    };
    !relative.as_os_str().is_empty()
        && relative
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
        && crate::watcher::should_index_file(relative_text)
        && !SecurityFilter::new(root).is_excluded(relative_text)
}

/// Ignore discovery is hierarchical; symbolic links are never traversed.
/// Read errors remain errors so callers cannot claim complete coverage.
pub fn collect_sources(root: &Path) -> io::Result<Vec<PathBuf>> {
    let filter = SecurityFilter::new(root);
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .follow_links(false)
        .hidden(false)
        .require_git(false)
        // Ignore files are evaluated by SecurityFilter for both traversal and
        // direct reads. Disabling the walker's implicit loader prevents a
        // symlinked ignore file from importing rules outside the workspace.
        .git_ignore(false)
        .git_exclude(false)
        .git_global(false)
        .ignore(false);
    let boundary = root.to_path_buf();
    builder.filter_entry(move |entry| {
        let Some(relative) = entry.path().strip_prefix(&boundary).ok() else {
            return false;
        };
        !filter.is_excluded(&relative.to_string_lossy())
    });
    let mut paths = Vec::new();
    for entry in builder.build() {
        let entry = entry.map_err(io::Error::other)?;
        if entry.file_type().is_some_and(|kind| kind.is_file()) {
            let relative = entry.path().strip_prefix(root).map_err(io::Error::other)?;
            validate_ignore_policy(root, relative)?;
            let relative_text = relative.to_str().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "source path is not valid UTF-8")
            })?;
            if crate::watcher::should_index_file(relative_text) {
                paths.push(entry.into_path());
            }
        }
    }
    paths.sort();
    Ok(paths)
}

fn validate_ignore_policy(root: &Path, relative: &Path) -> io::Result<()> {
    let mut directories = vec![PathBuf::new()];
    let mut current = PathBuf::new();
    for component in relative.components() {
        current.push(component);
        if current != relative {
            directories.push(current.clone());
        }
    }
    for directory in directories {
        let absolute = root.join(&directory);
        let mut builder = ignore::gitignore::GitignoreBuilder::new(&absolute);
        for name in [".gitignore", ".lattice_ignore", ".latticeignore"] {
            let path = absolute.join(name);
            let metadata = match std::fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            if metadata.file_type().is_symlink() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("ignore policy file '{}' is a symlink", path.display()),
                ));
            }
            if metadata.is_file() {
                if let Some(error) = builder.add(&path) {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, error));
                }
            }
        }
        builder
            .build()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    }
    Ok(())
}
