use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

#[test]
fn cli_query_subcommands_call_public_tools_over_daemon_protocol() {
    let workspace = unique_workspace("cli-happy");
    std::fs::create_dir_all(workspace.join(".git")).expect("create git marker");
    let observed = Arc::new(Mutex::new(Vec::new()));
    let (addr, server) = start_fake_daemon(7, Arc::clone(&observed), FakeMode::Immediate);

    let cases: Vec<(&str, Vec<&str>, Option<&str>)> = vec![
        ("context", vec!["context", "auth flow"], None),
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
            "recall".to_string(),
            "remember".to_string(),
            "search".to_string(),
            "status".to_string(),
        ])
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
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("lattice daemon not running — start with: lattice --daemon"));
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
