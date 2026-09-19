use serde_json::{json, Value};
use std::collections::hash_map::DefaultHasher;
use std::collections::BTreeSet;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[test]
fn cli_query_subcommands_call_public_tools_over_daemon_protocol() {
    let workspace = unique_workspace("cli-happy");
    std::fs::create_dir_all(workspace.join(".git")).expect("create git marker");
    let observed = Arc::new(Mutex::new(Vec::new()));
    let (addr, server) = start_fake_daemon(8, Arc::clone(&observed), FakeMode::Immediate);

    let cases: Vec<(&str, Vec<&str>, Option<&str>)> = vec![
        ("context", vec!["context", "auth flow"], None),
        (
            "prepare_change",
            vec!["prepare_change", "fix auth flow", "--mode", "prepare"],
            None,
        ),
        ("impact", vec!["impact", "src/main.rs", "--no-tests"], None),
        (
            "search",
            vec!["search", "McpHandler", "--kind", "symbol"],
            None,
        ),
        ("diagnose", vec!["diagnose", "-"], Some("compiler error")),
        ("remember", vec!["remember", "phase three works"], None),
        ("recall", vec!["recall", "phase three"], None),
        ("status", vec!["status", "--scope", "index"], None),
    ];

    for (name, args, stdin_text) in cases {
        let output = run_lattice(&addr, &workspace, &args, stdin_text);
        assert!(
            output.status.success(),
            "{name} failed: status={:?} stderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("### Summary"),
            "{name} did not render compact markdown: {stdout}"
        );
    }

    server.join().expect("fake daemon joins");
    let names = observed.lock().expect("observed lock").clone();
    let set = names.into_iter().collect::<BTreeSet<_>>();
    assert_eq!(
        set,
        BTreeSet::from([
            "context".to_string(),
            "diagnose".to_string(),
            "impact".to_string(),
            "prepare_change".to_string(),
            "recall".to_string(),
            "remember".to_string(),
            "search".to_string(),
            "status".to_string(),
        ])
    );
}

#[test]
fn cli_recall_and_status_forward_file_filters() {
    let workspace = unique_workspace("cli-file-filters");
    std::fs::create_dir_all(workspace.join(".git")).expect("create git marker");
    let observed = Arc::new(Mutex::new(Vec::new()));
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake daemon");
    let addr = listener
        .local_addr()
        .expect("fake daemon address")
        .to_string();
    let server = thread::spawn({
        let observed = Arc::clone(&observed);
        move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().expect("accept");
                let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
                let mut hello = String::new();
                reader.read_line(&mut hello).expect("read hello");
                acknowledge_hello(&mut stream, &hello);
                let mut request = String::new();
                reader.read_line(&mut request).expect("read request");
                let request: Value = serde_json::from_str(request.trim()).expect("request json");
                observed.lock().expect("observed lock").push(request);

                let response = json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "result": {
                        "content": [{
                            "type": "text",
                            "text": "### Summary\n- fake daemon accepted file filters"
                        }]
                    }
                });
                writeln!(stream, "{response}").expect("write response");
            }
        }
    });

    for args in [
        [
            "recall",
            "resume auth task",
            "--mode",
            "task",
            "--focus-files",
            "src/auth.rs",
            "--focus-files",
            "src/session.rs",
        ]
        .as_slice(),
        [
            "status",
            "--scope",
            "docs",
            "--files",
            "src/auth.rs",
            "--files",
            "docs/auth.md",
        ]
        .as_slice(),
    ] {
        let output = run_lattice(&addr, &workspace, args, None);
        assert!(
            output.status.success(),
            "CLI file-filter request failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    server.join().expect("fake daemon joins");
    let observed = observed.lock().expect("observed lock");
    assert_eq!(observed[0]["params"]["name"], "recall");
    assert_eq!(
        observed[0]["params"]["arguments"]["focus_files"],
        json!(["src/auth.rs", "src/session.rs"])
    );
    assert_eq!(observed[1]["params"]["name"], "status");
    assert_eq!(
        observed[1]["params"]["arguments"]["files"],
        json!(["src/auth.rs", "docs/auth.md"])
    );
}

#[test]
fn cli_query_daemon_down_exits_two_with_actionable_message() {
    let workspace = unique_workspace("cli-down");
    std::fs::create_dir_all(workspace.join(".git")).expect("create git marker");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    let addr = listener.local_addr().expect("local addr").to_string();
    drop(listener);

    let output = run_lattice(&addr, &workspace, &["status"], None);
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("lattice daemon is not accepting connections at"));
    assert!(stderr.contains("start with: lattice --daemon"));
}

#[test]
fn cli_query_timeout_exits_three() {
    let workspace = unique_workspace("cli-timeout");
    std::fs::create_dir_all(workspace.join(".git")).expect("create git marker");
    let observed = Arc::new(Mutex::new(Vec::new()));
    let (addr, server) = start_fake_daemon(1, Arc::clone(&observed), FakeMode::Delay);

    let output = run_lattice(
        &addr,
        &workspace,
        &["context", "slow query", "--timeout", "0.05"],
        None,
    );
    assert_eq!(output.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&output.stderr).contains("timed out"));

    server.join().expect("fake daemon joins");
}

#[test]
fn bare_help_and_unknown_invocations_do_not_start_a_daemon() {
    let workspace = unique_workspace("cli-usage");
    let cases = [
        (Vec::<&str>::new(), 0, "Usage: lattice <command>"),
        (vec!["--help"], 0, "Runtime modes (explicit only)"),
        (vec!["frobnicate"], 64, "unknown subcommand `frobnicate`"),
        (
            vec!["--workspace", ".", "status"],
            64,
            "a command must come before --workspace",
        ),
    ];

    for (args, status, expected) in cases {
        let output = run_lattice("127.0.0.1:9", &workspace, &args, None);
        assert_eq!(output.status.code(), Some(status), "args={args:?}");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(text.contains(expected), "args={args:?}, output={text}");
    }
}

#[test]
fn stdio_proxy_exits_within_two_seconds_when_client_stdin_closes() {
    let workspace = unique_workspace("proxy-eof");
    std::fs::create_dir_all(workspace.join(".git")).expect("create git marker");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake daemon");
    let addr = listener
        .local_addr()
        .expect("fake daemon address")
        .to_string();
    let (connected_tx, connected_rx) = std::sync::mpsc::channel();
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept proxy");
        let mut reader = BufReader::new(stream);
        let mut hello = String::new();
        reader.read_line(&mut hello).expect("read proxy hello");
        assert!(
            hello.contains("workspace_roots"),
            "expected proxy hello: {hello}"
        );
        let mut stream = reader.into_inner();
        acknowledge_hello(&mut stream, &hello);
        connected_tx
            .send(Instant::now())
            .expect("signal proxy handshake");
        let mut reader = BufReader::new(stream);
        drain_until_disconnect(&mut reader);
    });

    let mut child = spawn_stdio_proxy(&addr, &workspace, &[]);

    // The client closes stdin before the proxy has even started: the hardest
    // case, because EOF is already pending when the proxy connects. The
    // lifecycle bound is on the proxy's own behaviour, from the moment it is
    // connected with EOF pending to the moment it exits. Measuring from spawn
    // instead timed process startup, and failed whenever the machine was busy.
    drop(child.stdin.take());
    let connected_at = connected_rx
        .recv_timeout(PROCESS_STARTUP_ALLOWANCE)
        .expect("stdio proxy did not start and complete its handshake");
    let status = wait_for_exit(&mut child, PROCESS_STARTUP_ALLOWANCE);
    let exited_after = connected_at.elapsed();
    assert!(
        status.success(),
        "stdio proxy failed after EOF: status={status:?}"
    );
    assert!(
        exited_after <= Duration::from_secs(2),
        "stdio proxy took {exited_after:?} to exit after connecting with EOF pending; the lifecycle bound is two seconds"
    );
    server.join().expect("fake daemon joins");
}

#[test]
fn stdio_proxy_stays_connected_while_client_stdin_is_open_and_idle() {
    let workspace = unique_workspace("proxy-idle-client");
    std::fs::create_dir_all(workspace.join(".git")).expect("create git marker");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake daemon");
    let addr = listener
        .local_addr()
        .expect("fake daemon address")
        .to_string();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept proxy");
        let mut reader = BufReader::new(stream.try_clone().expect("clone proxy stream"));
        let mut hello = String::new();
        reader.read_line(&mut hello).expect("read proxy hello");
        acknowledge_hello(&mut stream, &hello);

        let mut request = String::new();
        reader.read_line(&mut request).expect("read later request");
        let request: Value = serde_json::from_str(request.trim()).expect("request json");
        assert_eq!(request["method"], "tools/list");
        assert_eq!(request["id"], 7);
        writeln!(
            stream,
            r#"{{"jsonrpc":"2.0","id":7,"result":{{"tools":[]}}}}"#
        )
        .expect("write later response");

        drain_until_disconnect(&mut reader);
    });

    // The removed idle reaper honored this small value. Keeping it in the
    // regression fixture proves stale operator configuration is harmless.
    let mut child = spawn_stdio_proxy(
        &addr,
        &workspace,
        &[("LATTICE_PROXY_IDLE_TIMEOUT_SECS", "1")],
    );

    thread::sleep(Duration::from_millis(1200));
    assert!(
        child.try_wait().expect("poll idle proxy").is_none(),
        "stdio proxy exited while its owning client's stdin remained open"
    );
    writeln!(
        child.stdin.as_mut().expect("proxy stdin"),
        r#"{{"jsonrpc":"2.0","id":7,"method":"tools/list","params":{{}}}}"#
    )
    .expect("write request after idle gap");
    child
        .stdin
        .as_mut()
        .expect("proxy stdin")
        .flush()
        .expect("flush request after idle gap");

    let mut response = String::new();
    BufReader::new(child.stdout.take().expect("proxy stdout"))
        .read_line(&mut response)
        .expect("read response after idle gap");
    let response: Value = serde_json::from_str(response.trim()).expect("response json");
    assert_eq!(response["id"], 7);
    assert_eq!(response["result"]["tools"], json!([]));

    let closed_at = Instant::now();
    drop(child.stdin.take());
    assert!(wait_for_exit(&mut child, Duration::from_secs(2)).success());
    assert!(
        closed_at.elapsed() <= Duration::from_secs(2),
        "stdio proxy did not exit promptly after post-idle EOF"
    );
    server.join().expect("fake daemon joins");
}

#[test]
fn stdio_proxy_keeps_an_in_flight_request_connected_until_response() {
    let workspace = unique_workspace("proxy-in-flight");
    std::fs::create_dir_all(workspace.join(".git")).expect("create git marker");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake daemon");
    let addr = listener
        .local_addr()
        .expect("fake daemon address")
        .to_string();
    let (request_received_tx, request_received_rx) = std::sync::mpsc::channel();
    let (response_sent_tx, response_sent_rx) = std::sync::mpsc::channel();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept proxy");
        let mut reader = BufReader::new(stream.try_clone().expect("clone proxy stream"));
        let mut hello = String::new();
        reader.read_line(&mut hello).expect("read proxy hello");
        acknowledge_hello(&mut stream, &hello);
        let mut request = String::new();
        reader.read_line(&mut request).expect("read proxy request");
        request_received_tx.send(()).expect("signal proxy request");
        thread::sleep(Duration::from_millis(2300));
        writeln!(stream, "{}", r#"{"jsonrpc":"2.0","id":1,"result":{}}"#)
            .expect("write delayed response");
        response_sent_tx.send(()).expect("signal delayed response");
        drain_until_disconnect(&mut reader);
    });

    let mut child = spawn_stdio_proxy(&addr, &workspace, &[]);
    writeln!(
        child.stdin.as_mut().expect("proxy stdin"),
        "{}",
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#
    )
    .expect("write proxy request");
    child
        .stdin
        .as_mut()
        .expect("proxy stdin")
        .flush()
        .expect("flush proxy request");
    // Includes process startup, which is not what this test is about.
    request_received_rx
        .recv_timeout(PROCESS_STARTUP_ALLOWANCE)
        .expect("proxy did not start and forward the request to the daemon");

    thread::sleep(Duration::from_millis(2100));
    assert!(
        child.try_wait().expect("poll proxy").is_none(),
        "proxy exited while a daemon response was outstanding"
    );
    response_sent_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("proxy did not remain connected for delayed response");
    drop(child.stdin.take());
    assert!(wait_for_exit(&mut child, Duration::from_secs(2)).success());
    server.join().expect("fake daemon joins");
}

enum FakeMode {
    Immediate,
    Delay,
}

fn start_fake_daemon(
    expected_connections: usize,
    observed: Arc<Mutex<Vec<String>>>,
    mode: FakeMode,
) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake daemon");
    let addr = listener.local_addr().expect("local addr").to_string();
    let handle = thread::spawn(move || {
        for _ in 0..expected_connections {
            let (stream, _) = listener.accept().expect("accept");
            handle_connection(stream, &observed, &mode);
        }
    });
    (addr, handle)
}

fn handle_connection(stream: TcpStream, observed: &Arc<Mutex<Vec<String>>>, mode: &FakeMode) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
    let mut hello = String::new();
    reader.read_line(&mut hello).expect("read hello");
    let hello_value: Value = serde_json::from_str(hello.trim()).expect("hello json");
    assert!(
        hello_value["workspace_roots"]
            .as_array()
            .expect("workspace roots")
            .len()
            == 1
    );
    let mut stream = stream;
    acknowledge_hello(&mut stream, &hello);

    let mut request = String::new();
    reader.read_line(&mut request).expect("read request");
    let request_value: Value = serde_json::from_str(request.trim()).expect("request json");
    assert_eq!(request_value["method"], "tools/call");
    let tool = request_value["params"]["name"]
        .as_str()
        .expect("tool name")
        .to_string();
    observed.lock().expect("observed lock").push(tool.clone());

    if matches!(mode, FakeMode::Delay) {
        thread::sleep(Duration::from_millis(200));
    }

    let response = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {
            "content": [{
                "type": "text",
                "text": format!("### Summary\n- fake daemon accepted `{tool}`")
            }]
        }
    });
    let mut writer = stream;
    let _ = writeln!(writer, "{}", response);
}

/// How long a test waits for a spawned `lattice` to start, connect and
/// complete its handshake. Startup is not a behaviour under test here.
///
/// It is also not cheap under `cargo test`. Cargo re-places the binary on
/// every invocation, which gives it a new inode with identical content, and
/// macOS validates the code signature of an executable it has not launched
/// before. For this unoptimised binary of about 107 MB that first launch was
/// measured at 2.4 to 3.8 seconds on an idle ten-core machine, against 20 ms
/// for the second launch of the same file, and it grows with load. Bounds of
/// two seconds measured from `spawn` were therefore timing the operating
/// system, and failed on every run once the machine was busy. Thirty seconds
/// is roughly ten times the idle measurement. A proxy that has not connected
/// by then is broken, not slow.
const PROCESS_STARTUP_ALLOWANCE: Duration = Duration::from_secs(30);

/// Every `lattice` process these tests start, built one way.
///
/// The fake daemon's address and credential are not enough isolation. A
/// proxy that loses its daemon reconnects, and if nothing is listening it
/// starts a real `lattice --daemon`. Before this helper a failing test left
/// exactly that behind: a surviving proxy auto-started a real daemon on the
/// test's random port, under the developer's real HOME, where it opened the
/// same hook-session state as their live daemon. So each process gets a
/// private HOME and state, config and log roots, and `LATTICE_DAEMON_EXE`
/// names a program that exits at once, so auto-start cannot produce a daemon.
fn lattice_command(addr: &str) -> Command {
    let sandbox = fake_runtime_base(addr).join("sandbox-home");
    std::fs::create_dir_all(&sandbox).expect("create sandbox home");
    let mut command = Command::new(env!("CARGO_BIN_EXE_lattice"));
    command
        .env("LATTICE_DAEMON_ADDR", addr)
        .env("XDG_RUNTIME_DIR", install_fake_credential(addr))
        .env("HOME", &sandbox)
        .env("XDG_STATE_HOME", sandbox.join("state"))
        .env("XDG_CONFIG_HOME", sandbox.join("config"))
        .env("LATTICE_LIFECYCLE_LOG_DIR", sandbox.join("logs"))
        .env("LATTICE_DAEMON_EXE", NOT_A_DAEMON)
        .env_remove("LATTICE_MAX_LOADED_SHARDS")
        .env_remove("LATTICE_MAX_LOADED_WORKSPACES");
    command
}

#[cfg(unix)]
const NOT_A_DAEMON: &str = "/usr/bin/false";
#[cfg(not(unix))]
const NOT_A_DAEMON: &str = "C:\\Windows\\System32\\where.exe";

/// Kills and reaps its process when dropped. A panicking assertion must not
/// leave a proxy running after its test's fake daemon has gone.
struct OwnedChild(std::process::Child);

impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl std::ops::Deref for OwnedChild {
    type Target = std::process::Child;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for OwnedChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

fn spawn_stdio_proxy(addr: &str, workspace: &PathBuf, extra_env: &[(&str, &str)]) -> OwnedChild {
    let mut command = lattice_command(addr);
    command
        .args(["--stdio", "--workspace"])
        .arg(workspace)
        .envs(extra_env.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    OwnedChild(command.spawn().expect("spawn stdio proxy"))
}

fn run_lattice(
    addr: &str,
    cwd: &PathBuf,
    args: &[&str],
    stdin_text: Option<&str>,
) -> std::process::Output {
    let mut command = lattice_command(addr);
    command
        .args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if stdin_text.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = command.spawn().expect("spawn lattice");
    if let Some(text) = stdin_text {
        child
            .stdin
            .as_mut()
            .expect("stdin")
            .write_all(text.as_bytes())
            .expect("write stdin");
    }
    child.wait_with_output().expect("wait")
}

/// Read until the proxy has gone away. These fake daemons drain only to
/// outlive the proxy; how the connection ends is not under test. A proxy that
/// exits closes its socket, and the peer may see that as a clean end of
/// stream or as a reset depending on timing, so both count as gone. Any other
/// error is still a failure.
fn drain_until_disconnect(reader: &mut impl BufRead) {
    let mut remainder = String::new();
    loop {
        match reader.read_line(&mut remainder) {
            Ok(0) => return,
            Ok(_) => remainder.clear(),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
                ) =>
            {
                return
            }
            Err(error) => panic!("read from proxy failed: {error}"),
        }
    }
}

fn acknowledge_hello(stream: &mut TcpStream, hello: &str) {
    let hello: Value = serde_json::from_str(hello.trim()).expect("authenticated hello json");
    assert_eq!(hello["protocol_version"], 1);
    assert_eq!(hello["transport_token"], "22".repeat(32));
    let ack = json!({
        "protocol_version": 1,
        "daemon_epoch": hello["daemon_epoch"],
        "connection_id": "33".repeat(16),
        "accepted_features": ["json_rpc_2_0"]
    });
    writeln!(stream, "{ack}").expect("write transport ack");
    stream.flush().expect("flush transport ack");
}

fn install_fake_credential(addr: &str) -> PathBuf {
    let base = fake_runtime_base(addr);
    let directory = base.join("lattice");
    std::fs::create_dir_all(&directory).expect("create fake protected runtime");
    #[cfg(unix)]
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
        .expect("protect fake runtime");
    let mut hasher = DefaultHasher::new();
    addr.hash(&mut hasher);
    let path = directory.join(format!("transport-{:016x}.json", hasher.finish()));
    let record = json!({
        "protocol_version": 1,
        "daemon_epoch": "11".repeat(32),
        "transport_token": "22".repeat(32),
        "listener_address": addr,
        "daemon_pid": std::process::id()
    });
    std::fs::write(&path, serde_json::to_vec(&record).unwrap()).expect("write fake credential");
    #[cfg(unix)]
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .expect("protect fake credential");
    base
}

fn fake_runtime_base(addr: &str) -> PathBuf {
    let mut hasher = DefaultHasher::new();
    addr.hash(&mut hasher);
    std::env::temp_dir().join(format!(
        "lattice-cli-query-runtime-{}-{:016x}",
        std::process::id(),
        hasher.finish()
    ))
}

fn wait_for_exit(child: &mut std::process::Child, limit: Duration) -> std::process::ExitStatus {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait().expect("poll child") {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "child did not exit within {limit:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn unique_workspace(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "lattice-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).expect("create temp workspace");
    root
}
