use super::*;
use std::path::PathBuf;

#[test]
fn test_default_exclusions() {
    // Use a non-existent path so no .lattice_ignore file is loaded
    let filter = SecurityFilter::new(&PathBuf::from("/nonexistent/workspace"));

    assert!(filter.is_excluded(".env"), ".env should be excluded");
    assert!(
        filter.is_excluded("config/.env.production"),
        ".env.production should be excluded"
    );
    assert!(
        filter.is_excluded("credentials.json"),
        "credentials.json should be excluded"
    );
    assert!(
        filter.is_excluded("keys/id_rsa"),
        "id_rsa should be excluded"
    );
    assert!(
        filter.is_excluded("certs/server.pem"),
        ".pem should be excluded"
    );
    assert!(
        filter.is_excluded("certs/server.key"),
        ".key should be excluded"
    );
}

#[test]
fn test_normal_files_not_excluded() {
    let filter = SecurityFilter::new(&PathBuf::from("/nonexistent/workspace"));

    assert!(
        !filter.is_excluded("src/auth.ts"),
        "auth.ts should not be excluded"
    );
    assert!(
        !filter.is_excluded("helpers/utils.py"),
        "utils.py should not be excluded"
    );
    assert!(
        !filter.is_excluded("README.md"),
        "README.md should not be excluded"
    );
    assert!(
        !filter.is_excluded("src/index.js"),
        "index.js should not be excluded"
    );
}

#[test]
fn test_redact_password() {
    let source = r#"const config = {
    host: "localhost",
    password=abc123
    port: 5432
};"#;

    let redacted = SecurityFilter::redact_content(source);
    assert!(
        redacted.contains("[REDACTED]"),
        "password line should be redacted"
    );
    assert!(
        !redacted.contains("abc123"),
        "password value should not appear"
    );
    assert!(
        redacted.contains("host"),
        "non-sensitive lines should remain"
    );
    assert!(
        redacted.contains("port"),
        "non-sensitive lines should remain"
    );
}

#[test]
fn test_redact_aws_key() {
    let source = r#"# Config
aws_key = "AKIAIOSFODNN7EXAMPLE"
region = "us-east-1"
"#;

    let redacted = SecurityFilter::redact_content(source);
    assert!(
        redacted.contains("[REDACTED]"),
        "AWS key line should be redacted"
    );
    assert!(
        !redacted.contains("AKIAIOSFODNN7EXAMPLE"),
        "AWS key should not appear"
    );
    assert!(
        redacted.contains("region"),
        "non-sensitive lines should remain"
    );
}

#[test]
fn test_excluded_dirs() {
    let filter = SecurityFilter::new(&PathBuf::from("/nonexistent/workspace"));

    assert!(
        filter.is_excluded("node_modules/express/index.js"),
        "node_modules should be excluded"
    );
    assert!(filter.is_excluded(".git/config"), ".git should be excluded");
    assert!(
        filter.is_excluded(".claude/worktrees/agent-123/src/app.py"),
        ".claude assistant worktrees should be excluded"
    );
    assert!(
        filter.is_excluded(".codex/worktrees/task-123/src/app.py"),
        ".codex assistant worktrees should be excluded"
    );
    assert!(
        filter.is_excluded(".codex-home/.tmp/plugins/plugins/figma/SKILL.md"),
        ".codex-home scratch plugin cache should be excluded"
    );
    assert!(
        filter.is_excluded("docs/audit/2026-05-18-remediation-run/worktrees/IU-001/backend/app.py"),
        "generated remediation worktrees should be excluded"
    );
    assert!(
        filter.is_excluded(".pytest_cache/README.md"),
        "pytest cache should be excluded"
    );
    assert!(
        filter.is_excluded("backend/.ruff_cache/0.9.0/file.py"),
        "ruff cache should be excluded"
    );
    assert!(
        filter.is_excluded(".playwright-mcp/session/state.md"),
        "playwright mcp cache should be excluded"
    );
    assert!(
        filter.is_excluded(".agents/task-notes.md"),
        "agent scratch directories should be excluded"
    );
    assert!(
        filter.is_excluded("src/target/release/binary"),
        "target should be excluded"
    );
    assert!(
        !filter.is_excluded("src/main.ts"),
        "normal path should not be excluded"
    );
}

#[test]
fn test_is_excluded_dir() {
    let filter = SecurityFilter::new(&PathBuf::from("/nonexistent/workspace"));

    assert!(filter.is_excluded_dir("node_modules"));
    assert!(filter.is_excluded_dir(".git"));
    assert!(filter.is_excluded_dir(".claude"));
    assert!(filter.is_excluded_dir(".codex"));
    assert!(filter.is_excluded_dir(".codex-home"));
    assert!(filter.is_excluded_dir(".agents"));
    assert!(filter.is_excluded_dir(".playwright-mcp"));
    assert!(filter.is_excluded_dir(".pytest_cache"));
    assert!(filter.is_excluded_dir(".ruff_cache"));
    assert!(filter.is_excluded_dir("worktrees"));
    assert!(filter.is_excluded_dir("target"));
    assert!(filter.is_excluded_dir("__pycache__"));
    assert!(!filter.is_excluded_dir("src"));
    assert!(!filter.is_excluded_dir("lib"));
    assert!(filter.is_excluded_dir("vendor"));
    assert!(!filter.is_excluded_dir("helpers"));
}

#[test]
fn test_gitignore_integration() {
    // Create a temp dir with a .gitignore
    let tmp = std::env::temp_dir().join("lattice_test_gitignore");
    let _ = std::fs::create_dir_all(&tmp);
    std::fs::write(tmp.join(".gitignore"), "*.log\nbuild/\n").unwrap();

    let filter = SecurityFilter::new(&tmp);

    assert!(
        filter.is_excluded("app.log"),
        "*.log should be excluded via .gitignore"
    );
    assert!(
        filter.is_excluded("build/output.js"),
        "build/ should be excluded via .gitignore"
    );
    assert!(
        !filter.is_excluded("src/main.ts"),
        "normal file should not be excluded"
    );

    // Cleanup
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn nested_ignore_policy_matches_traversal_and_direct_reads() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::create_dir_all(dir.path().join("src/generated")).unwrap();
    std::fs::create_dir_all(dir.path().join("lib")).unwrap();
    std::fs::write(dir.path().join("src/.gitignore"), "hidden.rs\ngenerated/\n").unwrap();
    std::fs::write(dir.path().join("src/.latticeignore"), "custom.rs\n").unwrap();
    for name in ["src/hidden.rs", "src/custom.rs", "src/ok.rs", "lib/real.rs"] {
        std::fs::write(dir.path().join(name), "fn real() {}\n").unwrap();
    }
    std::fs::write(
        dir.path().join("src/generated/code.rs"),
        "fn generated() {}\n",
    )
    .unwrap();
    let paths = workspace::collect_sources(dir.path()).unwrap();
    assert_eq!(paths.len(), 2);
    assert!(workspace::read_source(dir.path(), Path::new("src/hidden.rs")).is_err());
    assert!(workspace::read_source(dir.path(), Path::new("src/custom.rs")).is_err());
    assert!(workspace::read_source(dir.path(), Path::new("src/generated/code.rs")).is_err());
    assert!(workspace::read_source(dir.path(), Path::new("lib/real.rs")).is_ok());
    assert!(workspace::read_source(dir.path(), Path::new("../outside.rs")).is_err());
    assert!(workspace::read_source(dir.path(), &dir.path().join("src/ok.rs")).is_err());
}

#[cfg(unix)]
#[test]
fn symlink_targets_and_cycles_are_never_source() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.rs"), "private").unwrap();
    symlink(outside.path(), dir.path().join("external")).unwrap();
    symlink(dir.path(), dir.path().join("cycle")).unwrap();
    symlink(
        outside.path().join("secret.rs"),
        dir.path().join("source.rs"),
    )
    .unwrap();
    assert!(workspace::collect_sources(dir.path()).unwrap().is_empty());
    for name in ["external/secret.rs", "source.rs", "cycle/source.rs"] {
        assert!(
            workspace::read_source(dir.path(), Path::new(name)).is_err(),
            "{name}"
        );
    }
}

#[cfg(unix)]
#[test]
fn symlinked_ignore_policy_is_a_coverage_error() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("src")).unwrap();
    std::fs::write(root.path().join("src/visible.rs"), "fn visible() {}\n").unwrap();
    std::fs::write(outside.path().join("rules"), "visible.rs\n").unwrap();
    symlink(
        outside.path().join("rules"),
        root.path().join("src/.gitignore"),
    )
    .unwrap();

    assert!(workspace::collect_sources(root.path()).is_err());
    assert!(workspace::read_source(root.path(), Path::new("src/visible.rs")).is_err());
}

#[cfg(unix)]
#[test]
fn component_safe_open_never_reads_renamed_external_symlink() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let source = root.path().join("source.rs");
    std::fs::write(&source, "safe").unwrap();
    let secret = outside.path().join("secret.rs");
    std::fs::write(&secret, "external-secret").unwrap();
    let root_path = root.path().to_path_buf();
    let secret_path = secret.clone();
    let mutator = std::thread::spawn(move || {
        for index in 0..300 {
            let candidate = root_path.join(format!("candidate-{index}"));
            if index % 2 == 0 {
                symlink(&secret_path, &candidate).unwrap();
            } else {
                std::fs::write(&candidate, "safe").unwrap();
            }
            std::fs::rename(candidate, root_path.join("source.rs")).unwrap();
        }
    });
    for _ in 0..600 {
        if let Ok(content) = workspace::read_source(root.path(), Path::new("source.rs")) {
            assert_eq!(content, "safe");
        }
    }
    mutator.join().unwrap();
}
