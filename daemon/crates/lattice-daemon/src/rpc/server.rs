use std::sync::Arc;
use std::io::{BufRead, BufReader, Write};

use super::protocol::{parse_request, format_response, JsonRpcResponse};

#[async_trait::async_trait]
pub trait RequestHandler: Send + Sync {
    async fn handle(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, (i32, String)>;
}

pub struct StdioServer {
    handler: Arc<dyn RequestHandler>,
}

impl StdioServer {
    pub fn new(handler: Arc<dyn RequestHandler>) -> Self {
        Self { handler }
    }

    fn disk_log(msg: &str) {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true).append(true)
            .open("D:\\lattice\\debug.log")
        {
            let _ = writeln!(f, "[server] {}", msg);
            let _ = f.flush();
        }
    }

    pub async fn run(&self) -> anyhow::Result<()> {
        // CRITICAL: Set stdout and stdin to binary mode on Windows.
        // Without this, \n gets translated to \r\n, doubling the \r\n in
        // Content-Length headers (producing \r\r\n which breaks MCP parsing).
        #[cfg(windows)]
        unsafe {
            extern "C" { fn _setmode(fd: i32, mode: i32) -> i32; }
            _setmode(0, 0x8000); // stdin  -> _O_BINARY
            _setmode(1, 0x8000); // stdout -> _O_BINARY
        }
        Self::disk_log("StdioServer::run() entered (binary mode)");

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();

        // Stdin reader in a blocking thread
        std::thread::spawn(move || {
            let stdin = std::io::stdin();
            let mut reader = BufReader::new(stdin.lock());
            loop {
                match read_message_sync(&mut reader) {
                    Ok(Some(msg)) => {
                        Self::disk_log(&format!("sync read: {} bytes", msg.len()));
                        if tx.send(msg).is_err() { break; }
                    }
                    Ok(None) => {
                        Self::disk_log("sync stdin EOF");
                        break;
                    }
                    Err(e) => {
                        Self::disk_log(&format!("sync read error: {}", e));
                        continue;
                    }
                }
            }
        });

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

            Self::disk_log(&format!("handling method={} id={}", request.method, request.id));

            let response = match self.handler.handle(&request.method, request.params).await {
                Ok(result) => JsonRpcResponse::success(request.id, result),
                Err((code, message)) => JsonRpcResponse::error(request.id, code, message),
            };

            if !is_notification {
                Self::disk_log("sending response");
                write_response_sync(&response);
            }
        }

        Self::disk_log("server loop exited");
        Ok(())
    }
}

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
            loop {
                let mut hdr = String::new();
                reader.read_line(&mut hdr)?;
                if hdr.trim().is_empty() { break; }
            }
            let mut body = vec![0u8; length];
            std::io::Read::read_exact(reader, &mut body)?;
            return Ok(Some(String::from_utf8(body)?));
        } else if trimmed.starts_with('{') {
            return Ok(Some(trimmed.to_string()));
        }
    }
}

/// Write response as JSON line to stdout.
/// MCP over stdio can use either Content-Length framing or newline-delimited JSON.
/// We send both: Content-Length header + body + trailing newline.
/// Clients that understand Content-Length will use that, others will read the JSON line.
fn write_response_sync(response: &JsonRpcResponse) {
    let body = format_response(response);
    // Just write the JSON followed by a newline — simplest possible format
    let mut frame: Vec<u8> = Vec::new();
    frame.extend_from_slice(body.as_bytes());
    frame.push(b'\n');

    StdioServer::disk_log(&format!("writing {} bytes as JSON line", frame.len()));

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    match out.write_all(&frame) {
        Ok(()) => {
            match out.flush() {
                Ok(()) => StdioServer::disk_log("write+flush OK"),
                Err(e) => StdioServer::disk_log(&format!("flush error: {}", e)),
            }
        }
        Err(e) => StdioServer::disk_log(&format!("write error: {}", e)),
    }
}
