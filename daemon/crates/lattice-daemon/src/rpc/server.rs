use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::sync::Arc;

use super::protocol::{format_response, parse_request, JsonRpcResponse};
use serde_json::Value;
use tokio::task::JoinHandle;

struct ActiveRequest {
    generation: u64,
    handle: JoinHandle<()>,
}

struct PendingResponse {
    request_key: String,
    generation: u64,
    response: JsonRpcResponse,
}

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
            extern "C" {
                fn _setmode(fd: i32, mode: i32) -> i32;
            }
            _setmode(0, 0x8000); // stdin  -> _O_BINARY
            _setmode(1, 0x8000); // stdout -> _O_BINARY
        }

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let (response_tx, mut response_rx) =
            tokio::sync::mpsc::unbounded_channel::<PendingResponse>();

        // Stdin reader in a dedicated blocking thread.
        std::thread::spawn(move || {
            let stdin = std::io::stdin();
            let mut reader = BufReader::new(stdin.lock());
            loop {
                match read_message_sync(&mut reader) {
                    Ok(Some(msg)) => {
                        if tx.send(msg).is_err() {
                            break;
                        }
                    }
                    Ok(None) => break, // EOF
                    Err(_) => continue,
                }
            }
        });

        // Process messages from the channel.
        let mut active_requests: HashMap<String, ActiveRequest> = HashMap::new();
        let mut cancelled_requests = HashSet::new();
        let mut shutting_down = false;
        let mut next_generation = 0u64;

        loop {
            tokio::select! {
                msg = rx.recv() => {
                    let Some(message) = msg else {
                        if !active_requests.is_empty() {
                            tracing::info!(
                                "Client disconnected; aborting {} in-flight request(s)",
                                active_requests.len()
                            );
                        }
                        abort_all_requests(&mut active_requests);
                        break;
                    };
                    let request = match parse_request(&message) {
                        Ok(req) => req,
                        Err(e) => {
                            let err_resp = JsonRpcResponse::error(
                                serde_json::Value::Null,
                                -32700,
                                format!("Parse error: {}", e),
                            );
                            write_response_sync(&err_resp);
                            continue;
                        }
                    };

                    let is_notification = request.id.is_null();
                    let request_id = request_id_key(&request.id);

                    match request.method.as_str() {
                        "notifications/cancelled" => {
                            if cancel_active_request(
                                &mut active_requests,
                                &mut cancelled_requests,
                                &request.params,
                            ) {
                                tracing::info!("Aborted cancelled in-flight request");
                            }
                            continue;
                        }
                        "shutdown" => {
                            shutting_down = true;
                            if !is_notification {
                                write_response_sync(&JsonRpcResponse::success(
                                    request.id,
                                    serde_json::json!({}),
                                ));
                            }
                            continue;
                        }
                        "exit" => {
                            tracing::info!(
                                "Received exit; aborting {} in-flight request(s)",
                                active_requests.len()
                            );
                            abort_all_requests(&mut active_requests);
                            break;
                        }
                        _ => {}
                    }

                    if shutting_down {
                        if !is_notification {
                            write_response_sync(&JsonRpcResponse::error(
                                request.id,
                                -32000,
                                "Server is shutting down".to_string(),
                            ));
                        }
                        continue;
                    }

                    let handler = Arc::clone(&self.handler);
                    if let Some(key) = request_id {
                        cancelled_requests.remove(&key);

                        let response_tx = response_tx.clone();
                        let response_key = key.clone();
                        let generation = next_generation;
                        next_generation = next_generation.wrapping_add(1);
                        let method = request.method;
                        let params = request.params;
                        let id = request.id;
                        let task = tokio::spawn(async move {
                            let response = match handler.handle(&method, params).await {
                                Ok(result) => JsonRpcResponse::success(id, result),
                                Err((code, message)) => JsonRpcResponse::error(id, code, message),
                            };
                            let _ = response_tx.send(PendingResponse {
                                request_key: response_key,
                                generation,
                                response,
                            });
                        });

                        if let Some(previous) = active_requests.insert(
                            key.clone(),
                            ActiveRequest {
                                generation,
                                handle: task,
                            },
                        ) {
                            tracing::warn!("Replacing duplicate in-flight request id {}", key);
                            previous.handle.abort();
                        }
                    } else {
                        tokio::spawn(async move {
                            let _ = handler.handle(&request.method, request.params).await;
                        });
                    }
                }
                response = response_rx.recv() => {
                    let Some(response) = response else { continue };

                    if should_write_tracked_response(
                        &mut active_requests,
                        &mut cancelled_requests,
                        &response,
                    ) {
                        write_response_sync(&response.response);
                    }
                }
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!(
                        "Received Ctrl+C, aborting {} in-flight request(s)",
                        active_requests.len()
                    );
                    abort_all_requests(&mut active_requests);
                    break;
                }
            }
        }

        Ok(())
    }
}

/// Maximum payload size (10 MB). Reject anything larger to prevent OOM.
const MAX_PAYLOAD_SIZE: usize = 10 * 1024 * 1024;

/// Read a single message from stdin.
/// Supports both Content-Length framing (for clients that send it) and raw JSON lines.
pub(crate) fn read_message_sync<R: BufRead>(reader: &mut R) -> anyhow::Result<Option<String>> {
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            return Ok(None);
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if trimmed.to_lowercase().starts_with("content-length:") {
            let length: usize = trimmed
                .split(':')
                .nth(1)
                .ok_or_else(|| anyhow::anyhow!("bad header"))?
                .trim()
                .parse()?;
            if length > MAX_PAYLOAD_SIZE {
                return Err(anyhow::anyhow!(
                    "Content-Length {} exceeds maximum allowed size of {} bytes",
                    length,
                    MAX_PAYLOAD_SIZE
                ));
            }
            // Skip remaining headers until blank line
            loop {
                let mut hdr = String::new();
                reader.read_line(&mut hdr)?;
                if hdr.trim().is_empty() {
                    break;
                }
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

fn request_id_key(id: &Value) -> Option<String> {
    match id {
        Value::Null => None,
        Value::String(value) => Some(format!("string:{value}")),
        Value::Number(value) => Some(format!("number:{value}")),
        Value::Bool(value) => Some(format!("bool:{value}")),
        other => serde_json::to_string(other)
            .ok()
            .map(|encoded| format!("json:{encoded}")),
    }
}

fn cancelled_request_key(params: &Value) -> Option<String> {
    params
        .get("requestId")
        .or_else(|| params.get("request_id"))
        .or_else(|| params.get("id"))
        .and_then(request_id_key)
}

fn cancel_active_request(
    active_requests: &mut HashMap<String, ActiveRequest>,
    cancelled_requests: &mut HashSet<String>,
    params: &Value,
) -> bool {
    let Some(key) = cancelled_request_key(params) else {
        return false;
    };

    cancelled_requests.insert(key.clone());
    if let Some(handle) = active_requests.remove(&key) {
        handle.handle.abort();
        return true;
    }

    false
}

fn should_write_tracked_response(
    active_requests: &mut HashMap<String, ActiveRequest>,
    cancelled_requests: &mut HashSet<String>,
    pending: &PendingResponse,
) -> bool {
    match active_requests.get(&pending.request_key) {
        Some(active) if active.generation == pending.generation => {
            active_requests.remove(&pending.request_key);
            if cancelled_requests.remove(&pending.request_key) {
                tracing::debug!(
                    "Dropping response for cancelled request {}",
                    pending.request_key
                );
                false
            } else {
                true
            }
        }
        Some(active) => {
            tracing::debug!(
                "Dropping stale response for replaced request {} (got generation {}, active generation {})",
                pending.request_key,
                pending.generation,
                active.generation
            );
            false
        }
        None => {
            if cancelled_requests.remove(&pending.request_key) {
                tracing::debug!(
                    "Dropping response for cancelled request {}",
                    pending.request_key
                );
            } else {
                tracing::debug!(
                    "Dropping orphaned response for inactive request {}",
                    pending.request_key
                );
            }
            false
        }
    }
}

fn abort_all_requests(active_requests: &mut HashMap<String, ActiveRequest>) {
    for (_, handle) in active_requests.drain() {
        handle.handle.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        abort_all_requests, cancel_active_request, cancelled_request_key, request_id_key,
        should_write_tracked_response, ActiveRequest, PendingResponse,
    };
    use crate::rpc::protocol::JsonRpcResponse;
    use serde_json::json;
    use std::collections::{HashMap, HashSet};
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use std::time::Duration;
    use tokio::sync::oneshot;
    use tokio::task::JoinHandle;

    struct PendingUntilAborted {
        signal: Option<oneshot::Sender<()>>,
    }

    impl Future for PendingUntilAborted {
        type Output = ();

        fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
            Poll::Pending
        }
    }

    impl Drop for PendingUntilAborted {
        fn drop(&mut self) {
            if let Some(tx) = self.signal.take() {
                let _ = tx.send(());
            }
        }
    }

    fn pending_task() -> (JoinHandle<()>, oneshot::Receiver<()>) {
        let (tx, rx) = oneshot::channel();
        let handle = tokio::spawn(PendingUntilAborted { signal: Some(tx) });
        (handle, rx)
    }

    #[test]
    fn test_cancelled_request_key_reads_mcp_request_id() {
        assert_eq!(
            cancelled_request_key(&json!({"requestId": 7})),
            Some("number:7".to_string())
        );
        assert_eq!(
            cancelled_request_key(&json!({"request_id": "child-req"})),
            Some("string:child-req".to_string())
        );
        assert_eq!(
            cancelled_request_key(&json!({"id": true})),
            Some("bool:true".to_string())
        );
        assert!(cancelled_request_key(&json!({})).is_none());
    }

    #[tokio::test]
    async fn test_cancel_active_request_aborts_matching_task() {
        let (handle, dropped) = pending_task();
        let key = request_id_key(&json!(42)).expect("request key");
        let mut active_requests = HashMap::from([(
            key.clone(),
            ActiveRequest {
                generation: 0,
                handle,
            },
        )]);
        let mut cancelled_requests = HashSet::new();

        assert!(cancel_active_request(
            &mut active_requests,
            &mut cancelled_requests,
            &json!({"requestId": 42}),
        ));
        assert!(active_requests.is_empty());
        assert!(cancelled_requests.contains(&key));

        tokio::time::timeout(Duration::from_secs(1), dropped)
            .await
            .expect("task should abort promptly")
            .expect("drop signal should arrive");
    }

    #[tokio::test]
    async fn test_abort_all_requests_aborts_every_in_flight_task() {
        let (first_handle, first_dropped) = pending_task();
        let (second_handle, second_dropped) = pending_task();
        let mut active_requests = HashMap::from([
            (
                request_id_key(&json!(1)).expect("first request key"),
                ActiveRequest {
                    generation: 0,
                    handle: first_handle,
                },
            ),
            (
                request_id_key(&json!("second")).expect("second request key"),
                ActiveRequest {
                    generation: 1,
                    handle: second_handle,
                },
            ),
        ]);

        abort_all_requests(&mut active_requests);
        assert!(active_requests.is_empty());

        tokio::time::timeout(Duration::from_secs(1), first_dropped)
            .await
            .expect("first task should abort promptly")
            .expect("first drop signal should arrive");
        tokio::time::timeout(Duration::from_secs(1), second_dropped)
            .await
            .expect("second task should abort promptly")
            .expect("second drop signal should arrive");
    }

    #[tokio::test]
    async fn test_should_write_tracked_response_drops_stale_duplicate_generation() {
        let (handle, dropped) = pending_task();
        let key = request_id_key(&json!(7)).expect("request key");
        let mut active_requests = HashMap::from([(
            key.clone(),
            ActiveRequest {
                generation: 2,
                handle,
            },
        )]);
        let mut cancelled_requests = HashSet::new();
        let stale = PendingResponse {
            request_key: key.clone(),
            generation: 1,
            response: JsonRpcResponse::success(json!(7), json!({"status": "stale"})),
        };

        assert!(!should_write_tracked_response(
            &mut active_requests,
            &mut cancelled_requests,
            &stale,
        ));
        assert_eq!(
            active_requests.get(&key).map(|request| request.generation),
            Some(2)
        );

        abort_all_requests(&mut active_requests);
        tokio::time::timeout(Duration::from_secs(1), dropped)
            .await
            .expect("replacement task should abort promptly")
            .expect("replacement drop signal should arrive");
    }

    #[tokio::test]
    async fn test_should_write_tracked_response_accepts_current_generation() {
        let key = request_id_key(&json!(9)).expect("request key");
        let mut active_requests = HashMap::from([(
            key.clone(),
            ActiveRequest {
                generation: 3,
                handle: tokio::spawn(async {}),
            },
        )]);
        let mut cancelled_requests = HashSet::new();
        let current = PendingResponse {
            request_key: key.clone(),
            generation: 3,
            response: JsonRpcResponse::success(json!(9), json!({"status": "ok"})),
        };

        assert!(should_write_tracked_response(
            &mut active_requests,
            &mut cancelled_requests,
            &current,
        ));
        assert!(!active_requests.contains_key(&key));
    }
}
