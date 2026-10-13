//! R2: line-delimited JSON protocol between the daemon and a recyclable
//! embedding worker child process.
//!
//! The ONNX/ORT CPU arena never returns memory to the OS and is process-global
//! (`embeddings/engine.rs`), so hosting it in a child the daemon can kill and
//! respawn keeps the daemon's own footprint flat. This module is just the wire
//! codec — pure functions, no I/O — so it can be unit-tested on both sides of
//! the pipe. The transport (spawn, stdin/stdout, lifecycle) lives in the daemon.

use serde::{Deserialize, Serialize};

/// One embed request: a batch of texts to embed. Kept minimal; the worker uses
/// the same `EmbeddingEngine` the in-process path would, so batching policy and
/// normalization are identical.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbedRequest {
    pub texts: Vec<String>,
}

/// One embed response: either the vectors (aligned 1:1 with the request texts)
/// or an error string. A worker never partially succeeds — a run that cannot
/// embed every text fails the whole batch, matching `in_bounded_runs`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum EmbedResponse {
    Ok { vectors: Vec<Vec<f32>> },
    Error { message: String },
}

/// Encode a request as one newline-terminated JSON line.
pub fn encode_request(request: &EmbedRequest) -> String {
    let mut line = serde_json::to_string(request).unwrap_or_else(|_| "{\"texts\":[]}".to_string());
    line.push('\n');
    line
}

/// Encode a response as one newline-terminated JSON line.
pub fn encode_response(response: &EmbedResponse) -> String {
    let mut line = serde_json::to_string(response).unwrap_or_else(|_| {
        "{\"status\":\"error\",\"message\":\"response serialization failed\"}".to_string()
    });
    line.push('\n');
    line
}

/// Decode one request line (worker side).
pub fn decode_request(line: &str) -> Result<EmbedRequest, String> {
    serde_json::from_str(line.trim_end())
        .map_err(|error| format!("malformed embed request: {error}"))
}

/// Decode one response line (daemon side).
pub fn decode_response(line: &str) -> Result<EmbedResponse, String> {
    serde_json::from_str(line.trim_end())
        .map_err(|error| format!("malformed embed response: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trips_through_the_wire() {
        let request = EmbedRequest {
            texts: vec!["login".to_string(), "verify credentials".to_string()],
        };
        let line = encode_request(&request);
        assert!(line.ends_with('\n'));
        assert_eq!(decode_request(&line).unwrap(), request);
    }

    #[test]
    fn ok_response_round_trips_with_vectors_aligned() {
        let response = EmbedResponse::Ok {
            vectors: vec![vec![0.1, 0.2, 0.3], vec![0.4, 0.5, 0.6]],
        };
        let line = encode_response(&response);
        assert!(line.ends_with('\n'));
        assert_eq!(decode_response(&line).unwrap(), response);
    }

    #[test]
    fn error_response_round_trips() {
        let response = EmbedResponse::Error {
            message: "onnx runtime unavailable".to_string(),
        };
        assert_eq!(
            decode_response(&encode_response(&response)).unwrap(),
            response
        );
    }

    #[test]
    fn malformed_lines_are_rejected_not_panicked() {
        assert!(decode_request("not json").is_err());
        assert!(decode_response("{").is_err());
    }
}
