# Lattice Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Build Lattice — a local, AI-agent-aware code intelligence engine that reduces context tokens by 65-70% through dependency graph analysis and semantic search, delivered as a VS Code extension with a Rust sidecar daemon.

**Architecture:** Two-process system — a Rust daemon (indexer, query engine, memory, file watcher, MCP server) communicates via stdio JSON-RPC with a lightweight TypeScript VS Code extension (UI, daemon lifecycle). The daemon uses Tree-sitter for multi-language parsing, petgraph for in-memory dependency graphs, SQLite + sqlite-vec for persistence and vector search, and ONNX Runtime for local embeddings.

**Tech Stack:** Rust (tokio, tree-sitter, petgraph, rusqlite, sqlite-vec, ort), TypeScript (VS Code extension API), MCP protocol (stdio JSON-RPC), all-MiniLM-L6-v2 ONNX model.

**Design Doc:** `docs/plans/2026-02-25-lattice-design.md`

---

## Phase 1: Project Scaffolding

Goal: Create the Rust workspace and VS Code extension skeleton with CI pipeline. Both build and test successfully.

### Task 1.1: Initialize Rust Workspace

**Files:**
- Create: `daemon/Cargo.toml` (workspace root)
- Create: `daemon/crates/lattice-core/Cargo.toml`
- Create: `daemon/crates/lattice-core/src/lib.rs`
- Create: `daemon/crates/lattice-daemon/Cargo.toml`
- Create: `daemon/crates/lattice-daemon/src/main.rs`

**Step 1: Create the Rust workspace**

The workspace has two crates:
- `lattice-core` — library crate with all business logic (parsing, graph, query, memory)
- `lattice-daemon` — binary crate that runs the daemon (JSON-RPC server, MCP, CLI)

`daemon/Cargo.toml`:
```toml
[workspace]
resolver = "2"
members = [
    "crates/lattice-core",
    "crates/lattice-daemon",
]

[workspace.package]
version = "0.1.0"
edition = "2021"
license = "MIT"
rust-version = "1.75"

[workspace.dependencies]
lattice-core = { path = "crates/lattice-core" }
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
anyhow = "1"
thiserror = "2"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
```

`daemon/crates/lattice-core/Cargo.toml`:
```toml
[package]
name = "lattice-core"
version.workspace = true
edition.workspace = true

[dependencies]
serde = { workspace = true }
serde_json = { workspace = true }
anyhow = { workspace = true }
thiserror = { workspace = true }
tracing = { workspace = true }

[dev-dependencies]
tokio = { workspace = true }
```

`daemon/crates/lattice-core/src/lib.rs`:
```rust
//! Lattice core library — parsing, graph, query engine, memory.

pub mod error;

pub use error::LatticeError;
```

Create `daemon/crates/lattice-core/src/error.rs`:
```rust
use thiserror::Error;

#[derive(Error, Debug)]
pub enum LatticeError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Parse error in {file}: {message}")]
    Parse { file: String, message: String },

    #[error("Storage error: {0}")]
    Storage(String),

    #[error("Query error: {0}")]
    Query(String),
}
```

`daemon/crates/lattice-daemon/Cargo.toml`:
```toml
[package]
name = "lattice-daemon"
version.workspace = true
edition.workspace = true

[[bin]]
name = "lattice"
path = "src/main.rs"

[dependencies]
lattice-core = { workspace = true }
tokio = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
anyhow = { workspace = true }
tracing = { workspace = true }
tracing-subscriber = { workspace = true }
```

`daemon/crates/lattice-daemon/src/main.rs`:
```rust
use anyhow::Result;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    tracing::info!("Lattice daemon starting...");

    // TODO: Start JSON-RPC server on stdio
    println!("Lattice daemon v{}", env!("CARGO_PKG_VERSION"));

    Ok(())
}
```

**Step 2: Verify it builds**

Run: `cd daemon && cargo build`
Expected: Compiles with no errors

**Step 3: Verify tests pass (empty for now)**

Run: `cd daemon && cargo test`
Expected: 0 tests, all pass

**Step 4: Commit**

```bash
git add daemon/
git commit -m "feat: initialize Rust workspace with core and daemon crates"
```

---

### Task 1.2: Initialize VS Code Extension

**Files:**
- Create: `extension/package.json`
- Create: `extension/tsconfig.json`
- Create: `extension/src/extension.ts`
- Create: `extension/.vscodeignore`

**Step 1: Create package.json**

`extension/package.json`:
```json
{
  "name": "lattice",
  "displayName": "Lattice",
  "description": "Local AI context engine — dependency graph analysis for 65-70% fewer tokens",
  "version": "0.1.0",
  "publisher": "lattice",
  "engines": {
    "vscode": "^1.85.0"
  },
  "categories": ["Other"],
  "activationEvents": ["onStartupFinished"],
  "main": "./out/extension.js",
  "contributes": {
    "commands": [
      {
        "command": "lattice.reindex",
        "title": "Lattice: Re-index Workspace"
      },
      {
        "command": "lattice.showStatus",
        "title": "Lattice: Show Status"
      }
    ]
  },
  "scripts": {
    "vscode:prepublish": "npm run compile",
    "compile": "tsc -p ./",
    "watch": "tsc -watch -p ./",
    "lint": "eslint src --ext ts",
    "test": "node ./out/test/runTest.js"
  },
  "devDependencies": {
    "@types/node": "^20.0.0",
    "@types/vscode": "^1.85.0",
    "@typescript-eslint/eslint-plugin": "^7.0.0",
    "@typescript-eslint/parser": "^7.0.0",
    "eslint": "^8.0.0",
    "typescript": "^5.3.0"
  }
}
```

**Step 2: Create tsconfig.json**

`extension/tsconfig.json`:
```json
{
  "compilerOptions": {
    "module": "commonjs",
    "target": "ES2022",
    "outDir": "out",
    "rootDir": "src",
    "lib": ["ES2022"],
    "sourceMap": true,
    "strict": true,
    "esModuleInterop": true,
    "skipLibCheck": true,
    "forceConsistentCasingInFileNames": true
  },
  "exclude": ["node_modules", ".vscode-test"]
}
```

**Step 3: Create extension entry point**

`extension/src/extension.ts`:
```typescript
import * as vscode from 'vscode';

export function activate(context: vscode.ExtensionContext) {
    console.log('Lattice extension activating...');

    const reindexCmd = vscode.commands.registerCommand('lattice.reindex', () => {
        vscode.window.showInformationMessage('Lattice: Re-indexing workspace...');
    });

    const statusCmd = vscode.commands.registerCommand('lattice.showStatus', () => {
        vscode.window.showInformationMessage('Lattice: Daemon not yet connected');
    });

    context.subscriptions.push(reindexCmd, statusCmd);
}

export function deactivate() {
    console.log('Lattice extension deactivating...');
}
```

**Step 4: Create .vscodeignore**

`extension/.vscodeignore`:
```
.vscode/**
.vscode-test/**
src/**
!out/**
node_modules/**
.gitignore
tsconfig.json
```

**Step 5: Install dependencies and compile**

Run: `cd extension && npm install && npm run compile`
Expected: Compiles with no errors, `out/extension.js` created

**Step 6: Commit**

```bash
git add extension/
git commit -m "feat: initialize VS Code extension skeleton"
```

---

### Task 1.3: Create Root Project Files

**Files:**
- Create: `.gitignore`
- Create: `.lattice_ignore` (example)

**Step 1: Create .gitignore**

`.gitignore`:
```
# Rust
daemon/target/
**/*.rs.bk

# Node
extension/node_modules/
extension/out/
extension/*.vsix

# IDE
.vscode/settings.json
.idea/

# OS
.DS_Store
Thumbs.db

# Lattice data
.lattice/
*.lattice.db
*.lattice.db-wal
*.lattice.db-shm

# Secrets
.env
*.pem
*.key
```

**Step 2: Commit**

```bash
git add .gitignore
git commit -m "chore: add root .gitignore"
```

---

## Phase 2: Tree-sitter Parsing Core

Goal: Parse source files using Tree-sitter, extract symbols (functions, classes, interfaces, types, variables), and produce a structured symbol table. Start with TypeScript, then add Python and more.

### Task 2.1: Add Tree-sitter Dependencies

**Files:**
- Modify: `daemon/Cargo.toml` (workspace deps)
- Modify: `daemon/crates/lattice-core/Cargo.toml`

**Step 1: Add tree-sitter workspace dependencies**

Add to `daemon/Cargo.toml` under `[workspace.dependencies]`:
```toml
tree-sitter = "0.24"
tree-sitter-typescript = "0.23"
tree-sitter-python = "0.23"
tree-sitter-javascript = "0.23"
```

Add to `daemon/crates/lattice-core/Cargo.toml` under `[dependencies]`:
```toml
tree-sitter = { workspace = true }
tree-sitter-typescript = { workspace = true }
tree-sitter-python = { workspace = true }
tree-sitter-javascript = { workspace = true }
```

**Step 2: Verify it builds**

Run: `cd daemon && cargo build`
Expected: Compiles (tree-sitter C compilation may take a moment)

**Step 3: Commit**

```bash
git add daemon/Cargo.toml daemon/crates/lattice-core/Cargo.toml
git commit -m "chore: add tree-sitter dependencies for TS, Python, JS"
```

---

### Task 2.2: Define the Symbol Model

**Files:**
- Create: `daemon/crates/lattice-core/src/symbols.rs`
- Modify: `daemon/crates/lattice-core/src/lib.rs`

**Step 1: Write the symbol model**

`daemon/crates/lattice-core/src/symbols.rs`:
```rust
use serde::{Deserialize, Serialize};

/// Unique identifier for a symbol in the graph.
#[derive(Debug, Clone, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub struct SymbolId {
    /// File path relative to workspace root.
    pub file: String,
    /// Symbol name (e.g., "loginUser", "AuthService").
    pub name: String,
    /// Byte offset in the source file (for disambiguation).
    pub byte_offset: usize,
}

/// The kind of symbol extracted from source code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SymbolKind {
    Function,
    Class,
    Interface,
    TypeAlias,
    Enum,
    Module,
    Variable,
    Constant,
    Method,
    Trait,
    Struct,
}

/// A symbol extracted from a source file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Symbol {
    pub id: SymbolId,
    pub kind: SymbolKind,
    pub name: String,
    /// Full signature (e.g., "fn loginUser(creds: Credentials): Promise<Session>").
    pub signature: String,
    /// Full source code of the symbol body.
    pub body: String,
    /// File path relative to workspace root.
    pub file: String,
    /// 1-based line number where the symbol starts.
    pub line: usize,
    /// 1-based line number where the symbol ends.
    pub end_line: usize,
    /// Whether this symbol is exported/public.
    pub is_exported: bool,
    /// Language of the source file.
    pub language: Language,
    /// Symbols referenced within this symbol's body (unresolved names).
    pub references: Vec<String>,
    /// Import paths used by this symbol's file.
    pub imports: Vec<ImportInfo>,
}

/// Language identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Language {
    TypeScript,
    JavaScript,
    Python,
    Rust,
    Go,
    Java,
    Unknown,
}

impl Language {
    pub fn from_extension(ext: &str) -> Self {
        match ext {
            "ts" | "tsx" => Language::TypeScript,
            "js" | "jsx" | "mjs" | "cjs" => Language::JavaScript,
            "py" | "pyi" => Language::Python,
            "rs" => Language::Rust,
            "go" => Language::Go,
            "java" => Language::Java,
            _ => Language::Unknown,
        }
    }
}

/// An import statement extracted from a file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportInfo {
    /// The module path (e.g., "./auth/login", "express").
    pub source: String,
    /// Imported names (e.g., ["loginUser", "validateToken"]).
    pub names: Vec<String>,
    /// Whether this is a default import.
    pub is_default: bool,
    /// Whether this is a wildcard import (import *).
    pub is_wildcard: bool,
}

/// Result of parsing a single file.
#[derive(Debug, Clone)]
pub struct ParsedFile {
    pub file: String,
    pub language: Language,
    pub symbols: Vec<Symbol>,
    pub imports: Vec<ImportInfo>,
}
```

**Step 2: Register the module**

Add to `daemon/crates/lattice-core/src/lib.rs`:
```rust
pub mod symbols;
```

**Step 3: Verify it compiles**

Run: `cd daemon && cargo build`
Expected: Compiles with no errors

**Step 4: Commit**

```bash
git add daemon/crates/lattice-core/src/symbols.rs daemon/crates/lattice-core/src/lib.rs
git commit -m "feat: define Symbol model with kinds, imports, and parsed file structure"
```

---

### Task 2.3: Build the TypeScript Parser

**Files:**
- Create: `daemon/crates/lattice-core/src/parser/mod.rs`
- Create: `daemon/crates/lattice-core/src/parser/typescript.rs`
- Create: `daemon/crates/lattice-core/src/parser/tests.rs`
- Modify: `daemon/crates/lattice-core/src/lib.rs`

**Step 1: Write the failing test**

`daemon/crates/lattice-core/src/parser/tests.rs`:
```rust
#[cfg(test)]
mod tests {
    use crate::parser::parse_file;
    use crate::symbols::{Language, SymbolKind};

    #[test]
    fn test_parse_typescript_function() {
        let source = r#"
export function loginUser(username: string, password: string): Promise<User> {
    const user = await findUser(username);
    if (!user) throw new Error("not found");
    return user;
}
"#;
        let result = parse_file("src/auth.ts", source).unwrap();
        assert_eq!(result.symbols.len(), 1);
        assert_eq!(result.symbols[0].name, "loginUser");
        assert_eq!(result.symbols[0].kind, SymbolKind::Function);
        assert!(result.symbols[0].is_exported);
        assert_eq!(result.symbols[0].language, Language::TypeScript);
        assert!(result.symbols[0].signature.contains("loginUser"));
        assert!(result.symbols[0].signature.contains("Promise<User>"));
    }

    #[test]
    fn test_parse_typescript_class() {
        let source = r#"
export class AuthService {
    private secret: string;

    constructor(secret: string) {
        this.secret = secret;
    }

    validateToken(token: string): boolean {
        return verify(token, this.secret);
    }

    refreshToken(token: string): string {
        return sign(decode(token), this.secret);
    }
}
"#;
        let result = parse_file("src/auth-service.ts", source).unwrap();

        // Should find: class + 3 methods (constructor, validateToken, refreshToken)
        let class_symbols: Vec<_> = result.symbols.iter()
            .filter(|s| s.kind == SymbolKind::Class)
            .collect();
        assert_eq!(class_symbols.len(), 1);
        assert_eq!(class_symbols[0].name, "AuthService");

        let method_symbols: Vec<_> = result.symbols.iter()
            .filter(|s| s.kind == SymbolKind::Method)
            .collect();
        assert!(method_symbols.len() >= 2, "Expected at least 2 methods, got {}", method_symbols.len());
    }

    #[test]
    fn test_parse_typescript_interface() {
        let source = r#"
export interface Authenticator {
    login(credentials: Credentials): Promise<Session>;
    logout(sessionId: string): Promise<void>;
}
"#;
        let result = parse_file("src/types.ts", source).unwrap();
        let interfaces: Vec<_> = result.symbols.iter()
            .filter(|s| s.kind == SymbolKind::Interface)
            .collect();
        assert_eq!(interfaces.len(), 1);
        assert_eq!(interfaces[0].name, "Authenticator");
    }

    #[test]
    fn test_parse_typescript_imports() {
        let source = r#"
import { User, Session } from './models';
import express from 'express';
import * as jwt from 'jsonwebtoken';

export function handler() { return null; }
"#;
        let result = parse_file("src/handler.ts", source).unwrap();
        assert_eq!(result.imports.len(), 3);
        assert_eq!(result.imports[0].source, "./models");
        assert_eq!(result.imports[0].names, vec!["User", "Session"]);
        assert!(result.imports[1].is_default);
        assert!(result.imports[2].is_wildcard);
    }

    #[test]
    fn test_parse_typescript_type_alias_and_enum() {
        let source = r#"
export type Role = 'admin' | 'user' | 'guest';

export enum Status {
    Active,
    Inactive,
    Suspended,
}

export const MAX_RETRIES = 3;
"#;
        let result = parse_file("src/types.ts", source).unwrap();
        let type_alias: Vec<_> = result.symbols.iter()
            .filter(|s| s.kind == SymbolKind::TypeAlias)
            .collect();
        assert_eq!(type_alias.len(), 1);
        assert_eq!(type_alias[0].name, "Role");

        let enums: Vec<_> = result.symbols.iter()
            .filter(|s| s.kind == SymbolKind::Enum)
            .collect();
        assert_eq!(enums.len(), 1);
        assert_eq!(enums[0].name, "Status");

        let constants: Vec<_> = result.symbols.iter()
            .filter(|s| s.kind == SymbolKind::Constant)
            .collect();
        assert_eq!(constants.len(), 1);
        assert_eq!(constants[0].name, "MAX_RETRIES");
    }
}
```

**Step 2: Run test to verify it fails**

Run: `cd daemon && cargo test`
Expected: FAIL — `parser` module doesn't exist yet

**Step 3: Write the parser module**

`daemon/crates/lattice-core/src/parser/mod.rs`:
```rust
pub mod typescript;

#[cfg(test)]
mod tests;

use crate::error::LatticeError;
use crate::symbols::{Language, ParsedFile};

/// Parse a source file and extract symbols.
///
/// The language is detected from the file extension.
pub fn parse_file(file_path: &str, source: &str) -> Result<ParsedFile, LatticeError> {
    let ext = file_path.rsplit('.').next().unwrap_or("");
    let language = Language::from_extension(ext);

    match language {
        Language::TypeScript | Language::JavaScript => {
            typescript::parse(file_path, source, language)
        }
        _ => Err(LatticeError::Parse {
            file: file_path.to_string(),
            message: format!("Unsupported language: {:?}", language),
        }),
    }
}
```

**Step 4: Write the TypeScript parser implementation**

`daemon/crates/lattice-core/src/parser/typescript.rs`:
```rust
use crate::error::LatticeError;
use crate::symbols::*;
use tree_sitter::{Node, Parser};

pub fn parse(file_path: &str, source: &str, language: Language) -> Result<ParsedFile, LatticeError> {
    let mut parser = Parser::new();
    let ts_language = match language {
        Language::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Language::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        _ => unreachable!(),
    };
    parser.set_language(&ts_language).map_err(|e| LatticeError::Parse {
        file: file_path.to_string(),
        message: format!("Failed to set language: {}", e),
    })?;

    let tree = parser.parse(source, None).ok_or_else(|| LatticeError::Parse {
        file: file_path.to_string(),
        message: "Failed to parse file".to_string(),
    })?;

    let root = tree.root_node();
    let source_bytes = source.as_bytes();

    let mut symbols = Vec::new();
    let mut imports = Vec::new();

    extract_from_node(root, file_path, source_bytes, language, &mut symbols, &mut imports, false);

    Ok(ParsedFile {
        file: file_path.to_string(),
        language,
        symbols,
        imports,
    })
}

fn extract_from_node(
    node: Node,
    file_path: &str,
    source: &[u8],
    language: Language,
    symbols: &mut Vec<Symbol>,
    imports: &mut Vec<ImportInfo>,
    parent_exported: bool,
) {
    let mut cursor = node.walk();

    for child in node.children(&mut cursor) {
        match child.kind() {
            "export_statement" => {
                // Recurse into the export's declaration child
                let mut inner = child.walk();
                for export_child in child.children(&mut inner) {
                    extract_declaration(export_child, file_path, source, language, symbols, true);
                }
            }
            "function_declaration" | "function_signature" => {
                extract_function(child, file_path, source, language, symbols, parent_exported);
            }
            "class_declaration" => {
                extract_class(child, file_path, source, language, symbols, parent_exported);
            }
            "interface_declaration" => {
                extract_interface(child, file_path, source, language, symbols, parent_exported);
            }
            "type_alias_declaration" => {
                extract_type_alias(child, file_path, source, language, symbols, parent_exported);
            }
            "enum_declaration" => {
                extract_enum(child, file_path, source, language, symbols, parent_exported);
            }
            "lexical_declaration" => {
                extract_variable(child, file_path, source, language, symbols, parent_exported);
            }
            "import_statement" => {
                extract_import(child, source, imports);
            }
            _ => {}
        }
    }
}

fn extract_declaration(
    node: Node,
    file_path: &str,
    source: &[u8],
    language: Language,
    symbols: &mut Vec<Symbol>,
    is_exported: bool,
) {
    match node.kind() {
        "function_declaration" | "function_signature" => {
            extract_function(node, file_path, source, language, symbols, is_exported);
        }
        "class_declaration" => {
            extract_class(node, file_path, source, language, symbols, is_exported);
        }
        "interface_declaration" => {
            extract_interface(node, file_path, source, language, symbols, is_exported);
        }
        "type_alias_declaration" => {
            extract_type_alias(node, file_path, source, language, symbols, is_exported);
        }
        "enum_declaration" => {
            extract_enum(node, file_path, source, language, symbols, is_exported);
        }
        "lexical_declaration" => {
            extract_variable(node, file_path, source, language, symbols, is_exported);
        }
        _ => {}
    }
}

fn node_text<'a>(node: Node, source: &'a [u8]) -> &'a str {
    node.utf8_text(source).unwrap_or("")
}

fn extract_function(
    node: Node,
    file_path: &str,
    source: &[u8],
    language: Language,
    symbols: &mut Vec<Symbol>,
    is_exported: bool,
) {
    let name = node.child_by_field_name("name")
        .map(|n| node_text(n, source).to_string())
        .unwrap_or_default();
    if name.is_empty() { return; }

    let body_text = node_text(node, source).to_string();

    // Build signature: everything before the body block
    let signature = build_function_signature(node, source);

    let references = extract_references_from_body(node, source);

    symbols.push(Symbol {
        id: SymbolId {
            file: file_path.to_string(),
            name: name.clone(),
            byte_offset: node.start_byte(),
        },
        kind: SymbolKind::Function,
        name,
        signature,
        body: body_text,
        file: file_path.to_string(),
        line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
        is_exported,
        language,
        references,
        imports: vec![],
    });
}

fn build_function_signature(node: Node, source: &[u8]) -> String {
    // Collect text up to the body block '{'
    let full = node_text(node, source);
    if let Some(brace_pos) = full.find('{') {
        full[..brace_pos].trim().to_string()
    } else {
        full.trim().to_string()
    }
}

fn extract_class(
    node: Node,
    file_path: &str,
    source: &[u8],
    language: Language,
    symbols: &mut Vec<Symbol>,
    is_exported: bool,
) {
    let name = node.child_by_field_name("name")
        .map(|n| node_text(n, source).to_string())
        .unwrap_or_default();
    if name.is_empty() { return; }

    let body_text = node_text(node, source).to_string();
    let signature = format!("class {}", name);

    symbols.push(Symbol {
        id: SymbolId {
            file: file_path.to_string(),
            name: name.clone(),
            byte_offset: node.start_byte(),
        },
        kind: SymbolKind::Class,
        name: name.clone(),
        signature,
        body: body_text,
        file: file_path.to_string(),
        line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
        is_exported,
        language,
        references: vec![],
        imports: vec![],
    });

    // Extract methods from class body
    if let Some(body) = node.child_by_field_name("body") {
        let mut cursor = body.walk();
        for child in body.children(&mut cursor) {
            if child.kind() == "method_definition" || child.kind() == "public_field_definition" {
                extract_method(child, file_path, source, language, symbols, is_exported);
            }
        }
    }
}

fn extract_method(
    node: Node,
    file_path: &str,
    source: &[u8],
    language: Language,
    symbols: &mut Vec<Symbol>,
    is_exported: bool,
) {
    let name = node.child_by_field_name("name")
        .map(|n| node_text(n, source).to_string())
        .unwrap_or_default();
    if name.is_empty() { return; }

    let body_text = node_text(node, source).to_string();
    let signature = build_function_signature(node, source);
    let references = extract_references_from_body(node, source);

    symbols.push(Symbol {
        id: SymbolId {
            file: file_path.to_string(),
            name: name.clone(),
            byte_offset: node.start_byte(),
        },
        kind: SymbolKind::Method,
        name,
        signature,
        body: body_text,
        file: file_path.to_string(),
        line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
        is_exported,
        language,
        references,
        imports: vec![],
    });
}

fn extract_interface(
    node: Node,
    file_path: &str,
    source: &[u8],
    language: Language,
    symbols: &mut Vec<Symbol>,
    is_exported: bool,
) {
    let name = node.child_by_field_name("name")
        .map(|n| node_text(n, source).to_string())
        .unwrap_or_default();
    if name.is_empty() { return; }

    symbols.push(Symbol {
        id: SymbolId {
            file: file_path.to_string(),
            name: name.clone(),
            byte_offset: node.start_byte(),
        },
        kind: SymbolKind::Interface,
        name: name.clone(),
        signature: format!("interface {}", name),
        body: node_text(node, source).to_string(),
        file: file_path.to_string(),
        line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
        is_exported,
        language,
        references: vec![],
        imports: vec![],
    });
}

fn extract_type_alias(
    node: Node,
    file_path: &str,
    source: &[u8],
    language: Language,
    symbols: &mut Vec<Symbol>,
    is_exported: bool,
) {
    let name = node.child_by_field_name("name")
        .map(|n| node_text(n, source).to_string())
        .unwrap_or_default();
    if name.is_empty() { return; }

    symbols.push(Symbol {
        id: SymbolId {
            file: file_path.to_string(),
            name: name.clone(),
            byte_offset: node.start_byte(),
        },
        kind: SymbolKind::TypeAlias,
        name: name.clone(),
        signature: node_text(node, source).to_string(),
        body: node_text(node, source).to_string(),
        file: file_path.to_string(),
        line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
        is_exported,
        language,
        references: vec![],
        imports: vec![],
    });
}

fn extract_enum(
    node: Node,
    file_path: &str,
    source: &[u8],
    language: Language,
    symbols: &mut Vec<Symbol>,
    is_exported: bool,
) {
    let name = node.child_by_field_name("name")
        .map(|n| node_text(n, source).to_string())
        .unwrap_or_default();
    if name.is_empty() { return; }

    symbols.push(Symbol {
        id: SymbolId {
            file: file_path.to_string(),
            name: name.clone(),
            byte_offset: node.start_byte(),
        },
        kind: SymbolKind::Enum,
        name: name.clone(),
        signature: format!("enum {}", name),
        body: node_text(node, source).to_string(),
        file: file_path.to_string(),
        line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
        is_exported,
        language,
        references: vec![],
        imports: vec![],
    });
}

fn extract_variable(
    node: Node,
    file_path: &str,
    source: &[u8],
    language: Language,
    symbols: &mut Vec<Symbol>,
    is_exported: bool,
) {
    let full_text = node_text(node, source);
    let is_const = full_text.starts_with("const");

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "variable_declarator" {
            let name = child.child_by_field_name("name")
                .map(|n| node_text(n, source).to_string())
                .unwrap_or_default();
            if name.is_empty() { continue; }

            symbols.push(Symbol {
                id: SymbolId {
                    file: file_path.to_string(),
                    name: name.clone(),
                    byte_offset: child.start_byte(),
                },
                kind: if is_const { SymbolKind::Constant } else { SymbolKind::Variable },
                name,
                signature: full_text.to_string(),
                body: full_text.to_string(),
                file: file_path.to_string(),
                line: node.start_position().row + 1,
                end_line: node.end_position().row + 1,
                is_exported,
                language,
                references: vec![],
                imports: vec![],
            });
        }
    }
}

fn extract_import(node: Node, source: &[u8], imports: &mut Vec<ImportInfo>) {
    let source_node = node.child_by_field_name("source")
        .or_else(|| {
            // Walk children to find a string node
            let mut c = node.walk();
            node.children(&mut c).find(|n| n.kind() == "string")
        });

    let source_path = source_node
        .map(|n| {
            let text = node_text(n, source);
            text.trim_matches(|c| c == '\'' || c == '"').to_string()
        })
        .unwrap_or_default();

    if source_path.is_empty() { return; }

    let mut names = Vec::new();
    let mut is_default = false;
    let mut is_wildcard = false;

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "import_clause" => {
                let mut inner = child.walk();
                for clause_child in child.children(&mut inner) {
                    match clause_child.kind() {
                        "identifier" => {
                            is_default = true;
                            names.push(node_text(clause_child, source).to_string());
                        }
                        "named_imports" => {
                            let mut imports_cursor = clause_child.walk();
                            for import_spec in clause_child.children(&mut imports_cursor) {
                                if import_spec.kind() == "import_specifier" {
                                    let name = import_spec.child_by_field_name("name")
                                        .map(|n| node_text(n, source).to_string())
                                        .unwrap_or_default();
                                    if !name.is_empty() {
                                        names.push(name);
                                    }
                                }
                            }
                        }
                        "namespace_import" => {
                            is_wildcard = true;
                            // Get the alias: import * as X
                            if let Some(alias) = clause_child.child(2) {
                                names.push(node_text(alias, source).to_string());
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    imports.push(ImportInfo {
        source: source_path,
        names,
        is_default,
        is_wildcard,
    });
}

fn extract_references_from_body(node: Node, source: &[u8]) -> Vec<String> {
    let mut refs = Vec::new();
    collect_identifiers(node, source, &mut refs);
    refs.sort();
    refs.dedup();
    refs
}

fn collect_identifiers(node: Node, source: &[u8], refs: &mut Vec<String>) {
    if node.kind() == "call_expression" {
        if let Some(func) = node.child_by_field_name("function") {
            let name = node_text(func, source).to_string();
            // Only collect simple identifiers and member expressions
            if !name.is_empty() && !name.contains(' ') {
                refs.push(name);
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_identifiers(child, source, refs);
    }
}
```

**Step 5: Register the parser module in lib.rs**

Add to `daemon/crates/lattice-core/src/lib.rs`:
```rust
pub mod parser;
```

**Step 6: Run tests to verify they pass**

Run: `cd daemon && cargo test`
Expected: All 5 tests pass

**Step 7: Commit**

```bash
git add daemon/crates/lattice-core/src/parser/
git commit -m "feat: TypeScript/JavaScript parser with symbol extraction via Tree-sitter"
```

---

### Task 2.4: Add Python Parser

**Files:**
- Create: `daemon/crates/lattice-core/src/parser/python.rs`
- Modify: `daemon/crates/lattice-core/src/parser/mod.rs`
- Modify: `daemon/crates/lattice-core/src/parser/tests.rs`

**Step 1: Write the failing test**

Append to `tests.rs`:
```rust
#[test]
fn test_parse_python_function() {
    let source = r#"
def login_user(username: str, password: str) -> User:
    """Authenticate a user and return their profile."""
    user = find_user(username)
    if not user:
        raise ValueError("User not found")
    if not verify_password(password, user.password_hash):
        raise AuthError("Invalid password")
    return user
"#;
    let result = parse_file("src/auth.py", source).unwrap();
    assert_eq!(result.symbols.len(), 1);
    assert_eq!(result.symbols[0].name, "login_user");
    assert_eq!(result.symbols[0].kind, SymbolKind::Function);
    assert_eq!(result.symbols[0].language, Language::Python);
}

#[test]
fn test_parse_python_class() {
    let source = r#"
class AuthService:
    def __init__(self, secret: str):
        self.secret = secret

    def validate_token(self, token: str) -> bool:
        return verify(token, self.secret)
"#;
    let result = parse_file("src/auth_service.py", source).unwrap();
    let classes: Vec<_> = result.symbols.iter()
        .filter(|s| s.kind == SymbolKind::Class)
        .collect();
    assert_eq!(classes.len(), 1);
    assert_eq!(classes[0].name, "AuthService");

    let methods: Vec<_> = result.symbols.iter()
        .filter(|s| s.kind == SymbolKind::Method)
        .collect();
    assert!(methods.len() >= 2);
}

#[test]
fn test_parse_python_imports() {
    let source = r#"
from models import User, Session
import os
from pathlib import Path

def handler():
    pass
"#;
    let result = parse_file("src/handler.py", source).unwrap();
    assert!(result.imports.len() >= 2);
}
```

**Step 2: Run tests — expect failure**

Run: `cd daemon && cargo test`
Expected: FAIL — Python not supported yet in the match arm

**Step 3: Write the Python parser**

`daemon/crates/lattice-core/src/parser/python.rs`:
```rust
use crate::error::LatticeError;
use crate::symbols::*;
use tree_sitter::{Node, Parser};

pub fn parse(file_path: &str, source: &str) -> Result<ParsedFile, LatticeError> {
    let mut parser = Parser::new();
    let py_language = tree_sitter_python::LANGUAGE.into();
    parser.set_language(&py_language).map_err(|e| LatticeError::Parse {
        file: file_path.to_string(),
        message: format!("Failed to set Python language: {}", e),
    })?;

    let tree = parser.parse(source, None).ok_or_else(|| LatticeError::Parse {
        file: file_path.to_string(),
        message: "Failed to parse Python file".to_string(),
    })?;

    let root = tree.root_node();
    let source_bytes = source.as_bytes();

    let mut symbols = Vec::new();
    let mut imports = Vec::new();

    extract_from_node(root, file_path, source_bytes, &mut symbols, &mut imports);

    Ok(ParsedFile {
        file: file_path.to_string(),
        language: Language::Python,
        symbols,
        imports,
    })
}

fn node_text<'a>(node: Node, source: &'a [u8]) -> &'a str {
    node.utf8_text(source).unwrap_or("")
}

fn extract_from_node(
    node: Node,
    file_path: &str,
    source: &[u8],
    symbols: &mut Vec<Symbol>,
    imports: &mut Vec<ImportInfo>,
) {
    let mut cursor = node.walk();

    for child in node.children(&mut cursor) {
        match child.kind() {
            "function_definition" => {
                extract_function(child, file_path, source, symbols);
            }
            "class_definition" => {
                extract_class(child, file_path, source, symbols);
            }
            "import_statement" | "import_from_statement" => {
                extract_import(child, source, imports);
            }
            "decorated_definition" => {
                // Recurse into the decorated definition's actual definition
                let mut inner = child.walk();
                for inner_child in child.children(&mut inner) {
                    match inner_child.kind() {
                        "function_definition" => {
                            extract_function(inner_child, file_path, source, symbols);
                        }
                        "class_definition" => {
                            extract_class(inner_child, file_path, source, symbols);
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
}

fn extract_function(
    node: Node,
    file_path: &str,
    source: &[u8],
    symbols: &mut Vec<Symbol>,
) {
    let name = node.child_by_field_name("name")
        .map(|n| node_text(n, source).to_string())
        .unwrap_or_default();
    if name.is_empty() { return; }

    let full_text = node_text(node, source).to_string();

    // Build signature from def line
    let signature = full_text.lines().next().unwrap_or("").trim_end_matches(':').to_string();

    let is_exported = !name.starts_with('_');

    symbols.push(Symbol {
        id: SymbolId {
            file: file_path.to_string(),
            name: name.clone(),
            byte_offset: node.start_byte(),
        },
        kind: SymbolKind::Function,
        name,
        signature,
        body: full_text,
        file: file_path.to_string(),
        line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
        is_exported,
        language: Language::Python,
        references: vec![],
        imports: vec![],
    });
}

fn extract_class(
    node: Node,
    file_path: &str,
    source: &[u8],
    symbols: &mut Vec<Symbol>,
) {
    let name = node.child_by_field_name("name")
        .map(|n| node_text(n, source).to_string())
        .unwrap_or_default();
    if name.is_empty() { return; }

    let is_exported = !name.starts_with('_');

    symbols.push(Symbol {
        id: SymbolId {
            file: file_path.to_string(),
            name: name.clone(),
            byte_offset: node.start_byte(),
        },
        kind: SymbolKind::Class,
        name: name.clone(),
        signature: format!("class {}", name),
        body: node_text(node, source).to_string(),
        file: file_path.to_string(),
        line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
        is_exported,
        language: Language::Python,
        references: vec![],
        imports: vec![],
    });

    // Extract methods from class body
    if let Some(body) = node.child_by_field_name("body") {
        let mut cursor = body.walk();
        for child in body.children(&mut cursor) {
            if child.kind() == "function_definition" {
                let method_name = child.child_by_field_name("name")
                    .map(|n| node_text(n, source).to_string())
                    .unwrap_or_default();
                if method_name.is_empty() { continue; }

                let method_text = node_text(child, source).to_string();
                let method_sig = method_text.lines().next()
                    .unwrap_or("").trim_end_matches(':').to_string();

                symbols.push(Symbol {
                    id: SymbolId {
                        file: file_path.to_string(),
                        name: method_name.clone(),
                        byte_offset: child.start_byte(),
                    },
                    kind: SymbolKind::Method,
                    name: method_name,
                    signature: method_sig,
                    body: method_text,
                    file: file_path.to_string(),
                    line: child.start_position().row + 1,
                    end_line: child.end_position().row + 1,
                    is_exported,
                    language: Language::Python,
                    references: vec![],
                    imports: vec![],
                });
            }
        }
    }
}

fn extract_import(node: Node, source: &[u8], imports: &mut Vec<ImportInfo>) {
    match node.kind() {
        "import_statement" => {
            // import os, import os.path
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "dotted_name" {
                    let name = node_text(child, source).to_string();
                    imports.push(ImportInfo {
                        source: name.clone(),
                        names: vec![name],
                        is_default: false,
                        is_wildcard: false,
                    });
                }
            }
        }
        "import_from_statement" => {
            // from X import Y, Z
            let module = node.child_by_field_name("module_name")
                .map(|n| node_text(n, source).to_string())
                .unwrap_or_default();

            let mut names = Vec::new();
            let mut is_wildcard = false;

            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                match child.kind() {
                    "dotted_name" | "identifier" => {
                        // Skip the module name itself (first dotted_name)
                        let text = node_text(child, source).to_string();
                        if text != module && !text.is_empty() {
                            names.push(text);
                        }
                    }
                    "wildcard_import" => {
                        is_wildcard = true;
                    }
                    "import_prefix" => {} // the "from" keyword
                    _ => {}
                }
            }

            if !module.is_empty() {
                imports.push(ImportInfo {
                    source: module,
                    names,
                    is_default: false,
                    is_wildcard,
                });
            }
        }
        _ => {}
    }
}
```

**Step 4: Update parser/mod.rs to include Python**

Add to `mod.rs`:
```rust
pub mod python;
```

And update the `parse_file` function:
```rust
Language::Python => python::parse(file_path, source),
```

**Step 5: Run tests**

Run: `cd daemon && cargo test`
Expected: All 8 tests pass

**Step 6: Commit**

```bash
git add daemon/crates/lattice-core/src/parser/
git commit -m "feat: add Python parser with function, class, method, and import extraction"
```

---

## Phase 3: Dependency Graph

Goal: Build an in-memory dependency graph using petgraph. Populate it from parsed symbols. Support traversal queries (dependents, dependencies, N-hop neighbors).

### Task 3.1: Add petgraph Dependency

**Files:**
- Modify: `daemon/Cargo.toml` (workspace deps)
- Modify: `daemon/crates/lattice-core/Cargo.toml`

**Step 1: Add petgraph to workspace**

Add to `daemon/Cargo.toml` under `[workspace.dependencies]`:
```toml
petgraph = "0.6"
```

Add to `daemon/crates/lattice-core/Cargo.toml` under `[dependencies]`:
```toml
petgraph = { workspace = true }
```

**Step 2: Verify it builds**

Run: `cd daemon && cargo build`
Expected: Compiles

**Step 3: Commit**

```bash
git add daemon/Cargo.toml daemon/crates/lattice-core/Cargo.toml
git commit -m "chore: add petgraph dependency"
```

---

### Task 3.2: Define Graph Model

**Files:**
- Create: `daemon/crates/lattice-core/src/graph/mod.rs`
- Create: `daemon/crates/lattice-core/src/graph/model.rs`
- Create: `daemon/crates/lattice-core/src/graph/tests.rs`
- Modify: `daemon/crates/lattice-core/src/lib.rs`

**Step 1: Write the failing test**

`daemon/crates/lattice-core/src/graph/tests.rs`:
```rust
#[cfg(test)]
mod tests {
    use crate::graph::{CodeGraph, EdgeKind};
    use crate::symbols::{Language, SymbolId, SymbolKind};

    fn make_id(file: &str, name: &str) -> SymbolId {
        SymbolId {
            file: file.to_string(),
            name: name.to_string(),
            byte_offset: 0,
        }
    }

    #[test]
    fn test_add_and_retrieve_node() {
        let mut graph = CodeGraph::new();
        let id = make_id("src/auth.ts", "loginUser");
        graph.add_node(
            id.clone(),
            SymbolKind::Function,
            "loginUser".to_string(),
            "fn loginUser(creds: Credentials): Promise<Session>".to_string(),
            "async function loginUser(creds) { ... }".to_string(),
            "src/auth.ts".to_string(),
            42,
            55,
            true,
            Language::TypeScript,
        );

        let node = graph.get_node(&id);
        assert!(node.is_some());
        assert_eq!(node.unwrap().name, "loginUser");
    }

    #[test]
    fn test_add_edge_and_get_dependents() {
        let mut graph = CodeGraph::new();
        let login_id = make_id("src/auth.ts", "loginUser");
        let hash_id = make_id("src/crypto.ts", "hashPassword");

        graph.add_node(login_id.clone(), SymbolKind::Function, "loginUser".into(),
            "".into(), "".into(), "src/auth.ts".into(), 1, 10, true, Language::TypeScript);
        graph.add_node(hash_id.clone(), SymbolKind::Function, "hashPassword".into(),
            "".into(), "".into(), "src/crypto.ts".into(), 1, 10, true, Language::TypeScript);

        graph.add_edge(&login_id, &hash_id, EdgeKind::Calls);

        let deps = graph.get_dependencies(&login_id);
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].0.name, "hashPassword");

        let dependents = graph.get_dependents(&hash_id);
        assert_eq!(dependents.len(), 1);
        assert_eq!(dependents[0].0.name, "loginUser");
    }

    #[test]
    fn test_n_hop_neighbors() {
        let mut graph = CodeGraph::new();
        let a = make_id("a.ts", "a");
        let b = make_id("b.ts", "b");
        let c = make_id("c.ts", "c");
        let d = make_id("d.ts", "d");

        for (id, name) in [(&a, "a"), (&b, "b"), (&c, "c"), (&d, "d")] {
            graph.add_node(id.clone(), SymbolKind::Function, name.into(),
                "".into(), "".into(), format!("{}.ts", name), 1, 1, true, Language::TypeScript);
        }

        graph.add_edge(&a, &b, EdgeKind::Calls);
        graph.add_edge(&b, &c, EdgeKind::Calls);
        graph.add_edge(&c, &d, EdgeKind::Calls);

        // 1 hop from a: just b
        let neighbors_1 = graph.n_hop_neighbors(&a, 1);
        assert_eq!(neighbors_1.len(), 1);

        // 2 hops from a: b and c
        let neighbors_2 = graph.n_hop_neighbors(&a, 2);
        assert_eq!(neighbors_2.len(), 2);

        // 3 hops from a: b, c, and d
        let neighbors_3 = graph.n_hop_neighbors(&a, 3);
        assert_eq!(neighbors_3.len(), 3);
    }

    #[test]
    fn test_remove_file_nodes() {
        let mut graph = CodeGraph::new();
        let fn1 = make_id("src/auth.ts", "login");
        let fn2 = make_id("src/auth.ts", "logout");
        let fn3 = make_id("src/other.ts", "helper");

        for (id, name) in [(&fn1, "login"), (&fn2, "logout"), (&fn3, "helper")] {
            graph.add_node(id.clone(), SymbolKind::Function, name.into(),
                "".into(), "".into(), id.file.clone(), 1, 1, true, Language::TypeScript);
        }

        graph.add_edge(&fn1, &fn3, EdgeKind::Calls);
        assert_eq!(graph.node_count(), 3);

        graph.remove_file_nodes("src/auth.ts");
        assert_eq!(graph.node_count(), 1);
        assert!(graph.get_node(&fn3).is_some());
    }

    #[test]
    fn test_graph_stats() {
        let mut graph = CodeGraph::new();
        let a = make_id("a.ts", "a");
        let b = make_id("b.ts", "b");

        graph.add_node(a.clone(), SymbolKind::Function, "a".into(),
            "".into(), "".into(), "a.ts".into(), 1, 1, true, Language::TypeScript);
        graph.add_node(b.clone(), SymbolKind::Function, "b".into(),
            "".into(), "".into(), "b.ts".into(), 1, 1, true, Language::TypeScript);
        graph.add_edge(&a, &b, EdgeKind::Calls);

        let stats = graph.stats();
        assert_eq!(stats.node_count, 2);
        assert_eq!(stats.edge_count, 1);
        assert_eq!(stats.file_count, 2);
    }
}
```

**Step 2: Run test to verify it fails**

Run: `cd daemon && cargo test`
Expected: FAIL — `graph` module doesn't exist

**Step 3: Write the graph model**

`daemon/crates/lattice-core/src/graph/model.rs`:
```rust
use std::collections::{HashMap, HashSet, VecDeque};
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::Direction;
use serde::{Deserialize, Serialize};

use crate::symbols::{Language, SymbolId, SymbolKind};

/// The kind of relationship between two symbols.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EdgeKind {
    Calls,
    Imports,
    Implements,
    Extends,
    TypeRef,
    Contains,
    CoChanges,
}

/// Data stored at each node in the dependency graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphNode {
    pub id: SymbolId,
    pub kind: SymbolKind,
    pub name: String,
    pub signature: String,
    pub body: String,
    pub file: String,
    pub line: usize,
    pub end_line: usize,
    pub is_exported: bool,
    pub language: Language,
    /// Edit frequency — how often this node has been modified.
    pub edit_count: u32,
    /// Timestamp of last modification (Unix seconds).
    pub last_modified: u64,
}

/// Statistics about the graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphStats {
    pub node_count: usize,
    pub edge_count: usize,
    pub file_count: usize,
}

/// The in-memory dependency graph.
pub struct CodeGraph {
    graph: DiGraph<GraphNode, EdgeKind>,
    /// Fast lookup from SymbolId to NodeIndex.
    index: HashMap<SymbolId, NodeIndex>,
}

impl CodeGraph {
    pub fn new() -> Self {
        Self {
            graph: DiGraph::new(),
            index: HashMap::new(),
        }
    }

    pub fn add_node(
        &mut self,
        id: SymbolId,
        kind: SymbolKind,
        name: String,
        signature: String,
        body: String,
        file: String,
        line: usize,
        end_line: usize,
        is_exported: bool,
        language: Language,
    ) -> NodeIndex {
        if let Some(&idx) = self.index.get(&id) {
            // Update existing node
            let node = &mut self.graph[idx];
            node.kind = kind;
            node.name = name;
            node.signature = signature;
            node.body = body;
            node.file = file;
            node.line = line;
            node.end_line = end_line;
            node.is_exported = is_exported;
            node.language = language;
            return idx;
        }

        let node = GraphNode {
            id: id.clone(),
            kind,
            name,
            signature,
            body,
            file,
            line,
            end_line,
            is_exported,
            language,
            edit_count: 0,
            last_modified: 0,
        };

        let idx = self.graph.add_node(node);
        self.index.insert(id, idx);
        idx
    }

    pub fn add_edge(&mut self, from: &SymbolId, to: &SymbolId, kind: EdgeKind) {
        if let (Some(&from_idx), Some(&to_idx)) = (self.index.get(from), self.index.get(to)) {
            // Avoid duplicate edges
            let exists = self.graph.edges_connecting(from_idx, to_idx)
                .any(|e| *e.weight() == kind);
            if !exists {
                self.graph.add_edge(from_idx, to_idx, kind);
            }
        }
    }

    pub fn get_node(&self, id: &SymbolId) -> Option<&GraphNode> {
        self.index.get(id).map(|&idx| &self.graph[idx])
    }

    /// Get all nodes that `id` depends on (outgoing edges).
    pub fn get_dependencies(&self, id: &SymbolId) -> Vec<(&GraphNode, EdgeKind)> {
        let Some(&idx) = self.index.get(id) else { return vec![] };
        self.graph.edges_directed(idx, Direction::Outgoing)
            .map(|e| (&self.graph[e.target()], *e.weight()))
            .collect()
    }

    /// Get all nodes that depend on `id` (incoming edges).
    pub fn get_dependents(&self, id: &SymbolId) -> Vec<(&GraphNode, EdgeKind)> {
        let Some(&idx) = self.index.get(id) else { return vec![] };
        self.graph.edges_directed(idx, Direction::Incoming)
            .map(|e| (&self.graph[e.source()], *e.weight()))
            .collect()
    }

    /// BFS traversal: get all nodes within N hops (both directions).
    pub fn n_hop_neighbors(&self, id: &SymbolId, hops: usize) -> Vec<&GraphNode> {
        let Some(&start_idx) = self.index.get(id) else { return vec![] };

        let mut visited = HashSet::new();
        let mut queue = VecDeque::new();
        visited.insert(start_idx);
        queue.push_back((start_idx, 0));

        let mut result = Vec::new();

        while let Some((idx, depth)) = queue.pop_front() {
            if depth > 0 {
                result.push(&self.graph[idx]);
            }
            if depth < hops {
                // Traverse both directions
                for neighbor in self.graph.neighbors_directed(idx, Direction::Outgoing) {
                    if visited.insert(neighbor) {
                        queue.push_back((neighbor, depth + 1));
                    }
                }
                for neighbor in self.graph.neighbors_directed(idx, Direction::Incoming) {
                    if visited.insert(neighbor) {
                        queue.push_back((neighbor, depth + 1));
                    }
                }
            }
        }

        result
    }

    /// Remove all nodes belonging to a specific file. Also removes connected edges.
    pub fn remove_file_nodes(&mut self, file: &str) {
        let to_remove: Vec<_> = self.index.iter()
            .filter(|(id, _)| id.file == file)
            .map(|(id, &idx)| (id.clone(), idx))
            .collect();

        for (id, idx) in to_remove {
            self.graph.remove_node(idx);
            self.index.remove(&id);
        }

        // Rebuild index since petgraph may reuse NodeIndex values after removal
        self.rebuild_index();
    }

    fn rebuild_index(&mut self) {
        self.index.clear();
        for idx in self.graph.node_indices() {
            let id = self.graph[idx].id.clone();
            self.index.insert(id, idx);
        }
    }

    pub fn node_count(&self) -> usize {
        self.graph.node_count()
    }

    pub fn edge_count(&self) -> usize {
        self.graph.edge_count()
    }

    pub fn stats(&self) -> GraphStats {
        let files: HashSet<_> = self.graph.node_indices()
            .map(|idx| self.graph[idx].file.as_str())
            .collect();

        GraphStats {
            node_count: self.graph.node_count(),
            edge_count: self.graph.edge_count(),
            file_count: files.len(),
        }
    }

    /// Get all node IDs in the graph.
    pub fn all_node_ids(&self) -> Vec<&SymbolId> {
        self.index.keys().collect()
    }

    /// Get all nodes in the graph.
    pub fn all_nodes(&self) -> Vec<&GraphNode> {
        self.graph.node_indices()
            .map(|idx| &self.graph[idx])
            .collect()
    }

    /// Calculate the degree centrality of a node (in-degree + out-degree).
    pub fn centrality(&self, id: &SymbolId) -> f64 {
        let Some(&idx) = self.index.get(id) else { return 0.0 };
        let total_nodes = self.graph.node_count() as f64;
        if total_nodes <= 1.0 { return 0.0; }
        let degree = self.graph.edges_directed(idx, Direction::Incoming).count()
            + self.graph.edges_directed(idx, Direction::Outgoing).count();
        degree as f64 / (total_nodes - 1.0)
    }
}

impl Default for CodeGraph {
    fn default() -> Self {
        Self::new()
    }
}
```

`daemon/crates/lattice-core/src/graph/mod.rs`:
```rust
pub mod model;

#[cfg(test)]
mod tests;

pub use model::{CodeGraph, EdgeKind, GraphNode, GraphStats};
```

**Step 4: Register the graph module**

Add to `daemon/crates/lattice-core/src/lib.rs`:
```rust
pub mod graph;
```

**Step 5: Run tests**

Run: `cd daemon && cargo test`
Expected: All tests pass (parser tests + graph tests)

**Step 6: Commit**

```bash
git add daemon/crates/lattice-core/src/graph/
git commit -m "feat: in-memory dependency graph with petgraph — nodes, edges, traversal, centrality"
```

---

### Task 3.3: Build Graph from Parsed Symbols

**Files:**
- Create: `daemon/crates/lattice-core/src/graph/builder.rs`
- Modify: `daemon/crates/lattice-core/src/graph/mod.rs`
- Modify: `daemon/crates/lattice-core/src/graph/tests.rs`

**Step 1: Write the failing test**

Append to `daemon/crates/lattice-core/src/graph/tests.rs`:
```rust
#[test]
fn test_build_graph_from_parsed_files() {
    use crate::parser::parse_file;
    use crate::graph::builder::GraphBuilder;

    let auth_source = r#"
import { hashPassword } from './crypto';

export function loginUser(username: string, password: string): Promise<User> {
    const hashed = hashPassword(password);
    return authenticate(username, hashed);
}
"#;

    let crypto_source = r#"
export function hashPassword(plain: string): string {
    return bcrypt.hash(plain, 10);
}
"#;

    let auth_parsed = parse_file("src/auth.ts", auth_source).unwrap();
    let crypto_parsed = parse_file("src/crypto.ts", crypto_source).unwrap();

    let mut builder = GraphBuilder::new();
    builder.add_file(auth_parsed);
    builder.add_file(crypto_parsed);
    let graph = builder.build();

    // Should have nodes for loginUser and hashPassword
    assert!(graph.node_count() >= 2);

    // loginUser should have an import edge to crypto.ts
    let login_id = SymbolId {
        file: "src/auth.ts".to_string(),
        name: "loginUser".to_string(),
        byte_offset: 0, // builder normalizes this
    };
    // The graph should contain edges from the import resolution
    assert!(graph.stats().edge_count > 0);
}
```

**Step 2: Run test — expect failure**

Run: `cd daemon && cargo test`
Expected: FAIL — `builder` module doesn't exist

**Step 3: Write the graph builder**

`daemon/crates/lattice-core/src/graph/builder.rs`:
```rust
use std::collections::HashMap;

use crate::symbols::{ParsedFile, Symbol, SymbolKind};
use super::{CodeGraph, EdgeKind};

/// Builds a CodeGraph from parsed files.
///
/// Two-pass approach:
/// 1. Add all symbols as nodes
/// 2. Resolve references and add edges
pub struct GraphBuilder {
    files: Vec<ParsedFile>,
}

impl GraphBuilder {
    pub fn new() -> Self {
        Self { files: Vec::new() }
    }

    pub fn add_file(&mut self, file: ParsedFile) {
        self.files.push(file);
    }

    pub fn build(self) -> CodeGraph {
        let mut graph = CodeGraph::new();

        // Pass 1: Add all symbols as nodes
        // Build a lookup of name -> SymbolId for cross-reference resolution
        let mut name_lookup: HashMap<String, Vec<crate::symbols::SymbolId>> = HashMap::new();

        for file in &self.files {
            for symbol in &file.symbols {
                let idx = graph.add_node(
                    symbol.id.clone(),
                    symbol.kind,
                    symbol.name.clone(),
                    symbol.signature.clone(),
                    symbol.body.clone(),
                    symbol.file.clone(),
                    symbol.line,
                    symbol.end_line,
                    symbol.is_exported,
                    symbol.language,
                );
                let _ = idx; // We use the SymbolId-based API

                name_lookup.entry(symbol.name.clone())
                    .or_default()
                    .push(symbol.id.clone());
            }
        }

        // Pass 2: Resolve edges
        for file in &self.files {
            // Module-level import edges
            for import in &file.imports {
                let target_file = resolve_import_path(&file.file, &import.source);

                for imported_name in &import.names {
                    // Find the imported symbol by name in the target file
                    if let Some(candidates) = name_lookup.get(imported_name) {
                        for candidate in candidates {
                            // Prefer symbols from the resolved target file
                            if candidate.file == target_file || target_file.is_empty() {
                                // Find any symbol in this file that references the imported name
                                for symbol in &file.symbols {
                                    if symbol.references.contains(imported_name)
                                        || symbol.body.contains(imported_name)
                                    {
                                        graph.add_edge(&symbol.id, candidate, EdgeKind::Calls);
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Contains edges (class -> method)
            let class_symbols: Vec<_> = file.symbols.iter()
                .filter(|s| s.kind == SymbolKind::Class)
                .collect();

            for class_sym in &class_symbols {
                for symbol in &file.symbols {
                    if symbol.kind == SymbolKind::Method
                        && symbol.file == class_sym.file
                        && symbol.line > class_sym.line
                        && symbol.end_line <= class_sym.end_line
                    {
                        graph.add_edge(&class_sym.id, &symbol.id, EdgeKind::Contains);
                    }
                }
            }
        }

        graph
    }
}

/// Resolve an import path relative to the importing file.
/// e.g., "./crypto" from "src/auth.ts" -> "src/crypto.ts" (approximate)
fn resolve_import_path(from_file: &str, import_source: &str) -> String {
    if !import_source.starts_with('.') {
        // External package import — can't resolve to a local file
        return String::new();
    }

    let from_dir = from_file.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    let clean = import_source.trim_start_matches("./");

    if from_dir.is_empty() {
        clean.to_string()
    } else {
        format!("{}/{}", from_dir, clean)
    }
    // Note: we don't append extensions here — matching is done by name, not exact path
}

impl Default for GraphBuilder {
    fn default() -> Self {
        Self::new()
    }
}
```

**Step 4: Register the builder module**

Add to `daemon/crates/lattice-core/src/graph/mod.rs`:
```rust
pub mod builder;
```

**Step 5: Run tests**

Run: `cd daemon && cargo test`
Expected: All tests pass

**Step 6: Commit**

```bash
git add daemon/crates/lattice-core/src/graph/
git commit -m "feat: graph builder — populates dependency graph from parsed files with edge resolution"
```

---

## Phase 4: SQLite Persistence

Goal: Persist the dependency graph and metadata to SQLite. Load it back on startup. Use sqlite-vec for vector storage (embedding vectors added in Phase 5).

### Task 4.1: Add SQLite Dependencies

**Files:**
- Modify: `daemon/Cargo.toml`
- Modify: `daemon/crates/lattice-core/Cargo.toml`

**Step 1: Add rusqlite to workspace**

Add to `daemon/Cargo.toml` under `[workspace.dependencies]`:
```toml
rusqlite = { version = "0.32", features = ["bundled"] }
```

Add to `daemon/crates/lattice-core/Cargo.toml` under `[dependencies]`:
```toml
rusqlite = { workspace = true }
```

**Step 2: Verify it builds**

Run: `cd daemon && cargo build`
Expected: Compiles (SQLite C compilation may take a moment)

**Step 3: Commit**

```bash
git add daemon/Cargo.toml daemon/crates/lattice-core/Cargo.toml
git commit -m "chore: add rusqlite with bundled SQLite"
```

---

### Task 4.2: Define Database Schema and Storage Module

**Files:**
- Create: `daemon/crates/lattice-core/src/storage/mod.rs`
- Create: `daemon/crates/lattice-core/src/storage/schema.rs`
- Create: `daemon/crates/lattice-core/src/storage/graph_store.rs`
- Create: `daemon/crates/lattice-core/src/storage/tests.rs`
- Modify: `daemon/crates/lattice-core/src/lib.rs`

**Step 1: Write the failing test**

`daemon/crates/lattice-core/src/storage/tests.rs`:
```rust
#[cfg(test)]
mod tests {
    use crate::graph::{CodeGraph, EdgeKind};
    use crate::storage::GraphStore;
    use crate::symbols::{Language, SymbolId, SymbolKind};

    fn make_test_graph() -> CodeGraph {
        let mut graph = CodeGraph::new();
        let login = SymbolId {
            file: "src/auth.ts".into(),
            name: "loginUser".into(),
            byte_offset: 0,
        };
        let hash = SymbolId {
            file: "src/crypto.ts".into(),
            name: "hashPassword".into(),
            byte_offset: 0,
        };

        graph.add_node(login.clone(), SymbolKind::Function, "loginUser".into(),
            "fn loginUser(creds: Credentials): Promise<Session>".into(),
            "async function loginUser(creds) { ... }".into(),
            "src/auth.ts".into(), 42, 55, true, Language::TypeScript);
        graph.add_node(hash.clone(), SymbolKind::Function, "hashPassword".into(),
            "fn hashPassword(plain: string): string".into(),
            "function hashPassword(plain) { ... }".into(),
            "src/crypto.ts".into(), 10, 15, true, Language::TypeScript);

        graph.add_edge(&login, &hash, EdgeKind::Calls);
        graph
    }

    #[test]
    fn test_save_and_load_graph() {
        let store = GraphStore::open_in_memory().unwrap();
        store.initialize().unwrap();

        let original = make_test_graph();
        store.save_graph(&original).unwrap();

        let loaded = store.load_graph().unwrap();
        assert_eq!(loaded.node_count(), 2);
        assert_eq!(loaded.edge_count(), 1);

        let login_id = SymbolId {
            file: "src/auth.ts".into(),
            name: "loginUser".into(),
            byte_offset: 0,
        };
        let node = loaded.get_node(&login_id);
        assert!(node.is_some());
        assert_eq!(node.unwrap().signature, "fn loginUser(creds: Credentials): Promise<Session>");
    }

    #[test]
    fn test_save_replaces_previous() {
        let store = GraphStore::open_in_memory().unwrap();
        store.initialize().unwrap();

        let graph1 = make_test_graph();
        store.save_graph(&graph1).unwrap();

        // Save a different graph
        let mut graph2 = CodeGraph::new();
        let id = SymbolId { file: "x.ts".into(), name: "x".into(), byte_offset: 0 };
        graph2.add_node(id, SymbolKind::Function, "x".into(), "".into(),
            "".into(), "x.ts".into(), 1, 1, true, Language::TypeScript);
        store.save_graph(&graph2).unwrap();

        let loaded = store.load_graph().unwrap();
        assert_eq!(loaded.node_count(), 1);
    }

    #[test]
    fn test_file_based_store() {
        let dir = std::env::temp_dir().join("lattice_test_db");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("test.lattice.db");

        {
            let store = GraphStore::open(db_path.to_str().unwrap()).unwrap();
            store.initialize().unwrap();
            let graph = make_test_graph();
            store.save_graph(&graph).unwrap();
        }

        {
            let store = GraphStore::open(db_path.to_str().unwrap()).unwrap();
            let loaded = store.load_graph().unwrap();
            assert_eq!(loaded.node_count(), 2);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
```

**Step 2: Run test — expect failure**

Run: `cd daemon && cargo test`
Expected: FAIL — `storage` module doesn't exist

**Step 3: Write the schema**

`daemon/crates/lattice-core/src/storage/schema.rs`:
```rust
pub const CREATE_TABLES: &str = r#"
CREATE TABLE IF NOT EXISTS nodes (
    file TEXT NOT NULL,
    name TEXT NOT NULL,
    byte_offset INTEGER NOT NULL,
    kind TEXT NOT NULL,
    signature TEXT NOT NULL,
    body TEXT NOT NULL,
    line INTEGER NOT NULL,
    end_line INTEGER NOT NULL,
    is_exported INTEGER NOT NULL,
    language TEXT NOT NULL,
    edit_count INTEGER NOT NULL DEFAULT 0,
    last_modified INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (file, name, byte_offset)
);

CREATE TABLE IF NOT EXISTS edges (
    from_file TEXT NOT NULL,
    from_name TEXT NOT NULL,
    from_offset INTEGER NOT NULL,
    to_file TEXT NOT NULL,
    to_name TEXT NOT NULL,
    to_offset INTEGER NOT NULL,
    kind TEXT NOT NULL,
    FOREIGN KEY (from_file, from_name, from_offset) REFERENCES nodes(file, name, byte_offset),
    FOREIGN KEY (to_file, to_name, to_offset) REFERENCES nodes(file, name, byte_offset)
);

CREATE INDEX IF NOT EXISTS idx_nodes_file ON nodes(file);
CREATE INDEX IF NOT EXISTS idx_edges_from ON edges(from_file, from_name, from_offset);
CREATE INDEX IF NOT EXISTS idx_edges_to ON edges(to_file, to_name, to_offset);
"#;
```

**Step 4: Write the graph store**

`daemon/crates/lattice-core/src/storage/graph_store.rs`:
```rust
use anyhow::Result;
use rusqlite::{params, Connection};

use crate::graph::{CodeGraph, EdgeKind};
use crate::symbols::{Language, SymbolId, SymbolKind};
use super::schema;

pub struct GraphStore {
    conn: Connection,
}

impl GraphStore {
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
        Ok(Self { conn })
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Ok(Self { conn })
    }

    pub fn initialize(&self) -> Result<()> {
        self.conn.execute_batch(schema::CREATE_TABLES)?;
        Ok(())
    }

    pub fn save_graph(&self, graph: &CodeGraph) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;

        // Clear existing data
        tx.execute_batch("DELETE FROM edges; DELETE FROM nodes;")?;

        // Insert nodes
        let mut insert_node = tx.prepare(
            "INSERT INTO nodes (file, name, byte_offset, kind, signature, body, line, end_line, is_exported, language, edit_count, last_modified)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)"
        )?;

        for node in graph.all_nodes() {
            insert_node.execute(params![
                node.id.file,
                node.id.name,
                node.id.byte_offset,
                format!("{:?}", node.kind),
                node.signature,
                node.body,
                node.line,
                node.end_line,
                node.is_exported as i32,
                format!("{:?}", node.language),
                node.edit_count,
                node.last_modified,
            ])?;
        }

        // Insert edges
        let mut insert_edge = tx.prepare(
            "INSERT INTO edges (from_file, from_name, from_offset, to_file, to_name, to_offset, kind)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"
        )?;

        for node in graph.all_nodes() {
            for (dep, edge_kind) in graph.get_dependencies(&node.id) {
                insert_edge.execute(params![
                    node.id.file,
                    node.id.name,
                    node.id.byte_offset,
                    dep.id.file,
                    dep.id.name,
                    dep.id.byte_offset,
                    format!("{:?}", edge_kind),
                ])?;
            }
        }

        drop(insert_node);
        drop(insert_edge);
        tx.commit()?;
        Ok(())
    }

    pub fn load_graph(&self) -> Result<CodeGraph> {
        let mut graph = CodeGraph::new();

        // Load nodes
        let mut stmt = self.conn.prepare(
            "SELECT file, name, byte_offset, kind, signature, body, line, end_line, is_exported, language, edit_count, last_modified FROM nodes"
        )?;

        let nodes: Vec<_> = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,  // file
                row.get::<_, String>(1)?,  // name
                row.get::<_, usize>(2)?,   // byte_offset
                row.get::<_, String>(3)?,  // kind
                row.get::<_, String>(4)?,  // signature
                row.get::<_, String>(5)?,  // body
                row.get::<_, usize>(6)?,   // line
                row.get::<_, usize>(7)?,   // end_line
                row.get::<_, bool>(8)?,    // is_exported
                row.get::<_, String>(9)?,  // language
                row.get::<_, u32>(10)?,    // edit_count
                row.get::<_, u64>(11)?,    // last_modified
            ))
        })?.collect::<Result<Vec<_>, _>>()?;

        for (file, name, byte_offset, kind_str, signature, body, line, end_line, is_exported, lang_str, edit_count, last_modified) in &nodes {
            let id = SymbolId {
                file: file.clone(),
                name: name.clone(),
                byte_offset: *byte_offset,
            };
            let kind = parse_symbol_kind(kind_str);
            let language = parse_language(lang_str);

            graph.add_node(id, kind, name.clone(), signature.clone(), body.clone(),
                file.clone(), *line, *end_line, *is_exported, language);
        }

        // Load edges
        let mut stmt = self.conn.prepare(
            "SELECT from_file, from_name, from_offset, to_file, to_name, to_offset, kind FROM edges"
        )?;

        let edges: Vec<_> = stmt.query_map([], |row| {
            Ok((
                SymbolId { file: row.get(0)?, name: row.get(1)?, byte_offset: row.get(2)? },
                SymbolId { file: row.get(3)?, name: row.get(4)?, byte_offset: row.get(5)? },
                row.get::<_, String>(6)?,
            ))
        })?.collect::<Result<Vec<_>, _>>()?;

        for (from_id, to_id, kind_str) in edges {
            let kind = parse_edge_kind(&kind_str);
            graph.add_edge(&from_id, &to_id, kind);
        }

        Ok(graph)
    }
}

fn parse_symbol_kind(s: &str) -> SymbolKind {
    match s {
        "Function" => SymbolKind::Function,
        "Class" => SymbolKind::Class,
        "Interface" => SymbolKind::Interface,
        "TypeAlias" => SymbolKind::TypeAlias,
        "Enum" => SymbolKind::Enum,
        "Module" => SymbolKind::Module,
        "Variable" => SymbolKind::Variable,
        "Constant" => SymbolKind::Constant,
        "Method" => SymbolKind::Method,
        "Trait" => SymbolKind::Trait,
        "Struct" => SymbolKind::Struct,
        _ => SymbolKind::Function, // fallback
    }
}

fn parse_language(s: &str) -> Language {
    match s {
        "TypeScript" => Language::TypeScript,
        "JavaScript" => Language::JavaScript,
        "Python" => Language::Python,
        "Rust" => Language::Rust,
        "Go" => Language::Go,
        "Java" => Language::Java,
        _ => Language::Unknown,
    }
}

fn parse_edge_kind(s: &str) -> EdgeKind {
    match s {
        "Calls" => EdgeKind::Calls,
        "Imports" => EdgeKind::Imports,
        "Implements" => EdgeKind::Implements,
        "Extends" => EdgeKind::Extends,
        "TypeRef" => EdgeKind::TypeRef,
        "Contains" => EdgeKind::Contains,
        "CoChanges" => EdgeKind::CoChanges,
        _ => EdgeKind::Calls, // fallback
    }
}
```

`daemon/crates/lattice-core/src/storage/mod.rs`:
```rust
pub mod schema;
pub mod graph_store;

#[cfg(test)]
mod tests;

pub use graph_store::GraphStore;
```

**Step 5: Register the storage module**

Add to `daemon/crates/lattice-core/src/lib.rs`:
```rust
pub mod storage;
```

**Step 6: Run tests**

Run: `cd daemon && cargo test`
Expected: All tests pass

**Step 7: Commit**

```bash
git add daemon/crates/lattice-core/src/storage/
git commit -m "feat: SQLite persistence — save and load dependency graph with WAL mode"
```

---

## Phase 5: Embeddings and Semantic Search

Goal: Integrate ONNX Runtime for local embedding inference (all-MiniLM-L6-v2). Store vectors in sqlite-vec. Support semantic similarity search over graph nodes.

### Task 5.1: Add ONNX and sqlite-vec Dependencies

**Files:**
- Modify: `daemon/Cargo.toml`
- Modify: `daemon/crates/lattice-core/Cargo.toml`

**Step 1: Add dependencies to workspace**

Add to `daemon/Cargo.toml` under `[workspace.dependencies]`:
```toml
ort = { version = "2", features = ["load-dynamic"] }
ndarray = "0.16"
tokenizers = "0.20"
```

Add to `daemon/crates/lattice-core/Cargo.toml` under `[dependencies]`:
```toml
ort = { workspace = true }
ndarray = { workspace = true }
tokenizers = { workspace = true }
```

Note: sqlite-vec will be loaded as a runtime SQLite extension via rusqlite. No separate crate needed — we'll use `conn.load_extension()`.

**Step 2: Verify it builds**

Run: `cd daemon && cargo build`
Expected: Compiles

**Step 3: Commit**

```bash
git add daemon/Cargo.toml daemon/crates/lattice-core/Cargo.toml
git commit -m "chore: add ONNX Runtime, ndarray, and tokenizers dependencies"
```

---

### Task 5.2: Build the Embedding Engine

**Files:**
- Create: `daemon/crates/lattice-core/src/embeddings/mod.rs`
- Create: `daemon/crates/lattice-core/src/embeddings/engine.rs`
- Create: `daemon/crates/lattice-core/src/embeddings/tests.rs`
- Modify: `daemon/crates/lattice-core/src/lib.rs`

**Step 1: Write the failing test**

`daemon/crates/lattice-core/src/embeddings/tests.rs`:
```rust
#[cfg(test)]
mod tests {
    use crate::embeddings::EmbeddingEngine;

    // Note: These tests require the ONNX model file to be present.
    // In CI, the model is downloaded as a build step.
    // For local dev, run: scripts/download-model.sh

    #[test]
    #[ignore] // Requires ONNX model file — run with: cargo test -- --ignored
    fn test_embed_single_text() {
        let engine = EmbeddingEngine::new("models/all-MiniLM-L6-v2.onnx").unwrap();
        let embedding = engine.embed("function loginUser authenticates a user").unwrap();
        assert_eq!(embedding.len(), 384); // MiniLM outputs 384-dim vectors
        // Vector should be normalized (L2 norm ≈ 1.0)
        let norm: f32 = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 0.1);
    }

    #[test]
    #[ignore]
    fn test_embed_batch() {
        let engine = EmbeddingEngine::new("models/all-MiniLM-L6-v2.onnx").unwrap();
        let texts = vec![
            "authentication login user",
            "database query SQL",
            "HTTP request handler",
        ];
        let embeddings = engine.embed_batch(&texts).unwrap();
        assert_eq!(embeddings.len(), 3);
        assert_eq!(embeddings[0].len(), 384);
    }

    #[test]
    #[ignore]
    fn test_semantic_similarity() {
        let engine = EmbeddingEngine::new("models/all-MiniLM-L6-v2.onnx").unwrap();
        let auth_vec = engine.embed("user authentication login password").unwrap();
        let validate_vec = engine.embed("validate credentials check password").unwrap();
        let database_vec = engine.embed("SQL database query table insert").unwrap();

        let auth_validate_sim = cosine_similarity(&auth_vec, &validate_vec);
        let auth_database_sim = cosine_similarity(&auth_vec, &database_vec);

        // Auth and validate should be more similar than auth and database
        assert!(auth_validate_sim > auth_database_sim,
            "Expected auth-validate ({}) > auth-database ({})",
            auth_validate_sim, auth_database_sim);
    }

    fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
        let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
        let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm_a == 0.0 || norm_b == 0.0 { return 0.0; }
        dot / (norm_a * norm_b)
    }
}
```

**Step 2: Run test — expect failure**

Run: `cd daemon && cargo test`
Expected: FAIL — `embeddings` module doesn't exist

**Step 3: Write the embedding engine**

`daemon/crates/lattice-core/src/embeddings/engine.rs`:
```rust
use anyhow::{Context, Result};
use ndarray::{Array2, Axis};
use ort::session::Session;
use tokenizers::Tokenizer;

/// Local embedding engine using ONNX Runtime.
/// Loads all-MiniLM-L6-v2 (384-dim output).
pub struct EmbeddingEngine {
    session: Session,
    tokenizer: Tokenizer,
}

impl EmbeddingEngine {
    pub fn new(model_path: &str) -> Result<Self> {
        let session = Session::builder()?
            .with_intra_threads(4)?
            .commit_from_file(model_path)
            .context("Failed to load ONNX model")?;

        // Load tokenizer from the same directory as the model
        let model_dir = std::path::Path::new(model_path).parent()
            .unwrap_or(std::path::Path::new("."));
        let tokenizer_path = model_dir.join("tokenizer.json");
        let tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| anyhow::anyhow!("Failed to load tokenizer: {}", e))?;

        Ok(Self { session, tokenizer })
    }

    /// Embed a single text string. Returns a 384-dim f32 vector.
    pub fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let batch = self.embed_batch(&[text])?;
        Ok(batch.into_iter().next().unwrap())
    }

    /// Embed a batch of text strings. Returns one 384-dim vector per input.
    pub fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let encodings = self.tokenizer.encode_batch(texts.to_vec(), true)
            .map_err(|e| anyhow::anyhow!("Tokenization failed: {}", e))?;

        let max_len = encodings.iter().map(|e| e.get_ids().len()).max().unwrap_or(0);
        let batch_size = encodings.len();

        // Build input tensors
        let mut input_ids = Array2::<i64>::zeros((batch_size, max_len));
        let mut attention_mask = Array2::<i64>::zeros((batch_size, max_len));
        let mut token_type_ids = Array2::<i64>::zeros((batch_size, max_len));

        for (i, encoding) in encodings.iter().enumerate() {
            for (j, &id) in encoding.get_ids().iter().enumerate() {
                input_ids[[i, j]] = id as i64;
            }
            for (j, &mask) in encoding.get_attention_mask().iter().enumerate() {
                attention_mask[[i, j]] = mask as i64;
            }
            for (j, &type_id) in encoding.get_type_ids().iter().enumerate() {
                token_type_ids[[i, j]] = type_id as i64;
            }
        }

        let outputs = self.session.run(ort::inputs! {
            "input_ids" => input_ids,
            "attention_mask" => attention_mask.clone(),
            "token_type_ids" => token_type_ids,
        }?)?;

        // Get the token embeddings output (shape: batch_size x seq_len x hidden_dim)
        let token_embeddings = outputs[0].try_extract_tensor::<f32>()?;
        let token_embeddings = token_embeddings.to_owned();

        // Mean pooling with attention mask
        let mut results = Vec::with_capacity(batch_size);
        let hidden_dim = token_embeddings.shape()[2];

        for i in 0..batch_size {
            let mut pooled = vec![0.0f32; hidden_dim];
            let mut mask_sum = 0.0f32;

            for j in 0..max_len {
                let mask_val = attention_mask[[i, j]] as f32;
                mask_sum += mask_val;
                for k in 0..hidden_dim {
                    pooled[k] += token_embeddings[[i, j, k]] * mask_val;
                }
            }

            if mask_sum > 0.0 {
                for val in &mut pooled {
                    *val /= mask_sum;
                }
            }

            // L2 normalize
            let norm: f32 = pooled.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm > 0.0 {
                for val in &mut pooled {
                    *val /= norm;
                }
            }

            results.push(pooled);
        }

        Ok(results)
    }

    /// Get the embedding dimension (384 for MiniLM).
    pub fn dimension(&self) -> usize {
        384
    }
}
```

`daemon/crates/lattice-core/src/embeddings/mod.rs`:
```rust
pub mod engine;

#[cfg(test)]
mod tests;

pub use engine::EmbeddingEngine;
```

**Step 4: Register the module**

Add to `daemon/crates/lattice-core/src/lib.rs`:
```rust
pub mod embeddings;
```

**Step 5: Verify it compiles (tests are #[ignore] for now)**

Run: `cd daemon && cargo build`
Expected: Compiles

**Step 6: Commit**

```bash
git add daemon/crates/lattice-core/src/embeddings/
git commit -m "feat: local ONNX embedding engine with MiniLM — single and batch inference"
```

---

### Task 5.3: Add Vector Storage to SQLite

**Files:**
- Create: `daemon/crates/lattice-core/src/storage/vector_store.rs`
- Modify: `daemon/crates/lattice-core/src/storage/mod.rs`
- Modify: `daemon/crates/lattice-core/src/storage/schema.rs`
- Modify: `daemon/crates/lattice-core/src/storage/tests.rs`

**Step 1: Write the failing test**

Append to `daemon/crates/lattice-core/src/storage/tests.rs`:
```rust
#[test]
fn test_vector_store_and_search() {
    use crate::storage::VectorStore;

    let store = VectorStore::open_in_memory().unwrap();
    store.initialize(384).unwrap();

    // Insert some vectors
    let vec_a = vec![1.0f32; 384]; // dummy vectors
    let mut vec_b = vec![0.0f32; 384];
    vec_b[0] = 1.0;
    let vec_c = vec![-1.0f32; 384];

    store.upsert_vector("src/auth.ts", "loginUser", 0, &vec_a).unwrap();
    store.upsert_vector("src/crypto.ts", "hashPassword", 0, &vec_b).unwrap();
    store.upsert_vector("src/other.ts", "unrelated", 0, &vec_c).unwrap();

    // Search for vectors similar to vec_a
    let results = store.search(&vec_a, 2).unwrap();
    assert_eq!(results.len(), 2);
    // First result should be loginUser (exact match)
    assert_eq!(results[0].0, "loginUser");
}

#[test]
fn test_vector_delete_by_file() {
    use crate::storage::VectorStore;

    let store = VectorStore::open_in_memory().unwrap();
    store.initialize(384).unwrap();

    let vec_a = vec![1.0f32; 384];
    store.upsert_vector("src/auth.ts", "login", 0, &vec_a).unwrap();
    store.upsert_vector("src/auth.ts", "logout", 10, &vec_a).unwrap();
    store.upsert_vector("src/other.ts", "helper", 0, &vec_a).unwrap();

    store.delete_by_file("src/auth.ts").unwrap();

    let results = store.search(&vec_a, 10).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].0, "helper");
}
```

**Step 2: Run test — expect failure**

**Step 3: Write the vector store**

Note: Since sqlite-vec requires a native extension, we'll use a pure-Rust approach for MVP: store vectors as BLOBs in SQLite and compute cosine similarity in Rust. This avoids the sqlite-vec native extension complexity for now. We can swap to sqlite-vec later for performance.

`daemon/crates/lattice-core/src/storage/vector_store.rs`:
```rust
use anyhow::Result;
use rusqlite::{params, Connection};

/// Stores embedding vectors in SQLite and provides similarity search.
///
/// MVP implementation: vectors stored as BLOBs, similarity computed in Rust.
/// Can be upgraded to sqlite-vec for native vector search at scale.
pub struct VectorStore {
    conn: Connection,
}

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub name: String,
    pub file: String,
    pub byte_offset: usize,
    pub similarity: f32,
}

impl VectorStore {
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
        Ok(Self { conn })
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Ok(Self { conn })
    }

    pub fn initialize(&self, _dimension: usize) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS vectors (
                file TEXT NOT NULL,
                name TEXT NOT NULL,
                byte_offset INTEGER NOT NULL,
                embedding BLOB NOT NULL,
                PRIMARY KEY (file, name, byte_offset)
            );
            CREATE INDEX IF NOT EXISTS idx_vectors_file ON vectors(file);"
        )?;
        Ok(())
    }

    pub fn upsert_vector(&self, file: &str, name: &str, byte_offset: usize, vector: &[f32]) -> Result<()> {
        let blob = vector_to_blob(vector);
        self.conn.execute(
            "INSERT OR REPLACE INTO vectors (file, name, byte_offset, embedding) VALUES (?1, ?2, ?3, ?4)",
            params![file, name, byte_offset, blob],
        )?;
        Ok(())
    }

    pub fn delete_by_file(&self, file: &str) -> Result<()> {
        self.conn.execute("DELETE FROM vectors WHERE file = ?1", params![file])?;
        Ok(())
    }

    /// Search for the top-K most similar vectors.
    /// Returns (name, file, byte_offset, similarity_score).
    pub fn search(&self, query: &[f32], top_k: usize) -> Result<Vec<(String, String, usize, f32)>> {
        let mut stmt = self.conn.prepare(
            "SELECT file, name, byte_offset, embedding FROM vectors"
        )?;

        let mut results: Vec<(String, String, usize, f32)> = stmt.query_map([], |row| {
            let file: String = row.get(0)?;
            let name: String = row.get(1)?;
            let byte_offset: usize = row.get(2)?;
            let blob: Vec<u8> = row.get(3)?;
            let vector = blob_to_vector(&blob);
            let sim = cosine_similarity(query, &vector);
            Ok((name, file, byte_offset, sim))
        })?.collect::<Result<Vec<_>, _>>()?;

        // Sort by similarity descending
        results.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal));
        results.truncate(top_k);
        Ok(results)
    }
}

fn vector_to_blob(vector: &[f32]) -> Vec<u8> {
    vector.iter().flat_map(|f| f.to_le_bytes()).collect()
}

fn blob_to_vector(blob: &[u8]) -> Vec<f32> {
    blob.chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect()
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 { return 0.0; }
    dot / (norm_a * norm_b)
}
```

**Step 4: Register vector store**

Add to `daemon/crates/lattice-core/src/storage/mod.rs`:
```rust
pub mod vector_store;
pub use vector_store::VectorStore;
```

**Step 5: Run tests**

Run: `cd daemon && cargo test`
Expected: All tests pass

**Step 6: Commit**

```bash
git add daemon/crates/lattice-core/src/storage/
git commit -m "feat: vector storage in SQLite with cosine similarity search"
```

---

## Phase 6: Context Capsule Query Engine

Goal: Implement the core query pipeline — intent detection, semantic search, graph traversal, ranking, budget allocation, and capsule assembly.

### Task 6.1: Define the Capsule Model

**Files:**
- Create: `daemon/crates/lattice-core/src/query/mod.rs`
- Create: `daemon/crates/lattice-core/src/query/capsule.rs`
- Modify: `daemon/crates/lattice-core/src/lib.rs`

**Step 1: Write the capsule data model**

`daemon/crates/lattice-core/src/query/capsule.rs`:
```rust
use serde::{Deserialize, Serialize};

/// The detected intent of a query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueryIntent {
    Explore,
    FixBug,
    Refactor,
    AddFeature,
    Unknown,
}

/// A pivot node — full source code included.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PivotNode {
    pub symbol: String,
    pub kind: String,
    pub file: String,
    pub line: usize,
    pub source: String,
    pub why: String,
    pub score: f64,
}

/// A context node — skeleton (signature) only.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextNode {
    pub symbol: String,
    pub kind: String,
    pub file: String,
    pub line: usize,
    pub skeleton: String,
    pub relationship: String,
    pub score: f64,
}

/// Statistics about the capsule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapsuleStats {
    pub tokens_used: usize,
    pub tokens_saved: usize,
    pub nodes_evaluated: usize,
    pub nodes_included: usize,
}

/// A Context Capsule — the query result returned to AI agents.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextCapsule {
    pub query: String,
    pub intent: QueryIntent,
    pub pivots: Vec<PivotNode>,
    pub context: Vec<ContextNode>,
    pub memories: Vec<serde_json::Value>, // Populated in Phase 11
    pub stats: CapsuleStats,
}
```

`daemon/crates/lattice-core/src/query/mod.rs`:
```rust
pub mod capsule;
pub mod intent;
pub mod engine;

#[cfg(test)]
mod tests;

pub use capsule::ContextCapsule;
pub use engine::QueryEngine;
pub use intent::detect_intent;
```

**Step 2: Register the module**

Add to `daemon/crates/lattice-core/src/lib.rs`:
```rust
pub mod query;
```

**Step 3: Verify it compiles**

Run: `cd daemon && cargo build`
Expected: Compiles

**Step 4: Commit**

```bash
git add daemon/crates/lattice-core/src/query/
git commit -m "feat: define Context Capsule model — pivots, context, stats"
```

---

### Task 6.2: Implement Intent Detection

**Files:**
- Create: `daemon/crates/lattice-core/src/query/intent.rs`
- Modify: `daemon/crates/lattice-core/src/query/tests.rs`

**Step 1: Write the failing test**

`daemon/crates/lattice-core/src/query/tests.rs`:
```rust
#[cfg(test)]
mod tests {
    use crate::query::intent::detect_intent;
    use crate::query::capsule::QueryIntent;

    #[test]
    fn test_detect_explore_intent() {
        assert_eq!(detect_intent("How does authentication work?"), QueryIntent::Explore);
        assert_eq!(detect_intent("Explain the login flow"), QueryIntent::Explore);
        assert_eq!(detect_intent("What does this module do?"), QueryIntent::Explore);
    }

    #[test]
    fn test_detect_fix_bug_intent() {
        assert_eq!(detect_intent("Fix the login bug"), QueryIntent::FixBug);
        assert_eq!(detect_intent("There's an error in authentication"), QueryIntent::FixBug);
        assert_eq!(detect_intent("Debug the crash in session handler"), QueryIntent::FixBug);
    }

    #[test]
    fn test_detect_refactor_intent() {
        assert_eq!(detect_intent("Refactor the auth module"), QueryIntent::Refactor);
        assert_eq!(detect_intent("Clean up the login code"), QueryIntent::Refactor);
        assert_eq!(detect_intent("Restructure the database layer"), QueryIntent::Refactor);
    }

    #[test]
    fn test_detect_add_feature_intent() {
        assert_eq!(detect_intent("Add OAuth support"), QueryIntent::AddFeature);
        assert_eq!(detect_intent("Implement two-factor authentication"), QueryIntent::AddFeature);
        assert_eq!(detect_intent("Create a new endpoint for user profiles"), QueryIntent::AddFeature);
    }
}
```

**Step 2: Run test — expect failure**

**Step 3: Write intent detection (keyword-based for MVP)**

`daemon/crates/lattice-core/src/query/intent.rs`:
```rust
use super::capsule::QueryIntent;

/// Detect the intent of a natural language query.
/// Uses keyword matching for MVP — can be upgraded to a classifier later.
pub fn detect_intent(query: &str) -> QueryIntent {
    let lower = query.to_lowercase();

    // Check for fix/bug intent
    let fix_keywords = ["fix", "bug", "error", "crash", "fail", "broken", "debug", "issue", "wrong", "exception"];
    if fix_keywords.iter().any(|k| lower.contains(k)) {
        return QueryIntent::FixBug;
    }

    // Check for refactor intent
    let refactor_keywords = ["refactor", "clean up", "cleanup", "restructure", "reorganize", "simplify", "improve", "optimize"];
    if refactor_keywords.iter().any(|k| lower.contains(k)) {
        return QueryIntent::Refactor;
    }

    // Check for add feature intent
    let add_keywords = ["add", "implement", "create", "build", "new", "introduce", "support for"];
    if add_keywords.iter().any(|k| lower.contains(k)) {
        return QueryIntent::AddFeature;
    }

    // Check for explore intent
    let explore_keywords = ["how", "what", "explain", "describe", "show", "where", "understand", "overview"];
    if explore_keywords.iter().any(|k| lower.contains(k)) {
        return QueryIntent::Explore;
    }

    QueryIntent::Unknown
}

/// Get query parameters based on intent.
pub fn intent_params(intent: QueryIntent) -> IntentParams {
    match intent {
        QueryIntent::Explore => IntentParams {
            semantic_k: 10,
            hop_depth: 2,
            semantic_weight: 0.4,
            centrality_weight: 0.3,
            recency_weight: 0.2,
            caller_weight: 0.1,
            base_token_budget: 3000,
        },
        QueryIntent::FixBug => IntentParams {
            semantic_k: 5,
            hop_depth: 3,
            semantic_weight: 0.3,
            centrality_weight: 0.1,
            recency_weight: 0.4,
            caller_weight: 0.2,
            base_token_budget: 2500,
        },
        QueryIntent::Refactor => IntentParams {
            semantic_k: 8,
            hop_depth: 2,
            semantic_weight: 0.3,
            centrality_weight: 0.4,
            recency_weight: 0.1,
            caller_weight: 0.2,
            base_token_budget: 3500,
        },
        QueryIntent::AddFeature => IntentParams {
            semantic_k: 8,
            hop_depth: 2,
            semantic_weight: 0.4,
            centrality_weight: 0.2,
            recency_weight: 0.1,
            caller_weight: 0.3,
            base_token_budget: 3000,
        },
        QueryIntent::Unknown => IntentParams {
            semantic_k: 8,
            hop_depth: 2,
            semantic_weight: 0.4,
            centrality_weight: 0.3,
            recency_weight: 0.2,
            caller_weight: 0.1,
            base_token_budget: 3000,
        },
    }
}

/// Parameters that vary based on detected intent.
#[derive(Debug, Clone)]
pub struct IntentParams {
    pub semantic_k: usize,
    pub hop_depth: usize,
    pub semantic_weight: f64,
    pub centrality_weight: f64,
    pub recency_weight: f64,
    pub caller_weight: f64,
    pub base_token_budget: usize,
}
```

**Step 4: Run tests**

Run: `cd daemon && cargo test`
Expected: All intent tests pass

**Step 5: Commit**

```bash
git add daemon/crates/lattice-core/src/query/
git commit -m "feat: intent detection — keyword-based classification of query intent"
```

---

### Task 6.3: Implement the Query Engine

**Files:**
- Create: `daemon/crates/lattice-core/src/query/engine.rs`
- Modify: `daemon/crates/lattice-core/src/query/tests.rs`

**Step 1: Write the failing test**

Append to `daemon/crates/lattice-core/src/query/tests.rs`:
```rust
#[test]
fn test_query_engine_produces_capsule() {
    use crate::graph::{CodeGraph, EdgeKind};
    use crate::symbols::{Language, SymbolId, SymbolKind};
    use crate::query::engine::QueryEngine;

    // Build a small test graph
    let mut graph = CodeGraph::new();
    let login = SymbolId { file: "src/auth.ts".into(), name: "loginUser".into(), byte_offset: 0 };
    let hash = SymbolId { file: "src/crypto.ts".into(), name: "hashPassword".into(), byte_offset: 0 };
    let session = SymbolId { file: "src/session.ts".into(), name: "createSession".into(), byte_offset: 0 };
    let unrelated = SymbolId { file: "src/utils.ts".into(), name: "formatDate".into(), byte_offset: 0 };

    graph.add_node(login.clone(), SymbolKind::Function, "loginUser".into(),
        "fn loginUser(u: string, p: string): Promise<Session>".into(),
        "async function loginUser(u, p) {\n  const h = hashPassword(p);\n  return createSession(u, h);\n}".into(),
        "src/auth.ts".into(), 10, 15, true, Language::TypeScript);
    graph.add_node(hash.clone(), SymbolKind::Function, "hashPassword".into(),
        "fn hashPassword(plain: string): string".into(),
        "function hashPassword(plain) { return bcrypt.hash(plain); }".into(),
        "src/crypto.ts".into(), 5, 8, true, Language::TypeScript);
    graph.add_node(session.clone(), SymbolKind::Function, "createSession".into(),
        "fn createSession(user: string, hash: string): Session".into(),
        "function createSession(user, hash) { return { user, token: sign(hash) }; }".into(),
        "src/session.ts".into(), 1, 5, true, Language::TypeScript);
    graph.add_node(unrelated.clone(), SymbolKind::Function, "formatDate".into(),
        "fn formatDate(d: Date): string".into(),
        "function formatDate(d) { return d.toISOString(); }".into(),
        "src/utils.ts".into(), 1, 3, true, Language::TypeScript);

    graph.add_edge(&login, &hash, EdgeKind::Calls);
    graph.add_edge(&login, &session, EdgeKind::Calls);

    // Create engine without embeddings (falls back to name matching)
    let engine = QueryEngine::new(graph, None);
    let capsule = engine.query("How does authentication work?", None);

    // Should include login-related nodes, not formatDate
    assert!(!capsule.pivots.is_empty() || !capsule.context.is_empty());
    assert_eq!(capsule.intent, QueryIntent::Explore);

    let all_symbols: Vec<_> = capsule.pivots.iter().map(|p| p.symbol.as_str())
        .chain(capsule.context.iter().map(|c| c.symbol.as_str()))
        .collect();

    // loginUser should be included (name matches "authentication" concept)
    // formatDate should NOT be included
    assert!(!all_symbols.contains(&"formatDate"),
        "formatDate should not be in results for an auth query");
}

#[test]
fn test_query_engine_token_budget() {
    use crate::graph::{CodeGraph, EdgeKind};
    use crate::symbols::{Language, SymbolId, SymbolKind};
    use crate::query::engine::QueryEngine;

    let mut graph = CodeGraph::new();
    // Add many nodes to test budget enforcement
    for i in 0..20 {
        let id = SymbolId {
            file: format!("src/mod{}.ts", i),
            name: format!("func{}", i),
            byte_offset: 0,
        };
        let body = "x".repeat(500); // ~125 tokens each
        graph.add_node(id, SymbolKind::Function, format!("func{}", i),
            format!("fn func{}(): void", i), body,
            format!("src/mod{}.ts", i), 1, 10, true, Language::TypeScript);
    }

    let engine = QueryEngine::new(graph, None);
    let capsule = engine.query("find all functions", None);

    // Total tokens should be within budget (3000 default for explore)
    assert!(capsule.stats.tokens_used <= 4000,
        "Token budget exceeded: {} tokens", capsule.stats.tokens_used);
}
```

**Step 2: Run test — expect failure**

**Step 3: Write the query engine**

`daemon/crates/lattice-core/src/query/engine.rs`:
```rust
use std::collections::{HashMap, HashSet};

use crate::graph::{CodeGraph, GraphNode};
use crate::symbols::SymbolId;
use crate::storage::VectorStore;

use super::capsule::*;
use super::intent::{detect_intent, intent_params};

/// The core query engine. Produces Context Capsules from natural language queries.
pub struct QueryEngine {
    graph: CodeGraph,
    vector_store: Option<VectorStore>,
    /// Tracks repeated queries for adaptive budget expansion.
    query_history: HashMap<String, usize>,
}

impl QueryEngine {
    pub fn new(graph: CodeGraph, vector_store: Option<VectorStore>) -> Self {
        Self {
            graph,
            vector_store,
            query_history: HashMap::new(),
        }
    }

    /// Execute a query and return a Context Capsule.
    pub fn query(&self, query_text: &str, _embedding: Option<&[f32]>) -> ContextCapsule {
        // Step 1: Detect intent
        let intent = detect_intent(query_text);
        let params = intent_params(intent);

        // Step 2: Semantic search (or fallback to name matching)
        let semantic_hits = self.semantic_search(query_text, _embedding, params.semantic_k);

        // Step 3: Graph traversal — expand from semantic hits
        let mut candidates: HashMap<SymbolId, CandidateNode> = HashMap::new();

        for (id, similarity) in &semantic_hits {
            // Add the hit itself
            if let Some(node) = self.graph.get_node(id) {
                candidates.entry(id.clone()).or_insert(CandidateNode {
                    node: node.clone(),
                    semantic_similarity: *similarity,
                    relationship: "semantic_match".to_string(),
                });
            }

            // Traverse N hops outward
            let neighbors = self.graph.n_hop_neighbors(id, params.hop_depth);
            for neighbor in neighbors {
                candidates.entry(neighbor.id.clone()).or_insert(CandidateNode {
                    node: neighbor.clone(),
                    semantic_similarity: 0.0, // Not a direct semantic hit
                    relationship: format!("graph_neighbor_of:{}", id.name),
                });
            }
        }

        let nodes_evaluated = candidates.len();

        // Step 4: Rank candidates
        let mut scored: Vec<(SymbolId, f64, CandidateNode)> = candidates.into_iter()
            .map(|(id, candidate)| {
                let centrality = self.graph.centrality(&id);
                let dependents = self.graph.get_dependents(&id).len() as f64;
                let max_dependents = 20.0; // normalize

                let score = candidate.semantic_similarity as f64 * params.semantic_weight
                    + centrality * params.centrality_weight
                    + 0.0 * params.recency_weight // TODO: recency from edit tracking
                    + (dependents / max_dependents).min(1.0) * params.caller_weight;

                (id, score, candidate)
            })
            .collect();

        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        // Step 5: Budget allocation
        let repeat_count = self.query_history.get(query_text).copied().unwrap_or(0);
        let budget = params.base_token_budget + (repeat_count * 500); // Adaptive expansion

        let mut pivots = Vec::new();
        let mut context = Vec::new();
        let mut tokens_used = 0;

        for (id, score, candidate) in &scored {
            let est_tokens = estimate_tokens(&candidate.node.body);

            if *score > 0.7 && tokens_used + est_tokens <= budget {
                // Pivot — include full source
                pivots.push(PivotNode {
                    symbol: candidate.node.name.clone(),
                    kind: format!("{:?}", candidate.node.kind),
                    file: candidate.node.file.clone(),
                    line: candidate.node.line,
                    source: candidate.node.body.clone(),
                    why: format!("score: {:.2}, {}", score, candidate.relationship),
                    score: *score,
                });
                tokens_used += est_tokens;
            } else if *score > 0.3 {
                // Context — skeleton only
                let skeleton_tokens = estimate_tokens(&candidate.node.signature) + 10;
                if tokens_used + skeleton_tokens <= budget {
                    let line_count = candidate.node.end_line - candidate.node.line + 1;
                    let dep_count = self.graph.get_dependents(&candidate.node.id).len();

                    context.push(ContextNode {
                        symbol: candidate.node.name.clone(),
                        kind: format!("{:?}", candidate.node.kind),
                        file: candidate.node.file.clone(),
                        line: candidate.node.line,
                        skeleton: format!("{}  // {} lines, {} callers",
                            candidate.node.signature, line_count, dep_count),
                        relationship: candidate.relationship.clone(),
                        score: *score,
                    });
                    tokens_used += skeleton_tokens;
                }
            }
        }

        // Estimate tokens saved
        let total_possible: usize = scored.iter()
            .map(|(_, _, c)| estimate_tokens(&c.node.body))
            .sum();
        let tokens_saved = total_possible.saturating_sub(tokens_used);

        ContextCapsule {
            query: query_text.to_string(),
            intent,
            pivots,
            context,
            memories: vec![], // Populated in Phase 11
            stats: CapsuleStats {
                tokens_used,
                tokens_saved,
                nodes_evaluated,
                nodes_included: pivots.len() + context.len(),
            },
        }
    }

    /// Semantic search using vector store, or fallback to name-based matching.
    fn semantic_search(
        &self,
        query_text: &str,
        embedding: Option<&[f32]>,
        top_k: usize,
    ) -> Vec<(SymbolId, f32)> {
        // If we have an embedding and vector store, use semantic search
        if let (Some(emb), Some(vs)) = (embedding, &self.vector_store) {
            if let Ok(results) = vs.search(emb, top_k) {
                return results.into_iter()
                    .map(|(name, file, offset, sim)| {
                        (SymbolId { file, name, byte_offset: offset }, sim)
                    })
                    .collect();
            }
        }

        // Fallback: keyword matching against node names and signatures
        let query_lower = query_text.to_lowercase();
        let query_words: Vec<&str> = query_lower.split_whitespace().collect();

        let mut matches: Vec<(SymbolId, f32)> = self.graph.all_nodes().iter()
            .filter_map(|node| {
                let name_lower = node.name.to_lowercase();
                let sig_lower = node.signature.to_lowercase();
                let combined = format!("{} {}", name_lower, sig_lower);

                let match_count = query_words.iter()
                    .filter(|w| combined.contains(**w))
                    .count();

                if match_count > 0 {
                    let score = match_count as f32 / query_words.len() as f32;
                    Some((node.id.clone(), score))
                } else {
                    None
                }
            })
            .collect();

        matches.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        matches.truncate(top_k);
        matches
    }

    /// Update the graph (used for incremental updates).
    pub fn update_graph(&mut self, graph: CodeGraph) {
        self.graph = graph;
    }

    /// Record a query for adaptive budget expansion.
    pub fn record_query(&mut self, query: &str) {
        *self.query_history.entry(query.to_string()).or_insert(0) += 1;
    }
}

struct CandidateNode {
    node: crate::graph::GraphNode,
    semantic_similarity: f32,
    relationship: String,
}

/// Rough token estimation: ~4 characters per token.
fn estimate_tokens(text: &str) -> usize {
    (text.len() + 3) / 4
}
```

**Step 4: Run tests**

Run: `cd daemon && cargo test`
Expected: All tests pass

**Step 5: Commit**

```bash
git add daemon/crates/lattice-core/src/query/
git commit -m "feat: Context Capsule query engine — semantic search, graph traversal, ranking, budget allocation"
```

---

## Phase 7: JSON-RPC and MCP Server

Goal: Implement the stdio JSON-RPC server that handles both internal commands (from the VS Code extension) and MCP tool calls (from AI agents). This is the daemon's communication layer.

### Task 7.1: Add JSON-RPC Dependencies

**Files:**
- Modify: `daemon/Cargo.toml`
- Modify: `daemon/crates/lattice-daemon/Cargo.toml`

**Step 1: Add dependencies**

Add to `daemon/Cargo.toml` under `[workspace.dependencies]`:
```toml
async-trait = "0.1"
```

Add to `daemon/crates/lattice-daemon/Cargo.toml` under `[dependencies]`:
```toml
async-trait = { workspace = true }
```

**Step 2: Verify it builds**

Run: `cd daemon && cargo build`
Expected: Compiles

**Step 3: Commit**

```bash
git add daemon/Cargo.toml daemon/crates/lattice-daemon/Cargo.toml
git commit -m "chore: add async-trait dependency for daemon"
```

---

### Task 7.2: Implement JSON-RPC Protocol Layer

**Files:**
- Create: `daemon/crates/lattice-daemon/src/rpc/mod.rs`
- Create: `daemon/crates/lattice-daemon/src/rpc/protocol.rs`
- Create: `daemon/crates/lattice-daemon/src/rpc/tests.rs`

**Step 1: Write the failing test**

`daemon/crates/lattice-daemon/src/rpc/tests.rs`:
```rust
#[cfg(test)]
mod tests {
    use crate::rpc::protocol::{JsonRpcRequest, JsonRpcResponse, parse_request, format_response};

    #[test]
    fn test_parse_valid_request() {
        let json = r#"{"jsonrpc":"2.0","id":1,"method":"query_context","params":{"query":"How does auth work?"}}"#;
        let req = parse_request(json).unwrap();
        assert_eq!(req.method, "query_context");
        assert_eq!(req.id, serde_json::json!(1));
    }

    #[test]
    fn test_parse_notification() {
        let json = r#"{"jsonrpc":"2.0","method":"initialized"}"#;
        let req = parse_request(json).unwrap();
        assert_eq!(req.method, "initialized");
        assert!(req.id.is_null());
    }

    #[test]
    fn test_format_success_response() {
        let response = JsonRpcResponse::success(
            serde_json::json!(1),
            serde_json::json!({"result": "ok"}),
        );
        let json = format_response(&response);
        assert!(json.contains("\"jsonrpc\":\"2.0\""));
        assert!(json.contains("\"id\":1"));
        assert!(json.contains("\"result\""));
    }

    #[test]
    fn test_format_error_response() {
        let response = JsonRpcResponse::error(
            serde_json::json!(1),
            -32600,
            "Invalid Request".to_string(),
        );
        let json = format_response(&response);
        assert!(json.contains("\"error\""));
        assert!(json.contains("-32600"));
    }
}
```

**Step 2: Run test — expect failure**

**Step 3: Write the protocol module**

`daemon/crates/lattice-daemon/src/rpc/protocol.rs`:
```rust
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    #[serde(default)]
    pub id: Value,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Value::is_null")]
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl JsonRpcResponse {
    pub fn success(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(id: Value, code: i32, message: String) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(JsonRpcError { code, message, data: None }),
        }
    }
}

pub fn parse_request(json: &str) -> Result<JsonRpcRequest, String> {
    serde_json::from_str(json).map_err(|e| format!("Failed to parse JSON-RPC request: {}", e))
}

pub fn format_response(response: &JsonRpcResponse) -> String {
    serde_json::to_string(response).unwrap_or_else(|_| "{}".to_string())
}
```

`daemon/crates/lattice-daemon/src/rpc/mod.rs`:
```rust
pub mod protocol;
pub mod server;
pub mod mcp;

#[cfg(test)]
mod tests;
```

**Step 4: Run tests**

Run: `cd daemon && cargo test`
Expected: All tests pass

**Step 5: Commit**

```bash
git add daemon/crates/lattice-daemon/src/rpc/
git commit -m "feat: JSON-RPC 2.0 protocol layer — request parsing, response formatting"
```

---

### Task 7.3: Implement stdio JSON-RPC Server

**Files:**
- Create: `daemon/crates/lattice-daemon/src/rpc/server.rs`

**Step 1: Write the server**

`daemon/crates/lattice-daemon/src/rpc/server.rs`:
```rust
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;
use tracing;

use super::protocol::{JsonRpcRequest, JsonRpcResponse, parse_request, format_response};

/// Handler trait for processing JSON-RPC requests.
#[async_trait::async_trait]
pub trait RequestHandler: Send + Sync {
    async fn handle(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value, (i32, String)>;
}

/// Stdio-based JSON-RPC server.
/// Reads newline-delimited JSON from stdin, writes responses to stdout.
pub struct StdioServer {
    handler: Arc<dyn RequestHandler>,
}

impl StdioServer {
    pub fn new(handler: Arc<dyn RequestHandler>) -> Self {
        Self { handler }
    }

    /// Run the server loop. Reads from stdin, writes to stdout.
    /// Uses Content-Length headers per MCP spec (LSP-style framing).
    pub async fn run(&self) -> anyhow::Result<()> {
        let stdin = tokio::io::stdin();
        let stdout = Arc::new(Mutex::new(tokio::io::stdout()));
        let mut reader = BufReader::new(stdin);

        tracing::info!("Lattice daemon listening on stdio");

        loop {
            // Read Content-Length header
            let mut header_line = String::new();
            let bytes_read = reader.read_line(&mut header_line).await?;
            if bytes_read == 0 {
                tracing::info!("stdin closed, shutting down");
                break;
            }

            let header_line = header_line.trim();
            if header_line.is_empty() {
                continue; // Skip empty lines between messages
            }

            // Try to parse as Content-Length header
            let content_length = if header_line.starts_with("Content-Length:") {
                let len_str = header_line.trim_start_matches("Content-Length:").trim();
                len_str.parse::<usize>().ok()
            } else {
                // Fallback: treat as raw JSON line (for simple testing)
                let response = self.handle_raw_line(header_line).await;
                if let Some(resp) = response {
                    let json = format_response(&resp);
                    let msg = format!("Content-Length: {}\r\n\r\n{}", json.len(), json);
                    let mut out = stdout.lock().await;
                    out.write_all(msg.as_bytes()).await?;
                    out.flush().await?;
                }
                continue;
            };

            if let Some(length) = content_length {
                // Read the blank line after headers
                let mut blank = String::new();
                reader.read_line(&mut blank).await?;

                // Read the content body
                let mut body = vec![0u8; length];
                tokio::io::AsyncReadExt::read_exact(&mut reader, &mut body).await?;
                let body_str = String::from_utf8_lossy(&body);

                let response = self.handle_raw_line(&body_str).await;
                if let Some(resp) = response {
                    let json = format_response(&resp);
                    let msg = format!("Content-Length: {}\r\n\r\n{}", json.len(), json);
                    let mut out = stdout.lock().await;
                    out.write_all(msg.as_bytes()).await?;
                    out.flush().await?;
                }
            }
        }

        Ok(())
    }

    async fn handle_raw_line(&self, line: &str) -> Option<JsonRpcResponse> {
        let request = match parse_request(line) {
            Ok(req) => req,
            Err(e) => {
                return Some(JsonRpcResponse::error(
                    serde_json::Value::Null,
                    -32700,
                    format!("Parse error: {}", e),
                ));
            }
        };

        // Notifications (no id) don't get responses
        if request.id.is_null() {
            tracing::debug!("Received notification: {}", request.method);
            return None;
        }

        let result = self.handler.handle(&request.method, request.params).await;

        Some(match result {
            Ok(value) => JsonRpcResponse::success(request.id, value),
            Err((code, message)) => JsonRpcResponse::error(request.id, code, message),
        })
    }
}
```

**Step 2: Verify it compiles**

Run: `cd daemon && cargo build`
Expected: Compiles

**Step 3: Commit**

```bash
git add daemon/crates/lattice-daemon/src/rpc/server.rs
git commit -m "feat: stdio JSON-RPC server with Content-Length framing (MCP-compatible)"
```

---

### Task 7.4: Implement MCP Tool Handlers

**Files:**
- Create: `daemon/crates/lattice-daemon/src/rpc/mcp.rs`
- Modify: `daemon/crates/lattice-daemon/src/main.rs`

**Step 1: Write the MCP handler**

`daemon/crates/lattice-daemon/src/rpc/mcp.rs`:
```rust
use std::sync::Arc;
use tokio::sync::RwLock;
use serde_json::{json, Value};

use lattice_core::query::QueryEngine;
use super::server::RequestHandler;

/// MCP-compatible request handler.
/// Routes JSON-RPC methods to the appropriate tool implementations.
pub struct McpHandler {
    engine: Arc<RwLock<QueryEngine>>,
}

impl McpHandler {
    pub fn new(engine: Arc<RwLock<QueryEngine>>) -> Self {
        Self { engine }
    }

    async fn handle_initialize(&self, _params: Value) -> Result<Value, (i32, String)> {
        Ok(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {
                "tools": {}
            },
            "serverInfo": {
                "name": "lattice",
                "version": env!("CARGO_PKG_VERSION")
            }
        }))
    }

    async fn handle_tools_list(&self, _params: Value) -> Result<Value, (i32, String)> {
        Ok(json!({
            "tools": [
                {
                    "name": "query_context",
                    "description": "Query the codebase dependency graph. Returns a Context Capsule with pivot functions (full code) and context skeletons (signatures only) relevant to your question. Use this for any code-related question.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "query": {
                                "type": "string",
                                "description": "Natural language query about the codebase"
                            },
                            "file_filter": {
                                "type": "string",
                                "description": "Optional glob pattern to restrict search (e.g., 'src/auth/**')"
                            }
                        },
                        "required": ["query"]
                    }
                },
                {
                    "name": "get_symbol",
                    "description": "Look up a specific symbol (function, class, type) by name. Returns full source code, file location, and dependency information.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "name": {
                                "type": "string",
                                "description": "The symbol name to look up"
                            }
                        },
                        "required": ["name"]
                    }
                },
                {
                    "name": "get_dependents",
                    "description": "Find all symbols that depend on a given symbol. Shows callers, importers, and implementors.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string", "description": "Symbol name" }
                        },
                        "required": ["name"]
                    }
                },
                {
                    "name": "get_dependencies",
                    "description": "Find all symbols that a given symbol depends on. Shows callees, imports, and type references.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string", "description": "Symbol name" }
                        },
                        "required": ["name"]
                    }
                },
                {
                    "name": "blast_radius",
                    "description": "Analyze the impact of changing a symbol. Returns all transitively dependent nodes ranked by impact.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string", "description": "Symbol name to analyze" }
                        },
                        "required": ["name"]
                    }
                },
                {
                    "name": "search_symbols",
                    "description": "Search for symbols by name pattern or semantic query.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "query": { "type": "string", "description": "Search query" },
                            "limit": { "type": "integer", "description": "Max results (default 10)" }
                        },
                        "required": ["query"]
                    }
                },
                {
                    "name": "get_file_context",
                    "description": "Get a file's role in the dependency graph — exports, importers, hotspot score, co-change partners.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "file": { "type": "string", "description": "File path relative to workspace root" }
                        },
                        "required": ["file"]
                    }
                },
                {
                    "name": "store_memory",
                    "description": "Store an observation, decision, or pattern for future reference. Memories persist across sessions and are automatically linked to relevant code.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "content": { "type": "string", "description": "The memory content" },
                            "type": { "type": "string", "enum": ["observation", "decision", "exploration"], "description": "Memory type" },
                            "linked_symbols": {
                                "type": "array",
                                "items": { "type": "string" },
                                "description": "Symbol names this memory relates to"
                            }
                        },
                        "required": ["content", "type"]
                    }
                },
                {
                    "name": "recall_memories",
                    "description": "Retrieve memories relevant to a query. Returns ranked memories with staleness indicators.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "query": { "type": "string", "description": "What to recall" },
                            "limit": { "type": "integer", "description": "Max results (default 5)" }
                        },
                        "required": ["query"]
                    }
                },
                {
                    "name": "get_project_rules",
                    "description": "Get automatically detected project conventions and patterns.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {}
                    }
                }
            ]
        }))
    }

    async fn handle_tools_call(&self, params: Value) -> Result<Value, (i32, String)> {
        let tool_name = params.get("name")
            .and_then(|v| v.as_str())
            .ok_or((-32602, "Missing tool name".to_string()))?;

        let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

        match tool_name {
            "query_context" => self.tool_query_context(arguments).await,
            "get_symbol" => self.tool_get_symbol(arguments).await,
            "get_dependents" => self.tool_get_dependents(arguments).await,
            "get_dependencies" => self.tool_get_dependencies(arguments).await,
            "blast_radius" => self.tool_blast_radius(arguments).await,
            "search_symbols" => self.tool_search_symbols(arguments).await,
            "store_memory" => Ok(json!({"content": [{"type": "text", "text": "Memory storage not yet implemented (Phase 11)"}]})),
            "recall_memories" => Ok(json!({"content": [{"type": "text", "text": "Memory recall not yet implemented (Phase 11)"}]})),
            "get_project_rules" => Ok(json!({"content": [{"type": "text", "text": "Project rules not yet implemented (Phase 12)"}]})),
            "get_file_context" => self.tool_get_file_context(arguments).await,
            _ => Err((-32601, format!("Unknown tool: {}", tool_name))),
        }
    }

    async fn tool_query_context(&self, args: Value) -> Result<Value, (i32, String)> {
        let query = args.get("query")
            .and_then(|v| v.as_str())
            .ok_or((-32602, "Missing query parameter".to_string()))?;

        let engine = self.engine.read().await;
        let capsule = engine.query(query, None);

        Ok(json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string_pretty(&capsule).unwrap_or_default()
            }]
        }))
    }

    async fn tool_get_symbol(&self, args: Value) -> Result<Value, (i32, String)> {
        let name = args.get("name")
            .and_then(|v| v.as_str())
            .ok_or((-32602, "Missing name parameter".to_string()))?;

        let engine = self.engine.read().await;
        // Search for symbol by name — use query engine's fallback search
        let capsule = engine.query(&format!("symbol:{}", name), None);

        let found = capsule.pivots.iter()
            .find(|p| p.symbol == name)
            .or_else(|| capsule.pivots.first());

        match found {
            Some(pivot) => Ok(json!({
                "content": [{
                    "type": "text",
                    "text": format!("// {}:{}\n{}", pivot.file, pivot.line, pivot.source)
                }]
            })),
            None => Ok(json!({
                "content": [{
                    "type": "text",
                    "text": format!("Symbol '{}' not found in the index", name)
                }]
            })),
        }
    }

    async fn tool_get_dependents(&self, args: Value) -> Result<Value, (i32, String)> {
        let name = args.get("name")
            .and_then(|v| v.as_str())
            .ok_or((-32602, "Missing name parameter".to_string()))?;

        // TODO: Direct graph lookup by name. For now, use query engine.
        let engine = self.engine.read().await;
        let capsule = engine.query(&format!("what depends on {}", name), None);

        Ok(json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string_pretty(&capsule).unwrap_or_default()
            }]
        }))
    }

    async fn tool_get_dependencies(&self, args: Value) -> Result<Value, (i32, String)> {
        let name = args.get("name")
            .and_then(|v| v.as_str())
            .ok_or((-32602, "Missing name parameter".to_string()))?;

        let engine = self.engine.read().await;
        let capsule = engine.query(&format!("what does {} depend on", name), None);

        Ok(json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string_pretty(&capsule).unwrap_or_default()
            }]
        }))
    }

    async fn tool_blast_radius(&self, args: Value) -> Result<Value, (i32, String)> {
        let name = args.get("name")
            .and_then(|v| v.as_str())
            .ok_or((-32602, "Missing name parameter".to_string()))?;

        let engine = self.engine.read().await;
        let capsule = engine.query(&format!("blast radius of changing {}", name), None);

        Ok(json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string_pretty(&capsule).unwrap_or_default()
            }]
        }))
    }

    async fn tool_search_symbols(&self, args: Value) -> Result<Value, (i32, String)> {
        let query = args.get("query")
            .and_then(|v| v.as_str())
            .ok_or((-32602, "Missing query parameter".to_string()))?;

        let engine = self.engine.read().await;
        let capsule = engine.query(query, None);

        Ok(json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string_pretty(&capsule).unwrap_or_default()
            }]
        }))
    }

    async fn tool_get_file_context(&self, args: Value) -> Result<Value, (i32, String)> {
        let file = args.get("file")
            .and_then(|v| v.as_str())
            .ok_or((-32602, "Missing file parameter".to_string()))?;

        let engine = self.engine.read().await;
        let capsule = engine.query(&format!("file context for {}", file), None);

        Ok(json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string_pretty(&capsule).unwrap_or_default()
            }]
        }))
    }
}

#[async_trait::async_trait]
impl RequestHandler for McpHandler {
    async fn handle(&self, method: &str, params: Value) -> Result<Value, (i32, String)> {
        match method {
            "initialize" => self.handle_initialize(params).await,
            "tools/list" => self.handle_tools_list(params).await,
            "tools/call" => self.handle_tools_call(params).await,
            "ping" => Ok(json!({})),
            // Internal commands from VS Code extension
            "lattice/status" => Ok(json!({
                "status": "running",
                "version": env!("CARGO_PKG_VERSION"),
            })),
            "lattice/reindex" => {
                // TODO: Trigger full re-index
                Ok(json!({"status": "reindex_started"}))
            },
            _ => Err((-32601, format!("Method not found: {}", method))),
        }
    }
}
```

**Step 2: Update main.rs to wire everything together**

`daemon/crates/lattice-daemon/src/main.rs`:
```rust
mod rpc;

use std::sync::Arc;
use anyhow::Result;
use tokio::sync::RwLock;
use tracing_subscriber::EnvFilter;

use lattice_core::graph::CodeGraph;
use lattice_core::query::QueryEngine;
use rpc::mcp::McpHandler;
use rpc::server::StdioServer;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr) // Log to stderr, not stdout (stdout is for JSON-RPC)
        .init();

    tracing::info!("Lattice daemon v{} starting...", env!("CARGO_PKG_VERSION"));

    // Initialize with empty graph — will be populated by indexer
    let graph = CodeGraph::new();
    let engine = QueryEngine::new(graph, None);
    let engine = Arc::new(RwLock::new(engine));

    let handler = Arc::new(McpHandler::new(engine));
    let server = StdioServer::new(handler);

    server.run().await?;

    Ok(())
}
```

**Step 3: Verify it compiles**

Run: `cd daemon && cargo build`
Expected: Compiles

**Step 4: Commit**

```bash
git add daemon/crates/lattice-daemon/
git commit -m "feat: MCP server with stdio JSON-RPC — all 10 tool handlers wired up"
```

---

## Phase 8: File Watcher and Incremental Updates

Goal: Watch the workspace for file changes, re-parse only changed files, update the graph incrementally, and re-embed modified nodes.

### Task 8.1: Add notify Dependency

**Files:**
- Modify: `daemon/Cargo.toml`
- Modify: `daemon/crates/lattice-core/Cargo.toml`

**Step 1: Add notify**

Add to `daemon/Cargo.toml` under `[workspace.dependencies]`:
```toml
notify = "7"
```

Add to `daemon/crates/lattice-core/Cargo.toml` under `[dependencies]`:
```toml
notify = { workspace = true }
tokio = { workspace = true }
```

**Step 2: Verify it builds**

**Step 3: Commit**

```bash
git add daemon/Cargo.toml daemon/crates/lattice-core/Cargo.toml
git commit -m "chore: add notify dependency for file watching"
```

---

### Task 8.2: Implement the File Watcher

**Files:**
- Create: `daemon/crates/lattice-core/src/watcher/mod.rs`
- Create: `daemon/crates/lattice-core/src/watcher/tests.rs`
- Modify: `daemon/crates/lattice-core/src/lib.rs`

**Step 1: Write the failing test**

`daemon/crates/lattice-core/src/watcher/tests.rs`:
```rust
#[cfg(test)]
mod tests {
    use crate::watcher::{FileEvent, FileEventKind, should_index_file};
    use crate::symbols::Language;

    #[test]
    fn test_should_index_typescript() {
        assert!(should_index_file("src/auth.ts"));
        assert!(should_index_file("src/component.tsx"));
        assert!(should_index_file("lib/utils.js"));
    }

    #[test]
    fn test_should_index_python() {
        assert!(should_index_file("src/auth.py"));
        assert!(should_index_file("lib/utils.pyi"));
    }

    #[test]
    fn test_should_not_index_non_code() {
        assert!(!should_index_file("README.md"));
        assert!(!should_index_file("package.json"));
        assert!(!should_index_file("image.png"));
        assert!(!should_index_file(".env"));
    }

    #[test]
    fn test_should_not_index_excluded_dirs() {
        assert!(!should_index_file("node_modules/express/index.js"));
        assert!(!should_index_file(".git/objects/abc123"));
        assert!(!should_index_file("target/debug/build.rs"));
        assert!(!should_index_file("__pycache__/module.cpython-39.pyc"));
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
}
```

**Step 2: Run test — expect failure**

**Step 3: Write the watcher**

`daemon/crates/lattice-core/src/watcher/mod.rs`:
```rust
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::mpsc;
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::symbols::Language;

/// A file system event relevant to indexing.
#[derive(Debug, Clone)]
pub struct FileEvent {
    pub path: PathBuf,
    pub kind: FileEventKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FileEventKind {
    Created,
    Modified,
    Deleted,
}

/// Directories to always exclude from indexing.
const EXCLUDED_DIRS: &[&str] = &[
    "node_modules", ".git", "target", "dist", "build", "out",
    "__pycache__", ".venv", "venv", ".tox", ".mypy_cache",
    ".next", ".nuxt", ".svelte-kit", "coverage",
    ".lattice",
];

/// File patterns to always exclude (secrets, non-code).
const EXCLUDED_PATTERNS: &[&str] = &[
    ".env", "credentials", "id_rsa", "id_ed25519",
    ".pem", ".key", ".pfx", ".p12", ".jks",
];

/// Check if a file should be indexed based on its path.
pub fn should_index_file(path: &str) -> bool {
    let path_lower = path.to_lowercase();

    // Check excluded directories
    for dir in EXCLUDED_DIRS {
        if path_lower.contains(&format!("{}/", dir)) || path_lower.contains(&format!("{}\\", dir)) {
            return false;
        }
    }

    // Check excluded patterns
    for pattern in EXCLUDED_PATTERNS {
        let filename = Path::new(path).file_name()
            .and_then(|f| f.to_str())
            .unwrap_or("")
            .to_lowercase();
        if filename.contains(pattern) {
            return false;
        }
    }

    // Check if it's a supported language
    let ext = path.rsplit('.').next().unwrap_or("");
    let lang = Language::from_extension(ext);
    !matches!(lang, Language::Unknown)
}

/// Start watching a directory for file changes.
/// Returns a receiver channel that emits FileEvents.
pub fn start_watcher(
    root: PathBuf,
) -> anyhow::Result<(RecommendedWatcher, mpsc::UnboundedReceiver<FileEvent>)> {
    let (tx, rx) = mpsc::unbounded_channel();

    let mut watcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
        if let Ok(event) = res {
            let kind = match event.kind {
                EventKind::Create(_) => Some(FileEventKind::Created),
                EventKind::Modify(_) => Some(FileEventKind::Modified),
                EventKind::Remove(_) => Some(FileEventKind::Deleted),
                _ => None,
            };

            if let Some(kind) = kind {
                for path in event.paths {
                    let path_str = path.to_string_lossy().to_string();
                    if should_index_file(&path_str) {
                        let _ = tx.send(FileEvent {
                            path: path.clone(),
                            kind: kind.clone(),
                        });
                    }
                }
            }
        }
    })?;

    watcher.watch(&root, RecursiveMode::Recursive)?;

    Ok((watcher, rx))
}
```

**Step 4: Register the module**

Add to `daemon/crates/lattice-core/src/lib.rs`:
```rust
pub mod watcher;
```

**Step 5: Run tests**

Run: `cd daemon && cargo test`
Expected: All tests pass

**Step 6: Commit**

```bash
git add daemon/crates/lattice-core/src/watcher/
git commit -m "feat: file watcher with security exclusions — filters by language, excludes secrets and deps"
```

---

### Task 8.3: Implement Incremental Indexer

**Files:**
- Create: `daemon/crates/lattice-core/src/indexer/mod.rs`
- Create: `daemon/crates/lattice-core/src/indexer/tests.rs`
- Modify: `daemon/crates/lattice-core/src/lib.rs`

**Step 1: Write the failing test**

`daemon/crates/lattice-core/src/indexer/tests.rs`:
```rust
#[cfg(test)]
mod tests {
    use crate::indexer::Indexer;
    use crate::symbols::SymbolKind;
    use std::path::PathBuf;

    #[test]
    fn test_index_single_file() {
        let mut indexer = Indexer::new(PathBuf::from("/test/workspace"));
        indexer.index_file_content("src/auth.ts", r#"
export function loginUser(username: string): User {
    return findUser(username);
}
"#).unwrap();

        let graph = indexer.graph();
        assert!(graph.node_count() >= 1);

        let nodes = graph.all_nodes();
        let login = nodes.iter().find(|n| n.name == "loginUser");
        assert!(login.is_some());
    }

    #[test]
    fn test_incremental_update() {
        let mut indexer = Indexer::new(PathBuf::from("/test/workspace"));

        // Index initial version
        indexer.index_file_content("src/auth.ts", r#"
export function loginUser(): void {}
export function logoutUser(): void {}
"#).unwrap();
        assert_eq!(indexer.graph().all_nodes().iter()
            .filter(|n| n.kind == SymbolKind::Function).count(), 2);

        // Update: remove logoutUser, add validateUser
        indexer.index_file_content("src/auth.ts", r#"
export function loginUser(): void {}
export function validateUser(): void {}
"#).unwrap();

        let nodes = indexer.graph().all_nodes();
        let names: Vec<_> = nodes.iter().map(|n| n.name.as_str()).collect();
        assert!(names.contains(&"loginUser"));
        assert!(names.contains(&"validateUser"));
        assert!(!names.contains(&"logoutUser"));
    }
}
```

**Step 2: Run test — expect failure**

**Step 3: Write the indexer**

`daemon/crates/lattice-core/src/indexer/mod.rs`:
```rust
#[cfg(test)]
mod tests;

use std::path::PathBuf;
use anyhow::Result;
use tracing;

use crate::error::LatticeError;
use crate::graph::CodeGraph;
use crate::graph::builder::GraphBuilder;
use crate::parser;
use crate::symbols::ParsedFile;
use crate::watcher::should_index_file;

/// The incremental indexer. Maintains the dependency graph as files change.
pub struct Indexer {
    root: PathBuf,
    graph: CodeGraph,
    /// Cache of parsed files for incremental updates.
    parsed_files: std::collections::HashMap<String, ParsedFile>,
}

impl Indexer {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            graph: CodeGraph::new(),
            parsed_files: std::collections::HashMap::new(),
        }
    }

    /// Get a reference to the current graph.
    pub fn graph(&self) -> &CodeGraph {
        &self.graph
    }

    /// Take ownership of the graph (for transfer to query engine).
    pub fn take_graph(self) -> CodeGraph {
        self.graph
    }

    /// Index a directory by scanning all supported files.
    pub fn index_directory(&mut self, dir: &std::path::Path) -> Result<usize> {
        let mut count = 0;
        self.scan_directory(dir, &mut count)?;
        self.rebuild_graph();
        Ok(count)
    }

    fn scan_directory(&mut self, dir: &std::path::Path, count: &mut usize) -> Result<()> {
        let entries = std::fs::read_dir(dir)?;
        for entry in entries {
            let entry = entry?;
            let path = entry.path();

            if path.is_dir() {
                let dir_name = path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("");
                // Skip excluded directories
                if !crate::watcher::EXCLUDED_DIRS.contains(&dir_name) {
                    self.scan_directory(&path, count)?;
                }
            } else if path.is_file() {
                let rel_path = path.strip_prefix(&self.root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");

                if should_index_file(&rel_path) {
                    match std::fs::read_to_string(&path) {
                        Ok(content) => {
                            if let Err(e) = self.index_file_content(&rel_path, &content) {
                                tracing::warn!("Failed to parse {}: {}", rel_path, e);
                            } else {
                                *count += 1;
                            }
                        }
                        Err(e) => {
                            tracing::warn!("Failed to read {}: {}", rel_path, e);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Index (or re-index) a single file's content.
    pub fn index_file_content(&mut self, rel_path: &str, content: &str) -> Result<(), LatticeError> {
        let parsed = parser::parse_file(rel_path, content)?;

        // Remove old nodes for this file
        self.graph.remove_file_nodes(rel_path);

        // Store the parsed file
        self.parsed_files.insert(rel_path.to_string(), parsed);

        // Rebuild graph from all parsed files
        self.rebuild_graph();

        Ok(())
    }

    /// Remove a file from the index.
    pub fn remove_file(&mut self, rel_path: &str) {
        self.parsed_files.remove(rel_path);
        self.graph.remove_file_nodes(rel_path);
        self.rebuild_graph();
    }

    /// Rebuild the entire graph from cached parsed files.
    fn rebuild_graph(&mut self) {
        let mut builder = GraphBuilder::new();
        for parsed in self.parsed_files.values() {
            builder.add_file(parsed.clone());
        }
        self.graph = builder.build();
    }

    pub fn file_count(&self) -> usize {
        self.parsed_files.len()
    }
}
```

**Step 4: Make EXCLUDED_DIRS public**

In `daemon/crates/lattice-core/src/watcher/mod.rs`, change:
```rust
const EXCLUDED_DIRS: &[&str] = &[
```
to:
```rust
pub const EXCLUDED_DIRS: &[&str] = &[
```

**Step 5: Register the module**

Add to `daemon/crates/lattice-core/src/lib.rs`:
```rust
pub mod indexer;
```

**Step 6: Run tests**

Run: `cd daemon && cargo test`
Expected: All tests pass

**Step 7: Commit**

```bash
git add daemon/crates/lattice-core/src/indexer/ daemon/crates/lattice-core/src/watcher/
git commit -m "feat: incremental indexer — parse files, update graph, support file add/modify/delete"
```

---

## Phase 9: VS Code Extension — Daemon Lifecycle

Goal: The VS Code extension spawns the Rust daemon on activation, communicates via stdio JSON-RPC, monitors health, and restarts on failure.

### Task 9.1: Create Daemon Manager

**Files:**
- Create: `extension/src/daemon.ts`
- Modify: `extension/src/extension.ts`

**Step 1: Write the daemon manager**

`extension/src/daemon.ts`:
```typescript
import * as vscode from 'vscode';
import * as cp from 'child_process';
import * as path from 'path';

interface JsonRpcRequest {
    jsonrpc: '2.0';
    id: number;
    method: string;
    params?: unknown;
}

interface JsonRpcResponse {
    jsonrpc: '2.0';
    id: number;
    result?: unknown;
    error?: { code: number; message: string };
}

export class DaemonManager {
    private process: cp.ChildProcess | null = null;
    private requestId = 0;
    private pendingRequests = new Map<number, {
        resolve: (value: unknown) => void;
        reject: (error: Error) => void;
    }>();
    private buffer = '';
    private contentLength: number | null = null;
    private _onStatusChange = new vscode.EventEmitter<DaemonStatus>();
    public readonly onStatusChange = this._onStatusChange.event;
    private status: DaemonStatus = 'stopped';
    private restartCount = 0;
    private maxRestarts = 5;

    constructor(private context: vscode.ExtensionContext) {}

    get currentStatus(): DaemonStatus {
        return this.status;
    }

    async start(): Promise<void> {
        if (this.process) {
            return;
        }

        const binaryPath = this.getBinaryPath();
        if (!binaryPath) {
            this.setStatus('error');
            vscode.window.showErrorMessage('Lattice: Could not find daemon binary');
            return;
        }

        this.setStatus('starting');

        this.process = cp.spawn(binaryPath, [], {
            stdio: ['pipe', 'pipe', 'pipe'],
            env: {
                ...process.env,
                RUST_LOG: 'lattice=info',
            },
        });

        this.process.stdout?.on('data', (data: Buffer) => {
            this.handleStdoutData(data.toString());
        });

        this.process.stderr?.on('data', (data: Buffer) => {
            // Daemon logs go to stderr
            const output = data.toString().trim();
            if (output) {
                console.log(`[Lattice daemon] ${output}`);
            }
        });

        this.process.on('exit', (code) => {
            console.log(`Lattice daemon exited with code ${code}`);
            this.process = null;
            this.rejectAllPending(new Error(`Daemon exited with code ${code}`));

            if (this.status !== 'stopped' && this.restartCount < this.maxRestarts) {
                this.restartCount++;
                this.setStatus('starting');
                setTimeout(() => this.start(), 1000 * this.restartCount);
            } else {
                this.setStatus('stopped');
            }
        });

        // Send initialize request
        try {
            await this.sendRequest('initialize', {});
            this.setStatus('running');
            this.restartCount = 0;
        } catch (e) {
            this.setStatus('error');
        }
    }

    async stop(): Promise<void> {
        this.setStatus('stopped');
        if (this.process) {
            this.process.kill();
            this.process = null;
        }
        this.rejectAllPending(new Error('Daemon stopped'));
    }

    async sendRequest(method: string, params?: unknown): Promise<unknown> {
        if (!this.process?.stdin) {
            throw new Error('Daemon not running');
        }

        const id = ++this.requestId;
        const request: JsonRpcRequest = {
            jsonrpc: '2.0',
            id,
            method,
            params,
        };

        return new Promise((resolve, reject) => {
            this.pendingRequests.set(id, { resolve, reject });

            const body = JSON.stringify(request);
            const message = `Content-Length: ${Buffer.byteLength(body)}\r\n\r\n${body}`;

            this.process!.stdin!.write(message, (err) => {
                if (err) {
                    this.pendingRequests.delete(id);
                    reject(err);
                }
            });

            // Timeout after 30 seconds
            setTimeout(() => {
                if (this.pendingRequests.has(id)) {
                    this.pendingRequests.delete(id);
                    reject(new Error(`Request ${method} timed out`));
                }
            }, 30000);
        });
    }

    private handleStdoutData(data: string): void {
        this.buffer += data;

        while (true) {
            if (this.contentLength === null) {
                // Looking for Content-Length header
                const headerEnd = this.buffer.indexOf('\r\n\r\n');
                if (headerEnd === -1) { break; }

                const header = this.buffer.substring(0, headerEnd);
                const match = header.match(/Content-Length:\s*(\d+)/i);
                if (match) {
                    this.contentLength = parseInt(match[1], 10);
                    this.buffer = this.buffer.substring(headerEnd + 4);
                } else {
                    // Skip malformed header
                    this.buffer = this.buffer.substring(headerEnd + 4);
                    continue;
                }
            }

            if (this.contentLength !== null) {
                if (this.buffer.length >= this.contentLength) {
                    const body = this.buffer.substring(0, this.contentLength);
                    this.buffer = this.buffer.substring(this.contentLength);
                    this.contentLength = null;

                    try {
                        const response = JSON.parse(body) as JsonRpcResponse;
                        this.handleResponse(response);
                    } catch (e) {
                        console.error('Failed to parse daemon response:', e);
                    }
                } else {
                    break; // Wait for more data
                }
            }
        }
    }

    private handleResponse(response: JsonRpcResponse): void {
        const pending = this.pendingRequests.get(response.id);
        if (!pending) { return; }

        this.pendingRequests.delete(response.id);

        if (response.error) {
            pending.reject(new Error(`${response.error.code}: ${response.error.message}`));
        } else {
            pending.resolve(response.result);
        }
    }

    private getBinaryPath(): string | null {
        const ext = process.platform === 'win32' ? '.exe' : '';
        const binaryName = `lattice${ext}`;

        // Check extension directory first
        const bundled = path.join(this.context.extensionPath, 'bin', binaryName);
        if (require('fs').existsSync(bundled)) {
            return bundled;
        }

        // Check PATH
        try {
            const which = cp.execSync(`which ${binaryName} 2>/dev/null || where ${binaryName} 2>nul`, {
                encoding: 'utf8',
            }).trim();
            if (which) { return which.split('\n')[0]; }
        } catch {}

        // Development: check cargo target
        const cargoTarget = path.join(this.context.extensionPath, '..', 'daemon', 'target', 'debug', binaryName);
        if (require('fs').existsSync(cargoTarget)) {
            return cargoTarget;
        }

        return null;
    }

    private setStatus(status: DaemonStatus): void {
        this.status = status;
        this._onStatusChange.fire(status);
    }

    private rejectAllPending(error: Error): void {
        for (const [id, pending] of this.pendingRequests) {
            pending.reject(error);
        }
        this.pendingRequests.clear();
    }

    dispose(): void {
        this.stop();
        this._onStatusChange.dispose();
    }
}

export type DaemonStatus = 'starting' | 'running' | 'stopped' | 'error';
```

**Step 2: Update extension.ts to use DaemonManager**

`extension/src/extension.ts`:
```typescript
import * as vscode from 'vscode';
import { DaemonManager } from './daemon';

let daemon: DaemonManager;

export async function activate(context: vscode.ExtensionContext) {
    console.log('Lattice extension activating...');

    daemon = new DaemonManager(context);
    context.subscriptions.push({ dispose: () => daemon.dispose() });

    // Start the daemon
    await daemon.start();

    // Register commands
    const reindexCmd = vscode.commands.registerCommand('lattice.reindex', async () => {
        try {
            await daemon.sendRequest('lattice/reindex');
            vscode.window.showInformationMessage('Lattice: Re-indexing started');
        } catch (e) {
            vscode.window.showErrorMessage(`Lattice: Failed to re-index: ${e}`);
        }
    });

    const statusCmd = vscode.commands.registerCommand('lattice.showStatus', async () => {
        try {
            const result = await daemon.sendRequest('lattice/status') as any;
            vscode.window.showInformationMessage(
                `Lattice: ${result.status} (v${result.version})`
            );
        } catch (e) {
            vscode.window.showErrorMessage(`Lattice: Daemon not running`);
        }
    });

    context.subscriptions.push(reindexCmd, statusCmd);

    // Listen for status changes
    daemon.onStatusChange((status) => {
        console.log(`Lattice daemon status: ${status}`);
    });
}

export function deactivate() {
    console.log('Lattice extension deactivating...');
    daemon?.dispose();
}
```

**Step 3: Compile**

Run: `cd extension && npm run compile`
Expected: Compiles

**Step 4: Commit**

```bash
git add extension/src/
git commit -m "feat: daemon lifecycle manager — spawn, IPC, health monitoring, auto-restart"
```

---

## Phase 10: VS Code Extension — UI

Goal: Add sidebar panel, CodeLens, hover info, and status bar to the VS Code extension.

### Task 10.1: Add Status Bar

**Files:**
- Create: `extension/src/statusbar.ts`
- Modify: `extension/src/extension.ts`

**Step 1: Write the status bar provider**

`extension/src/statusbar.ts`:
```typescript
import * as vscode from 'vscode';
import { DaemonManager, DaemonStatus } from './daemon';

export class StatusBarProvider {
    private item: vscode.StatusBarItem;
    private nodeCount = 0;
    private indexingProgress: number | null = null;

    constructor(private daemon: DaemonManager) {
        this.item = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 100);
        this.item.command = 'lattice.showStatus';
        this.update(daemon.currentStatus);

        daemon.onStatusChange((status) => this.update(status));
    }

    update(status: DaemonStatus): void {
        switch (status) {
            case 'running':
                this.item.text = `$(check) Lattice: ${this.nodeCount.toLocaleString()} nodes`;
                this.item.tooltip = 'Lattice: Daemon running. Click for status.';
                this.item.backgroundColor = undefined;
                break;
            case 'starting':
                this.item.text = '$(sync~spin) Lattice: Starting...';
                this.item.tooltip = 'Lattice: Daemon starting...';
                this.item.backgroundColor = undefined;
                break;
            case 'stopped':
                this.item.text = '$(error) Lattice: Stopped';
                this.item.tooltip = 'Lattice: Daemon stopped. Click to restart.';
                this.item.backgroundColor = new vscode.ThemeColor('statusBarItem.errorBackground');
                break;
            case 'error':
                this.item.text = '$(error) Lattice: Error';
                this.item.tooltip = 'Lattice: Daemon error. Click for details.';
                this.item.backgroundColor = new vscode.ThemeColor('statusBarItem.errorBackground');
                break;
        }

        this.item.show();
    }

    updateNodeCount(count: number): void {
        this.nodeCount = count;
        if (this.daemon.currentStatus === 'running') {
            this.update('running');
        }
    }

    updateIndexingProgress(percent: number | null): void {
        this.indexingProgress = percent;
        if (percent !== null) {
            this.item.text = `$(sync~spin) Lattice: Indexing ${percent}%`;
        } else if (this.daemon.currentStatus === 'running') {
            this.update('running');
        }
    }

    dispose(): void {
        this.item.dispose();
    }
}
```

**Step 2: Wire into extension.ts**

Add to `extension/src/extension.ts` activate function:
```typescript
import { StatusBarProvider } from './statusbar';

// Inside activate():
const statusBar = new StatusBarProvider(daemon);
context.subscriptions.push({ dispose: () => statusBar.dispose() });
```

**Step 3: Commit**

```bash
git add extension/src/statusbar.ts extension/src/extension.ts
git commit -m "feat: status bar — daemon status, node count, indexing progress"
```

---

### Task 10.2: Add CodeLens Provider

**Files:**
- Create: `extension/src/codelens.ts`
- Modify: `extension/src/extension.ts`
- Modify: `extension/package.json`

**Step 1: Write the CodeLens provider**

`extension/src/codelens.ts`:
```typescript
import * as vscode from 'vscode';
import { DaemonManager } from './daemon';

interface SymbolInfo {
    name: string;
    line: number;
    dependentCount: number;
    fileCount: number;
}

export class LatticeCodeLensProvider implements vscode.CodeLensProvider {
    private _onDidChangeCodeLenses = new vscode.EventEmitter<void>();
    public readonly onDidChangeCodeLenses = this._onDidChangeCodeLenses.event;

    constructor(private daemon: DaemonManager) {}

    async provideCodeLenses(
        document: vscode.TextDocument,
        _token: vscode.CancellationToken
    ): Promise<vscode.CodeLens[]> {
        if (this.daemon.currentStatus !== 'running') {
            return [];
        }

        try {
            const relPath = vscode.workspace.asRelativePath(document.uri);
            const result = await this.daemon.sendRequest('lattice/file_symbols', {
                file: relPath,
            }) as { symbols: SymbolInfo[] } | null;

            if (!result?.symbols) { return []; }

            return result.symbols
                .filter(s => s.dependentCount > 0)
                .map(symbol => {
                    const range = new vscode.Range(
                        new vscode.Position(symbol.line - 1, 0),
                        new vscode.Position(symbol.line - 1, 0)
                    );
                    const lens = new vscode.CodeLens(range);
                    const fileText = symbol.fileCount === 1 ? 'file' : 'files';
                    lens.command = {
                        title: `Lattice: ${symbol.dependentCount} dependents across ${symbol.fileCount} ${fileText}`,
                        command: 'lattice.showDependents',
                        arguments: [symbol.name],
                    };
                    return lens;
                });
        } catch {
            return [];
        }
    }

    refresh(): void {
        this._onDidChangeCodeLenses.fire();
    }

    dispose(): void {
        this._onDidChangeCodeLenses.dispose();
    }
}
```

**Step 2: Register the CodeLens provider in extension.ts**

Add to activate():
```typescript
import { LatticeCodeLensProvider } from './codelens';

// Inside activate():
const codeLensProvider = new LatticeCodeLensProvider(daemon);
const codeLensReg = vscode.languages.registerCodeLensProvider(
    { scheme: 'file' },
    codeLensProvider
);
context.subscriptions.push(codeLensReg, { dispose: () => codeLensProvider.dispose() });

const showDependentsCmd = vscode.commands.registerCommand('lattice.showDependents', async (symbolName: string) => {
    try {
        const result = await daemon.sendRequest('tools/call', {
            name: 'get_dependents',
            arguments: { name: symbolName },
        });
        // Show in output channel or peek view
        const channel = vscode.window.createOutputChannel('Lattice Dependents');
        channel.clear();
        channel.appendLine(JSON.stringify(result, null, 2));
        channel.show();
    } catch (e) {
        vscode.window.showErrorMessage(`Lattice: ${e}`);
    }
});
context.subscriptions.push(showDependentsCmd);
```

**Step 3: Commit**

```bash
git add extension/src/codelens.ts extension/src/extension.ts
git commit -m "feat: CodeLens provider — inline dependent counts on exported symbols"
```

---

### Task 10.3: Add Hover Provider

**Files:**
- Create: `extension/src/hover.ts`
- Modify: `extension/src/extension.ts`

**Step 1: Write the hover provider**

`extension/src/hover.ts`:
```typescript
import * as vscode from 'vscode';
import { DaemonManager } from './daemon';

interface HoverInfo {
    name: string;
    dependentCount: number;
    crossRepoCount: number;
    topCallers: string[];
    hotspot: number; // 0-5
    lastModified: string;
}

export class LatticeHoverProvider implements vscode.HoverProvider {
    constructor(private daemon: DaemonManager) {}

    async provideHover(
        document: vscode.TextDocument,
        position: vscode.Position,
        _token: vscode.CancellationToken
    ): Promise<vscode.Hover | null> {
        if (this.daemon.currentStatus !== 'running') {
            return null;
        }

        const wordRange = document.getWordRangeAtPosition(position);
        if (!wordRange) { return null; }

        const word = document.getText(wordRange);
        if (!word || word.length < 2) { return null; }

        try {
            const result = await this.daemon.sendRequest('lattice/symbol_info', {
                name: word,
                file: vscode.workspace.asRelativePath(document.uri),
                line: position.line + 1,
            }) as HoverInfo | null;

            if (!result || result.dependentCount === 0) { return null; }

            const hotspotDots = '●'.repeat(result.hotspot) + '○'.repeat(5 - result.hotspot);
            const callersText = result.topCallers.length > 0
                ? result.topCallers.join(', ')
                : 'none';
            const crossRepoText = result.crossRepoCount > 0
                ? ` (${result.crossRepoCount} cross-repo)`
                : '';

            const md = new vscode.MarkdownString();
            md.appendMarkdown(`**Lattice Impact**\n\n`);
            md.appendMarkdown(`| | |\n|---|---|\n`);
            md.appendMarkdown(`| Dependents | ${result.dependentCount}${crossRepoText} |\n`);
            md.appendMarkdown(`| Top callers | ${callersText} |\n`);
            md.appendMarkdown(`| Hotspot | ${hotspotDots} |\n`);
            md.appendMarkdown(`| Last modified | ${result.lastModified} |\n`);
            md.isTrusted = true;

            return new vscode.Hover(md, wordRange);
        } catch {
            return null;
        }
    }
}
```

**Step 2: Register in extension.ts**

Add to activate():
```typescript
import { LatticeHoverProvider } from './hover';

// Inside activate():
const hoverProvider = new LatticeHoverProvider(daemon);
const hoverReg = vscode.languages.registerHoverProvider(
    { scheme: 'file' },
    hoverProvider
);
context.subscriptions.push(hoverReg);
```

**Step 3: Commit**

```bash
git add extension/src/hover.ts extension/src/extension.ts
git commit -m "feat: hover provider — impact info (dependents, callers, hotspot) on symbols"
```

---

### Task 10.4: Add Sidebar Panel

**Files:**
- Create: `extension/src/sidebar.ts`
- Modify: `extension/package.json`
- Modify: `extension/src/extension.ts`

**Step 1: Write the sidebar webview provider**

`extension/src/sidebar.ts`:
```typescript
import * as vscode from 'vscode';
import { DaemonManager, DaemonStatus } from './daemon';

export class LatticeSidebarProvider implements vscode.WebviewViewProvider {
    public static readonly viewType = 'lattice.sidebar';
    private _view?: vscode.WebviewView;

    constructor(
        private context: vscode.ExtensionContext,
        private daemon: DaemonManager,
    ) {
        daemon.onStatusChange(() => this.refresh());
    }

    resolveWebviewView(
        webviewView: vscode.WebviewView,
        _context: vscode.WebviewViewResolveContext,
        _token: vscode.CancellationToken,
    ): void {
        this._view = webviewView;
        webviewView.webview.options = { enableScripts: true };
        webviewView.webview.html = this.getHtml();

        webviewView.webview.onDidReceiveMessage(async (message) => {
            switch (message.command) {
                case 'reindex':
                    await vscode.commands.executeCommand('lattice.reindex');
                    break;
                case 'clearMemory':
                    await this.daemon.sendRequest('lattice/clear_memory');
                    break;
            }
        });
    }

    refresh(): void {
        if (this._view) {
            this._view.webview.html = this.getHtml();
        }
    }

    updateStats(stats: { nodeCount: number; fileCount: number; edgeCount: number }): void {
        if (this._view) {
            this._view.webview.postMessage({ command: 'updateStats', ...stats });
        }
    }

    private getHtml(): string {
        const status = this.daemon.currentStatus;
        const statusColor = status === 'running' ? '#4caf50' : status === 'error' ? '#f44336' : '#ff9800';
        const statusText = status.charAt(0).toUpperCase() + status.slice(1);

        return `<!DOCTYPE html>
<html>
<head>
    <style>
        body { font-family: var(--vscode-font-family); padding: 12px; color: var(--vscode-foreground); }
        .status { display: flex; align-items: center; gap: 8px; margin-bottom: 16px; }
        .dot { width: 10px; height: 10px; border-radius: 50%; background: ${statusColor}; }
        .stats { margin: 12px 0; }
        .stat { display: flex; justify-content: space-between; padding: 4px 0; }
        .stat-label { color: var(--vscode-descriptionForeground); }
        button {
            width: 100%; padding: 8px; margin: 4px 0;
            background: var(--vscode-button-background);
            color: var(--vscode-button-foreground);
            border: none; cursor: pointer; border-radius: 4px;
        }
        button:hover { background: var(--vscode-button-hoverBackground); }
        h3 { margin: 16px 0 8px 0; font-size: 12px; text-transform: uppercase;
             color: var(--vscode-descriptionForeground); }
    </style>
</head>
<body>
    <div class="status">
        <span class="dot"></span>
        <span>Daemon: ${statusText}</span>
    </div>

    <h3>Index Statistics</h3>
    <div class="stats" id="stats">
        <div class="stat"><span class="stat-label">Nodes</span><span id="nodeCount">-</span></div>
        <div class="stat"><span class="stat-label">Files</span><span id="fileCount">-</span></div>
        <div class="stat"><span class="stat-label">Edges</span><span id="edgeCount">-</span></div>
    </div>

    <h3>Actions</h3>
    <button onclick="post('reindex')">Re-index Workspace</button>
    <button onclick="post('clearMemory')">Clear Memory</button>

    <script>
        const vscode = acquireVsCodeApi();
        function post(cmd) { vscode.postMessage({ command: cmd }); }

        window.addEventListener('message', event => {
            const msg = event.data;
            if (msg.command === 'updateStats') {
                document.getElementById('nodeCount').textContent = msg.nodeCount.toLocaleString();
                document.getElementById('fileCount').textContent = msg.fileCount.toLocaleString();
                document.getElementById('edgeCount').textContent = msg.edgeCount.toLocaleString();
            }
        });
    </script>
</body>
</html>`;
    }
}
```

**Step 2: Register sidebar in package.json**

Add to `extension/package.json` under `"contributes"`:
```json
"viewsContainers": {
    "activitybar": [
        {
            "id": "lattice",
            "title": "Lattice",
            "icon": "resources/lattice-icon.svg"
        }
    ]
},
"views": {
    "lattice": [
        {
            "type": "webview",
            "id": "lattice.sidebar",
            "name": "Lattice"
        }
    ]
}
```

**Step 3: Register in extension.ts**

Add to activate():
```typescript
import { LatticeSidebarProvider } from './sidebar';

// Inside activate():
const sidebarProvider = new LatticeSidebarProvider(context, daemon);
const sidebarReg = vscode.window.registerWebviewViewProvider(
    LatticeSidebarProvider.viewType,
    sidebarProvider
);
context.subscriptions.push(sidebarReg);
```

**Step 4: Compile**

Run: `cd extension && npm run compile`
Expected: Compiles

**Step 5: Commit**

```bash
git add extension/
git commit -m "feat: sidebar panel — daemon status, index stats, action buttons"
```

---

## Phase 11: Session Memory

Goal: Implement persistent memory that survives across AI agent sessions. Memories are stored in SQLite, linked to graph nodes, embedded for semantic retrieval, and flagged as stale when linked code changes.

### Task 11.1: Define Memory Model and Storage

**Files:**
- Create: `daemon/crates/lattice-core/src/memory/mod.rs`
- Create: `daemon/crates/lattice-core/src/memory/model.rs`
- Create: `daemon/crates/lattice-core/src/memory/store.rs`
- Create: `daemon/crates/lattice-core/src/memory/tests.rs`
- Modify: `daemon/crates/lattice-core/src/lib.rs`

**Step 1: Write the failing test**

`daemon/crates/lattice-core/src/memory/tests.rs`:
```rust
#[cfg(test)]
mod tests {
    use crate::memory::{MemoryStore, MemoryType, Memory};

    #[test]
    fn test_store_and_retrieve_memory() {
        let store = MemoryStore::open_in_memory().unwrap();
        store.initialize().unwrap();

        let id = store.store(Memory {
            id: String::new(),
            content: "Auth system uses JWT with RS256".to_string(),
            memory_type: MemoryType::Observation,
            confidence: 0.9,
            linked_symbols: vec!["loginUser".to_string(), "validateToken".to_string()],
            source_query: Some("How does auth work?".to_string()),
            created_at: 0,
            last_accessed: 0,
            access_count: 0,
            is_stale: false,
            stale_reason: None,
        }).unwrap();

        assert!(!id.is_empty());

        let memories = store.list_all().unwrap();
        assert_eq!(memories.len(), 1);
        assert_eq!(memories[0].content, "Auth system uses JWT with RS256");
    }

    #[test]
    fn test_mark_stale_by_symbol() {
        let store = MemoryStore::open_in_memory().unwrap();
        store.initialize().unwrap();

        store.store(Memory {
            id: String::new(),
            content: "validatePassword uses bcrypt".to_string(),
            memory_type: MemoryType::Observation,
            confidence: 0.8,
            linked_symbols: vec!["validatePassword".to_string()],
            source_query: None,
            created_at: 0,
            last_accessed: 0,
            access_count: 0,
            is_stale: false,
            stale_reason: None,
        }).unwrap();

        store.mark_stale_by_symbol("validatePassword", "validatePassword() was modified").unwrap();

        let memories = store.list_all().unwrap();
        assert!(memories[0].is_stale);
        assert_eq!(memories[0].stale_reason.as_deref(), Some("validatePassword() was modified"));
    }

    #[test]
    fn test_search_memories_by_keyword() {
        let store = MemoryStore::open_in_memory().unwrap();
        store.initialize().unwrap();

        store.store(Memory {
            id: String::new(),
            content: "Auth uses JWT tokens".to_string(),
            memory_type: MemoryType::Observation,
            confidence: 0.9,
            linked_symbols: vec![],
            source_query: None,
            created_at: 0, last_accessed: 0, access_count: 0,
            is_stale: false, stale_reason: None,
        }).unwrap();

        store.store(Memory {
            id: String::new(),
            content: "Database uses PostgreSQL with connection pooling".to_string(),
            memory_type: MemoryType::Decision,
            confidence: 0.95,
            linked_symbols: vec![],
            source_query: None,
            created_at: 0, last_accessed: 0, access_count: 0,
            is_stale: false, stale_reason: None,
        }).unwrap();

        let results = store.search_by_keyword("JWT").unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].content.contains("JWT"));
    }

    #[test]
    fn test_invalidate_memory() {
        let store = MemoryStore::open_in_memory().unwrap();
        store.initialize().unwrap();

        let id = store.store(Memory {
            id: String::new(),
            content: "old info".to_string(),
            memory_type: MemoryType::Observation,
            confidence: 0.5,
            linked_symbols: vec![],
            source_query: None,
            created_at: 0, last_accessed: 0, access_count: 0,
            is_stale: false, stale_reason: None,
        }).unwrap();

        store.invalidate(&id).unwrap();
        let memories = store.list_all().unwrap();
        // Invalidated memories should not appear in list
        assert_eq!(memories.len(), 0);
    }
}
```

**Step 2: Run test — expect failure**

**Step 3: Write the memory model**

`daemon/crates/lattice-core/src/memory/model.rs`:
```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemoryType {
    Observation,
    Decision,
    Exploration,
    Pattern,
    AntiPattern,
}

impl MemoryType {
    pub fn as_str(&self) -> &str {
        match self {
            MemoryType::Observation => "observation",
            MemoryType::Decision => "decision",
            MemoryType::Exploration => "exploration",
            MemoryType::Pattern => "pattern",
            MemoryType::AntiPattern => "anti_pattern",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "observation" => MemoryType::Observation,
            "decision" => MemoryType::Decision,
            "exploration" => MemoryType::Exploration,
            "pattern" => MemoryType::Pattern,
            "anti_pattern" => MemoryType::AntiPattern,
            _ => MemoryType::Observation,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    pub id: String,
    pub content: String,
    pub memory_type: MemoryType,
    pub confidence: f64,
    pub linked_symbols: Vec<String>,
    pub source_query: Option<String>,
    pub created_at: u64,
    pub last_accessed: u64,
    pub access_count: u32,
    pub is_stale: bool,
    pub stale_reason: Option<String>,
}
```

**Step 4: Write the memory store**

`daemon/crates/lattice-core/src/memory/store.rs`:
```rust
use anyhow::Result;
use rusqlite::{params, Connection};

use super::model::{Memory, MemoryType};

pub struct MemoryStore {
    conn: Connection,
}

impl MemoryStore {
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
        Ok(Self { conn })
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Ok(Self { conn })
    }

    pub fn initialize(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS memories (
                id TEXT PRIMARY KEY,
                content TEXT NOT NULL,
                memory_type TEXT NOT NULL,
                confidence REAL NOT NULL DEFAULT 0.5,
                linked_symbols TEXT NOT NULL DEFAULT '[]',
                source_query TEXT,
                created_at INTEGER NOT NULL DEFAULT 0,
                last_accessed INTEGER NOT NULL DEFAULT 0,
                access_count INTEGER NOT NULL DEFAULT 0,
                is_stale INTEGER NOT NULL DEFAULT 0,
                stale_reason TEXT,
                is_invalidated INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS idx_memories_type ON memories(memory_type);
            CREATE INDEX IF NOT EXISTS idx_memories_stale ON memories(is_stale);"
        )?;
        Ok(())
    }

    pub fn store(&self, mut memory: Memory) -> Result<String> {
        if memory.id.is_empty() {
            memory.id = uuid_v4();
        }
        if memory.created_at == 0 {
            memory.created_at = now_unix();
        }

        let symbols_json = serde_json::to_string(&memory.linked_symbols)?;

        self.conn.execute(
            "INSERT OR REPLACE INTO memories (id, content, memory_type, confidence, linked_symbols, source_query, created_at, last_accessed, access_count, is_stale, stale_reason, is_invalidated)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 0)",
            params![
                memory.id, memory.content, memory.memory_type.as_str(),
                memory.confidence, symbols_json, memory.source_query,
                memory.created_at, memory.last_accessed, memory.access_count,
                memory.is_stale as i32, memory.stale_reason,
            ],
        )?;

        Ok(memory.id)
    }

    pub fn list_all(&self) -> Result<Vec<Memory>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, content, memory_type, confidence, linked_symbols, source_query, created_at, last_accessed, access_count, is_stale, stale_reason
             FROM memories WHERE is_invalidated = 0 ORDER BY created_at DESC"
        )?;

        let memories = stmt.query_map([], |row| {
            let symbols_json: String = row.get(4)?;
            let linked_symbols: Vec<String> = serde_json::from_str(&symbols_json).unwrap_or_default();

            Ok(Memory {
                id: row.get(0)?,
                content: row.get(1)?,
                memory_type: MemoryType::from_str(&row.get::<_, String>(2)?),
                confidence: row.get(3)?,
                linked_symbols,
                source_query: row.get(5)?,
                created_at: row.get(6)?,
                last_accessed: row.get(7)?,
                access_count: row.get(8)?,
                is_stale: row.get(9)?,
                stale_reason: row.get(10)?,
            })
        })?.collect::<Result<Vec<_>, _>>()?;

        Ok(memories)
    }

    pub fn search_by_keyword(&self, keyword: &str) -> Result<Vec<Memory>> {
        let pattern = format!("%{}%", keyword);
        let mut stmt = self.conn.prepare(
            "SELECT id, content, memory_type, confidence, linked_symbols, source_query, created_at, last_accessed, access_count, is_stale, stale_reason
             FROM memories WHERE is_invalidated = 0 AND content LIKE ?1 ORDER BY confidence DESC"
        )?;

        let memories = stmt.query_map(params![pattern], |row| {
            let symbols_json: String = row.get(4)?;
            let linked_symbols: Vec<String> = serde_json::from_str(&symbols_json).unwrap_or_default();

            Ok(Memory {
                id: row.get(0)?,
                content: row.get(1)?,
                memory_type: MemoryType::from_str(&row.get::<_, String>(2)?),
                confidence: row.get(3)?,
                linked_symbols,
                source_query: row.get(5)?,
                created_at: row.get(6)?,
                last_accessed: row.get(7)?,
                access_count: row.get(8)?,
                is_stale: row.get(9)?,
                stale_reason: row.get(10)?,
            })
        })?.collect::<Result<Vec<_>, _>>()?;

        Ok(memories)
    }

    pub fn mark_stale_by_symbol(&self, symbol_name: &str, reason: &str) -> Result<usize> {
        let pattern = format!("%\"{}\"%" , symbol_name);
        let count = self.conn.execute(
            "UPDATE memories SET is_stale = 1, stale_reason = ?2 WHERE linked_symbols LIKE ?1 AND is_invalidated = 0",
            params![pattern, reason],
        )?;
        Ok(count)
    }

    pub fn invalidate(&self, id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE memories SET is_invalidated = 1 WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }
}

fn uuid_v4() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    format!("{:x}-{:x}", nanos, rand_u32())
}

fn rand_u32() -> u32 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::time::SystemTime;
    let mut hasher = DefaultHasher::new();
    SystemTime::now().hash(&mut hasher);
    hasher.finish() as u32
}

fn now_unix() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}
```

`daemon/crates/lattice-core/src/memory/mod.rs`:
```rust
pub mod model;
pub mod store;

#[cfg(test)]
mod tests;

pub use model::{Memory, MemoryType};
pub use store::MemoryStore;
```

**Step 5: Register the module**

Add to `daemon/crates/lattice-core/src/lib.rs`:
```rust
pub mod memory;
```

**Step 6: Run tests**

Run: `cd daemon && cargo test`
Expected: All tests pass

**Step 7: Wire memory into MCP handler**

Update `daemon/crates/lattice-daemon/src/rpc/mcp.rs` to use `MemoryStore` for the `store_memory` and `recall_memories` tools instead of returning placeholder messages.

**Step 8: Commit**

```bash
git add daemon/crates/lattice-core/src/memory/ daemon/crates/lattice-daemon/src/rpc/mcp.rs
git commit -m "feat: session memory — store, recall, stale detection, keyword search"
```

---

## Phase 12: Passive Intelligence (Change Tracking)

Goal: Detect structural changes via AST diffing, identify co-change patterns, hotspots, anti-patterns, and auto-generate project rules.

### Task 12.1: Implement AST Diffing

**Files:**
- Create: `daemon/crates/lattice-core/src/diff/mod.rs`
- Create: `daemon/crates/lattice-core/src/diff/tests.rs`
- Modify: `daemon/crates/lattice-core/src/lib.rs`

**Step 1: Write the failing test**

`daemon/crates/lattice-core/src/diff/tests.rs`:
```rust
#[cfg(test)]
mod tests {
    use crate::diff::{diff_symbols, SymbolChange, ChangeKind};
    use crate::parser::parse_file;

    #[test]
    fn test_detect_added_function() {
        let old_source = "export function login(): void {}";
        let new_source = "export function login(): void {}\nexport function logout(): void {}";

        let old = parse_file("auth.ts", old_source).unwrap();
        let new = parse_file("auth.ts", new_source).unwrap();

        let changes = diff_symbols(&old.symbols, &new.symbols);
        let added: Vec<_> = changes.iter().filter(|c| c.kind == ChangeKind::Added).collect();
        assert_eq!(added.len(), 1);
        assert_eq!(added[0].name, "logout");
    }

    #[test]
    fn test_detect_removed_function() {
        let old_source = "export function login(): void {}\nexport function logout(): void {}";
        let new_source = "export function login(): void {}";

        let old = parse_file("auth.ts", old_source).unwrap();
        let new = parse_file("auth.ts", new_source).unwrap();

        let changes = diff_symbols(&old.symbols, &new.symbols);
        let removed: Vec<_> = changes.iter().filter(|c| c.kind == ChangeKind::Removed).collect();
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].name, "logout");
    }

    #[test]
    fn test_detect_modified_function() {
        let old_source = "export function login(): void { return null; }";
        let new_source = "export function login(): void { return authenticate(); }";

        let old = parse_file("auth.ts", old_source).unwrap();
        let new = parse_file("auth.ts", new_source).unwrap();

        let changes = diff_symbols(&old.symbols, &new.symbols);
        let modified: Vec<_> = changes.iter().filter(|c| c.kind == ChangeKind::Modified).collect();
        assert_eq!(modified.len(), 1);
        assert_eq!(modified[0].name, "login");
    }

    #[test]
    fn test_no_changes() {
        let source = "export function login(): void { return null; }";
        let old = parse_file("auth.ts", source).unwrap();
        let new = parse_file("auth.ts", source).unwrap();

        let changes = diff_symbols(&old.symbols, &new.symbols);
        assert!(changes.is_empty());
    }
}
```

**Step 2: Run test — expect failure**

**Step 3: Write the diff module**

`daemon/crates/lattice-core/src/diff/mod.rs`:
```rust
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use crate::symbols::Symbol;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Removed,
    Modified,
}

#[derive(Debug, Clone)]
pub struct SymbolChange {
    pub name: String,
    pub kind: ChangeKind,
    pub file: String,
}

/// Compare old and new symbol lists to detect structural changes.
pub fn diff_symbols(old: &[Symbol], new: &[Symbol]) -> Vec<SymbolChange> {
    let old_map: HashMap<&str, &Symbol> = old.iter()
        .map(|s| (s.name.as_str(), s))
        .collect();
    let new_map: HashMap<&str, &Symbol> = new.iter()
        .map(|s| (s.name.as_str(), s))
        .collect();

    let mut changes = Vec::new();

    // Find added and modified
    for (name, new_sym) in &new_map {
        match old_map.get(name) {
            None => {
                changes.push(SymbolChange {
                    name: name.to_string(),
                    kind: ChangeKind::Added,
                    file: new_sym.file.clone(),
                });
            }
            Some(old_sym) => {
                if old_sym.body != new_sym.body {
                    changes.push(SymbolChange {
                        name: name.to_string(),
                        kind: ChangeKind::Modified,
                        file: new_sym.file.clone(),
                    });
                }
            }
        }
    }

    // Find removed
    for (name, old_sym) in &old_map {
        if !new_map.contains_key(name) {
            changes.push(SymbolChange {
                name: name.to_string(),
                kind: ChangeKind::Removed,
                file: old_sym.file.clone(),
            });
        }
    }

    changes
}
```

**Step 4: Register the module and run tests**

Add to lib.rs: `pub mod diff;`

Run: `cd daemon && cargo test`
Expected: All tests pass

**Step 5: Commit**

```bash
git add daemon/crates/lattice-core/src/diff/
git commit -m "feat: AST diffing — detect added, removed, and modified symbols"
```

---

### Task 12.2: Implement Change Tracker (Co-changes, Hotspots, Anti-patterns)

**Files:**
- Create: `daemon/crates/lattice-core/src/intelligence/mod.rs`
- Create: `daemon/crates/lattice-core/src/intelligence/tracker.rs`
- Create: `daemon/crates/lattice-core/src/intelligence/tests.rs`
- Modify: `daemon/crates/lattice-core/src/lib.rs`

**Step 1: Write the tests, implement the tracker**

The ChangeTracker:
- Accepts `SymbolChange` events from the AST differ
- Tracks which symbols change together within a time window → co-change pairs
- Counts edits per symbol → hotspot scores
- Detects add-then-remove within a session → dead-end exploration
- Detects 5+ edits to same symbol in a session → thrashing

Implement with a `HashMap<String, ChangeHistory>` keyed by symbol name, tracking:
- `edit_timestamps: Vec<u64>`
- `session_additions: Vec<u64>` (timestamp of when first added this session)
- `session_removals: Vec<u64>` (timestamp of when removed this session)
- `co_change_partners: HashMap<String, u32>` (symbol name → co-change count)

**Step 2: Register and test**

**Step 3: Commit**

```bash
git add daemon/crates/lattice-core/src/intelligence/
git commit -m "feat: change tracker — co-change detection, hotspots, anti-pattern alerts"
```

---

## Phase 13: Multi-Repo Workspaces

Goal: Support VS Code multi-root workspaces. Each repo gets its own subgraph and SQLite database. Queries span all repos by default. Cross-repo edges detected automatically.

### Task 13.1: Implement Workspace Manager

**Files:**
- Create: `daemon/crates/lattice-core/src/workspace/mod.rs`
- Create: `daemon/crates/lattice-core/src/workspace/manager.rs`
- Modify: `daemon/crates/lattice-core/src/lib.rs`

**Step 1: Write the WorkspaceManager**

The WorkspaceManager:
- Holds a map of `repo_name → (Indexer, GraphStore, VectorStore)`
- Each repo is indexed independently
- Provides a unified query method that searches all repos
- Detects cross-repo edges by matching import paths across repo boundaries
- Stores a workspace-level metadata SQLite DB for cross-repo edge tracking

**Step 2: Update the MCP handler to use WorkspaceManager instead of a single QueryEngine**

**Step 3: Test with a mock multi-root workspace**

**Step 4: Commit**

```bash
git add daemon/crates/lattice-core/src/workspace/
git commit -m "feat: multi-repo workspace manager — unified queries across repos"
```

---

### Task 13.2: Cross-repo Edge Detection

**Files:**
- Modify: `daemon/crates/lattice-core/src/workspace/manager.rs`

**Step 1: Implement cross-repo import resolution**

When Repo A imports a package that maps to Repo B (detected via package.json, Cargo.toml, pyproject.toml), create cross-repo edges.

**Step 2: Test and commit**

```bash
git commit -m "feat: cross-repo edge detection via package manifest resolution"
```

---

## Phase 14: Security

Goal: Implement `.lattice_ignore`, default secret exclusions, content pattern filtering, and binary verification.

### Task 14.1: Implement .lattice_ignore

**Files:**
- Create: `daemon/crates/lattice-core/src/security/mod.rs`
- Create: `daemon/crates/lattice-core/src/security/ignore.rs`
- Create: `daemon/crates/lattice-core/src/security/tests.rs`
- Modify: `daemon/crates/lattice-core/src/lib.rs`

**Step 1: Write the ignore file parser**

Use `.gitignore` syntax. Parse `.lattice_ignore` from the workspace root. Combine with default exclusions. Apply to file watcher and indexer.

The `ignore` crate on crates.io provides gitignore-compatible matching:

Add to `daemon/Cargo.toml`:
```toml
ignore = "0.4"
```

**Step 2: Implement content pattern filtering**

After parsing a file, scan for sensitive patterns:
- `password=`, `secret=`, `api_key=`, `token=` in string literals
- AWS access key patterns (`AKIA...`)
- Base64-encoded key patterns

Redact matched content from the stored `body` field — replace with `[REDACTED]`.

**Step 3: Test and commit**

```bash
git add daemon/crates/lattice-core/src/security/
git commit -m "feat: security — .lattice_ignore, content redaction, secret pattern filtering"
```

---

### Task 14.2: Binary Verification

**Files:**
- Modify: `extension/src/daemon.ts`

**Step 1: SHA-256 verification**

Before spawning the daemon binary, compute its SHA-256 hash and compare against the expected hash shipped in the extension package. If mismatch, warn the user and refuse to start.

```typescript
import * as crypto from 'crypto';
import * as fs from 'fs';

function verifyBinary(binaryPath: string, expectedHash: string): boolean {
    const content = fs.readFileSync(binaryPath);
    const hash = crypto.createHash('sha256').update(content).digest('hex');
    return hash === expectedHash;
}
```

**Step 2: Commit**

```bash
git add extension/src/daemon.ts
git commit -m "feat: binary SHA-256 verification before daemon launch"
```

---

## Phase 15: Lazy Indexing for Large Repos

Goal: For repos with 50k+ files, implement lazy indexing that prioritizes open/active files and expands in the background.

### Task 15.1: Implement Priority Queue Indexer

**Files:**
- Create: `daemon/crates/lattice-core/src/indexer/lazy.rs`
- Modify: `daemon/crates/lattice-core/src/indexer/mod.rs`

**Step 1: Write the lazy indexer**

The LazyIndexer extends the base Indexer with:
- A priority queue (BinaryHeap) where files are ranked by:
  1. Currently open in editor (highest priority)
  2. Same directory as open files
  3. Direct imports of open files (1 hop)
  4. Recently modified (git log or filesystem mtime)
  5. Everything else (lowest priority)
- On cold start, only the high-priority files are indexed synchronously
- A background tokio task indexes remaining files at lower priority
- The VS Code extension sends `lattice/file_opened` notifications to boost priority

**Step 2: Test with a simulated large workspace**

Create a test that generates 1000 temp files, opens 3 of them, and verifies:
- The 3 open files are indexed within the first batch
- Their direct imports are indexed next
- Total index eventually covers all 1000 files
- Query engine returns results from open files immediately, even before full index

**Step 3: Commit**

```bash
git add daemon/crates/lattice-core/src/indexer/
git commit -m "feat: lazy indexer — priority queue with background expansion for large repos"
```

---

### Task 15.2: Wire Lazy Indexer into Daemon

**Files:**
- Modify: `daemon/crates/lattice-daemon/src/main.rs`
- Modify: `daemon/crates/lattice-daemon/src/rpc/mcp.rs`

**Step 1: Add file_opened handler**

When the MCP handler receives `lattice/file_opened`, boost that file's priority in the lazy indexer.

**Step 2: Add index progress reporting**

The daemon should periodically send `lattice/index_progress` notifications to the extension with:
```json
{"files_indexed": 234, "files_total": 5000, "percent": 4}
```

**Step 3: Commit**

```bash
git add daemon/crates/lattice-daemon/
git commit -m "feat: lazy indexing integration — file priority boost, progress reporting"
```

---

## Summary

| Phase | Description | Key Deliverable |
|---|---|---|
| 1 | Project Scaffolding | Rust workspace + VS Code extension skeleton |
| 2 | Tree-sitter Parsing | Multi-language symbol extraction |
| 3 | Dependency Graph | petgraph with traversal and centrality |
| 4 | SQLite Persistence | Save/load graph across restarts |
| 5 | Embeddings | Local ONNX inference + vector storage |
| 6 | Query Engine | Context Capsule generation |
| 7 | MCP Server | stdio JSON-RPC with 10 tool handlers |
| 8 | File Watcher | Incremental re-indexing on file changes |
| 9 | Extension Core | Daemon lifecycle, IPC, health monitoring |
| 10 | Extension UI | Sidebar, CodeLens, hover, status bar |
| 11 | Session Memory | Persistent memory with stale detection |
| 12 | Passive Intelligence | AST diffing, co-changes, hotspots |
| 13 | Multi-Repo | Unified queries across workspaces |
| 14 | Security | .lattice_ignore, content redaction, SHA-256 |
| 15 | Lazy Indexing | Priority queue for 50k+ file repos |
