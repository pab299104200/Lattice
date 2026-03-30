#[cfg(test)]
mod tests;

use std::path::PathBuf;
use tokio::sync::mpsc;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use crate::symbols::Language;

#[derive(Debug, Clone)]
pub struct FileEvent {
    pub path: PathBuf,
    pub kind: FileEventKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FileEventKind {
    Created,
    Modified,
    Deleted,
}

pub const EXCLUDED_DIRS: &[&str] = &[
    "node_modules", ".git", "target", "dist", "build", "out",
    "__pycache__", ".venv", "venv", ".tox", ".mypy_cache",
    ".next", ".nuxt", ".svelte-kit", "coverage", ".lattice",
    ".claude", ".codex",
];

const EXCLUDED_PATTERNS: &[&str] = &[
    ".env", "credentials", "id_rsa", "id_ed25519",
    ".pem", ".key", ".pfx", ".p12", ".jks",
];

/// Returns true if the given path should be indexed.
///
/// Checks that the path:
/// 1. Does not contain any excluded directory components
/// 2. Does not match any excluded security patterns in the filename
/// 3. Has a supported language extension
pub fn should_index_file(path: &str) -> bool {
    // Normalize path separators
    let normalized = path.replace('\\', "/");

    // Check excluded dirs — any path component matching an excluded dir disqualifies
    for component in normalized.split('/') {
        for excluded in EXCLUDED_DIRS {
            if component == *excluded {
                return false;
            }
        }
    }

    // Extract the filename (last component)
    let filename = match normalized.rsplit('/').next() {
        Some(f) => f,
        None => return false,
    };

    // Check excluded patterns — if filename contains any pattern, disqualify
    for pattern in EXCLUDED_PATTERNS {
        if filename.contains(pattern) {
            return false;
        }
    }

    // Check for supported language extension
    let ext = match filename.rsplit('.').next() {
        Some(e) => e,
        None => return false,
    };

    let lang = Language::from_extension(ext);
    !matches!(lang, Language::Unknown)
}

/// Start a file system watcher on the given root directory.
///
/// Returns the watcher handle and a receiver for file events.
/// Events are filtered through `should_index_file`.
pub fn start_watcher(
    root: PathBuf,
) -> anyhow::Result<(RecommendedWatcher, mpsc::UnboundedReceiver<FileEvent>)> {
    let (tx, rx) = mpsc::unbounded_channel();

    let root_clone = root.clone();
    let mut watcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
        let event = match res {
            Ok(e) => e,
            Err(_) => return,
        };

        let kind = match event.kind {
            EventKind::Create(_) => FileEventKind::Created,
            EventKind::Modify(_) => FileEventKind::Modified,
            EventKind::Remove(_) => FileEventKind::Deleted,
            _ => return,
        };

        for path in event.paths {
            // Convert to a relative-ish string for filtering
            let rel = path
                .strip_prefix(&root_clone)
                .unwrap_or(&path)
                .to_string_lossy()
                .to_string();

            if should_index_file(&rel) {
                let _ = tx.send(FileEvent {
                    path: path.clone(),
                    kind: kind.clone(),
                });
            }
        }
    })?;

    watcher.watch(&root, RecursiveMode::Recursive)?;

    Ok((watcher, rx))
}
