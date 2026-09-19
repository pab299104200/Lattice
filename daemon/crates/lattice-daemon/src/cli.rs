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

use crate::adoption_metrics::{
    render_metrics_for_workspace, AdoptionMetricsStore, MemoryInjectionActionRecord,
    MemoryInjectionRecord, MemoryRetrievalRecord, MemoryUseRecord,
};
use crate::hook_enforcement::{
    load_policy, notice_text, policy_path, write_policy, NoticeCondition, PolicyState,
    BEST_EFFORT_DAEMON_NOTICE,
};
use crate::install::{
    installed_hooks, reconcile_hook_config, reconcile_mcp_config, render_config, HookClient,
    HookMode, InstallPaths,
};
use crate::proxy::daemon_addr;
use crate::transport::{self, ClientKind, ProxyRequest};
use lattice_core::embeddings::{install_shared_embedding_model, EmbeddingModelInstallStatus};
use lattice_core::health::backtest::replay::{
    replay_repository_streaming, repository_name, ReplayLimits,
};
use lattice_core::health::backtest::report::ReportBuilder;
use lattice_core::metrics::health_backtest;
use lattice_core::metrics::{
    report_exit_code, MetricScope, RegressionReport, ReportInput, SuccessCriteriaThresholds,
};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);
const INSTALL_VERIFY_TIMEOUT: Duration = Duration::from_secs(10);
const EXPECTED_MCP_TOOL_COUNT: usize = 8;

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
            | Some("health-backtest")
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
    "Usage: lattice <command> [options]\n\nCommands:\n  context\n  prepare_change\n  impact\n  search\n  diagnose\n  remember\n  recall\n  status\n  metrics [--memory|--health]\n  health-backtest [--json] [--regression] [--output <path>] [--repo <path>]…\n  install [all|mcp|claude-code|codex] [--workspace <path>] [--enforce|--no-enforce] [--verify] [--with-embeddings]\n  doctor\n  memory-migrate\n  storage status|cache plan|cache apply|backup|restore|relocate|historical plan|historical apply\n\nRuntime modes (explicit only):\n  --daemon\n  --stdio"
}

pub(crate) async fn run_from_env() -> i32 {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("metrics") {
        return run_metrics_command(args);
    }
    if args.get(1).map(String::as_str) == Some("health-backtest") {
        return run_health_backtest_command(args);
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
    Project,
    Mcp,
    ClaudeCode,
    Codex,
}

impl InstallTarget {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "all" => Ok(Self::Project),
            "mcp" => Ok(Self::Mcp),
            "claude-code" => Ok(Self::ClaudeCode),
            "codex" => Ok(Self::Codex),
            other => Err(anyhow!(
                "unknown install target `{other}`; expected one of: all, mcp, claude-code, codex"
            )),
        }
    }

    fn config_path(self, workspace: &Path) -> PathBuf {
        match self {
            Self::Project | Self::Mcp => workspace.join(".mcp.json"),
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
    with_embeddings: bool,
    /// `Some` records the mode in the workspace policy. `None` keeps whatever
    /// the policy already says, so a plain rerun never changes the mode.
    enforce: Option<bool>,
}

#[derive(Debug, Clone)]
struct InstallRuntime {
    executable: PathBuf,
    asset_root: PathBuf,
}

fn run_install_command(args: Vec<String>) -> i32 {
    let result = (|| {
        if is_embeddings_only_install(&args) {
            return install_embeddings_message();
        }
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

fn is_embeddings_only_install(args: &[String]) -> bool {
    args.len() == 3 && args.get(2).map(String::as_str) == Some("--with-embeddings")
}

fn install_embeddings_message() -> Result<String> {
    let status = install_shared_embedding_model()?;
    Ok(match status {
        EmbeddingModelInstallStatus::Installed(path) => format!(
            "installed checksum-verified shared embedding model at {}",
            path.display()
        ),
        EmbeddingModelInstallStatus::AlreadyInstalled(path) => format!(
            "shared checksum-verified embedding model is already installed at {}",
            path.display()
        ),
    })
}

fn parse_install_command(args: Vec<String>) -> Result<InstallCommand> {
    let explicit_target = args.get(2).filter(|value| !value.starts_with('-'));
    let target = explicit_target
        .map(|target| InstallTarget::parse(target))
        .transpose()?
        .unwrap_or(InstallTarget::Project);
    let mut workspaces = Vec::new();
    let mut verify = false;
    let mut with_embeddings = false;
    let mut enforce = None;
    let mut index = if explicit_target.is_some() { 3 } else { 2 };
    while index < args.len() {
        match args[index].as_str() {
            "--verify" => verify = true,
            "--with-embeddings" => with_embeddings = true,
            flag @ ("--enforce" | "--no-enforce") => {
                let requested = flag == "--enforce";
                if enforce.is_some_and(|previous| previous != requested) {
                    return Err(anyhow!("--enforce and --no-enforce cannot be combined"));
                }
                enforce = Some(requested);
            }
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
            "project and hook installation accepts exactly one --workspace; received {}",
            workspaces.len()
        ));
    }
    if enforce.is_some() && target == InstallTarget::Mcp {
        return Err(anyhow!(
            "--enforce and --no-enforce apply to hook installation; use `install`, `install claude-code` or `install codex`"
        ));
    }
    Ok(InstallCommand {
        target,
        workspaces,
        verify,
        with_embeddings,
        enforce,
    })
}

/// The hook mode this install must produce. Enforcement is a property of the
/// workspace, recorded in its policy file, and both clients' hooks follow it.
/// A policy that cannot be read is an error: guessing either way would
/// install hooks that disagree with what the operator recorded.
fn resolve_hook_mode(command: &InstallCommand, workspace: &Path) -> Result<HookMode> {
    if let Some(requested) = command.enforce {
        return Ok(HookMode::from_enforcing(requested));
    }
    match load_policy(workspace) {
        PolicyState::Absent => Ok(HookMode::BestEffort),
        PolicyState::Loaded(policy) => Ok(HookMode::from_enforcing(policy.hook_enforcement)),
        PolicyState::Unreadable(reason) => Err(anyhow!(
            "workspace policy `{}` is unreadable ({reason}); repair or delete it, then rerun",
            policy_path(workspace).display()
        )),
    }
}

/// Persist an explicitly requested mode. Runs before any hook configuration
/// is written: a policy that cannot be written must not leave hooks behind
/// that claim a mode the workspace never recorded.
fn record_requested_mode(command: &InstallCommand, workspace: &Path) -> Result<Option<String>> {
    let Some(requested) = command.enforce else {
        return Ok(None);
    };
    let path = write_policy(workspace, requested)?;
    Ok(Some(format!(
        "hook enforcement {} in {}",
        if requested { "enabled" } else { "disabled" },
        path.display()
    )))
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
    let embedding_message = if command.with_embeddings {
        Some(install_embeddings_message()?)
    } else {
        None
    };
    let config_workspace = command
        .workspaces
        .first()
        .expect("install parser guarantees a workspace");
    let mode_message = if command.target == InstallTarget::Mcp {
        None
    } else {
        record_requested_mode(&command, config_workspace)?
    };
    if command.target == InstallTarget::Project {
        let paths = InstallPaths::new(runtime.executable.clone(), runtime.asset_root.clone())?;
        let mode = resolve_hook_mode(&command, config_workspace)?;
        let installed = crate::install_project::install_project(config_workspace, &paths, mode)?;
        if command.verify {
            for target in [
                InstallTarget::Mcp,
                InstallTarget::Codex,
                InstallTarget::ClaudeCode,
            ] {
                let verification = InstallCommand {
                    target,
                    ..command.clone()
                };
                verify_install_config(
                    &target.config_path(config_workspace),
                    &verification,
                    runtime,
                )?;
            }
        }
        let mut message = format!(
            "installed Lattice project integration at {}",
            config_workspace.display()
        );
        for path in installed {
            message.push_str(&format!("\n  {}", path.display()));
        }
        if let Some(mode_message) = &mode_message {
            message.push_str(&format!("\n  {mode_message}"));
        }
        message.push_str("\nRestart/reconnect agent clients to load the configuration. Codex project configuration requires a trusted project; client trust and approval settings are unchanged.");
        if let Some(embedding_message) = embedding_message {
            message.push_str(&format!("\n{embedding_message}"));
        }
        return Ok(message);
    }
    let config_path = command.target.config_path(config_workspace);
    let mut config = read_install_config(&config_path)?;

    match command.target {
        InstallTarget::Project | InstallTarget::Mcp => {
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
            let mode = resolve_hook_mode(&command, config_workspace)?;
            reconcile_hook_config(&mut config, client, &paths, mode)?;
        }
    }

    write_install_config(&config_path, &config)?;
    if command.verify {
        verify_install_config(&config_path, &command, runtime)?;
    }
    let installed = format!(
        "installed Lattice {} configuration at {}",
        match command.target {
            InstallTarget::Project | InstallTarget::Mcp => "MCP",
            InstallTarget::ClaudeCode => "Claude Code hook",
            InstallTarget::Codex => "Codex hook",
        },
        config_path.display()
    );
    let installed = match mode_message {
        Some(mode_message) => format!("{installed}\n{mode_message}"),
        None => installed,
    };
    Ok(match embedding_message {
        Some(message) => format!("{installed}\n{message}"),
        None => installed,
    })
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
        "common.sh",
        "session-start.sh",
        "user-prompt-submit.sh",
        "pre-tool-use.sh",
        "post-tool-use.sh",
        "stop.sh",
        "session-end.sh",
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
        InstallTarget::Project | InstallTarget::Mcp => {
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
            let mode = resolve_hook_mode(command, config_workspace(command))?;
            reconcile_hook_config(&mut actual, client, &paths, mode)?;
        }
    }
    if before != render_config(&actual)? {
        return Err(anyhow!(
            "verification failed: `{}` is not a canonical Lattice installation config",
            path.display()
        ));
    }

    match command.target {
        InstallTarget::Project | InstallTarget::Mcp => verify_configured_mcp_server(&actual, path)?,
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
    // A command that dies at once closes its stdin before this write. That is
    // not the failure to report: carry on, so the read loop below can report
    // its exit status and stderr, which say why it died.
    match stdin
        .write_all(input.as_bytes())
        .and_then(|()| stdin.flush())
    {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => {}
        Err(error) => return Err(error.into()),
    }

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
                // End of stdout is seen a moment before the exit status can
                // be collected. Asking once loses that race under load and
                // reports "closed stdout" with no status and no stderr.
                if let Some(status) = wait_briefly_for_exit(&mut child, deadline)? {
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

/// Run every configured hook with a representative client payload. Protected
/// capture adapters are best-effort, but every installed wrapper must execute
/// successfully, must say only what its mode allows, and must not leak
/// host-envelope content.
fn verify_configured_hooks(
    config: &Value,
    config_path: &Path,
    client: HookClient,
    workspace: &Path,
    runtime: &InstallRuntime,
) -> Result<()> {
    let mode = HookMode::from_enforcing(load_policy(workspace).enforcing());
    // Verification must never create a binding, queue a delivery, or connect
    // to the operator's running daemon. The hook wrappers still execute with
    // representative structured host fields, but all protected state is
    // redirected to a disposable root and the loopback endpoint is reserved
    // (port zero), so this check cannot persist capture data.
    with_verify_state_root(|state_root| {
        for (event, script) in installed_hooks(mode) {
            let command = configured_hook_command(config, config_path, script)?;
            let output = run_fixture_process(
                &command,
                &[],
                hook_fixture_payload(event),
                Some(HookProcessContext {
                    workspace,
                    executable: &runtime.executable,
                    state_root,
                }),
                &format!("configured {event} hook"),
            )?;
            verify_hook_stdout(client, mode, event, &output)?;
        }
        Ok(())
    })?;
    if mode == HookMode::Enforcing {
        verify_enforcing_hooks(config, config_path, client, workspace, runtime)?;
    }
    Ok(())
}

/// Exercise what enforcement adds, each probe in its own state root so the
/// once-per-session notice is observable. The daemon is unreachable by
/// construction, so these prove the fail-open half of the contract: the gate
/// runs, classifies the path, never denies without a daemon, and says so. The
/// deny half needs a daemon and is covered by the route and adapter tests.
fn verify_enforcing_hooks(
    config: &Value,
    config_path: &Path,
    client: HookClient,
    workspace: &Path,
    runtime: &InstallRuntime,
) -> Result<()> {
    let gate = configured_hook_command(config, config_path, "pre-tool-use.sh")?;
    let post = configured_hook_command(config, config_path, "post-tool-use.sh")?;
    let product = workspace.join("lattice-install-verification-product.rs");
    let documentation = workspace.join("docs/lattice-install-verification.md");
    let edit_payload = |path: &Path| {
        json!({
            "session_id": "install-verification-enforcement",
            "tool_name": if client == HookClient::ClaudeCode { "Edit" } else { "apply_patch" },
            "tool_input": {
                "file_path": path,
                "old_string": "lattice-install-verification-tool-input",
                "new_string": "lattice-install-verification-tool-input",
            },
            "transcript_path": "/tmp/lattice-install-verification-transcript",
        })
        .to_string()
    };
    let run = |command: &Path, payload: &str, label: &str, state_root: &Path| {
        run_fixture_process(
            command,
            &[],
            payload,
            Some(HookProcessContext {
                workspace,
                executable: &runtime.executable,
                state_root,
            }),
            label,
        )
    };

    with_verify_state_root(|state_root| {
        let output = run(
            &gate,
            &edit_payload(&product),
            "configured PreToolUse gate (product path)",
            state_root,
        )?;
        let expected = json!({"hookSpecificOutput": {
            "additionalContext": notice_text(NoticeCondition::DaemonUnreachable),
            "hookEventName": "PreToolUse",
        }})
        .to_string();
        if !output_matches_line_terminated(&output, &expected) {
            return Err(anyhow!(
                "verification failed: with the daemon unreachable the PreToolUse gate must allow the edit and emit exactly the unreachable-daemon notice"
            ));
        }
        // Same session, same condition: the notice must not repeat.
        let repeated = run(
            &gate,
            &edit_payload(&product),
            "configured PreToolUse gate (repeat)",
            state_root,
        )?;
        if !repeated.is_empty() {
            return Err(anyhow!(
                "verification failed: the PreToolUse gate repeated a once-per-session notice"
            ));
        }
        Ok(())
    })?;

    with_verify_state_root(|state_root| {
        let output = run(
            &gate,
            &edit_payload(&documentation),
            "configured PreToolUse gate (documentation path)",
            state_root,
        )?;
        if !output.is_empty() {
            return Err(anyhow!(
                "verification failed: the PreToolUse gate must stay silent for an exempt documentation path"
            ));
        }
        Ok(())
    })?;

    with_verify_state_root(|state_root| {
        let payload = json!({
            "session_id": "install-verification-enforcement",
            "tool_name": "Bash",
            "tool_input": {"command": "lattice-install-verification-tool-input"},
            "tool_response": {"stdout": "lattice-install-verification-tool-output"},
        })
        .to_string();
        // The first shell call only records a baseline of repository state.
        let output = run(
            &post,
            &payload,
            "configured PostToolUse shell hook",
            state_root,
        )?;
        if !output.is_empty() {
            return Err(anyhow!(
                "verification failed: the first shell PostToolUse call must record a baseline silently"
            ));
        }
        Ok(())
    })
}

/// Run `check` against a disposable protected state root, prove nothing from
/// the fixtures was retained in it, and remove it whatever the outcome.
fn with_verify_state_root(check: impl FnOnce(&Path) -> Result<()>) -> Result<()> {
    let state_root = install_verify_state_root()?;
    let result =
        check(&state_root).and_then(|()| verify_hook_state_contains_no_fixture_data(&state_root));
    let cleanup = std::fs::remove_dir_all(&state_root);
    result.and(cleanup.with_context(|| {
        format!(
            "verification failed: remove temporary hook state `{}`",
            state_root.display()
        )
    }))
}

fn install_verify_state_root() -> Result<PathBuf> {
    let nonce = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .context("read clock for temporary hook verification state")?
            .as_nanos()
    );
    let root = std::env::temp_dir().join(format!("lattice-install-verify-{nonce}"));
    std::fs::create_dir(&root).with_context(|| {
        format!(
            "create temporary hook verification state `{}`",
            root.display()
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&root)?.permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&root, permissions)?;
    }
    Ok(root)
}

fn verify_hook_state_contains_no_fixture_data(root: &Path) -> Result<()> {
    const FORBIDDEN: [&str; 4] = [
        "lattice-install-verification-transcript",
        "lattice-install-verification-prompt",
        "lattice-install-verification-tool-input",
        "lattice-install-verification-tool-output",
    ];
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory)
            .with_context(|| format!("read temporary hook state `{}`", directory.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path)?;
            if metadata.is_dir() {
                pending.push(path);
            } else if metadata.is_file() {
                let bytes = std::fs::read(&path)?;
                if FORBIDDEN.iter().any(|needle| {
                    bytes
                        .windows(needle.len())
                        .any(|window| window == needle.as_bytes())
                }) {
                    return Err(anyhow!(
                        "verification failed: hook state retained raw fixture or transcript data"
                    ));
                }
            }
        }
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
        "SessionStart" => {
            r#"{"session_id":"install-verification","source":"startup","transcript_path":"/tmp/lattice-install-verification-transcript"}"#
        }
        "UserPromptSubmit" => {
            r#"{"session_id":"install-verification","prompt":"lattice-install-verification-prompt"}"#
        }
        "PreToolUse" => {
            r#"{"session_id":"install-verification","tool_name":"Edit","tool_input":{"file_path":"docs/lattice-install-verification.md","old_string":"lattice-install-verification-tool-input"},"transcript_path":"/tmp/lattice-install-verification-transcript"}"#
        }
        "PostToolUse" => {
            r#"{"session_id":"install-verification","tool_name":"apply_patch","file_path":"README.md","tool_input":"lattice-install-verification-tool-input","tool_response":"lattice-install-verification-tool-output","transcript_path":"/tmp/lattice-install-verification-transcript"}"#
        }
        "Stop" => {
            r#"{"session_id":"install-verification","last_assistant_message":"bounded install verification summary","transcript_path":"/tmp/lattice-install-verification-transcript","cwd":"/private"}"#
        }
        "SessionEnd" => {
            r#"{"session_id":"install-verification","transcript_path":"/tmp/lattice-install-verification-transcript","reason":"private","cwd":"/private","final_summary":"lattice-install-verification-prompt"}"#
        }
        _ => "{}",
    }
}

/// With the fixture daemon deliberately unreachable, a hook may say nothing,
/// or exactly the one notice its mode defines for its event. Keep this
/// allowlist exact: accepting arbitrary hook output would conceal a
/// host-envelope or transcript leak during installation verification.
fn verify_hook_stdout(client: HookClient, mode: HookMode, event: &str, output: &str) -> Result<()> {
    if output.is_empty() {
        return Ok(());
    }
    let expected = match (event, mode) {
        ("SessionStart", HookMode::BestEffort) => {
            Some(session_start_notice(client, BEST_EFFORT_DAEMON_NOTICE))
        }
        ("SessionStart", HookMode::Enforcing) => Some(session_start_notice(
            client,
            &notice_text(NoticeCondition::DaemonUnreachable),
        )),
        // Stop has no passive channel to the model, so an enforcing workspace
        // tells the operator that the turn summary was not captured.
        ("Stop", HookMode::Enforcing) => Some(
            json!({"systemMessage": notice_text(NoticeCondition::CaptureUnavailable)}).to_string(),
        ),
        _ => None,
    };
    if expected.is_some_and(|expected| output_matches_line_terminated(output, &expected)) {
        return Ok(());
    }
    Err(anyhow!(
        "verification failed: {event} hook wrote stdout its mode does not allow"
    ))
}

fn session_start_notice(client: HookClient, text: &str) -> String {
    match client {
        HookClient::Codex => text.to_string(),
        HookClient::ClaudeCode => json!({
            "hookSpecificOutput": {
                "additionalContext": text,
                "hookEventName": "SessionStart",
            }
        })
        .to_string(),
    }
}

/// Hook adapters write one rendered presentation with `println!`; accept that
/// one platform-neutral line terminator, but no other surrounding bytes.
fn output_matches_line_terminated(output: &str, expected: &str) -> bool {
    output == expected
        || output
            .strip_suffix('\n')
            .and_then(|output| output.strip_suffix('\r').or(Some(output)))
            == Some(expected)
}

struct HookProcessContext<'a> {
    workspace: &'a Path,
    executable: &'a Path,
    state_root: &'a Path,
}

/// How long to wait for the exit status of a process whose stdout has already
/// reached end of file. What remains is process teardown, which takes
/// milliseconds; two seconds allows for a heavily loaded machine. A process
/// still running after that really did close stdout while staying alive.
const EXIT_AFTER_EOF_GRACE: Duration = Duration::from_secs(2);

fn wait_briefly_for_exit(
    child: &mut std::process::Child,
    deadline: Instant,
) -> std::io::Result<Option<std::process::ExitStatus>> {
    let give_up = deadline.min(Instant::now() + EXIT_AFTER_EOF_GRACE);
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if Instant::now() >= give_up {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
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
            .env("LATTICE_SKIP_METRICS", "1")
            .env("XDG_STATE_HOME", context.state_root)
            .env("LATTICE_DAEMON_ADDR", "127.0.0.1:0");
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

/// Options for the `health-backtest` operational report.
struct HealthBacktestArgs {
    /// Repositories to replay, in the order given.
    repositories: Vec<PathBuf>,
    /// Emit JSON instead of markdown.
    json: bool,
    /// Write the report to this file instead of standard output.
    output: Option<PathBuf>,
    /// Bounds passed through to the replay engine.
    limits: ReplayLimits,
    /// Emit the Phase H5 regression report instead of the backtest document,
    /// and exit non-zero when a health signal has regressed.
    regression: bool,
}

/// `lattice health-backtest` — Phase H1 of the health engine plan.
///
/// An operational report like `metrics`, deliberately *not* an MCP verb: it
/// replays history offline and produces a document, so it has no place in the
/// eight-verb retrieval surface (spec design decision 5).
fn run_health_backtest_command(args: Vec<String>) -> i32 {
    match parse_health_backtest_args(args).and_then(run_health_backtest) {
        Ok(outcome) => {
            if let Some(output) = outcome.rendered {
                println!("{}", output.trim_end());
            }
            outcome.exit_code
        }
        Err(error) => {
            eprintln!("lattice: {error:#}");
            1
        }
    }
}

/// What a `health-backtest` run produced and what it should exit with.
struct HealthBacktestOutcome {
    /// Rendered document, absent when it was written to a file instead.
    rendered: Option<String>,
    /// Non-zero only when `--regression` found a regressed health signal.
    exit_code: i32,
}

impl HealthBacktestOutcome {
    fn document(rendered: Option<String>) -> Self {
        Self {
            rendered,
            exit_code: 0,
        }
    }
}

/// Replay every requested repository and render the report.
fn run_health_backtest(args: HealthBacktestArgs) -> Result<HealthBacktestOutcome> {
    let mut builder = ReportBuilder::new();
    for repository in &args.repositories {
        // A repository that cannot be replayed fails the run rather than being
        // quietly dropped: a pooled report that silently lost a repository
        // would misstate the evidence it rests on.
        // Cut points are folded in and dropped one at a time. A single cut
        // point of a large repository carries hundreds of megabytes of fact
        // snapshots, so collecting even one repository's worth was measured
        // above four gigabytes; streaming keeps peak memory at one cut point.
        builder.start_repository(&repository_name(repository));
        let mut cut_points = 0usize;
        let summary = replay_repository_streaming(repository, args.limits, |cut_point| {
            cut_points += 1;
            builder.push_cut_point(&cut_point);
        })
        .with_context(|| format!("could not replay repository `{}`", repository.display()))?;
        eprintln!(
            "lattice: replayed {} — {} cut points over {} first-parent commits",
            summary.name, cut_points, summary.report.spine_length
        );
        builder.finish_repository(&summary);
    }

    let report = builder.finish();
    if args.regression {
        return health_regression_outcome(&report, args.json, args.output);
    }
    let rendered = if args.json {
        report
            .render_json()
            .context("could not render the backtest report as JSON")?
    } else {
        report.render_markdown()
    };

    match args.output {
        Some(path) => {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent).with_context(|| {
                        format!("could not create directory `{}`", parent.display())
                    })?;
                }
            }
            std::fs::write(&path, &rendered)
                .with_context(|| format!("could not write `{}`", path.display()))?;
            eprintln!("lattice: wrote {}", path.display());
            Ok(HealthBacktestOutcome::document(None))
        }
        None => Ok(HealthBacktestOutcome::document(Some(rendered))),
    }
}

/// Turn a fresh backtest into the Phase H5 regression report.
///
/// This is the CI-facing surface of `docs/plans/2026-08-13-health-engine.md`
/// § "Phase H5 — Prove it stays honest": the run exits non-zero when a health
/// signal has fallen below the floor derived from
/// `docs/reports/health-backtest/2026-08-14.md`. A run whose corpus does not
/// match that report's exits zero with missing rows rather than pretending to a
/// comparison it cannot make — see `lattice_core::metrics::health_backtest`.
fn health_regression_outcome(
    report: &lattice_core::health::backtest::report::BacktestReport,
    json: bool,
    output: Option<PathBuf>,
) -> Result<HealthBacktestOutcome> {
    let now = lattice_core::Utc::now();
    let regression = RegressionReport::build(ReportInput {
        current: health_backtest::signals_from_report(report, now),
        baseline: Some(health_backtest::committed_baseline(now)),
        // The health signals carry their own evidence; no Phase 9 benchmark
        // document contributes to them.
        benchmark_report_path: PathBuf::new(),
        scope: MetricScope::repo("health-backtest"),
        success_criteria: SuccessCriteriaThresholds::initial(),
    })
    .context("could not build the health regression report")?;

    let rendered = if json {
        regression
            .render_json()
            .context("could not render the health regression report as JSON")?
    } else {
        regression.render_text()
    };
    let exit_code = report_exit_code(&regression, true);
    eprintln!("lattice: {}", regression.render_ci_summary());

    match output {
        Some(path) => {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent).with_context(|| {
                        format!("could not create directory `{}`", parent.display())
                    })?;
                }
            }
            std::fs::write(&path, &rendered)
                .with_context(|| format!("could not write `{}`", path.display()))?;
            eprintln!("lattice: wrote {}", path.display());
            Ok(HealthBacktestOutcome {
                rendered: None,
                exit_code,
            })
        }
        None => Ok(HealthBacktestOutcome {
            rendered: Some(rendered),
            exit_code,
        }),
    }
}

fn parse_health_backtest_args(args: Vec<String>) -> Result<HealthBacktestArgs> {
    let mut repositories: Vec<PathBuf> = Vec::new();
    let mut json = false;
    let mut output = None;
    let mut limits = ReplayLimits::default();
    let mut regression = false;
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--json" => json = true,
            "--regression" => regression = true,
            "--repo" | "-r" => {
                let value = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("--repo requires a path"))?;
                repositories.push(canonical_workspace(Path::new(value))?);
                i += 1;
            }
            "--output" | "-o" => {
                let value = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("--output requires a path"))?;
                output = Some(PathBuf::from(value));
                i += 1;
            }
            "--cut-points" => {
                limits.cut_points = parse_positive(&args, i, "--cut-points")?;
                i += 1;
            }
            "--horizon-days" => {
                limits.horizon.max_days = parse_positive::<u32>(&args, i, "--horizon-days")?;
                i += 1;
            }
            "--horizon-commits" => {
                limits.horizon.max_commits = parse_positive(&args, i, "--horizon-commits")?;
                i += 1;
            }
            "--window" => {
                limits.git.history_limit = parse_positive(&args, i, "--window")?;
                i += 1;
            }
            other => return Err(anyhow!("unknown health-backtest argument `{}`", other)),
        }
        i += 1;
    }
    if repositories.is_empty() {
        repositories.push(detect_workspace_root()?);
    }
    Ok(HealthBacktestArgs {
        repositories,
        json,
        output,
        limits,
        regression,
    })
}

/// Parse a positive integer flag value.
fn parse_positive<T>(args: &[String], index: usize, flag: &str) -> Result<T>
where
    T: std::str::FromStr + PartialOrd + From<u8>,
    T::Err: std::fmt::Display,
{
    let value = args
        .get(index + 1)
        .ok_or_else(|| anyhow!("{flag} requires a positive integer"))?;
    let parsed: T = value
        .parse()
        .map_err(|error| anyhow!("invalid {flag} `{value}`: {error}"))?;
    if parsed <= T::from(0u8) {
        return Err(anyhow!("{flag} must be positive"));
    }
    Ok(parsed)
}

fn run_metrics_command(args: Vec<String>) -> i32 {
    match parse_metrics_args(args).and_then(
        |(workspace, days, json, projection)| match projection {
            MetricsProjection::Memory => {
                render_memory_metrics_for_workspace(&workspace, days, json)
            }
            MetricsProjection::Health => {
                render_health_metrics_for_workspace(&workspace, days, json)
            }
            MetricsProjection::All => render_metrics_for_workspace(&workspace, days, json),
        },
    ) {
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

/// Which counters `lattice metrics` projects out of the one durable ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MetricsProjection {
    /// The per-tool call and follow-through table.
    All,
    /// The memory retrieval and injection counters.
    Memory,
    /// The health-evidence injection and follow-through counters.
    Health,
}

fn parse_metrics_args(args: Vec<String>) -> Result<(PathBuf, usize, bool, MetricsProjection)> {
    let mut workspace = None;
    let mut days = 14usize;
    let mut json = false;
    let mut projection = MetricsProjection::All;
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--json" => json = true,
            "--memory" => projection = MetricsProjection::Memory,
            "--health" => projection = MetricsProjection::Health,
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
        projection,
    ))
}

/// Project the health-evidence counters from the durable adoption ledger.
///
/// Answers spec H4.5's question — whether injected health evidence was followed
/// — by reusing the same edit-follow-through join and retention behaviour as
/// the other views, read-only, over the one ledger.
fn render_health_metrics_for_workspace(
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
                let Some(counter) = tools.get("health") else {
                    continue;
                };
                rows.push(json!({
                    "day": day,
                    "client": client,
                    "channel": channel,
                    "injections": counter.get("health_evidence_injections").cloned().unwrap_or_else(|| json!(0)),
                    "files_cited": counter.get("health_evidence_cited_files").cloned().unwrap_or_else(|| json!(0)),
                    "followed": counter.get("health_evidence_followed").cloned().unwrap_or_else(|| json!(0)),
                }));
            }
        }
    }
    if json {
        return Ok(serde_json::to_string_pretty(&json!({ "days": rows }))?);
    }
    let mut out = String::from(
        "day | client | channel | injections | files_cited | followed | follow_rate\n",
    );
    out.push_str("--- | --- | --- | ---: | ---: | ---: | ---:\n");
    if rows.is_empty() {
        out.push_str("_no health metrics recorded_\n");
        return Ok(out);
    }
    for row in rows {
        let injections = row["injections"].as_u64().unwrap_or(0);
        let followed = row["followed"].as_u64().unwrap_or(0);
        let follow_rate = if injections == 0 {
            0.0
        } else {
            followed as f64 * 100.0 / injections as f64
        };
        out.push_str(&format!(
            "{} | {} | {} | {} | {} | {} | {:.0}%\n",
            row["day"].as_str().unwrap_or("unknown"),
            row["client"].as_str().unwrap_or("unknown"),
            row["channel"].as_str().unwrap_or("unknown"),
            row["injections"],
            row["files_cited"],
            row["followed"],
            follow_rate,
        ));
    }
    Ok(out)
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
    if request.json
        && matches!(
            command.as_str(),
            "context" | "prepare_change" | "impact" | "diagnose"
        )
    {
        set_default(&mut request.arguments, "render", json!("json"));
        set_default(&mut request.arguments, "wire_format", json!("standard"));
    } else if !request.json && command != "remember" {
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
    if kind == "evolution" {
        let mut arguments = json!({"kind": "evolution"});
        for (flag, field) in [
            ("--action", "action"),
            ("--memory-id", "memory_id"),
            ("--proposal-id", "proposal_id"),
            ("--superseded-by-memory-id", "superseded_by_memory_id"),
            ("--invalidate-reason", "invalidate_reason"),
            ("--reason", "reason"),
            ("--decided-by", "decided_by"),
        ] {
            if let Some(value) = parser.take_flag_value(flag)? {
                set_value(&mut arguments, field, json!(value));
            }
        }
        for (flag, field) in [
            ("--linked-file", "linked_files"),
            ("--linked-symbol", "linked_symbols"),
            ("--linked-doc", "linked_docs"),
            ("--linked-test", "linked_tests"),
            ("--linked-memory", "linked_memories"),
            ("--validity-condition", "validity_conditions"),
            ("--invalidation-trigger", "invalidation_triggers"),
        ] {
            let values = parser.take_repeated(flag)?;
            if !values.is_empty() {
                set_value(&mut arguments, field, json!(values));
            }
        }
        parser.reject_unknown_flags()?;
        let content = parser.join_positionals();
        if !content.trim().is_empty() {
            set_value(&mut arguments, "content", json!(content));
        }
        let parsed = crate::rpc::memory_v2::propose_memory_evolution::parse_args(&arguments)
            .map_err(|error| anyhow!(error))?;
        crate::rpc::memory_v2::propose_memory_evolution::validate_args(&parsed)
            .map_err(|error| anyhow!(error))?;
        return Ok(parser.request("remember", arguments));
    }
    if !matches!(kind.as_str(), "quick" | "durable" | "outcome") {
        return Err(anyhow!("unknown remember kind `{kind}`"));
    }
    let outcome_status = if kind == "outcome" {
        let value = parser
            .take_flag_value("--status")?
            .unwrap_or_else(|| "success".into());
        if !matches!(value.as_str(), "success" | "failure") {
            return Err(anyhow!("outcome --status must be success or failure"));
        }
        Some(value)
    } else {
        None
    };
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
        set_default(&mut arguments, "status", json!(outcome_status));
    }
    Ok(parser.request("remember", arguments))
}

fn parse_recall(parser: &mut ArgParser) -> Result<CliRequest> {
    let mode = parser
        .take_flag_value("--mode")?
        .unwrap_or_else(|| "search".to_string());
    let task_id = parser.take_flag_value("--task-id")?;
    let authority = parser.take_flag_value("--authority")?;
    let delivery_id = parser.take_flag_value("--delivery-id")?;
    let payload_hash = parser.take_flag_value("--payload-hash")?;
    let focus_files = parser.take_repeated("--focus-files")?;
    let memory_id = parser.take_flag_value("--memory-id")?;
    let run_check = parser.take_flag_value("--run-check")?;
    let include_retention_stale = parser.take_bool("--include-retention-stale");
    let query = parser.join_positionals();
    if mode == "verify" {
        if include_retention_stale {
            return Err(anyhow!(
                "--include-retention-stale is only valid for recall search or task"
            ));
        }
        if parser.positionals.len() > 1 {
            return Err(anyhow!(
                "recall --mode verify accepts one positional memory ID"
            ));
        }
        if memory_id.is_some() && !query.trim().is_empty() {
            return Err(anyhow!(
                "recall --mode verify cannot combine --memory-id with a query"
            ));
        }
        let memory_id = memory_id
            .or_else(|| (!query.trim().is_empty()).then_some(query.clone()))
            .ok_or_else(|| anyhow!("recall --mode verify requires --memory-id or a memory ID"))?;
        let mut arguments = json!({
            "mode": mode,
            "memory_id": memory_id,
        });
        if let Some(run_check) = run_check {
            set_value(&mut arguments, "run_check", json!(run_check));
        }
        if !focus_files.is_empty() {
            set_value(&mut arguments, "focus_files", json!(focus_files));
        }
        return Ok(parser.request("recall", arguments));
    }
    if memory_id.is_some() {
        return Err(anyhow!(
            "--memory-id is only valid for recall --mode verify"
        ));
    }
    if run_check.is_some() {
        return Err(anyhow!(
            "--run-check is only valid for recall --mode verify"
        ));
    }
    if include_retention_stale && mode != "search" && mode != "task" {
        return Err(anyhow!(
            "--include-retention-stale is only valid for recall search or task"
        ));
    }
    if query.trim().is_empty() && mode != "acknowledge_delivery" {
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
    if include_retention_stale {
        set_value(&mut arguments, "include_retention_stale", json!(true));
    }
    if mode == "acknowledge_delivery" {
        let authority = authority
            .ok_or_else(|| anyhow!("recall --mode acknowledge_delivery requires --authority"))?;
        let delivery_id = delivery_id
            .ok_or_else(|| anyhow!("recall --mode acknowledge_delivery requires --delivery-id"))?;
        let payload_hash = payload_hash
            .ok_or_else(|| anyhow!("recall --mode acknowledge_delivery requires --payload-hash"))?;
        set_value(&mut arguments, "authority", json!(authority));
        set_value(&mut arguments, "delivery_id", json!(delivery_id));
        set_value(&mut arguments, "payload_hash", json!(payload_hash));
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
    #[test]
    fn remember_evolution_cli_uses_the_public_contract() {
        let root = tempfile::tempdir().unwrap();
        let parse = |tail: &[&str]| {
            let mut args = vec![
                "lattice".to_string(),
                "remember".to_string(),
                "--workspace".to_string(),
                root.path().display().to_string(),
            ];
            args.extend(tail.iter().map(|value| value.to_string()));
            super::parse_args(args)
        };
        let proposed = parse(&[
            "--kind",
            "evolution",
            "--action",
            "propose",
            "--memory-id",
            "old",
            "--superseded-by-memory-id",
            "new",
            "--reason",
            "Current contract changed",
        ])
        .unwrap();
        assert_eq!(proposed.arguments["superseded_by_memory_id"], "new");
        assert!(proposed.arguments.get("content").is_none());
        assert!(proposed.arguments.get("render").is_none());
        let applied = parse(&[
            "--kind",
            "evolution",
            "--action",
            "apply",
            "--proposal-id",
            "proposal",
        ])
        .unwrap();
        assert_eq!(applied.arguments["action"], "apply");
        for invalid in [
            vec!["--kind", "unknown", "text"],
            vec!["--kind", "evolution", "--action", "apply"],
            vec![
                "--kind",
                "evolution",
                "--action",
                "propose",
                "--memory-id",
                "old",
            ],
            vec![
                "--kind",
                "evolution",
                "--action",
                "reject",
                "--proposal-id",
                "p",
                "unexpected content",
            ],
        ] {
            assert!(parse(&invalid).is_err());
        }
        let outcome = parse(&["--kind", "outcome", "--status", "failure", "Build failed"]).unwrap();
        assert_eq!(outcome.arguments["status"], "failure");
        assert_eq!(outcome.arguments["summary"], "Build failed");
    }

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
            for (_event, script) in installed_hooks(HookMode::Enforcing) {
                write_executable(&hooks.join(script), "#!/bin/sh\nexit 0\n");
            }
            write_executable(&hooks.join("common.sh"), "#!/bin/sh\nexit 0\n");
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
    fn health_backtest_defaults_to_the_detected_workspace_and_markdown() {
        let parsed =
            parse_health_backtest_args(vec!["lattice".into(), "health-backtest".into()]).unwrap();
        assert_eq!(parsed.repositories.len(), 1);
        assert!(!parsed.json);
        assert!(parsed.output.is_none());
        assert_eq!(parsed.limits.cut_points, ReplayLimits::default().cut_points);
        // The default run produces the backtest document, not the CI gate.
        assert!(!parsed.regression);
    }

    /// Phase H5: the regression gate is opt-in and composes with `--json`.
    #[test]
    fn health_backtest_regression_mode_is_requested_explicitly() {
        let parsed = parse_health_backtest_args(vec![
            "lattice".into(),
            "health-backtest".into(),
            "--regression".into(),
            "--json".into(),
        ])
        .unwrap();
        assert!(parsed.regression);
        assert!(parsed.json);
    }

    #[test]
    fn health_backtest_accepts_several_repositories_in_the_order_given() {
        let workspace = std::env::current_dir().unwrap();
        let parsed = parse_health_backtest_args(vec![
            "lattice".into(),
            "health-backtest".into(),
            "--repo".into(),
            workspace.to_string_lossy().into_owned(),
            "--repo".into(),
            workspace.to_string_lossy().into_owned(),
            "--json".into(),
            "--output".into(),
            "report.json".into(),
        ])
        .unwrap();
        assert_eq!(parsed.repositories.len(), 2);
        assert!(parsed.json);
        assert_eq!(parsed.output, Some(PathBuf::from("report.json")));
    }

    #[test]
    fn health_backtest_bounds_are_configurable() {
        let parsed = parse_health_backtest_args(vec![
            "lattice".into(),
            "health-backtest".into(),
            "--cut-points".into(),
            "3".into(),
            "--horizon-days".into(),
            "30".into(),
            "--horizon-commits".into(),
            "50".into(),
            "--window".into(),
            "200".into(),
        ])
        .unwrap();
        assert_eq!(parsed.limits.cut_points, 3);
        assert_eq!(parsed.limits.horizon.max_days, 30);
        assert_eq!(parsed.limits.horizon.max_commits, 50);
        assert_eq!(parsed.limits.git.history_limit, 200);
    }

    #[test]
    fn health_backtest_rejects_unusable_arguments_rather_than_guessing() {
        for bad in [
            vec!["lattice".into(), "health-backtest".into(), "--nope".into()],
            vec!["lattice".into(), "health-backtest".into(), "--repo".into()],
            vec![
                "lattice".into(),
                "health-backtest".into(),
                "--output".into(),
            ],
            vec![
                "lattice".into(),
                "health-backtest".into(),
                "--cut-points".into(),
                "0".into(),
            ],
            vec![
                "lattice".into(),
                "health-backtest".into(),
                "--horizon-days".into(),
                "not-a-number".into(),
            ],
        ] {
            let bad: Vec<String> = bad;
            assert!(
                parse_health_backtest_args(bad.clone()).is_err(),
                "accepted {bad:?}"
            );
        }
    }

    #[test]
    fn health_backtest_is_dispatched_as_a_local_command_not_an_mcp_verb() {
        // The eight-verb surface is fixed (spec design decision 5); this
        // subcommand is an operational report like `metrics`.
        assert!(usage().contains("health-backtest"));
        assert_eq!(EXPECTED_MCP_TOOL_COUNT, 8);
    }

    #[test]
    fn parses_memory_metrics_view_flag() {
        let workspace = std::env::current_dir().unwrap();
        let (parsed_workspace, days, json, projection) = parse_metrics_args(vec![
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
        assert_eq!(projection, MetricsProjection::Memory);
    }

    #[test]
    fn parses_health_metrics_view_flag() {
        let workspace = std::env::current_dir().unwrap();
        let (_, _, _, projection) = parse_metrics_args(vec![
            "lattice".into(),
            "metrics".into(),
            "--health".into(),
            "--workspace".into(),
            workspace.to_string_lossy().into_owned(),
        ])
        .unwrap();
        assert_eq!(projection, MetricsProjection::Health);
    }

    #[test]
    fn metrics_defaults_to_the_full_projection() {
        let workspace = std::env::current_dir().unwrap();
        let (_, _, _, projection) = parse_metrics_args(vec![
            "lattice".into(),
            "metrics".into(),
            "--workspace".into(),
            workspace.to_string_lossy().into_owned(),
        ])
        .unwrap();
        assert_eq!(projection, MetricsProjection::All);
    }

    #[test]
    fn memory_metrics_view_projects_retrieval_and_injection_counters() {
        let temporary = tempfile::tempdir().expect("create existing metrics workspace");
        let root = temporary.path().to_path_buf();
        let store = AdoptionMetricsStore::new(&root);
        store
            .record_memory_retrieval(MemoryRetrievalRecord {
                session_id: "s".to_string(),
                client: "codex".to_string(),
                channel: "cli".to_string(),
                retrieval_id: "r".to_string(),
                retrieved_count: 3,
            })
            .unwrap();
        store
            .record_memory_use(MemoryUseRecord {
                retrieval_id: "r".to_string(),
                used_count: 2,
            })
            .unwrap();
        store
            .record_memory_injection(MemoryInjectionRecord {
                session_id: "s".to_string(),
                client: "codex".to_string(),
                channel: "cli".to_string(),
                injection_id: "i".to_string(),
                shown_count: 2,
            })
            .unwrap();
        store
            .record_memory_injection_action(MemoryInjectionActionRecord {
                injection_id: "i".to_string(),
                acted_count: 1,
            })
            .unwrap();

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
        assert_eq!(request.arguments["render"], "json");
        assert_eq!(request.timeout, Duration::from_millis(1500));
    }

    #[test]
    fn json_workflows_request_structured_payloads_and_text_defaults_stay_compact() {
        for command in ["context", "prepare_change", "impact", "diagnose"] {
            for structured in [false, true] {
                let mut args = vec!["lattice".into(), command.into(), "selector.py".into()];
                if structured {
                    args.push("--json".into());
                }
                let request = parse_args(args).unwrap();
                assert_eq!(
                    request.arguments["render"],
                    if structured { "json" } else { "markdown" }
                );
                if !structured {
                    assert_eq!(request.arguments["render_mode"], "compact");
                } else {
                    assert_eq!(request.arguments["wire_format"], "standard");
                }
            }
        }
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
    fn parses_delivery_acknowledgement_without_a_query_and_requires_full_receipt() {
        let request = parse_args(vec![
            "lattice".into(),
            "recall".into(),
            "--mode".into(),
            "acknowledge_delivery".into(),
            "--authority".into(),
            "repository:repo".into(),
            "--delivery-id".into(),
            "mdel_receipt".into(),
            "--payload-hash".into(),
            "sha256:payload".into(),
        ])
        .expect("parse acknowledgement");
        assert_eq!(request.tool, "recall");
        assert_eq!(request.arguments["mode"], "acknowledge_delivery");
        assert_eq!(request.arguments["authority"], "repository:repo");
        assert_eq!(request.arguments["delivery_id"], "mdel_receipt");
        assert_eq!(request.arguments["payload_hash"], "sha256:payload");
        assert!(parse_args(vec![
            "lattice".into(),
            "recall".into(),
            "--mode".into(),
            "acknowledge_delivery".into(),
            "--authority".into(),
            "repository:repo".into(),
        ])
        .is_err());
    }

    #[test]
    fn parses_verify_memory_id_and_explicit_run_check() {
        let request = parse_args(vec![
            "lattice".into(),
            "recall".into(),
            "--mode".into(),
            "verify".into(),
            "--memory-id".into(),
            "memory-123".into(),
            "--run-check".into(),
            "unit".into(),
        ])
        .expect("parse verify request");
        assert_eq!(request.arguments["mode"], "verify");
        assert_eq!(request.arguments["memory_id"], "memory-123");
        assert_eq!(request.arguments["run_check"], "unit");
        assert!(request.arguments.get("query").is_none());

        let positional = parse_args(vec![
            "lattice".into(),
            "recall".into(),
            "--mode".into(),
            "verify".into(),
            "memory-positional".into(),
        ])
        .expect("parse positional verify request");
        assert_eq!(positional.arguments["memory_id"], "memory-positional");
        assert!(positional.arguments.get("run_check").is_none());
    }

    #[test]
    fn recall_verify_rejects_missing_or_conflicting_memory_id() {
        assert!(parse_args(vec![
            "lattice".into(),
            "recall".into(),
            "--mode".into(),
            "verify".into(),
        ])
        .is_err());
        assert!(parse_args(vec![
            "lattice".into(),
            "recall".into(),
            "--mode".into(),
            "verify".into(),
            "--memory-id".into(),
            "memory-123".into(),
            "query".into(),
        ])
        .is_err());
        assert!(parse_args(vec![
            "lattice".into(),
            "recall".into(),
            "--mode".into(),
            "search".into(),
            "--run-check".into(),
            "unit".into(),
            "query".into(),
        ])
        .is_err());
    }

    #[test]
    fn forwards_retention_stale_only_for_search_and_task() {
        for mode in ["search", "task"] {
            let request = parse_args(vec![
                "lattice".into(),
                "recall".into(),
                "--mode".into(),
                mode.into(),
                "query".into(),
                "--include-retention-stale".into(),
            ])
            .expect("parse stale-inclusive recall");
            assert_eq!(request.arguments["include_retention_stale"], true);
        }
        assert!(parse_args(vec![
            "lattice".into(),
            "recall".into(),
            "--mode".into(),
            "acknowledge_delivery".into(),
            "--include-retention-stale".into(),
            "--authority".into(),
            "repository:repo".into(),
            "--delivery-id".into(),
            "receipt".into(),
            "--payload-hash".into(),
            "sha256:payload".into(),
        ])
        .is_err());
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
            with_embeddings: false,
            enforce: None,
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
            r#"{"hooks":{"Stop":[{"hooks":[{"command":"custom-hook"},{"command":"/old/integrations/codex/hooks/stop.sh"}]}]}}"#,
        )
        .unwrap();
        let command = InstallCommand {
            target: InstallTarget::Codex,
            workspaces: vec![workspace.clone()],
            verify: true,
            with_embeddings: false,
            enforce: None,
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
        assert!(!twice.contains("/old/integrations/codex/hooks/stop.sh"));
        assert!(twice.contains(
            &root
                .join("integrations/codex/hooks/session-end.sh")
                .display()
                .to_string()
        ));
        assert_eq!(config["hooks"]["SessionEnd"][0]["hooks"][0]["timeout"], 3);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn install_verify_allows_only_the_exact_notice_each_mode_defines() {
        let best_effort = HookMode::BestEffort;
        let codex_notice = session_start_notice(HookClient::Codex, BEST_EFFORT_DAEMON_NOTICE);
        assert_eq!(
            codex_notice,
            "lattice: daemon unreachable — run 'lattice doctor'"
        );
        assert!(verify_hook_stdout(
            HookClient::Codex,
            best_effort,
            "SessionStart",
            &codex_notice
        )
        .is_ok());
        assert!(verify_hook_stdout(
            HookClient::Codex,
            best_effort,
            "SessionStart",
            &(codex_notice.clone() + "\r\n"),
        )
        .is_ok());

        let claude_notice = session_start_notice(HookClient::ClaudeCode, BEST_EFFORT_DAEMON_NOTICE);
        let claude: Value = serde_json::from_str(&claude_notice).unwrap();
        assert_eq!(
            claude["hookSpecificOutput"]["hookEventName"],
            "SessionStart"
        );
        assert_eq!(
            claude["hookSpecificOutput"]["additionalContext"],
            "lattice: daemon unreachable — run 'lattice doctor'"
        );
        assert!(verify_hook_stdout(
            HookClient::ClaudeCode,
            best_effort,
            "SessionStart",
            &claude_notice
        )
        .is_ok());

        let enforcing = HookMode::Enforcing;
        let loud = session_start_notice(
            HookClient::ClaudeCode,
            &notice_text(NoticeCondition::DaemonUnreachable),
        );
        let capture =
            json!({"systemMessage": notice_text(NoticeCondition::CaptureUnavailable)}).to_string();
        assert!(
            verify_hook_stdout(HookClient::ClaudeCode, enforcing, "SessionStart", &loud).is_ok()
        );
        assert!(verify_hook_stdout(HookClient::ClaudeCode, enforcing, "Stop", &capture).is_ok());
        for event in ["SessionStart", "UserPromptSubmit", "PreToolUse", "Stop"] {
            assert!(verify_hook_stdout(HookClient::ClaudeCode, enforcing, event, "").is_ok());
        }

        for (client, mode, event, output) in [
            (
                HookClient::Codex,
                best_effort,
                "UserPromptSubmit",
                codex_notice.as_str(),
            ),
            (
                HookClient::Codex,
                best_effort,
                "SessionStart",
                "unexpected hook output",
            ),
            (
                HookClient::Codex,
                best_effort,
                "SessionStart",
                "lattice: daemon unreachable — run 'lattice doctor'\nextra",
            ),
            (
                HookClient::Codex,
                best_effort,
                "SessionStart",
                claude_notice.as_str(),
            ),
            (
                HookClient::ClaudeCode,
                best_effort,
                "SessionStart",
                codex_notice.as_str(),
            ),
            // Each mode accepts only its own wording, on its own event.
            (
                HookClient::ClaudeCode,
                best_effort,
                "SessionStart",
                loud.as_str(),
            ),
            (
                HookClient::ClaudeCode,
                enforcing,
                "SessionStart",
                claude_notice.as_str(),
            ),
            (
                HookClient::ClaudeCode,
                best_effort,
                "Stop",
                capture.as_str(),
            ),
            (
                HookClient::ClaudeCode,
                enforcing,
                "PreToolUse",
                loud.as_str(),
            ),
            (
                HookClient::ClaudeCode,
                enforcing,
                "PreToolUse",
                r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny"}}"#,
            ),
        ] {
            let error = verify_hook_stdout(client, mode, event, output)
                .unwrap_err()
                .to_string();
            assert!(error.contains("its mode does not allow"), "{error}");
        }
    }

    #[test]
    fn enforce_flags_parse_conflict_and_are_refused_for_the_mcp_target() {
        let (root, workspace, _runtime) = install_fixture();
        let parse = |extra: &[&str]| {
            let mut args = vec![
                "lattice".to_string(),
                "install".to_string(),
                "claude-code".to_string(),
                "--workspace".to_string(),
                workspace.to_string_lossy().into_owned(),
            ];
            args.extend(extra.iter().map(|value| value.to_string()));
            parse_install_command(args)
        };
        assert_eq!(parse(&[]).unwrap().enforce, None);
        assert_eq!(parse(&["--enforce"]).unwrap().enforce, Some(true));
        assert_eq!(parse(&["--no-enforce"]).unwrap().enforce, Some(false));
        assert_eq!(
            parse(&["--enforce", "--enforce"]).unwrap().enforce,
            Some(true)
        );
        assert!(parse(&["--enforce", "--no-enforce"])
            .unwrap_err()
            .to_string()
            .contains("cannot be combined"));
        let mcp = parse_install_command(vec![
            "lattice".into(),
            "install".into(),
            "mcp".into(),
            "--workspace".into(),
            workspace.to_string_lossy().into_owned(),
            "--enforce".into(),
        ]);
        assert!(mcp.unwrap_err().to_string().contains("hook installation"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn enforce_on_off_and_plain_rerun_keep_policy_and_hooks_in_step() {
        let (root, workspace, runtime) = install_fixture();
        let settings = workspace.join(".claude/settings.json");
        fs::create_dir_all(settings.parent().unwrap()).unwrap();
        fs::write(
            &settings,
            r#"{"permissions":{"allow":["Bash(ls:*)"]},"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"/team/guard.sh"}]}]}}"#,
        )
        .unwrap();
        let install = |enforce: Option<bool>| {
            run_install_command_with(
                InstallCommand {
                    target: InstallTarget::ClaudeCode,
                    workspaces: vec![workspace.clone()],
                    verify: false,
                    with_embeddings: false,
                    enforce,
                },
                &runtime,
            )
        };
        let gate_count = || {
            let config: Value =
                serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
            assert_eq!(config["permissions"]["allow"][0], "Bash(ls:*)");
            let commands = config["hooks"]["PreToolUse"]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|entry| entry["hooks"].as_array().cloned().unwrap_or_default())
                .filter_map(|hook| hook["command"].as_str().map(str::to_owned))
                .collect::<Vec<_>>();
            assert!(commands.contains(&"/team/guard.sh".to_string()));
            commands
                .iter()
                .filter(|command| command.ends_with("/pre-tool-use.sh"))
                .count()
        };

        // Default: best-effort, and no policy file is invented.
        install(None).unwrap();
        assert_eq!(gate_count(), 0);
        assert!(!policy_path(&workspace).exists());

        let message = install(Some(true)).unwrap();
        assert!(message.contains("hook enforcement enabled"), "{message}");
        assert_eq!(gate_count(), 1);
        assert!(load_policy(&workspace).enforcing());

        // A plain rerun keeps the recorded mode and changes nothing.
        let enforced = fs::read_to_string(&settings).unwrap();
        install(None).unwrap();
        assert_eq!(enforced, fs::read_to_string(&settings).unwrap());
        assert!(load_policy(&workspace).enforcing());

        let message = install(Some(false)).unwrap();
        assert!(message.contains("hook enforcement disabled"), "{message}");
        assert_eq!(gate_count(), 0);
        assert!(!load_policy(&workspace).enforcing());
        install(None).unwrap();
        assert_eq!(gate_count(), 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_unreadable_policy_stops_the_install_before_any_hook_is_written() {
        let (root, workspace, runtime) = install_fixture();
        fs::create_dir_all(workspace.join(".lattice")).unwrap();
        fs::write(policy_path(&workspace), "{not json").unwrap();
        for enforce in [None, Some(true), Some(false)] {
            let error = run_install_command_with(
                InstallCommand {
                    target: InstallTarget::ClaudeCode,
                    workspaces: vec![workspace.clone()],
                    verify: false,
                    with_embeddings: false,
                    enforce,
                },
                &runtime,
            )
            .unwrap_err();
            assert!(
                format!("{error:#}").contains("workspace-policy.json"),
                "{error:#}"
            );
            assert!(!workspace.join(".claude/settings.json").exists());
            assert_eq!(
                fs::read_to_string(policy_path(&workspace)).unwrap(),
                "{not json"
            );
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn install_verify_rejects_persisted_raw_fixture_and_transcript_data() {
        let root = install_verify_state_root().unwrap();
        fs::write(
            root.join("hostile-capture"),
            "lattice-install-verification-prompt /tmp/lattice-install-verification-transcript",
        )
        .unwrap();

        let error = verify_hook_state_contains_no_fixture_data(&root)
            .unwrap_err()
            .to_string();

        assert!(
            error.contains("retained raw fixture or transcript data"),
            "{error}"
        );
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

    fn mcp_config_for(executable: &Path) -> Value {
        json!({"mcpServers": {"lattice": {
            "type": "stdio", "command": executable, "args": ["--stdio"]
        }}})
    }

    #[test]
    fn install_verify_reports_the_exit_status_even_when_stdout_closes_first() {
        let (root, _workspace, runtime) = install_fixture();
        // Close stdout, then take a moment to exit: the window the verifier
        // used to lose, when it asked for the status exactly once.
        write_executable(
            &runtime.executable,
            "#!/bin/sh\necho 'lattice: cannot open index' >&2\nexec 1>&-\nsleep 0.4\nexit 23\n",
        );
        let error = verify_configured_mcp_server(
            &mcp_config_for(&runtime.executable),
            Path::new("/fixture/.mcp.json"),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("configured MCP command exited"), "{error}");
        assert!(error.contains("23"), "{error}");
        assert!(error.contains("cannot open index"), "{error}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn install_verify_says_stdout_closed_only_when_the_command_is_still_running() {
        let (root, _workspace, runtime) = install_fixture();
        write_executable(&runtime.executable, "#!/bin/sh\nexec 1>&-\nsleep 6\n");
        let started = Instant::now();
        let error = verify_configured_mcp_server(
            &mcp_config_for(&runtime.executable),
            Path::new("/fixture/.mcp.json"),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("closed stdout before returning"), "{error}");
        assert!(started.elapsed() >= EXIT_AFTER_EOF_GRACE);
        assert!(started.elapsed() < INSTALL_VERIFY_TIMEOUT);
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
    fn default_install_sets_up_both_clients_and_preserves_project_instructions() {
        let (root, workspace, runtime) = install_fixture();
        fs::write(
            workspace.join("AGENTS.md"),
            "# Project rules\nKeep this exact text.\n",
        )
        .unwrap();
        fs::write(
            workspace.join("CLAUDE.md"),
            "# Claude rules\nKeep these too.\n",
        )
        .unwrap();
        let command = parse_install_command(vec![
            "lattice".into(),
            "install".into(),
            "--workspace".into(),
            workspace.display().to_string(),
            "--verify".into(),
        ])
        .unwrap();
        assert_eq!(command.target, InstallTarget::Project);
        run_install_command_with(command.clone(), &runtime).unwrap();
        let files = [
            "AGENTS.md",
            "CLAUDE.md",
            ".mcp.json",
            ".codex/config.toml",
            ".codex/hooks.json",
            ".claude/settings.json",
        ];
        let before = files
            .iter()
            .map(|file| fs::read(workspace.join(file)).unwrap())
            .collect::<Vec<_>>();
        run_install_command_with(command, &runtime).unwrap();
        for (file, original) in files.iter().zip(before) {
            assert_eq!(
                fs::read(workspace.join(file)).unwrap(),
                original,
                "{file} changed on repeat"
            );
        }
        assert!(fs::read_to_string(workspace.join("AGENTS.md"))
            .unwrap()
            .starts_with("# Project rules\nKeep this exact text.\n"));
        assert!(fs::read_to_string(workspace.join("CLAUDE.md"))
            .unwrap()
            .starts_with("# Claude rules\nKeep these too.\n"));
        let mcp: Value =
            serde_json::from_str(&fs::read_to_string(workspace.join(".mcp.json")).unwrap())
                .unwrap();
        assert_eq!(
            mcp["mcpServers"]["lattice"]["args"],
            json!([
                "--stdio",
                "--workspace",
                workspace.canonicalize().unwrap().to_string_lossy()
            ])
        );
        let codex = fs::read_to_string(workspace.join(".codex/config.toml")).unwrap();
        assert!(codex.contains("mcp_servers.lattice"));
        assert!(codex.contains("--stdio"));
        assert!(codex.contains(workspace.canonicalize().unwrap().to_string_lossy().as_ref()));
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

    #[test]
    fn install_parser_accepts_embedding_provisioning_without_network_access() {
        assert!(is_embeddings_only_install(&[
            "lattice".to_string(),
            "install".to_string(),
            "--with-embeddings".to_string(),
        ]));

        let (root, workspace, _) = install_fixture();
        let command = parse_install_command(vec![
            "lattice".into(),
            "install".into(),
            "mcp".into(),
            "--with-embeddings".into(),
            "--workspace".into(),
            workspace.display().to_string(),
        ])
        .unwrap();
        assert!(command.with_embeddings);
        fs::remove_dir_all(root).unwrap();
    }
}
