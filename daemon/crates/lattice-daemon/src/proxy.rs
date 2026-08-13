use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{BufReader, Write};
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader as AsyncBufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::process::Command;

use crate::lifecycle_log;
use crate::rpc::server::read_message_sync;
use crate::transport::{self, ClientKind, ProxyRequest};

pub(crate) fn daemon_addr() -> String {
    std::env::var("LATTICE_DAEMON_ADDR").unwrap_or_else(|_| "127.0.0.1:47659".to_string())
}

const DEFAULT_PROXY_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Returns the bounded lifetime for a connected but inactive stdio client.
///
/// A proxy only exits after this interval when no request is awaiting a daemon
/// response, so a slow tool call cannot be terminated by the idle reaper.
fn proxy_idle_timeout() -> Duration {
    proxy_idle_timeout_from(
        std::env::var("LATTICE_PROXY_IDLE_TIMEOUT_SECS")
            .ok()
            .as_deref(),
    )
}

fn proxy_idle_timeout_from(value: Option<&str>) -> Duration {
    value
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_PROXY_IDLE_TIMEOUT)
}

pub(crate) async fn run_stdio_proxy(request: ProxyRequest) -> Result<()> {
    let (stdin_tx, mut stdin_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut reader = BufReader::new(stdin.lock());
        loop {
            match read_message_sync(&mut reader) {
                Ok(Some(message)) => {
                    if stdin_tx.send(message).is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(err) => {
                    tracing::warn!("proxy skipped malformed client frame: {}", err);
                }
            }
        }
    });

    let (mut lines, mut write_half) = connect_with_hello(&request)
        .await
        .context("failed to connect to lattice daemon")?;
    lifecycle_log::log_event(
        "proxy",
        "daemon_connected",
        &[("daemon_addr", serde_json::json!(daemon_addr()))],
    );
    let stdout = std::io::stdout();
    let mut pending_message: Option<String> = None;
    let mut in_flight_requests = HashSet::new();
    let idle_timeout = proxy_idle_timeout();
    let mut last_activity = tokio::time::Instant::now();

    loop {
        if let Some(message) = pending_message.take() {
            match write_message(&mut write_half, &message).await {
                Ok(()) => {}
                Err(error) => {
                    tracing::warn!(%error, "proxy write failed; reconnecting to daemon");
                    lifecycle_log::log_event(
                        "proxy",
                        "daemon_write_failed",
                        &[("error", serde_json::json!(error.to_string()))],
                    );
                    let (new_lines, new_write_half) = connect_with_hello(&request)
                        .await
                        .context("failed to reconnect to lattice daemon after write failure")?;
                    lines = new_lines;
                    write_half = new_write_half;
                    lifecycle_log::log_event("proxy", "daemon_reconnected", &[]);
                    write_message(&mut write_half, &message)
                        .await
                        .context("failed to resend request after reconnect")?;
                }
            }
            record_outbound_request(&message, &mut in_flight_requests);
            last_activity = tokio::time::Instant::now();
            continue;
        }

        tokio::select! {
            maybe_message = stdin_rx.recv() => {
                let Some(message) = maybe_message else {
                    lifecycle_log::log_event(
                        "proxy",
                        "stdin_closed",
                        &[("in_flight_requests", serde_json::json!(in_flight_requests.len()))],
                    );
                    write_half.shutdown().await?;
                    break;
                };
                apply_client_cancellation(&message, &mut in_flight_requests);
                pending_message = Some(message);
            }
            line = lines.next_line() => {
                match line {
                    Ok(Some(line)) => {
                        if let Some(id) = response_id(&line) {
                            in_flight_requests.remove(&id);
                        }
                        last_activity = tokio::time::Instant::now();
                        let mut out = stdout.lock();
                        out.write_all(line.as_bytes())?;
                        out.write_all(b"\n")?;
                        out.flush()?;
                    }
                    Ok(None) => {
                        tracing::warn!("proxy daemon connection closed; reconnecting");
                        lifecycle_log::log_event("proxy", "daemon_connection_closed", &[]);
                        let (new_lines, new_write_half) = connect_with_hello(&request)
                            .await
                            .context("failed to reconnect to lattice daemon after disconnect")?;
                        lines = new_lines;
                        write_half = new_write_half;
                        lifecycle_log::log_event("proxy", "daemon_reconnected", &[]);
                    }
                    Err(error) => {
                        tracing::warn!(%error, "proxy daemon read failed; reconnecting");
                        lifecycle_log::log_event(
                            "proxy",
                            "daemon_read_failed",
                            &[("error", serde_json::json!(error.to_string()))],
                        );
                        let (new_lines, new_write_half) = connect_with_hello(&request)
                            .await
                            .context("failed to reconnect to lattice daemon after read failure")?;
                        lines = new_lines;
                        write_half = new_write_half;
                        lifecycle_log::log_event("proxy", "daemon_reconnected", &[]);
                    }
                }
            }
            _ = tokio::time::sleep_until(last_activity + idle_timeout), if in_flight_requests.is_empty() => {
                lifecycle_log::log_event(
                    "proxy",
                    "idle_exit",
                    &[("idle_timeout_secs", serde_json::json!(idle_timeout.as_secs()))],
                );
                write_half.shutdown().await?;
                break;
            }
        }
    }
    Ok(())
}

fn record_outbound_request(message: &str, in_flight_requests: &mut HashSet<String>) {
    let Ok(value) = serde_json::from_str::<Value>(message) else {
        return;
    };
    if value.get("method").is_some() {
        if let Some(id) = value.get("id").and_then(json_rpc_id_key) {
            in_flight_requests.insert(id);
        }
    }
}

fn apply_client_cancellation(message: &str, in_flight_requests: &mut HashSet<String>) {
    let Ok(value) = serde_json::from_str::<Value>(message) else {
        return;
    };
    if value.get("method").and_then(Value::as_str) == Some("notifications/cancelled") {
        if let Some(id) = value
            .get("params")
            .and_then(|params| {
                params
                    .get("requestId")
                    .or_else(|| params.get("request_id"))
                    .or_else(|| params.get("id"))
            })
            .and_then(json_rpc_id_key)
        {
            in_flight_requests.remove(&id);
        }
    }
}

fn response_id(message: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(message).ok()?;
    if value.get("method").is_some() {
        return None;
    }
    value.get("id").and_then(json_rpc_id_key)
}

fn json_rpc_id_key(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(value) => Some(format!("string:{value}")),
        Value::Number(value) => Some(format!("number:{value}")),
        Value::Bool(value) => Some(format!("bool:{value}")),
        other => serde_json::to_string(other)
            .ok()
            .map(|encoded| format!("json:{encoded}")),
    }
}

async fn connect_with_hello(
    request: &ProxyRequest,
) -> Result<(
    tokio::io::Lines<AsyncBufReader<OwnedReadHalf>>,
    OwnedWriteHalf,
)> {
    let mut stream = connect_or_start_daemon().await?;
    let addr = daemon_addr();
    transport::client_handshake(&mut stream, &addr, ClientKind::StdioProxy, request).await?;
    let (read_half, write_half) = stream.into_split();
    Ok((AsyncBufReader::new(read_half).lines(), write_half))
}

async fn connect_or_start_daemon() -> Result<TcpStream> {
    match TcpStream::connect(daemon_addr()).await {
        Ok(stream) => return Ok(stream),
        Err(err) => {
            tracing::info!("lattice daemon unavailable, starting daemon: {}", err);
            lifecycle_log::log_event(
                "proxy",
                "daemon_connect_failed",
                &[("error", serde_json::json!(err.to_string()))],
            );
        }
    }

    let _startup_guard = tokio::task::spawn_blocking(acquire_daemon_start_lock)
        .await
        .context("daemon startup lock task failed")??;

    // Another proxy may have started the daemon while this process waited for the lock.
    if let Ok(stream) = TcpStream::connect(daemon_addr()).await {
        return Ok(stream);
    }

    start_daemon_process().await?;
    let mut last_error = None;
    for _ in 0..80 {
        match TcpStream::connect(daemon_addr()).await {
            Ok(stream) => return Ok(stream),
            Err(err) => {
                last_error = Some(err);
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }

    Err(anyhow::anyhow!(
        "daemon did not listen on {}: {}",
        daemon_addr(),
        last_error
            .map(|err| err.to_string())
            .unwrap_or_else(|| "unknown connect error".to_string())
    ))
}

fn acquire_daemon_start_lock() -> Result<File> {
    let path = daemon_start_lock_path();
    acquire_lock_at(&path)
}

fn acquire_lock_at(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create daemon runtime directory {}",
                parent.display()
            )
        })?;
    }
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("failed to open daemon startup lock {}", path.display()))?;
    lock_file_exclusive(&file)
        .with_context(|| format!("failed to acquire daemon startup lock {}", path.display()))?;
    Ok(file)
}

fn daemon_start_lock_path() -> PathBuf {
    let mut hasher = DefaultHasher::new();
    daemon_addr().hash(&mut hasher);
    let filename = format!("daemon-start-{:016x}.lock", hasher.finish());
    if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home)
            .join(".lattice")
            .join("run")
            .join(filename)
    } else {
        std::env::temp_dir().join(filename)
    }
}

#[cfg(unix)]
fn lock_file_exclusive(file: &File) -> std::io::Result<()> {
    // SAFETY: flock only reads the valid file descriptor and does not retain the pointer state.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(unix))]
fn lock_file_exclusive(_file: &File) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "cross-process daemon startup locking requires Unix flock",
    ))
}

async fn start_daemon_process() -> Result<()> {
    let exe = resolve_daemon_executable()?;
    lifecycle_log::log_event(
        "proxy",
        "daemon_spawn_requested",
        &[("exe", serde_json::json!(exe.to_string_lossy().to_string()))],
    );
    let child = Command::new(exe)
        .arg("--daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .context("failed to spawn lattice --daemon")?;
    lifecycle_log::log_event(
        "proxy",
        "daemon_spawned",
        &[("child_pid", serde_json::json!(child.id()))],
    );
    Ok(())
}

fn resolve_daemon_executable() -> Result<PathBuf> {
    if let Some(explicit) = std::env::var_os("LATTICE_DAEMON_EXE") {
        let path = PathBuf::from(explicit);
        if path.is_file() {
            return Ok(path);
        }
    }

    if let Some(argv0) = std::env::args_os().next() {
        if let Some(path) = resolve_invocation_path(&argv0) {
            return Ok(path);
        }
    }

    let current =
        std::env::current_exe().context("failed to resolve current lattice executable")?;
    if let Some(path) = normalize_deleted_executable_path(&current) {
        return Ok(path);
    }

    Err(anyhow::anyhow!(
        "failed to resolve a stable lattice daemon executable path"
    ))
}

fn resolve_invocation_path(argv0: &std::ffi::OsStr) -> Option<PathBuf> {
    let candidate = PathBuf::from(argv0);
    if candidate.components().count() > 1 {
        return normalize_deleted_executable_path(&candidate);
    }

    let path_env = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_env) {
        let joined = dir.join(&candidate);
        if let Some(path) = normalize_deleted_executable_path(&joined) {
            return Some(path);
        }
    }
    None
}

fn normalize_deleted_executable_path(path: &Path) -> Option<PathBuf> {
    if path.is_file() {
        return Some(path.to_path_buf());
    }

    let raw = path.to_string_lossy();
    let trimmed = raw.strip_suffix(" (deleted)")?;
    let candidate = PathBuf::from(trimmed);
    if candidate.is_file() {
        return Some(candidate);
    }
    None
}

async fn write_message<W: AsyncWriteExt + Unpin>(writer: &mut W, message: &str) -> Result<()> {
    writer.write_all(&format_message_frame(message)).await?;
    writer.flush().await?;
    Ok(())
}

fn format_message_frame(message: &str) -> Vec<u8> {
    let mut frame: Vec<u8> = Vec::new();
    frame.extend_from_slice(message.as_bytes());
    frame.push(b'\n');
    frame
}

#[cfg(test)]
mod tests {
    use super::{
        acquire_lock_at, daemon_start_lock_path, json_rpc_id_key,
        normalize_deleted_executable_path, proxy_idle_timeout_from, response_id,
        DEFAULT_PROXY_IDLE_TIMEOUT,
    };
    use std::path::Path;
    use std::time::Duration;

    #[test]
    fn normalize_deleted_path_uses_live_binary_path() {
        let unique = format!(
            "lattice-proxy-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time")
                .as_nanos()
        );
        let tempdir = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&tempdir).expect("create temp dir");
        let exe = tempdir.join("lattice");
        std::fs::write(&exe, b"bin").expect("write fake binary");

        let deleted = format!("{} (deleted)", exe.display());
        let resolved = normalize_deleted_executable_path(Path::new(&deleted)).expect("resolved");
        assert_eq!(resolved, exe);
        let _ = std::fs::remove_file(&exe);
        let _ = std::fs::remove_dir(&tempdir);
    }

    #[test]
    fn normalize_deleted_path_rejects_missing_binary() {
        let missing = Path::new("/tmp/lattice-missing (deleted)");
        assert!(normalize_deleted_executable_path(missing).is_none());
    }

    #[test]
    fn daemon_start_lock_is_scoped_to_daemon_address() {
        let previous = std::env::var_os("LATTICE_DAEMON_ADDR");
        std::env::set_var("LATTICE_DAEMON_ADDR", "127.0.0.1:47659");
        let first = daemon_start_lock_path();
        std::env::set_var("LATTICE_DAEMON_ADDR", "127.0.0.1:47660");
        let second = daemon_start_lock_path();
        match previous {
            Some(value) => std::env::set_var("LATTICE_DAEMON_ADDR", value),
            None => std::env::remove_var("LATTICE_DAEMON_ADDR"),
        }

        assert_ne!(first, second);
        assert_eq!(
            first.extension().and_then(|value| value.to_str()),
            Some("lock")
        );
    }

    #[cfg(unix)]
    #[test]
    fn daemon_start_lock_serializes_processes_until_guard_drops() {
        let unique = format!(
            "lattice-start-lock-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time")
                .as_nanos()
        );
        let path = std::env::temp_dir().join(unique).join("start.lock");
        let first = acquire_lock_at(&path).expect("first lock");
        let (tx, rx) = std::sync::mpsc::channel();
        let child_path = path.clone();
        let waiter = std::thread::spawn(move || {
            let guard = acquire_lock_at(&child_path).expect("second lock");
            tx.send(()).expect("send acquired");
            drop(guard);
        });

        assert!(
            rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "second startup acquired the lock before the first released it"
        );
        drop(first);
        rx.recv_timeout(Duration::from_secs(1))
            .expect("second startup did not acquire released lock");
        waiter.join().expect("waiter thread");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(path.parent().expect("lock parent"));
    }

    #[test]
    fn proxy_idle_timeout_accepts_only_positive_seconds() {
        assert_eq!(proxy_idle_timeout_from(Some("17")), Duration::from_secs(17));
        assert_eq!(
            proxy_idle_timeout_from(Some("0")),
            DEFAULT_PROXY_IDLE_TIMEOUT
        );
        assert_eq!(
            proxy_idle_timeout_from(Some("invalid")),
            DEFAULT_PROXY_IDLE_TIMEOUT
        );
    }

    #[test]
    fn response_id_ignores_notifications_and_matches_request_ids() {
        assert_eq!(
            response_id(r#"{"jsonrpc":"2.0","id":42,"result":{}}"#),
            Some("number:42".to_string())
        );
        assert_eq!(
            response_id(r#"{"jsonrpc":"2.0","method":"notifications/progress"}"#),
            None
        );
        assert_eq!(
            json_rpc_id_key(&serde_json::json!("request-1")),
            Some("string:request-1".to_string())
        );
    }
}
