use super::*;

#[test]
fn test_should_index_typescript() {
    assert!(should_index_file("src/app.ts"));
    assert!(should_index_file("components/Button.tsx"));
    assert!(should_index_file("lib/utils.js"));
}

#[test]
fn test_should_index_python() {
    assert!(should_index_file("main.py"));
    assert!(should_index_file("stubs/typing.pyi"));
}

#[test]
fn test_should_not_index_non_code() {
    assert!(should_index_file("README.md"));
    assert!(should_index_file("docs/runbook.md"));
    assert!(!should_index_file("package.json"));
    assert!(!should_index_file("logo.png"));
    assert!(!should_index_file(".env"));
}

#[test]
fn test_should_not_index_excluded_dirs() {
    assert!(!should_index_file("node_modules/express/index.js"));
    assert!(!should_index_file(".git/hooks/pre-commit"));
    assert!(!should_index_file("target/debug/main.rs"));
    assert!(!should_index_file(".claude/worktrees/agent-123/src/app.py"));
    assert!(!should_index_file(".codex/worktrees/task-123/src/app.py"));
    assert!(!should_index_file(
        "docs/audit/2026-05-18-remediation-run/worktrees/IU-001/backend/app.py"
    ));
    assert!(!should_index_file(".pytest_cache/README.md"));
    assert!(!should_index_file("backend/.ruff_cache/0.9.0/file.py"));
    assert!(!should_index_file(".playwright-mcp/session/state.md"));
    assert!(!should_index_file(".agents/task-notes.md"));
}

#[test]
fn test_should_not_index_secrets() {
    assert!(!should_index_file(".env"));
    assert!(!should_index_file(".env.local"));
    assert!(!should_index_file("credentials.json"));
    assert!(!should_index_file("id_rsa"));
    assert!(!should_index_file("server.key"));
    assert!(!should_index_file("cert.pem"));
}
