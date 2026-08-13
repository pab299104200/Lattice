#[cfg(test)]
mod tests {
    use anyhow::{bail, Result};
    use std::fs;
    use std::path::Path;
    use tempfile::tempdir;

    use crate::embeddings::{
        install_embedding_model_at, EmbeddingEngine, EmbeddingModelAsset, EmbeddingModelDownloader,
        EmbeddingModelInstallStatus, EmbeddingModelManifest,
    };

    const FIXTURE_ASSETS: [EmbeddingModelAsset; 2] = [
        EmbeddingModelAsset {
            file_name: "model.onnx",
            source_url: "fixture://model",
            sha256: "85859949cad0dc38d596699174004f0b75944afc8fd7228caedbaa552fc7701d",
        },
        EmbeddingModelAsset {
            file_name: "tokenizer.json",
            source_url: "fixture://tokenizer",
            sha256: "faabab27305405b709c315f83e7fc0e1bbf3204b09d10e1a2efb37f4b2c109b4",
        },
    ];
    const FIXTURE_MANIFEST: EmbeddingModelManifest = EmbeddingModelManifest {
        version: "fixture-minilm",
        assets: &FIXTURE_ASSETS,
    };

    struct FixtureDownloader;

    impl EmbeddingModelDownloader for FixtureDownloader {
        fn download(&self, source_url: &str, destination: &Path) -> Result<()> {
            let bytes = match source_url {
                "fixture://model" => b"model fixture\n".as_slice(),
                "fixture://tokenizer" => b"tokenizer fixture\n".as_slice(),
                other => bail!("unexpected local fixture URL {other}"),
            };
            fs::write(destination, bytes)?;
            Ok(())
        }
    }

    #[test]
    fn local_fixture_installer_is_atomic_verified_and_idempotent() {
        let root = tempdir().unwrap();
        let destination = root.path().join("models/fixture-minilm");

        let first = install_embedding_model_at(&destination, &FIXTURE_MANIFEST, &FixtureDownloader)
            .unwrap();
        assert_eq!(
            first,
            EmbeddingModelInstallStatus::Installed(destination.clone())
        );
        assert_eq!(
            fs::read(destination.join("model.onnx")).unwrap(),
            b"model fixture\n"
        );

        let second =
            install_embedding_model_at(&destination, &FIXTURE_MANIFEST, &FixtureDownloader)
                .unwrap();
        assert_eq!(
            second,
            EmbeddingModelInstallStatus::AlreadyInstalled(destination)
        );
    }

    #[test]
    fn local_fixture_installer_replaces_corrupt_bundle_only_after_verification() {
        let root = tempdir().unwrap();
        let destination = root.path().join("models/fixture-minilm");
        fs::create_dir_all(&destination).unwrap();
        fs::write(destination.join("model.onnx"), b"corrupt").unwrap();
        fs::write(destination.join("tokenizer.json"), b"corrupt").unwrap();

        let result =
            install_embedding_model_at(&destination, &FIXTURE_MANIFEST, &FixtureDownloader)
                .unwrap();
        assert_eq!(
            result,
            EmbeddingModelInstallStatus::Installed(destination.clone())
        );
        assert_eq!(
            fs::read(destination.join("tokenizer.json")).unwrap(),
            b"tokenizer fixture\n"
        );
        assert!(fs::read_dir(root.path().join("models"))
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| entry.file_name().to_string_lossy().contains(".corrupt")));
    }

    #[test]
    #[ignore] // Requires ONNX model file
    fn test_embed_single_text() {
        let engine = EmbeddingEngine::new("models/all-MiniLM-L6-v2.onnx").unwrap();
        let embedding = engine
            .embed("function loginUser authenticates a user")
            .unwrap();
        assert_eq!(embedding.len(), 384);
        let norm: f32 = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 0.1);
    }

    #[test]
    #[ignore] // Requires ONNX model file
    fn test_embed_batch() {
        let engine = EmbeddingEngine::new("models/all-MiniLM-L6-v2.onnx").unwrap();
        let texts = vec!["authentication login", "database query", "HTTP handler"];
        let embeddings = engine.embed_batch(&texts).unwrap();
        assert_eq!(embeddings.len(), 3);
        assert_eq!(embeddings[0].len(), 384);
    }

    #[test]
    #[ignore] // Requires ONNX model file
    fn test_semantic_similarity() {
        let engine = EmbeddingEngine::new("models/all-MiniLM-L6-v2.onnx").unwrap();
        let auth_vec = engine.embed("user authentication login password").unwrap();
        let validate_vec = engine.embed("validate credentials check password").unwrap();
        let database_vec = engine.embed("SQL database query table insert").unwrap();

        let auth_validate_sim = cosine_similarity(&auth_vec, &validate_vec);
        let auth_database_sim = cosine_similarity(&auth_vec, &database_vec);

        assert!(auth_validate_sim > auth_database_sim);
    }

    fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
        let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
        let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm_a == 0.0 || norm_b == 0.0 {
            return 0.0;
        }
        dot / (norm_a * norm_b)
    }
}
