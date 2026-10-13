//! R2: run the ONNX embedding engine in a recyclable child process.
//!
//! `ort`'s CPU arena never returns memory to the OS and is process-global, so
//! in-process embedding permanently inflates the daemon's footprint. Hosting
//! the engine in a child the daemon can kill and respawn keeps the daemon's own
//! RSS flat. Flag-gated (`LATTICE_EMBEDDING_SUBPROCESS=1`), default OFF, with
//! the in-process engine as the fallback — a child that cannot start or crashes
//! surfaces as an error, which the existing semantic circuit breaker turns into
//! lexical-only retrieval without ever crashing the daemon.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use lattice_core::embeddings::{
    decode_response, encode_request, EmbedRequest, EmbedResponse, EmbeddingEngine,
    EmbeddingProvider,
};

/// Is the subprocess embedding provider opted in? Default OFF.
pub(crate) fn subprocess_embeddings_enabled() -> bool {
    matches!(
        std::env::var("LATTICE_EMBEDDING_SUBPROCESS")
            .ok()
            .as_deref(),
        Some("1") | Some("true")
    )
}

/// How large the child's footprint may grow before it is recycled. The arena
/// grows to the largest batch seen; recycling returns that memory to the OS.
const DEFAULT_CHILD_RSS_LIMIT_BYTES: u64 = 1536 * 1024 * 1024;

/// How long to wait for a child's response to one batch before giving up on it.
/// A wedged worker (deadlocked in ONNX, stuck, never emitting a line) must not
/// block the embed call — and the mutex it holds — forever; the circuit breaker
/// turns the resulting error into lexical-only retrieval. Generous enough for a
/// large batch on a loaded machine, far below "hung forever".
const DEFAULT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);

struct ChildHandle {
    process: Child,
    stdin: ChildStdin,
    /// Lines read off the child's stdout by a dedicated reader thread. Reading
    /// on a thread (rather than inline) is what lets the caller apply a
    /// `recv_timeout` deadline to an otherwise unbounded blocking pipe read.
    lines: Receiver<std::io::Result<String>>,
    /// Embed batches served since this child was spawned, for diagnostics.
    served: u64,
}

/// An `EmbeddingProvider` that forwards batches to a recyclable child process.
/// All access is serialized: embedding is already batched and off the hot path,
/// so a single worker with a mutex is the simplest correct design.
pub(crate) struct SubprocessEmbeddingProvider {
    rss_limit_bytes: u64,
    response_timeout: Duration,
    child: Mutex<Option<ChildHandle>>,
    identity: Option<String>,
    /// Produces the (unspawned) command for a fresh worker. Boxed so tests can
    /// inject a fake worker that speaks the same line protocol over real pipes
    /// without an ONNX model; production always builds `self --embed-worker`.
    spawn_command: Box<dyn Fn() -> Result<Command> + Send + Sync>,
}

impl SubprocessEmbeddingProvider {
    /// Build a provider for the given model, deriving the storage identity from
    /// an in-process engine load so vectors stay compatible with the in-process
    /// path. Spawning the worker is lazy (first embed call).
    pub(crate) fn new(model_path: &str) -> Result<Self> {
        // Load once to capture the identity; dropped immediately. This also
        // fails fast if the model/runtime is unusable, so the caller can fall
        // back to lexical before any worker is spawned.
        let probe = EmbeddingEngine::new(model_path)?;
        let identity = probe.storage_identity()?;
        let model_path = model_path.to_string();
        Ok(Self {
            rss_limit_bytes: child_rss_limit(),
            response_timeout: child_response_timeout(),
            child: Mutex::new(None),
            identity,
            spawn_command: Box::new(move || {
                let exe = std::env::current_exe()
                    .context("locating lattice executable for embed worker")?;
                let mut command = Command::new(exe);
                command
                    .arg("--embed-worker")
                    .arg("--model")
                    .arg(&model_path);
                Ok(command)
            }),
        })
    }

    fn spawn_child(&self) -> Result<ChildHandle> {
        let mut command = (self.spawn_command)()?;
        let mut process = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("spawning embed worker child")?;
        let stdin = process
            .stdin
            .take()
            .ok_or_else(|| anyhow!("embed worker child has no stdin"))?;
        let stdout = process
            .stdout
            .take()
            .ok_or_else(|| anyhow!("embed worker child has no stdout"))?;

        // Read the child's stdout on a dedicated thread so the caller can bound
        // the wait with `recv_timeout`. A blocking `read_line` on the pipe has
        // no deadline, so without this a wedged child would hang the embed call
        // (and the mutex it holds) indefinitely. The thread exits when the pipe
        // closes (child death or recycle), dropping the sender.
        let (tx, rx) = std::sync::mpsc::channel::<std::io::Result<String>>();
        std::thread::Builder::new()
            .name("embed-worker-reader".to_string())
            .spawn(move || {
                let mut reader = BufReader::new(stdout);
                loop {
                    let mut line = String::new();
                    match reader.read_line(&mut line) {
                        Ok(0) => break, // EOF: child closed stdout.
                        Ok(_) => {
                            if tx.send(Ok(line)).is_err() {
                                break; // Receiver gone (handle dropped).
                            }
                        }
                        Err(error) => {
                            let _ = tx.send(Err(error));
                            break;
                        }
                    }
                }
            })
            .context("spawning embed worker reader thread")?;

        Ok(ChildHandle {
            process,
            stdin,
            lines: rx,
            served: 0,
        })
    }

    /// Send one batch to the child and read its response. On any transport
    /// failure the child is dropped (killed on drop) so the next call respawns.
    fn embed_via_child(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let mut guard = self
            .child
            .lock()
            .map_err(|_| anyhow!("embed worker lock poisoned"))?;

        // Recycle a child that has grown past its footprint limit before use.
        if let Some(handle) = guard.as_mut() {
            if child_rss_bytes(handle.process.id()).is_some_and(|rss| rss > self.rss_limit_bytes) {
                tracing::info!(
                    served = handle.served,
                    "recycling embed worker to reclaim its arena"
                );
                let _ = handle.process.kill();
                *guard = None;
            }
        }
        if guard.is_none() {
            *guard = Some(self.spawn_child()?);
        }
        let handle = guard.as_mut().expect("child was just ensured");

        let request = EmbedRequest {
            texts: texts.iter().map(|text| text.to_string()).collect(),
        };
        // One request/response round-trip over the child's pipes. Factored out
        // of a closure so the `&mut handle` borrow ends before we touch
        // `handle.served`/`handle.process` in the match below.
        fn send_and_read(
            handle: &mut ChildHandle,
            request: &EmbedRequest,
            expected: usize,
            timeout: Duration,
        ) -> Result<Vec<Vec<f32>>> {
            handle
                .stdin
                .write_all(encode_request(request).as_bytes())
                .context("writing embed request to worker")?;
            handle.stdin.flush().context("flushing embed request")?;
            // Bounded wait: a hung worker must not block forever. The reader
            // thread forwards each line (or an EOF via channel disconnect).
            let line = match handle.lines.recv_timeout(timeout) {
                Ok(Ok(line)) => line,
                Ok(Err(error)) => {
                    return Err(anyhow!(error)).context("reading embed response from worker")
                }
                Err(RecvTimeoutError::Timeout) => {
                    return Err(anyhow!(
                        "embed worker did not respond within {:?}; treating it as wedged",
                        timeout
                    ))
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(anyhow!("embed worker closed its output"))
                }
            };
            match decode_response(&line).map_err(|error| anyhow!(error))? {
                EmbedResponse::Ok { vectors } => {
                    if vectors.len() != expected {
                        return Err(anyhow!(
                            "embed worker returned {} vectors for {} texts",
                            vectors.len(),
                            expected
                        ));
                    }
                    Ok(vectors)
                }
                EmbedResponse::Error { message } => Err(anyhow!(message)),
            }
        }
        match send_and_read(handle, &request, texts.len(), self.response_timeout) {
            Ok(vectors) => {
                handle.served += 1;
                Ok(vectors)
            }
            Err(error) => {
                // Drop the (possibly broken) child so the next call respawns.
                let _ = handle.process.kill();
                *guard = None;
                Err(error)
            }
        }
    }
}

impl Drop for SubprocessEmbeddingProvider {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.child.lock() {
            if let Some(handle) = guard.as_mut() {
                let _ = handle.process.kill();
            }
        }
    }
}

impl EmbeddingProvider for SubprocessEmbeddingProvider {
    fn storage_identity(&self) -> Result<Option<String>> {
        Ok(self.identity.clone())
    }

    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let mut vectors = self.embed_via_child(&[text])?;
        vectors
            .pop()
            .ok_or_else(|| anyhow!("embed worker returned no vector"))
    }

    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        self.embed_via_child(texts)
    }
}

fn child_rss_limit() -> u64 {
    std::env::var("LATTICE_EMBEDDING_SUBPROCESS_RSS_LIMIT_MB")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .map(|mb| mb * 1024 * 1024)
        .unwrap_or(DEFAULT_CHILD_RSS_LIMIT_BYTES)
}

/// How long to wait for a child's batch response before declaring it wedged.
/// Overridable via `LATTICE_EMBEDDING_SUBPROCESS_TIMEOUT_SECS`.
fn child_response_timeout() -> Duration {
    std::env::var("LATTICE_EMBEDDING_SUBPROCESS_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_RESPONSE_TIMEOUT)
}

/// Best-effort RSS of a child process in bytes. Uses `ps`, so it is effective
/// on macOS/Linux; on platforms without `ps` the command fails and this returns
/// `None`, which simply means RSS-triggered recycling never fires there (the
/// worker still runs; it just is not recycled by footprint). ONNX arena growth
/// is the macOS problem this targets, so that degradation is acceptable.
fn child_rss_bytes(pid: u32) -> Option<u64> {
    let output = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let kib = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u64>()
        .ok()?;
    Some(kib * 1024)
}

/// The `--embed-worker` run mode: read one embed request per line from stdin,
/// embed with an in-process engine, write one response per line to stdout.
/// Runs until stdin closes. Any embed failure is reported in-band (never a
/// crash) so the parent can decide to retry, recycle, or fall back.
pub(crate) fn run_embed_worker() -> Result<()> {
    let model = model_arg().context("--embed-worker requires --model <path>")?;
    let engine = EmbeddingEngine::new(model.to_string_lossy().as_ref())
        .context("loading embedding model in worker")?;
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut line = String::new();
    loop {
        line.clear();
        let read = stdin.lock().read_line(&mut line)?;
        if read == 0 {
            break; // parent closed the pipe
        }
        let response = match lattice_core::embeddings::decode_request(&line) {
            Ok(request) => {
                let refs: Vec<&str> = request.texts.iter().map(String::as_str).collect();
                match engine.embed_batch(&refs) {
                    Ok(vectors) => EmbedResponse::Ok { vectors },
                    Err(error) => EmbedResponse::Error {
                        message: error.to_string(),
                    },
                }
            }
            Err(message) => EmbedResponse::Error { message },
        };
        stdout.write_all(lattice_core::embeddings::encode_response(&response).as_bytes())?;
        stdout.flush()?;
    }
    Ok(())
}

fn model_arg() -> Option<PathBuf> {
    let args: Vec<String> = std::env::args().collect();
    let index = args.iter().position(|arg| arg == "--model")?;
    args.get(index + 1).map(PathBuf::from)
}

#[cfg(test)]
impl SubprocessEmbeddingProvider {
    /// Build a provider whose worker is produced by `spawn_command`, skipping
    /// the ONNX probe so transport, recycling, and failure handling can be
    /// exercised without a model. The injected command must speak the same
    /// line-delimited protocol as `run_embed_worker`.
    fn with_spawn_command(
        rss_limit_bytes: u64,
        response_timeout: Duration,
        spawn_command: Box<dyn Fn() -> Result<Command> + Send + Sync>,
    ) -> Self {
        Self {
            rss_limit_bytes,
            response_timeout,
            child: Mutex::new(None),
            identity: Some("test-identity".to_string()),
            spawn_command,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lattice_core::embeddings::{encode_response, EmbedResponse};

    /// Generous response timeout for the non-hang tests (they reply instantly).
    const TEST_TIMEOUT: Duration = Duration::from_secs(10);

    /// A fake worker: a shell loop that reads one request line and replies with
    /// a canned response line, mirroring the real worker's protocol. Unix-only
    /// because it relies on `sh`; the production worker is the lattice binary
    /// itself, which is cross-platform.
    #[cfg(unix)]
    fn fake_worker(response_line: String) -> Box<dyn Fn() -> Result<Command> + Send + Sync> {
        Box::new(move || {
            let mut command = Command::new("sh");
            command.arg("-c").arg(format!(
                "while IFS= read -r _line; do printf '%s' {}; done",
                shell_quote(&response_line)
            ));
            Ok(command)
        }) as Box<dyn Fn() -> Result<Command> + Send + Sync>
    }

    #[cfg(unix)]
    fn shell_quote(value: &str) -> String {
        let escaped = value.replace('\'', "'\\''");
        format!("'{escaped}'")
    }

    #[cfg(unix)]
    #[test]
    fn child_round_trips_a_batch_over_real_pipes() {
        let response = encode_response(&EmbedResponse::Ok {
            vectors: vec![vec![1.0, 2.0], vec![3.0, 4.0]],
        });
        let provider = SubprocessEmbeddingProvider::with_spawn_command(
            u64::MAX,
            TEST_TIMEOUT,
            fake_worker(response),
        );

        let vectors = provider.embed_batch(&["alpha", "beta"]).unwrap();

        assert_eq!(vectors, vec![vec![1.0, 2.0], vec![3.0, 4.0]]);
    }

    #[cfg(unix)]
    #[test]
    fn worker_error_response_degrades_to_an_error_not_a_panic() {
        let response = encode_response(&EmbedResponse::Error {
            message: "model is unavailable".to_string(),
        });
        let provider = SubprocessEmbeddingProvider::with_spawn_command(
            u64::MAX,
            TEST_TIMEOUT,
            fake_worker(response),
        );

        let result = provider.embed_batch(&["alpha"]);

        let error = result.expect_err("an error response must surface as Err");
        assert!(
            error.to_string().contains("model is unavailable"),
            "unexpected error: {error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn vector_count_mismatch_is_rejected() {
        // Child returns one vector for a two-text batch: a protocol violation
        // that must fail rather than silently misalign embeddings.
        let response = encode_response(&EmbedResponse::Ok {
            vectors: vec![vec![1.0, 2.0]],
        });
        let provider = SubprocessEmbeddingProvider::with_spawn_command(
            u64::MAX,
            TEST_TIMEOUT,
            fake_worker(response),
        );

        let result = provider.embed_batch(&["alpha", "beta"]);

        assert!(result.is_err(), "mismatched vector count must be rejected");
    }

    #[cfg(unix)]
    #[test]
    fn a_closed_child_pipe_surfaces_as_an_error() {
        // `true` exits immediately, closing stdout without a response line.
        let spawn: Box<dyn Fn() -> Result<Command> + Send + Sync> =
            Box::new(|| Ok(Command::new("true")));
        let provider =
            SubprocessEmbeddingProvider::with_spawn_command(u64::MAX, TEST_TIMEOUT, spawn);

        let result = provider.embed_batch(&["alpha"]);

        assert!(
            result.is_err(),
            "a worker that closes its pipe must surface as an error, not hang or panic"
        );
    }

    #[test]
    fn empty_batch_short_circuits_without_spawning() {
        // Spawn command that would fail if ever called proves the empty-batch
        // fast path never touches a child.
        let spawn: Box<dyn Fn() -> Result<Command> + Send + Sync> =
            Box::new(|| Err(anyhow!("spawn must not be called for an empty batch")));
        let provider =
            SubprocessEmbeddingProvider::with_spawn_command(u64::MAX, TEST_TIMEOUT, spawn);

        let vectors = provider.embed_batch(&[]).unwrap();

        assert!(vectors.is_empty());
    }

    #[test]
    fn rss_limit_env_override_is_respected() {
        // Guard against cross-test env bleed by scoping the variable.
        std::env::set_var("LATTICE_EMBEDDING_SUBPROCESS_RSS_LIMIT_MB", "64");
        let limit = child_rss_limit();
        std::env::remove_var("LATTICE_EMBEDDING_SUBPROCESS_RSS_LIMIT_MB");
        assert_eq!(limit, 64 * 1024 * 1024);
    }

    #[test]
    fn response_timeout_env_override_is_respected() {
        std::env::set_var("LATTICE_EMBEDDING_SUBPROCESS_TIMEOUT_SECS", "7");
        let timeout = child_response_timeout();
        std::env::remove_var("LATTICE_EMBEDDING_SUBPROCESS_TIMEOUT_SECS");
        assert_eq!(timeout, Duration::from_secs(7));
    }

    /// Soft #2 (independent review): the response read had no deadline, so a
    /// wedged worker (alive but never replying) blocked the embed call — and
    /// the mutex it holds — forever. The read is now bounded: a worker that
    /// consumes the request but never answers must surface as an error within
    /// the timeout, so the circuit breaker can fall back to lexical.
    ///
    /// The fake worker reads the request line then sleeps indefinitely. The
    /// whole test runs on a watchdog thread so that, if the fix regresses and
    /// the call blocks forever, the test fails by timing out rather than
    /// hanging the suite.
    #[cfg(unix)]
    #[test]
    fn a_wedged_worker_times_out_instead_of_hanging() {
        let (done_tx, done_rx) = std::sync::mpsc::channel::<Result<()>>();
        std::thread::spawn(move || {
            // Reads one line, then blocks forever without replying.
            let spawn: Box<dyn Fn() -> Result<Command> + Send + Sync> = Box::new(|| {
                let mut command = Command::new("sh");
                command
                    .arg("-c")
                    .arg("IFS= read -r _line; while true; do sleep 3600; done");
                Ok(command)
            });
            let provider = SubprocessEmbeddingProvider::with_spawn_command(
                u64::MAX,
                Duration::from_millis(500),
                spawn,
            );
            let result = provider.embed_batch(&["alpha"]);
            let _ = done_tx.send(result.map(|_| ()));
        });

        // The call must return (as Err) well within this bound. Generous
        // relative to the 500ms response timeout to avoid CI flakiness.
        match done_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(result) => assert!(
                result.is_err(),
                "a wedged worker must surface as an error, not a success"
            ),
            Err(_) => panic!("embed_batch hung past the response timeout instead of failing fast"),
        }
    }
}
