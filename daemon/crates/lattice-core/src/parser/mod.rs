pub mod complexity_profile;
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
use complexity_profile::LanguageComplexityProfile;

/// The tree-sitter grammar backing a language, when one is vendored.
///
/// Single source of truth for grammar selection so fact producers parse with
/// exactly the grammar the symbol extractor used.
pub fn tree_sitter_language(language: Language) -> Option<tree_sitter::Language> {
    match language {
        Language::TypeScript => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        Language::JavaScript => Some(tree_sitter_javascript::LANGUAGE.into()),
        Language::Python => Some(tree_sitter_python::LANGUAGE.into()),
        Language::Rust => Some(tree_sitter_rust::LANGUAGE.into()),
        Language::Go => Some(tree_sitter_go::LANGUAGE.into()),
        Language::Java => Some(tree_sitter_java::LANGUAGE.into()),
        Language::Markdown | Language::Unknown => None,
    }
}

/// The complexity profile a language parser contributes, when the language is
/// one this crate parses. `Language::Unknown` has no parser and no profile.
pub fn complexity_profile_for(language: Language) -> Option<&'static LanguageComplexityProfile> {
    match language {
        Language::Rust => Some(rust_lang::complexity_profile()),
        Language::Python => Some(python::complexity_profile()),
        Language::Go => Some(go_lang::complexity_profile()),
        Language::Java => Some(java::complexity_profile()),
        Language::TypeScript | Language::JavaScript => {
            Some(typescript::complexity_profile(language))
        }
        _ => None,
    }
}

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
