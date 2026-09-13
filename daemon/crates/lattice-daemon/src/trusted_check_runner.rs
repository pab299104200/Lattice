//! Explicit, repository-declared behavioral checks.
//!
//! This module is intentionally not connected to hooks, memories, or background
//! work.  Its only entry point requires a caller-selected check identifier and
//! an already-resolved workspace authority.

use crate::workspace_identity::WorkspaceIdentity;
use lattice_core::security::{workspace, SecurityFilter};
use lattice_core::storage::SecureDir;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::ExitStatus;
#[cfg(not(windows))]
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use thiserror::Error;

#[cfg(windows)]
#[path = "trusted_check_windows.rs"]
mod windows_process;

const CONFIG_PATH: &str = ".lattice/verification-checks.json";
const MAX_CONFIG_BYTES: u64 = 64 * 1024;
const MAX_CHECKS: usize = 64;
const MAX_ARGV: usize = 32;
const MAX_ARG_BYTES: usize = 1024;
const MAX_ENV: usize = 32;
const MAX_ENV_BYTES: usize = 4096;
const MAX_OUTPUT_BYTES: usize = 256 * 1024;
const MAX_SOURCE_FILES: usize = 200_000;
const MAX_DIRECTORY_ENTRIES: usize = 1_000_000;
const MAX_INDEX_BYTES: u64 = 128 * 1024 * 1024;
const MAX_EXECUTABLE_BYTES: u64 = 512 * 1024 * 1024;
const PAGE_SIZE: usize = 256;

#[derive(Debug)]
pub(crate) struct TrustedCheckRequest<'a> {
    pub(crate) check_id: &'a str,
    pub(crate) workspace: &'a WorkspaceIdentity,
    pub(crate) graph_generation: u64,
    pub(crate) max_timeout: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrustedCheckStatus {
    Passed,
    Failed,
}

#[derive(Debug)]
pub(crate) struct TrustedCheckObservation {
    pub(crate) repository_id: String,
    pub(crate) checkout_id: String,
    pub(crate) check_id: String,
    pub(crate) evidence_reference: Option<String>,
    pub(crate) status: TrustedCheckStatus,
    pub(crate) revision: Option<String>,
    pub(crate) graph_generation: u64,
    /// Full content binding. Consumers must compare this value; truncating it
    /// into a numeric graph generation does not certify dirty checkout bytes.
    pub(crate) source_fingerprint: [u8; 32],
    pub(crate) observed_at: u64,
    pub(crate) exit_code: Option<i32>,
    pub(crate) elapsed: Duration,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

#[derive(Debug, Error)]
pub(crate) enum TrustedCheckError {
    #[error("verification configuration is invalid: {0}")]
    InvalidConfig(String),
    #[error("verification check `{0}` is not declared by this repository")]
    MissingCheck(String),
    #[error("workspace authority changed or does not match the supplied identity: {0}")]
    AuthorityChanged(String),
    #[error("workspace source state changed while verification check `{0}` ran")]
    SourceChanged(String),
    #[error("verification check `{check_id}` exceeded its {timeout_ms}ms timeout")]
    Timeout { check_id: String, timeout_ms: u64 },
    #[error("verification check `{check_id}` could not execute: {source}")]
    Execution {
        check_id: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to inspect verification authority: {0}")]
    Inspection(String),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct VerificationConfig {
    pub(crate) schema_version: u32,
    pub(crate) checks: Vec<DeclaredCheck>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeclaredCheck {
    pub(crate) id: String,
    pub(crate) label: String,
    pub(crate) argv: Vec<String>,
    pub(crate) timeout_ms: u64,
    #[serde(default)]
    pub(crate) env: BTreeMap<String, String>,
    #[serde(default)]
    pub(crate) evidence_reference: Option<String>,
    #[serde(default)]
    pub(crate) error: Option<DeclaredError>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeclaredError {
    pub(crate) category: String,
    pub(crate) fingerprint: String,
}

struct State {
    authority: Authority,
    fingerprint: [u8; 32],
}

#[derive(PartialEq, Eq)]
struct Authority {
    repository_id: String,
    checkout_id: String,
    common_dir: Option<PathBuf>,
    revision: Option<String>,
}

/// Run one explicitly requested, repository-declared check.
pub(crate) fn run_explicit_check(
    request: TrustedCheckRequest<'_>,
) -> Result<TrustedCheckObservation, TrustedCheckError> {
    {
        validate_identifier(request.check_id, "check identifier")?;
        let root = SecureDir::open(&request.workspace.checkout_root)
            .map_err(|e| TrustedCheckError::Inspection(e.to_string()))?;
        let (config, config_hash) = load_config(&root)?;
        let check = config
            .checks
            .iter()
            .find(|check| check.id == request.check_id)
            .ok_or_else(|| TrustedCheckError::MissingCheck(request.check_id.to_owned()))?
            .clone();
        let timeout = Duration::from_millis(check.timeout_ms).min(request.max_timeout);
        if timeout.is_zero() {
            return Err(TrustedCheckError::InvalidConfig(
                "effective timeout must be greater than zero".into(),
            ));
        }
        let executable = executable_identity(&check, root.path())?;
        let before = inspect_state(request.workspace, &root, config_hash, &executable)?;
        let started = Instant::now();
        let execution = execute(&check, root.path(), timeout)?;
        let elapsed = started.elapsed();
        let (_, after_hash) = load_config(&root)?;
        let after_executable = executable_identity(&check, root.path())?;
        if executable != after_executable {
            return Err(TrustedCheckError::AuthorityChanged(
                "declared executable changed while the check ran".into(),
            ));
        }
        let after = inspect_state(request.workspace, &root, after_hash, &after_executable)?;
        if before.authority != after.authority {
            return Err(TrustedCheckError::AuthorityChanged(
                "repository, checkout, common directory, HEAD, or index authority changed".into(),
            ));
        }
        if before.fingerprint != after.fingerprint {
            return Err(TrustedCheckError::SourceChanged(check.id));
        }
        Ok(TrustedCheckObservation {
            repository_id: before.authority.repository_id,
            checkout_id: before.authority.checkout_id,
            check_id: check.id,
            evidence_reference: check.evidence_reference,
            status: if execution.status.success() {
                TrustedCheckStatus::Passed
            } else {
                TrustedCheckStatus::Failed
            },
            revision: before.authority.revision,
            graph_generation: request.graph_generation,
            source_fingerprint: before.fingerprint,
            observed_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            exit_code: execution.status.code(),
            elapsed,
            stdout: execution.stdout,
            stderr: execution.stderr,
        })
    }
}

/// Recompute the authority/content binding for a stored observation without
/// executing its check. This is the only freshness helper an adapter should
/// use before constructing a behavioral-validation record.
pub(crate) fn observation_matches_current_state(
    observation: &TrustedCheckObservation,
    workspace: &WorkspaceIdentity,
    graph_generation: u64,
) -> Result<bool, TrustedCheckError> {
    if observation.repository_id != workspace.repository_id
        || observation.checkout_id != workspace.checkout_id
        || observation.graph_generation != graph_generation
    {
        return Ok(false);
    }
    let root = SecureDir::open(&workspace.checkout_root)
        .map_err(|e| TrustedCheckError::Inspection(e.to_string()))?;
    let (config, config_hash) = load_config(&root)?;
    let check = config
        .checks
        .iter()
        .find(|check| check.id == observation.check_id)
        .ok_or_else(|| TrustedCheckError::MissingCheck(observation.check_id.clone()))?;
    if check.evidence_reference != observation.evidence_reference {
        return Ok(false);
    }
    let executable = executable_identity(check, root.path())?;
    let state = inspect_state(workspace, &root, config_hash, &executable)?;
    Ok(state.authority.repository_id == observation.repository_id
        && state.authority.checkout_id == observation.checkout_id
        && state.authority.revision == observation.revision
        && state.fingerprint == observation.source_fingerprint)
}

pub(crate) fn load_verification_config(
    checkout_root: &Path,
) -> Result<VerificationConfig, TrustedCheckError> {
    let root = SecureDir::open(checkout_root)
        .map_err(|e| TrustedCheckError::InvalidConfig(e.to_string()))?;
    load_config(&root).map(|(config, _)| config)
}

fn load_config(root: &SecureDir) -> Result<(VerificationConfig, [u8; 32]), TrustedCheckError> {
    let lattice = root.open_dir(".lattice").map_err(|e| {
        TrustedCheckError::InvalidConfig(format!("cannot open `.lattice` without symlinks: {e}"))
    })?;
    let file = lattice
        .open_file("verification-checks.json", false)
        .map_err(|e| {
            TrustedCheckError::InvalidConfig(format!("cannot securely open `{CONFIG_PATH}`: {e}"))
        })?;
    let len = file
        .metadata()
        .map_err(|e| TrustedCheckError::InvalidConfig(e.to_string()))?
        .len();
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let directory = std::fs::symlink_metadata(root.path().join(".lattice"))
            .map_err(|e| TrustedCheckError::InvalidConfig(e.to_string()))?;
        let metadata = file
            .metadata()
            .map_err(|e| TrustedCheckError::InvalidConfig(e.to_string()))?;
        let owner = unsafe { libc::geteuid() };
        if directory.file_type().is_symlink()
            || !directory.is_dir()
            || directory.uid() != owner
            || directory.mode() & 0o022 != 0
            || metadata.uid() != owner
            || metadata.mode() & 0o022 != 0
            || metadata.nlink() != 1
        {
            return Err(TrustedCheckError::InvalidConfig(
                "configuration and `.lattice` must be user-owned and not group/world writable"
                    .into(),
            ));
        }
    }
    if len > MAX_CONFIG_BYTES {
        return Err(TrustedCheckError::InvalidConfig(format!(
            "`{CONFIG_PATH}` exceeds {MAX_CONFIG_BYTES} bytes"
        )));
    }
    let mut bytes = Vec::with_capacity(len as usize);
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| TrustedCheckError::InvalidConfig(e.to_string()))?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        return Err(TrustedCheckError::InvalidConfig(
            "configuration grew while reading".into(),
        ));
    }
    let hash: [u8; 32] = Sha256::digest(&bytes).into();
    let parsed: VerificationConfig = serde_json::from_slice(&bytes)
        .map_err(|e| TrustedCheckError::InvalidConfig(e.to_string()))?;
    validate_config(&parsed)?;
    Ok((parsed, hash))
}

fn validate_config(config: &VerificationConfig) -> Result<(), TrustedCheckError> {
    if config.schema_version != 2 {
        return Err(TrustedCheckError::InvalidConfig(
            "schema_version must be 2".into(),
        ));
    }
    if config.checks.is_empty() || config.checks.len() > MAX_CHECKS {
        return Err(TrustedCheckError::InvalidConfig(format!(
            "checks must contain 1..={MAX_CHECKS} entries"
        )));
    }
    let mut ids = BTreeSet::new();
    for check in &config.checks {
        validate_identifier(&check.id, "check identifier")?;
        if !ids.insert(&check.id) {
            return Err(TrustedCheckError::InvalidConfig(format!(
                "duplicate check identifier `{}`",
                check.id
            )));
        }
        if check.argv.is_empty() || check.argv.len() > MAX_ARGV {
            return Err(TrustedCheckError::InvalidConfig(format!(
                "check `{}` argv must contain 1..={MAX_ARGV} values",
                check.id
            )));
        }
        if check
            .argv
            .iter()
            .any(|arg| arg.is_empty() || arg.len() > MAX_ARG_BYTES)
        {
            return Err(TrustedCheckError::InvalidConfig(format!(
                "check `{}` has an empty or oversized argv value",
                check.id
            )));
        }
        validate_executable(&check.argv[0])?;
        if check.label.is_empty()
            || check.label.len() > 256
            || check
                .label
                .bytes()
                .any(|byte| byte == 0 || byte.is_ascii_control())
        {
            return Err(TrustedCheckError::InvalidConfig(format!(
                "check `{}` has an invalid label",
                check.id
            )));
        }
        if check.timeout_ms == 0 {
            return Err(TrustedCheckError::InvalidConfig(format!(
                "check `{}` timeout must be positive",
                check.id
            )));
        }
        if check.env.len() > MAX_ENV
            || check.env.iter().any(|(key, value)| {
                key.is_empty()
                    || key.contains('=')
                    || key.len() + value.len() > MAX_ENV_BYTES
                    || key.bytes().any(|b| b == 0)
                    || value.bytes().any(|b| b == 0)
            })
        {
            return Err(TrustedCheckError::InvalidConfig(format!(
                "check `{}` has invalid or oversized environment",
                check.id
            )));
        }
        #[cfg(windows)]
        {
            let distinct: BTreeSet<String> = check
                .env
                .keys()
                .map(|key| key.to_ascii_uppercase())
                .collect();
            if distinct.len() != check.env.len() {
                return Err(TrustedCheckError::InvalidConfig(format!(
                    "check `{}` repeats a case-insensitive Windows environment key",
                    check.id
                )));
            }
        }
        if let Some(reference) = &check.evidence_reference {
            validate_reference(reference)?;
        }
        if let Some(error) = &check.error {
            if error.category.is_empty()
                || error.category.len() > 128
                || error.fingerprint.is_empty()
                || error.fingerprint.len() > 256
                || error
                    .category
                    .bytes()
                    .chain(error.fingerprint.bytes())
                    .any(|byte| byte == 0 || byte.is_ascii_control())
            {
                return Err(TrustedCheckError::InvalidConfig(format!(
                    "check `{}` has invalid error mapping",
                    check.id
                )));
            }
        }
    }
    Ok(())
}

fn validate_identifier(value: &str, label: &str) -> Result<(), TrustedCheckError> {
    if value.is_empty()
        || value.len() > 64
        || !value.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || (index > 0 && matches!(byte, b'-' | b'_' | b'.'))
        })
    {
        return Err(TrustedCheckError::InvalidConfig(format!(
            "{label} must be 1..=64 ASCII identifier characters"
        )));
    }
    Ok(())
}

fn validate_reference(value: &str) -> Result<(), TrustedCheckError> {
    let path = Path::new(value);
    if value.len() > 256
        || value.is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(TrustedCheckError::InvalidConfig(
            "evidence_reference must be a repository-relative normal path".into(),
        ));
    }
    Ok(())
}

fn validate_executable(value: &str) -> Result<(), TrustedCheckError> {
    let path = Path::new(value);
    if !path.is_absolute()
        && !(value.starts_with("./")
            && path
                .components()
                .all(|c| matches!(c, Component::CurDir | Component::Normal(_))))
    {
        return Err(TrustedCheckError::InvalidConfig(
            "argv[0] must be an absolute path or a checkout-relative `./` path".into(),
        ));
    }
    let leaf = path
        .file_name()
        .and_then(|v| v.to_str())
        .unwrap_or_default();
    if matches!(
        leaf.to_ascii_lowercase().as_str(),
        "sh" | "bash"
            | "zsh"
            | "dash"
            | "fish"
            | "cmd"
            | "cmd.exe"
            | "powershell"
            | "powershell.exe"
            | "pwsh"
            | "pwsh.exe"
    ) {
        return Err(TrustedCheckError::InvalidConfig(
            "shell executables are not accepted as verification checks".into(),
        ));
    }
    Ok(())
}

struct Execution {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
struct ExecutableIdentity {
    canonical: PathBuf,
    len: u64,
    modified: Option<SystemTime>,
    digest: [u8; 32],
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

fn executable_identity(
    check: &DeclaredCheck,
    root: &Path,
) -> Result<ExecutableIdentity, TrustedCheckError> {
    let declared = Path::new(&check.argv[0]);
    let path = if check.argv[0].starts_with("./") {
        root.join(&check.argv[0][2..])
    } else {
        declared.to_path_buf()
    };
    let lexical = std::fs::symlink_metadata(&path)
        .map_err(|e| TrustedCheckError::InvalidConfig(format!("cannot inspect executable: {e}")))?;
    if lexical.file_type().is_symlink() || !lexical.is_file() {
        return Err(TrustedCheckError::InvalidConfig(
            "declared executable must be a regular file, not a symlink".into(),
        ));
    }
    if lexical.len() > MAX_EXECUTABLE_BYTES {
        return Err(TrustedCheckError::InvalidConfig(format!(
            "declared executable exceeds {MAX_EXECUTABLE_BYTES} bytes"
        )));
    }
    let canonical = path
        .canonicalize()
        .map_err(|e| TrustedCheckError::InvalidConfig(format!("cannot resolve executable: {e}")))?;
    if check.argv[0].starts_with("./") && !canonical.starts_with(root) {
        return Err(TrustedCheckError::InvalidConfig(
            "checkout-relative executable escapes the workspace".into(),
        ));
    }
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    let mut executable = File::open(&canonical)
        .map_err(|e| TrustedCheckError::InvalidConfig(format!("cannot open executable: {e}")))?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut length = 0u64;
    loop {
        let count = executable.read(&mut buffer).map_err(|e| {
            TrustedCheckError::InvalidConfig(format!("cannot hash executable: {e}"))
        })?;
        if count == 0 {
            break;
        }
        length += count as u64;
        if length > MAX_EXECUTABLE_BYTES {
            return Err(TrustedCheckError::InvalidConfig(
                "declared executable grew while hashing".into(),
            ));
        }
        digest.update(&buffer[..count]);
    }
    Ok(ExecutableIdentity {
        canonical,
        len: lexical.len(),
        modified: lexical.modified().ok(),
        digest: digest.finalize().into(),
        #[cfg(unix)]
        device: lexical.dev(),
        #[cfg(unix)]
        inode: lexical.ino(),
    })
}

fn execute(
    check: &DeclaredCheck,
    root: &Path,
    timeout: Duration,
) -> Result<Execution, TrustedCheckError> {
    #[cfg(windows)]
    return windows_process::execute(check, root, timeout);
    #[cfg(not(windows))]
    {
        let executable = if check.argv[0].starts_with("./") {
            root.join(&check.argv[0][2..])
        } else {
            PathBuf::from(&check.argv[0])
        };
        let mut command = Command::new(executable);
        command
            .args(&check.argv[1..])
            .current_dir(root)
            .env_clear()
            .envs(&check.env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command
            .spawn()
            .map_err(|source| TrustedCheckError::Execution {
                check_id: check.id.clone(),
                source,
            })?;
        let stdout = bounded_reader(child.stdout.take().unwrap());
        let stderr = bounded_reader(child.stderr.take().unwrap());
        let deadline = Instant::now() + timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                Ok(None) => {
                    terminate_process_tree(&mut child);
                    let _ = stdout.join();
                    let _ = stderr.join();
                    return Err(TrustedCheckError::Timeout {
                        check_id: check.id.clone(),
                        timeout_ms: timeout.as_millis().try_into().unwrap_or(u64::MAX),
                    });
                }
                Err(source) => {
                    terminate_process_tree(&mut child);
                    return Err(TrustedCheckError::Execution {
                        check_id: check.id.clone(),
                        source,
                    });
                }
            }
        };
        let stdout = stdout.join().unwrap_or_default();
        let stderr = stderr.join().unwrap_or_default();
        if stdout.len() > MAX_OUTPUT_BYTES || stderr.len() > MAX_OUTPUT_BYTES {
            return Err(TrustedCheckError::Execution {
                check_id: check.id.clone(),
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("check output exceeded {MAX_OUTPUT_BYTES} bytes per stream"),
                ),
            });
        }
        Ok(Execution {
            status,
            stdout,
            stderr,
        })
    }
}

#[cfg(not(windows))]
fn bounded_reader(mut stream: impl Read + Send + 'static) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut output = Vec::new();
        let _ = stream
            .by_ref()
            .take(MAX_OUTPUT_BYTES as u64 + 1)
            .read_to_end(&mut output);
        output
    })
}

#[cfg(not(windows))]
fn terminate_process_tree(child: &mut std::process::Child) {
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn inspect_state(
    expected: &WorkspaceIdentity,
    root: &SecureDir,
    config_hash: [u8; 32],
    executable: &ExecutableIdentity,
) -> Result<State, TrustedCheckError> {
    let current_root = SecureDir::open(&expected.checkout_root)
        .map_err(|e| TrustedCheckError::AuthorityChanged(e.to_string()))?;
    let current_identity = current_root
        .identity()
        .map_err(|e| TrustedCheckError::AuthorityChanged(e.to_string()))?;
    let pinned_identity = root
        .identity()
        .map_err(|e| TrustedCheckError::AuthorityChanged(e.to_string()))?;
    if current_identity != pinned_identity {
        return Err(TrustedCheckError::AuthorityChanged(
            "checkout root was renamed or replaced".into(),
        ));
    }
    let actual = WorkspaceIdentity::resolve(root.path())
        .map_err(|e| TrustedCheckError::AuthorityChanged(e.to_string()))?;
    if actual.checkout_root != expected.checkout_root
        || actual.repository_id != expected.repository_id
        || actual.checkout_id != expected.checkout_id
        || actual.git_common_dir != expected.git_common_dir
    {
        return Err(TrustedCheckError::AuthorityChanged(
            "resolved workspace identity differs from caller authority".into(),
        ));
    }
    let mut hash = Sha256::new();
    hash.update(b"lattice-trusted-check-state-v1\0");
    hash.update(config_hash);
    hash.update(executable.canonical.as_os_str().as_encoded_bytes());
    hash.update(executable.len.to_le_bytes());
    hash.update(executable.digest);
    #[cfg(unix)]
    {
        hash.update(executable.device.to_le_bytes());
        hash.update(executable.inode.to_le_bytes());
    }
    hash.update(actual.repository_id.as_bytes());
    hash.update([0]);
    hash.update(actual.checkout_id.as_bytes());
    let revision = if actual.is_git_repository {
        let repo = git2::Repository::open(root.path())
            .map_err(|e| TrustedCheckError::Inspection(e.to_string()))?;
        let head = repo
            .head()
            .ok()
            .and_then(|head| head.target())
            .map(|oid| oid.to_string());
        if let Some(value) = &head {
            hash.update(value.as_bytes());
        }
        hash_index(&repo, &mut hash)?;
        head
    } else {
        None
    };
    hash_sources(root, &mut hash)?;
    Ok(State {
        authority: Authority {
            repository_id: actual.repository_id,
            checkout_id: actual.checkout_id,
            common_dir: actual.git_common_dir,
            revision,
        },
        fingerprint: hash.finalize().into(),
    })
}

fn hash_index(repo: &git2::Repository, hash: &mut Sha256) -> Result<(), TrustedCheckError> {
    let path = repo.path().join("index");
    let mut file = File::open(&path)
        .map_err(|e| TrustedCheckError::Inspection(format!("cannot open Git index: {e}")))?;
    let before = file
        .metadata()
        .map_err(|e| TrustedCheckError::Inspection(e.to_string()))?;
    if before.len() > MAX_INDEX_BYTES {
        return Err(TrustedCheckError::Inspection(format!(
            "Git index exceeds {MAX_INDEX_BYTES} bytes"
        )));
    }
    hash.update(b"index\0");
    let mut remaining = before.len();
    let mut buffer = [0u8; 64 * 1024];
    while remaining > 0 {
        let capacity = buffer.len().min(remaining as usize);
        let count = file
            .read(&mut buffer[..capacity])
            .map_err(|e| TrustedCheckError::Inspection(e.to_string()))?;
        if count == 0 {
            return Err(TrustedCheckError::Inspection(
                "Git index shortened while reading".into(),
            ));
        }
        hash.update(&buffer[..count]);
        remaining -= count as u64;
    }
    if file.read(&mut [0]).unwrap_or(0) != 0
        || file.metadata().ok().map(|m| m.len()) != Some(before.len())
    {
        return Err(TrustedCheckError::Inspection(
            "Git index changed while reading".into(),
        ));
    }
    Ok(())
}

fn hash_sources(root: &SecureDir, hash: &mut Sha256) -> Result<(), TrustedCheckError> {
    let filter = SecurityFilter::new(root.path());
    let mut pending = vec![PathBuf::new()];
    let mut source_paths = Vec::new();
    let mut examined = 0usize;
    let mut files = 0usize;
    while let Some(relative_dir) = pending.pop() {
        let directory = if relative_dir.as_os_str().is_empty() {
            root.try_clone()
        } else {
            root.open_dir(&relative_dir)
        }
        .map_err(|e| TrustedCheckError::Inspection(e.to_string()))?;
        let mut cursor = None;
        loop {
            let page = directory
                .read_dir_page(cursor, PAGE_SIZE)
                .map_err(|e| TrustedCheckError::Inspection(e.to_string()))?;
            examined = examined.saturating_add(page.entries.len());
            if examined > MAX_DIRECTORY_ENTRIES {
                return Err(TrustedCheckError::Inspection(format!(
                    "workspace traversal exceeds {MAX_DIRECTORY_ENTRIES} entries"
                )));
            }
            for entry in page.entries {
                let relative = relative_dir.join(&entry.name);
                let text = relative.to_str().ok_or_else(|| {
                    TrustedCheckError::Inspection("workspace path is not UTF-8".into())
                })?;
                if entry.is_dir {
                    if !filter.is_excluded_dir(&entry.name) && !filter.is_excluded(text) {
                        pending.push(relative);
                    }
                } else if entry.is_file
                    && (matches!(
                        entry.name.as_str(),
                        ".gitignore" | ".lattice_ignore" | ".latticeignore"
                    ) || workspace::allows_source_path(root.path(), &relative))
                {
                    files += 1;
                    if files > MAX_SOURCE_FILES {
                        return Err(TrustedCheckError::Inspection(format!(
                            "workspace exceeds {MAX_SOURCE_FILES} source files"
                        )));
                    }
                    source_paths.push(relative);
                }
            }
            cursor = page.next_cookie;
            if cursor.is_none() {
                break;
            }
        }
    }
    source_paths.sort();
    for relative in source_paths {
        let text = relative
            .to_str()
            .ok_or_else(|| TrustedCheckError::Inspection("workspace path is not UTF-8".into()))?;
        let mut source = open_pinned_file(root, &relative)?;
        let mut content = Sha256::new();
        let mut length = 0u64;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let count = source
                .read(&mut buffer)
                .map_err(|e| TrustedCheckError::Inspection(e.to_string()))?;
            if count == 0 {
                break;
            }
            length = length
                .checked_add(count as u64)
                .ok_or_else(|| TrustedCheckError::Inspection("source length overflow".into()))?;
            content.update(&buffer[..count]);
        }
        hash.update(b"source\0");
        hash.update((text.len() as u64).to_le_bytes());
        hash.update(text.as_bytes());
        hash.update(length.to_le_bytes());
        hash.update(content.finalize());
    }
    Ok(())
}

fn open_pinned_file(root: &SecureDir, relative: &Path) -> Result<File, TrustedCheckError> {
    let parent = relative.parent().unwrap_or_else(|| Path::new(""));
    let directory = if parent.as_os_str().is_empty() {
        root.try_clone()
    } else {
        root.open_dir(parent)
    }
    .map_err(|e| TrustedCheckError::Inspection(e.to_string()))?;
    let leaf = relative
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| TrustedCheckError::Inspection("source path is not UTF-8".into()))?;
    let file = directory
        .open_file(leaf, false)
        .map_err(|e| TrustedCheckError::Inspection(e.to_string()))?;
    let metadata = file
        .metadata()
        .map_err(|e| TrustedCheckError::Inspection(e.to_string()))?;
    if !metadata.is_file() || metadata.len() > workspace::max_source_bytes() {
        return Err(TrustedCheckError::Inspection(
            "source is not regular or exceeds the configured source byte limit".into(),
        ));
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    const CHILD_MARKER: &str = "LATTICE_TRUSTED_CHECK_CHILD";

    #[test]
    fn child_helper() {
        let Ok(action) = std::env::var(CHILD_MARKER) else {
            return;
        };
        match action.as_str() {
            "pass" => println!("bounded-pass"),
            "fail" => panic!("requested failure"),
            "sleep" => thread::sleep(Duration::from_secs(10)),
            "mutate" => {
                fs::write(std::env::var("LATTICE_MUTATE_PATH").unwrap(), "changed\n").unwrap()
            }
            #[cfg(windows)]
            "descendant" => {
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "trusted_check_runner::tests::child_helper",
                        "--exact",
                        "--nocapture",
                    ])
                    .env(CHILD_MARKER, "grandchild")
                    .env(
                        "LATTICE_SENTINEL_PATH",
                        std::env::var("LATTICE_SENTINEL_PATH").unwrap(),
                    )
                    .spawn()
                    .unwrap();
                fs::write(std::env::var("LATTICE_MUTATE_PATH").unwrap(), "ready\n").unwrap();
                thread::sleep(Duration::from_secs(10));
            }
            #[cfg(windows)]
            "grandchild" => {
                thread::sleep(Duration::from_secs(2));
                fs::write(
                    std::env::var("LATTICE_SENTINEL_PATH").unwrap(),
                    "survived\n",
                )
                .unwrap();
            }
            #[cfg(windows)]
            "output" => {
                use std::io::Write;
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "trusted_check_runner::tests::child_helper",
                        "--exact",
                        "--nocapture",
                    ])
                    .env(CHILD_MARKER, "grandchild")
                    .env(
                        "LATTICE_SENTINEL_PATH",
                        std::env::var("LATTICE_SENTINEL_PATH").unwrap(),
                    )
                    .spawn()
                    .unwrap();
                fs::write(std::env::var("LATTICE_MUTATE_PATH").unwrap(), "ready\n").unwrap();
                std::io::stdout()
                    .write_all(&vec![b'x'; MAX_OUTPUT_BYTES + 1])
                    .unwrap();
            }
            other => panic!("unknown child action {other}"),
        }
    }

    fn fixture(action: &str) -> (TempDir, WorkspaceIdentity) {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join(".lattice")).unwrap();
        fs::write(temp.path().join("source.rs"), "fn stable() {}\n").unwrap();
        write_config(temp.path(), "check", action, 5_000);
        let root = temp.path().canonicalize().unwrap();
        let identity = WorkspaceIdentity::standalone(root);
        (temp, identity)
    }

    fn write_config(root: &Path, id: &str, action: &str, timeout_ms: u64) {
        let executable = std::env::current_exe().unwrap();
        let mutate = root.join("source.rs");
        let value = serde_json::json!({
            "schema_version": 2,
            "checks": [{
                "id": id,
                "label": "isolated runner test",
                "argv": [executable, "trusted_check_runner::tests::child_helper", "--exact", "--nocapture"],
                "timeout_ms": timeout_ms,
                "env": {
                    CHILD_MARKER: action,
                    "LATTICE_MUTATE_PATH": mutate,
                    "LATTICE_SENTINEL_PATH": root.join("descendant-survived")
                },
                "evidence_reference": "tests/behavior.rs"
            }]
        });
        fs::write(
            root.join(CONFIG_PATH),
            serde_json::to_vec_pretty(&value).unwrap(),
        )
        .unwrap();
    }

    fn run<'a>(identity: &'a WorkspaceIdentity, id: &'a str) -> TrustedCheckRequest<'a> {
        TrustedCheckRequest {
            check_id: id,
            workspace: identity,
            graph_generation: 17,
            max_timeout: Duration::from_secs(6),
        }
    }

    #[test]
    fn records_pass_and_fail_with_bounded_output() {
        let (_temp, identity) = fixture("pass");
        let passed = run_explicit_check(run(&identity, "check")).unwrap();
        assert_eq!(passed.status, TrustedCheckStatus::Passed);
        assert_eq!(passed.graph_generation, 17);
        assert_eq!(
            passed.evidence_reference.as_deref(),
            Some("tests/behavior.rs")
        );
        assert!(!passed.source_fingerprint.iter().all(|byte| *byte == 0));
        assert!(observation_matches_current_state(&passed, &identity, 17).unwrap());
        assert!(!observation_matches_current_state(&passed, &identity, 18).unwrap());

        write_config(&identity.checkout_root, "check", "fail", 5_000);
        let failed = run_explicit_check(run(&identity, "check")).unwrap();
        assert_eq!(failed.status, TrustedCheckStatus::Failed);
        assert_ne!(failed.exit_code, Some(0));
    }

    #[test]
    fn timeout_kills_the_process_group() {
        let (_temp, identity) = fixture("sleep");
        write_config(&identity.checkout_root, "check", "sleep", 40);
        let error = run_explicit_check(run(&identity, "check")).unwrap_err();
        assert!(matches!(error, TrustedCheckError::Timeout { .. }));
    }

    #[test]
    fn rejects_changed_source_state() {
        let (_temp, identity) = fixture("mutate");
        let error = run_explicit_check(run(&identity, "check")).unwrap_err();
        assert!(matches!(error, TrustedCheckError::SourceChanged(_)));
    }

    #[test]
    fn rejects_wrong_checkout_and_missing_check() {
        let (_temp, identity) = fixture("pass");
        let mut wrong = identity.clone();
        wrong.checkout_id.push_str("-forged");
        assert!(matches!(
            run_explicit_check(run(&wrong, "check")).unwrap_err(),
            TrustedCheckError::AuthorityChanged(_)
        ));
        assert!(matches!(
            run_explicit_check(run(&identity, "missing")).unwrap_err(),
            TrustedCheckError::MissingCheck(_)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlinked_configuration() {
        use std::os::unix::fs::symlink;
        let (temp, identity) = fixture("pass");
        let outside = temp.path().join("outside.json");
        fs::rename(temp.path().join(CONFIG_PATH), &outside).unwrap();
        symlink(&outside, temp.path().join(CONFIG_PATH)).unwrap();
        assert!(matches!(
            run_explicit_check(run(&identity, "check")).unwrap_err(),
            TrustedCheckError::InvalidConfig(_)
        ));
    }

    #[cfg(windows)]
    #[test]
    fn job_timeout_removes_descendants_before_they_can_escape() {
        let (_temp, identity) = fixture("descendant");
        write_config(&identity.checkout_root, "check", "descendant", 500);
        assert!(matches!(
            run_explicit_check(run(&identity, "check")).unwrap_err(),
            TrustedCheckError::Timeout { .. }
        ));
        assert_eq!(
            fs::read(identity.checkout_root.join("source.rs")).unwrap(),
            b"ready\n"
        );
        thread::sleep(Duration::from_secs(3));
        assert!(!identity.checkout_root.join("descendant-survived").exists());
    }

    #[cfg(windows)]
    #[test]
    fn output_overflow_terminates_the_contained_check() {
        let (_temp, identity) = fixture("output");
        let error = run_explicit_check(run(&identity, "check")).unwrap_err();
        assert!(matches!(error, TrustedCheckError::Execution { .. }));
        assert!(error.to_string().contains("output exceeded"));
        assert_eq!(
            fs::read(identity.checkout_root.join("source.rs")).unwrap(),
            b"ready\n"
        );
        thread::sleep(Duration::from_secs(3));
        assert!(!identity.checkout_root.join("descendant-survived").exists());
    }

    #[test]
    fn rejects_shell_and_oversized_declarations() {
        let (temp, identity) = fixture("pass");
        let value = serde_json::json!({
            "schema_version": 2,
            "checks": [{"id":"check", "label":"shell", "argv":["/bin/sh", "-c", "true"], "timeout_ms":100}]
        });
        fs::write(
            temp.path().join(CONFIG_PATH),
            serde_json::to_vec(&value).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            run_explicit_check(run(&identity, "check")).unwrap_err(),
            TrustedCheckError::InvalidConfig(_)
        ));
    }
}
