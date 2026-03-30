pub mod go_lang;
pub mod java;
pub mod markdown;
pub mod python;
pub mod rust_lang;
pub mod typescript;

#[cfg(test)]
mod tests;

use crate::error::LatticeError;
use crate::symbols::{Language, ParsedFile};

/// Parse a source file and extract symbols.
pub fn parse_file(file_path: &str, source: &str) -> Result<ParsedFile, LatticeError> {
    let ext = file_path.rsplit('.').next().unwrap_or("");
    let language = Language::from_extension(ext);

    match language {
        Language::TypeScript | Language::JavaScript => {
            typescript::parse(file_path, source, language)
        }
        Language::Python => python::parse(file_path, source),
        Language::Rust => rust_lang::parse(file_path, source),
        Language::Go => go_lang::parse(file_path, source),
        Language::Java => java::parse(file_path, source),
        Language::Markdown => markdown::parse(file_path, source),
        _ => Err(LatticeError::Parse {
            file: file_path.to_string(),
            message: format!("Unsupported language: {:?}", language),
        }),
    }
}
