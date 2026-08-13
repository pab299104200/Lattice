use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

use crate::adoption_metrics::render_metrics_for_workspace;
use crate::install::{
    reconcile_hook_config, reconcile_mcp_config, render_config, HookClient, InstallPaths,
};
use crate::proxy::daemon_addr;
use crate::transport::{self, ClientKind, ProxyRequest};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);
const INSTALL_VERIFY_TIMEOUT: Duration = Duration::from_secs(10);
const EXPECTED_MCP_TOOL_COUNT: usize = 8;

const INSTALLED_HOOKS: [(&str, &str); 4] = [
    ("SessionStart", "session-start.sh"),
    ("UserPromptSubmit", "user-prompt-submit.sh"),
    ("PostToolUse", "post-tool-use.sh"),
    ("Stop", "stop.sh"),
];

#[derive(Debug, Clone)]
pub(crate) struct CliRequest {
    tool: String,
    arguments: Value,
    workspace: PathBuf,
    json: bool,
    timeout: Duration,
}

#[derive(Debug)]
enum CliError {
    DaemonUnavailable,
    DaemonConnection(std::io::Error),
    Rpc(String),
    Other(anyhow::Error),
}

impl From<anyhow::Error> for CliError {
    fn from(error: anyhow::Error) -> Self {
        Self::Other(error)
    }
}

pub(crate) fn is_cli_query_command() -> bool {
    matches!(
        std::env::args().nth(1).as_deref(),
        Some("context")
            | Some("prepare_change")
            | Some("impact")
            | Some("search")
            | Some("diagnose")
            | Some("remember")
            | Some("recall")
            | Some("status")
            | Some("metrics")
            | Some("install")
    )
}

/// Reject invocations which are neither an explicit runtime mode nor a public
/// query command, so malformed commands cannot silently start a daemon.
pub(crate) fn run_usage_or_error() -> i32 {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        None | Some("--help") | Some("-h") => {
            println!("{}", usage());
            0
        }
        Some("--workspace") | Some("-w") => {
            eprintln!(
                "lattice: a command must come before --workspace; try `lattice status --workspace <path>`"
            );
            64
        }
        Some(command) => {
            eprintln!("lattice: unknown subcommand `{command}`\n\n{}", usage());
            64
        }
    }
}

fn usage() -> &'static str {
    "Usage: lattice <command> [options]\n\nCommands:\n  context\n  prepare_change\n  impact\n  search\n  diagnose\n  remember\n  recall\n  status\n  metrics [--memory]\n  install <mcp|claude-code|codex>\n  doctor\n  memory-migrate\n\nRuntime modes (explicit only):\n  --daemon\n  --stdio"
}

pub(crate) async fn run_from_env() -> i32 {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("metrics") {
        return run_metrics_command(args);
    }
    if args.get(1).map(String::as_str) == Some("install") {
        return run_install_command(args);
    }
    match parse_args(args) {
        Ok(request) => run_request(request).await,
        Err(error) => {
            eprintln!("lattice: {}", error);
            1
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallTarget {
    Mcp,
    ClaudeCode,
    Codex,
}

impl InstallTarget {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "mcp" => Ok(Self::Mcp),
            "claude-code" => Ok(Self::ClaudeCode),
            "codex" => Ok(Self::Codex),
            other => Err(anyhow!(
                "unknown install target `{other}`; expected one of: mcp, claude-code, codex"
            )),
        }
    }

    fn config_path(self, workspace: &Path) -> PathBuf {
        match self {
            Self::Mcp => workspace.join(".mcp.json"),
            Self::ClaudeCode => workspace.join(".claude/settings.json"),
            Self::Codex => workspace.join(".codex/hooks.json"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InstallCommand {
    target: InstallTarget,
    workspaces: Vec<PathBuf>,
    verify: bool,
}

#[derive(Debug, Clone)]
struct InstallRuntime {
    executable: PathBuf,
    asset_root: PathBuf,
}

fn run_install_command(args: Vec<String>) -> i32 {
    let result = (|| {
        let command = parse_install_command(args)?;
        let runtime = resolve_install_runtime()?;
        run_install_command_with(command, &runtime)
    })();
    match result {
        Ok(message) => {
            println!("{message}");
            0
        }
        Err(error) => {
            eprintln!("lattice: {error:#}");
            1
        }
    }
}

fn parse_install_command(args: Vec<String>) -> Result<InstallCommand> {
    let target = args
        .get(2)
        .ok_or_else(|| anyhow!("install requires a target: mcp, claude-code, or codex"))
        .and_then(|target| InstallTarget::parse(target))?;
    let mut workspaces = Vec::new();
    let mut verify = false;
    let mut index = 3;
    while index < args.len() {
        match args[index].as_str() {
            "--verify" => verify = true,
            "--workspace" | "-w" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| anyhow!("--workspace requires a path"))?;
                workspaces.push(canonical_workspace(Path::new(value))?);
                index += 1;
            }
            argument => return Err(anyhow!("unknown install argument `{argument}`")),
        }
        index += 1;
    }
    if workspaces.is_empty() {
        workspaces.push(detect_workspace_root()?);
    }
    if target != InstallTarget::Mcp && workspaces.len() != 1 {
        return Err(anyhow!(
            "install hooks accepts exactly one --workspace; received {}",
            workspaces.len()
        ));
    }
    Ok(InstallCommand {
        target,
        workspaces,
        verify,
    })
}

/// Resolve all machine-specific installation inputs at the CLI boundary. The
/// reconciliation domain never consults PATH or a caller's current directory.
fn resolve_install_runtime() -> Result<InstallRuntime> {
    let executable = std::env::current_exe()
        .context("resolve the currently running lattice executable")?
        .canonicalize()
        .context("canonicalize the currently running lattice executable")?;
    let asset_root = stable_asset_root(&executable)?;
    Ok(InstallRuntime {
        executable,
        asset_root,
    })
}

fn stable_asset_root(executable: &Path) -> Result<PathBuf> {
    if let Some(root) = std::env::var_os("LATTICE_ASSET_ROOT") {
        let root = PathBuf::from(root);
        return root
            .canonicalize()
            .with_context(|| format!("canonicalize LATTICE_ASSET_ROOT `{}`", root.display()));
    }

    // Development builds live under daemon/target/*; installed distributions
    // may place the binary elsewhere. Search only explicit, stable roots.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for candidate in executable.ancestors().chain(manifest.ancestors()) {
        if candidate.join("integrations").is_dir() {
            return candidate
                .canonicalize()
                .with_context(|| format!("canonicalize asset root `{}`", candidate.display()));
        }
    }
    Err(anyhow!(
        "cannot locate installation assets; set LATTICE_ASSET_ROOT to the directory containing integrations/"
    ))
}

fn run_install_command_with(command: InstallCommand, runtime: &InstallRuntime) -> Result<String> {
    let config_workspace = command
        .workspaces
        .first()
        .expect("install parser guarantees a workspace");
    let config_path = command.target.config_path(config_workspace);
    let mut config = read_install_config(&config_path)?;

    match command.target {
        InstallTarget::Mcp => {
            reconcile_mcp_config(&mut config, &runtime.executable, &command.workspaces)?;
        }
        InstallTarget::ClaudeCode | InstallTarget::Codex => {
            let client = if command.target == InstallTarget::ClaudeCode {
                HookClient::ClaudeCode
            } else {
                HookClient::Codex
            };
            let paths = InstallPaths::new(runtime.executable.clone(), runtime.asset_root.clone())?;
            verify_hook_assets(&paths, client)?;
            reconcile_hook_config(&mut config, client, &paths)?;
        }
    }

    write_install_config(&config_path, &config)?;
    if command.verify {
        verify_install_config(&config_path, &command, runtime)?;
    }
    Ok(format!(
        "installed Lattice {} configuration at {}",
        match command.target {
            InstallTarget::Mcp => "MCP",
            InstallTarget::ClaudeCode => "Claude Code hook",
            InstallTarget::Codex => "Codex hook",
        },
        config_path.display()
    ))
}

fn read_install_config(path: &Path) -> Result<Value> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let config: Value = serde_json::from_str(&text).with_context(|| {
                format!("parse existing installation config `{}`", path.display())
            })?;
            if !config.is_object() {
                return Err(anyhow!(
                    "installation config `{}` must be a JSON object",
                    path.display()
                ));
            }
            Ok(config)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(error) => {
            Err(error).with_context(|| format!("read installation config `{}`", path.display()))
        }
    }
}

fn write_install_config(path: &Path, config: &Value) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("installation config `{}` has no parent", path.display()))?;
    std::fs::create_dir_all(parent).with_context(|| {
        format!(
            "create installation config directory `{}`",
            parent.display()
        )
    })?;
    let staged = path.with_extension("json.lattice-install-tmp");
    std::fs::write(&staged, render_config(config)?)
        .with_context(|| format!("write staged installation config `{}`", staged.display()))?;
    std::fs::rename(&staged, path).with_context(|| {
        format!(
            "replace installation config `{}` with staged file `{}`",
            path.display(),
            staged.display()
        )
    })
}

fn verify_hook_assets(paths: &InstallPaths, client: HookClient) -> Result<()> {
    let directory = match client {
        HookClient::ClaudeCode => "claude-code",
        HookClient::Codex => "codex",
    };
    for script in [
        "session-start.sh",
        "user-prompt-submit.sh",
        "post-tool-use.sh",
        "stop.sh",
    ] {
        let path = paths
            .asset_root
            .join("integrations")
            .join(directory)
            .join("hooks")
            .join(script);
        if !path.is_file() {
            return Err(anyhow!(
                "required {} hook asset is missing: {}",
                directory,
                path.display()
            ));
        }
    }
    Ok(())
}

/// Re-read from disk after replacement, so verification catches serialization
/// and target-path mistakes rather than validating only the in-memory value.
fn verify_install_config(
    path: &Path,
    command: &InstallCommand,
    runtime: &InstallRuntime,
) -> Result<()> {
    let mut actual = read_install_config(path)?;
    let before = render_config(&actual)?;
    match command.target {
        InstallTarget::Mcp => {
            reconcile_mcp_config(&mut actual, &runtime.executable, &command.workspaces)?
        }
        InstallTarget::ClaudeCode | InstallTarget::Codex => {
            let client = if command.target == InstallTarget::ClaudeCode {
                HookClient::ClaudeCode
            } else {
                HookClient::Codex
            };
            let paths = InstallPaths::new(runtime.executable.clone(), runtime.asset_root.clone())?;
            verify_hook_assets(&paths, client)?;
            reconcile_hook_config(&mut actual, client, &paths)?;
        }
    }
    if before != render_config(&actual)? {
        return Err(anyhow!(
            "verification failed: `{}` is not a canonical Lattice installation config",
            path.display()
        ));
    }

    match command.target {
        InstallTarget::Mcp => verify_configured_mcp_server(&actual, path)?,
        InstallTarget::ClaudeCode | InstallTarget::Codex => {
            let client = if command.target == InstallTarget::ClaudeCode {
                HookClient::ClaudeCode
            } else {
                HookClient::Codex
            };
            verify_configured_hooks(&actual, path, client, config_workspace(command), runtime)?;
        }
    }
    Ok(())
}

fn config_workspace(command: &InstallCommand) -> &Path {
    command
        .workspaces
        .first()
        .expect("install parser guarantees a workspace")
}

/// Exercise the command written to `.mcp.json`, rather than merely checking
/// that it looks plausible. A broken binary, invalid argument list, protocol
/// failure, or tool-surface drift makes `install --verify` fail.
fn verify_configured_mcp_server(config: &Value, config_path: &Path) -> Result<()> {
    let server = config
        .pointer("/mcpServers/lattice")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            anyhow!(
                "verification failed: `{}` has no mcpServers.lattice object",
                config_path.display()
            )
        })?;
    let executable = server
        .get("command")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            anyhow!(
                "verification failed: `{}` has no Lattice MCP command",
                config_path.display()
            )
        })?;
    let args = server
        .get("args")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            anyhow!(
                "verification failed: `{}` has no Lattice MCP arguments",
                config_path.display()
            )
        })?
        .iter()
        .map(|value| {
            value.as_str().map(str::to_owned).ok_or_else(|| {
                anyhow!(
                    "verification failed: `{}` has a non-string Lattice MCP argument",
                    config_path.display()
                )
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if !args.iter().any(|argument| argument == "--stdio") {
        return Err(anyhow!(
            "verification failed: `{}` Lattice MCP command is missing --stdio",
            config_path.display()
        ));
    }

    let responses = run_mcp_verification_process(
        Path::new(executable),
        &args,
        &mcp_verification_payload(),
        "configured MCP command",
    )?;
    let initialize = json_rpc_result_for(&responses, 1, "initialize")?;
    if initialize.is_null() {
        return Err(anyhow!(
            "verification failed: configured MCP command returned a null initialize result"
        ));
    }
    let tools = json_rpc_result_for(&responses, 2, "tools/list")?
        .get("tools")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("verification failed: tools/list returned no tools array"))?;
    if tools.len() != EXPECTED_MCP_TOOL_COUNT {
        return Err(anyhow!(
            "verification failed: tools/list returned {} tools, expected {EXPECTED_MCP_TOOL_COUNT}",
            tools.len()
        ));
    }
    Ok(())
}

/// Exercise an MCP proxy without closing its stdin before it has returned the
/// requested responses. Closing stdin is a client-disconnect signal for the
/// lightweight proxy; doing so immediately after writing the fixture can make
/// the proxy close its daemon socket before a freshly started daemon replies.
fn run_mcp_verification_process(
    program: &Path,
    args: &[String],
    input: &str,
    label: &str,
) -> Result<Vec<Value>> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| {
            format!(
                "verification failed: could not spawn {label} `{}`",
                program.display()
            )
        })?;
    let mut stdin = child.stdin.take().ok_or_else(|| {
        anyhow!(
            "verification failed: {label} `{}` has no stdin",
            program.display()
        )
    })?;
    stdin.write_all(input.as_bytes())?;
    stdin.flush()?;

    let stdout = child.stdout.take().ok_or_else(|| {
        anyhow!(
            "verification failed: {label} `{}` has no stdout",
            program.display()
        )
    })?;
    let (sender, receiver) = mpsc::sync_channel(4);
    std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(stdout);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    let _ = sender.send(Ok(None));
                    break;
                }
                Ok(_) => {
                    if sender.send(Ok(Some(line))).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let _ = sender.send(Err(error));
                    break;
                }
            }
        }
    });

    let deadline = Instant::now() + INSTALL_VERIFY_TIMEOUT;
    let mut responses = Vec::new();
    let result = loop {
        if responses
            .iter()
            .any(|response: &Value| response.get("id") == Some(&json!(1)))
            && responses
                .iter()
                .any(|response: &Value| response.get("id") == Some(&json!(2)))
        {
            break Ok(responses);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break Err(anyhow!(
                "verification failed: {label} exceeded {} seconds while waiting for initialize and tools/list responses",
                INSTALL_VERIFY_TIMEOUT.as_secs()
            ));
        }
        match receiver.recv_timeout(remaining.min(Duration::from_millis(250))) {
            Ok(Ok(Some(line))) if line.trim().is_empty() => {}
            Ok(Ok(Some(line))) => match serde_json::from_str::<Value>(line.trim()) {
                Ok(response) => responses.push(response),
                Err(error) => break Err(error).with_context(|| {
                    format!(
                        "verification failed: configured MCP command emitted non-JSON response `{}`",
                        line.trim()
                    )
                }),
            },
            Ok(Ok(None)) => {
                if let Some(status) = child.try_wait()? {
                    let mut stderr = String::new();
                    if let Some(mut stream) = child.stderr.take() {
                        let _ = stream.read_to_string(&mut stderr);
                    }
                    break Err(anyhow!(
                        "verification failed: {label} exited with {status} before returning initialize and tools/list responses: {}",
                        stderr.trim()
                    ));
                }
                break Err(anyhow!(
                    "verification failed: {label} closed stdout before returning initialize and tools/list responses"
                ));
            }
            Ok(Err(error)) => break Err(anyhow!(error).context(format!(
                "verification failed: could not read {label} stdout"
            ))),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break Err(anyhow!(
                "verification failed: {label} stdout reader stopped before returning initialize and tools/list responses"
            )),
        }

        // A short-lived command can write both responses and exit before this
        // loop observes it. Let the stdout reader deliver its queued lines
        // before treating that exit as a missing-response failure.
        if let Some(status) = child.try_wait()? {
            if !status.success() {
                let mut stderr = String::new();
                if let Some(mut stream) = child.stderr.take() {
                    let _ = stream.read_to_string(&mut stderr);
                }
                break Err(anyhow!(
                    "verification failed: {label} exited with {status} before returning initialize and tools/list responses: {}",
                    stderr.trim()
                ));
            }
        }
    };

    // The verifier owns this temporary proxy. Keep stdin open until responses
    // arrive, then terminate it rather than waiting for the normal idle timer.
    drop(stdin);
    if child.try_wait()?.is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

fn mcp_verification_payload() -> String {
    format!(
        "{}\n{}\n{}\n",
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "lattice-install-verify", "version": env!("CARGO_PKG_VERSION")}
            }
        }),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
    )
}

fn json_rpc_result_for<'a>(responses: &'a [Value], id: u64, method: &str) -> Result<&'a Value> {
    let response = responses
        .iter()
        .find(|response| response.get("id") == Some(&json!(id)))
        .ok_or_else(|| {
            anyhow!("verification failed: configured MCP command returned no {method} response")
        })?;
    if let Some(error) = response.get("error") {
        return Err(anyhow!(
            "verification failed: configured MCP command {method} response contained an error: {error}"
        ));
    }
    response.get("result").ok_or_else(|| {
        anyhow!("verification failed: configured MCP command {method} response has no result")
    })
}

/// Run every configured hook with a representative client payload. Session
/// startup is required to emit context; prompt and impact hooks may correctly
/// emit nothing when no result clears their relevance thresholds; Stop is an
/// acknowledgement hook and must be silent. All four must run successfully.
fn verify_configured_hooks(
    config: &Value,
    config_path: &Path,
    client: HookClient,
    workspace: &Path,
    runtime: &InstallRuntime,
) -> Result<()> {
    for (event, script) in INSTALLED_HOOKS {
        let command = configured_hook_command(config, config_path, script)?;
        let output = run_fixture_process(
            &command,
            &[],
            hook_fixture_payload(event),
            Some(HookProcessContext {
                workspace,
                executable: &runtime.executable,
            }),
            &format!("configured {event} hook"),
        )?;
        verify_hook_stdout(client, event, &output)?;
    }
    Ok(())
}

fn configured_hook_command(config: &Value, config_path: &Path, script: &str) -> Result<PathBuf> {
    let entries = config
        .pointer("/hooks")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            anyhow!(
                "verification failed: `{}` has no hooks object",
                config_path.display()
            )
        })?;
    let commands = entries
        .values()
        .filter_map(Value::as_array)
        .flatten()
        .filter_map(|entry| entry.get("hooks").and_then(Value::as_array))
        .flatten()
        .filter_map(|hook| hook.get("command").and_then(Value::as_str))
        .filter(|command| {
            Path::new(command)
                .file_name()
                .and_then(|name| name.to_str())
                == Some(script)
        })
        .collect::<Vec<_>>();
    match commands.as_slice() {
        [command] if !command.trim().is_empty() => Ok(PathBuf::from(command)),
        [] => Err(anyhow!(
            "verification failed: `{}` has no configured `{script}` hook",
            config_path.display()
        )),
        _ => Err(anyhow!(
            "verification failed: `{}` has multiple configured `{script}` hooks",
            config_path.display()
        )),
    }
}

fn hook_fixture_payload(event: &str) -> &'static str {
    match event {
        "SessionStart" => r#"{"source":"startup"}"#,
        "UserPromptSubmit" => r#"{"prompt":"verify Lattice hook configuration"}"#,
        "PostToolUse" => r#"{"tool_name":"apply_patch","file_path":"README.md"}"#,
        "Stop" => r#"{"edited_files":["README.md"]}"#,
        _ => "{}",
    }
}

fn verify_hook_stdout(client: HookClient, event: &str, output: &str) -> Result<()> {
    if event == "Stop" {
        if !output.trim().is_empty() {
            return Err(anyhow!(
                "verification failed: Stop hook must not write stdout, got `{}`",
                output.trim()
            ));
        }
        return Ok(());
    }
    if event == "SessionStart" && output.trim().is_empty() {
        return Err(anyhow!(
            "verification failed: SessionStart hook produced no context"
        ));
    }
    if output.trim().is_empty() {
        return Ok(());
    }
    if client == HookClient::ClaudeCode {
        let envelope: Value = serde_json::from_str(output.trim())
            .context("verification failed: Claude Code hook emitted invalid JSON")?;
        let actual_event = envelope
            .pointer("/hookSpecificOutput/hookEventName")
            .and_then(Value::as_str);
        let context = envelope
            .pointer("/hookSpecificOutput/additionalContext")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty());
        if actual_event != Some(event) || context.is_none() {
            return Err(anyhow!(
                "verification failed: Claude Code {event} hook emitted an invalid context envelope"
            ));
        }
    }
    Ok(())
}

struct HookProcessContext<'a> {
    workspace: &'a Path,
    executable: &'a Path,
}

/// The verifier deliberately has a deadline: a broken hook must make install
/// fail, not leave the invoking agent blocked forever.
fn run_fixture_process(
    program: &Path,
    args: &[String],
    input: &str,
    hook_context: Option<HookProcessContext<'_>>,
    label: &str,
) -> Result<String> {
    let mut process = Command::new(program);
    process
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(context) = hook_context {
        process
            .current_dir(context.workspace)
            .env("LATTICE_BIN", context.executable)
            .env("LATTICE_SKIP_METRICS", "1");
    }
    let mut child = process.spawn().with_context(|| {
        format!(
            "verification failed: could not spawn {label} `{}`",
            program.display()
        )
    })?;
    {
        let stdin = child.stdin.as_mut().ok_or_else(|| {
            anyhow!(
                "verification failed: {label} `{}` has no stdin",
                program.display()
            )
        })?;
        stdin.write_all(input.as_bytes())?;
    }
    // Closing stdin lets stdio proxies terminate after processing the fixture.
    drop(child.stdin.take());

    let stdout = child.stdout.take().ok_or_else(|| {
        anyhow!(
            "verification failed: {label} `{}` has no stdout",
            program.display()
        )
    })?;
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut output = String::new();
        let result = std::io::BufReader::new(stdout)
            .read_to_string(&mut output)
            .map(|_| output);
        let _ = sender.send(result);
    });

    let deadline = Instant::now() + INSTALL_VERIFY_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait()? {
            let output = receiver
                .recv_timeout(Duration::from_millis(250))
                .map_err(|_| {
                    anyhow!("verification failed: {label} closed without readable stdout")
                })??;
            if !status.success() {
                let mut stderr = String::new();
                if let Some(mut stream) = child.stderr.take() {
                    let _ = stream.read_to_string(&mut stderr);
                }
                return Err(anyhow!(
                    "verification failed: {label} exited with {status}: {}",
                    stderr.trim()
                ));
            }
            return Ok(output);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow!(
                "verification failed: {label} exceeded {} seconds",
                INSTALL_VERIFY_TIMEOUT.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

async fn run_request(request: CliRequest) -> i32 {
    let timeout = request.timeout;
    match tokio::time::timeout(timeout, call_daemon(&request)).await {
        Ok(Ok(result)) => {
            let output = render_output(&result, request.json);
            if !output.trim().is_empty() {
                println!("{}", output.trim_end());
                0
            } else {
                1
            }
        }
        Ok(Err(CliError::DaemonUnavailable)) => {
            eprintln!("lattice daemon not running — start with: lattice --daemon");
            2
        }
        Ok(Err(CliError::DaemonConnection(error))) => {
            eprintln!("{}", daemon_connection_message(&error));
            2
        }
        Err(_) => {
            eprintln!(
                "lattice query timed out after {:.3}s — partial results unavailable",
                timeout.as_secs_f64()
            );
            3
        }
        Ok(Err(CliError::Rpc(message))) => {
            eprintln!("lattice daemon error: {}", message);
            1
        }
        Ok(Err(CliError::Other(error))) => {
            eprintln!("lattice: {}", error);
            1
        }
    }
}

async fn call_daemon(request: &CliRequest) -> Result<Value, CliError> {
    let mut stream = TcpStream::connect(daemon_addr())
        .await
        .map_err(CliError::DaemonConnection)?;
    let transport_request = ProxyRequest {
        workspace_roots: vec![request.workspace.to_string_lossy().to_string()],
        focus_files: Vec::new(),
        focus_dirs: Vec::new(),
    };
    transport::client_handshake(
        &mut stream,
        &daemon_addr(),
        ClientKind::Cli,
        &transport_request,
    )
    .await?;
    let mut arguments = request.arguments.clone();
    attach_invocation_metadata(&mut arguments);
    let rpc = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": request.tool,
            "arguments": arguments,
        }
    });
    write_json_line(&mut stream, &rpc).await?;

    let mut lines = BufReader::new(stream).lines();
    while let Some(line) = lines.next_line().await.map_err(anyhow::Error::from)? {
        if line.trim().is_empty() {
            continue;
        }
        let response: Value = serde_json::from_str(&line).map_err(anyhow::Error::from)?;
        if response.get("id").and_then(Value::as_i64) != Some(1) {
            continue;
        }
        if let Some(error) = response.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown JSON-RPC error")
                .to_string();
            return Err(CliError::Rpc(message));
        }
        return response
            .get("result")
            .cloned()
            .ok_or_else(|| CliError::Rpc("missing JSON-RPC result".to_string()));
    }
    Err(CliError::DaemonUnavailable)
}

fn daemon_connection_message(error: &std::io::Error) -> String {
    if error.kind() == std::io::ErrorKind::PermissionDenied {
        format!(
            "lattice cannot connect to {} because localhost access was denied: {}",
            daemon_addr(),
            error
        )
    } else if error.kind() == std::io::ErrorKind::ConnectionRefused {
        format!(
            "lattice daemon is not accepting connections at {} — start with: lattice --daemon ({})",
            daemon_addr(),
            error
        )
    } else {
        format!(
            "lattice could not connect to daemon at {}: {}",
            daemon_addr(),
            error
        )
    }
}

fn run_metrics_command(args: Vec<String>) -> i32 {
    match parse_metrics_args(args).and_then(|(workspace, days, json, memory)| {
        if memory {
            render_memory_metrics_for_workspace(&workspace, days, json)
        } else {
            render_metrics_for_workspace(&workspace, days, json)
        }
    }) {
        Ok(output) => {
            println!("{}", output.trim_end());
            0
        }
        Err(error) => {
            eprintln!("lattice: {}", error);
            1
        }
    }
}

fn parse_metrics_args(args: Vec<String>) -> Result<(PathBuf, usize, bool, bool)> {
    let mut workspace = None;
    let mut days = 14usize;
    let mut json = false;
    let mut memory = false;
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--json" => json = true,
            "--memory" => memory = true,
            "--workspace" | "-w" => {
                let value = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("--workspace requires a path"))?;
                workspace = Some(canonical_workspace(Path::new(value))?);
                i += 1;
            }
            "--days" => {
                let value = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("--days requires a positive integer"))?;
                days = value
                    .parse::<usize>()
                    .with_context(|| format!("invalid --days `{}`", value))?;
                if days == 0 {
                    return Err(anyhow!("--days must be positive"));
                }
                i += 1;
            }
            other => return Err(anyhow!("unknown metrics argument `{}`", other)),
        }
        i += 1;
    }
    Ok((
        workspace.unwrap_or(detect_workspace_root()?),
        days,
        json,
        memory,
    ))
}

/// Project the memory counters from the durable adoption ledger. This keeps
/// `--memory` read-only and uses the same attribution and retention behavior as
/// the regular metrics view.
fn render_memory_metrics_for_workspace(
    workspace: &Path,
    days: usize,
    json: bool,
) -> Result<String> {
    let visible_days = render_metrics_for_workspace(workspace, days, false)?
        .lines()
        .skip(2)
        .filter_map(|line| line.split('|').next().map(str::trim))
        .filter(|day| !day.is_empty() && !day.starts_with('_'))
        .map(ToString::to_string)
        .collect::<BTreeSet<_>>();
    let ledger_text = render_metrics_for_workspace(workspace, days, true)?;
    let ledger: Value =
        serde_json::from_str(&ledger_text).context("adoption metrics ledger is not valid JSON")?;
    let days_value = ledger
        .get("days")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("adoption metrics ledger has no days object"))?;
    let mut rows = Vec::new();
    for (day, clients) in days_value {
        if !visible_days.contains(day) {
            continue;
        }
        let Some(clients) = clients.as_object() else {
            continue;
        };
        for (client, channels) in clients {
            let Some(channels) = channels.as_object() else {
                continue;
            };
            for (channel, tools) in channels {
                let Some(counter) = tools.get("memory") else {
                    continue;
                };
                rows.push(json!({
                    "day": day,
                    "client": client,
                    "channel": channel,
                    "retrievals": counter.get("memory_retrievals").cloned().unwrap_or_else(|| json!(0)),
                    "memories_returned": counter.get("memory_retrieved_items").cloned().unwrap_or_else(|| json!(0)),
                    "memories_used": counter.get("memory_used_items").cloned().unwrap_or_else(|| json!(0)),
                    "injections": counter.get("memory_injections").cloned().unwrap_or_else(|| json!(0)),
                    "memories_shown": counter.get("memory_injected_items").cloned().unwrap_or_else(|| json!(0)),
                    "injection_actions": counter.get("memory_injection_actions").cloned().unwrap_or_else(|| json!(0)),
                }));
            }
        }
    }
    if json {
        return Ok(serde_json::to_string_pretty(&json!({ "days": rows }))?);
    }
    let mut out = String::from("day | client | channel | retrievals | memories_returned | memories_used | use_rate | injections | memories_shown | injection_actions | action_rate\n");
    out.push_str("--- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---:\n");
    if rows.is_empty() {
        out.push_str("_no memory metrics recorded_\n");
        return Ok(out);
    }
    for row in rows {
        let returned = row["memories_returned"].as_u64().unwrap_or(0);
        let used = row["memories_used"].as_u64().unwrap_or(0);
        let shown = row["memories_shown"].as_u64().unwrap_or(0);
        let actions = row["injection_actions"].as_u64().unwrap_or(0);
        let use_rate = if returned == 0 {
            0.0
        } else {
            used as f64 * 100.0 / returned as f64
        };
        let action_rate = if shown == 0 {
            0.0
        } else {
            actions as f64 * 100.0 / shown as f64
        };
        out.push_str(&format!(
            "{} | {} | {} | {} | {} | {} | {:.0}% | {} | {} | {} | {:.0}%\n",
            row["day"].as_str().unwrap_or("unknown"),
            row["client"].as_str().unwrap_or("unknown"),
            row["channel"].as_str().unwrap_or("unknown"),
            row["retrievals"],
            row["memories_returned"],
            row["memories_used"],
            use_rate,
            row["injections"],
            row["memories_shown"],
            row["injection_actions"],
            action_rate,
        ));
    }
    Ok(out)
}

fn attach_invocation_metadata(arguments: &mut Value) {
    if std::env::var("LATTICE_SKIP_METRICS").ok().as_deref() == Some("1") {
        return;
    }
    let client = std::env::var("LATTICE_CLIENT_NAME").unwrap_or_else(|_| "lattice-cli".to_string());
    let channel = std::env::var("LATTICE_CLIENT_CHANNEL").unwrap_or_else(|_| "cli".to_string());
    set_default(arguments, "_lattice_client", json!(client));
    set_default(arguments, "_lattice_channel", json!(channel));
}

async fn write_json_line(stream: &mut TcpStream, value: &Value) -> Result<(), CliError> {
    let mut text = serde_json::to_string(value).map_err(anyhow::Error::from)?;
    text.push('\n');
    stream
        .write_all(text.as_bytes())
        .await
        .map_err(anyhow::Error::from)?;
    stream.flush().await.map_err(anyhow::Error::from)?;
    Ok(())
}

fn parse_args(args: Vec<String>) -> Result<CliRequest> {
    let mut parser = ArgParser::new(args)?;
    let command = parser.command.clone();
    let mut request = match command.as_str() {
        "context" => parse_context(&mut parser)?,
        "prepare_change" => parse_prepare_change(&mut parser)?,
        "impact" => parse_impact(&mut parser)?,
        "search" => parse_search(&mut parser)?,
        "diagnose" => parse_diagnose(&mut parser)?,
        "remember" => parse_remember(&mut parser)?,
        "recall" => parse_recall(&mut parser)?,
        "status" => parse_status(&mut parser)?,
        other => return Err(anyhow!("unknown lattice CLI command `{}`", other)),
    };
    parser.reject_unknown_flags()?;
    parser.apply_common(&mut request)?;
    if !request.json {
        set_default(&mut request.arguments, "render", json!("markdown"));
        set_default(&mut request.arguments, "render_mode", json!("compact"));
        set_default(&mut request.arguments, "budget", json!("compact"));
    }
    Ok(request)
}

fn parse_context(parser: &mut ArgParser) -> Result<CliRequest> {
    let mode = parser
        .take_flag_value("--mode")?
        .unwrap_or_else(|| "subsystem".to_string());
    let files = parser.take_repeated("--files")?;
    let min_relevance = parser.take_flag_value("--min-relevance")?;
    let query = parser.join_positionals();
    if query.trim().is_empty() {
        return Err(anyhow!("context requires a query"));
    }
    let mut arguments = json!({ "query": query });
    set_value(&mut arguments, "mode", json!(mode));
    if let Some(min_relevance) = min_relevance {
        let score = min_relevance
            .parse::<f64>()
            .with_context(|| format!("invalid --min-relevance `{}`", min_relevance))?;
        if !(score.is_finite() && (0.0..=1.0).contains(&score)) {
            return Err(anyhow!("--min-relevance must be a number from 0.0 to 1.0"));
        }
        set_value(&mut arguments, "min_relevance", json!(score));
    }
    if !files.is_empty() {
        set_value(&mut arguments, "files", json!(files));
    }
    Ok(parser.request("context", arguments))
}

fn parse_prepare_change(parser: &mut ArgParser) -> Result<CliRequest> {
    let mode = parser
        .take_flag_value("--mode")?
        .unwrap_or_else(|| "prepare".to_string());
    let task = parser.join_positionals();
    if task.trim().is_empty() {
        return Err(anyhow!("prepare_change requires a task"));
    }
    Ok(parser.request(
        "prepare_change",
        json!({
            "task": task,
            "mode": mode,
        }),
    ))
}

fn parse_impact(parser: &mut ArgParser) -> Result<CliRequest> {
    let include_tests = !parser.take_bool("--no-tests");
    let direction = parser.take_flag_value("--direction")?;
    let diff = parser.take_bool("--diff");
    let mut arguments = json!({ "include_tests": include_tests });
    if let Some(direction) = direction {
        set_value(&mut arguments, "direction", json!(direction));
    }
    if diff {
        set_value(&mut arguments, "diff", json!(read_stdin()?));
    } else {
        let target = parser.join_positionals();
        if target.trim().is_empty() {
            return Err(anyhow!("impact requires a symbol, path, or --diff"));
        }
        if looks_like_path_target(&target) {
            set_value(&mut arguments, "file", json!(target));
        } else {
            set_value(&mut arguments, "target", json!(target));
        }
    }
    Ok(parser.request("impact", arguments))
}

fn looks_like_path_target(target: &str) -> bool {
    target.contains('/')
        || target.contains('\\')
        || Path::new(target).exists()
        || target.rsplit_once('.').is_some_and(|(_, ext)| {
            ext.chars().all(|ch| ch.is_ascii_alphanumeric()) && (1..=8).contains(&ext.len())
        })
}

fn parse_search(parser: &mut ArgParser) -> Result<CliRequest> {
    let kind = parser
        .take_flag_value("--kind")?
        .unwrap_or_else(|| "symbol".to_string());
    let direction = parser.take_flag_value("--direction")?;
    let from = parser.take_flag_value("--from")?;
    let to = parser.take_flag_value("--to")?;
    let mut arguments = json!({ "kind": kind });
    if let Some(direction) = direction {
        set_value(&mut arguments, "direction", json!(direction));
    }
    if let Some(from) = from {
        set_value(&mut arguments, "from", json!(from));
    }
    if let Some(to) = to {
        set_value(&mut arguments, "to", json!(to));
    }
    let query = parser.join_positionals();
    if !query.trim().is_empty() {
        set_value(&mut arguments, "query", json!(query));
    } else if arguments.get("from").is_none() && arguments.get("to").is_none() {
        return Err(anyhow!("search requires a query"));
    }
    Ok(parser.request("search", arguments))
}

fn parse_diagnose(parser: &mut ArgParser) -> Result<CliRequest> {
    let text = if parser.positionals.is_empty()
        || parser.positionals.iter().any(|value| value.as_str() == "-")
    {
        read_stdin()?
    } else {
        parser.join_positionals()
    };
    if text.trim().is_empty() {
        return Err(anyhow!("diagnose requires failure text or '-' for stdin"));
    }
    Ok(parser.request("diagnose", json!({ "failure_text": text })))
}

fn parse_remember(parser: &mut ArgParser) -> Result<CliRequest> {
    let kind = parser
        .take_flag_value("--kind")?
        .unwrap_or_else(|| "quick".to_string());
    let content = parser.join_positionals();
    if content.trim().is_empty() {
        return Err(anyhow!("remember requires content"));
    }
    let mut arguments = json!({
        "kind": kind,
        "content": content,
    });
    if arguments.get("kind").and_then(Value::as_str) == Some("outcome") {
        set_default(&mut arguments, "task", json!(content));
        set_default(&mut arguments, "summary", json!(content));
    }
    Ok(parser.request("remember", arguments))
}

fn parse_recall(parser: &mut ArgParser) -> Result<CliRequest> {
    let mode = parser
        .take_flag_value("--mode")?
        .unwrap_or_else(|| "search".to_string());
    let task_id = parser.take_flag_value("--task-id")?;
    let focus_files = parser.take_repeated("--focus-files")?;
    let query = parser.join_positionals();
    if query.trim().is_empty() {
        return Err(anyhow!("recall requires a query"));
    }
    let mut arguments = json!({
        "mode": mode,
        "query": query,
    });
    if arguments.get("mode").and_then(Value::as_str) == Some("task") {
        let id = task_id.unwrap_or_else(|| query.clone());
        set_value(&mut arguments, "task_id", json!(id));
        set_value(&mut arguments, "task_statement", json!(query));
    }
    if !focus_files.is_empty() {
        set_value(&mut arguments, "focus_files", json!(focus_files));
    }
    Ok(parser.request("recall", arguments))
}

fn parse_status(parser: &mut ArgParser) -> Result<CliRequest> {
    let scope = parser
        .take_flag_value("--scope")?
        .unwrap_or_else(|| "index".to_string());
    let files = parser.take_repeated("--files")?;
    let query = parser.join_positionals();
    let mut arguments = json!({ "scope": scope });
    if !query.trim().is_empty() {
        set_value(&mut arguments, "query", json!(query));
    }
    if !files.is_empty() {
        set_value(&mut arguments, "files", json!(files));
    }
    Ok(parser.request("status", arguments))
}

#[derive(Debug)]
struct ArgParser {
    command: String,
    positionals: Vec<String>,
    json: bool,
    timeout: Duration,
    workspace: Option<PathBuf>,
}

impl ArgParser {
    fn new(args: Vec<String>) -> Result<Self> {
        let command = args
            .get(1)
            .cloned()
            .ok_or_else(|| anyhow!("missing lattice CLI command"))?;
        let mut parser = Self {
            command,
            positionals: Vec::new(),
            json: false,
            timeout: DEFAULT_TIMEOUT,
            workspace: None,
        };
        let mut i = 2;
        while i < args.len() {
            match args[i].as_str() {
                "--json" => parser.json = true,
                "--timeout" => {
                    let value = args
                        .get(i + 1)
                        .ok_or_else(|| anyhow!("--timeout requires seconds"))?;
                    parser.timeout = parse_timeout(value)?;
                    i += 1;
                }
                "--workspace" | "-w" => {
                    let value = args
                        .get(i + 1)
                        .ok_or_else(|| anyhow!("--workspace requires a path"))?;
                    parser.workspace = Some(canonical_workspace(Path::new(value))?);
                    i += 1;
                }
                value => parser.positionals.push(value.to_string()),
            }
            i += 1;
        }
        Ok(parser)
    }

    fn take_bool(&mut self, flag: &str) -> bool {
        let before = self.positionals.len();
        self.positionals.retain(|value| value != flag);
        before != self.positionals.len()
    }

    fn take_flag_value(&mut self, flag: &str) -> Result<Option<String>> {
        let mut next = Vec::with_capacity(self.positionals.len());
        let mut value = None;
        let mut i = 0;
        while i < self.positionals.len() {
            if self.positionals[i] == flag {
                let item = self
                    .positionals
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("{} requires a value", flag))?
                    .clone();
                value = Some(item);
                i += 2;
            } else {
                next.push(self.positionals[i].clone());
                i += 1;
            }
        }
        self.positionals = next;
        Ok(value)
    }

    fn take_repeated(&mut self, flag: &str) -> Result<Vec<String>> {
        let mut next = Vec::with_capacity(self.positionals.len());
        let mut values = Vec::new();
        let mut i = 0;
        while i < self.positionals.len() {
            if self.positionals[i] == flag {
                let item = self
                    .positionals
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("{} requires a value", flag))?
                    .clone();
                values.push(item);
                i += 2;
            } else {
                next.push(self.positionals[i].clone());
                i += 1;
            }
        }
        self.positionals = next;
        Ok(values)
    }

    fn join_positionals(&self) -> String {
        self.positionals
            .iter()
            .filter(|value| !value.starts_with("--"))
            .cloned()
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn reject_unknown_flags(&self) -> Result<()> {
        if let Some(flag) = self
            .positionals
            .iter()
            .find(|value| value.starts_with("--"))
        {
            return Err(anyhow!("unknown argument `{}`", flag));
        }
        Ok(())
    }

    fn apply_common(&self, request: &mut CliRequest) -> Result<()> {
        request.json = self.json;
        request.timeout = self.timeout;
        request.workspace = match &self.workspace {
            Some(path) => path.clone(),
            None => detect_workspace_root()?,
        };
        Ok(())
    }

    fn request(&self, tool: &str, arguments: Value) -> CliRequest {
        CliRequest {
            tool: tool.to_string(),
            arguments,
            workspace: PathBuf::new(),
            json: self.json,
            timeout: self.timeout,
        }
    }
}

fn parse_timeout(value: &str) -> Result<Duration> {
    let seconds = value
        .parse::<f64>()
        .with_context(|| format!("invalid --timeout `{}`", value))?;
    if !(seconds.is_finite() && seconds > 0.0) {
        return Err(anyhow!("--timeout must be a positive number of seconds"));
    }
    Ok(Duration::from_secs_f64(seconds))
}

fn canonical_workspace(path: &Path) -> Result<PathBuf> {
    if !path.is_dir() {
        return Err(anyhow!("workspace `{}` is not a directory", path.display()));
    }
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    validate_cli_workspace(&canonical)?;
    Ok(canonical)
}

fn detect_workspace_root() -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    for ancestor in cwd.ancestors() {
        if ancestor.join(".mcp.json").is_file()
            || ancestor.join(".lattice").is_dir()
            || ancestor.join(".git").exists()
        {
            return canonical_workspace(ancestor);
        }
    }
    for ancestor in cwd.ancestors() {
        if ancestor.join("Cargo.toml").is_file()
            || ancestor.join("package.json").is_file()
            || ancestor.join("pyproject.toml").is_file()
        {
            return canonical_workspace(ancestor);
        }
    }
    canonical_workspace(&cwd)
}

fn validate_cli_workspace(path: &Path) -> Result<()> {
    if path.parent().is_none() {
        return Err(anyhow!(
            "refusing workspace root `{}`: filesystem root is not a valid Lattice workspace",
            path.display()
        ));
    }
    if let Some(home) = std::env::var_os("HOME")
        .map(PathBuf::from)
        .and_then(|home| home.canonicalize().ok())
    {
        if path == home {
            return Err(anyhow!(
                "refusing workspace root `{}`: the user's home directory is not a valid Lattice workspace; choose a project directory instead",
                path.display()
            ));
        }
    }
    Ok(())
}

fn read_stdin() -> Result<String> {
    let mut text = String::new();
    std::io::stdin()
        .read_to_string(&mut text)
        .context("failed to read stdin")?;
    Ok(text)
}

fn set_value(value: &mut Value, key: &str, item: Value) {
    if let Some(object) = value.as_object_mut() {
        object.insert(key.to_string(), item);
    }
}

fn set_default(value: &mut Value, key: &str, item: Value) {
    if value.get(key).is_none() {
        set_value(value, key, item);
    }
}

fn render_output(result: &Value, json_output: bool) -> String {
    if json_output {
        return serde_json::to_string_pretty(result).unwrap_or_else(|_| result.to_string());
    }
    let Some(text) = first_text_block(result) else {
        return render_value_markdown(result);
    };
    if let Ok(value) = serde_json::from_str::<Value>(&text) {
        if let Some(markdown) = value.get("markdown").and_then(Value::as_str) {
            return markdown.to_string();
        }
        return render_value_markdown(&value);
    }
    text
}

fn first_text_block(value: &Value) -> Option<String> {
    value
        .get("content")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .and_then(|item| item.get("text"))
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

fn render_value_markdown(value: &Value) -> String {
    match value {
        Value::Object(object) => {
            let title = object
                .get("tool")
                .and_then(Value::as_str)
                .or_else(|| object.get("status").and_then(Value::as_str))
                .unwrap_or("Result");
            let mut out = format!("### {}\n", title);
            for (key, item) in object.iter().take(24) {
                out.push_str(&format!("- `{}`: {}\n", key, compact_value(item)));
            }
            out
        }
        Value::Array(items) => {
            let mut out = format!("### Results\n- `count`: {}\n", items.len());
            for item in items.iter().take(10) {
                out.push_str(&format!("- {}\n", compact_value(item)));
            }
            out
        }
        _ => compact_value(value),
    }
}

fn compact_value(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::String(value) => value.clone(),
        Value::Array(values) => format!("[{} item(s)]", values.len()),
        Value::Object(object) => {
            if let Some(summary) = object.get("summary").and_then(Value::as_str) {
                summary.to_string()
            } else if let Some(name) = object.get("name").and_then(Value::as_str) {
                name.to_string()
            } else if let Some(file) = object.get("file").and_then(Value::as_str) {
                file.to_string()
            } else {
                format!("{{{} field(s)}}", object.len())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static INSTALL_FIXTURE_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn install_fixture() -> (PathBuf, PathBuf, InstallRuntime) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sequence = INSTALL_FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("lattice-cli-install-{nonce}-{sequence}"));
        let workspace = root.join("workspace");
        fs::create_dir_all(workspace.join(".git")).unwrap();
        for client in ["claude-code", "codex"] {
            let hooks = root.join("integrations").join(client).join("hooks");
            fs::create_dir_all(&hooks).unwrap();
            for (event, script) in INSTALLED_HOOKS {
                let output = if event == "Stop" {
                    ""
                } else if client == "claude-code" {
                    &format!(
                        "{{\"hookSpecificOutput\":{{\"hookEventName\":\"{event}\",\"additionalContext\":\"fixture\"}}}}"
                    )
                } else {
                    "fixture context"
                };
                write_executable(
                    &hooks.join(script),
                    &format!("#!/bin/sh\nprintf '%s\\n' '{output}'\n"),
                );
            }
        }
        let executable = root.join("bin/lattice");
        write_executable(
            &executable,
            "#!/bin/sh\nprintf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}'\nprintf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"tools\":[{}, {}, {}, {}, {}, {}, {}, {}]}}'\n",
        );
        let runtime = InstallRuntime {
            executable,
            asset_root: root.clone(),
        };
        (root, workspace, runtime)
    }

    #[cfg(unix)]
    fn write_executable(path: &Path, contents: &str) {
        use std::os::unix::fs::PermissionsExt;

        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }

    #[test]
    fn permission_denied_connection_error_is_not_reported_as_daemon_absence() {
        let error = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "blocked by sandbox");
        let message = daemon_connection_message(&error);

        assert!(message.contains("localhost access was denied"));
        assert!(message.contains("blocked by sandbox"));
        assert!(!message.contains("daemon not running"));
    }

    #[test]
    fn parses_memory_metrics_view_flag() {
        let workspace = std::env::current_dir().unwrap();
        let (parsed_workspace, days, json, memory) = parse_metrics_args(vec![
            "lattice".into(),
            "metrics".into(),
            "--memory".into(),
            "--days".into(),
            "7".into(),
            "--json".into(),
            "--workspace".into(),
            workspace.to_string_lossy().into_owned(),
        ])
        .unwrap();
        assert_eq!(parsed_workspace, workspace.canonicalize().unwrap());
        assert_eq!(days, 7);
        assert!(json);
        assert!(memory);
    }

    #[test]
    fn memory_metrics_view_projects_retrieval_and_injection_counters() {
        let root = std::env::temp_dir().join(format!(
            "lattice-memory-metrics-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join(".lattice")).unwrap();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let events = [
            json!({"kind":"memory_retrieval","timestamp_secs":now,"session_id":"s","client":"codex","channel":"cli","retrieval_id":"r","retrieved_count":3}),
            json!({"kind":"memory_use","timestamp_secs":now,"retrieval_id":"r","used_count":2}),
            json!({"kind":"memory_injection","timestamp_secs":now,"session_id":"s","client":"codex","channel":"cli","injection_id":"i","shown_count":2}),
            json!({"kind":"memory_injection_action","timestamp_secs":now,"injection_id":"i","acted_count":1}),
        ];
        let text = events
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        fs::write(root.join(".lattice/adoption_metrics.jsonl"), text).unwrap();

        let markdown = render_memory_metrics_for_workspace(&root, 14, false).unwrap();
        assert!(markdown.contains("retrievals"));
        assert!(
            markdown.contains("| 1 | 3 | 2 | 67% | 1 | 2 | 1 | 50%"),
            "{markdown}"
        );
        let json_output = render_memory_metrics_for_workspace(&root, 14, true).unwrap();
        let value: Value = serde_json::from_str(&json_output).unwrap();
        assert_eq!(value["days"].as_array().unwrap()[0]["retrievals"], 1);
        assert_eq!(value["days"].as_array().unwrap()[0]["injection_actions"], 1);
    }

    #[test]
    fn parses_context_command_with_common_flags() {
        let request = parse_args(vec![
            "lattice".into(),
            "context".into(),
            "auth flow".into(),
            "--mode".into(),
            "docs".into(),
            "--files".into(),
            "README.md".into(),
            "--min-relevance".into(),
            "0.35".into(),
            "--json".into(),
            "--timeout".into(),
            "1.5".into(),
            "--workspace".into(),
            std::env::current_dir()
                .unwrap()
                .to_string_lossy()
                .to_string(),
        ])
        .expect("parse succeeds");
        assert_eq!(request.tool, "context");
        assert_eq!(request.arguments["query"], "auth flow");
        assert_eq!(request.arguments["mode"], "docs");
        assert_eq!(request.arguments["min_relevance"], 0.35);
        assert!(request.json);
        assert_eq!(request.timeout, Duration::from_millis(1500));
    }

    #[test]
    fn parses_impact_path_as_file_argument() {
        let request = parse_args(vec![
            "lattice".into(),
            "impact".into(),
            "daemon/crates/lattice-daemon/src/cli.rs".into(),
            "--no-tests".into(),
        ])
        .expect("parse succeeds");
        assert_eq!(request.tool, "impact");
        assert_eq!(
            request.arguments["file"],
            "daemon/crates/lattice-daemon/src/cli.rs"
        );
        assert!(request.arguments["target"].is_null());
        assert_eq!(request.arguments["include_tests"], false);
    }

    #[test]
    fn renders_wrapped_markdown_text_directly() {
        let result = json!({
            "content": [{
                "type": "text",
                "text": "### Summary\n- ok"
            }]
        });
        assert_eq!(render_output(&result, false), "### Summary\n- ok");
    }

    #[test]
    fn install_mcp_reconciles_config_idempotently_and_verifies_disk_round_trip() {
        let (root, workspace, runtime) = install_fixture();
        fs::write(
            workspace.join(".mcp.json"),
            r#"{"mcpServers":{"other":{"command":"other"},"lattice":{"command":"stale"}}}"#,
        )
        .unwrap();
        let command = InstallCommand {
            target: InstallTarget::Mcp,
            workspaces: vec![workspace.clone()],
            verify: true,
        };

        run_install_command_with(command.clone(), &runtime).unwrap();
        let once = fs::read_to_string(workspace.join(".mcp.json")).unwrap();
        run_install_command_with(command, &runtime).unwrap();
        let twice = fs::read_to_string(workspace.join(".mcp.json")).unwrap();

        assert_eq!(once, twice);
        let config: Value = serde_json::from_str(&twice).unwrap();
        assert_eq!(config["mcpServers"]["other"]["command"], "other");
        assert_eq!(
            config["mcpServers"]["lattice"]["command"],
            runtime.executable.to_string_lossy().as_ref()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn install_hook_targets_preserve_foreign_entries_and_write_stable_assets() {
        let (root, workspace, runtime) = install_fixture();
        fs::create_dir_all(workspace.join(".codex")).unwrap();
        fs::write(
            workspace.join(".codex/hooks.json"),
            r#"{"hooks":{"Stop":[{"hooks":[{"command":"custom-hook"}]}]}}"#,
        )
        .unwrap();
        let command = InstallCommand {
            target: InstallTarget::Codex,
            workspaces: vec![workspace.clone()],
            verify: true,
        };

        run_install_command_with(command.clone(), &runtime).unwrap();
        let once = fs::read_to_string(workspace.join(".codex/hooks.json")).unwrap();
        run_install_command_with(command, &runtime).unwrap();
        let twice = fs::read_to_string(workspace.join(".codex/hooks.json")).unwrap();

        assert_eq!(once, twice);
        let config: Value = serde_json::from_str(&twice).unwrap();
        let stop_hooks = config["hooks"]["Stop"].as_array().unwrap();
        assert!(stop_hooks.iter().any(|entry| {
            entry["hooks"]
                .as_array()
                .is_some_and(|hooks| hooks.iter().any(|hook| hook["command"] == "custom-hook"))
        }));
        assert!(twice.contains(
            &root
                .join("integrations/codex/hooks/stop.sh")
                .display()
                .to_string()
        ));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn install_verify_fails_loudly_for_a_broken_configured_mcp_binary() {
        let (root, _workspace, runtime) = install_fixture();
        write_executable(&runtime.executable, "#!/bin/sh\nexit 23\n");
        let config = json!({
            "mcpServers": {
                "lattice": {
                    "type": "stdio",
                    "command": runtime.executable,
                    "args": ["--stdio"]
                }
            }
        });

        let error = verify_configured_mcp_server(&config, Path::new("/fixture/.mcp.json"))
            .unwrap_err()
            .to_string();

        assert!(error.contains("configured MCP command exited"), "{error}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn install_verify_rejects_tool_surface_drift() {
        let (root, _workspace, runtime) = install_fixture();
        write_executable(
            &runtime.executable,
            "#!/bin/sh\nprintf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}'\nprintf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"tools\":[{}]}}'\n",
        );
        let config = json!({
            "mcpServers": {
                "lattice": {
                    "type": "stdio",
                    "command": runtime.executable,
                    "args": ["--stdio"]
                }
            }
        });

        let error = verify_configured_mcp_server(&config, Path::new("/fixture/.mcp.json"))
            .unwrap_err()
            .to_string();

        assert!(error.contains("returned 1 tools, expected 8"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn install_verify_drains_mcp_stdout_after_a_clean_child_exit() {
        let (root, _workspace, runtime) = install_fixture();
        let responses = run_mcp_verification_process(
            &runtime.executable,
            &["--stdio".to_string()],
            &mcp_verification_payload(),
            "fast-exiting MCP fixture",
        )
        .unwrap();

        assert!(responses.iter().any(|response| response["id"] == 1));
        assert!(responses.iter().any(|response| response["id"] == 2));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn install_verify_keeps_mcp_stdin_open_until_responses_arrive() {
        let (root, _workspace, runtime) = install_fixture();
        write_executable(
            &runtime.executable,
            "#!/bin/sh\n\
             IFS= read -r _ || exit 1\n\
             IFS= read -r _ || exit 1\n\
             IFS= read -r _ || exit 1\n\
             printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}'\n\
             printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"tools\":[{}, {}, {}, {}, {}, {}, {}, {}]}}'\n\
             while IFS= read -r _; do :; done\n",
        );
        let config = json!({
            "mcpServers": {
                "lattice": {
                    "type": "stdio",
                    "command": runtime.executable,
                    "args": ["--stdio"]
                }
            }
        });

        verify_configured_mcp_server(&config, Path::new("/fixture/.mcp.json")).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn install_parser_rejects_unknown_targets_and_multiple_hook_workspaces() {
        let error = InstallTarget::parse("wrong").unwrap_err().to_string();
        assert!(error.contains("unknown install target"));

        let (root, workspace, _) = install_fixture();
        let second = root.join("second");
        fs::create_dir_all(second.join(".git")).unwrap();
        let error = parse_install_command(vec![
            "lattice".into(),
            "install".into(),
            "codex".into(),
            "--workspace".into(),
            workspace.display().to_string(),
            "--workspace".into(),
            second.display().to_string(),
        ])
        .unwrap_err()
        .to_string();
        assert!(error.contains("exactly one --workspace"));
        fs::remove_dir_all(root).unwrap();
    }
}
