#[cfg(test)]
mod tests {
    use crate::embeddings::EmbeddingEngine;

    #[test]
    #[ignore] // Requires ONNX model file
    fn test_embed_single_text() {
        let engine = EmbeddingEngine::new("models/all-MiniLM-L6-v2.onnx").unwrap();
        let embedding = engine.embed("function loginUser authenticates a user").unwrap();
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
