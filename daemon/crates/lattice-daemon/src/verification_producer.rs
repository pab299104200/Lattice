//! Explicit, repository-local producer for typed session verification facts.
//!
//! This private command executes a checkout-declared check without a shell,
//! discards every process stream, and hands only normalized categorical facts
//! to the installed hook adapter.

use lattice_core::memory::{
    parse_session_capture_event, CheckOutcome, ErrorStatus, SessionCaptureEvent,
    SessionCaptureFact, SESSION_CAPTURE_SCHEMA_VERSION,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use crate::workspace_identity::WorkspaceIdentity;

const CONFIG_RELATIVE_PATH: &str = ".lattice/verification-checks.json";
const MAX_CONFIG_BYTES: usize = 64 * 1024;
const MAX_CHECKS: usize = 64;
const MAX_ID_BYTES: usize = 64;
const MAX_ARGV_ITEMS: usize = 64;
const MAX_ARG_BYTES: usize = 4 * 1024;
const MAX_ARGV_BYTES: usize = 16 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct VerificationConfig {
    schema_version: u32,
    checks: Vec<DeclaredCheck>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeclaredCheck {
    id: String,
    label: String,
    argv: Vec<String>,
    #[serde(default)]
    error: Option<DeclaredError>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeclaredError {
    category: String,
    fingerprint: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProducerIntegration {
    Codex,
    ClaudeCode,
}

impl ProducerIntegration {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "codex" => Some(Self::Codex),
            "claude-code" => Some(Self::ClaudeCode),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::ClaudeCode => "claude-code",
        }
    }
}

#[derive(Debug)]
struct ProducerInvocation {
    integration: ProducerIntegration,
    host_session_id: String,
    check_id: String,
}

#[derive(Debug)]
struct CheckExecution {
    outcome: CheckOutcome,
    exit_code: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VerificationConfigHealth {
    Absent,
    Valid { checks: usize },
    Invalid,
}

pub(crate) fn is_verification_producer_command() -> bool {
    std::env::args().nth(1).as_deref() == Some("__hook-verify")
}

/// Invalid invocation, missing configuration, and unsafe declarations are
/// silent no-ops. Once the declared process runs, its status is preserved for
/// the caller; capture availability never changes that status.
pub(crate) async fn run_from_env() -> i32 {
    run_from_args(std::env::args().collect()).unwrap_or(0)
}

fn run_from_args(args: Vec<String>) -> Option<i32> {
    let invocation = parse_invocation(&args)?;
    let identity = checkout_identity_from_cwd()?;
    let check = load_declared_check(&identity.checkout_root, &invocation.check_id)?;
    let execution = execute_declared_check(&identity.checkout_root, &check);
    let events = declared_events(&check, execution.outcome)?;
    for event in events {
        let _ = emit_event(&identity.checkout_root, &invocation, &event);
    }
    Some(execution.exit_code)
}

fn parse_invocation(args: &[String]) -> Option<ProducerInvocation> {
    if args.len() != 5 || args.get(1).map(String::as_str) != Some("__hook-verify") {
        return None;
    }
    let integration = ProducerIntegration::parse(args.get(2)?)?;
    let host_session_id = args.get(3)?.clone();
    let check_id = args.get(4)?.clone();
    if !valid_opaque_session_id(&host_session_id) || !valid_check_id(&check_id) {
        return None;
    }
    Some(ProducerInvocation {
        integration,
        host_session_id,
        check_id,
    })
}

fn checkout_identity_from_cwd() -> Option<WorkspaceIdentity> {
    let cwd = std::env::current_dir().ok()?.canonicalize().ok()?;
    let root = cwd
        .ancestors()
        .find(|candidate| candidate.join(".git").exists())
        .or_else(|| {
            cwd.ancestors()
                .find(|candidate| candidate.join(".lattice").is_dir())
        })?;
    let identity = WorkspaceIdentity::resolve(root).ok()?;
    (identity.checkout_root == root.canonicalize().ok()?).then_some(identity)
}

fn load_declared_check(root: &Path, check_id: &str) -> Option<DeclaredCheck> {
    let config = load_config(root)?;
    config.checks.into_iter().find(|check| check.id == check_id)
}

pub(crate) fn verification_config_health(root: &Path) -> VerificationConfigHealth {
    let config_path = root.join(CONFIG_RELATIVE_PATH);
    match std::fs::symlink_metadata(&config_path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return VerificationConfigHealth::Absent;
        }
        Err(_) => return VerificationConfigHealth::Invalid,
        Ok(_) => {}
    }
    match load_config(root) {
        Some(config) => VerificationConfigHealth::Valid {
            checks: config.checks.len(),
        },
        None => VerificationConfigHealth::Invalid,
    }
}

fn load_config(root: &Path) -> Option<VerificationConfig> {
    let config_path = root.join(CONFIG_RELATIVE_PATH);
    let file = open_config(root, &config_path)?;
    let metadata = file.metadata().ok()?;
    let mut bytes = Vec::with_capacity((metadata.len() as usize).min(MAX_CONFIG_BYTES));
    file.take((MAX_CONFIG_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > MAX_CONFIG_BYTES {
        return None;
    }
    let config: VerificationConfig = serde_json::from_slice(&bytes).ok()?;
    validate_config(&config, root)?;
    Some(config)
}

#[cfg(unix)]
fn open_config(root: &Path, config_path: &Path) -> Option<File> {
    let lattice_dir = root.join(".lattice");
    let directory = std::fs::symlink_metadata(&lattice_dir).ok()?;
    if !directory.is_dir()
        || directory.file_type().is_symlink()
        || directory.uid() != unsafe { libc::geteuid() }
        || directory.mode() & 0o022 != 0
    {
        return None;
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(config_path)
        .ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o022 != 0
        || metadata.nlink() != 1
    {
        return None;
    }
    Some(file)
}

#[cfg(not(unix))]
fn open_config(root: &Path, config_path: &Path) -> Option<File> {
    let directory = std::fs::symlink_metadata(root.join(".lattice")).ok()?;
    let metadata = std::fs::symlink_metadata(config_path).ok()?;
    if !directory.is_dir()
        || directory.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.file_type().is_symlink()
    {
        return None;
    }
    File::open(config_path).ok()
}

fn validate_config(config: &VerificationConfig, root: &Path) -> Option<()> {
    if config.schema_version != 1 || config.checks.is_empty() || config.checks.len() > MAX_CHECKS {
        return None;
    }
    let mut ids = BTreeSet::new();
    for check in &config.checks {
        if !valid_check_id(&check.id) || !ids.insert(check.id.as_str()) {
            return None;
        }
        validate_argv(root, &check.argv)?;
        for outcome in [
            CheckOutcome::Passed,
            CheckOutcome::Failed,
            CheckOutcome::Skipped,
        ] {
            check_event(&check.label, outcome)?;
        }
        if let Some(error) = &check.error {
            error_event(error, ErrorStatus::Observed)?;
            error_event(error, ErrorStatus::Resolved)?;
        }
    }
    Some(())
}

fn validate_argv(root: &Path, argv: &[String]) -> Option<()> {
    if argv.is_empty() || argv.len() > MAX_ARGV_ITEMS {
        return None;
    }
    let mut total = 0_usize;
    for arg in argv {
        if arg.is_empty()
            || arg.len() > MAX_ARG_BYTES
            || arg.bytes().any(|byte| byte == 0 || byte.is_ascii_control())
        {
            return None;
        }
        total = total.checked_add(arg.len())?;
        if total > MAX_ARGV_BYTES {
            return None;
        }
    }
    let executable = Path::new(&argv[0]);
    if executable.is_absolute()
        || executable.components().any(|part| {
            matches!(
                part,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return None;
    }
    let basename = executable.file_name()?.to_str()?;
    if matches!(
        basename,
        "sh" | "bash"
            | "dash"
            | "zsh"
            | "fish"
            | "ksh"
            | "csh"
            | "tcsh"
            | "pwsh"
            | "powershell"
            | "cmd"
            | "cmd.exe"
    ) {
        return None;
    }
    if executable.components().count() > 1 {
        let resolved = root.join(executable).canonicalize().ok()?;
        if !resolved.starts_with(root) || !resolved.is_file() {
            return None;
        }
    }
    Some(())
}

fn execute_declared_check(root: &Path, check: &DeclaredCheck) -> CheckExecution {
    let executable = Path::new(&check.argv[0]);
    let executable: PathBuf = if executable.components().count() > 1 {
        root.join(executable)
    } else {
        executable.to_path_buf()
    };
    match Command::new(executable)
        .args(&check.argv[1..])
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(status) if status.success() => CheckExecution {
            outcome: CheckOutcome::Passed,
            exit_code: 0,
        },
        Ok(status) => CheckExecution {
            outcome: CheckOutcome::Failed,
            exit_code: status
                .code()
                .filter(|code| (1..=255).contains(code))
                .unwrap_or(1),
        },
        Err(_) => CheckExecution {
            outcome: CheckOutcome::Skipped,
            exit_code: 126,
        },
    }
}

fn declared_events(
    check: &DeclaredCheck,
    outcome: CheckOutcome,
) -> Option<Vec<SessionCaptureEvent>> {
    let mut events = vec![check_event(&check.label, outcome)?];
    if let Some(error) = &check.error {
        match outcome {
            CheckOutcome::Failed => events.push(error_event(error, ErrorStatus::Observed)?),
            CheckOutcome::Passed => events.push(error_event(error, ErrorStatus::Resolved)?),
            CheckOutcome::Skipped => {}
        }
    }
    Some(events)
}

fn check_event(label: &str, outcome: CheckOutcome) -> Option<SessionCaptureEvent> {
    parse_session_capture_event(
        &json!({
            "schema_version": SESSION_CAPTURE_SCHEMA_VERSION,
            "kind": "check",
            "label": label,
            "outcome": outcome,
        })
        .to_string(),
    )
    .ok()
}

fn error_event(error: &DeclaredError, status: ErrorStatus) -> Option<SessionCaptureEvent> {
    parse_session_capture_event(
        &json!({
            "schema_version": SESSION_CAPTURE_SCHEMA_VERSION,
            "kind": "error",
            "category": error.category,
            "fingerprint": error.fingerprint,
            "status": status,
        })
        .to_string(),
    )
    .ok()
}

fn emit_event(
    root: &Path,
    invocation: &ProducerInvocation,
    event: &SessionCaptureEvent,
) -> Option<()> {
    let executable = std::env::current_exe().ok()?;
    let mut child = Command::new(executable)
        .args([
            "__hook-adapter",
            invocation.integration.as_str(),
            "structured-fact",
        ])
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let envelope = structured_fact_envelope(&invocation.host_session_id, event)?;
    child
        .stdin
        .take()?
        .write_all(&serde_json::to_vec(&envelope).ok()?)
        .ok()?;
    let _ = child.wait();
    Some(())
}

fn structured_fact_envelope(host_session_id: &str, event: &SessionCaptureEvent) -> Option<Value> {
    match &event.fact {
        SessionCaptureFact::Check { label, outcome } => Some(json!({
            "session_id": host_session_id,
            "schema_version": event.schema_version,
            "kind": "check",
            "label": label,
            "outcome": outcome,
        })),
        SessionCaptureFact::Error {
            category,
            fingerprint,
            status,
            summary: None,
        } => Some(json!({
            "session_id": host_session_id,
            "schema_version": event.schema_version,
            "kind": "error",
            "category": category,
            "fingerprint": fingerprint,
            "status": status,
        })),
        _ => None,
    }
}

fn valid_check_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_ID_BYTES
        && value.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || (index > 0 && matches!(byte, b'-' | b'_' | b'.'))
        })
}

fn valid_opaque_session_id(value: &str) -> bool {
    !value.trim().is_empty()
        && value.len() <= 4096
        && !value
            .bytes()
            .any(|byte| byte == 0 || byte.is_ascii_control())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("lattice-verification-{name}-{nonce}"));
        fs::create_dir_all(root.join(".lattice")).unwrap();
        root
    }

    #[test]
    fn strict_manifest_selects_one_declared_check() {
        let root = temp_root("manifest");
        fs::write(
            root.join(CONFIG_RELATIVE_PATH),
            r#"{
              "schema_version":1,
              "checks":[{
                "id":"core-tests",
                "label":"lattice core tests",
                "argv":["cargo","test","-p","lattice-core"],
                "error":{"category":"test","fingerprint":"sha256:1111111111111111111111111111111111111111111111111111111111111111"}
              }]
            }"#,
        )
        .unwrap();
        let check = load_declared_check(&root, "core-tests").unwrap();
        assert_eq!(check.label, "lattice core tests");
        assert!(load_declared_check(&root, "other").is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn manifest_rejects_unknown_fields_duplicate_ids_shells_and_escaping_programs() {
        let root = temp_root("reject");
        for checks in [
            json!([{"id":"a","label":"safe","argv":["cargo"],"extra":true}]),
            json!([
                {"id":"a","label":"safe","argv":["cargo"]},
                {"id":"a","label":"safe","argv":["cargo"]}
            ]),
            json!([{"id":"a","label":"safe","argv":["sh","-c","false"]}]),
            json!([{"id":"a","label":"safe","argv":["../private/check"]}]),
            json!([{"id":"Upper","label":"safe","argv":["cargo"]}]),
        ] {
            fs::write(
                root.join(CONFIG_RELATIVE_PATH),
                json!({"schema_version":1,"checks":checks}).to_string(),
            )
            .unwrap();
            assert!(load_declared_check(&root, "a").is_none());
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn execution_maps_only_categorical_outcomes() {
        let root = temp_root("execute");
        let passed = DeclaredCheck {
            id: "passed".into(),
            label: "declared pass".into(),
            argv: vec!["true".into()],
            error: None,
        };
        let failed = DeclaredCheck {
            id: "failed".into(),
            label: "declared fail".into(),
            argv: vec!["false".into()],
            error: None,
        };
        assert_eq!(
            execute_declared_check(&root, &passed).outcome,
            CheckOutcome::Passed
        );
        assert_eq!(
            execute_declared_check(&root, &failed).outcome,
            CheckOutcome::Failed
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn emitted_envelopes_contain_no_execution_authority_or_process_data() {
        let check = DeclaredCheck {
            id: "private-id".into(),
            label: "stable public label".into(),
            argv: vec!["private-command".into(), "private-argument".into()],
            error: Some(DeclaredError {
                category: "test".into(),
                fingerprint: format!("sha256:{}", "2".repeat(64)),
            }),
        };
        let events = declared_events(&check, CheckOutcome::Failed).unwrap();
        assert_eq!(events.len(), 2);
        for event in events {
            let wire = structured_fact_envelope("opaque-session", &event)
                .unwrap()
                .to_string();
            for forbidden in [
                "private-id",
                "private-command",
                "private-argument",
                "argv",
                "cwd",
                "env",
                "output",
            ] {
                assert!(!wire.contains(forbidden), "wire retained {forbidden}");
            }
        }
    }
}
