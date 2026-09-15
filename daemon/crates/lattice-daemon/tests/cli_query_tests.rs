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
        let mut reader = BufReader::new(stream);
        let mut remainder = String::new();
        while reader.read_line(&mut remainder).expect("read proxy EOF") != 0 {
            remainder.clear();
        }
    });

    let mut child = Command::new(env!("CARGO_BIN_EXE_lattice"))
        .args(["--stdio", "--workspace"])
        .arg(&workspace)
        .env("LATTICE_DAEMON_ADDR", &addr)
        .env("XDG_RUNTIME_DIR", install_fake_credential(&addr))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn stdio proxy");

    let closed_at = Instant::now();
    drop(child.stdin.take());
    let status = wait_for_exit(&mut child, Duration::from_secs(2));
    assert!(
        status.success(),
        "stdio proxy failed after EOF: status={status:?}"
    );
    assert!(
        closed_at.elapsed() <= Duration::from_secs(2),
        "stdio proxy exceeded the two-second EOF lifecycle bound"
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

        let mut remainder = String::new();
        while reader.read_line(&mut remainder).expect("read proxy EOF") != 0 {
            remainder.clear();
        }
    });

    let mut child = Command::new(env!("CARGO_BIN_EXE_lattice"))
        .args(["--stdio", "--workspace"])
        .arg(&workspace)
        .env("LATTICE_DAEMON_ADDR", &addr)
        .env("XDG_RUNTIME_DIR", install_fake_credential(&addr))
        // The removed idle reaper honored this small value. Keeping it in the
        // regression fixture proves stale operator configuration is harmless.
        .env("LATTICE_PROXY_IDLE_TIMEOUT_SECS", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn stdio proxy");

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
        let mut remainder = String::new();
        while reader.read_line(&mut remainder).expect("read proxy EOF") != 0 {
            remainder.clear();
        }
    });

    let mut child = Command::new(env!("CARGO_BIN_EXE_lattice"))
        .args(["--stdio", "--workspace"])
        .arg(&workspace)
        .env("LATTICE_DAEMON_ADDR", &addr)
        .env("XDG_RUNTIME_DIR", install_fake_credential(&addr))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn stdio proxy");
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
    request_received_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("proxy did not forward request to daemon");

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

fn run_lattice(
    addr: &str,
    cwd: &PathBuf,
    args: &[&str],
    stdin_text: Option<&str>,
) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_lattice"));
    command
        .args(args)
        .current_dir(cwd)
        .env("LATTICE_DAEMON_ADDR", addr)
        .env("XDG_RUNTIME_DIR", install_fake_credential(addr))
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
