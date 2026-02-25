use super::parse_file;
use crate::symbols::SymbolKind;

#[test]
fn test_parse_typescript_function() {
    let source = r#"
export function greet(name: string): string {
    return `Hello, ${name}!`;
}
"#;

    let result = parse_file("test.ts", source).expect("Failed to parse");
    assert_eq!(result.language, crate::symbols::Language::TypeScript);
    assert!(!result.symbols.is_empty(), "Should extract at least one symbol");

    let func = result.symbols.iter().find(|s| s.name == "greet").expect("Should find 'greet'");
    assert_eq!(func.kind, SymbolKind::Function);
    assert!(func.is_exported, "greet should be exported");
    assert!(
        func.signature.contains("greet"),
        "Signature should contain 'greet': {}",
        func.signature
    );
    assert!(
        func.signature.contains("name: string"),
        "Signature should contain params: {}",
        func.signature
    );
}

#[test]
fn test_parse_typescript_class() {
    let source = r#"
export class UserService {
    private db: Database;

    constructor(db: Database) {
        this.db = db;
    }

    async getUser(id: string): Promise<User> {
        return this.db.find(id);
    }

    async saveUser(user: User): Promise<void> {
        await this.db.save(user);
    }
}
"#;

    let result = parse_file("service.ts", source).expect("Failed to parse");

    let class_sym = result
        .symbols
        .iter()
        .find(|s| s.kind == SymbolKind::Class)
        .expect("Should find a class symbol");
    assert_eq!(class_sym.name, "UserService");
    assert!(class_sym.is_exported);

    let methods: Vec<_> = result
        .symbols
        .iter()
        .filter(|s| s.kind == SymbolKind::Method)
        .collect();
    assert!(
        methods.len() >= 2,
        "Should find at least 2 methods, found {}",
        methods.len()
    );
}

#[test]
fn test_parse_typescript_interface() {
    let source = r#"
export interface Config {
    host: string;
    port: number;
    debug?: boolean;
}
"#;

    let result = parse_file("config.ts", source).expect("Failed to parse");

    let iface = result
        .symbols
        .iter()
        .find(|s| s.kind == SymbolKind::Interface)
        .expect("Should find an interface");
    assert_eq!(iface.name, "Config");
    assert!(iface.is_exported);
}

#[test]
fn test_parse_typescript_imports() {
    let source = r#"
import { readFile, writeFile } from "fs";
import express from "express";
import * as path from "path";
"#;

    let result = parse_file("app.ts", source).expect("Failed to parse");

    assert!(
        result.imports.len() >= 3,
        "Should find at least 3 imports, found {}",
        result.imports.len()
    );

    // Named import
    let fs_import = result
        .imports
        .iter()
        .find(|i| i.source == "fs")
        .expect("Should find fs import");
    assert!(
        fs_import.names.contains(&"readFile".to_string()),
        "Should contain readFile"
    );
    assert!(
        fs_import.names.contains(&"writeFile".to_string()),
        "Should contain writeFile"
    );
    assert!(!fs_import.is_default);
    assert!(!fs_import.is_wildcard);

    // Default import
    let express_import = result
        .imports
        .iter()
        .find(|i| i.source == "express")
        .expect("Should find express import");
    assert!(express_import.is_default);
    assert!(
        express_import.names.contains(&"express".to_string()),
        "Should contain 'express' as default name"
    );

    // Wildcard import
    let path_import = result
        .imports
        .iter()
        .find(|i| i.source == "path")
        .expect("Should find path import");
    assert!(path_import.is_wildcard);
    assert!(
        path_import.names.contains(&"path".to_string()),
        "Should contain 'path' as namespace name"
    );
}

#[test]
fn test_parse_typescript_type_alias_and_enum() {
    let source = r#"
export type UserId = string | number;

export enum Status {
    Active,
    Inactive,
    Pending,
}

export const MAX_RETRIES = 3;
"#;

    let result = parse_file("types.ts", source).expect("Failed to parse");

    let type_alias = result
        .symbols
        .iter()
        .find(|s| s.kind == SymbolKind::TypeAlias)
        .expect("Should find type alias");
    assert_eq!(type_alias.name, "UserId");
    assert!(type_alias.is_exported);

    let enum_sym = result
        .symbols
        .iter()
        .find(|s| s.kind == SymbolKind::Enum)
        .expect("Should find enum");
    assert_eq!(enum_sym.name, "Status");
    assert!(enum_sym.is_exported);

    let const_sym = result
        .symbols
        .iter()
        .find(|s| s.kind == SymbolKind::Constant)
        .expect("Should find constant");
    assert_eq!(const_sym.name, "MAX_RETRIES");
    assert!(const_sym.is_exported);
}
