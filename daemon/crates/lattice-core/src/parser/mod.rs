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
        _ => Err(LatticeError::Parse {
            file: file_path.to_string(),
            message: format!("Unsupported language: {:?}", language),
        }),
    }
}
