use crate::symbols::{Language, Symbol, SymbolId, SymbolKind};
use super::{diff_symbols, ChangeKind};

fn make_symbol(name: &str, file: &str, body: &str) -> Symbol {
    Symbol {
        id: SymbolId {
            file: file.to_string(),
            name: name.to_string(),
            byte_offset: 0,
        },
        kind: SymbolKind::Function,
        name: name.to_string(),
        signature: format!("function {}()", name),
        body: body.to_string(),
        file: file.to_string(),
        line: 1,
        end_line: 3,
        is_exported: true,
        language: Language::TypeScript,
        references: vec![],
        imports: vec![],
    }
}

#[test]
fn test_detect_added_function() {
    let old = vec![
        make_symbol("loginUser", "src/auth.ts", "function loginUser() { return true; }"),
    ];
    let new = vec![
        make_symbol("loginUser", "src/auth.ts", "function loginUser() { return true; }"),
        make_symbol("logoutUser", "src/auth.ts", "function logoutUser() { session.destroy(); }"),
    ];

    let changes = diff_symbols(&old, &new);
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].name, "logoutUser");
    assert_eq!(changes[0].kind, ChangeKind::Added);
}

#[test]
fn test_detect_removed_function() {
    let old = vec![
        make_symbol("loginUser", "src/auth.ts", "function loginUser() { return true; }"),
        make_symbol("logoutUser", "src/auth.ts", "function logoutUser() { session.destroy(); }"),
    ];
    let new = vec![
        make_symbol("loginUser", "src/auth.ts", "function loginUser() { return true; }"),
    ];

    let changes = diff_symbols(&old, &new);
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].name, "logoutUser");
    assert_eq!(changes[0].kind, ChangeKind::Removed);
}

#[test]
fn test_detect_modified_function() {
    let old = vec![
        make_symbol("hashPassword", "src/crypto.ts", "function hashPassword(p) { return md5(p); }"),
    ];
    let new = vec![
        make_symbol("hashPassword", "src/crypto.ts", "function hashPassword(p) { return bcrypt(p, 12); }"),
    ];

    let changes = diff_symbols(&old, &new);
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].name, "hashPassword");
    assert_eq!(changes[0].kind, ChangeKind::Modified);
}

#[test]
fn test_no_changes() {
    let symbols = vec![
        make_symbol("loginUser", "src/auth.ts", "function loginUser() { return true; }"),
        make_symbol("hashPassword", "src/crypto.ts", "function hashPassword(p) { return bcrypt(p); }"),
    ];

    let changes = diff_symbols(&symbols, &symbols);
    assert!(changes.is_empty());
}
