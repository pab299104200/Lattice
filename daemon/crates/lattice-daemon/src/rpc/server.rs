use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

use super::protocol::{parse_request, format_response, JsonRpcResponse};

/// Trait for handling JSON-RPC requests.
///
/// Implementors receive the method name and params, and return either
/// a success value or an (error_code, error_message) tuple.
#[async_trait::async_trait]
pub trait RequestHandler: Send + Sync {
    async fn handle(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, (i32, String)>;
}

/// A JSON-RPC server that communicates over stdio using Content-Length framing
/// (compatible with the MCP specification).
pub struct StdioServer {
    handler: Arc<dyn RequestHandler>,
}

impl StdioServer {
    /// Create a new StdioServer with the given request handler.
    pub fn new(handler: Arc<dyn RequestHandler>) -> Self {
        Self { handler }
    }

    /// Run the server loop, reading requests from stdin and writing responses to stdout.
    ///
    /// Reads Content-Length framed messages (per MCP/LSP spec). Falls back to
    /// treating each line as a raw JSON message if no Content-Length header is found.
    /// Notifications (requests with no id / null id) do not receive responses.
    pub async fn run(&self) -> anyhow::Result<()> {
        let stdin = tokio::io::stdin();
        let stdout = Arc::new(Mutex::new(tokio::io::stdout()));
        let mut reader = BufReader::new(stdin);

        loop {
            // Try to read a Content-Length header or a raw JSON line.
            let message = match self.read_message(&mut reader).await {
                Ok(Some(msg)) => msg,
                Ok(None) => {
                    // EOF — clean shutdown
                    tracing::info!("stdin closed, shutting down");
                    break;
                }
                Err(e) => {
                    tracing::warn!("failed to read message: {}", e);
                    continue;
                }
            };

            // Parse the JSON-RPC request
            let request = match parse_request(&message) {
                Ok(req) => req,
                Err(e) => {
                    tracing::warn!("invalid JSON-RPC request: {}", e);
                    let err_response = JsonRpcResponse::error(
                        serde_json::Value::Null,
                        -32700,
                        format!("Parse error: {}", e),
                    );
                    self.write_response(&stdout, &err_response).await?;
                    continue;
                }
            };

            // Check if this is a notification (no id)
            let is_notification = request.id.is_null();

            tracing::debug!(
                method = %request.method,
                id = %request.id,
                "received request"
            );

            // Dispatch to handler
            let response = match self.handler.handle(&request.method, request.params).await {
                Ok(result) => JsonRpcResponse::success(request.id, result),
                Err((code, message)) => JsonRpcResponse::error(request.id, code, message),
            };

            // Notifications don't get responses
            if !is_notification {
                self.write_response(&stdout, &response).await?;
            }
        }

        Ok(())
    }

    /// Read a single message from the reader.
    ///
    /// Supports two modes:
    /// 1. Content-Length framed (MCP/LSP spec): reads headers until blank line,
    ///    then reads exactly Content-Length bytes of body.
    /// 2. Raw JSON line: if the first line looks like JSON (starts with '{'),
    ///    treat it as a complete message.
    async fn read_message<R: tokio::io::AsyncRead + Unpin>(
        &self,
        reader: &mut BufReader<R>,
    ) -> anyhow::Result<Option<String>> {
        loop {
            let mut first_line = String::new();
            let bytes_read = reader.read_line(&mut first_line).await?;
            if bytes_read == 0 {
                return Ok(None); // EOF
            }

            let trimmed = first_line.trim().to_string();
            if trimmed.is_empty() {
                // Skip blank lines
                continue;
            }

            // Check if this looks like a Content-Length header
            if trimmed.to_lowercase().starts_with("content-length:") {
                let length: usize = trimmed
                    .split(':')
                    .nth(1)
                    .ok_or_else(|| anyhow::anyhow!("malformed Content-Length header"))?
                    .trim()
                    .parse()
                    .map_err(|e| anyhow::anyhow!("invalid Content-Length value: {}", e))?;

                // Read remaining headers until blank line
                loop {
                    let mut header_line = String::new();
                    reader.read_line(&mut header_line).await?;
                    if header_line.trim().is_empty() {
                        break;
                    }
                }

                // Read exactly `length` bytes of body
                let mut body = vec![0u8; length];
                tokio::io::AsyncReadExt::read_exact(reader, &mut body).await?;
                let message = String::from_utf8(body)
                    .map_err(|e| anyhow::anyhow!("invalid UTF-8 in message body: {}", e))?;
                return Ok(Some(message));
            } else if trimmed.starts_with('{') {
                // Raw JSON line (fallback mode)
                return Ok(Some(trimmed));
            } else {
                // Unknown line — skip it
                tracing::debug!("skipping unrecognized line: {}", trimmed);
                continue;
            }
        }
    }

    /// Write a Content-Length framed response to stdout.
    async fn write_response(
        &self,
        stdout: &Arc<Mutex<tokio::io::Stdout>>,
        response: &JsonRpcResponse,
    ) -> anyhow::Result<()> {
        let body = format_response(response);
        let frame = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);

        let mut out = stdout.lock().await;
        out.write_all(frame.as_bytes()).await?;
        out.flush().await?;
        Ok(())
    }
}
