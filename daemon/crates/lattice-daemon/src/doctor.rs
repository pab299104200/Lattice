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
use std::sync::mpsc;
use std::time::{Duration, Instant, UNIX_EPOCH};

use crate::adoption_metrics::{capture_health_for_workspace, CaptureHealth};
use crate::proxy::daemon_addr;
use crate::rpc::server::read_message_sync;
use crate::transport::{self, ClientKind, ProxyRequest};
use crate::verification_producer::{verification_config_health, VerificationConfigHealth};

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
    pub(crate) event: String,
    pub(crate) command: String,
    pub(crate) timeout_secs: Option<u64>,
}

const DOCTOR_HOOK_FIXTURE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_HOOK_FIXTURE_OUTPUT_BYTES: usize = 8 * 1024;
const SESSION_START_RECOVERY_NOTICE: &str = "lattice: daemon unreachable — run 'lattice doctor'";

#[derive(Debug, Clone, PartialEq, Eq)]
struct HookFixtureResult {
    elapsed_ms: u128,
}

/// The doctor fixture deliberately cannot reach the daemon.  SessionStart is
/// therefore the sole hook allowed to render the adapter's bounded recovery
/// notice; all other fixture output is a wiring failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HookFixtureStdout {
    Silent,
    SessionStartRecovery(HookFixtureHost),
    /// The workspace enforces the Lattice workflow. The fixture cuts the
    /// hook off from the daemon, so the hook must say so, once, and must not
    /// block anything (`docs/hook-enforcement.md`, "Fail-open rule").
    /// Silence here would be the defect enforcement exists to prevent.
    EnforcedFailOpen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HookFixtureHost {
    Codex,
    ClaudeCode,
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

    // What the next daemon start would use. Checked here because an invalid
    // settings file stops the daemon, and this is where an operator looks.
    let configured = crate::daemon_settings::load();
    match &configured {
        Ok(settings) => println!("{}", format_configured_settings(settings)),
        Err(error) => {
            failures += 1;
            println!(
                "FAIL daemon settings: {error:#}. The daemon will not start until this is fixed."
            );
        }
    }

    let mut running_reported = false;
    for root in &workspace_roots {
        match index_status_for(root) {
            Ok(status) => {
                let payload = status_payload(&status);
                if !running_reported {
                    running_reported = true;
                    let (line, warned) =
                        format_running_settings(payload.get("daemon"), configured.as_ref().ok());
                    warnings += usize::from(warned);
                    println!("{line}");
                }
                let line = format_index_status(root, &status);
                warnings += usize::from(line.starts_with("WARN"));
                println!("{line}");
            }
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
        match verification_config_health(root) {
            VerificationConfigHealth::Absent => {}
            VerificationConfigHealth::Valid { checks } => println!(
                "PASS workspace {} verification manifest has {} declared check(s)",
                root.display(),
                checks
            ),
            VerificationConfigHealth::Invalid => {
                failures += 1;
                println!(
                    "FAIL workspace {} verification manifest is unsafe or invalid",
                    root.display()
                );
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
    if let Some(workspace) = workspace_roots.first() {
        match std::env::current_exe() {
            Ok(executable) => {
                for registration in &config_report.hook_registrations {
                    if !hook_fixture_is_eligible(registration, workspace) {
                        continue;
                    }
                    match run_hook_fixture(registration, workspace, &executable) {
                        Ok(result) => println!(
                            "PASS hook fixture {} at {} ({} ms)",
                            registration.event, registration.json_path, result.elapsed_ms
                        ),
                        Err(error) => {
                            failures += 1;
                            println!(
                                "FAIL hook fixture {} at {}: {}",
                                registration.event, registration.json_path, error
                            );
                        }
                    }
                }
            }
            Err(error) => {
                failures += config_report.hook_registrations.len();
                println!("FAIL hook fixture setup: cannot resolve lattice executable: {error}");
            }
        }
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

fn status_payload(response: &Value) -> Value {
    response["content"]
        .as_array()
        .and_then(|items| items.first())
        .and_then(|item| item["text"].as_str())
        .and_then(|text| serde_json::from_str::<Value>(text).ok())
        .unwrap_or_else(|| response.clone())
}

fn describe_ceiling(ceiling: Option<u64>, source: &str) -> String {
    match ceiling {
        Some(ceiling) => format!("shard ceiling {ceiling} from {source}"),
        None => "no shard ceiling".to_string(),
    }
}

fn format_configured_settings(settings: &crate::daemon_settings::DaemonSettings) -> String {
    let file = match (&settings.settings_file, settings.settings_file_present) {
        (Some(path), true) => format!("settings file {} is valid", path.display()),
        (Some(path), false) => format!("no settings file at {}", path.display()),
        (None, _) => "no settings file location (HOME is unset)".to_string(),
    };
    format!(
        "PASS daemon settings: memory budget {} MiB from {}; {}; {}",
        settings.memory_budget_bytes / (1024 * 1024),
        settings.memory_budget_source.describe(),
        describe_ceiling(
            settings.max_loaded_shards.map(|ceiling| ceiling as u64),
            &settings.max_loaded_shards_source.describe()
        ),
        file
    )
}

/// What the running daemon is doing with its capacity, and whether a restart
/// would change it. Capacity follows connected agents; memory is the limit.
fn format_running_settings(
    running: Option<&Value>,
    configured: Option<&crate::daemon_settings::DaemonSettings>,
) -> (String, bool) {
    let Some(running) = running.filter(|running| running.is_object()) else {
        return (
            "WARN running daemon does not report its capacity; it predates capacity reporting, so restart it to load this build".to_string(),
            true,
        );
    };
    if running.get("connected_workspaces").is_none() {
        return (
            "WARN running daemon still uses a fixed shard count; restart it to load demand-driven capacity".to_string(),
            true,
        );
    }
    let mib = |key: &str| running[key].as_u64().map(|bytes| bytes / (1024 * 1024));
    let ceiling = running["max_loaded_shards"].as_u64();
    let budget = mib("memory_budget_bytes");
    let memory = match (mib("memory_footprint_bytes"), budget) {
        (Some(used), Some(budget)) => format!(
            "memory {used} MiB of {budget} MiB after {} h up",
            running["uptime_secs"].as_u64().unwrap_or_default() / 3600
        ),
        _ => "memory unknown".to_string(),
    };
    let usage = format!(
        "running daemon: {} connected workspace(s), {} agent(s); {} shard(s) loaded, {} idle, {} connected but idle; {memory}; {}",
        running["connected_workspaces"],
        running["agents"],
        running["loaded_shards"],
        running["idle_shards"],
        running["connected_idle_shards"],
        describe_ceiling(
            ceiling,
            running["max_loaded_shards_source"].as_str().unwrap_or("unknown")
        ),
    );
    match running["deferred"].as_str() {
        Some("ceiling") => {
            return (
                format!("WARN {usage}. The next workspace to connect would be DEFERRED: the ceiling is reached and every loaded workspace is in use. Raise or remove max_loaded_shards in the daemon settings file"),
                true,
            )
        }
        Some("memory") => {
            return (
                format!("WARN {usage}. The next workspace to connect would be DEFERRED: memory is too close to its budget to load another workspace and no loaded workspace is idle. Raise memory_budget_mb or close an agent session"),
                true,
            )
        }
        _ => {}
    }
    if let Some(configured) = configured {
        let configured_ceiling = configured.max_loaded_shards.map(|ceiling| ceiling as u64);
        let configured_budget = configured.memory_budget_bytes / (1024 * 1024);
        if configured_ceiling != ceiling || Some(configured_budget) != budget {
            return (
                format!(
                    "WARN {usage}. A restart would use a memory budget of {configured_budget} MiB and {}",
                    describe_ceiling(
                        configured_ceiling,
                        &configured.max_loaded_shards_source.describe()
                    )
                ),
                true,
            );
        }
    }
    (format!("PASS {usage}"), false)
}

fn format_index_status(root: &Path, response: &Value) -> String {
    let payload = status_payload(response);
    let status = payload["status"].as_str().unwrap_or("unknown");
    if status == "deferred" {
        return format!(
            "WARN workspace {} status=deferred index=not_loaded: {}",
            root.display(),
            payload["bootstrap"]["deferred_reason"]
                .as_str()
                .unwrap_or("the daemon could not load this workspace; see `lattice status`")
        );
    }
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
                    event: event.to_string(),
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
            Some(
                "session-start.sh"
                    | "user-prompt-submit.sh"
                    | "post-tool-use.sh"
                    | "stop.sh"
                    | "session-end.sh"
            )
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
            (!hook_timeout_is_valid(registration, timeout)).then(|| {
                format!(
                    "{} at {} uses an invalid {timeout}s outer timeout for {}",
                    registration.source.display(),
                    registration.json_path,
                    registration.event,
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

fn hook_timeout_is_valid(registration: &HookRegistration, timeout_secs: u64) -> bool {
    if registration.event == "SessionEnd" && path_has_component(&registration.source, ".codex") {
        timeout_secs > 2
    } else {
        timeout_secs > crate::install::MAX_INNER_HOOK_TIMEOUT_SECS
    }
}

/// Execute an installed hook with a synthetic, allowlisted host envelope.
/// This is a doctor-only wiring check: protected state is disposable and the
/// loopback address is intentionally unreachable, so no binding or delivery
/// can be created on an operator's live daemon.
fn run_hook_fixture(
    registration: &HookRegistration,
    workspace: &Path,
    executable: &Path,
) -> Result<HookFixtureResult> {
    let timeout_secs = registration
        .timeout_secs
        .ok_or_else(|| anyhow::anyhow!("configured hook has no numeric outer timeout"))?;
    if !hook_timeout_is_valid(registration, timeout_secs) {
        anyhow::bail!(
            "configured outer timeout ({timeout_secs}s) is invalid for {}",
            registration.event
        );
    }
    let command = hook_command_path(registration, workspace);
    if !command.is_file() {
        anyhow::bail!("configured hook path is missing: {}", command.display());
    }
    let enforcing = crate::hook_enforcement::load_policy(workspace).enforcing();
    let expected_stdout = hook_fixture_stdout_contract(registration, enforcing)?;
    let state_root = doctor_hook_fixture_state_root()?;
    let result = run_hook_fixture_at(
        &command,
        &registration.event,
        workspace,
        executable,
        &state_root,
        Duration::from_secs(timeout_secs).min(DOCTOR_HOOK_FIXTURE_TIMEOUT),
        expected_stdout,
    );
    let cleanup = fs::remove_dir_all(&state_root);
    match result {
        Ok(result) => {
            cleanup.with_context(|| {
                format!(
                    "remove doctor hook fixture state `{}`",
                    state_root.display()
                )
            })?;
            Ok(result)
        }
        Err(error) => {
            let _ = cleanup;
            Err(error)
        }
    }
}

fn hook_command_path(registration: &HookRegistration, workspace: &Path) -> PathBuf {
    let command = PathBuf::from(&registration.command);
    if command.is_absolute() {
        command
    } else {
        workspace.join(command)
    }
}

fn hook_fixture_is_eligible(registration: &HookRegistration, workspace: &Path) -> bool {
    registration
        .timeout_secs
        .is_some_and(|timeout| hook_timeout_is_valid(registration, timeout))
        && hook_command_path(registration, workspace).is_file()
}

fn hook_fixture_stdout_contract(
    registration: &HookRegistration,
    enforcing: bool,
) -> Result<HookFixtureStdout> {
    // SessionEnd has no channel to the agent, so it is silent either way.
    if enforcing && registration.event != "SessionEnd" {
        return Ok(HookFixtureStdout::EnforcedFailOpen);
    }
    if registration.event != "SessionStart" {
        return Ok(HookFixtureStdout::Silent);
    }

    let command = Path::new(&registration.command);
    match (
        path_has_component(&registration.source, ".codex"),
        path_has_component(command, "codex"),
        path_has_component(&registration.source, ".claude"),
        path_has_component(command, "claude-code"),
    ) {
        (true, true, false, false) => Ok(HookFixtureStdout::SessionStartRecovery(
            HookFixtureHost::Codex,
        )),
        (false, false, true, true) => Ok(HookFixtureStdout::SessionStartRecovery(
            HookFixtureHost::ClaudeCode,
        )),
        _ => anyhow::bail!(
            "SessionStart fixture is not a recognized Codex or Claude Code hook: {} at {}",
            registration.source.display(),
            registration.json_path
        ),
    }
}

fn path_has_component(path: &Path, component: &str) -> bool {
    path.components()
        .any(|part| part.as_os_str() == std::ffi::OsStr::new(component))
}

fn doctor_hook_fixture_state_root() -> Result<PathBuf> {
    let nonce = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("read clock for doctor hook fixture state")?
            .as_nanos()
    );
    let root = std::env::temp_dir().join(format!("lattice-doctor-hook-fixture-{nonce}"));
    fs::create_dir(&root)
        .with_context(|| format!("create doctor hook fixture state `{}`", root.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&root)?.permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&root, permissions)?;
    }
    Ok(root)
}

fn run_hook_fixture_at(
    command: &Path,
    event: &str,
    workspace: &Path,
    executable: &Path,
    state_root: &Path,
    timeout: Duration,
    expected_stdout: HookFixtureStdout,
) -> Result<HookFixtureResult> {
    let started = Instant::now();
    let mut child = Command::new(command)
        .current_dir(workspace)
        .env("LATTICE_BIN", executable)
        .env("LATTICE_SKIP_METRICS", "1")
        .env("XDG_STATE_HOME", state_root)
        .env("LATTICE_DAEMON_ADDR", "127.0.0.1:0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("could not spawn `{}`", command.display()))?;
    child
        .stdin
        .as_mut()
        .context("configured hook has no stdin")?
        .write_all(hook_fixture_payload(event).as_bytes())?;
    drop(child.stdin.take());

    let stdout = child
        .stdout
        .take()
        .context("configured hook has no stdout")?;
    let stderr = child
        .stderr
        .take()
        .context("configured hook has no stderr")?;
    let (stdout_sender, stdout_receiver) = mpsc::sync_channel(1);
    let (stderr_sender, stderr_receiver) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _ = stdout_sender.send(read_limited_output(stdout));
    });
    std::thread::spawn(move || {
        let _ = stderr_sender.send(read_limited_output(stderr));
    });

    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("exceeded the {} ms fixture deadline", timeout.as_millis());
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let stdout = stdout_receiver
        .recv_timeout(Duration::from_millis(250))
        .map_err(|_| anyhow::anyhow!("closed without readable stdout"))??;
    let stderr = stderr_receiver
        .recv_timeout(Duration::from_millis(250))
        .map_err(|_| anyhow::anyhow!("closed without readable stderr"))??;
    if !status.success() {
        anyhow::bail!("exited with {status}: {}", fixture_output_summary(&stderr));
    }
    validate_hook_fixture_stdout(expected_stdout, &stdout)?;
    Ok(HookFixtureResult {
        elapsed_ms: started.elapsed().as_millis(),
    })
}

fn validate_hook_fixture_stdout(
    expected: HookFixtureStdout,
    stdout: &LimitedFixtureOutput,
) -> Result<()> {
    if stdout.truncated {
        anyhow::bail!("wrote stdout: {}", fixture_output_summary(stdout));
    }
    match expected {
        HookFixtureStdout::EnforcedFailOpen => {
            let text = String::from_utf8_lossy(&stdout.bytes);
            let notices = [
                crate::hook_enforcement::NoticeCondition::DaemonUnreachable,
                crate::hook_enforcement::NoticeCondition::CaptureUnavailable,
            ]
            .map(crate::hook_enforcement::notice_text);
            // Notices may arrive JSON-escaped inside a host envelope.
            let says_why = notices.iter().any(|notice| {
                text.contains(notice.as_str())
                    || text.contains(serde_json::to_string(notice).unwrap().trim_matches('"'))
            });
            let blocks = text.contains("\"deny\"") || text.contains("\"block\"");
            if says_why && !blocks {
                Ok(())
            } else if blocks {
                anyhow::bail!(
                    "blocked while the daemon was unreachable; enforcement must fail open: {}",
                    fixture_output_summary(stdout)
                )
            } else {
                anyhow::bail!(
                    "did not state that Lattice was unreachable, which an enforcing workspace must: {}",
                    fixture_output_summary(stdout)
                )
            }
        }
        HookFixtureStdout::Silent if stdout.bytes.is_empty() => Ok(()),
        HookFixtureStdout::Silent => {
            anyhow::bail!("wrote stdout: {}", fixture_output_summary(stdout))
        }
        HookFixtureStdout::SessionStartRecovery(HookFixtureHost::Codex)
            if stdout.bytes == format!("{SESSION_START_RECOVERY_NOTICE}\n").as_bytes() =>
        {
            Ok(())
        }
        HookFixtureStdout::SessionStartRecovery(HookFixtureHost::Codex) => anyhow::bail!(
            "wrote unexpected Codex SessionStart recovery output: {}",
            fixture_output_summary(stdout)
        ),
        HookFixtureStdout::SessionStartRecovery(HookFixtureHost::ClaudeCode) => {
            let envelope: Value = serde_json::from_slice(&stdout.bytes).map_err(|_| {
                anyhow::anyhow!(
                    "wrote invalid Claude Code SessionStart recovery output: {}",
                    fixture_output_summary(stdout)
                )
            })?;
            let expected = json!({
                "hookSpecificOutput": {
                    "hookEventName": "SessionStart",
                    "additionalContext": SESSION_START_RECOVERY_NOTICE,
                }
            });
            if envelope == expected {
                Ok(())
            } else {
                anyhow::bail!(
                    "wrote unexpected Claude Code SessionStart recovery output: {}",
                    fixture_output_summary(stdout)
                );
            }
        }
    }
}

struct LimitedFixtureOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

fn read_limited_output(mut reader: impl Read) -> std::io::Result<LimitedFixtureOutput> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 1024];
    let mut truncated = false;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            return Ok(LimitedFixtureOutput { bytes, truncated });
        }
        let remaining = MAX_HOOK_FIXTURE_OUTPUT_BYTES.saturating_sub(bytes.len());
        let copied = remaining.min(read);
        bytes.extend_from_slice(&buffer[..copied]);
        truncated |= copied < read;
    }
}

fn fixture_output_summary(output: &LimitedFixtureOutput) -> String {
    let text = String::from_utf8_lossy(&output.bytes).trim().to_string();
    if output.truncated {
        if text.is_empty() {
            "output exceeded the 8 KiB diagnostic limit".to_string()
        } else {
            format!("{text} (truncated at 8 KiB)")
        }
    } else if text.is_empty() {
        "no diagnostic output".to_string()
    } else {
        text
    }
}

fn hook_fixture_payload(event: &str) -> &'static str {
    match event {
        "SessionStart" => r#"{"session_id":"doctor-hook-fixture","source":"startup"}"#,
        "UserPromptSubmit" => {
            r#"{"session_id":"doctor-hook-fixture","prompt":"doctor bounded hook fixture"}"#
        }
        "PostToolUse" => {
            r#"{"session_id":"doctor-hook-fixture","tool_name":"apply_patch","file_path":"README.md"}"#
        }
        "Stop" => {
            r#"{"session_id":"doctor-hook-fixture","last_assistant_message":"bounded doctor turn summary","transcript_path":"/tmp/private","cwd":"/private"}"#
        }
        "SessionEnd" => {
            r#"{"session_id":"doctor-hook-fixture","transcript_path":"/tmp/private","reason":"private","cwd":"/private","final_summary":"private"}"#
        }
        _ => "{}",
    }
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
    fn an_enforcing_workspace_fixture_must_fail_open_loudly() {
        use crate::hook_enforcement::{notice_text, NoticeCondition};
        let output = |text: String| LimitedFixtureOutput {
            bytes: text.into_bytes(),
            truncated: false,
        };
        let unreachable = notice_text(NoticeCondition::DaemonUnreachable);
        // Plain text, as Codex prints it.
        validate_hook_fixture_stdout(
            HookFixtureStdout::EnforcedFailOpen,
            &output(format!("lattice: {unreachable}\n")),
        )
        .unwrap();
        // Inside a Claude Code envelope.
        let envelope = serde_json::json!({"hookSpecificOutput": {
            "hookEventName": "UserPromptSubmit",
            "additionalContext": format!("lattice: {unreachable}")}});
        validate_hook_fixture_stdout(
            HookFixtureStdout::EnforcedFailOpen,
            &output(envelope.to_string()),
        )
        .unwrap();
        // Stop reports missing capture instead.
        let stop = serde_json::json!({"systemMessage":
            format!("lattice: {}", notice_text(NoticeCondition::CaptureUnavailable))});
        validate_hook_fixture_stdout(
            HookFixtureStdout::EnforcedFailOpen,
            &output(stop.to_string()),
        )
        .unwrap();

        // Silence is the defect enforcement exists to prevent.
        let error = validate_hook_fixture_stdout(
            HookFixtureStdout::EnforcedFailOpen,
            &output(String::new()),
        )
        .unwrap_err();
        assert!(error.to_string().contains("did not state"), "{error}");
        // Blocking while the daemon is unreachable breaks the fail-open rule.
        let deny = serde_json::json!({"hookSpecificOutput": {
            "permissionDecision": "deny",
            "permissionDecisionReason": format!("lattice: {unreachable}")}});
        let error = validate_hook_fixture_stdout(
            HookFixtureStdout::EnforcedFailOpen,
            &output(deny.to_string()),
        )
        .unwrap_err();
        assert!(error.to_string().contains("must fail open"), "{error}");
    }

    #[test]
    fn doctor_reports_capacity_memory_the_ceiling_and_a_pending_restart() {
        use crate::daemon_settings::{DaemonSettings, SettingSource};
        const MIB: u64 = 1024 * 1024;
        let path = PathBuf::from("/home/op/.config/lattice/daemon.toml");
        let with_ceiling = DaemonSettings {
            max_loaded_shards: Some(6),
            max_loaded_shards_source: SettingSource::SettingsFile(path.clone()),
            memory_budget_bytes: 5461 * MIB,
            memory_budget_source: SettingSource::Default,
            settings_file: Some(path.clone()),
            settings_file_present: true,
        };
        assert_eq!(
            format_configured_settings(&with_ceiling),
            "PASS daemon settings: memory budget 5461 MiB from built-in default; shard ceiling 6 from settings file /home/op/.config/lattice/daemon.toml; settings file /home/op/.config/lattice/daemon.toml is valid"
        );
        let mut unlimited = DaemonSettings::unconfigured(None);
        unlimited.memory_budget_bytes = 5461 * MIB;
        unlimited.settings_file = Some(path);
        let line = format_configured_settings(&unlimited);
        assert!(line.contains("no shard ceiling"), "{line}");
        assert!(line.ends_with("no settings file at /home/op/.config/lattice/daemon.toml"));

        let running = |ceiling: Value, deferred: Value| {
            json!({
                "max_loaded_shards": ceiling,
                "max_loaded_shards_source": "settings file /home/op/.config/lattice/daemon.toml",
                "memory_budget_bytes": 5461 * MIB, "memory_footprint_bytes": 1443 * MIB,
                "uptime_secs": 8 * 3600, "connected_workspaces": 5, "agents": 7,
                "loaded_shards": 5, "idle_shards": 0, "connected_idle_shards": 2,
                "deferred": deferred,
            })
        };
        let (line, warned) =
            format_running_settings(Some(&running(json!(6), Value::Null)), Some(&with_ceiling));
        assert!(!warned, "{line}");
        assert_eq!(
            line,
            "PASS running daemon: 5 connected workspace(s), 7 agent(s); 5 shard(s) loaded, 0 idle, 2 connected but idle; memory 1443 MiB of 5461 MiB after 8 h up; shard ceiling 6 from settings file /home/op/.config/lattice/daemon.toml"
        );

        // A ceiling or the memory budget about to defer someone is said loudly.
        let (line, warned) = format_running_settings(
            Some(&running(json!(6), json!("ceiling"))),
            Some(&with_ceiling),
        );
        assert!(warned);
        assert!(
            line.contains("would be DEFERRED: the ceiling is reached"),
            "{line}"
        );
        let (line, warned) = format_running_settings(
            Some(&running(json!(6), json!("memory"))),
            Some(&with_ceiling),
        );
        assert!(warned);
        assert!(
            line.contains("would be DEFERRED: memory is too close to its budget"),
            "{line}"
        );

        // The settings changed after the daemon started.
        let (line, warned) =
            format_running_settings(Some(&running(json!(6), Value::Null)), Some(&unlimited));
        assert!(warned);
        assert!(
            line.ends_with("A restart would use a memory budget of 5461 MiB and no shard ceiling"),
            "{line}"
        );

        // Older daemons.
        let fixed_count = json!({"max_loaded_shards": 6, "loaded_shards": 6, "pinned_shards": 5});
        let (line, warned) = format_running_settings(Some(&fixed_count), Some(&with_ceiling));
        assert!(warned);
        assert!(line.contains("still uses a fixed shard count"), "{line}");
        for missing in [None, Some(&Value::Null)] {
            let (line, warned) = format_running_settings(missing, Some(&with_ceiling));
            assert!(warned);
            assert!(line.contains("predates capacity reporting"), "{line}");
        }
    }

    #[test]
    fn a_deferred_workspace_is_a_warning_not_a_healthy_pass() {
        let response = json!({"content": [{"type": "text", "text": json!({
            "status": "deferred", "indexing": false, "files": null,
            "bootstrap": {"deferred_reason": "the shard ceiling max_loaded_shards=6 is reached"}
        }).to_string()}]});
        let line = format_index_status(Path::new("/work/relay"), &response);
        assert!(line.starts_with("WARN workspace /work/relay status=deferred"));
        assert!(line.contains("the shard ceiling max_loaded_shards=6 is reached"));
        assert!(!line.contains("watcher=healthy"));
    }

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

        let report = scan_configs(None, Some(workspace.clone()));

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

        let report = scan_configs(None, Some(workspace.clone()));

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

        let report = scan_configs(None, Some(workspace.clone()));

        assert_eq!(report.hook_registrations.len(), 1);
        assert!(report.stale_paths.is_empty());
        assert_eq!(report.hook_timeout_violations.len(), 1);
        assert!(report.hook_timeout_violations[0].contains("invalid 4s"));
        assert!(!hook_fixture_is_eligible(
            &report.hook_registrations[0],
            &workspace
        ));
    }

    #[test]
    fn config_scan_accepts_codex_session_end_three_second_timeout() {
        let root = unique_test_dir("doctor-session-end-timeout");
        let workspace = root.join("repo");
        let hook = workspace.join("integrations/codex/hooks/session-end.sh");
        fs::create_dir_all(hook.parent().unwrap()).unwrap();
        fs::write(&hook, "#!/bin/sh\n").unwrap();
        fs::create_dir_all(workspace.join(".codex")).unwrap();
        fs::write(
            workspace.join(".codex/hooks.json"),
            format!(
                r#"{{"hooks":{{"SessionEnd":[{{"hooks":[{{"command":"{}","timeout":3}}]}}]}}}}"#,
                hook.display()
            ),
        )
        .unwrap();

        let report = scan_configs(None, Some(workspace.clone()));

        assert!(report.hook_timeout_violations.is_empty());
        assert!(hook_fixture_is_eligible(
            &report.hook_registrations[0],
            &workspace
        ));
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

        let report = scan_configs(None, Some(workspace.clone()));

        assert_eq!(report.hook_registrations.len(), 1);
        assert_eq!(report.stale_paths.len(), 1);
        assert!(report.stale_paths[0].contains("session-start.sh"));
        assert!(report.hook_timeout_violations.is_empty());
        assert!(!hook_fixture_is_eligible(
            &report.hook_registrations[0],
            &workspace
        ));
    }

    #[cfg(unix)]
    #[test]
    fn configured_bounded_hook_fixture_executes_with_sanitized_input() {
        use std::os::unix::fs::PermissionsExt;

        let root = unique_test_dir("doctor-hook-fixture");
        let workspace = root.join("workspace");
        let hook = workspace.join("integrations/codex/hooks/session-start.sh");
        fs::create_dir_all(hook.parent().unwrap()).unwrap();
        fs::create_dir_all(&workspace).unwrap();
        fs::write(
            &hook,
            "#!/bin/sh\ninput=$(cat)\nprintf '%s' \"$input\" | grep -q '\"session_id\":\"doctor-hook-fixture\"'\ntest -n \"$LATTICE_BIN\"\ntest \"$LATTICE_DAEMON_ADDR\" = '127.0.0.1:0'\nprintf '%s\\n' \"lattice: daemon unreachable — run 'lattice doctor'\"\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&hook).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&hook, permissions).unwrap();
        let registration = HookRegistration {
            source: workspace.join(".codex/hooks.json"),
            json_path: "$.hooks.SessionStart[0].hooks[0]".to_string(),
            event: "SessionStart".to_string(),
            command: hook.to_string_lossy().into_owned(),
            timeout_secs: Some(5),
        };
        let state = root.join("state");
        fs::create_dir(&state).unwrap();

        let result = run_hook_fixture_at(
            &hook,
            &registration.event,
            &workspace,
            Path::new("/fixture/lattice"),
            &state,
            Duration::from_secs(2),
            HookFixtureStdout::SessionStartRecovery(HookFixtureHost::Codex),
        );

        assert!(result.is_ok(), "{result:?}");
        assert!(hook_fixture_is_eligible(&registration, &workspace));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn session_start_fixture_accepts_only_expected_host_recovery_envelopes() {
        let codex = LimitedFixtureOutput {
            bytes: format!("{SESSION_START_RECOVERY_NOTICE}\n").into_bytes(),
            truncated: false,
        };
        assert!(validate_hook_fixture_stdout(
            HookFixtureStdout::SessionStartRecovery(HookFixtureHost::Codex),
            &codex,
        )
        .is_ok());

        let claude = LimitedFixtureOutput {
            bytes: json!({
                "hookSpecificOutput": {
                    "hookEventName": "SessionStart",
                    "additionalContext": SESSION_START_RECOVERY_NOTICE,
                }
            })
            .to_string()
            .into_bytes(),
            truncated: false,
        };
        assert!(validate_hook_fixture_stdout(
            HookFixtureStdout::SessionStartRecovery(HookFixtureHost::ClaudeCode),
            &claude,
        )
        .is_ok());

        let unexpected = LimitedFixtureOutput {
            bytes: format!("{SESSION_START_RECOVERY_NOTICE}\nunexpected").into_bytes(),
            truncated: false,
        };
        let error = validate_hook_fixture_stdout(
            HookFixtureStdout::SessionStartRecovery(HookFixtureHost::Codex),
            &unexpected,
        )
        .unwrap_err();
        assert!(error.to_string().contains("unexpected Codex"));

        let wrong_claude_event = LimitedFixtureOutput {
            bytes: json!({
                "hookSpecificOutput": {
                    "hookEventName": "UserPromptSubmit",
                    "additionalContext": SESSION_START_RECOVERY_NOTICE,
                }
            })
            .to_string()
            .into_bytes(),
            truncated: false,
        };
        let error = validate_hook_fixture_stdout(
            HookFixtureStdout::SessionStartRecovery(HookFixtureHost::ClaudeCode),
            &wrong_claude_event,
        )
        .unwrap_err();
        assert!(error.to_string().contains("unexpected Claude Code"));
    }

    #[test]
    fn session_start_fixture_contract_requires_matching_host_configuration() {
        let codex = HookRegistration {
            source: PathBuf::from("/workspace/.codex/hooks.json"),
            json_path: "$.hooks.SessionStart[0].hooks[0]".to_string(),
            event: "SessionStart".to_string(),
            command: "/workspace/integrations/codex/hooks/session-start.sh".to_string(),
            timeout_secs: Some(5),
        };
        assert_eq!(
            hook_fixture_stdout_contract(&codex, false).unwrap(),
            HookFixtureStdout::SessionStartRecovery(HookFixtureHost::Codex)
        );

        let claude = HookRegistration {
            source: PathBuf::from("/workspace/.claude/settings.json"),
            command: "/workspace/integrations/claude-code/hooks/session-start.sh".to_string(),
            ..codex.clone()
        };
        assert_eq!(
            hook_fixture_stdout_contract(&claude, false).unwrap(),
            HookFixtureStdout::SessionStartRecovery(HookFixtureHost::ClaudeCode)
        );

        // Under enforcement every event but SessionEnd must speak up.
        assert_eq!(
            hook_fixture_stdout_contract(&codex, true).unwrap(),
            HookFixtureStdout::EnforcedFailOpen
        );
        assert_eq!(
            hook_fixture_stdout_contract(
                &HookRegistration {
                    event: "SessionEnd".to_string(),
                    ..claude.clone()
                },
                true
            )
            .unwrap(),
            HookFixtureStdout::Silent
        );
        let error = hook_fixture_stdout_contract(
            &HookRegistration {
                command: "/workspace/integrations/claude-code/hooks/session-start.sh".to_string(),
                ..codex
            },
            false,
        )
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("recognized Codex or Claude Code"));
    }

    #[cfg(unix)]
    #[test]
    fn hook_fixture_deadline_is_enforced() {
        use std::os::unix::fs::PermissionsExt;

        let root = unique_test_dir("doctor-hook-fixture-timeout");
        let workspace = root.join("workspace");
        let hook = workspace.join("integrations/codex/hooks/session-start.sh");
        fs::create_dir_all(hook.parent().unwrap()).unwrap();
        fs::create_dir_all(&workspace).unwrap();
        fs::write(&hook, "#!/bin/sh\nsleep 1\n").unwrap();
        let mut permissions = fs::metadata(&hook).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&hook, permissions).unwrap();
        let state = root.join("state");
        fs::create_dir(&state).unwrap();

        let error = run_hook_fixture_at(
            &hook,
            "SessionStart",
            &workspace,
            Path::new("/fixture/lattice"),
            &state,
            Duration::from_millis(25),
            HookFixtureStdout::Silent,
        )
        .unwrap_err();

        assert!(error.to_string().contains("fixture deadline"));
        fs::remove_dir_all(root).unwrap();
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
