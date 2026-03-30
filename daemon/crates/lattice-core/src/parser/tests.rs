use super::parse_file;
use crate::symbols::{Language, SymbolKind};

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

#[test]
fn test_parse_markdown_document_and_sections() {
    let source = r#"
# Guide

Intro paragraph with `prepare_change`.

## Setup

See [[runbook#Checklist]] and [Overview](./overview.md).
"#;

    let result = parse_file("docs/guide.md", source).expect("Failed to parse");
    assert_eq!(result.language, Language::Markdown);

    let document = result
        .symbols
        .iter()
        .find(|s| s.kind == SymbolKind::Document)
        .expect("Should find a document symbol");
    assert_eq!(document.name, "Guide");

    let sections: Vec<_> = result
        .symbols
        .iter()
        .filter(|s| s.kind == SymbolKind::Section)
        .collect();
    assert_eq!(sections.len(), 2, "Should create one section per heading");
    assert!(
        sections
            .iter()
            .any(|section| section.references.contains(&"prepare_change".to_string())),
        "Markdown sections should capture inline code references"
    );

    assert_eq!(result.links.len(), 2, "Should extract both Markdown and wiki links");
    assert!(
        result.links.iter().any(|link| link.target == "runbook" && link.heading.as_deref() == Some("Checklist") && link.is_wiki),
        "Should extract wiki-links with section targets"
    );
    assert!(
        result.links.iter().any(|link| link.target == "./overview.md" && !link.is_wiki),
        "Should extract Markdown links"
    );
}

// ==================== Python Tests ====================

#[test]
fn test_parse_python_function() {
    let source = r#"
def greet(name: str) -> str:
    return f"Hello, {name}!"

def _private_helper(x):
    return x + 1
"#;

    let result = parse_file("test.py", source).expect("Failed to parse");
    assert_eq!(result.language, Language::Python);

    let greet = result
        .symbols
        .iter()
        .find(|s| s.name == "greet")
        .expect("Should find 'greet'");
    assert_eq!(greet.kind, SymbolKind::Function);
    assert!(greet.is_exported, "greet should be exported (no leading _)");
    assert!(
        greet.signature.contains("greet"),
        "Signature should contain 'greet': {}",
        greet.signature
    );
    assert!(
        greet.signature.contains("name: str"),
        "Signature should contain params: {}",
        greet.signature
    );
    // Signature should not end with ':'
    assert!(
        !greet.signature.ends_with(':'),
        "Signature should not end with colon: {}",
        greet.signature
    );

    let private = result
        .symbols
        .iter()
        .find(|s| s.name == "_private_helper")
        .expect("Should find '_private_helper'");
    assert!(
        !private.is_exported,
        "_private_helper should not be exported"
    );
}

#[test]
fn test_parse_python_class() {
    let source = r#"
class UserService:
    def __init__(self, db):
        self.db = db

    def get_user(self, user_id: str):
        return self.db.find(user_id)

    def save_user(self, user):
        self.db.save(user)
"#;

    let result = parse_file("service.py", source).expect("Failed to parse");

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
        methods.len() >= 3,
        "Should find at least 3 methods (__init__, get_user, save_user), found {}",
        methods.len()
    );

    // Check qualified method names
    let method_names: Vec<&str> = methods.iter().map(|m| m.name.as_str()).collect();
    assert!(
        method_names.contains(&"UserService.__init__"),
        "Should have UserService.__init__"
    );
    assert!(
        method_names.contains(&"UserService.get_user"),
        "Should have UserService.get_user"
    );
}

#[test]
fn test_parse_python_imports() {
    let source = r#"
import os
from pathlib import Path
from collections import OrderedDict, defaultdict
"#;

    let result = parse_file("app.py", source).expect("Failed to parse");

    assert!(
        result.imports.len() >= 3,
        "Should find at least 3 imports, found {}",
        result.imports.len()
    );

    // Plain import
    let os_import = result
        .imports
        .iter()
        .find(|i| i.source == "os")
        .expect("Should find os import");
    assert!(
        os_import.names.contains(&"os".to_string()),
        "Should contain 'os'"
    );

    // From import (single)
    let pathlib_import = result
        .imports
        .iter()
        .find(|i| i.source == "pathlib")
        .expect("Should find pathlib import");
    assert!(
        pathlib_import.names.contains(&"Path".to_string()),
        "Should contain 'Path'"
    );

    // From import (multiple)
    let collections_import = result
        .imports
        .iter()
        .find(|i| i.source == "collections")
        .expect("Should find collections import");
    assert!(
        collections_import.names.contains(&"OrderedDict".to_string()),
        "Should contain 'OrderedDict', got: {:?}",
        collections_import.names
    );
    assert!(
        collections_import.names.contains(&"defaultdict".to_string()),
        "Should contain 'defaultdict', got: {:?}",
        collections_import.names
    );
}

// ==================== Rust Tests ====================

#[test]
fn test_parse_rust_function_and_struct() {
    let source = r#"
pub fn process_data(input: &str) -> Result<Vec<u8>, Error> {
    let parsed = parse(input)?;
    Ok(parsed.into_bytes())
}

fn private_helper(x: i32) -> i32 {
    x + 1
}

pub struct Config {
    pub host: String,
    pub port: u16,
    debug: bool,
}

pub enum Status {
    Active,
    Inactive,
    Pending,
}

pub trait Processor {
    fn process(&self) -> Result<(), Error>;
}

impl Config {
    pub fn new(host: String) -> Self {
        Config { host, port: 8080, debug: false }
    }

    fn validate(&self) -> bool {
        !self.host.is_empty()
    }
}
"#;

    let result = parse_file("lib.rs", source).expect("Failed to parse Rust");
    assert_eq!(result.language, Language::Rust);

    // Function
    let process = result.symbols.iter().find(|s| s.name == "process_data")
        .expect("Should find 'process_data'");
    assert_eq!(process.kind, SymbolKind::Function);
    assert!(process.is_exported, "process_data should be pub");
    assert!(process.signature.contains("process_data"), "Sig: {}", process.signature);

    let helper = result.symbols.iter().find(|s| s.name == "private_helper")
        .expect("Should find 'private_helper'");
    assert!(!helper.is_exported, "private_helper should not be pub");

    // Struct
    let config = result.symbols.iter().find(|s| s.name == "Config" && s.kind == SymbolKind::Struct)
        .expect("Should find Config struct");
    assert!(config.is_exported);

    // Enum
    let status = result.symbols.iter().find(|s| s.name == "Status" && s.kind == SymbolKind::Enum)
        .expect("Should find Status enum");
    assert!(status.is_exported);

    // Trait
    let processor = result.symbols.iter().find(|s| s.name == "Processor" && s.kind == SymbolKind::Trait)
        .expect("Should find Processor trait");
    assert!(processor.is_exported);

    // Impl methods
    let new_method = result.symbols.iter().find(|s| s.name == "Config.new")
        .expect("Should find Config.new");
    assert_eq!(new_method.kind, SymbolKind::Method);
    assert!(new_method.is_exported);

    let validate = result.symbols.iter().find(|s| s.name == "Config.validate")
        .expect("Should find Config.validate");
    assert!(!validate.is_exported, "validate should not be pub");
}

// ==================== Go Tests ====================

#[test]
fn test_parse_go_function_and_struct() {
    let source = r#"
package main

import (
    "fmt"
    "strings"
)

func ProcessData(input string) (string, error) {
    result := strings.ToUpper(input)
    return result, nil
}

func privateHelper(x int) int {
    return x + 1
}

type Config struct {
    Host string
    Port int
}

type Processor interface {
    Process() error
}

func (c *Config) Validate() bool {
    return c.Host != ""
}
"#;

    let result = parse_file("main.go", source).expect("Failed to parse Go");
    assert_eq!(result.language, Language::Go);

    // Function
    let process = result.symbols.iter().find(|s| s.name == "ProcessData")
        .expect("Should find 'ProcessData'");
    assert_eq!(process.kind, SymbolKind::Function);
    assert!(process.is_exported, "ProcessData starts with uppercase");

    let helper = result.symbols.iter().find(|s| s.name == "privateHelper")
        .expect("Should find 'privateHelper'");
    assert!(!helper.is_exported, "privateHelper starts with lowercase");

    // Struct
    let config = result.symbols.iter().find(|s| s.name == "Config" && s.kind == SymbolKind::Struct)
        .expect("Should find Config struct");
    assert!(config.is_exported);

    // Interface
    let processor = result.symbols.iter().find(|s| s.name == "Processor" && s.kind == SymbolKind::Interface)
        .expect("Should find Processor interface");
    assert!(processor.is_exported);

    // Method
    let validate = result.symbols.iter().find(|s| s.name == "Config.Validate")
        .expect("Should find Config.Validate method");
    assert_eq!(validate.kind, SymbolKind::Method);
    assert!(validate.is_exported);

    // Imports
    assert!(result.imports.len() >= 2, "Should find at least 2 imports, found {}", result.imports.len());
}

// ==================== Java Tests ====================

#[test]
fn test_parse_java_class_and_methods() {
    let source = r#"
import java.util.List;
import java.util.ArrayList;

public class UserService {
    private String dbUrl;

    public UserService(String dbUrl) {
        this.dbUrl = dbUrl;
    }

    public List<User> getUsers() {
        return new ArrayList<>();
    }

    private void logAction(String action) {
        System.out.println(action);
    }
}

public interface Repository {
    void save(Object entity);
    Object find(String id);
}

public enum Status {
    ACTIVE,
    INACTIVE
}
"#;

    let result = parse_file("UserService.java", source).expect("Failed to parse Java");
    assert_eq!(result.language, Language::Java);

    // Class
    let class_sym = result.symbols.iter()
        .find(|s| s.name == "UserService" && s.kind == SymbolKind::Class)
        .expect("Should find UserService class");
    assert!(class_sym.is_exported);

    // Methods
    let get_users = result.symbols.iter().find(|s| s.name == "UserService.getUsers")
        .expect("Should find UserService.getUsers");
    assert_eq!(get_users.kind, SymbolKind::Method);
    assert!(get_users.is_exported, "getUsers should be public");

    let log_action = result.symbols.iter().find(|s| s.name == "UserService.logAction")
        .expect("Should find UserService.logAction");
    assert!(!log_action.is_exported, "logAction should not be public");

    // Interface
    let repo = result.symbols.iter()
        .find(|s| s.name == "Repository" && s.kind == SymbolKind::Interface)
        .expect("Should find Repository interface");
    assert!(repo.is_exported);

    // Enum
    let status = result.symbols.iter()
        .find(|s| s.name == "Status" && s.kind == SymbolKind::Enum)
        .expect("Should find Status enum");
    assert!(status.is_exported);

    // Imports
    assert!(result.imports.len() >= 2, "Should find at least 2 imports, found {}", result.imports.len());
}

#[test]
fn test_parse_python_sqlalchemy_model() {
    let source = r#"
from sqlalchemy import Column, Integer, String, ForeignKey
from sqlalchemy.orm import relationship

class Host(Base):
    __tablename__ = "hosts"
    id = Column(Integer, primary_key=True)
    name = Column(String(255))
    org_id = Column(Integer, ForeignKey("organizations.id"))

class HypervisorDiscovery(Base):
    __tablename__ = "hypervisor_discoveries"
    id = Column(Integer, primary_key=True)
    hypervisor_host_id = Column(Integer, ForeignKey("hosts.id"), nullable=False)
    hypervisor = relationship("Host", foreign_keys=[hypervisor_host_id])
    cloud_meta = relationship("CloudMetadata")
"#;

    let result = parse_file("models/discovery.py", source).expect("Failed to parse");

    // HypervisorDiscovery should have references to Host and CloudMetadata via relationship()
    let hyp = result.symbols.iter()
        .find(|s| s.name == "HypervisorDiscovery" && s.kind == SymbolKind::Class)
        .expect("Should find HypervisorDiscovery class");

    assert!(
        hyp.references.contains(&"Host".to_string()),
        "Should have Host reference from relationship(), got: {:?}",
        hyp.references
    );
    assert!(
        hyp.references.contains(&"CloudMetadata".to_string()),
        "Should have CloudMetadata reference from relationship(), got: {:?}",
        hyp.references
    );

    // Should also capture ForeignKey table refs
    assert!(
        hyp.references.contains(&"hosts".to_string()),
        "Should have hosts reference from ForeignKey(), got: {:?}",
        hyp.references
    );

    // Host class should have organizations ref from ForeignKey
    let host = result.symbols.iter()
        .find(|s| s.name == "Host" && s.kind == SymbolKind::Class)
        .expect("Should find Host class");
    assert!(
        host.references.contains(&"organizations".to_string()),
        "Should have organizations reference from ForeignKey(), got: {:?}",
        host.references
    );
}
