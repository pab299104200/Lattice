#![allow(dead_code)]

use serde_json::{json, Map, Value};
use std::fs::{create_dir_all, metadata, rename, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

static LIFECYCLE_LOG_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
const DEFAULT_MAX_LOG_BYTES: u64 = 32 * 1024 * 1024;

pub(crate) fn log_event(role: &str, event: &str, fields: &[(&str, Value)]) {
    if should_skip_event(event) {
        return;
    }

    let path = lifecycle_log_path();
    if let Some(parent) = path.parent() {
        let _ = create_dir_all(parent);
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
        return;
    };

    rotate_if_oversized(&path);

    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };
    let Ok(line) = serde_json::to_string(&entry) else {
        return;
    };
    let _ = writeln!(file, "{line}");
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

    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home)
            .join(".lattice")
            .join("logs")
            .join("lifecycle.jsonl");
    }

    std::env::temp_dir().join("lattice-lifecycle.jsonl")
}
