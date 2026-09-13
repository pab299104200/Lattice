#[cfg(test)]
mod tests {
    use anyhow::{bail, Result};
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};
    use std::thread;
    use tempfile::tempdir;

    use crate::embeddings::{
        install_embedding_model_at, EmbeddingEngine, EmbeddingIdentity, EmbeddingModelAsset,
        EmbeddingModelDownloader, EmbeddingModelInstallStatus, EmbeddingModelManifest,
        EmbeddingObjectCache,
    };

    fn identity(model_byte: u8) -> EmbeddingIdentity {
        EmbeddingIdentity {
            model_artifact_sha256: format!("{model_byte:02x}").repeat(32),
            tokenizer_sha256: "22".repeat(32),
            dimension: 3,
            normalization_version: "l2-v1".into(),
            preprocessing_version: "fixture-v1".into(),
        }
    }

    fn vector_for(text: &str) -> Vec<f32> {
        vec![
            text.len() as f32,
            text.bytes().map(u32::from).sum::<u32>() as f32,
            1.0,
        ]
    }

    #[test]
    fn ten_views_reuse_at_least_ninety_percent_of_embedding_inputs_and_bytes() {
        let root = tempdir().unwrap();
        let cache = EmbeddingObjectCache::open(root.path()).unwrap();
        let inputs = [
            "shared-a", "shared-b", "shared-c", "shared-d", "shared-e", "shared-f", "shared-g",
            "shared-h", "shared-i", "shared-j",
        ];
        let mut computed = 0;
        let mut hits = 0;
        let mut reused_bytes = 0;
        let mut written_bytes = 0;
        for checkout in 0..10 {
            let (vectors, stats) = cache
                .get_or_compute_batch(&identity(1), &inputs, |missing| {
                    computed += missing.len();
                    Ok(missing.iter().map(|text| vector_for(text)).collect())
                })
                .unwrap();
            assert_eq!(vectors.len(), inputs.len());
            hits += stats.object_hits;
            reused_bytes += stats.reused_bytes;
            written_bytes += stats.written_bytes;
            let members = inputs
                .iter()
                .enumerate()
                .map(|(i, text)| {
                    (
                        format!("symbol-{i}"),
                        EmbeddingObjectCache::object_key(&identity(1), text).unwrap(),
                    )
                })
                .collect::<BTreeMap<_, _>>();
            cache
                .replace_checkout_membership(&format!("checkout-{checkout}"), &members)
                .unwrap();
        }
        assert_eq!(computed, 10);
        assert_eq!(hits, 90);
        assert!(reused_bytes >= written_bytes * 9);
        assert_eq!(
            fs::read_dir(root.path().join("objects"))
                .unwrap()
                .map(Result::unwrap)
                .flat_map(|shard| fs::read_dir(shard.path()).unwrap())
                .count(),
            10
        );
    }

    #[test]
    fn duplicate_batch_inputs_compute_once_and_preserve_result_order() {
        let root = tempdir().unwrap();
        let cache = EmbeddingObjectCache::open(root.path()).unwrap();
        let inputs = ["a", "b", "a", "a", "b"];
        let (vectors, stats) = cache
            .get_or_compute_batch(&identity(1), &inputs, |missing| {
                assert_eq!(missing.len(), 2);
                Ok(missing.iter().map(|text| vector_for(text)).collect())
            })
            .unwrap();
        assert_eq!(stats.computed, 2);
        assert_eq!(vectors[0], vectors[2]);
        assert_eq!(vectors[1], vectors[4]);
    }

    #[test]
    fn concurrent_identical_requests_singleflight_one_compute() {
        let root = tempdir().unwrap();
        let barrier = Arc::new(Barrier::new(8));
        let computes = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let cache = EmbeddingObjectCache::open(root.path()).unwrap();
            let barrier = Arc::clone(&barrier);
            let computes = Arc::clone(&computes);
            handles.push(thread::spawn(move || {
                barrier.wait();
                cache
                    .get_or_compute_batch(&identity(1), &["same"], |missing| {
                        computes.fetch_add(1, Ordering::SeqCst);
                        thread::sleep(std::time::Duration::from_millis(20));
                        Ok(missing.iter().map(|text| vector_for(text)).collect())
                    })
                    .unwrap()
                    .0
                    .remove(0)
            }));
        }
        for handle in handles {
            assert_eq!(handle.join().unwrap(), vector_for("same"));
        }
        assert_eq!(computes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn equal_dimension_model_change_invalidates_object_identity() {
        let root = tempdir().unwrap();
        let cache = EmbeddingObjectCache::open(root.path()).unwrap();
        let first = cache
            .get_or_compute_batch(&identity(1), &["same"], |_| Ok(vec![vec![1.0, 0.0, 0.0]]))
            .unwrap();
        let second = cache
            .get_or_compute_batch(&identity(2), &["same"], |_| Ok(vec![vec![0.0, 1.0, 0.0]]))
            .unwrap();
        assert_eq!(first.1.computed, 1);
        assert_eq!(second.1.computed, 1);
        assert_ne!(
            EmbeddingObjectCache::object_key(&identity(1), "same").unwrap(),
            EmbeddingObjectCache::object_key(&identity(2), "same").unwrap()
        );
    }

    #[test]
    fn corrupt_object_and_interrupted_temporary_write_recompute_safely() {
        let root = tempdir().unwrap();
        let cache = EmbeddingObjectCache::open(root.path()).unwrap();
        cache
            .get_or_compute_batch(&identity(1), &["same"], |_| Ok(vec![vec![1.0, 0.0, 0.0]]))
            .unwrap();
        let key = EmbeddingObjectCache::object_key(&identity(1), "same").unwrap();
        let shard = root.path().join("objects").join(&key[..2]);
        fs::write(shard.join(&key), b"truncated").unwrap();
        fs::write(shard.join(".interrupted.tmp"), b"partial").unwrap();
        let result = cache
            .get_or_compute_batch(&identity(1), &["same"], |_| Ok(vec![vec![0.0, 1.0, 0.0]]))
            .unwrap();
        assert_eq!(result.0[0], vec![0.0, 1.0, 0.0]);
        assert!(shard.join(".interrupted.tmp").exists());
    }

    #[test]
    fn gc_retains_checkout_membership_and_bounds_unreferenced_objects() {
        let root = tempdir().unwrap();
        let cache = EmbeddingObjectCache::open(root.path()).unwrap();
        cache
            .get_or_compute_batch(&identity(1), &["live", "dead"], |missing| {
                Ok(missing.iter().map(|text| vector_for(text)).collect())
            })
            .unwrap();
        let live_key = EmbeddingObjectCache::object_key(&identity(1), "live").unwrap();
        cache
            .replace_checkout_membership(
                "checkout-a",
                &BTreeMap::from([("symbol".into(), live_key.clone())]),
            )
            .unwrap();
        let report = cache.collect_garbage(0, 10).unwrap();
        assert_eq!(report.removed, 1);
        let reused = cache
            .get_or_compute_batch(&identity(1), &["live"], |_| {
                panic!("live membership must retain the object")
            })
            .unwrap();
        assert_eq!(reused.1.object_hits, 1);
    }

    #[test]
    fn publication_lease_excludes_gc_until_membership_commits() {
        let root = tempdir().unwrap();
        let cache = EmbeddingObjectCache::open(root.path()).unwrap();
        let lease = cache.publication_lease().unwrap();
        cache
            .get_or_compute_batch(&identity(1), &["live"], |inputs| {
                Ok(inputs.iter().map(|s| vector_for(s)).collect())
            })
            .unwrap();
        let other = EmbeddingObjectCache::open(root.path()).unwrap();
        let report = other.collect_garbage(0, 1).unwrap();
        assert!(report.deferred_active_publication);
        assert_eq!(report.removed, 0);
        cache
            .replace_checkout_membership(
                "view",
                &BTreeMap::from([(
                    "symbol".into(),
                    EmbeddingObjectCache::object_key(&identity(1), "live").unwrap(),
                )]),
            )
            .unwrap();
        drop(lease);
        assert_eq!(other.collect_garbage(0, 1).unwrap().removed, 0);
    }

    #[test]
    fn concurrent_changed_membership_preserves_both_updates_and_gc_is_bounded() {
        let root = tempdir().unwrap();
        let cache = EmbeddingObjectCache::open(root.path()).unwrap();
        cache
            .get_or_compute_batch(&identity(1), &["a", "b", "dead-a", "dead-b"], |inputs| {
                Ok(inputs.iter().map(|s| vector_for(s)).collect())
            })
            .unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let handles: Vec<_> = ["a", "b"]
            .into_iter()
            .map(|name| {
                let other = EmbeddingObjectCache::open(root.path()).unwrap();
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    other
                        .update_checkout_membership(
                            "view",
                            &[format!("{name}:")],
                            &BTreeMap::from([(
                                format!("{name}:symbol"),
                                EmbeddingObjectCache::object_key(&identity(1), name).unwrap(),
                            )]),
                        )
                        .unwrap();
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        let first = cache.collect_garbage(0, 1).unwrap();
        assert_eq!(first.examined, 1);
        assert_eq!(first.removed, 1);
        assert_eq!(cache.collect_garbage(0, 10).unwrap().removed, 1);
        let (_, stats) = cache
            .get_or_compute_batch(&identity(1), &["a", "b"], |_| {
                panic!("live updates were lost")
            })
            .unwrap();
        assert_eq!(stats.object_hits, 2);
    }

    #[test]
    fn abandoned_publication_and_missing_object_are_reclaimed_in_bounded_passes() {
        let root = tempdir().unwrap();
        let cache = EmbeddingObjectCache::open(root.path()).unwrap();
        let key = EmbeddingObjectCache::object_key(&identity(1), "crashed").unwrap();
        let shard = root.path().join("objects").join(&key[..2]);
        std::fs::create_dir_all(&shard).unwrap();
        let temp = format!(".{key}.crashed.tmp");
        std::fs::write(shard.join(&temp), b"partial").unwrap();
        let db = rusqlite::Connection::open(root.path().join("index.db")).unwrap();
        db.execute("INSERT INTO objects(key,bytes) VALUES(?1,500)", [&key])
            .unwrap();
        db.execute(
            "INSERT INTO pending_publications VALUES(?1,?2)",
            rusqlite::params![temp, key],
        )
        .unwrap();
        let first = cache.collect_garbage(0, 1).unwrap();
        assert_eq!(first.examined, 1);
        assert!(!shard.join(&temp).exists());
        std::fs::remove_dir(&shard).unwrap();
        let second = cache.collect_garbage(0, 1).unwrap();
        assert_eq!(second.removed, 1);
        assert_eq!(second.remaining_bytes, 0);
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM pending_publications", [], |r| r
                .get::<_, u64>(0))
                .unwrap(),
            0
        );
    }

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
