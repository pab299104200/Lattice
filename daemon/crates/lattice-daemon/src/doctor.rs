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

use crate::adoption_metrics::{capture_health_for_workspace, CaptureHealth};
use crate::proxy::daemon_addr;
use crate::rpc::server::read_message_sync;
use crate::transport::{self, ClientKind, ProxyRequest};

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
    pub(crate) hook_registrations: Vec<HookRegistration>,
    pub(crate) conflicts: Vec<String>,
    pub(crate) stale_paths: Vec<String>,
    pub(crate) registration_issues: Vec<String>,
    pub(crate) hook_timeout_violations: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HookRegistration {
    pub(crate) source: PathBuf,
    pub(crate) json_path: String,
    pub(crate) command: String,
    pub(crate) timeout_secs: Option<u64>,
}

/// A snapshot of the process table.  Keeping this boundary explicit makes the
/// doctor check deterministic in tests and prevents fixture tests from ever
/// inspecting the host process table.
trait ProcessSnapshot {
    fn output(&self) -> Result<String>;
}

struct SystemProcessSnapshot;

impl ProcessSnapshot for SystemProcessSnapshot {
    fn output(&self) -> Result<String> {
        let output = Command::new("ps")
            .args(["-axo", "pid=,ppid=,command="])
            .output()
            .context("failed to inspect process table")?;
        if !output.status.success() {
            anyhow::bail!("process inspection exited with {}", output.status);
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
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
        Ok((latency_ms, connection)) => {
            println!(
                "PASS daemon reachable at {} ({} ms); authenticated transport protocol v{}",
                daemon_addr(),
                latency_ms,
                connection.protocol_version
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
                println!("FAIL workspace {} status: {}", root.display(), error);
            }
        }
        match capture_health_for_workspace(root) {
            Ok(health) => {
                let line = format_capture_health(&health);
                if health.has_concerning_outcomes() {
                    warnings += 1;
                }
                println!("{line}");
            }
            Err(_) => {
                warnings += 1;
                // Capture payloads and paths are intentionally absent from
                // doctor output, including when the aggregate cannot load.
                println!("WARN capture health unavailable");
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
        workspace_roots
            .first()
            .cloned()
            .or_else(|| std::env::current_dir().ok()),
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
        println!("WARN stale configured path: {}", stale);
    }
    for issue in &config_report.registration_issues {
        failures += 1;
        println!("FAIL config registration: {}", issue);
    }
    for violation in &config_report.hook_timeout_violations {
        failures += 1;
        println!("FAIL hook timeout: {}", violation);
    }

    match orphan_proxy_count(&SystemProcessSnapshot) {
        Ok(0) => println!("PASS no orphaned lattice --stdio proxies"),
        Ok(count) => {
            warnings += 1;
            println!(
                "WARN found {count} orphaned lattice --stdio proxy process(es); stop them with: pkill -f 'lattice --stdio'"
            );
        }
        Err(error) => {
            warnings += 1;
            println!("WARN orphan proxy check failed: {}", error);
        }
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

fn format_capture_health(health: &CaptureHealth) -> String {
    if health.total_attempts == 0 {
        return "PASS capture health no attempts recorded".to_string();
    }
    let outcomes = health
        .outcomes
        .iter()
        .map(|(outcome, count)| format!("{outcome}={count}"))
        .collect::<Vec<_>>()
        .join(" ");
    let level = if health.has_concerning_outcomes() {
        "WARN"
    } else {
        "PASS"
    };
    format!(
        "{level} capture health attempts={} {outcomes}",
        health.total_attempts
    )
}

fn daemon_ping(workspace_roots: &[PathBuf]) -> Result<(u128, transport::ConnectionMetadata)> {
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
    let request = ProxyRequest {
        workspace_roots: roots,
        focus_files: Vec::new(),
        focus_dirs: Vec::new(),
    };
    let connection = transport::client_handshake_sync(
        &mut stream,
        &daemon_addr(),
        ClientKind::Doctor,
        &request,
    )?;
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
    Ok((started.elapsed().as_millis(), connection))
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
                "params": {"name": "status", "arguments": {"scope": "index"}}
            }),
        ],
        2,
    )?;
    let status_line = output
        .get(1)
        .ok_or_else(|| anyhow::anyhow!("missing status response"))?;
    let response: JsonRpcLine = serde_json::from_str(status_line)?;
    if let Some(error) = response.error {
        anyhow::bail!("{}", error);
    }
    Ok(response.result.unwrap_or(Value::Null))
}

fn print_index_status(root: &Path, response: &Value) {
    println!("{}", format_index_status(root, response));
}

fn format_index_status(root: &Path, response: &Value) -> String {
    let payload = response["content"]
        .as_array()
        .and_then(|items| items.first())
        .and_then(|item| item["text"].as_str())
        .and_then(|text| serde_json::from_str::<Value>(text).ok())
        .unwrap_or_else(|| response.clone());
    let status = payload["status"].as_str().unwrap_or("unknown");
    let files = payload["files"].as_i64().unwrap_or_default();
    let parse_failures = payload["parse_failures"].as_u64().unwrap_or_default();
    if parse_failures > 0 {
        let failed_files = payload["failed_files"]
            .as_array()
            .map(|files| {
                files
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .filter(|files| !files.is_empty())
            .unwrap_or_else(|| "<unavailable>".to_string());
        return format!(
            "WARN workspace {} status={} files={} index=partial parse_failures={} failed_files={}",
            root.display(),
            status,
            files,
            parse_failures,
            failed_files
        );
    }
    let degraded = payload["watch_degraded"].as_bool().unwrap_or(false);
    if degraded {
        let reason = payload["watch_degraded_reason"]
            .as_str()
            .unwrap_or("unknown");
        format!(
            "WARN workspace {} status={} files={} watcher=degraded reason={}",
            root.display(),
            status,
            files,
            reason
        )
    } else {
        format!(
            "PASS workspace {} status={} files={} watcher=healthy",
            root.display(),
            status,
            files
        )
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
        .iter()
        .find(|line| {
            serde_json::from_str::<Value>(line)
                .ok()
                .and_then(|value| value.get("id").cloned())
                == Some(json!(2))
        })
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
    if count != 8 {
        anyhow::bail!("tools/list returned {count} tools, expected 8");
    }
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
        let workspace = config_workspace_root(&workspace);
        files.push(workspace.join(".mcp.json"));
        files.push(workspace.join(".codex").join("hooks.json"));
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
    let mut hook_registrations = Vec::new();
    for file in files {
        let Ok(text) = fs::read_to_string(&file) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        collect_lattice_registrations(
            &file,
            "$",
            &value,
            &mut registrations,
            &mut hook_registrations,
        );
    }

    let conflicts = registration_conflicts(&registrations);
    let mut stale_paths = registrations
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
        .collect::<Vec<_>>();
    stale_paths.extend(hook_registrations.iter().filter_map(|registration| {
        (Path::new(&registration.command).is_absolute()
            && !Path::new(&registration.command).exists())
        .then(|| {
            format!(
                "{} at {} references missing {}",
                registration.source.display(),
                registration.json_path,
                registration.command
            )
        })
    }));
    let registration_issues = registration_issues(&registrations);
    let hook_timeout_violations = hook_timeout_violations(&hook_registrations);
    ConfigScanReport {
        registrations,
        hook_registrations,
        conflicts,
        stale_paths,
        registration_issues,
        hook_timeout_violations,
    }
}

/// Resolve configuration from the project root when doctor is invoked in a
/// descendant such as `daemon/`.  The command's default workspace is its cwd,
/// but MCP registration is conventionally kept at the repository root.
fn config_workspace_root(workspace: &Path) -> &Path {
    workspace
        .ancestors()
        .find(|candidate| candidate.join(".mcp.json").is_file())
        .unwrap_or(workspace)
}

fn collect_lattice_registrations(
    source: &Path,
    json_path: &str,
    value: &Value,
    registrations: &mut Vec<McpRegistration>,
    hook_registrations: &mut Vec<HookRegistration>,
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
            if let Some(hooks) = object.get("hooks").and_then(Value::as_object) {
                collect_lattice_hook_registrations(source, json_path, hooks, hook_registrations);
            }
            for (key, child) in object {
                collect_lattice_registrations(
                    source,
                    &format!("{json_path}.{key}"),
                    child,
                    registrations,
                    hook_registrations,
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
                    hook_registrations,
                );
            }
        }
        _ => {}
    }
}

fn collect_lattice_hook_registrations(
    source: &Path,
    json_path: &str,
    hooks: &serde_json::Map<String, Value>,
    registrations: &mut Vec<HookRegistration>,
) {
    for (event, entries) in hooks {
        let Some(entries) = entries.as_array() else {
            continue;
        };
        for (entry_index, entry) in entries.iter().enumerate() {
            let Some(commands) = entry.get("hooks").and_then(Value::as_array) else {
                continue;
            };
            for (command_index, command) in commands.iter().enumerate() {
                let Some(path) = command.get("command").and_then(Value::as_str) else {
                    continue;
                };
                if !is_lattice_hook_path(path) {
                    continue;
                }
                registrations.push(HookRegistration {
                    source: source.to_path_buf(),
                    json_path: format!(
                        "{json_path}.hooks.{event}[{entry_index}].hooks[{command_index}]"
                    ),
                    command: path.to_string(),
                    timeout_secs: command.get("timeout").and_then(Value::as_u64),
                });
            }
        }
    }
}

fn is_lattice_hook_path(command: &str) -> bool {
    let path = Path::new(command);
    path.components()
        .any(|component| component.as_os_str() == "integrations")
        && path
            .components()
            .any(|component| component.as_os_str() == "hooks")
        && matches!(
            path.file_name().and_then(|name| name.to_str()),
            Some("session-start.sh" | "user-prompt-submit.sh" | "post-tool-use.sh" | "stop.sh")
        )
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

fn registration_issues(registrations: &[McpRegistration]) -> Vec<String> {
    registrations
        .iter()
        .filter_map(|registration| {
            let source = format!(
                "{} at {}",
                registration.source.display(),
                registration.json_path
            );
            let command = registration.command.as_deref()?;
            if command.trim().is_empty() {
                return Some(format!("{source} has an empty command"));
            }
            if !registration.args.iter().any(|arg| arg == "--stdio") {
                return Some(format!("{source} is missing required --stdio argument"));
            }
            None
        })
        .chain(registrations.iter().filter_map(|registration| {
            registration.command.is_none().then(|| {
                format!(
                    "{} at {} is missing a command",
                    registration.source.display(),
                    registration.json_path
                )
            })
        }))
        .collect()
}

fn hook_timeout_violations(registrations: &[HookRegistration]) -> Vec<String> {
    registrations
        .iter()
        .filter_map(|registration| {
            let timeout = registration.timeout_secs?;
            (timeout <= crate::install::MAX_INNER_HOOK_TIMEOUT_SECS).then(|| {
                format!(
                    "{} at {} uses {timeout}s, but it must exceed the {}s internal query timeout",
                    registration.source.display(),
                    registration.json_path,
                    crate::install::MAX_INNER_HOOK_TIMEOUT_SECS
                )
            })
        })
        .chain(registrations.iter().filter_map(|registration| {
            registration.timeout_secs.is_none().then(|| {
                format!(
                    "{} at {} has no numeric outer timeout",
                    registration.source.display(),
                    registration.json_path
                )
            })
        }))
        .collect()
}

fn orphan_proxy_count(snapshot: &dyn ProcessSnapshot) -> Result<usize> {
    Ok(orphaned_stdio_proxies(&snapshot.output()?))
}

fn orphaned_stdio_proxies(processes: &str) -> usize {
    processes
        .lines()
        .filter(|line| {
            let mut fields = line.split_whitespace();
            let Some(_pid) = fields.next().and_then(|value| value.parse::<u32>().ok()) else {
                return false;
            };
            let ppid = fields.next().and_then(|value| value.parse::<u32>().ok());
            let command = fields.collect::<Vec<_>>().join(" ");
            ppid == Some(1) && is_lattice_stdio_proxy(&command)
        })
        .count()
}

fn is_lattice_stdio_proxy(command: &str) -> bool {
    command
        .split_whitespace()
        .any(|argument| argument == "--stdio")
        && command.split_whitespace().any(|argument| {
            Path::new(argument)
                .file_name()
                .is_some_and(|name| name == std::ffi::OsStr::new("lattice"))
        })
}

fn binary_skew_report() -> Result<Vec<String>> {
    let current = std::env::current_exe()?;
    let paths = std::env::current_dir()
        .ok()
        .map(|cwd| binary_paths_for(&current, &cwd))
        .unwrap_or_else(|| vec![current.clone()]);

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

fn binary_paths_for(current: &Path, cwd: &Path) -> Vec<PathBuf> {
    let mut paths = vec![current.to_path_buf()];
    let Some(daemon_dir) = daemon_directory_for(cwd) else {
        return paths;
    };
    let release_binary = daemon_dir.join("target/release/lattice");
    if release_binary != current {
        paths.push(release_binary);
    }
    paths
}

/// Locate the daemon Cargo workspace from either the repository root, the
/// `daemon/` directory itself, or one of its descendants.
fn daemon_directory_for(cwd: &Path) -> Option<PathBuf> {
    let nested_daemon = cwd.join("daemon");
    if is_daemon_workspace(&nested_daemon) {
        return Some(nested_daemon);
    }
    cwd.ancestors()
        .find(|candidate| is_daemon_workspace(candidate))
        .map(Path::to_path_buf)
}

fn is_daemon_workspace(path: &Path) -> bool {
    path.join("Cargo.toml").is_file() && path.join("crates/lattice-daemon/Cargo.toml").is_file()
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

    #[test]
    fn config_scan_finds_project_mcp_registration_from_daemon_directory() {
        let root = unique_test_dir("doctor-project-root-config");
        let daemon = root.join("daemon");
        fs::create_dir_all(&daemon).unwrap();
        fs::write(
            root.join(".mcp.json"),
            r#"{"mcpServers":{"lattice":{"command":"/bin/false","args":["--stdio"]}}}"#,
        )
        .unwrap();

        let report = scan_configs(None, Some(daemon));

        assert_eq!(report.registrations.len(), 1);
        assert_eq!(report.registrations[0].source, root.join(".mcp.json"));
    }

    #[test]
    fn config_scan_reports_stale_paths_and_invalid_mcp_registration() {
        let root = unique_test_dir("doctor-stale-registration");
        let workspace = root.join("repo");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(
            workspace.join(".mcp.json"),
            r#"{"mcpServers":{"lattice":{"command":"/missing/lattice","args":["--workspace","/repo"]}}}"#,
        )
        .unwrap();

        let report = scan_configs(None, Some(workspace));

        assert_eq!(report.stale_paths.len(), 1);
        assert!(report.stale_paths[0].contains("/missing/lattice"));
        assert_eq!(report.registration_issues.len(), 1);
        assert!(report.registration_issues[0].contains("--stdio"));
    }

    #[test]
    fn config_scan_rejects_hook_timeouts_that_do_not_exceed_internal_timeout() {
        let root = unique_test_dir("doctor-hook-timeout");
        let workspace = root.join("repo");
        let hook = workspace.join("integrations/codex/hooks/session-start.sh");
        fs::create_dir_all(hook.parent().unwrap()).unwrap();
        fs::write(&hook, "#!/bin/sh\n").unwrap();
        fs::create_dir_all(workspace.join(".codex")).unwrap();
        fs::write(
            workspace.join(".codex/hooks.json"),
            format!(
                r#"{{"hooks":{{"SessionStart":[{{"hooks":[{{"command":"{}","timeout":4}}]}}]}}}}"#,
                hook.display()
            ),
        )
        .unwrap();

        let report = scan_configs(None, Some(workspace));

        assert_eq!(report.hook_registrations.len(), 1);
        assert!(report.stale_paths.is_empty());
        assert_eq!(report.hook_timeout_violations.len(), 1);
        assert!(report.hook_timeout_violations[0].contains("must exceed"));
    }

    #[test]
    fn config_scan_reports_missing_lattice_hook_path() {
        let root = unique_test_dir("doctor-stale-hook");
        let workspace = root.join("repo");
        fs::create_dir_all(workspace.join(".codex")).unwrap();
        fs::write(
            workspace.join(".codex/hooks.json"),
            r#"{"hooks":{"SessionStart":[{"hooks":[{"command":"/missing/integrations/codex/hooks/session-start.sh","timeout":5}]}]}}"#,
        )
        .unwrap();

        let report = scan_configs(None, Some(workspace));

        assert_eq!(report.hook_registrations.len(), 1);
        assert_eq!(report.stale_paths.len(), 1);
        assert!(report.stale_paths[0].contains("session-start.sh"));
        assert!(report.hook_timeout_violations.is_empty());
    }

    #[test]
    fn orphan_proxy_count_uses_injectable_process_snapshot() {
        struct FixtureSnapshot(&'static str);

        impl ProcessSnapshot for FixtureSnapshot {
            fn output(&self) -> Result<String> {
                Ok(self.0.to_string())
            }
        }

        let snapshot = FixtureSnapshot(
            "  12     1 /opt/lattice --stdio --workspace /repo\n\
               13     7 /opt/lattice --stdio --workspace /repo\n\
               14     1 /usr/bin/other --stdio\n\
               15     1 /opt/lattice doctor\n\
               16     1 /opt/lattice-helper --stdio\n",
        );

        assert_eq!(orphan_proxy_count(&snapshot).unwrap(), 1);
    }

    #[test]
    fn index_status_reports_partial_index_as_a_warning() {
        let root = PathBuf::from("/workspace");
        let output = format_index_status(
            &root,
            &json!({
                "status": "ready",
                "files": 42,
                "parse_failures": 2,
                "failed_files": ["src/a.rs", "src/z.rs"]
            }),
        );

        assert_eq!(
            output,
            "WARN workspace /workspace status=ready files=42 index=partial parse_failures=2 failed_files=src/a.rs,src/z.rs"
        );
    }

    #[test]
    fn capture_health_reports_aggregate_outcomes_without_sensitive_identifiers() {
        let health = CaptureHealth {
            total_attempts: 3,
            outcomes: BTreeMap::from([
                ("captured".to_string(), 1),
                ("daemon_unavailable".to_string(), 1),
                ("queued".to_string(), 1),
            ]),
        };

        let output = format_capture_health(&health);

        assert_eq!(
            output,
            "WARN capture health attempts=3 captured=1 daemon_unavailable=1 queued=1"
        );
        for forbidden in ["capability", "session", "/workspace", "delivery"] {
            assert!(!output.contains(forbidden));
        }
    }

    #[test]
    fn capture_health_reports_no_attempts_as_healthy() {
        assert_eq!(
            format_capture_health(&CaptureHealth::default()),
            "PASS capture health no attempts recorded"
        );
    }

    #[test]
    fn binary_paths_use_daemon_target_when_invoked_from_daemon_directory() {
        let root = unique_test_dir("doctor-binary-path");
        let daemon = root.join("daemon");
        fs::create_dir_all(daemon.join("crates/lattice-daemon")).unwrap();
        fs::write(daemon.join("Cargo.toml"), "[package]\nname = \"fixture\"\n").unwrap();
        fs::write(
            daemon.join("crates/lattice-daemon/Cargo.toml"),
            "[package]\nname = \"fixture-daemon\"\n",
        )
        .unwrap();
        let current = daemon.join("target/release/lattice");

        assert_eq!(
            binary_paths_for(&current, &daemon),
            vec![current],
            "the current daemon binary must not produce a daemon/daemon target candidate"
        );
    }

    #[test]
    fn binary_paths_use_daemon_target_when_invoked_from_repository_root() {
        let root = unique_test_dir("doctor-repository-binary-path");
        let daemon = root.join("daemon");
        fs::create_dir_all(daemon.join("crates/lattice-daemon")).unwrap();
        fs::write(daemon.join("Cargo.toml"), "[package]\nname = \"fixture\"\n").unwrap();
        fs::write(
            daemon.join("crates/lattice-daemon/Cargo.toml"),
            "[package]\nname = \"fixture-daemon\"\n",
        )
        .unwrap();
        let current = PathBuf::from("/installed/lattice");

        assert_eq!(
            binary_paths_for(&current, &root),
            vec![current, daemon.join("target/release/lattice")]
        );
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
