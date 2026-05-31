use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader as AsyncBufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::process::Command;

use crate::lifecycle_log;
use crate::rpc::server::read_message_sync;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ProxyHello {
    pub workspace_roots: Vec<String>,
    #[serde(default)]
    pub focus_files: Vec<String>,
    #[serde(default)]
    pub focus_dirs: Vec<String>,
}

pub(crate) fn daemon_addr() -> String {
    std::env::var("LATTICE_DAEMON_ADDR").unwrap_or_else(|_| "127.0.0.1:47659".to_string())
}

pub(crate) async fn run_stdio_proxy(hello: ProxyHello) -> Result<()> {
    let hello = serde_json::to_string(&hello)?;
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

    let (mut lines, mut write_half) = connect_with_hello(&hello)
        .await
        .context("failed to connect to lattice daemon")?;
    lifecycle_log::log_event(
        "proxy",
        "daemon_connected",
        &[("daemon_addr", serde_json::json!(daemon_addr()))],
    );
    let stdout = std::io::stdout();
    let mut pending_message: Option<String> = None;

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
                    let (new_lines, new_write_half) = connect_with_hello(&hello)
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
            continue;
        }

        tokio::select! {
            maybe_message = stdin_rx.recv() => {
                let Some(message) = maybe_message else {
                    lifecycle_log::log_event("proxy", "stdin_closed", &[]);
                    write_half.shutdown().await?;
                    break;
                };
                pending_message = Some(message);
            }
            line = lines.next_line() => {
                match line {
                    Ok(Some(line)) => {
                        let mut out = stdout.lock();
                        out.write_all(line.as_bytes())?;
                        out.write_all(b"\n")?;
                        out.flush()?;
                    }
                    Ok(None) => {
                        tracing::warn!("proxy daemon connection closed; reconnecting");
                        lifecycle_log::log_event("proxy", "daemon_connection_closed", &[]);
                        let (new_lines, new_write_half) = connect_with_hello(&hello)
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
                        let (new_lines, new_write_half) = connect_with_hello(&hello)
                            .await
                            .context("failed to reconnect to lattice daemon after read failure")?;
                        lines = new_lines;
                        write_half = new_write_half;
                        lifecycle_log::log_event("proxy", "daemon_reconnected", &[]);
                    }
                }
            }
        }
    }
    Ok(())
}

async fn connect_with_hello(
    hello: &str,
) -> Result<(
    tokio::io::Lines<AsyncBufReader<OwnedReadHalf>>,
    OwnedWriteHalf,
)> {
    let mut stream = connect_or_start_daemon().await?;
    write_message(&mut stream, hello).await?;
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
    use super::normalize_deleted_executable_path;
    use std::path::Path;

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
}
