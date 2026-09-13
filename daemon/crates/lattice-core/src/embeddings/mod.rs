pub mod engine;
pub mod object_cache;

#[cfg(test)]
mod tests;

pub use engine::{
    embedding_runtime_status, CachedEmbeddingEngine, EmbeddingEngine, EmbeddingProvider,
    EmbeddingRuntimeStatus, EMBEDDING_NORMALIZATION_VERSION, EMBEDDING_PREPROCESSING_VERSION,
};
pub use object_cache::{
    EmbeddingCacheStats, EmbeddingGcReport, EmbeddingIdentity, EmbeddingObjectCache,
};

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// The pinned, compatible MiniLM bundle used by the ONNX embedding engine.
/// The bundle is shared by every workspace under the user's Lattice data.
const EMBEDDING_MODEL_VERSION: &str = "all-minilm-l6-v2-1110a243";
const MODEL_FILE_NAME: &str = "model.onnx";
const TOKENIZER_FILE_NAME: &str = "tokenizer.json";
const MODEL_SHA256: &str = "6fd5d72fe4589f189f8ebc006442dbb529bb7ce38f8082112682524616046452";
const TOKENIZER_SHA256: &str = "be50c3628f2bf5bb5e3a7f17b1f74611b2561a3a27eeab05e5aa30f411572037";
const MODEL_URL: &str = "https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2/resolve/1110a243fdf4706b3f48f1d95db1a4f5529b4d41/onnx/model.onnx";
const TOKENIZER_URL: &str = "https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2/resolve/1110a243fdf4706b3f48f1d95db1a4f5529b4d41/tokenizer.json";

#[derive(Debug, Clone, Copy)]
pub struct EmbeddingModelAsset {
    pub file_name: &'static str,
    pub source_url: &'static str,
    pub sha256: &'static str,
}

#[derive(Debug, Clone, Copy)]
pub struct EmbeddingModelManifest {
    pub version: &'static str,
    pub assets: &'static [EmbeddingModelAsset],
}

const DEFAULT_MODEL_ASSETS: [EmbeddingModelAsset; 2] = [
    EmbeddingModelAsset {
        file_name: MODEL_FILE_NAME,
        source_url: MODEL_URL,
        sha256: MODEL_SHA256,
    },
    EmbeddingModelAsset {
        file_name: TOKENIZER_FILE_NAME,
        source_url: TOKENIZER_URL,
        sha256: TOKENIZER_SHA256,
    },
];

pub const DEFAULT_EMBEDDING_MODEL: EmbeddingModelManifest = EmbeddingModelManifest {
    version: EMBEDDING_MODEL_VERSION,
    assets: &DEFAULT_MODEL_ASSETS,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmbeddingModelInstallStatus {
    Installed(PathBuf),
    AlreadyInstalled(PathBuf),
}

/// The test seam keeps model provisioning tests fully local and deterministic.
pub trait EmbeddingModelDownloader {
    fn download(&self, source_url: &str, destination: &Path) -> Result<()>;
}

#[derive(Debug, Default)]
pub struct CurlEmbeddingModelDownloader;

impl EmbeddingModelDownloader for CurlEmbeddingModelDownloader {
    fn download(&self, source_url: &str, destination: &Path) -> Result<()> {
        let status = Command::new("curl")
            .args(["--fail", "--location", "--silent", "--show-error"])
            .arg("--output")
            .arg(destination)
            .arg(source_url)
            .status()
            .context("start curl to download the Lattice embedding model")?;
        if !status.success() {
            bail!("curl failed while downloading embedding asset `{source_url}` ({status})");
        }
        Ok(())
    }
}

/// Return the shared model directory. An explicit environment override supports
/// managed installs without changing the default user-level location.
pub fn shared_embedding_model_dir() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("LATTICE_EMBEDDING_MODEL_DIR") {
        return Ok(PathBuf::from(path));
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .ok_or_else(|| {
            anyhow!("cannot determine a home directory for the shared embedding model")
        })?;
    Ok(PathBuf::from(home)
        .join(".lattice")
        .join("models")
        .join(EMBEDDING_MODEL_VERSION))
}

/// Provision the pinned shared bundle. Existing complete bundles are reused;
/// partial or corrupt bundles are quarantined and replaced by a fully verified
/// sibling staging directory through an atomic rename.
pub fn install_shared_embedding_model() -> Result<EmbeddingModelInstallStatus> {
    let destination = shared_embedding_model_dir()?;
    install_embedding_model_at(
        &destination,
        &DEFAULT_EMBEDDING_MODEL,
        &CurlEmbeddingModelDownloader,
    )
}

pub fn install_embedding_model_at(
    destination: &Path,
    manifest: &EmbeddingModelManifest,
    downloader: &dyn EmbeddingModelDownloader,
) -> Result<EmbeddingModelInstallStatus> {
    validate_manifest(manifest)?;
    if validate_embedding_model_at(destination, manifest).is_ok() {
        return Ok(EmbeddingModelInstallStatus::AlreadyInstalled(
            destination.to_path_buf(),
        ));
    }
    let parent = destination.parent().ok_or_else(|| {
        anyhow!(
            "embedding model destination `{}` has no parent directory",
            destination.display()
        )
    })?;
    fs::create_dir_all(parent)
        .with_context(|| format!("create embedding model parent `{}`", parent.display()))?;
    let nonce = unique_suffix();
    let staging = parent.join(format!(".{}-{nonce}.staging", manifest.version));
    fs::create_dir(&staging).with_context(|| {
        format!(
            "create embedding model staging directory `{}`",
            staging.display()
        )
    })?;
    let outcome: Result<()> = (|| {
        for asset in manifest.assets {
            let path = staging.join(asset.file_name);
            downloader
                .download(asset.source_url, &path)
                .with_context(|| format!("download embedding asset `{}`", asset.file_name))?;
            verify_asset(&path, asset)?;
        }
        validate_embedding_model_at(&staging, manifest)?;
        if destination.exists() {
            let quarantine = parent.join(format!(".{}-{nonce}.corrupt", manifest.version));
            fs::rename(destination, &quarantine).with_context(|| {
                format!(
                    "quarantine incomplete or corrupt embedding model `{}`",
                    destination.display()
                )
            })?;
        }
        fs::rename(&staging, destination).with_context(|| {
            format!(
                "atomically activate embedding model at `{}`",
                destination.display()
            )
        })?;
        Ok(())
    })();
    if outcome.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    outcome?;
    Ok(EmbeddingModelInstallStatus::Installed(
        destination.to_path_buf(),
    ))
}

/// Resolve only a complete, checksum-verified bundle. Failures deliberately
/// return `None`, so daemon startup keeps lexical retrieval available.
pub fn verified_shared_embedding_model_path() -> Option<PathBuf> {
    let directory = shared_embedding_model_dir().ok()?;
    validate_embedding_model_at(&directory, &DEFAULT_EMBEDDING_MODEL)
        .ok()
        .map(|_| directory.join(MODEL_FILE_NAME))
}

fn validate_manifest(manifest: &EmbeddingModelManifest) -> Result<()> {
    if manifest.version.is_empty() || manifest.assets.is_empty() {
        bail!("embedding model manifest must have a version and at least one asset");
    }
    for asset in manifest.assets {
        if asset.file_name.is_empty()
            || Path::new(asset.file_name).components().count() != 1
            || asset.sha256.len() != 64
            || !asset.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            bail!("embedding model manifest contains an invalid asset entry");
        }
    }
    Ok(())
}

fn validate_embedding_model_at(
    destination: &Path,
    manifest: &EmbeddingModelManifest,
) -> Result<()> {
    validate_manifest(manifest)?;
    if !destination.is_dir() {
        bail!(
            "embedding model directory `{}` is missing",
            destination.display()
        );
    }
    for asset in manifest.assets {
        verify_asset(&destination.join(asset.file_name), asset)?;
    }
    Ok(())
}

fn verify_asset(path: &Path, asset: &EmbeddingModelAsset) -> Result<()> {
    let metadata =
        fs::metadata(path).with_context(|| format!("read embedding asset `{}`", path.display()))?;
    if !metadata.is_file() {
        bail!("embedding asset `{}` is not a regular file", path.display());
    }
    let actual = sha256_file(path)?;
    if !actual.eq_ignore_ascii_case(asset.sha256) {
        bail!(
            "embedding asset `{}` failed checksum verification (expected {}, got {})",
            path.display(),
            asset.sha256,
            actual
        );
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)
        .with_context(|| format!("open embedding asset `{}`", path.display()))?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let bytes = file
            .read(&mut buffer)
            .with_context(|| format!("read embedding asset `{}`", path.display()))?;
        if bytes == 0 {
            break;
        }
        hash.update(&buffer[..bytes]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn unique_suffix() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{}-{now}", std::process::id())
}

const FINGERPRINT_DIMENSIONS: usize = 64;

pub fn fingerprint_text(text: &str) -> Vec<f32> {
    let mut vector = vec![0.0f32; FINGERPRINT_DIMENSIONS];
    for token in text
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|token| token.len() >= 2)
    {
        let normalized = token.to_ascii_lowercase();
        let mut hash = 1469598103934665603u64;
        for byte in normalized.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(1099511628211);
        }
        let index = (hash as usize) % FINGERPRINT_DIMENSIONS;
        vector[index] += 1.0;
    }
    normalize(&mut vector);
    vector
}

pub fn cosine_similarity(left: &[f32], right: &[f32]) -> f32 {
    if left.len() != right.len() || left.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut left_norm = 0.0f32;
    let mut right_norm = 0.0f32;
    for (lhs, rhs) in left.iter().zip(right.iter()) {
        dot += lhs * rhs;
        left_norm += lhs * lhs;
        right_norm += rhs * rhs;
    }
    if left_norm == 0.0 || right_norm == 0.0 {
        0.0
    } else {
        dot / (left_norm.sqrt() * right_norm.sqrt())
    }
}

fn normalize(vector: &mut [f32]) {
    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    if norm > 0.0 {
        for value in vector.iter_mut() {
            *value /= norm;
        }
    }
}
