pub mod engine;

#[cfg(test)]
mod tests;

pub use engine::EmbeddingEngine;

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
