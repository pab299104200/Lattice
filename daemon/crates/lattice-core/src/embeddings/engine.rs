use anyhow::{Context, Result};
use ndarray::Array2;
use ort::session::Session;
use ort::value::Tensor;
use std::any::Any;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tokenizers::Tokenizer;

pub struct EmbeddingEngine {
    session: Mutex<Session>,
    tokenizer: Tokenizer,
}

impl EmbeddingEngine {
    /// Create a new EmbeddingEngine by loading an ONNX model and its associated tokenizer.
    ///
    /// The `model_path` should point to an ONNX model file (e.g., all-MiniLM-L6-v2.onnx).
    /// The tokenizer.json file is expected to be in the same directory as the model.
    pub fn new(model_path: &str) -> Result<Self> {
        initialize_onnx_runtime()?;
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            Self::new_after_runtime_load(model_path)
        }))
        .map_err(|panic| {
            anyhow::anyhow!(
                "ONNX embedding engine initialization panicked: {}; semantic retrieval is disabled and lexical retrieval remains available",
                panic_payload_message(panic.as_ref())
            )
        })?
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

        Ok(Self {
            session: Mutex::new(session),
            tokenizer,
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
        384
    }
}

/// Loads ONNX Runtime before any `ort` API can invoke its implicit, panicking
/// loader. The explicit path turns an absent or incompatible shared library
/// into the normal semantic-to-lexical fallback rather than a daemon crash.
fn initialize_onnx_runtime() -> Result<()> {
    let runtime_path = onnx_runtime_library_path();
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

fn onnx_runtime_library_path() -> PathBuf {
    if let Some(path) = std::env::var_os("ORT_DYLIB_PATH").filter(|value| !value.is_empty()) {
        return PathBuf::from(path);
    }

    #[cfg(target_os = "windows")]
    let default_name = "onnxruntime.dll";
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
    let default_name = "libonnxruntime.so";
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    let default_name = "libonnxruntime.dylib";

    PathBuf::from(default_name)
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
    use super::{bounded_error, initialize_onnx_runtime_with};
    use std::path::Path;

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
}
