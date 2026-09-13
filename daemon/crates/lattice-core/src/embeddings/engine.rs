use anyhow::{Context, Result};
use ndarray::Array2;
use ort::session::Session;
use ort::value::Tensor;
use sha2::{Digest, Sha256};
use std::any::Any;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use tokenizers::Tokenizer;

use super::object_cache::{EmbeddingCacheStats, EmbeddingIdentity, EmbeddingObjectCache};

pub const EMBEDDING_NORMALIZATION_VERSION: &str = "mean-pool-l2-v1";
pub const EMBEDDING_PREPROCESSING_VERSION: &str = "tokenizer-json-special-tokens-v1";

static PROCESS_EMBEDDING_RUNTIME: OnceLock<ProcessEmbeddingRuntime<EmbeddingEngine>> =
    OnceLock::new();

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmbeddingRuntimeStatus {
    Uninitialized,
    Available,
    Disabled { reason: String },
}

enum ProcessEmbeddingState<T> {
    Uninitialized,
    Available(Arc<T>),
    Disabled(String),
}

struct ProcessEmbeddingRuntime<T> {
    state: Mutex<ProcessEmbeddingState<T>>,
}

impl<T> ProcessEmbeddingRuntime<T> {
    fn new() -> Self {
        Self {
            state: Mutex::new(ProcessEmbeddingState::Uninitialized),
        }
    }

    fn state(&self) -> MutexGuard<'_, ProcessEmbeddingState<T>> {
        match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => {
                // A panic must not turn the semantic fallback coordinator into
                // a second panic source. Permanently close the process-wide
                // circuit and recover the protected state only to record why.
                let mut state = poisoned.into_inner();
                *state = ProcessEmbeddingState::Disabled(
                    "embedding runtime coordination failed; semantic retrieval is disabled and lexical retrieval remains available"
                        .to_string(),
                );
                state
            }
        }
    }

    fn get_or_try_init(
        &self,
        initializer: impl FnOnce() -> std::result::Result<T, String>,
    ) -> std::result::Result<Arc<T>, String> {
        let mut state = self.state();
        match &*state {
            ProcessEmbeddingState::Available(value) => return Ok(Arc::clone(value)),
            ProcessEmbeddingState::Disabled(reason) => return Err(reason.clone()),
            ProcessEmbeddingState::Uninitialized => {}
        }

        let initialized = std::panic::catch_unwind(std::panic::AssertUnwindSafe(initializer));
        match initialized {
            Ok(Ok(value)) => {
                let value = Arc::new(value);
                *state = ProcessEmbeddingState::Available(Arc::clone(&value));
                Ok(value)
            }
            Ok(Err(reason)) => {
                let reason = bounded_error(&reason);
                *state = ProcessEmbeddingState::Disabled(reason.clone());
                Err(reason)
            }
            Err(panic) => {
                let reason = format!(
                    "ONNX embedding engine initialization panicked: {}; semantic retrieval is disabled and lexical retrieval remains available",
                    panic_payload_message(panic.as_ref())
                );
                let reason = bounded_error(&reason);
                *state = ProcessEmbeddingState::Disabled(reason.clone());
                Err(reason)
            }
        }
    }

    fn status(&self) -> EmbeddingRuntimeStatus {
        match &*self.state() {
            ProcessEmbeddingState::Uninitialized => EmbeddingRuntimeStatus::Uninitialized,
            ProcessEmbeddingState::Available(_) => EmbeddingRuntimeStatus::Available,
            ProcessEmbeddingState::Disabled(reason) => EmbeddingRuntimeStatus::Disabled {
                reason: reason.clone(),
            },
        }
    }
}

pub struct EmbeddingEngine {
    session: Mutex<Session>,
    tokenizer: Tokenizer,
    identity: EmbeddingIdentity,
}

impl EmbeddingEngine {
    /// Create a new EmbeddingEngine by loading an ONNX model and its associated tokenizer.
    ///
    /// The `model_path` should point to an ONNX model file (e.g., all-MiniLM-L6-v2.onnx).
    /// The tokenizer.json file is expected to be in the same directory as the model.
    pub fn new(model_path: &str) -> Result<Arc<Self>> {
        PROCESS_EMBEDDING_RUNTIME
            .get_or_init(ProcessEmbeddingRuntime::new)
            .get_or_try_init(|| {
                initialize_onnx_runtime()
                    .and_then(|_| Self::new_after_runtime_load(model_path))
                    .map_err(|error| error.to_string())
            })
            .map_err(anyhow::Error::msg)
    }

    fn new_after_runtime_load(model_path: &str) -> Result<Self> {
        let session = Session::builder()
            .map_err(|e| anyhow::anyhow!("Failed to create session builder: {}", e))?
            .with_intra_threads(4)
            .map_err(|e| anyhow::anyhow!("Failed to set intra threads: {}", e))?
            .commit_from_file(model_path)
            .map_err(|e| anyhow::anyhow!("Failed to load ONNX model: {}", e))?;

        let model_dir = std::path::Path::new(model_path)
            .parent()
            .unwrap_or(std::path::Path::new("."));
        let tokenizer_path = model_dir.join("tokenizer.json");
        let tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| anyhow::anyhow!("Failed to load tokenizer: {}", e))?;

        let identity = EmbeddingIdentity {
            model_artifact_sha256: sha256_file(Path::new(model_path))?,
            tokenizer_sha256: sha256_file(&tokenizer_path)?,
            dimension: 384,
            normalization_version: EMBEDDING_NORMALIZATION_VERSION.to_string(),
            preprocessing_version: EMBEDDING_PREPROCESSING_VERSION.to_string(),
        };
        Ok(Self {
            session: Mutex::new(session),
            tokenizer,
            identity,
        })
    }

    /// Embed a single text string, returning a 384-dimensional L2-normalized vector.
    pub fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let batch = self.embed_batch(&[text])?;
        Ok(batch.into_iter().next().unwrap())
    }

    /// Embed a batch of text strings, returning a vector of 384-dimensional L2-normalized vectors.
    ///
    /// Uses mean pooling with attention mask and L2 normalization.
    pub fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let batch_size = texts.len();
        if batch_size == 0 {
            return Ok(Vec::new());
        }

        // 1. Tokenize batch
        let encodings = self
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(|e| anyhow::anyhow!("Tokenization failed: {}", e))?;

        // 2. Find max sequence length and build padded input arrays
        let max_len = encodings
            .iter()
            .map(|e| e.get_ids().len())
            .max()
            .unwrap_or(0);

        let mut input_ids_data = vec![0i64; batch_size * max_len];
        let mut attention_mask_data = vec![0i64; batch_size * max_len];
        let mut token_type_ids_data = vec![0i64; batch_size * max_len];

        for (i, encoding) in encodings.iter().enumerate() {
            let ids = encoding.get_ids();
            let mask = encoding.get_attention_mask();
            let type_ids = encoding.get_type_ids();
            let seq_len = ids.len();

            for j in 0..seq_len {
                input_ids_data[i * max_len + j] = ids[j] as i64;
                attention_mask_data[i * max_len + j] = mask[j] as i64;
                token_type_ids_data[i * max_len + j] = type_ids[j] as i64;
            }
            // Remaining positions stay 0 (padding)
        }

        let shape = vec![batch_size as i64, max_len as i64];

        // 3. Create ORT tensors
        let input_ids_tensor = Tensor::from_array((shape.clone(), input_ids_data))
            .map_err(|e| anyhow::anyhow!("Failed to create input_ids tensor: {}", e))?;
        let attention_mask_tensor =
            Tensor::from_array((shape.clone(), attention_mask_data.clone()))
                .map_err(|e| anyhow::anyhow!("Failed to create attention_mask tensor: {}", e))?;
        let token_type_ids_tensor = Tensor::from_array((shape.clone(), token_type_ids_data))
            .map_err(|e| anyhow::anyhow!("Failed to create token_type_ids tensor: {}", e))?;

        // 4. Run ONNX session
        let mut session = self
            .session
            .lock()
            .map_err(|e| anyhow::anyhow!("Session lock poisoned: {}", e))?;

        let outputs = session
            .run(ort::inputs! {
                "input_ids" => input_ids_tensor,
                "attention_mask" => attention_mask_tensor,
                "token_type_ids" => token_type_ids_tensor
            })
            .map_err(|e| anyhow::anyhow!("ONNX inference failed: {}", e))?;

        // 5. Extract token embeddings from output
        // Output shape: (batch_size, seq_len, hidden_dim)
        let (output_shape, output_data) = outputs[0]
            .try_extract_tensor::<f32>()
            .map_err(|e| anyhow::anyhow!("Failed to extract output tensor: {}", e))?;

        let dims: &[i64] = &**output_shape;
        let hidden_dim = dims[2] as usize;
        let seq_len = dims[1] as usize;

        // 6. Mean pooling with attention mask
        let attention_mask = Array2::from_shape_vec(
            (batch_size, max_len),
            attention_mask_data.iter().map(|&v| v as f32).collect(),
        )
        .context("Failed to create attention mask array")?;

        let mut results = Vec::with_capacity(batch_size);

        for b in 0..batch_size {
            let mut pooled = vec![0.0f32; hidden_dim];
            let mut mask_sum = 0.0f32;

            for s in 0..seq_len {
                let mask_val = attention_mask[[b, s]];
                mask_sum += mask_val;
                for d in 0..hidden_dim {
                    let idx = b * seq_len * hidden_dim + s * hidden_dim + d;
                    pooled[d] += output_data[idx] * mask_val;
                }
            }

            // Divide by mask sum (avoid division by zero)
            if mask_sum > 0.0 {
                for d in 0..hidden_dim {
                    pooled[d] /= mask_sum;
                }
            }

            // 7. L2 normalize
            let norm: f32 = pooled.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm > 0.0 {
                for d in 0..hidden_dim {
                    pooled[d] /= norm;
                }
            }

            results.push(pooled);
        }

        Ok(results)
    }

    /// Returns the dimensionality of the embedding vectors (384 for all-MiniLM-L6-v2).
    pub fn dimension(&self) -> usize {
        self.identity.dimension
    }

    pub fn identity(&self) -> &EmbeddingIdentity {
        &self.identity
    }
}

/// Cache-aware embedding facade used by repository vector synchronization.
/// Cache failures are returned to the caller so semantic work can degrade while
/// the exact lexical/path index remains usable.
pub struct CachedEmbeddingEngine {
    engine: Arc<EmbeddingEngine>,
    cache: EmbeddingObjectCache,
    checkout_id: String,
}

pub trait EmbeddingProvider: Send + Sync {
    fn storage_identity(&self) -> Result<Option<String>> {
        Ok(None)
    }

    fn publication_lease(&self) -> Result<Option<std::fs::File>> {
        Ok(None)
    }
    fn embed(&self, text: &str) -> Result<Vec<f32>>;
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;
    fn object_key(&self, _text: &str) -> Result<Option<String>> {
        Ok(None)
    }
    fn publish_membership(
        &self,
        _members: &BTreeMap<String, String>,
        _replace: bool,
        _remove_prefixes: &[String],
    ) -> Result<()> {
        Ok(())
    }
}

impl EmbeddingProvider for EmbeddingEngine {
    fn storage_identity(&self) -> Result<Option<String>> {
        Ok(Some(format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(self.identity())?)
        )))
    }

    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        EmbeddingEngine::embed(self, text)
    }
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        EmbeddingEngine::embed_batch(self, texts)
    }
}

impl CachedEmbeddingEngine {
    pub fn new(
        engine: Arc<EmbeddingEngine>,
        cache: EmbeddingObjectCache,
        checkout_id: String,
    ) -> Self {
        Self {
            engine,
            cache,
            checkout_id,
        }
    }

    pub fn embed(&self, text: &str) -> Result<Vec<f32>> {
        Ok(self.embed_batch(&[text])?.0.into_iter().next().unwrap())
    }

    pub fn embed_batch(&self, texts: &[&str]) -> Result<(Vec<Vec<f32>>, EmbeddingCacheStats)> {
        self.cache
            .get_or_compute_batch(self.engine.identity(), texts, |missing| {
                self.engine.embed_batch(missing)
            })
    }

    pub fn identity(&self) -> &EmbeddingIdentity {
        self.engine.identity()
    }

    pub fn cache(&self) -> &EmbeddingObjectCache {
        &self.cache
    }
}

impl EmbeddingProvider for CachedEmbeddingEngine {
    fn storage_identity(&self) -> Result<Option<String>> {
        self.engine.storage_identity()
    }

    fn publication_lease(&self) -> Result<Option<std::fs::File>> {
        Ok(Some(self.cache.publication_lease()?))
    }
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        CachedEmbeddingEngine::embed(self, text)
    }
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        Ok(CachedEmbeddingEngine::embed_batch(self, texts)?.0)
    }
    fn object_key(&self, text: &str) -> Result<Option<String>> {
        Ok(Some(EmbeddingObjectCache::object_key(
            self.identity(),
            text,
        )?))
    }
    fn publish_membership(
        &self,
        members: &BTreeMap<String, String>,
        replace: bool,
        remove_prefixes: &[String],
    ) -> Result<()> {
        if replace {
            self.cache
                .replace_checkout_membership(&self.checkout_id, members)
        } else {
            self.cache
                .update_checkout_membership(&self.checkout_id, remove_prefixes, members)
        }
    }
}

fn sha256_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("read embedding identity asset `{}`", path.display()))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

/// Process-wide semantic runtime state. All workspace shards share this
/// circuit breaker because `ort` dynamically loads one process-global runtime.
pub fn embedding_runtime_status() -> EmbeddingRuntimeStatus {
    PROCESS_EMBEDDING_RUNTIME
        .get_or_init(ProcessEmbeddingRuntime::new)
        .status()
}

/// Loads ONNX Runtime before any `ort` API can invoke its implicit, panicking
/// loader. The explicit path turns an absent or incompatible shared library
/// into the normal semantic-to-lexical fallback rather than a daemon crash.
fn initialize_onnx_runtime() -> Result<()> {
    let runtime_path = onnx_runtime_library_path()?;
    initialize_onnx_runtime_with(&runtime_path, |path| {
        ort::init_from(path)
            .map(|builder| {
                builder.commit();
            })
            .map_err(|error| error.to_string())
    })
}

fn initialize_onnx_runtime_with(
    runtime_path: &Path,
    loader: impl FnOnce(&Path) -> std::result::Result<(), String>,
) -> Result<()> {
    loader(runtime_path).map_err(|error| {
        anyhow::anyhow!(
            "ONNX Runtime is unavailable at `{}`: {}; semantic retrieval is disabled and lexical retrieval remains available. Install a compatible ONNX Runtime library beside the lattice executable or set ORT_DYLIB_PATH to its absolute path",
            runtime_path.display(),
            bounded_error(&error)
        )
    })
}

fn onnx_runtime_library_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("ORT_DYLIB_PATH").filter(|value| !value.is_empty()) {
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            anyhow::bail!(
                "ORT_DYLIB_PATH must be an absolute path, got `{}`; semantic retrieval is disabled and lexical retrieval remains available",
                path.display()
            );
        }
        return validate_onnx_runtime_library(path);
    }

    #[cfg(target_os = "windows")]
    let default_name = "onnxruntime.dll";
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
    let default_name = "libonnxruntime.so";
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    let default_name = "libonnxruntime.dylib";

    let executable = std::env::current_exe()
        .context("resolve the lattice executable while locating ONNX Runtime")?;
    let executable_dir = executable.parent().ok_or_else(|| {
        anyhow::anyhow!(
            "lattice executable `{}` has no parent directory; semantic retrieval is disabled and lexical retrieval remains available",
            executable.display()
        )
    })?;
    validate_onnx_runtime_library(executable_dir.join(default_name))
}

fn validate_onnx_runtime_library(path: PathBuf) -> Result<PathBuf> {
    match std::fs::metadata(&path) {
        Ok(metadata) if metadata.is_file() => Ok(path),
        Ok(_) => anyhow::bail!(
            "ONNX Runtime path `{}` is not a regular file; semantic retrieval is disabled and lexical retrieval remains available",
            path.display()
        ),
        Err(error) => anyhow::bail!(
            "ONNX Runtime is unavailable at `{}`: {}; semantic retrieval is disabled and lexical retrieval remains available. Install a compatible ONNX Runtime library beside the lattice executable or set ORT_DYLIB_PATH to its absolute path",
            path.display(),
            bounded_error(&error.to_string())
        ),
    }
}

fn bounded_error(error: &str) -> String {
    const MAX_ERROR_CHARS: usize = 400;
    if error.chars().count() <= MAX_ERROR_CHARS {
        return error.to_string();
    }
    let prefix: String = error.chars().take(MAX_ERROR_CHARS).collect();
    format!("{prefix}…")
}

fn panic_payload_message(payload: &(dyn Any + Send)) -> String {
    let message = payload
        .downcast_ref::<&str>()
        .map(|value| (*value).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".to_string());
    bounded_error(&message)
}

#[cfg(test)]
mod tests {
    use super::{
        bounded_error, initialize_onnx_runtime_with, validate_onnx_runtime_library,
        EmbeddingRuntimeStatus, ProcessEmbeddingRuntime,
    };
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn missing_runtime_is_an_actionable_lexical_fallback() {
        let error =
            initialize_onnx_runtime_with(Path::new("/missing/libonnxruntime.dylib"), |_| {
                Err("library not found".to_string())
            })
            .expect_err("a missing ONNX Runtime must not be treated as available");
        let message = error.to_string();

        assert!(message.contains("ONNX Runtime is unavailable"));
        assert!(message.contains("semantic retrieval is disabled"));
        assert!(message.contains("lexical retrieval remains available"));
        assert!(message.contains("ORT_DYLIB_PATH"));
    }

    #[test]
    fn runtime_failure_reason_is_bounded() {
        let error = "x".repeat(500);
        let bounded = bounded_error(&error);
        assert!(bounded.ends_with('…'));
        assert_eq!(bounded.chars().count(), 401);
    }

    #[test]
    fn missing_runtime_is_rejected_before_dynamic_loader() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("libonnxruntime-missing.so");
        let error = validate_onnx_runtime_library(missing.clone())
            .expect_err("missing runtime must be rejected before ORT is called");
        assert!(error.to_string().contains(&missing.display().to_string()));
        assert!(error
            .to_string()
            .contains("lexical retrieval remains available"));
    }

    #[test]
    fn concurrent_runtime_failure_is_attempted_once_and_never_panics() {
        let runtime = Arc::new(ProcessEmbeddingRuntime::<usize>::new());
        let attempts = Arc::new(AtomicUsize::new(0));
        let mut workers = Vec::new();

        for _ in 0..16 {
            let runtime = Arc::clone(&runtime);
            let attempts = Arc::clone(&attempts);
            workers.push(std::thread::spawn(move || {
                runtime.get_or_try_init(|| {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    Err("incompatible ONNX Runtime".to_string())
                })
            }));
        }

        for worker in workers {
            let error = worker
                .join()
                .expect("runtime failure must not escape as a panic")
                .expect_err("every shard must receive lexical fallback");
            assert_eq!(error, "incompatible ONNX Runtime");
        }
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert_eq!(
            runtime.status(),
            EmbeddingRuntimeStatus::Disabled {
                reason: "incompatible ONNX Runtime".to_string()
            }
        );
    }

    #[test]
    fn panicking_runtime_is_permanently_disabled_without_poisoning_gate() {
        let runtime = ProcessEmbeddingRuntime::<usize>::new();
        let attempts = AtomicUsize::new(0);

        let first = runtime.get_or_try_init(|| {
            attempts.fetch_add(1, Ordering::SeqCst);
            panic!("invalid ORT handle")
        });
        let second = runtime.get_or_try_init(|| {
            attempts.fetch_add(1, Ordering::SeqCst);
            Ok(42)
        });

        let first = first.expect_err("panic must close the semantic circuit");
        let second = second.expect_err("closed circuit must never retry ORT");
        assert!(first.contains("initialization panicked"));
        assert!(first.contains("invalid ORT handle"));
        assert_eq!(second, first);
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn concurrent_runtime_success_is_shared_across_shards() {
        let runtime = Arc::new(ProcessEmbeddingRuntime::<usize>::new());
        let attempts = Arc::new(AtomicUsize::new(0));
        let mut workers = Vec::new();

        for _ in 0..8 {
            let runtime = Arc::clone(&runtime);
            let attempts = Arc::clone(&attempts);
            workers.push(std::thread::spawn(move || {
                runtime
                    .get_or_try_init(|| {
                        attempts.fetch_add(1, Ordering::SeqCst);
                        Ok(42)
                    })
                    .expect("shared initialization succeeds")
            }));
        }

        let values: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().expect("worker must not panic"))
            .collect();
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert!(values.iter().all(|value| **value == 42));
        assert!(values
            .windows(2)
            .all(|pair| Arc::ptr_eq(&pair[0], &pair[1])));
    }
}
