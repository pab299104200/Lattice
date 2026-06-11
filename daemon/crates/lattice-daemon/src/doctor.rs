use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::hash::{DefaultHasher, Hasher};
use std::io::{BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, UNIX_EPOCH};

use crate::proxy::{daemon_addr, ProxyHello};
use crate::rpc::server::read_message_sync;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpRegistration {
    pub(crate) source: PathBuf,
    pub(crate) json_path: String,
    pub(crate) command: Option<String>,
    pub(crate) args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfigScanReport {
    pub(crate) registrations: Vec<McpRegistration>,
    pub(crate) conflicts: Vec<String>,
    pub(crate) stale_paths: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct JsonRpcLine {
    result: Option<Value>,
    error: Option<Value>,
}

#[derive(Debug, Serialize)]
struct DoctorSummary {
    failures: usize,
    warnings: usize,
}

pub(crate) async fn run(workspace_roots: Vec<PathBuf>) -> Result<bool> {
    let mut failures = 0usize;
    let mut warnings = 0usize;

    println!("Lattice doctor");
    println!("==============");

    match daemon_ping(&workspace_roots) {
        Ok(latency_ms) => {
            println!(
                "PASS daemon reachable at {} ({} ms)",
                daemon_addr(),
                latency_ms
            );
        }
        Err(error) => {
            failures += 1;
            println!(
                "FAIL daemon reachable at {}: {}. Start with: lattice --daemon",
                daemon_addr(),
                error
            );
        }
    }

    for root in &workspace_roots {
        match index_status_for(root) {
            Ok(status) => print_index_status(root, &status),
            Err(error) => {
                failures += 1;
                println!("FAIL workspace {} index_status: {}", root.display(), error);
            }
        }
    }

    match self_handshake(workspace_roots.first()) {
        Ok((count, latency_ms)) => {
            println!(
                "PASS MCP self-handshake returned {} tools ({} ms)",
                count, latency_ms
            );
        }
        Err(error) => {
            failures += 1;
            println!("FAIL MCP self-handshake: {}", error);
        }
    }

    let config_report = scan_configs(
        std::env::var_os("HOME").map(PathBuf::from),
        std::env::current_dir().ok(),
    );
    if config_report.registrations.is_empty() {
        warnings += 1;
        println!("WARN config scan found no lattice MCP registration");
    } else {
        println!(
            "PASS config scan found {} lattice registration(s)",
            config_report.registrations.len()
        );
        for registration in &config_report.registrations {
            println!(
                "  - {} at {} command={} args={}",
                registration.source.display(),
                registration.json_path,
                registration.command.as_deref().unwrap_or("<missing>"),
                registration.args.join(" ")
            );
        }
    }
    for conflict in &config_report.conflicts {
        failures += 1;
        println!("FAIL config conflict: {}", conflict);
    }
    for stale in &config_report.stale_paths {
        warnings += 1;
        println!("WARN stale binary path: {}", stale);
    }

    match binary_skew_report() {
        Ok(lines) => {
            for line in lines {
                if line.starts_with("WARN") {
                    warnings += 1;
                }
                println!("{}", line);
            }
        }
        Err(error) => {
            warnings += 1;
            println!("WARN binary skew check failed: {}", error);
        }
    }

    println!(
        "{}",
        serde_json::to_string(&DoctorSummary { failures, warnings })?
    );
    Ok(failures == 0)
}

fn daemon_ping(workspace_roots: &[PathBuf]) -> Result<u128> {
    let started = Instant::now();
    let mut stream = TcpStream::connect(daemon_addr()).context("TCP connect failed")?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let roots = if workspace_roots.is_empty() {
        vec![std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .to_string_lossy()
            .to_string()]
    } else {
        workspace_roots
            .iter()
            .map(|root| root.to_string_lossy().to_string())
            .collect()
    };
    let hello = serde_json::to_string(&ProxyHello {
        workspace_roots: roots,
        focus_files: Vec::new(),
        focus_dirs: Vec::new(),
    })?;
    writeln!(stream, "{hello}")?;
    writeln!(
        stream,
        "{}",
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "lattice-doctor", "version": env!("CARGO_PKG_VERSION") }
            }
        })
    )?;
    stream.flush()?;

    let read_stream = stream.try_clone()?;
    let mut reader = BufReader::new(read_stream);
    let response = read_message_sync(&mut reader)?.ok_or_else(|| {
        anyhow::anyhow!("daemon closed connection before JSON-RPC initialize response")
    })?;
    let response: JsonRpcLine = serde_json::from_str(&response)?;
    if let Some(error) = response.error {
        anyhow::bail!("JSON-RPC initialize failed: {}", error);
    }
    response
        .result
        .ok_or_else(|| anyhow::anyhow!("JSON-RPC initialize returned no result"))?;
    Ok(started.elapsed().as_millis())
}

fn index_status_for(root: &Path) -> Result<Value> {
    let output = self_stdio_exchange(
        Some(root),
        &[
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": { "name": "lattice-doctor", "version": env!("CARGO_PKG_VERSION") }
                }
            }),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}}),
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {"name": "index_status", "arguments": {}}
            }),
        ],
        2,
    )?;
    let status_line = output
        .get(1)
        .ok_or_else(|| anyhow::anyhow!("missing index_status response"))?;
    let response: JsonRpcLine = serde_json::from_str(status_line)?;
    if let Some(error) = response.error {
        anyhow::bail!("{}", error);
    }
    Ok(response.result.unwrap_or(Value::Null))
}

fn print_index_status(root: &Path, response: &Value) {
    let payload = response["content"]
        .as_array()
        .and_then(|items| items.first())
        .and_then(|item| item["text"].as_str())
        .and_then(|text| serde_json::from_str::<Value>(text).ok())
        .unwrap_or_else(|| response.clone());
    let status = payload["status"].as_str().unwrap_or("unknown");
    let files = payload["files"].as_i64().unwrap_or_default();
    let degraded = payload["watch_degraded"].as_bool().unwrap_or(false);
    if degraded {
        let reason = payload["watch_degraded_reason"]
            .as_str()
            .unwrap_or("unknown");
        println!(
            "WARN workspace {} status={} files={} watcher=degraded reason={}",
            root.display(),
            status,
            files,
            reason
        );
    } else {
        println!(
            "PASS workspace {} status={} files={} watcher=healthy",
            root.display(),
            status,
            files
        );
    }
}

fn self_handshake(root: Option<&PathBuf>) -> Result<(usize, u128)> {
    let started = Instant::now();
    let output = self_stdio_exchange(
        root.map(PathBuf::as_path),
        &[
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": { "name": "lattice-doctor", "version": env!("CARGO_PKG_VERSION") }
                }
            }),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}}),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
        ],
        2,
    )?;
    let tools_line = output
        .get(1)
        .ok_or_else(|| anyhow::anyhow!("missing tools/list response"))?;
    let response: JsonRpcLine = serde_json::from_str(tools_line)?;
    if let Some(error) = response.error {
        anyhow::bail!("{}", error);
    }
    let count = response
        .result
        .as_ref()
        .and_then(|result| result["tools"].as_array())
        .map(Vec::len)
        .unwrap_or_default();
    Ok((count, started.elapsed().as_millis()))
}

fn self_stdio_exchange(
    root: Option<&Path>,
    requests: &[Value],
    expected: usize,
) -> Result<Vec<String>> {
    let exe = std::env::current_exe()?;
    let workspace = root
        .map(Path::to_path_buf)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let mut child = Command::new(exe)
        .arg("--stdio")
        .arg("--workspace")
        .arg(workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn self --stdio")?;

    {
        let stdin = child.stdin.as_mut().context("child stdin unavailable")?;
        for request in requests {
            writeln!(stdin, "{}", request)?;
        }
    }

    let stdout = child.stdout.take().context("child stdout unavailable")?;
    let mut reader = BufReader::new(stdout);
    let mut responses = Vec::new();
    for _ in 0..expected {
        let Some(message) = read_message_sync(&mut reader)? else {
            break;
        };
        responses.push(message);
    }
    let _ = child.kill();
    let _ = child.wait();
    Ok(responses)
}

pub(crate) fn scan_configs(home: Option<PathBuf>, workspace: Option<PathBuf>) -> ConfigScanReport {
    let mut files = Vec::new();
    if let Some(home) = home {
        files.push(home.join(".claude.json"));
    }
    if let Some(workspace) = workspace {
        files.push(workspace.join(".mcp.json"));
        let claude_dir = workspace.join(".claude");
        files.push(claude_dir.join("settings.json"));
        if let Ok(entries) = fs::read_dir(&claude_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                    continue;
                };
                if name.starts_with("settings.") && name.ends_with(".json") {
                    files.push(path);
                }
            }
        }
    }

    let mut registrations = Vec::new();
    for file in files {
        let Ok(text) = fs::read_to_string(&file) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        collect_lattice_registrations(&file, "$", &value, &mut registrations);
    }

    let conflicts = registration_conflicts(&registrations);
    let stale_paths = registrations
        .iter()
        .filter_map(|registration| {
            let command = registration.command.as_ref()?;
            if Path::new(command).is_absolute() && !Path::new(command).exists() {
                Some(format!(
                    "{} references missing {}",
                    registration.source.display(),
                    command
                ))
            } else {
                None
            }
        })
        .collect();
    ConfigScanReport {
        registrations,
        conflicts,
        stale_paths,
    }
}

fn collect_lattice_registrations(
    source: &Path,
    json_path: &str,
    value: &Value,
    registrations: &mut Vec<McpRegistration>,
) {
    match value {
        Value::Object(object) => {
            if let Some(mcp_servers) = object.get("mcpServers").and_then(Value::as_object) {
                if let Some(lattice) = mcp_servers.get("lattice") {
                    registrations.push(McpRegistration {
                        source: source.to_path_buf(),
                        json_path: format!("{json_path}.mcpServers.lattice"),
                        command: lattice["command"].as_str().map(ToString::to_string),
                        args: lattice["args"]
                            .as_array()
                            .map(|args| {
                                args.iter()
                                    .filter_map(Value::as_str)
                                    .map(ToString::to_string)
                                    .collect()
                            })
                            .unwrap_or_default(),
                    });
                }
            }
            for (key, child) in object {
                collect_lattice_registrations(
                    source,
                    &format!("{json_path}.{key}"),
                    child,
                    registrations,
                );
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                collect_lattice_registrations(
                    source,
                    &format!("{json_path}[{index}]"),
                    child,
                    registrations,
                );
            }
        }
        _ => {}
    }
}

fn registration_conflicts(registrations: &[McpRegistration]) -> Vec<String> {
    let mut by_signature: BTreeMap<(Option<String>, Vec<String>), Vec<String>> = BTreeMap::new();
    for registration in registrations {
        by_signature
            .entry((registration.command.clone(), registration.args.clone()))
            .or_default()
            .push(format!(
                "{} at {}",
                registration.source.display(),
                registration.json_path
            ));
    }
    if registrations.len() <= 1 || by_signature.len() <= 1 {
        return Vec::new();
    }
    vec![format!(
        "server name `lattice` has {} conflicting command/args signatures: {}",
        by_signature.len(),
        by_signature
            .values()
            .map(|sources| sources.join(", "))
            .collect::<Vec<_>>()
            .join(" | ")
    )]
}

fn binary_skew_report() -> Result<Vec<String>> {
    let current = std::env::current_exe()?;
    let mut paths = vec![current.clone()];
    if let Ok(cwd) = std::env::current_dir() {
        paths.push(cwd.join("daemon/target/release/lattice"));
        paths.push(cwd.join("extension/bin/lattice"));
    }
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        paths.push(home.join(".vscode/extensions/lattice.lattice-0.1.0/bin/lattice"));
    }

    let mut lines = Vec::new();
    let current_digest = file_digest(&current).ok();
    let current_mtime = fs::metadata(&current)
        .ok()
        .and_then(|meta| modified_epoch_secs(&meta));
    for path in paths {
        match fs::metadata(&path) {
            Ok(meta) => {
                let mtime = modified_epoch_secs(&meta).unwrap_or_default();
                let size = meta.len();
                let digest = file_digest(&path).ok();
                if digest != current_digest {
                    lines.push(format!(
                        "WARN binary skew {} size={} mtime={} current_mtime={:?}",
                        path.display(),
                        size,
                        mtime,
                        current_mtime
                    ));
                } else {
                    lines.push(format!(
                        "PASS binary {} size={} mtime={}",
                        path.display(),
                        size,
                        mtime
                    ));
                }
            }
            Err(_) => lines.push(format!("WARN binary missing {}", path.display())),
        }
    }
    Ok(lines)
}

fn modified_epoch_secs(meta: &fs::Metadata) -> Option<u64> {
    meta.modified()
        .ok()
        .and_then(|mtime| mtime.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
}

fn file_digest(path: &Path) -> Result<u64> {
    let mut file = fs::File::open(path)?;
    let mut hasher = DefaultHasher::new();
    let mut buffer = [0u8; 8192];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.write(&buffer[..read]);
    }
    Ok(hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_scan_reports_conflicting_lattice_registrations() {
        let root = unique_test_dir("doctor-conflict");
        let home = root.join("home");
        let workspace = root.join("repo");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&workspace).unwrap();
        fs::write(
            home.join(".claude.json"),
            r#"{"mcpServers":{"lattice":{"command":"/a/lattice","args":["--stdio","--workspace","/home"]}}}"#,
        )
        .unwrap();
        fs::write(
            workspace.join(".mcp.json"),
            r#"{"mcpServers":{"lattice":{"command":"/b/lattice","args":["--stdio","--workspace","/repo"]}}}"#,
        )
        .unwrap();

        let report = scan_configs(Some(home), Some(workspace));

        assert_eq!(report.registrations.len(), 2);
        assert_eq!(report.conflicts.len(), 1);
        assert!(report.conflicts[0].contains("conflicting"));
    }

    #[test]
    fn config_scan_accepts_single_project_registration() {
        let root = unique_test_dir("doctor-single");
        let workspace = root.join("repo");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(
            workspace.join(".mcp.json"),
            r#"{"mcpServers":{"lattice":{"command":"/bin/false","args":["--stdio"]}}}"#,
        )
        .unwrap();

        let report = scan_configs(None, Some(workspace));

        assert_eq!(report.registrations.len(), 1);
        assert!(report.conflicts.is_empty());
    }

    fn unique_test_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "lattice-{name}-{}",
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }
}
