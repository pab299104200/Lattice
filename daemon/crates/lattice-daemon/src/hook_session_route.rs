//! Authenticated, shard-independent `hook/session_open` routing.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::hook_session_binding::{
    HookBindingId, HookCheckoutIdentity, HookIntegrationId, HookRepositoryState,
    HookSessionCapability, HookSessionCryptography, HostSessionId,
};
use crate::hook_session_registry::{
    HookRegistryConfig, HookRegistryError, HookSessionRegistry, RegistryOpenRequest,
    RegistrySessionResume,
};
use crate::transport::ProxyRequest;
use crate::workspace_identity::WorkspaceIdentity;

const KEY_BYTES: usize = 32;
const MAX_PARAMS_BYTES: usize = 16 * 1024;
const IDLE_TTL_MS: i64 = 30 * 60 * 1_000;
const ABSOLUTE_TTL_MS: i64 = 12 * 60 * 60 * 1_000;
const RETENTION_MS: i64 = 7 * 24 * 60 * 60 * 1_000;

pub(crate) const HOOK_SESSION_OPEN_METHOD: &str = "hook/session_open";

#[derive(Debug)]
pub(crate) enum HookSessionRouteError {
    InvalidRequest,
    AuthorityRejected,
    Unavailable,
}

impl HookSessionRouteError {
    pub(crate) fn json_rpc_error(&self) -> (i32, String) {
        match self {
            Self::InvalidRequest => (-32602, "hook session open request is invalid".into()),
            Self::AuthorityRejected => (-32001, "hook session open rejected".into()),
            Self::Unavailable => (-32603, "hook session service is unavailable".into()),
        }
    }
}

pub(crate) struct HookSessionRoute {
    cryptography: HookSessionCryptography,
    registry: Mutex<HookSessionRegistry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HookSessionOpenParams {
    integration: String,
    host_session_id: String,
    checkout_root: String,
    repository_id: String,
    checkout_id: String,
    #[serde(default)]
    resume: Option<HookSessionResumeParams>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HookSessionResumeParams {
    binding_id: String,
    capability: String,
}

#[derive(Serialize)]
struct HookSessionOpenResult {
    binding_id: String,
    capability: String,
    generation: u64,
    resumed: bool,
    idle_deadline_ms: i64,
    absolute_deadline_ms: i64,
    repository_id: String,
    checkout_id: String,
}

impl HookSessionRoute {
    pub(crate) fn open_default() -> Result<Self> {
        let state_root = default_state_root()?;
        ensure_private_directory(&state_root)?;
        Self::open_at(&state_root.join("hook-sessions"))
    }

    pub(crate) fn open_at(directory: &Path) -> Result<Self> {
        ensure_private_directory(directory)?;
        let secret = load_or_create_key(&directory.join("authority.key"))?;
        let registry_path = directory.join("sessions.db");
        ensure_private_database_file(&registry_path)?;
        let registry = HookSessionRegistry::open(&registry_path, HookRegistryConfig::default())
            .context("failed to open hook-session registry")?;
        validate_private_file(&registry_path, "hook-session registry")?;
        Ok(Self {
            cryptography: HookSessionCryptography::from_secret(secret),
            registry: Mutex::new(registry),
        })
    }

    pub(crate) fn handle(
        &self,
        hello: &ProxyRequest,
        params: Value,
    ) -> Result<Value, HookSessionRouteError> {
        let encoded_len = serde_json::to_vec(&params)
            .map_err(|_| HookSessionRouteError::InvalidRequest)?
            .len();
        if encoded_len > MAX_PARAMS_BYTES {
            return Err(HookSessionRouteError::InvalidRequest);
        }
        let params: HookSessionOpenParams =
            serde_json::from_value(params).map_err(|_| HookSessionRouteError::InvalidRequest)?;
        let resolved = resolve_authority(hello, &params)?;
        let integration = HookIntegrationId::new(params.integration)
            .map_err(|_| HookSessionRouteError::InvalidRequest)?;
        let host_session_id = HostSessionId::new(params.host_session_id)
            .map_err(|_| HookSessionRouteError::InvalidRequest)?;
        let checkout = HookCheckoutIdentity::new(
            resolved.repository_id.clone(),
            resolved.checkout_root.to_string_lossy().to_string(),
        )
        .map_err(|_| HookSessionRouteError::Unavailable)?;
        let repository_state = resolve_repository_state(&resolved.checkout_root)?;
        let resume = params.resume.map(decode_resume).transpose()?;
        let request = registry_open_request(
            integration,
            host_session_id,
            checkout,
            repository_state,
            resume,
        )?;
        let outcome = self
            .registry
            .lock()
            .map_err(|_| HookSessionRouteError::Unavailable)?
            .open_or_resume(&self.cryptography, request)
            .map_err(map_registry_error)?;
        serde_json::to_value(HookSessionOpenResult {
            binding_id: encode_hex(outcome.binding_id.as_bytes()),
            capability: encode_hex(outcome.capability.as_bytes()),
            generation: outcome.generation,
            resumed: outcome.resumed,
            idle_deadline_ms: outcome.idle_deadline_ms,
            absolute_deadline_ms: outcome.absolute_deadline_ms,
            repository_id: resolved.repository_id,
            checkout_id: resolved.checkout_root.to_string_lossy().to_string(),
        })
        .map_err(|_| HookSessionRouteError::Unavailable)
    }
}

fn registry_open_request(
    integration: HookIntegrationId,
    host_session_id: HostSessionId,
    checkout: HookCheckoutIdentity,
    repository_state: HookRepositoryState,
    resume: Option<RegistrySessionResume>,
) -> Result<RegistryOpenRequest, HookSessionRouteError> {
    Ok(RegistryOpenRequest {
        integration,
        host_session_id,
        checkout,
        repository_state,
        resume,
        now_ms: now_ms()?,
        idle_ttl_ms: IDLE_TTL_MS,
        absolute_ttl_ms: ABSOLUTE_TTL_MS,
        retention_ms: RETENTION_MS,
    })
}

fn resolve_authority(
    hello: &ProxyRequest,
    params: &HookSessionOpenParams,
) -> Result<WorkspaceIdentity, HookSessionRouteError> {
    if hello.workspace_roots.len() != 1
        || !hello.focus_files.is_empty()
        || !hello.focus_dirs.is_empty()
    {
        return Err(HookSessionRouteError::AuthorityRejected);
    }
    let claimed_root = PathBuf::from(&params.checkout_root);
    if !claimed_root.is_absolute() {
        return Err(HookSessionRouteError::InvalidRequest);
    }
    let identity = WorkspaceIdentity::resolve(&claimed_root)
        .map_err(|_| HookSessionRouteError::AuthorityRejected)?;
    let hello_root = PathBuf::from(&hello.workspace_roots[0])
        .canonicalize()
        .map_err(|_| HookSessionRouteError::AuthorityRejected)?;
    let checkout_id = identity.checkout_root.to_string_lossy();
    if hello_root != identity.checkout_root
        || params.checkout_root != checkout_id
        || params.checkout_id != checkout_id
        || params.repository_id != identity.repository_id
    {
        return Err(HookSessionRouteError::AuthorityRejected);
    }
    Ok(identity)
}

fn resolve_repository_state(root: &Path) -> Result<HookRepositoryState, HookSessionRouteError> {
    let snapshot = crate::repo_state::resolve_repo_state(root)
        .ok_or(HookSessionRouteError::AuthorityRejected)?;
    let revision = snapshot
        .head_oid
        .ok_or(HookSessionRouteError::AuthorityRejected)?;
    let branch = snapshot
        .head_ref
        .and_then(|value| value.strip_prefix("refs/heads/").map(str::to_owned));
    HookRepositoryState::new(branch, revision).map_err(|_| HookSessionRouteError::Unavailable)
}

fn decode_resume(
    resume: HookSessionResumeParams,
) -> Result<RegistrySessionResume, HookSessionRouteError> {
    Ok(RegistrySessionResume {
        binding_id: HookBindingId::from_bytes(decode_hex::<16>(&resume.binding_id)?),
        capability: HookSessionCapability::from_bytes(decode_hex::<32>(&resume.capability)?),
    })
}

fn map_registry_error(error: HookRegistryError) -> HookSessionRouteError {
    match error {
        HookRegistryError::BindingAlreadyOpen
        | HookRegistryError::BindingConflict
        | HookRegistryError::BindingNotFound
        | HookRegistryError::InvalidCapability
        | HookRegistryError::AuthorityMismatch
        | HookRegistryError::Expired
        | HookRegistryError::Sealed
        | HookRegistryError::Revoked => HookSessionRouteError::AuthorityRejected,
        HookRegistryError::InvalidConfiguration
        | HookRegistryError::InvalidIdentifier
        | HookRegistryError::InvalidValue => HookSessionRouteError::InvalidRequest,
        _ => HookSessionRouteError::Unavailable,
    }
}

fn now_ms() -> Result<i64, HookSessionRouteError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| HookSessionRouteError::Unavailable)?
        .as_millis();
    i64::try_from(millis).map_err(|_| HookSessionRouteError::Unavailable)
}

fn default_state_root() -> Result<PathBuf> {
    if let Some(state) = std::env::var_os("XDG_STATE_HOME").filter(|value| !value.is_empty()) {
        let state = PathBuf::from(state);
        if !state.is_absolute() {
            anyhow::bail!("XDG_STATE_HOME must be absolute");
        }
        return Ok(state.join("lattice"));
    }
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
    if !home.is_absolute() {
        anyhow::bail!("HOME must be absolute");
    }
    Ok(home.join(".local/state/lattice"))
}

#[cfg(unix)]
fn ensure_private_directory(path: &Path) -> Result<()> {
    if std::fs::symlink_metadata(path).is_ok() {
        return validate_private_directory(path);
    }
    let parent = path.context_parent()?;
    std::fs::create_dir_all(parent).with_context(|| {
        format!(
            "failed to create hook-session state parent {}",
            parent.display()
        )
    })?;
    match std::fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error).context("failed to create hook-session state directory"),
    }
    validate_private_directory(path)
}

#[cfg(not(unix))]
fn ensure_private_directory(_path: &Path) -> Result<()> {
    anyhow::bail!("hook-session state requires Unix ownership and mode checks")
}

trait PathContextExt {
    fn context_parent(&self) -> Result<&Path>;
}

impl PathContextExt for Path {
    fn context_parent(&self) -> Result<&Path> {
        self.parent()
            .context("hook-session state directory has no parent")
    }
}

#[cfg(unix)]
fn validate_private_directory(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        anyhow::bail!("hook-session state path is not a real directory");
    }
    if metadata.uid() != unsafe { libc::geteuid() } {
        anyhow::bail!("hook-session state directory has unsafe ownership");
    }
    if metadata.permissions().mode() & 0o777 != 0o700 {
        anyhow::bail!("hook-session state directory must have mode 0700");
    }
    Ok(())
}

#[cfg(unix)]
fn validate_private_file(path: &Path, label: &str) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        anyhow::bail!("{label} is not a regular file");
    }
    validate_private_file_metadata(&metadata, label)
}

#[cfg(unix)]
fn validate_private_file_metadata(metadata: &std::fs::Metadata, label: &str) -> Result<()> {
    if !metadata.is_file() {
        anyhow::bail!("{label} is not a regular file");
    }
    if metadata.uid() != unsafe { libc::geteuid() } {
        anyhow::bail!("{label} has unsafe ownership");
    }
    if metadata.permissions().mode() & 0o777 != 0o600 {
        anyhow::bail!("{label} must have mode 0600");
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_private_file(_path: &Path, _label: &str) -> Result<()> {
    anyhow::bail!("hook-session state requires Unix ownership and mode checks")
}

#[cfg(unix)]
fn open_private_read(path: &Path, label: &str) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("failed to open {label}"))?;
    validate_private_file_metadata(&file.metadata()?, label)?;
    Ok(file)
}

#[cfg(unix)]
fn ensure_private_database_file(path: &Path) -> Result<()> {
    if std::fs::symlink_metadata(path).is_ok() {
        open_private_read(path, "hook-session registry")?;
        return Ok(());
    }
    match OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file.sync_all()?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            open_private_read(path, "hook-session registry")?;
        }
        Err(error) => return Err(error).context("failed to create hook-session registry"),
    }
    validate_private_file(path, "hook-session registry")
}

#[cfg(not(unix))]
fn ensure_private_database_file(_path: &Path) -> Result<()> {
    anyhow::bail!("hook-session state requires Unix ownership and mode checks")
}

#[cfg(unix)]
fn load_or_create_key(path: &Path) -> Result<[u8; KEY_BYTES]> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => return read_key(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("failed to inspect hook-session authority key"),
    }
    let parent = path.context_parent()?;
    let temporary = parent.join(format!(".authority-{}.tmp", random_suffix()?));
    let create_result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temporary)?;
        let secret = random_key()?;
        file.write_all(&secret)?;
        file.sync_all()?;
        match std::fs::hard_link(&temporary, path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error).context("failed to install hook-session authority key")
            }
        }
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    let _ = std::fs::remove_file(&temporary);
    create_result?;
    read_key(path)
}

#[cfg(not(unix))]
fn load_or_create_key(_path: &Path) -> Result<[u8; KEY_BYTES]> {
    anyhow::bail!("hook-session state requires Unix ownership and mode checks")
}

fn read_key(path: &Path) -> Result<[u8; KEY_BYTES]> {
    let mut file = open_private_read(path, "hook-session authority key")?;
    if file.metadata()?.len() != KEY_BYTES as u64 {
        anyhow::bail!("hook-session authority key has an invalid length");
    }
    let mut secret = [0_u8; KEY_BYTES];
    file.read_exact(&mut secret)?;
    Ok(secret)
}

fn random_key() -> Result<[u8; KEY_BYTES]> {
    let mut secret = [0_u8; KEY_BYTES];
    File::open("/dev/urandom")?.read_exact(&mut secret)?;
    Ok(secret)
}

fn random_suffix() -> Result<String> {
    Ok(encode_hex(&random_key()?[..16]))
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(HEX[(byte >> 4) as usize] as char);
        result.push(HEX[(byte & 0x0f) as usize] as char);
    }
    result
}

fn decode_hex<const N: usize>(value: &str) -> Result<[u8; N], HookSessionRouteError> {
    if value.len() != N * 2 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(HookSessionRouteError::InvalidRequest);
    }
    let mut result = [0_u8; N];
    for (index, slot) in result.iter_mut().enumerate() {
        let offset = index * 2;
        *slot = (hex_nibble(value.as_bytes()[offset])? << 4)
            | hex_nibble(value.as_bytes()[offset + 1])?;
    }
    Ok(result)
}

fn hex_nibble(byte: u8) -> Result<u8, HookSessionRouteError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err(HookSessionRouteError::InvalidRequest),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn durable_route_reuses_key_and_rejects_unsafe_state() {
        let directory = test_directory("durable");
        HookSessionRoute::open_at(&directory).expect("create durable route");
        HookSessionRoute::open_at(&directory).expect("reopen durable route");
        assert_eq!(
            std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(directory.join("authority.key"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(directory.join("sessions.db"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        std::fs::set_permissions(
            directory.join("authority.key"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert!(HookSessionRoute::open_at(&directory).is_err());
        std::fs::remove_dir_all(directory).unwrap();

        let symlink_directory = test_directory("symlink");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&symlink_directory)
            .unwrap();
        let target = symlink_directory.join("key-target");
        std::fs::write(&target, [7_u8; KEY_BYTES]).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::os::unix::fs::symlink(&target, symlink_directory.join("authority.key")).unwrap();
        assert!(HookSessionRoute::open_at(&symlink_directory).is_err());
        std::fs::remove_dir_all(symlink_directory).unwrap();
    }

    #[test]
    fn hex_decoder_is_exact_and_lowercase_only() {
        assert_eq!(decode_hex::<2>("00af").unwrap(), [0, 175]);
        assert!(decode_hex::<2>("00AF").is_err());
        assert!(decode_hex::<2>("00").is_err());
    }

    #[test]
    fn binding_capability_resumes_after_route_reopen() {
        let directory = test_directory("restart-state");
        let checkout = committed_repository("restart-checkout");
        let identity = WorkspaceIdentity::resolve(&checkout).unwrap();
        let hello = ProxyRequest {
            workspace_roots: vec![identity.checkout_root.to_string_lossy().to_string()],
            focus_files: Vec::new(),
            focus_dirs: Vec::new(),
        };
        let params = serde_json::json!({
            "integration": "codex/v1",
            "host_session_id": "persistent-host-session",
            "checkout_root": identity.checkout_root.to_string_lossy(),
            "repository_id": identity.repository_id,
            "checkout_id": identity.checkout_root.to_string_lossy(),
        });
        let opened = HookSessionRoute::open_at(&directory)
            .unwrap()
            .handle(&hello, params.clone())
            .unwrap();

        let reopened = HookSessionRoute::open_at(&directory).unwrap();
        assert!(matches!(
            reopened.handle(&hello, params.clone()),
            Err(HookSessionRouteError::AuthorityRejected)
        ));
        let mut resume = params;
        resume["resume"] = serde_json::json!({
            "binding_id": opened["binding_id"],
            "capability": opened["capability"],
        });
        let resumed = reopened.handle(&hello, resume).unwrap();
        assert_eq!(resumed["resumed"], true);
        assert_eq!(resumed["binding_id"], opened["binding_id"]);

        drop(reopened);
        std::fs::remove_dir_all(directory).unwrap();
        std::fs::remove_dir_all(checkout).unwrap();
    }

    fn committed_repository(label: &str) -> PathBuf {
        let root = test_directory(label);
        std::fs::create_dir_all(&root).unwrap();
        git(&root, &["init"]);
        git(&root, &["config", "user.email", "lattice@example.test"]);
        git(&root, &["config", "user.name", "Lattice Test"]);
        std::fs::write(root.join("fixture.txt"), "fixture\n").unwrap();
        git(&root, &["add", "fixture.txt"]);
        git(&root, &["commit", "-m", "fixture"]);
        root
    }

    fn git(root: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn test_directory(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "lattice-hook-route-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }
}
