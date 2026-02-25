use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A JSON-RPC 2.0 request object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    #[serde(default)]
    pub id: Value,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

/// A JSON-RPC 2.0 response object.
#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Value::is_null")]
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

/// A JSON-RPC 2.0 error object.
#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl JsonRpcResponse {
    /// Create a success response with the given id and result.
    pub fn success(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    /// Create an error response with the given id, code, and message.
    pub fn error(id: Value, code: i32, message: String) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message,
                data: None,
            }),
        }
    }
}

/// Parse a JSON string into a JsonRpcRequest.
pub fn parse_request(json: &str) -> Result<JsonRpcRequest, String> {
    serde_json::from_str(json).map_err(|e| format!("Failed to parse JSON-RPC request: {}", e))
}

/// Serialize a JsonRpcResponse to a JSON string.
pub fn format_response(response: &JsonRpcResponse) -> String {
    serde_json::to_string(response).unwrap_or_else(|e| {
        format!(
            r#"{{"jsonrpc":"2.0","error":{{"code":-32603,"message":"Internal error: {}"}}}}"#,
            e
        )
    })
}
