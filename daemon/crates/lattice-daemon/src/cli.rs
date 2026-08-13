use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

use crate::adoption_metrics::render_metrics_for_workspace;
use crate::proxy::{daemon_addr, ProxyHello};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub(crate) struct CliRequest {
    tool: String,
    arguments: Value,
    workspace: PathBuf,
    json: bool,
    timeout: Duration,
}

#[derive(Debug)]
enum CliError {
    DaemonUnavailable,
    Rpc(String),
    Other(anyhow::Error),
}

impl From<anyhow::Error> for CliError {
    fn from(error: anyhow::Error) -> Self {
        Self::Other(error)
    }
}

pub(crate) fn is_cli_query_command() -> bool {
    matches!(
        std::env::args().nth(1).as_deref(),
        Some("context")
            | Some("prepare_change")
            | Some("impact")
            | Some("search")
            | Some("diagnose")
            | Some("remember")
            | Some("recall")
            | Some("status")
            | Some("metrics")
    )
}

/// Reject invocations which are neither an explicit runtime mode nor a public
/// query command, so malformed commands cannot silently start a daemon.
pub(crate) fn run_usage_or_error() -> i32 {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        None | Some("--help") | Some("-h") => {
            println!("{}", usage());
            0
        }
        Some("--workspace") | Some("-w") => {
            eprintln!(
                "lattice: a command must come before --workspace; try `lattice status --workspace <path>`"
            );
            64
        }
        Some(command) => {
            eprintln!("lattice: unknown subcommand `{command}`\n\n{}", usage());
            64
        }
    }
}

fn usage() -> &'static str {
    "Usage: lattice <command> [options]\n\nCommands:\n  context\n  prepare_change\n  impact\n  search\n  diagnose\n  remember\n  recall\n  status\n  metrics\n  doctor\n  memory-migrate\n\nRuntime modes (explicit only):\n  --daemon\n  --stdio"
}

pub(crate) async fn run_from_env() -> i32 {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("metrics") {
        return run_metrics_command(args);
    }
    match parse_args(args) {
        Ok(request) => run_request(request).await,
        Err(error) => {
            eprintln!("lattice: {}", error);
            1
        }
    }
}

async fn run_request(request: CliRequest) -> i32 {
    let timeout = request.timeout;
    match tokio::time::timeout(timeout, call_daemon(&request)).await {
        Ok(Ok(result)) => {
            let output = render_output(&result, request.json);
            if !output.trim().is_empty() {
                println!("{}", output.trim_end());
                0
            } else {
                1
            }
        }
        Ok(Err(CliError::DaemonUnavailable)) => {
            eprintln!("lattice daemon not running — start with: lattice --daemon");
            2
        }
        Err(_) => {
            eprintln!(
                "lattice query timed out after {:.3}s — partial results unavailable",
                timeout.as_secs_f64()
            );
            3
        }
        Ok(Err(CliError::Rpc(message))) => {
            eprintln!("lattice daemon error: {}", message);
            1
        }
        Ok(Err(CliError::Other(error))) => {
            eprintln!("lattice: {}", error);
            1
        }
    }
}

async fn call_daemon(request: &CliRequest) -> Result<Value, CliError> {
    let mut stream = TcpStream::connect(daemon_addr())
        .await
        .map_err(|_| CliError::DaemonUnavailable)?;
    let hello = ProxyHello {
        workspace_roots: vec![request.workspace.to_string_lossy().to_string()],
        focus_files: Vec::new(),
        focus_dirs: Vec::new(),
    };
    write_json_line(
        &mut stream,
        &serde_json::to_value(hello).map_err(anyhow::Error::from)?,
    )
    .await?;
    let mut arguments = request.arguments.clone();
    attach_invocation_metadata(&mut arguments);
    let rpc = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": request.tool,
            "arguments": arguments,
        }
    });
    write_json_line(&mut stream, &rpc).await?;

    let mut lines = BufReader::new(stream).lines();
    while let Some(line) = lines.next_line().await.map_err(anyhow::Error::from)? {
        if line.trim().is_empty() {
            continue;
        }
        let response: Value = serde_json::from_str(&line).map_err(anyhow::Error::from)?;
        if response.get("id").and_then(Value::as_i64) != Some(1) {
            continue;
        }
        if let Some(error) = response.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown JSON-RPC error")
                .to_string();
            return Err(CliError::Rpc(message));
        }
        return response
            .get("result")
            .cloned()
            .ok_or_else(|| CliError::Rpc("missing JSON-RPC result".to_string()));
    }
    Err(CliError::DaemonUnavailable)
}

fn run_metrics_command(args: Vec<String>) -> i32 {
    match parse_metrics_args(args)
        .and_then(|(workspace, days, json)| render_metrics_for_workspace(&workspace, days, json))
    {
        Ok(output) => {
            println!("{}", output.trim_end());
            0
        }
        Err(error) => {
            eprintln!("lattice: {}", error);
            1
        }
    }
}

fn parse_metrics_args(args: Vec<String>) -> Result<(PathBuf, usize, bool)> {
    let mut workspace = None;
    let mut days = 14usize;
    let mut json = false;
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--json" => json = true,
            "--workspace" | "-w" => {
                let value = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("--workspace requires a path"))?;
                workspace = Some(canonical_workspace(Path::new(value))?);
                i += 1;
            }
            "--days" => {
                let value = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("--days requires a positive integer"))?;
                days = value
                    .parse::<usize>()
                    .with_context(|| format!("invalid --days `{}`", value))?;
                if days == 0 {
                    return Err(anyhow!("--days must be positive"));
                }
                i += 1;
            }
            other => return Err(anyhow!("unknown metrics argument `{}`", other)),
        }
        i += 1;
    }
    Ok((workspace.unwrap_or(detect_workspace_root()?), days, json))
}

fn attach_invocation_metadata(arguments: &mut Value) {
    if std::env::var("LATTICE_SKIP_METRICS").ok().as_deref() == Some("1") {
        return;
    }
    let client = std::env::var("LATTICE_CLIENT_NAME").unwrap_or_else(|_| "lattice-cli".to_string());
    let channel = std::env::var("LATTICE_CLIENT_CHANNEL").unwrap_or_else(|_| "cli".to_string());
    set_default(arguments, "_lattice_client", json!(client));
    set_default(arguments, "_lattice_channel", json!(channel));
}

async fn write_json_line(stream: &mut TcpStream, value: &Value) -> Result<(), CliError> {
    let mut text = serde_json::to_string(value).map_err(anyhow::Error::from)?;
    text.push('\n');
    stream
        .write_all(text.as_bytes())
        .await
        .map_err(anyhow::Error::from)?;
    stream.flush().await.map_err(anyhow::Error::from)?;
    Ok(())
}

fn parse_args(args: Vec<String>) -> Result<CliRequest> {
    let mut parser = ArgParser::new(args)?;
    let command = parser.command.clone();
    let mut request = match command.as_str() {
        "context" => parse_context(&mut parser)?,
        "prepare_change" => parse_prepare_change(&mut parser)?,
        "impact" => parse_impact(&mut parser)?,
        "search" => parse_search(&mut parser)?,
        "diagnose" => parse_diagnose(&mut parser)?,
        "remember" => parse_remember(&mut parser)?,
        "recall" => parse_recall(&mut parser)?,
        "status" => parse_status(&mut parser)?,
        other => return Err(anyhow!("unknown lattice CLI command `{}`", other)),
    };
    parser.reject_unknown_flags()?;
    parser.apply_common(&mut request)?;
    if !request.json {
        set_default(&mut request.arguments, "render", json!("markdown"));
        set_default(&mut request.arguments, "render_mode", json!("compact"));
        set_default(&mut request.arguments, "budget", json!("compact"));
    }
    Ok(request)
}

fn parse_context(parser: &mut ArgParser) -> Result<CliRequest> {
    let mode = parser
        .take_flag_value("--mode")?
        .unwrap_or_else(|| "subsystem".to_string());
    let files = parser.take_repeated("--files")?;
    let min_relevance = parser.take_flag_value("--min-relevance")?;
    let query = parser.join_positionals();
    if query.trim().is_empty() {
        return Err(anyhow!("context requires a query"));
    }
    let mut arguments = json!({ "query": query });
    set_value(&mut arguments, "mode", json!(mode));
    if let Some(min_relevance) = min_relevance {
        let score = min_relevance
            .parse::<f64>()
            .with_context(|| format!("invalid --min-relevance `{}`", min_relevance))?;
        if !(score.is_finite() && (0.0..=1.0).contains(&score)) {
            return Err(anyhow!("--min-relevance must be a number from 0.0 to 1.0"));
        }
        set_value(&mut arguments, "min_relevance", json!(score));
    }
    if !files.is_empty() {
        set_value(&mut arguments, "files", json!(files));
    }
    Ok(parser.request("context", arguments))
}

fn parse_prepare_change(parser: &mut ArgParser) -> Result<CliRequest> {
    let mode = parser
        .take_flag_value("--mode")?
        .unwrap_or_else(|| "prepare".to_string());
    let task = parser.join_positionals();
    if task.trim().is_empty() {
        return Err(anyhow!("prepare_change requires a task"));
    }
    Ok(parser.request(
        "prepare_change",
        json!({
            "task": task,
            "mode": mode,
        }),
    ))
}

fn parse_impact(parser: &mut ArgParser) -> Result<CliRequest> {
    let include_tests = !parser.take_bool("--no-tests");
    let direction = parser.take_flag_value("--direction")?;
    let diff = parser.take_bool("--diff");
    let mut arguments = json!({ "include_tests": include_tests });
    if let Some(direction) = direction {
        set_value(&mut arguments, "direction", json!(direction));
    }
    if diff {
        set_value(&mut arguments, "diff", json!(read_stdin()?));
    } else {
        let target = parser.join_positionals();
        if target.trim().is_empty() {
            return Err(anyhow!("impact requires a symbol, path, or --diff"));
        }
        if looks_like_path_target(&target) {
            set_value(&mut arguments, "file", json!(target));
        } else {
            set_value(&mut arguments, "target", json!(target));
        }
    }
    Ok(parser.request("impact", arguments))
}

fn looks_like_path_target(target: &str) -> bool {
    target.contains('/')
        || target.contains('\\')
        || Path::new(target).exists()
        || target.rsplit_once('.').is_some_and(|(_, ext)| {
            ext.chars().all(|ch| ch.is_ascii_alphanumeric()) && (1..=8).contains(&ext.len())
        })
}

fn parse_search(parser: &mut ArgParser) -> Result<CliRequest> {
    let kind = parser
        .take_flag_value("--kind")?
        .unwrap_or_else(|| "symbol".to_string());
    let direction = parser.take_flag_value("--direction")?;
    let from = parser.take_flag_value("--from")?;
    let to = parser.take_flag_value("--to")?;
    let mut arguments = json!({ "kind": kind });
    if let Some(direction) = direction {
        set_value(&mut arguments, "direction", json!(direction));
    }
    if let Some(from) = from {
        set_value(&mut arguments, "from", json!(from));
    }
    if let Some(to) = to {
        set_value(&mut arguments, "to", json!(to));
    }
    let query = parser.join_positionals();
    if !query.trim().is_empty() {
        set_value(&mut arguments, "query", json!(query));
    } else if arguments.get("from").is_none() && arguments.get("to").is_none() {
        return Err(anyhow!("search requires a query"));
    }
    Ok(parser.request("search", arguments))
}

fn parse_diagnose(parser: &mut ArgParser) -> Result<CliRequest> {
    let text = if parser.positionals.is_empty()
        || parser.positionals.iter().any(|value| value.as_str() == "-")
    {
        read_stdin()?
    } else {
        parser.join_positionals()
    };
    if text.trim().is_empty() {
        return Err(anyhow!("diagnose requires failure text or '-' for stdin"));
    }
    Ok(parser.request("diagnose", json!({ "failure_text": text })))
}

fn parse_remember(parser: &mut ArgParser) -> Result<CliRequest> {
    let kind = parser
        .take_flag_value("--kind")?
        .unwrap_or_else(|| "quick".to_string());
    let content = parser.join_positionals();
    if content.trim().is_empty() {
        return Err(anyhow!("remember requires content"));
    }
    let mut arguments = json!({
        "kind": kind,
        "content": content,
    });
    if arguments.get("kind").and_then(Value::as_str) == Some("outcome") {
        set_default(&mut arguments, "task", json!(content));
        set_default(&mut arguments, "summary", json!(content));
    }
    Ok(parser.request("remember", arguments))
}

fn parse_recall(parser: &mut ArgParser) -> Result<CliRequest> {
    let mode = parser
        .take_flag_value("--mode")?
        .unwrap_or_else(|| "search".to_string());
    let task_id = parser.take_flag_value("--task-id")?;
    let query = parser.join_positionals();
    if query.trim().is_empty() {
        return Err(anyhow!("recall requires a query"));
    }
    let mut arguments = json!({
        "mode": mode,
        "query": query,
    });
    if arguments.get("mode").and_then(Value::as_str) == Some("task") {
        let id = task_id.unwrap_or_else(|| query.clone());
        set_value(&mut arguments, "task_id", json!(id));
        set_value(&mut arguments, "task_statement", json!(query));
    }
    Ok(parser.request("recall", arguments))
}

fn parse_status(parser: &mut ArgParser) -> Result<CliRequest> {
    let scope = parser
        .take_flag_value("--scope")?
        .unwrap_or_else(|| "index".to_string());
    let query = parser.join_positionals();
    let mut arguments = json!({ "scope": scope });
    if !query.trim().is_empty() {
        set_value(&mut arguments, "query", json!(query));
    }
    Ok(parser.request("status", arguments))
}

#[derive(Debug)]
struct ArgParser {
    command: String,
    positionals: Vec<String>,
    json: bool,
    timeout: Duration,
    workspace: Option<PathBuf>,
}

impl ArgParser {
    fn new(args: Vec<String>) -> Result<Self> {
        let command = args
            .get(1)
            .cloned()
            .ok_or_else(|| anyhow!("missing lattice CLI command"))?;
        let mut parser = Self {
            command,
            positionals: Vec::new(),
            json: false,
            timeout: DEFAULT_TIMEOUT,
            workspace: None,
        };
        let mut i = 2;
        while i < args.len() {
            match args[i].as_str() {
                "--json" => parser.json = true,
                "--timeout" => {
                    let value = args
                        .get(i + 1)
                        .ok_or_else(|| anyhow!("--timeout requires seconds"))?;
                    parser.timeout = parse_timeout(value)?;
                    i += 1;
                }
                "--workspace" | "-w" => {
                    let value = args
                        .get(i + 1)
                        .ok_or_else(|| anyhow!("--workspace requires a path"))?;
                    parser.workspace = Some(canonical_workspace(Path::new(value))?);
                    i += 1;
                }
                value => parser.positionals.push(value.to_string()),
            }
            i += 1;
        }
        Ok(parser)
    }

    fn take_bool(&mut self, flag: &str) -> bool {
        let before = self.positionals.len();
        self.positionals.retain(|value| value != flag);
        before != self.positionals.len()
    }

    fn take_flag_value(&mut self, flag: &str) -> Result<Option<String>> {
        let mut next = Vec::with_capacity(self.positionals.len());
        let mut value = None;
        let mut i = 0;
        while i < self.positionals.len() {
            if self.positionals[i] == flag {
                let item = self
                    .positionals
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("{} requires a value", flag))?
                    .clone();
                value = Some(item);
                i += 2;
            } else {
                next.push(self.positionals[i].clone());
                i += 1;
            }
        }
        self.positionals = next;
        Ok(value)
    }

    fn take_repeated(&mut self, flag: &str) -> Result<Vec<String>> {
        let mut next = Vec::with_capacity(self.positionals.len());
        let mut values = Vec::new();
        let mut i = 0;
        while i < self.positionals.len() {
            if self.positionals[i] == flag {
                let item = self
                    .positionals
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("{} requires a value", flag))?
                    .clone();
                values.push(item);
                i += 2;
            } else {
                next.push(self.positionals[i].clone());
                i += 1;
            }
        }
        self.positionals = next;
        Ok(values)
    }

    fn join_positionals(&self) -> String {
        self.positionals
            .iter()
            .filter(|value| !value.starts_with("--"))
            .cloned()
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn reject_unknown_flags(&self) -> Result<()> {
        if let Some(flag) = self
            .positionals
            .iter()
            .find(|value| value.starts_with("--"))
        {
            return Err(anyhow!("unknown argument `{}`", flag));
        }
        Ok(())
    }

    fn apply_common(&self, request: &mut CliRequest) -> Result<()> {
        request.json = self.json;
        request.timeout = self.timeout;
        request.workspace = match &self.workspace {
            Some(path) => path.clone(),
            None => detect_workspace_root()?,
        };
        Ok(())
    }

    fn request(&self, tool: &str, arguments: Value) -> CliRequest {
        CliRequest {
            tool: tool.to_string(),
            arguments,
            workspace: PathBuf::new(),
            json: self.json,
            timeout: self.timeout,
        }
    }
}

fn parse_timeout(value: &str) -> Result<Duration> {
    let seconds = value
        .parse::<f64>()
        .with_context(|| format!("invalid --timeout `{}`", value))?;
    if !(seconds.is_finite() && seconds > 0.0) {
        return Err(anyhow!("--timeout must be a positive number of seconds"));
    }
    Ok(Duration::from_secs_f64(seconds))
}

fn canonical_workspace(path: &Path) -> Result<PathBuf> {
    if !path.is_dir() {
        return Err(anyhow!("workspace `{}` is not a directory", path.display()));
    }
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    validate_cli_workspace(&canonical)?;
    Ok(canonical)
}

fn detect_workspace_root() -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    for ancestor in cwd.ancestors() {
        if ancestor.join(".mcp.json").is_file()
            || ancestor.join(".lattice").is_dir()
            || ancestor.join(".git").exists()
        {
            return canonical_workspace(ancestor);
        }
    }
    for ancestor in cwd.ancestors() {
        if ancestor.join("Cargo.toml").is_file()
            || ancestor.join("package.json").is_file()
            || ancestor.join("pyproject.toml").is_file()
        {
            return canonical_workspace(ancestor);
        }
    }
    canonical_workspace(&cwd)
}

fn validate_cli_workspace(path: &Path) -> Result<()> {
    if path.parent().is_none() {
        return Err(anyhow!(
            "refusing workspace root `{}`: filesystem root is not a valid Lattice workspace",
            path.display()
        ));
    }
    if let Some(home) = std::env::var_os("HOME")
        .map(PathBuf::from)
        .and_then(|home| home.canonicalize().ok())
    {
        if path == home {
            return Err(anyhow!(
                "refusing workspace root `{}`: the user's home directory is not a valid Lattice workspace; choose a project directory instead",
                path.display()
            ));
        }
    }
    Ok(())
}

fn read_stdin() -> Result<String> {
    let mut text = String::new();
    std::io::stdin()
        .read_to_string(&mut text)
        .context("failed to read stdin")?;
    Ok(text)
}

fn set_value(value: &mut Value, key: &str, item: Value) {
    if let Some(object) = value.as_object_mut() {
        object.insert(key.to_string(), item);
    }
}

fn set_default(value: &mut Value, key: &str, item: Value) {
    if value.get(key).is_none() {
        set_value(value, key, item);
    }
}

fn render_output(result: &Value, json_output: bool) -> String {
    if json_output {
        return serde_json::to_string_pretty(result).unwrap_or_else(|_| result.to_string());
    }
    let Some(text) = first_text_block(result) else {
        return render_value_markdown(result);
    };
    if let Ok(value) = serde_json::from_str::<Value>(&text) {
        if let Some(markdown) = value.get("markdown").and_then(Value::as_str) {
            return markdown.to_string();
        }
        return render_value_markdown(&value);
    }
    text
}

fn first_text_block(value: &Value) -> Option<String> {
    value
        .get("content")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .and_then(|item| item.get("text"))
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

fn render_value_markdown(value: &Value) -> String {
    match value {
        Value::Object(object) => {
            let title = object
                .get("tool")
                .and_then(Value::as_str)
                .or_else(|| object.get("status").and_then(Value::as_str))
                .unwrap_or("Result");
            let mut out = format!("### {}\n", title);
            for (key, item) in object.iter().take(24) {
                out.push_str(&format!("- `{}`: {}\n", key, compact_value(item)));
            }
            out
        }
        Value::Array(items) => {
            let mut out = format!("### Results\n- `count`: {}\n", items.len());
            for item in items.iter().take(10) {
                out.push_str(&format!("- {}\n", compact_value(item)));
            }
            out
        }
        _ => compact_value(value),
    }
}

fn compact_value(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::String(value) => value.clone(),
        Value::Array(values) => format!("[{} item(s)]", values.len()),
        Value::Object(object) => {
            if let Some(summary) = object.get("summary").and_then(Value::as_str) {
                summary.to_string()
            } else if let Some(name) = object.get("name").and_then(Value::as_str) {
                name.to_string()
            } else if let Some(file) = object.get("file").and_then(Value::as_str) {
                file.to_string()
            } else {
                format!("{{{} field(s)}}", object.len())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_context_command_with_common_flags() {
        let request = parse_args(vec![
            "lattice".into(),
            "context".into(),
            "auth flow".into(),
            "--mode".into(),
            "docs".into(),
            "--files".into(),
            "README.md".into(),
            "--min-relevance".into(),
            "0.35".into(),
            "--json".into(),
            "--timeout".into(),
            "1.5".into(),
            "--workspace".into(),
            std::env::current_dir()
                .unwrap()
                .to_string_lossy()
                .to_string(),
        ])
        .expect("parse succeeds");
        assert_eq!(request.tool, "context");
        assert_eq!(request.arguments["query"], "auth flow");
        assert_eq!(request.arguments["mode"], "docs");
        assert_eq!(request.arguments["min_relevance"], 0.35);
        assert!(request.json);
        assert_eq!(request.timeout, Duration::from_millis(1500));
    }

    #[test]
    fn parses_impact_path_as_file_argument() {
        let request = parse_args(vec![
            "lattice".into(),
            "impact".into(),
            "daemon/crates/lattice-daemon/src/cli.rs".into(),
            "--no-tests".into(),
        ])
        .expect("parse succeeds");
        assert_eq!(request.tool, "impact");
        assert_eq!(
            request.arguments["file"],
            "daemon/crates/lattice-daemon/src/cli.rs"
        );
        assert!(request.arguments["target"].is_null());
        assert_eq!(request.arguments["include_tests"], false);
    }

    #[test]
    fn renders_wrapped_markdown_text_directly() {
        let result = json!({
            "content": [{
                "type": "text",
                "text": "### Summary\n- ok"
            }]
        });
        assert_eq!(render_output(&result, false), "### Summary\n- ok");
    }
}
