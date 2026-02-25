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
    assert!(!should_index_file("README.md"));
    assert!(!should_index_file("package.json"));
    assert!(!should_index_file("logo.png"));
    assert!(!should_index_file(".env"));
}

#[test]
fn test_should_not_index_excluded_dirs() {
    assert!(!should_index_file("node_modules/express/index.js"));
    assert!(!should_index_file(".git/hooks/pre-commit"));
    assert!(!should_index_file("target/debug/main.rs"));
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
