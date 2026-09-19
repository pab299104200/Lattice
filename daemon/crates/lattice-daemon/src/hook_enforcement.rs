//! Workspace-scoped hook enforcement: policy, path classification, plan
//! freshness, and the bounded agent-facing wording.
//!
//! Everything here is deterministic and free of daemon or host I/O apart from
//! reading and writing the one policy file, so each decision is unit-testable.
//! The authoritative description is `docs/hook-enforcement.md`.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Map, Value};
use std::path::{Component, Path, PathBuf};

/// Checkout-relative location of the per-workspace policy. It lives beside
/// the other checkout-local Lattice state and is never read from global
/// configuration, so one repository opting in cannot change another.
pub(crate) const POLICY_RELATIVE_PATH: &str = ".lattice/workspace-policy.json";
pub(crate) const POLICY_SCHEMA_VERSION: u64 = 1;
const MAX_POLICY_BYTES: u64 = 64 * 1024;

/// A plan keeps covering edits while work under it continues. It lapses after
/// this much time with neither a new plan nor a covered product edit.
pub(crate) const PLAN_IDLE_WINDOW_MS: i64 = 45 * 60 * 1_000;
/// A plan never covers edits for longer than this, however busy the session.
pub(crate) const PLAN_ABSOLUTE_WINDOW_MS: i64 = 8 * 60 * 60 * 1_000;

/// Upper bound on shell-changed paths forwarded for one tool call.
pub(crate) const MAX_SHELL_CHANGED_PATHS: usize = 20;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct WorkspacePolicy {
    pub(crate) hook_enforcement: bool,
}

/// An unreadable policy is its own state. Treating it as "off" would silently
/// disable a control the operator turned on; treating it as "on" could block
/// work on the strength of a corrupt file. Callers fail open and say so.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PolicyState {
    Absent,
    Loaded(WorkspacePolicy),
    Unreadable(String),
}

impl PolicyState {
    pub(crate) fn enforcing(&self) -> bool {
        matches!(self, Self::Loaded(policy) if policy.hook_enforcement)
    }
}

pub(crate) fn policy_path(checkout_root: &Path) -> PathBuf {
    checkout_root.join(POLICY_RELATIVE_PATH)
}

pub(crate) fn load_policy(checkout_root: &Path) -> PolicyState {
    let path = policy_path(checkout_root);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return PolicyState::Absent,
        Err(error) => return PolicyState::Unreadable(format!("cannot stat policy: {error}")),
    };
    if !metadata.is_file() {
        return PolicyState::Unreadable("policy is not a regular file".into());
    }
    if metadata.len() > MAX_POLICY_BYTES {
        return PolicyState::Unreadable("policy exceeds the size limit".into());
    }
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => return PolicyState::Unreadable(format!("cannot read policy: {error}")),
    };
    match parse_policy(&text) {
        Ok(policy) => PolicyState::Loaded(policy),
        Err(error) => PolicyState::Unreadable(format!("{error:#}")),
    }
}

fn parse_policy(text: &str) -> Result<WorkspacePolicy> {
    let value: Value = serde_json::from_str(text).context("policy is not valid JSON")?;
    let object = value
        .as_object()
        .ok_or_else(|| anyhow!("policy must be a JSON object"))?;
    match object.get("schema_version").and_then(Value::as_u64) {
        Some(POLICY_SCHEMA_VERSION) => {}
        Some(other) => bail!("policy schema_version {other} is not supported"),
        None => bail!("policy schema_version is missing"),
    }
    let hook_enforcement = match object.get("hook_enforcement") {
        None => false,
        Some(section) => section
            .as_object()
            .ok_or_else(|| anyhow!("policy hook_enforcement must be an object"))?
            .get("enabled")
            .map(|enabled| {
                enabled
                    .as_bool()
                    .ok_or_else(|| anyhow!("policy hook_enforcement.enabled must be a boolean"))
            })
            .transpose()?
            .unwrap_or(false),
    };
    Ok(WorkspacePolicy { hook_enforcement })
}

/// Record the enforcement mode, preserving any other section of an existing
/// valid policy. A corrupt existing policy is an error rather than something
/// to overwrite: the operator may have put other controls in it.
pub(crate) fn write_policy(checkout_root: &Path, hook_enforcement: bool) -> Result<PathBuf> {
    let path = policy_path(checkout_root);
    let mut document = match std::fs::read_to_string(&path) {
        Ok(text) => {
            parse_policy(&text)
                .with_context(|| format!("existing policy `{}` is invalid", path.display()))?;
            match serde_json::from_str::<Value>(&text)? {
                Value::Object(object) => object,
                _ => unreachable!("parse_policy accepted a non-object policy"),
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Map::new(),
        Err(error) => {
            return Err(error).with_context(|| format!("read policy `{}`", path.display()))
        }
    };
    document.insert("schema_version".into(), json!(POLICY_SCHEMA_VERSION));
    let section = document
        .entry("hook_enforcement".to_owned())
        .or_insert_with(|| Value::Object(Map::new()));
    section
        .as_object_mut()
        .expect("parse_policy validated the section shape")
        .insert("enabled".into(), Value::Bool(hook_enforcement));

    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("policy path has no parent"))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("create policy directory `{}`", parent.display()))?;
    let staged = path.with_extension("json.lattice-install-tmp");
    let rendered = format!(
        "{}\n",
        serde_json::to_string_pretty(&Value::Object(document))?
    );
    std::fs::write(&staged, rendered)
        .with_context(|| format!("write staged policy `{}`", staged.display()))?;
    std::fs::rename(&staged, &path)
        .with_context(|| format!("replace policy `{}`", path.display()))?;
    Ok(path)
}

/// Why an edit target is, or is not, subject to the plan gate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PathClass {
    /// Inside the workspace and none of the exemptions apply.
    Product,
    Documentation,
    /// `.lattice/`, the client configuration directories and `.git/`.
    ToolState,
    Scratch,
    OutsideWorkspace,
}

impl PathClass {
    pub(crate) fn gated(self) -> bool {
        self == Self::Product
    }
}

const TOOL_STATE_DIRECTORIES: [&str; 4] = [".lattice", ".claude", ".codex", ".git"];
const SCRATCH_DIRECTORIES: [&str; 5] = ["tmp", "temp", ".tmp", "scratch", ".scratch"];
const DOCUMENTATION_EXTENSIONS: [&str; 5] = ["md", "mdx", "markdown", "rst", "adoc"];

/// Classify an edit target. `raw` may be absolute (Claude Code always sends
/// absolute paths) or checkout-relative. `scratch_directory` is the host's
/// dedicated scratchpad field when it supplies one.
pub(crate) fn classify_edit_path(
    checkout_root: &Path,
    raw: &str,
    scratch_directory: Option<&Path>,
) -> PathClass {
    let normalized = raw.replace('\\', "/");
    let candidate = Path::new(&normalized);
    let absolute = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        checkout_root.join(candidate)
    };
    let resolved = resolve_through_existing_ancestor(&absolute);
    if let Some(scratch) = scratch_directory {
        if resolved.starts_with(resolve_through_existing_ancestor(scratch)) {
            return PathClass::Scratch;
        }
    }
    let Ok(relative) = resolved.strip_prefix(checkout_root) else {
        return PathClass::OutsideWorkspace;
    };
    classify_relative_path(relative)
}

/// The checkout-relative, `/`-separated form of an edit target, or `None`
/// when it resolves outside the checkout. Claude Code always sends absolute
/// paths, while capture facts and presentations are repository-relative.
pub(crate) fn checkout_relative_path(checkout_root: &Path, raw: &str) -> Option<String> {
    let normalized = raw.replace('\\', "/");
    let candidate = Path::new(&normalized);
    let absolute = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        checkout_root.join(candidate)
    };
    let resolved = resolve_through_existing_ancestor(&absolute);
    let relative = resolved.strip_prefix(checkout_root).ok()?;
    let parts = relative
        .components()
        .map(|component| match component {
            Component::Normal(part) => part.to_str(),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// Classify a path already known to be checkout-relative, such as a `git
/// status` entry.
pub(crate) fn classify_relative_path(relative: &Path) -> PathClass {
    let mut components = relative.components();
    let Some(Component::Normal(first)) = components.next() else {
        return PathClass::OutsideWorkspace;
    };
    let first = first.to_string_lossy();
    if TOOL_STATE_DIRECTORIES.contains(&first.as_ref()) {
        return PathClass::ToolState;
    }
    if SCRATCH_DIRECTORIES.contains(&first.as_ref()) {
        return PathClass::Scratch;
    }
    let has_more = components.next().is_some();
    if first == "docs" && has_more {
        return PathClass::Documentation;
    }
    let documentation_extension = relative
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            DOCUMENTATION_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str())
        });
    if documentation_extension {
        return PathClass::Documentation;
    }
    PathClass::Product
}

/// Resolve symlinks in the part of the path that exists, then re-attach the
/// rest lexically. A file about to be created has no canonical form, yet
/// `/tmp/x` must still compare equal to `/private/tmp/x` on macOS, and `..`
/// must not let a path escape its classification.
fn resolve_through_existing_ancestor(path: &Path) -> PathBuf {
    let mut lexical = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                lexical.pop();
            }
            Component::CurDir => {}
            other => lexical.push(other.as_os_str()),
        }
    }
    let mut existing = lexical.as_path();
    let mut remainder = Vec::new();
    loop {
        if let Ok(canonical) = existing.canonicalize() {
            let mut resolved = canonical;
            for part in remainder.iter().rev() {
                resolved.push(part);
            }
            return resolved;
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                remainder.push(name.to_os_string());
                existing = parent;
            }
            _ => return lexical,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PlanStaleReason {
    /// No plan and no covered edit within `PLAN_IDLE_WINDOW_MS`.
    Idle,
    /// The plan is older than `PLAN_ABSOLUTE_WINDOW_MS`.
    Expired,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PlanState {
    Current,
    Missing,
    Stale(PlanStaleReason),
}

impl PlanState {
    pub(crate) fn wire(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Missing => "missing",
            Self::Stale(PlanStaleReason::Idle) => "stale-idle",
            Self::Stale(PlanStaleReason::Expired) => "stale-expired",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "current" => Some(Self::Current),
            "missing" => Some(Self::Missing),
            "stale-idle" => Some(Self::Stale(PlanStaleReason::Idle)),
            "stale-expired" => Some(Self::Stale(PlanStaleReason::Expired)),
            _ => None,
        }
    }
}

/// The freshness rule, in one place.
///
/// * `latest_plan_at_ms` is the newest successful `prepare_change` recorded by
///   the daemon for this checkout at or after `not_before_ms`.
/// * `not_before_ms` is when this host session last started with a new
///   context (`startup`, `clear` or `compact`). A plan the model can no longer
///   see does not cover its edits.
/// * `last_covered_edit_at_ms` is the last product edit this session made
///   under a current plan. It slides the idle window, which is what stops a
///   long multi-file change from being asked to re-plan on every edit.
pub(crate) fn evaluate_plan(
    now_ms: i64,
    latest_plan_at_ms: Option<i64>,
    last_covered_edit_at_ms: Option<i64>,
    not_before_ms: i64,
) -> PlanState {
    let Some(plan_at) = latest_plan_at_ms.filter(|plan_at| *plan_at >= not_before_ms) else {
        return PlanState::Missing;
    };
    if now_ms.saturating_sub(plan_at) > PLAN_ABSOLUTE_WINDOW_MS {
        return PlanState::Stale(PlanStaleReason::Expired);
    }
    let last_activity = last_covered_edit_at_ms
        .filter(|edit_at| *edit_at >= plan_at)
        .map_or(plan_at, |edit_at| edit_at.max(plan_at));
    if now_ms.saturating_sub(last_activity) > PLAN_IDLE_WINDOW_MS {
        return PlanState::Stale(PlanStaleReason::Idle);
    }
    PlanState::Current
}

/// What the daemon can say about a workspace index without loading it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum IndexState {
    Ready,
    /// No shard is loaded. The first Lattice call loads it; this is normal.
    NotLoaded,
    Indexing,
    Deferred,
    Failed,
}

impl IndexState {
    pub(crate) fn wire(&self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::NotLoaded => "not-loaded",
            Self::Indexing => "indexing",
            Self::Deferred => "deferred",
            Self::Failed => "failed",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "ready" => Some(Self::Ready),
            "not-loaded" => Some(Self::NotLoaded),
            "indexing" => Some(Self::Indexing),
            "deferred" => Some(Self::Deferred),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }

    /// True when Lattice cannot currently give a full answer, so the agent
    /// must be told rather than blocked.
    pub(crate) fn degraded(&self) -> bool {
        matches!(self, Self::Indexing | Self::Deferred | Self::Failed)
    }
}

/// Conditions that produce exactly one agent-visible notice per session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NoticeCondition {
    DaemonUnreachable,
    DaemonRejected,
    AdapterTimeout,
    IndexIndexing,
    IndexDeferred,
    IndexFailed,
    PolicyUnreadable,
    ShellDetectionDegraded,
    ShellEditWithoutPlan,
    CaptureUnavailable,
    AdapterFailed,
}

impl NoticeCondition {
    /// Stable, content-free marker prefix. `session-start` predates
    /// enforcement and is kept so an upgrade does not repeat that notice.
    pub(crate) fn slug(self) -> &'static str {
        match self {
            Self::DaemonUnreachable => "session-start",
            Self::DaemonRejected => "daemon-rejected",
            Self::AdapterTimeout => "adapter-timeout",
            Self::IndexIndexing => "index-indexing",
            Self::IndexDeferred => "index-deferred",
            Self::IndexFailed => "index-failed",
            Self::PolicyUnreadable => "policy-unreadable",
            Self::ShellDetectionDegraded => "shell-detection-degraded",
            Self::ShellEditWithoutPlan => "shell-edit-without-plan",
            Self::CaptureUnavailable => "capture-unavailable",
            Self::AdapterFailed => "adapter-failed",
        }
    }

    pub(crate) fn for_index(state: &IndexState) -> Option<Self> {
        match state {
            IndexState::Indexing => Some(Self::IndexIndexing),
            IndexState::Deferred => Some(Self::IndexDeferred),
            IndexState::Failed => Some(Self::IndexFailed),
            IndexState::Ready | IndexState::NotLoaded => None,
        }
    }
}

/// The pre-enforcement SessionStart wording, unchanged for workspaces that
/// have not opted in.
pub(crate) const BEST_EFFORT_DAEMON_NOTICE: &str =
    "lattice: daemon unreachable — run 'lattice doctor'";

const REPORT_CLAUSE: &str =
    "Workspace policy enforces the Lattice workflow, so state in your report that Lattice was \
     unavailable and why.";

/// Bounded, fixed wording. No host, path or daemon text is interpolated, so a
/// notice can never carry captured content.
pub(crate) fn notice_text(condition: NoticeCondition) -> String {
    let what = match condition {
        NoticeCondition::DaemonUnreachable => {
            "Lattice daemon is unreachable, so plans, impact and session capture are off. \
             Edits are not blocked. Run `lattice doctor`."
        }
        NoticeCondition::DaemonRejected => {
            "Lattice daemon refused the enforcement request, most likely because it predates \
             enforcement or the session binding is invalid. Edits are not blocked. Restart the \
             daemon, then run `lattice doctor`."
        }
        NoticeCondition::AdapterTimeout => {
            "Lattice did not answer within its two-second hook deadline. This edit was not \
             checked and is not blocked. Run `lattice status --timeout 2`."
        }
        NoticeCondition::IndexIndexing => {
            "Lattice is still indexing this workspace, so plan and impact answers are partial. \
             Edits are not blocked. Check progress with `lattice status`."
        }
        NoticeCondition::IndexDeferred => {
            "Lattice could not load this workspace: the daemon is at its memory budget or its \
             configured shard ceiling, and every loaded workspace is in use. Plan and impact \
             answers are partial. Edits are not blocked. Run `lattice status` for the reason \
             and what to raise."
        }
        NoticeCondition::IndexFailed => {
            "Lattice failed to load this workspace index, so plan and impact answers are \
             unavailable. Edits are not blocked. Run `lattice doctor`."
        }
        NoticeCondition::PolicyUnreadable => {
            "Lattice cannot read .lattice/workspace-policy.json, so enforcement is off for this \
             session. Repair it with `lattice install claude-code --workspace <dir> --enforce`."
        }
        NoticeCondition::ShellDetectionDegraded => {
            "Lattice could not list repository changes within its deadline, so shell-made edits \
             are not getting impact feedback. Run `lattice impact <file>` yourself for files \
             changed through the shell."
        }
        NoticeCondition::ShellEditWithoutPlan => {
            "Product files changed through the shell with no current Lattice plan on record. \
             This could not be blocked after the fact. Run `lattice prepare_change \"<task>\"` \
             now and check `lattice impact <file>` for each changed file."
        }
        NoticeCondition::CaptureUnavailable => {
            "Lattice session capture is not recording this session's turn summaries. Run \
             `lattice doctor`."
        }
        NoticeCondition::AdapterFailed => {
            "Lattice's hook adapter failed before it could reach the daemon, most likely because \
             its private state under ~/.local/state/lattice is unreadable. Edits are not \
             blocked. Run `lattice doctor`."
        }
    };
    match condition {
        NoticeCondition::ShellEditWithoutPlan => format!(
            "lattice: {what} Workspace policy enforces the Lattice workflow, so state this in \
             your report."
        ),
        NoticeCondition::PolicyUnreadable | NoticeCondition::CaptureUnavailable => {
            format!("lattice: {what} State this in your report.")
        }
        _ => format!("lattice: {what} {REPORT_CLAUSE}"),
    }
}

/// The two-line reason shown to the agent when the gate denies an edit: what
/// to run, and that workspace policy is what requires it.
pub(crate) fn deny_reason(state: PlanState, path_known: bool) -> String {
    let first = match state {
        PlanState::Missing => {
            "Lattice has no change plan for this session. Run `lattice prepare_change \"<the \
             task you are implementing>\"` (MCP tool `prepare_change`), then retry the edit."
        }
        PlanState::Stale(PlanStaleReason::Idle) => {
            "The last Lattice change plan lapsed after 45 minutes without a covered edit. Run \
             `lattice prepare_change \"<the task you are implementing>\"` (MCP tool \
             `prepare_change`), then retry the edit."
        }
        PlanState::Stale(PlanStaleReason::Expired) => {
            "The last Lattice change plan is more than 8 hours old. Run `lattice prepare_change \
             \"<the task you are implementing>\"` (MCP tool `prepare_change`), then retry the \
             edit."
        }
        PlanState::Current => unreachable!("a current plan never denies"),
    };
    let scope = if path_known {
        "Documentation, .lattice/, .claude/, scratch and out-of-workspace paths are exempt."
    } else {
        "This client does not expose the edit target, so every patch needs a current plan."
    };
    format!(
        "{first}\nEnforced by this workspace's Lattice policy \
         (.lattice/workspace-policy.json); one plan covers the whole multi-file change. {scope}"
    )
}

/// Which end-of-work steps the session skipped after editing product files.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct FollowupGaps {
    pub(crate) stale_docs: bool,
    pub(crate) remember: bool,
}

impl FollowupGaps {
    pub(crate) fn any(self) -> bool {
        self.stale_docs || self.remember
    }
}

pub(crate) fn followup_reminder(gaps: FollowupGaps) -> Option<String> {
    let steps = match (gaps.stale_docs, gaps.remember) {
        (false, false) => return None,
        (true, true) => {
            "the stale-docs check (`lattice status --scope docs --files <changed files>`, MCP \
             `status` with scope `docs`) and `lattice remember` for any verified, reusable \
             outcome"
        }
        (true, false) => {
            "the stale-docs check (`lattice status --scope docs --files <changed files>`, MCP \
             `status` with scope `docs`)"
        }
        (false, true) => "`lattice remember` for any verified, reusable outcome",
    };
    Some(format!(
        "lattice: this session edited product files but has not run {steps}. Workspace policy \
         asks for it before you finish; if it does not apply, say so in your report. This \
         reminder appears once per session."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "lattice-enforcement-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root.canonicalize().unwrap()
    }

    #[test]
    fn policy_round_trips_and_preserves_unrelated_sections() {
        let root = temp_root("policy");
        assert_eq!(load_policy(&root), PolicyState::Absent);
        assert!(!load_policy(&root).enforcing());

        write_policy(&root, true).unwrap();
        assert!(load_policy(&root).enforcing());

        let path = policy_path(&root);
        let mut document: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        document["other_control"] = json!({"keep": "me"});
        document["hook_enforcement"]["note"] = json!("operator note");
        std::fs::write(&path, document.to_string()).unwrap();

        write_policy(&root, false).unwrap();
        let rewritten: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(rewritten["other_control"]["keep"], "me");
        assert_eq!(rewritten["hook_enforcement"]["note"], "operator note");
        assert_eq!(rewritten["hook_enforcement"]["enabled"], false);
        assert_eq!(
            load_policy(&root),
            PolicyState::Loaded(WorkspacePolicy {
                hook_enforcement: false
            })
        );

        let once = std::fs::read_to_string(&path).unwrap();
        write_policy(&root, false).unwrap();
        assert_eq!(once, std::fs::read_to_string(&path).unwrap());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn corrupt_policy_is_unreadable_never_enforcing_and_never_overwritten() {
        let root = temp_root("corrupt");
        std::fs::create_dir_all(root.join(".lattice")).unwrap();
        for corrupt in [
            "{not json",
            "[]",
            r#"{"hook_enforcement":{"enabled":true}}"#,
            r#"{"schema_version":2,"hook_enforcement":{"enabled":true}}"#,
            r#"{"schema_version":1,"hook_enforcement":{"enabled":"yes"}}"#,
            r#"{"schema_version":1,"hook_enforcement":true}"#,
        ] {
            std::fs::write(policy_path(&root), corrupt).unwrap();
            let state = load_policy(&root);
            assert!(
                matches!(state, PolicyState::Unreadable(_)),
                "{corrupt} gave {state:?}"
            );
            assert!(!state.enforcing());
            assert!(write_policy(&root, true).is_err());
            assert_eq!(
                std::fs::read_to_string(policy_path(&root)).unwrap(),
                corrupt
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn classifies_product_documentation_tool_state_scratch_and_outside_paths() {
        let root = temp_root("classify");
        std::fs::create_dir_all(root.join("src")).unwrap();
        let absolute = |relative: &str| root.join(relative).to_string_lossy().into_owned();

        for (path, expected) in [
            (absolute("src/lib.rs"), PathClass::Product),
            (absolute("src/new/deep/file.rs"), PathClass::Product),
            ("src/lib.rs".to_string(), PathClass::Product),
            (absolute("requirements.txt"), PathClass::Product),
            (absolute("docs"), PathClass::Product),
            (absolute("docs/guide/setup.md"), PathClass::Documentation),
            (absolute("docs/diagram.svg"), PathClass::Documentation),
            (absolute("README.md"), PathClass::Documentation),
            (absolute("src/NOTES.MD"), PathClass::Documentation),
            (
                absolute(".lattice/workspace-policy.json"),
                PathClass::ToolState,
            ),
            (absolute(".claude/settings.json"), PathClass::ToolState),
            (absolute(".codex/hooks.json"), PathClass::ToolState),
            (absolute("tmp/probe.py"), PathClass::Scratch),
            (absolute(".scratch/x.rs"), PathClass::Scratch),
            ("/etc/hosts".to_string(), PathClass::OutsideWorkspace),
            (absolute("src/../../escape.rs"), PathClass::OutsideWorkspace),
            (absolute("docs/../src/lib.rs"), PathClass::Product),
        ] {
            assert_eq!(classify_edit_path(&root, &path, None), expected, "{path}");
        }
        assert!(PathClass::Product.gated());
        for exempt in [
            PathClass::Documentation,
            PathClass::ToolState,
            PathClass::Scratch,
            PathClass::OutsideWorkspace,
        ] {
            assert!(!exempt.gated());
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn absolute_edit_targets_become_checkout_relative_and_outside_paths_do_not() {
        let root = temp_root("relative");
        std::fs::create_dir_all(root.join("src")).unwrap();
        let absolute = root.join("src/lib.rs").to_string_lossy().into_owned();
        assert_eq!(
            checkout_relative_path(&root, &absolute).as_deref(),
            Some("src/lib.rs")
        );
        assert_eq!(
            checkout_relative_path(&root, "src/new/file.rs").as_deref(),
            Some("src/new/file.rs")
        );
        assert_eq!(
            checkout_relative_path(&root, "src/../src/./lib.rs").as_deref(),
            Some("src/lib.rs")
        );
        assert_eq!(checkout_relative_path(&root, "/etc/hosts"), None);
        assert_eq!(checkout_relative_path(&root, "../escape.rs"), None);
        assert_eq!(checkout_relative_path(&root, &root.to_string_lossy()), None);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn host_scratchpad_inside_the_workspace_is_exempt() {
        let root = temp_root("scratchpad");
        let scratch = root.join("work/session-scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        let target = scratch.join("probe.rs").to_string_lossy().into_owned();
        assert_eq!(classify_edit_path(&root, &target, None), PathClass::Product);
        assert_eq!(
            classify_edit_path(&root, &target, Some(&scratch)),
            PathClass::Scratch
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_cannot_disguise_a_product_path_or_an_outside_path() {
        let root = temp_root("symlink");
        let outside = temp_root("symlink-outside");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::os::unix::fs::symlink(root.join("src"), root.join("docs")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("vendor")).unwrap();
        let through_docs = root.join("docs/lib.rs").to_string_lossy().into_owned();
        assert_eq!(
            classify_edit_path(&root, &through_docs, None),
            PathClass::Product
        );
        let through_vendor = root.join("vendor/x.rs").to_string_lossy().into_owned();
        assert_eq!(
            classify_edit_path(&root, &through_vendor, None),
            PathClass::OutsideWorkspace
        );
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn one_plan_covers_a_long_multi_file_change_without_nagging() {
        let minute = 60_000;
        let plan_at = 1_000_000;
        assert_eq!(evaluate_plan(plan_at, None, None, 0), PlanState::Missing);
        assert_eq!(
            evaluate_plan(plan_at + minute, Some(plan_at), None, 0),
            PlanState::Current
        );
        // Edits every 40 minutes keep sliding the idle window for hours.
        let mut last_edit = None;
        let mut now = plan_at;
        for _ in 0..11 {
            now += 40 * minute;
            assert_eq!(
                evaluate_plan(now, Some(plan_at), last_edit, 0),
                PlanState::Current,
                "edit at +{} min",
                (now - plan_at) / minute
            );
            last_edit = Some(now);
        }
        // The absolute window still ends it.
        assert_eq!(
            evaluate_plan(
                plan_at + PLAN_ABSOLUTE_WINDOW_MS + 1,
                Some(plan_at),
                last_edit,
                0
            ),
            PlanState::Stale(PlanStaleReason::Expired)
        );
    }

    #[test]
    fn plan_lapses_when_idle_and_after_a_context_reset() {
        let plan_at = 5_000_000;
        assert_eq!(
            evaluate_plan(plan_at + PLAN_IDLE_WINDOW_MS, Some(plan_at), None, 0),
            PlanState::Current
        );
        assert_eq!(
            evaluate_plan(plan_at + PLAN_IDLE_WINDOW_MS + 1, Some(plan_at), None, 0),
            PlanState::Stale(PlanStaleReason::Idle)
        );
        // An edit made before the plan does not extend it.
        assert_eq!(
            evaluate_plan(
                plan_at + PLAN_IDLE_WINDOW_MS + 1,
                Some(plan_at),
                Some(plan_at - 1),
                0
            ),
            PlanState::Stale(PlanStaleReason::Idle)
        );
        // A plan from before the session's context was reset is not a plan.
        assert_eq!(
            evaluate_plan(plan_at + 1, Some(plan_at), None, plan_at + 1),
            PlanState::Missing
        );
        assert_eq!(
            evaluate_plan(plan_at + 1, Some(plan_at), None, plan_at),
            PlanState::Current
        );
    }

    #[test]
    fn wire_forms_round_trip() {
        for state in [
            PlanState::Current,
            PlanState::Missing,
            PlanState::Stale(PlanStaleReason::Idle),
            PlanState::Stale(PlanStaleReason::Expired),
        ] {
            assert_eq!(PlanState::parse(state.wire()), Some(state));
        }
        for state in [
            IndexState::Ready,
            IndexState::NotLoaded,
            IndexState::Indexing,
            IndexState::Deferred,
            IndexState::Failed,
        ] {
            assert_eq!(IndexState::parse(state.wire()), Some(state.clone()));
            assert_eq!(
                state.degraded(),
                NoticeCondition::for_index(&state).is_some()
            );
        }
        assert_eq!(PlanState::parse("allowed"), None);
        assert_eq!(IndexState::parse(""), None);
    }

    #[test]
    fn deny_reason_is_two_actionable_lines() {
        for state in [
            PlanState::Missing,
            PlanState::Stale(PlanStaleReason::Idle),
            PlanState::Stale(PlanStaleReason::Expired),
        ] {
            for path_known in [true, false] {
                let reason = deny_reason(state, path_known);
                let lines = reason.lines().collect::<Vec<_>>();
                assert_eq!(lines.len(), 2, "{reason}");
                assert!(lines[0].contains("lattice prepare_change"));
                assert!(lines[0].contains("retry the edit"));
                assert!(lines[1].contains("workspace's Lattice policy"));
                assert!(reason.len() < 700);
            }
        }
    }

    #[test]
    fn notices_are_bounded_distinct_and_say_what_to_report() {
        let conditions = [
            NoticeCondition::DaemonUnreachable,
            NoticeCondition::DaemonRejected,
            NoticeCondition::AdapterTimeout,
            NoticeCondition::IndexIndexing,
            NoticeCondition::IndexDeferred,
            NoticeCondition::IndexFailed,
            NoticeCondition::PolicyUnreadable,
            NoticeCondition::ShellDetectionDegraded,
            NoticeCondition::ShellEditWithoutPlan,
            NoticeCondition::CaptureUnavailable,
            NoticeCondition::AdapterFailed,
        ];
        let mut slugs = std::collections::BTreeSet::new();
        let mut texts = std::collections::BTreeSet::new();
        for condition in conditions {
            let text = notice_text(condition);
            assert!(text.starts_with("lattice: "));
            assert!(text.contains("report"), "{text}");
            assert!(text.len() < 600, "{text}");
            assert!(!text.contains('\n'));
            assert!(slugs.insert(condition.slug()));
            assert!(texts.insert(text));
        }
    }

    #[test]
    fn followup_reminder_names_only_the_missing_steps() {
        assert_eq!(followup_reminder(FollowupGaps::default()), None);
        let both = followup_reminder(FollowupGaps {
            stale_docs: true,
            remember: true,
        })
        .unwrap();
        assert!(both.contains("stale-docs") && both.contains("lattice remember"));
        let docs = followup_reminder(FollowupGaps {
            stale_docs: true,
            remember: false,
        })
        .unwrap();
        assert!(docs.contains("stale-docs") && !docs.contains("lattice remember"));
        let remember = followup_reminder(FollowupGaps {
            stale_docs: false,
            remember: true,
        })
        .unwrap();
        assert!(!remember.contains("stale-docs") && remember.contains("lattice remember"));
        assert!(both.contains("once per session"));
    }
}
