use super::protocol::{format_response, parse_request, JsonRpcResponse};
use serde_json::{json, Value};

#[test]
fn test_parse_valid_request() {
    let json = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#;
    let req = parse_request(json).expect("should parse valid request");
    assert_eq!(req.method, "tools/list");
    assert_eq!(req.id, json!(1));
    assert_eq!(req.jsonrpc, "2.0");
}

#[test]
fn test_parse_notification() {
    // Notifications have no id field
    let json = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
    let req = parse_request(json).expect("should parse notification");
    assert_eq!(req.method, "notifications/initialized");
    assert_eq!(req.id, Value::Null);
}

#[test]
fn test_format_success_response() {
    let response = JsonRpcResponse::success(json!(42), json!({"status": "ok"}));
    let output = format_response(&response);
    let parsed: Value = serde_json::from_str(&output).expect("should be valid JSON");
    assert_eq!(parsed["jsonrpc"], "2.0");
    assert_eq!(parsed["id"], 42);
    assert_eq!(parsed["result"]["status"], "ok");
    assert!(parsed.get("error").is_none());
}

#[test]
fn test_format_error_response() {
    let response = JsonRpcResponse::error(json!(7), -32601, "Method not found".to_string());
    let output = format_response(&response);
    let parsed: Value = serde_json::from_str(&output).expect("should be valid JSON");
    assert_eq!(parsed["jsonrpc"], "2.0");
    assert_eq!(parsed["id"], 7);
    assert_eq!(parsed["error"]["code"], -32601);
    assert_eq!(parsed["error"]["message"], "Method not found");
    assert!(parsed.get("result").is_none());
}
