//! Deterministic configuration reconciliation for `lattice install`.
//!
//! The command-line layer owns locating configuration files and writing the
//! result.  This module deliberately owns only the domain contract so it can
//! be tested without a user's home directory, current working directory, or
//! the executable that happens to be on `PATH`.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

/// The largest timeout used by the shipped hook scripts.  The outer timeout
/// recorded in agent configuration must always leave room for the hook's
/// query to finish and emit its result.
pub(crate) const MAX_INNER_HOOK_TIMEOUT_SECS: u64 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HookClient {
    ClaudeCode,
    Codex,
}

impl HookClient {
    fn asset_directory(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InstallPaths {
    /// The resolved executable to record in `.mcp.json`, injected by the CLI
    /// instead of looking up `lattice` on PATH.
    pub executable: PathBuf,
    /// Absolute, installation-stable root containing `integrations/`.
    pub asset_root: PathBuf,
}

impl InstallPaths {
    pub(crate) fn new(executable: PathBuf, asset_root: PathBuf) -> Result<Self> {
        if !executable.is_absolute() {
            bail!(
                "installer executable path must be absolute, got `{}`",
                executable.display()
            );
        }
        if !asset_root.is_absolute() {
            bail!(
                "installer asset root must be absolute, got `{}`",
                asset_root.display()
            );
        }
        Ok(Self {
            executable,
            asset_root,
        })
    }

    fn hook_path(&self, client: HookClient, script: &str) -> PathBuf {
        self.asset_root
            .join("integrations")
            .join(client.asset_directory())
            .join("hooks")
            .join(script)
    }
}

/// Whether a workspace has opted in to hook enforcement. It decides which
/// hooks exist and what the tool matchers cover; see
/// `docs/hook-enforcement.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HookMode {
    BestEffort,
    Enforcing,
}

impl HookMode {
    pub(crate) fn from_enforcing(enforcing: bool) -> Self {
        if enforcing {
            Self::Enforcing
        } else {
            Self::BestEffort
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HookDefinition {
    event: &'static str,
    script: &'static str,
    matcher: Option<&'static str>,
    /// Replaces `matcher` in an enforcing workspace.
    enforcing_matcher: Option<&'static str>,
    /// Installed only in an enforcing workspace, and removed otherwise.
    enforcing_only: bool,
    timeout_secs: u64,
    status_message: &'static str,
}

impl HookDefinition {
    fn matcher_for(self, mode: HookMode) -> Option<&'static str> {
        match mode {
            HookMode::Enforcing => self.enforcing_matcher.or(self.matcher),
            HookMode::BestEffort => self.matcher,
        }
    }

    fn installed_in(self, mode: HookMode) -> bool {
        !self.enforcing_only || mode == HookMode::Enforcing
    }
}

/// Every edit tool either host names. `MultiEdit` is no longer documented by
/// Claude Code; an alternative that matches nothing is harmless.
const EDIT_TOOL_MATCHER: &str = "apply_patch|Edit|Write|MultiEdit|NotebookEdit";
/// Edit tools plus the shell tools, whose edits are found from repository
/// state because their input is never read.
const EDIT_AND_SHELL_TOOL_MATCHER: &str =
    "apply_patch|Edit|Write|MultiEdit|NotebookEdit|Bash|PowerShell";

/// Scripts the installer owns, in the order `--verify` runs them.
pub(crate) fn installed_hooks(mode: HookMode) -> Vec<(&'static str, &'static str)> {
    HOOKS
        .iter()
        .filter(|definition| definition.installed_in(mode))
        .map(|definition| (definition.event, definition.script))
        .collect()
}

const HOOKS: [HookDefinition; 6] = [
    HookDefinition {
        event: "SessionStart",
        script: "session-start.sh",
        matcher: Some("startup|resume|clear|compact"),
        enforcing_matcher: None,
        enforcing_only: false,
        timeout_secs: 5,
        status_message: "Loading Lattice session context",
    },
    HookDefinition {
        event: "UserPromptSubmit",
        script: "user-prompt-submit.sh",
        matcher: None,
        enforcing_matcher: None,
        enforcing_only: false,
        timeout_secs: 5,
        status_message: "Loading Lattice prompt context",
    },
    HookDefinition {
        event: "PreToolUse",
        script: "pre-tool-use.sh",
        matcher: Some(EDIT_TOOL_MATCHER),
        enforcing_matcher: None,
        enforcing_only: true,
        timeout_secs: 5,
        status_message: "Checking Lattice change plan",
    },
    HookDefinition {
        event: "PostToolUse",
        script: "post-tool-use.sh",
        matcher: Some("apply_patch|Edit|Write"),
        enforcing_matcher: Some(EDIT_AND_SHELL_TOOL_MATCHER),
        enforcing_only: false,
        timeout_secs: 5,
        status_message: "Checking Lattice edit impact",
    },
    HookDefinition {
        event: "Stop",
        script: "stop.sh",
        matcher: None,
        enforcing_matcher: None,
        enforcing_only: false,
        timeout_secs: 5,
        status_message: "Capturing bounded Lattice turn summary",
    },
    HookDefinition {
        event: "SessionEnd",
        script: "session-end.sh",
        matcher: None,
        enforcing_matcher: None,
        enforcing_only: false,
        timeout_secs: 3,
        status_message: "Finalizing protected Lattice session capture",
    },
];

/// Reconcile Lattice's hook entries while preserving unrelated client hooks.
///
/// Existing entries are identified by hook script basename.  This lets an
/// install replace stale checkout paths, normalize matcher and timeout values,
/// and collapse duplicate Lattice registrations without touching other hooks.
///
/// `mode` selects the hook set. Moving a workspace back to best-effort removes
/// the enforcing-only hook and restores the narrower tool matcher, so the
/// mode on disk is always the mode the policy file records.
pub(crate) fn reconcile_hook_config(
    config: &mut Value,
    client: HookClient,
    paths: &InstallPaths,
    mode: HookMode,
) -> Result<()> {
    for definition in HOOKS {
        validate_outer_timeout(client, definition)?;
        if definition.installed_in(mode) {
            reconcile_hook(config, client, paths, definition, mode)?;
        } else {
            remove_hook(config, definition)?;
        }
    }
    Ok(())
}

/// Remove every registration of one Lattice script, leaving foreign hooks in
/// the same entry, and the entry itself if it still holds any, untouched.
fn remove_hook(config: &mut Value, definition: HookDefinition) -> Result<()> {
    let root = object_mut(config, "hook configuration")?;
    let Some(hooks) = root.get_mut("hooks") else {
        return Ok(());
    };
    let hooks = hooks
        .as_object_mut()
        .ok_or_else(|| anyhow!("hook configuration.hooks must be a JSON object"))?;
    let Some(entries) = hooks.get_mut(definition.event) else {
        return Ok(());
    };
    let entries = entries.as_array_mut().ok_or_else(|| {
        anyhow!(
            "hook configuration.{} must be a JSON array",
            definition.event
        )
    })?;
    for entry in entries.iter_mut() {
        if let Some(entry_hooks) = entry.get_mut("hooks").and_then(Value::as_array_mut) {
            entry_hooks
                .retain(|hook| hook_command_basename(hook).as_deref() != Some(definition.script));
        }
    }
    entries.retain(|entry| {
        entry
            .get("hooks")
            .and_then(Value::as_array)
            .is_none_or(|hooks| !hooks.is_empty())
    });
    if entries.is_empty() {
        hooks.remove(definition.event);
    }
    Ok(())
}

/// Reconcile the repository-local MCP registration, replacing only the
/// `mcpServers.lattice` entry.  The executable is an injected absolute path;
/// callers should obtain it from `current_exe()` rather than PATH.
pub(crate) fn reconcile_mcp_config(
    config: &mut Value,
    executable: &Path,
    workspace_roots: &[PathBuf],
) -> Result<()> {
    if !executable.is_absolute() {
        bail!(
            "MCP executable path must be absolute, got `{}`",
            executable.display()
        );
    }
    if workspace_roots.is_empty() {
        bail!("MCP installation requires at least one workspace root");
    }
    if let Some(root) = workspace_roots.iter().find(|root| !root.is_absolute()) {
        bail!("workspace root must be absolute, got `{}`", root.display());
    }

    let root = object_mut(config, "MCP configuration")?;
    let servers = object_field_mut(root, "mcpServers", "MCP configuration")?;
    let mut args = Vec::with_capacity(1 + workspace_roots.len() * 2);
    args.push(Value::String("--stdio".into()));
    for workspace in workspace_roots {
        args.push(Value::String("--workspace".into()));
        args.push(Value::String(workspace.to_string_lossy().into_owned()));
    }
    servers.insert(
        "lattice".into(),
        json!({
            "type": "stdio",
            "command": executable.to_string_lossy(),
            "args": args,
        }),
    );
    Ok(())
}

pub(crate) fn render_config(config: &Value) -> Result<String> {
    serde_json::to_string_pretty(config)
        .context("serialize reconciled installation configuration")
        .map(|text| format!("{text}\n"))
}

fn hook_timeout_secs(client: HookClient, definition: HookDefinition) -> u64 {
    if client == HookClient::ClaudeCode && definition.event == "SessionEnd" {
        5
    } else {
        definition.timeout_secs
    }
}

fn validate_outer_timeout(client: HookClient, definition: HookDefinition) -> Result<()> {
    let outer_timeout_secs = hook_timeout_secs(client, definition);
    let terminal_adapter_only = client == HookClient::Codex && definition.event == "SessionEnd";
    if (!terminal_adapter_only && outer_timeout_secs <= MAX_INNER_HOOK_TIMEOUT_SECS)
        || (terminal_adapter_only && outer_timeout_secs <= 2)
    {
        bail!(
            "hook timeout invariant violated for {}: outer timeout ({}s) is too short",
            definition.event,
            outer_timeout_secs,
        );
    }
    Ok(())
}

fn reconcile_hook(
    config: &mut Value,
    client: HookClient,
    paths: &InstallPaths,
    definition: HookDefinition,
    mode: HookMode,
) -> Result<()> {
    let matcher = definition.matcher_for(mode);
    let command = paths.hook_path(client, definition.script);
    let timeout_secs = hook_timeout_secs(client, definition);
    let desired_hook = json!({
        "type": "command",
        "command": command.to_string_lossy(),
        "timeout": timeout_secs,
        "statusMessage": definition.status_message,
    });
    let root = object_mut(config, "hook configuration")?;
    let hooks = object_field_mut(root, "hooks", "hook configuration")?;
    let entries = array_field_mut(hooks, definition.event, "hook configuration")?;

    let mut primary = None;
    for (entry_index, entry) in entries.iter_mut().enumerate() {
        let Some(entry_object) = entry.as_object_mut() else {
            continue;
        };
        let Some(entry_hooks) = entry_object.get("hooks").and_then(Value::as_array) else {
            continue;
        };
        let mut matches = Vec::new();
        for (hook_index, hook) in entry_hooks.iter().enumerate() {
            if hook_command_basename(hook).as_deref() == Some(definition.script) {
                matches.push(hook_index);
            }
        }
        if matches.is_empty() {
            continue;
        }
        if primary.is_none() {
            let first = matches.remove(0);
            entry_object
                .get_mut("hooks")
                .and_then(Value::as_array_mut)
                .expect("hook array was checked above")[first] = desired_hook.clone();
            set_matcher(entry_object, matcher);
            primary = Some(entry_index);
        }
        for hook_index in matches.into_iter().rev() {
            entry_object
                .get_mut("hooks")
                .and_then(Value::as_array_mut)
                .expect("hook array was checked above")
                .remove(hook_index);
        }
    }

    if primary.is_none() {
        let mut entry = Map::new();
        set_matcher(&mut entry, matcher);
        entry.insert("hooks".into(), Value::Array(vec![desired_hook]));
        entries.push(Value::Object(entry));
    }
    entries.retain(|entry| {
        entry
            .as_object()
            .and_then(|entry| entry.get("hooks"))
            .and_then(Value::as_array)
            .is_none_or(|hooks| !hooks.is_empty())
    });
    Ok(())
}

fn hook_command_basename(hook: &Value) -> Option<String> {
    hook.as_object()?
        .get("command")?
        .as_str()
        .and_then(|command| Path::new(command).file_name())
        .and_then(|name| name.to_str())
        .map(str::to_owned)
}

fn set_matcher(entry: &mut Map<String, Value>, matcher: Option<&str>) {
    match matcher {
        Some(matcher) => {
            entry.insert("matcher".into(), Value::String(matcher.into()));
        }
        None => {
            entry.remove("matcher");
        }
    }
}

fn object_mut<'a>(value: &'a mut Value, label: &str) -> Result<&'a mut Map<String, Value>> {
    value
        .as_object_mut()
        .ok_or_else(|| anyhow!("{label} must be a JSON object"))
}

fn object_field_mut<'a>(
    parent: &'a mut Map<String, Value>,
    key: &str,
    label: &str,
) -> Result<&'a mut Map<String, Value>> {
    let value = parent
        .entry(key.to_owned())
        .or_insert_with(|| Value::Object(Map::new()));
    value
        .as_object_mut()
        .ok_or_else(|| anyhow!("{label}.{key} must be a JSON object"))
}

fn array_field_mut<'a>(
    parent: &'a mut Map<String, Value>,
    key: &str,
    label: &str,
) -> Result<&'a mut Vec<Value>> {
    let value = parent
        .entry(key.to_owned())
        .or_insert_with(|| Value::Array(Vec::new()));
    value
        .as_array_mut()
        .ok_or_else(|| anyhow!("{label}.{key} must be a JSON array"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn paths() -> InstallPaths {
        InstallPaths::new(
            PathBuf::from("/opt/lattice/bin/lattice"),
            PathBuf::from("/opt/lattice"),
        )
        .unwrap()
    }

    #[test]
    fn reconciles_stale_codex_entries_idempotently_and_preserves_foreign_hooks() {
        let mut config = json!({
            "hooks": {
                "UserPromptSubmit": [
                    {"matcher": "old", "hooks": [
                        {"type": "command", "command": "/old/hooks/user-prompt-submit.sh", "timeout": 1},
                        {"type": "command", "command": "custom-hook"}
                    ]},
                    {"hooks": [{"type": "command", "command": "/another/user-prompt-submit.sh"}]}
                ],
                "Stop": [
                    {"hooks": [
                        {"type": "command", "command": "/old/integrations/codex/hooks/stop.sh"},
                        {"type": "command", "command": "foreign-stop.sh"}
                    ]}
                ],
                "Custom": [{"hooks": [{"command": "custom-hook"}]}]
            }
        });

        reconcile_hook_config(&mut config, HookClient::Codex, &paths(), HookMode::BestEffort)
            .unwrap();
        let once = render_config(&config).unwrap();
        reconcile_hook_config(&mut config, HookClient::Codex, &paths(), HookMode::BestEffort)
            .unwrap();

        assert_eq!(once, render_config(&config).unwrap());
        assert_eq!(
            config["hooks"]["UserPromptSubmit"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|entry| entry["hooks"].as_array().unwrap())
                .filter(
                    |hook| hook_command_basename(hook).as_deref() == Some("user-prompt-submit.sh")
                )
                .count(),
            1
        );
        assert_eq!(
            config["hooks"]["UserPromptSubmit"][0]["hooks"][1]["command"],
            "custom-hook"
        );
        assert_eq!(config["hooks"]["SessionStart"][0]["hooks"][0]["timeout"], 5);
        assert_eq!(config["hooks"]["SessionEnd"][0]["hooks"][0]["timeout"], 3);
        let stop_commands = config["hooks"]["Stop"][0]["hooks"].as_array().unwrap();
        assert_eq!(stop_commands.len(), 2);
        assert!(stop_commands
            .iter()
            .any(|hook| { hook["command"] == "/opt/lattice/integrations/codex/hooks/stop.sh" }));
        assert!(stop_commands
            .iter()
            .any(|hook| hook["command"] == "foreign-stop.sh"));
        assert_eq!(
            config["hooks"]["Custom"][0]["hooks"][0]["command"],
            "custom-hook"
        );
    }

    #[test]
    fn replaces_stale_stop_asset_for_codex_and_claude_idempotently() {
        for client in [HookClient::Codex, HookClient::ClaudeCode] {
            let stale_client = client.asset_directory();
            let mut config = json!({
                "hooks": {
                    "Stop": [{"hooks": [{
                        "type": "command",
                        "command": format!("/old/integrations/{stale_client}/hooks/stop.sh")
                    }] }]
                }
            });

            reconcile_hook_config(&mut config, client, &paths(), HookMode::BestEffort).unwrap();
            let once = render_config(&config).unwrap();
            assert_eq!(
                config["hooks"]["Stop"][0]["hooks"][0]["command"],
                format!("/opt/lattice/integrations/{stale_client}/hooks/stop.sh")
            );

            reconcile_hook_config(&mut config, client, &paths(), HookMode::BestEffort).unwrap();
            assert_eq!(once, render_config(&config).unwrap());
        }
    }

    #[test]
    fn fills_empty_stop_event_container_without_removing_foreign_stop_hooks() {
        let mut empty_config = json!({"hooks": {"Stop": []}});
        reconcile_hook_config(
            &mut empty_config,
            HookClient::Codex,
            &paths(),
            HookMode::BestEffort,
        )
        .unwrap();
        assert_eq!(
            empty_config["hooks"]["Stop"][0]["hooks"][0]["command"],
            "/opt/lattice/integrations/codex/hooks/stop.sh"
        );

        let mut foreign_config = json!({
            "hooks": {
                "Stop": [{"hooks": [{
                    "type": "command",
                    "command": "foreign-stop.sh"
                }]}]
            }
        });
        reconcile_hook_config(
            &mut foreign_config,
            HookClient::Codex,
            &paths(),
            HookMode::BestEffort,
        )
        .unwrap();
        let commands = foreign_config["hooks"]["Stop"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|entry| entry["hooks"].as_array().unwrap())
            .collect::<Vec<_>>();
        assert!(commands
            .iter()
            .any(|hook| hook["command"] == "foreign-stop.sh"));
        assert!(commands
            .iter()
            .any(|hook| { hook["command"] == "/opt/lattice/integrations/codex/hooks/stop.sh" }));
    }

    fn lattice_commands(config: &Value, event: &str) -> Vec<String> {
        config["hooks"][event]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|entry| entry["hooks"].as_array().cloned().unwrap_or_default())
            .filter_map(|hook| hook["command"].as_str().map(str::to_owned))
            .collect()
    }

    #[test]
    fn enforce_on_adds_the_gate_and_widens_post_tool_use_for_both_clients() {
        for client in [HookClient::ClaudeCode, HookClient::Codex] {
            let directory = client.asset_directory();
            let mut config = json!({});
            reconcile_hook_config(&mut config, client, &paths(), HookMode::Enforcing).unwrap();
            assert_eq!(
                lattice_commands(&config, "PreToolUse"),
                vec![format!("/opt/lattice/integrations/{directory}/hooks/pre-tool-use.sh")]
            );
            assert_eq!(
                config["hooks"]["PreToolUse"][0]["matcher"],
                "apply_patch|Edit|Write|MultiEdit|NotebookEdit"
            );
            assert_eq!(config["hooks"]["PreToolUse"][0]["hooks"][0]["timeout"], 5);
            assert_eq!(
                config["hooks"]["PostToolUse"][0]["matcher"],
                "apply_patch|Edit|Write|MultiEdit|NotebookEdit|Bash|PowerShell"
            );
            // The gate never matches a shell tool: a shell command cannot be
            // classified without reading it.
            assert!(!config["hooks"]["PreToolUse"][0]["matcher"]
                .as_str()
                .unwrap()
                .contains("Bash"));

            let once = render_config(&config).unwrap();
            reconcile_hook_config(&mut config, client, &paths(), HookMode::Enforcing).unwrap();
            assert_eq!(once, render_config(&config).unwrap(), "rerun must be idempotent");
        }
    }

    #[test]
    fn enforce_off_removes_only_the_lattice_gate_and_restores_the_narrow_matcher() {
        let mut config = json!({
            "hooks": {
                "PreToolUse": [
                    {"matcher": "Bash", "hooks": [
                        {"type": "command", "command": "/team/guard-rm.sh"}
                    ]},
                    {"matcher": "Edit", "hooks": [
                        {"type": "command", "command": "/team/lint-before-edit.sh"},
                        {"type": "command", "command": "/stale/checkout/hooks/pre-tool-use.sh"}
                    ]}
                ]
            }
        });
        reconcile_hook_config(
            &mut config,
            HookClient::ClaudeCode,
            &paths(),
            HookMode::Enforcing,
        )
        .unwrap();
        // The stale registration is replaced in place; foreign hooks survive.
        assert_eq!(
            lattice_commands(&config, "PreToolUse"),
            vec![
                "/team/guard-rm.sh".to_string(),
                "/team/lint-before-edit.sh".to_string(),
                "/opt/lattice/integrations/claude-code/hooks/pre-tool-use.sh".to_string(),
            ]
        );

        reconcile_hook_config(
            &mut config,
            HookClient::ClaudeCode,
            &paths(),
            HookMode::BestEffort,
        )
        .unwrap();
        assert_eq!(
            lattice_commands(&config, "PreToolUse"),
            vec![
                "/team/guard-rm.sh".to_string(),
                "/team/lint-before-edit.sh".to_string()
            ]
        );
        assert_eq!(config["hooks"]["PreToolUse"][0]["matcher"], "Bash");
        assert_eq!(
            config["hooks"]["PostToolUse"][0]["matcher"],
            "apply_patch|Edit|Write"
        );
        let once = render_config(&config).unwrap();
        reconcile_hook_config(
            &mut config,
            HookClient::ClaudeCode,
            &paths(),
            HookMode::BestEffort,
        )
        .unwrap();
        assert_eq!(once, render_config(&config).unwrap());

        // With no foreign hook left, the event container goes too.
        let mut only_ours = json!({});
        reconcile_hook_config(
            &mut only_ours,
            HookClient::Codex,
            &paths(),
            HookMode::Enforcing,
        )
        .unwrap();
        reconcile_hook_config(
            &mut only_ours,
            HookClient::Codex,
            &paths(),
            HookMode::BestEffort,
        )
        .unwrap();
        assert!(only_ours["hooks"].get("PreToolUse").is_none());
        let mut best_effort = json!({});
        reconcile_hook_config(
            &mut best_effort,
            HookClient::Codex,
            &paths(),
            HookMode::BestEffort,
        )
        .unwrap();
        assert_eq!(
            render_config(&only_ours).unwrap(),
            render_config(&best_effort).unwrap()
        );
    }

    #[test]
    fn duplicate_gate_registrations_collapse_to_one() {
        let mut config = json!({
            "hooks": {"PreToolUse": [
                {"matcher": "Edit", "hooks": [
                    {"type": "command", "command": "/a/pre-tool-use.sh", "timeout": 60},
                    {"type": "command", "command": "/b/pre-tool-use.sh"}
                ]},
                {"hooks": [{"type": "command", "command": "/c/pre-tool-use.sh"}]}
            ]}
        });
        reconcile_hook_config(
            &mut config,
            HookClient::ClaudeCode,
            &paths(),
            HookMode::Enforcing,
        )
        .unwrap();
        assert_eq!(lattice_commands(&config, "PreToolUse").len(), 1);
        assert_eq!(config["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
        assert_eq!(config["hooks"]["PreToolUse"][0]["hooks"][0]["timeout"], 5);
    }

    #[test]
    fn installed_hook_list_follows_the_mode() {
        let best_effort = installed_hooks(HookMode::BestEffort);
        assert_eq!(best_effort.len(), 5);
        assert!(!best_effort.contains(&("PreToolUse", "pre-tool-use.sh")));
        let enforcing = installed_hooks(HookMode::Enforcing);
        assert_eq!(enforcing.len(), 6);
        assert!(enforcing.contains(&("PreToolUse", "pre-tool-use.sh")));
        assert_eq!(HookMode::from_enforcing(true), HookMode::Enforcing);
        assert_eq!(HookMode::from_enforcing(false), HookMode::BestEffort);
    }

    #[test]
    fn claude_session_end_retains_query_safe_outer_timeout() {
        let mut config = json!({});
        reconcile_hook_config(
            &mut config,
            HookClient::ClaudeCode,
            &paths(),
            HookMode::BestEffort,
        )
        .unwrap();
        assert_eq!(config["hooks"]["SessionEnd"][0]["hooks"][0]["timeout"], 5);
    }

    #[test]
    fn replaces_mcp_server_without_changing_other_servers() {
        let mut config = json!({
            "mcpServers": {
                "lattice": {"command": "/stale/lattice", "args": ["--stdio"]},
                "other": {"command": "other-mcp"}
            }
        });
        reconcile_mcp_config(
            &mut config,
            Path::new("/opt/lattice/bin/lattice"),
            &[PathBuf::from("/workspace/a"), PathBuf::from("/workspace/b")],
        )
        .unwrap();

        assert_eq!(config["mcpServers"]["other"]["command"], "other-mcp");
        assert_eq!(
            config["mcpServers"]["lattice"]["command"],
            "/opt/lattice/bin/lattice"
        );
        assert_eq!(
            config["mcpServers"]["lattice"]["args"],
            json!([
                "--stdio",
                "--workspace",
                "/workspace/a",
                "--workspace",
                "/workspace/b"
            ])
        );
    }

    #[test]
    fn rejects_invalid_shapes_and_timeout_invariant_violations() {
        let paths = paths();
        let mut invalid = json!({"hooks": []});
        let error =
            reconcile_hook_config(&mut invalid, HookClient::ClaudeCode, &paths, HookMode::BestEffort)
            .unwrap_err()
            .to_string();
        assert!(error.contains("hook configuration.hooks must be a JSON object"));

        let mut invalid_timeout = HOOKS[0];
        invalid_timeout.timeout_secs = MAX_INNER_HOOK_TIMEOUT_SECS;
        let error = validate_outer_timeout(HookClient::Codex, invalid_timeout)
            .unwrap_err()
            .to_string();
        assert!(error.contains("outer timeout (4s) is too short"));
    }

    #[test]
    fn rejects_relative_injected_paths() {
        assert!(InstallPaths::new(PathBuf::from("lattice"), PathBuf::from("/assets")).is_err());
        assert!(InstallPaths::new(PathBuf::from("/bin/lattice"), PathBuf::from("assets")).is_err());
    }
}
