use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::fs::{File, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub(crate) const TRANSPORT_PROTOCOL_VERSION: u32 = 1;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TransportCredential {
    pub protocol_version: u32,
    pub daemon_epoch: String,
    pub transport_token: String,
    pub listener_address: String,
    pub daemon_pid: u32,
}

pub(crate) struct IssuedTransportCredential {
    pub(crate) credential: TransportCredential,
    pub(crate) path: PathBuf,
}

impl IssuedTransportCredential {
    pub(crate) fn credential(&self) -> &TransportCredential {
        &self.credential
    }
}

impl Drop for IssuedTransportCredential {
    fn drop(&mut self) {
        // Never remove a credential installed by a replacement daemon.
        let Ok(current) = read_credential_path(&self.path) else {
            return;
        };
        if constant_time_eq(
            current.daemon_epoch.as_bytes(),
            self.credential.daemon_epoch.as_bytes(),
        ) && constant_time_eq(
            current.transport_token.as_bytes(),
            self.credential.transport_token.as_bytes(),
        ) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

pub(crate) fn issue(listener_address: &str) -> Result<IssuedTransportCredential> {
    let credential = TransportCredential {
        protocol_version: TRANSPORT_PROTOCOL_VERSION,
        daemon_epoch: random_hex(32)?,
        transport_token: random_hex(32)?,
        listener_address: listener_address.to_string(),
        daemon_pid: std::process::id(),
    };
    let directory = credential_directory()?;
    ensure_private_directory(&directory)?;
    let path = credential_path_in(&directory, listener_address);
    write_credential_path(&path, &credential)?;
    Ok(IssuedTransportCredential { credential, path })
}

pub(crate) fn load(listener_address: &str) -> Result<TransportCredential> {
    let directory = credential_directory()?;
    validate_private_directory(&directory)?;
    let path = credential_path_in(&directory, listener_address);
    let credential = read_credential_path(&path)?;
    validate_for_address(&credential, listener_address)?;
    Ok(credential)
}

fn validate_for_address(credential: &TransportCredential, listener_address: &str) -> Result<()> {
    if credential.listener_address != listener_address {
        anyhow::bail!("transport credential is scoped to a different daemon address");
    }
    if credential.protocol_version != TRANSPORT_PROTOCOL_VERSION {
        anyhow::bail!(
            "transport credential protocol version {} is incompatible with client version {}; restart the lattice daemon and reconnect with the matching binary",
            credential.protocol_version,
            TRANSPORT_PROTOCOL_VERSION
        );
    }
    if !valid_hex(&credential.daemon_epoch, 32) || !valid_hex(&credential.transport_token, 32) {
        anyhow::bail!("protected transport credential is malformed");
    }
    if credential.daemon_pid == 0 || !process_exists(credential.daemon_pid) {
        anyhow::bail!("protected transport credential belongs to a daemon that is not running");
    }
    Ok(())
}

#[cfg(unix)]
fn process_exists(pid: u32) -> bool {
    // SAFETY: signal zero performs existence/permission validation only.
    (unsafe { libc::kill(pid as libc::pid_t, 0) == 0 })
        || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn process_exists(_pid: u32) -> bool {
    false
}

pub(crate) fn random_hex(byte_count: usize) -> Result<String> {
    let mut bytes = vec![0_u8; byte_count];
    File::open("/dev/urandom")
        .context("failed to open operating-system random source")?
        .read_exact(&mut bytes)
        .context("failed to read operating-system random source")?;
    let mut encoded = String::with_capacity(byte_count * 2);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    Ok(encoded)
}

fn credential_directory() -> Result<PathBuf> {
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").filter(|value| !value.is_empty()) {
        let runtime = PathBuf::from(runtime);
        if !runtime.is_absolute() {
            anyhow::bail!("XDG_RUNTIME_DIR must be an absolute path");
        }
        return Ok(runtime.join("lattice"));
    }
    let home = std::env::var_os("HOME").context(
        "cannot locate the protected lattice runtime directory: neither XDG_RUNTIME_DIR nor HOME is set",
    )?;
    let home = PathBuf::from(home);
    if !home.is_absolute() {
        anyhow::bail!("HOME must be an absolute path");
    }
    Ok(home.join(".lattice").join("run"))
}

fn credential_path_in(directory: &Path, listener_address: &str) -> PathBuf {
    let mut hasher = DefaultHasher::new();
    listener_address.hash(&mut hasher);
    directory.join(format!("transport-{:016x}.json", hasher.finish()))
}

#[cfg(unix)]
fn ensure_private_directory(path: &Path) -> Result<()> {
    if path.exists() || std::fs::symlink_metadata(path).is_ok() {
        return validate_private_directory(path);
    }
    let parent = path
        .parent()
        .context("transport credential directory has no parent")?;
    std::fs::create_dir_all(parent).with_context(|| {
        format!(
            "failed to create transport runtime parent {}",
            parent.display()
        )
    })?;
    match std::fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to create transport runtime directory {}",
                    path.display()
                )
            });
        }
    }
    validate_private_directory(path)
}

#[cfg(not(unix))]
fn ensure_private_directory(_path: &Path) -> Result<()> {
    anyhow::bail!("protected transport credentials require Unix ownership and mode checks")
}

#[cfg(unix)]
fn validate_private_directory(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path).with_context(|| {
        format!(
            "protected transport runtime directory {} is unavailable",
            path.display()
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        anyhow::bail!("transport runtime path is not a real directory");
    }
    if metadata.uid() != unsafe { libc::geteuid() } {
        anyhow::bail!("transport runtime directory is not owned by the current user");
    }
    if metadata.permissions().mode() & 0o777 != 0o700 {
        anyhow::bail!("transport runtime directory must have mode 0700");
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_private_directory(_path: &Path) -> Result<()> {
    anyhow::bail!("protected transport credentials require Unix ownership and mode checks")
}

#[cfg(unix)]
fn validate_credential_metadata(metadata: &std::fs::Metadata) -> Result<()> {
    if !metadata.is_file() {
        anyhow::bail!("transport credential is not a regular file");
    }
    if metadata.uid() != unsafe { libc::geteuid() } {
        anyhow::bail!("transport credential is not owned by the current user");
    }
    if metadata.permissions().mode() & 0o777 != 0o600 {
        anyhow::bail!("transport credential must have mode 0600");
    }
    Ok(())
}

#[cfg(unix)]
fn open_credential(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| {
            format!(
                "failed to open protected transport credential {}",
                path.display()
            )
        })?;
    validate_credential_metadata(&file.metadata()?)?;
    Ok(file)
}

#[cfg(not(unix))]
fn open_credential(_path: &Path) -> Result<File> {
    anyhow::bail!("protected transport credentials require Unix ownership and mode checks")
}

fn read_credential_path(path: &Path) -> Result<TransportCredential> {
    let file = open_credential(path)?;
    if file.metadata()?.len() > 16 * 1024 {
        anyhow::bail!("protected transport credential exceeds the size limit");
    }
    let mut bytes = Vec::new();
    file.take(16 * 1024)
        .read_to_end(&mut bytes)
        .context("failed to read protected transport credential")?;
    serde_json::from_slice(&bytes).context("protected transport credential is malformed")
}

fn valid_hex(value: &str, byte_count: usize) -> bool {
    value.len() == byte_count * 2
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    let max_len = left.len().max(right.len());
    for index in 0..max_len {
        difference |= usize::from(
            left.get(index).copied().unwrap_or(0) ^ right.get(index).copied().unwrap_or(0),
        );
    }
    difference == 0
}

#[cfg(unix)]
fn write_credential_path(path: &Path, credential: &TransportCredential) -> Result<()> {
    if std::fs::symlink_metadata(path).is_ok() {
        let existing = open_credential(path)?;
        drop(existing);
    }
    let parent = path
        .parent()
        .context("transport credential path has no parent")?;
    let temporary = parent.join(format!(".transport-{}.tmp", random_hex(16)?));
    let write_result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temporary)
            .context("failed to create protected transport credential")?;
        let encoded = serde_json::to_vec(credential)?;
        file.write_all(&encoded)
            .context("failed to write protected transport credential")?;
        file.sync_all()
            .context("failed to sync protected transport credential")?;
        std::fs::rename(&temporary, path)
            .context("failed to atomically install protected transport credential")?;
        validate_credential_metadata(&open_credential(path)?.metadata()?)?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .context("failed to sync transport runtime directory")?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    write_result
}

#[cfg(not(unix))]
fn write_credential_path(_path: &Path, _credential: &TransportCredential) -> Result<()> {
    anyhow::bail!("protected transport credentials require Unix ownership and mode checks")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_directory(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "lattice-transport-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[cfg(unix)]
    #[test]
    fn credential_round_trip_enforces_private_modes() {
        let directory = test_directory("round-trip");
        ensure_private_directory(&directory).unwrap();
        let path = credential_path_in(&directory, "127.0.0.1:1234");
        let credential = TransportCredential {
            protocol_version: TRANSPORT_PROTOCOL_VERSION,
            daemon_epoch: random_hex(32).unwrap(),
            transport_token: random_hex(32).unwrap(),
            listener_address: "127.0.0.1:1234".into(),
            daemon_pid: 42,
        };
        write_credential_path(&path, &credential).unwrap();
        assert_eq!(
            std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let loaded = read_credential_path(&path).unwrap();
        assert_eq!(loaded.transport_token, credential.transport_token);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn unsafe_credential_mode_and_symlink_fail_closed() {
        let directory = test_directory("unsafe");
        ensure_private_directory(&directory).unwrap();
        let path = credential_path_in(&directory, "127.0.0.1:1234");
        std::fs::write(&path, b"{}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_credential_path(&path)
            .err()
            .expect("unsafe mode must fail")
            .to_string()
            .contains("0600"));
        std::fs::remove_file(&path).unwrap();
        let target = directory.join("target");
        std::fs::write(&target, b"{}").unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(read_credential_path(&path).is_err());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn unsafe_runtime_directory_mode_fails_closed() {
        let directory = test_directory("unsafe-directory");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(ensure_private_directory(&directory)
            .unwrap_err()
            .to_string()
            .contains("0700"));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn credential_address_and_random_fields_are_validated() {
        let mut credential = TransportCredential {
            protocol_version: TRANSPORT_PROTOCOL_VERSION,
            daemon_epoch: "11".repeat(32),
            transport_token: "22".repeat(32),
            listener_address: "127.0.0.1:1234".into(),
            daemon_pid: 42,
        };
        assert!(validate_for_address(&credential, "127.0.0.1:9999")
            .unwrap_err()
            .to_string()
            .contains("different daemon address"));
        credential.transport_token = "short".into();
        assert!(validate_for_address(&credential, "127.0.0.1:1234")
            .unwrap_err()
            .to_string()
            .contains("malformed"));
    }
}
