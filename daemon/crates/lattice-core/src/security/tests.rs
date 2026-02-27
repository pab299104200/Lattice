use super::*;
use std::path::PathBuf;

#[test]
fn test_default_exclusions() {
    // Use a non-existent path so no .lattice_ignore file is loaded
    let filter = SecurityFilter::new(&PathBuf::from("/nonexistent/workspace"));

    assert!(filter.is_excluded(".env"), ".env should be excluded");
    assert!(filter.is_excluded("config/.env.production"), ".env.production should be excluded");
    assert!(filter.is_excluded("credentials.json"), "credentials.json should be excluded");
    assert!(filter.is_excluded("keys/id_rsa"), "id_rsa should be excluded");
    assert!(filter.is_excluded("certs/server.pem"), ".pem should be excluded");
    assert!(filter.is_excluded("certs/server.key"), ".key should be excluded");
}

#[test]
fn test_normal_files_not_excluded() {
    let filter = SecurityFilter::new(&PathBuf::from("/nonexistent/workspace"));

    assert!(!filter.is_excluded("src/auth.ts"), "auth.ts should not be excluded");
    assert!(!filter.is_excluded("helpers/utils.py"), "utils.py should not be excluded");
    assert!(!filter.is_excluded("README.md"), "README.md should not be excluded");
    assert!(!filter.is_excluded("src/index.js"), "index.js should not be excluded");
}

#[test]
fn test_redact_password() {
    let source = r#"const config = {
    host: "localhost",
    password=abc123
    port: 5432
};"#;

    let redacted = SecurityFilter::redact_content(source);
    assert!(redacted.contains("[REDACTED]"), "password line should be redacted");
    assert!(!redacted.contains("abc123"), "password value should not appear");
    assert!(redacted.contains("host"), "non-sensitive lines should remain");
    assert!(redacted.contains("port"), "non-sensitive lines should remain");
}

#[test]
fn test_redact_aws_key() {
    let source = r#"# Config
aws_key = "AKIAIOSFODNN7EXAMPLE"
region = "us-east-1"
"#;

    let redacted = SecurityFilter::redact_content(source);
    assert!(redacted.contains("[REDACTED]"), "AWS key line should be redacted");
    assert!(!redacted.contains("AKIAIOSFODNN7EXAMPLE"), "AWS key should not appear");
    assert!(redacted.contains("region"), "non-sensitive lines should remain");
}

#[test]
fn test_excluded_dirs() {
    let filter = SecurityFilter::new(&PathBuf::from("/nonexistent/workspace"));

    assert!(filter.is_excluded("node_modules/express/index.js"), "node_modules should be excluded");
    assert!(filter.is_excluded(".git/config"), ".git should be excluded");
    assert!(filter.is_excluded("src/target/release/binary"), "target should be excluded");
    assert!(!filter.is_excluded("src/main.ts"), "normal path should not be excluded");
}

#[test]
fn test_is_excluded_dir() {
    let filter = SecurityFilter::new(&PathBuf::from("/nonexistent/workspace"));

    assert!(filter.is_excluded_dir("node_modules"));
    assert!(filter.is_excluded_dir(".git"));
    assert!(filter.is_excluded_dir("target"));
    assert!(filter.is_excluded_dir("__pycache__"));
    assert!(!filter.is_excluded_dir("src"));
    assert!(filter.is_excluded_dir("lib"));
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

    assert!(filter.is_excluded("app.log"), "*.log should be excluded via .gitignore");
    assert!(filter.is_excluded("build/output.js"), "build/ should be excluded via .gitignore");
    assert!(!filter.is_excluded("src/main.ts"), "normal file should not be excluded");

    // Cleanup
    let _ = std::fs::remove_dir_all(&tmp);
}
