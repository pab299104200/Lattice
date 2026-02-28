use std::sync::Arc;
use std::io::{BufRead, BufReader, Write};

use super::protocol::{parse_request, format_response, JsonRpcResponse};

/// Trait for handling JSON-RPC requests.
#[async_trait::async_trait]
pub trait RequestHandler: Send + Sync {
    async fn handle(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, (i32, String)>;
}

/// A JSON-RPC server that communicates over stdio using newline-delimited JSON.
/// Uses synchronous stdin (blocking thread) and stdout to avoid Windows pipe issues.
pub struct StdioServer {
    handler: Arc<dyn RequestHandler>,
}

impl StdioServer {
    pub fn new(handler: Arc<dyn RequestHandler>) -> Self {
        Self { handler }
    }

    /// Run the server loop.
    pub async fn run(&self) -> anyhow::Result<()> {
        // Set stdout/stdin to binary mode on Windows to prevent \n -> \r\n translation.
        #[cfg(windows)]
        unsafe {
            extern "C" { fn _setmode(fd: i32, mode: i32) -> i32; }
            _setmode(0, 0x8000); // stdin  -> _O_BINARY
            _setmode(1, 0x8000); // stdout -> _O_BINARY
        }

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();

        // Stdin reader in a dedicated blocking thread.
        std::thread::spawn(move || {
            let stdin = std::io::stdin();
            let mut reader = BufReader::new(stdin.lock());
            loop {
                match read_message_sync(&mut reader) {
                    Ok(Some(msg)) => {
                        if tx.send(msg).is_err() { break; }
                    }
                    Ok(None) => break, // EOF
                    Err(_) => continue,
                }
            }
        });

        // Process messages from the channel.
        while let Some(message) = rx.recv().await {
            let request = match parse_request(&message) {
                Ok(req) => req,
                Err(e) => {
                    let err_resp = JsonRpcResponse::error(
                        serde_json::Value::Null, -32700,
                        format!("Parse error: {}", e),
                    );
                    write_response_sync(&err_resp);
                    continue;
                }
            };

            let is_notification = request.id.is_null();

            let response = match self.handler.handle(&request.method, request.params).await {
                Ok(result) => JsonRpcResponse::success(request.id, result),
                Err((code, message)) => JsonRpcResponse::error(request.id, code, message),
            };

            if !is_notification {
                write_response_sync(&response);
            }
        }

        Ok(())
    }
}

/// Maximum payload size (10 MB). Reject anything larger to prevent OOM.
const MAX_PAYLOAD_SIZE: usize = 10 * 1024 * 1024;

/// Read a single message from stdin.
/// Supports both Content-Length framing (for clients that send it) and raw JSON lines.
fn read_message_sync<R: BufRead>(reader: &mut R) -> anyhow::Result<Option<String>> {
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 { return Ok(None); }

        let trimmed = line.trim();
        if trimmed.is_empty() { continue; }

        if trimmed.to_lowercase().starts_with("content-length:") {
            let length: usize = trimmed.split(':').nth(1)
                .ok_or_else(|| anyhow::anyhow!("bad header"))?
                .trim().parse()?;
            if length > MAX_PAYLOAD_SIZE {
                return Err(anyhow::anyhow!(
                    "Content-Length {} exceeds maximum allowed size of {} bytes",
                    length, MAX_PAYLOAD_SIZE
                ));
            }
            // Skip remaining headers until blank line
            loop {
                let mut hdr = String::new();
                reader.read_line(&mut hdr)?;
                if hdr.trim().is_empty() { break; }
            }
            let mut body = vec![0u8; length];
            std::io::Read::read_exact(reader, &mut body)?;
            return Ok(Some(String::from_utf8(body)?));
        } else if trimmed.starts_with('{') {
            // Raw JSON line
            return Ok(Some(trimmed.to_string()));
        }
        // Skip unrecognized lines
    }
}

/// Write a JSON-RPC response as a newline-delimited JSON line to stdout.
fn write_response_sync(response: &JsonRpcResponse) {
    let body = format_response(response);
    let mut frame: Vec<u8> = Vec::new();
    frame.extend_from_slice(body.as_bytes());
    frame.push(b'\n');

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let _ = out.write_all(&frame);
    let _ = out.flush();
}
