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
