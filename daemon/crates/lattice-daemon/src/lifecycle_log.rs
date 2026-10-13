#![allow(dead_code)]

use serde_json::{json, Map, Value};
use std::fs::{create_dir_all, metadata, rename, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

static LIFECYCLE_LOG_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
const DEFAULT_MAX_LOG_BYTES: u64 = 32 * 1024 * 1024;

/// The lifecycle log is best-effort and must never crash the daemon, but a
/// silently blank audit trail is its own operational failure — it is the first
/// thing read when diagnosing a start/crash loop. So the first time a write
/// fails we surface it via `tracing::warn`. The latch keeps a persistent
/// failure (a full disk, a read-only mount) from flooding the real log on every
/// subsequent event; one warning is enough to tell the operator the trail is no
/// longer trustworthy.
static WRITE_FAILURE_WARNED: AtomicBool = AtomicBool::new(false);

fn warn_lifecycle_write_failure_once(reason: &str, detail: &dyn std::fmt::Display) {
    if !WRITE_FAILURE_WARNED.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            reason,
            detail = %detail,
            "lifecycle audit log write failed; the lifecycle trail may be incomplete from here"
        );
    }
}

pub(crate) fn log_event(role: &str, event: &str, fields: &[(&str, Value)]) {
    if should_skip_event(event) {
        return;
    }

    let path = lifecycle_log_path();
    if let Some(parent) = path.parent() {
        if let Err(error) = create_dir_all(parent) {
            warn_lifecycle_write_failure_once("create_log_directory", &error);
        }
    }

    let mut entry = Map::new();
    entry.insert(
        "ts_epoch_ms".to_string(),
        json!(SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0)),
    );
    entry.insert("pid".to_string(), json!(std::process::id()));
    entry.insert("role".to_string(), json!(role));
    entry.insert("event".to_string(), json!(event));
    for (key, value) in fields {
        entry.insert((*key).to_string(), value.clone());
    }

    let Ok(_guard) = LIFECYCLE_LOG_LOCK.get_or_init(|| Mutex::new(())).lock() else {
        // A poisoned lock means another thread panicked mid-write; surface it
        // rather than silently dropping the event.
        warn_lifecycle_write_failure_once("lock_poisoned", &"lifecycle log mutex poisoned");
        return;
    };

    rotate_if_oversized(&path);

    let line = match serde_json::to_string(&entry) {
        Ok(line) => line,
        Err(error) => {
            warn_lifecycle_write_failure_once("serialize_event", &error);
            return;
        }
    };
    if let Err(error) = append_event_line(&path, &line) {
        warn_lifecycle_write_failure_once("append_event", &error);
    }
}

/// Append one serialized line to the log, returning the I/O result so the
/// caller can surface a failure instead of silently dropping the event. This
/// is the single write seam: previously the open and write errors were both
/// discarded here with `let _ = ...`.
fn append_event_line(path: &PathBuf, line: &str) -> std::io::Result<()> {
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(file, "{line}")
}

fn should_skip_event(event: &str) -> bool {
    if verbose_lifecycle_logging_enabled() {
        return false;
    }

    matches!(
        event,
        "proxy_connection_accepted"
            | "proxy_hello_received"
            | "daemon_reconnected"
            | "daemon_connection_closed"
    )
}

fn verbose_lifecycle_logging_enabled() -> bool {
    matches!(
        std::env::var("LATTICE_VERBOSE_LIFECYCLE_LOG")
            .ok()
            .as_deref(),
        Some("1") | Some("true") | Some("TRUE") | Some("yes") | Some("YES")
    )
}

fn rotate_if_oversized(path: &PathBuf) {
    let max_bytes = std::env::var("LATTICE_LIFECYCLE_MAX_BYTES")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or(DEFAULT_MAX_LOG_BYTES);

    let Ok(current) = metadata(path) else {
        return;
    };
    if current.len() < max_bytes {
        return;
    }

    let rotated = path.with_extension("jsonl.1");
    let _ = std::fs::remove_file(&rotated);
    let _ = rename(path, rotated);
}

pub(crate) fn lifecycle_log_path() -> PathBuf {
    if let Ok(dir) = std::env::var("LATTICE_LIFECYCLE_LOG_DIR") {
        return PathBuf::from(dir).join("lifecycle.jsonl");
    }

    // Unit tests build daemons in-process. Left to the default, their shard
    // events for throwaway workspaces landed in the operator's real log,
    // which is the first thing read when diagnosing the live daemon.
    #[cfg(test)]
    {
        test_lifecycle_log_path()
    }
    #[cfg(not(test))]
    {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home)
                .join(".lattice")
                .join("logs")
                .join("lifecycle.jsonl");
        }
        std::env::temp_dir().join("lattice-lifecycle.jsonl")
    }
}

#[cfg(test)]
fn test_lifecycle_log_path() -> PathBuf {
    std::env::temp_dir()
        .join(format!("lattice-test-lifecycle-{}", std::process::id()))
        .join("lifecycle.jsonl")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_test_process_never_defaults_to_the_operators_real_log() {
        if std::env::var_os("LATTICE_LIFECYCLE_LOG_DIR").is_some() {
            return;
        }
        let path = lifecycle_log_path();
        assert!(path.starts_with(std::env::temp_dir()), "{}", path.display());
        if let Some(home) = std::env::var_os("HOME") {
            assert!(
                !path.starts_with(PathBuf::from(home).join(".lattice")),
                "{}",
                path.display()
            );
        }
        log_event("daemon", "test_event_for_isolation", &[]);
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("test_event_for_isolation"));
    }

    /// Soft #1 (independent review): the append write used to discard its I/O
    /// Result (`let _ = writeln!(...)`), so a failed write was undetectable and
    /// the audit trail could go blank with no signal. The write seam now
    /// returns the error so the caller can surface it.
    #[test]
    fn append_event_line_surfaces_write_failures() {
        // A happy-path write lands the line.
        let dir = std::env::temp_dir().join(format!("lattice-lifecycle-ok-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let good = dir.join("lifecycle.jsonl");
        let _ = std::fs::remove_file(&good);
        append_event_line(&good, "{\"event\":\"ok\"}").expect("write to a valid path must succeed");
        assert!(std::fs::read_to_string(&good).unwrap().contains("ok"));

        // An unwritable path (parent is a regular file, not a directory) must
        // return Err rather than silently swallow the failure.
        let blocker = dir.join("not-a-dir");
        std::fs::write(&blocker, b"x").unwrap();
        let bad = blocker.join("lifecycle.jsonl");
        assert!(
            append_event_line(&bad, "{\"event\":\"blocked\"}").is_err(),
            "a write to an unwritable path must surface as Err, not be swallowed"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The failure warning fires on the first failure and then latches, so a
    /// persistent fault surfaces once without flooding the real log.
    #[test]
    fn write_failure_warning_latches_after_the_first() {
        // Reset the global latch for a deterministic observation.
        WRITE_FAILURE_WARNED.store(false, Ordering::Relaxed);
        assert!(
            !WRITE_FAILURE_WARNED.load(Ordering::Relaxed),
            "latch must start clear"
        );
        warn_lifecycle_write_failure_once("append_event", &"first failure");
        assert!(
            WRITE_FAILURE_WARNED.load(Ordering::Relaxed),
            "the first failure must set the latch (and emit a warning)"
        );
        // A second call must not re-arm anything; the latch stays set and no
        // second warning is emitted (verified structurally by the swap latch).
        warn_lifecycle_write_failure_once("append_event", &"second failure");
        assert!(WRITE_FAILURE_WARNED.load(Ordering::Relaxed));
        // Leave the latch clear so unrelated tests observing a real failure
        // still get their one warning.
        WRITE_FAILURE_WARNED.store(false, Ordering::Relaxed);
    }
}
