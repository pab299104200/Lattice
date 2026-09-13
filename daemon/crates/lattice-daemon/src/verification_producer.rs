//! Explicit, repository-local producer for typed session verification facts.
//!
//! This private command executes a checkout-declared check without a shell,
//! discards every process stream, and hands only normalized categorical facts
//! to the installed hook adapter.

use lattice_core::memory::{
    parse_session_capture_event, CheckOutcome, ErrorStatus, SessionCaptureEvent,
    SessionCaptureFact, SESSION_CAPTURE_SCHEMA_VERSION,
};
use serde_json::{json, Value};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::trusted_check_runner::{
    load_verification_config, run_explicit_check, DeclaredCheck, DeclaredError,
    TrustedCheckRequest, TrustedCheckStatus,
};
use crate::workspace_identity::WorkspaceIdentity;

const CONFIG_RELATIVE_PATH: &str = ".lattice/verification-checks.json";
const MAX_ID_BYTES: usize = 64;
const PRODUCER_MAX_TIMEOUT: Duration = Duration::from_secs(10 * 60);

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
    let observation = run_explicit_check(TrustedCheckRequest {
        check_id: &check.id,
        workspace: &identity,
        // Hook facts are deliberately not behavioral-validation records. The
        // future trusted adapter supplies a proven graph generation.
        graph_generation: 0,
        max_timeout: PRODUCER_MAX_TIMEOUT,
    });
    let (outcome, exit_code) = match observation {
        Ok(observation) => match observation.status {
            TrustedCheckStatus::Passed => (CheckOutcome::Passed, 0),
            TrustedCheckStatus::Failed => (
                CheckOutcome::Failed,
                observation
                    .exit_code
                    .filter(|code| (1..=255).contains(code))
                    .unwrap_or(1),
            ),
        },
        Err(_) => (CheckOutcome::Skipped, 126),
    };
    let events = declared_events(&check, outcome)?;
    for event in events {
        let _ = emit_event(&identity.checkout_root, &invocation, &event);
    }
    Some(exit_code)
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
    let config = load_verification_config(root).ok()?;
    validate_producer_mappings(&config.checks)?;
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
    match load_verification_config(root) {
        Ok(config) if validate_producer_mappings(&config.checks).is_some() => {
            VerificationConfigHealth::Valid {
                checks: config.checks.len(),
            }
        }
        Ok(_) => VerificationConfigHealth::Invalid,
        Err(_) => VerificationConfigHealth::Invalid,
    }
}

fn validate_producer_mappings(checks: &[DeclaredCheck]) -> Option<()> {
    for check in checks {
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
    use std::path::PathBuf;
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
        let executable = std::env::current_exe().unwrap();
        fs::write(
            root.join(CONFIG_RELATIVE_PATH),
            json!({
              "schema_version":2,
              "checks":[{
                "id":"core-tests",
                "label":"lattice core tests",
                "argv":[executable],
                "timeout_ms": 1000,
                "error":{"category":"test","fingerprint":"sha256:1111111111111111111111111111111111111111111111111111111111111111"}
              }]
            }).to_string(),
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
            json!([{"id":"a","label":"safe","argv":["/bin/true"],"timeout_ms":100,"extra":true}]),
            json!([
                {"id":"a","label":"safe","argv":["/bin/true"],"timeout_ms":100},
                {"id":"a","label":"safe","argv":["/bin/true"],"timeout_ms":100}
            ]),
            json!([{"id":"a","label":"safe","argv":["/bin/sh","-c","false"],"timeout_ms":100}]),
            json!([{"id":"a","label":"safe","argv":["../private/check"],"timeout_ms":100}]),
            json!([{"id":"Upper","label":"safe","argv":["/bin/true"],"timeout_ms":100}]),
        ] {
            fs::write(
                root.join(CONFIG_RELATIVE_PATH),
                json!({"schema_version":2,"checks":checks}).to_string(),
            )
            .unwrap();
            assert!(load_declared_check(&root, "a").is_none());
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn emitted_envelopes_contain_no_execution_authority_or_process_data() {
        let check = DeclaredCheck {
            id: "private-id".into(),
            label: "stable public label".into(),
            argv: vec!["private-command".into(), "private-argument".into()],
            timeout_ms: 100,
            env: Default::default(),
            evidence_reference: None,
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
