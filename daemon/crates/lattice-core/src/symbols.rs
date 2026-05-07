use serde::{Deserialize, Serialize};

pub const SYMBOL_HANDLE_PREFIX: &str = "symbol_id:";
pub const FILE_HANDLE_PREFIX: &str = "file_id:";

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

impl SymbolId {
    /// Stable handle used in follow-up expansion targets.
    pub fn stable_handle(&self) -> String {
        let payload = serde_json::json!({
            "file": self.file,
            "name": self.name,
            "byte_offset": self.byte_offset
        });
        format!("{SYMBOL_HANDLE_PREFIX}{payload}")
    }

    /// Parse a stable handle into a SymbolId.
    pub fn from_stable_handle(value: &str) -> Option<Self> {
        let payload = value.strip_prefix(SYMBOL_HANDLE_PREFIX)?;
        serde_json::from_str::<SymbolId>(payload).ok()
    }
}

/// Stable file handle used in follow-up expansion targets.
pub fn stable_file_handle(file: &str) -> String {
    format!("{FILE_HANDLE_PREFIX}{file}")
}

/// Parse a stable file handle into a file path.
pub fn parse_stable_file_handle(value: &str) -> Option<String> {
    let file = value.strip_prefix(FILE_HANDLE_PREFIX)?.trim();
    if file.is_empty() {
        return None;
    }
    Some(file.to_string())
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
    Document,
    Section,
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
    Markdown,
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
            "md" => Language::Markdown,
            _ => Language::Unknown,
        }
    }
}

impl SymbolKind {
    /// Compact string code for token-efficient output.
    pub fn short_code(&self) -> &'static str {
        match self {
            SymbolKind::Function => "fn",
            SymbolKind::Class => "cls",
            SymbolKind::Interface => "ifc",
            SymbolKind::TypeAlias => "type",
            SymbolKind::Enum => "enum",
            SymbolKind::Module => "mod",
            SymbolKind::Variable => "var",
            SymbolKind::Constant => "const",
            SymbolKind::Method => "meth",
            SymbolKind::Trait => "trait",
            SymbolKind::Struct => "struct",
            SymbolKind::Document => "doc",
            SymbolKind::Section => "sec",
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

/// A document-style link extracted from a file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkInfo {
    /// Symbol that owns the link.
    pub from: SymbolId,
    /// Raw target path or wiki-link target.
    pub target: String,
    /// Optional section or anchor target within the target document.
    pub heading: Option<String>,
    /// Visible text or alias, when available.
    pub text: Option<String>,
    /// Whether the original syntax was a wiki-link.
    pub is_wiki: bool,
}

/// Result of parsing a single file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedFile {
    pub file: String,
    pub language: Language,
    pub symbols: Vec<Symbol>,
    pub imports: Vec<ImportInfo>,
    pub links: Vec<LinkInfo>,
}
