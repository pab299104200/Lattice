use std::path::Path;

use crate::error::LatticeError;
use crate::parser::complexity_profile::{LanguageComplexityProfile, ProfileApplicability};
use crate::symbols::{Language, LinkInfo, ParsedFile, Symbol, SymbolId, SymbolKind};

/// Markdown is exempt from complexity facts.
///
/// The language has no functions and no executable control flow, so cyclomatic
/// complexity, nesting depth and parameter count are undefined for it rather
/// than zero. Documents are indexed as headings and sections, and the fact
/// producer reports `unavailable` with this reason instead of publishing
/// fabricated low-complexity facts that would make every document look like the
/// healthiest file in the repository (spec design decision 4, "unknown is never
/// zero"). Document size and link structure are the responsibility of other
/// fact families, not this one.
static MARKDOWN_COMPLEXITY_PROFILE: LanguageComplexityProfile = LanguageComplexityProfile {
    language: Language::Markdown,
    applicability: ProfileApplicability::NotApplicable {
        reason: "Markdown has no functions or executable control flow, so complexity facts are undefined rather than zero",
    },
    function_kinds: &[],
    branch_kinds: &[],
    boolean_operator_parent_kinds: &[],
    boolean_operator_kinds: &[],
    guarded_kinds: &[],
    nesting_kinds: &[],
    nesting_transparent_parent_kinds: &[],
    nesting_transparent_fields: &[],
    parameter_list_field: "",
    parameter_kinds: &[],
    is_default_branch: None,
    count_parameters: None,
    unit_identity: None,
};

/// The Markdown complexity profile contributed by this parser.
pub fn complexity_profile() -> &'static LanguageComplexityProfile {
    &MARKDOWN_COMPLEXITY_PROFILE
}

#[derive(Debug, Clone)]
struct Heading {
    title: String,
    line: usize,
    byte_offset: usize,
}

/// Parse a Markdown file into a document node plus section nodes.
///
/// This is intentionally lightweight and favors stable engineering-doc
/// structure over exhaustive Markdown compliance.
pub fn parse(file_path: &str, source: &str) -> Result<ParsedFile, LatticeError> {
    let lines: Vec<&str> = if source.is_empty() {
        vec![""]
    } else {
        source.lines().collect()
    };
    let line_offsets = line_offsets(source);
    let total_lines = lines.len().max(1);
    let mut headings = Vec::new();

    let mut in_frontmatter = source.starts_with("---\n") || source == "---";
    let mut in_fence = false;

    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim_end();
        let leading = trimmed.trim_start();

        if in_frontmatter {
            if index == 0 && leading == "---" {
                continue;
            }
            if leading == "---" {
                in_frontmatter = false;
            }
            continue;
        }

        if leading.starts_with("```") || leading.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }

        if in_fence {
            continue;
        }

        if let Some(title) = parse_heading_title(leading) {
            headings.push(Heading {
                title,
                line: index + 1,
                byte_offset: line_offsets.get(index).copied().unwrap_or(0),
            });
        }
    }

    let document_name = headings
        .first()
        .map(|heading| heading.title.clone())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| file_stem(file_path));

    let mut symbols = Vec::new();
    let mut links = Vec::new();

    let document_id = SymbolId {
        file: file_path.to_string(),
        name: document_name.clone(),
        byte_offset: 0,
    };
    symbols.push(Symbol {
        id: document_id,
        kind: SymbolKind::Document,
        name: document_name.clone(),
        signature: format!("doc {}", document_name),
        body: source.to_string(),
        file: file_path.to_string(),
        line: 1,
        end_line: total_lines,
        is_exported: true,
        language: Language::Markdown,
        references: Vec::new(),
        imports: Vec::new(),
    });

    if headings.is_empty() {
        let section_id = SymbolId {
            file: file_path.to_string(),
            name: document_name.clone(),
            byte_offset: 1,
        };
        let section_body = source.to_string();
        links.extend(extract_links(&section_id, file_path, &section_body));
        symbols.push(Symbol {
            id: section_id,
            kind: SymbolKind::Section,
            name: document_name,
            signature: "section".to_string(),
            body: section_body.clone(),
            file: file_path.to_string(),
            line: 1,
            end_line: total_lines,
            is_exported: true,
            language: Language::Markdown,
            references: extract_inline_code_references(&section_body),
            imports: Vec::new(),
        });
    } else {
        for (index, heading) in headings.iter().enumerate() {
            let start_line = heading.line;
            let end_line = headings
                .get(index + 1)
                .map(|next| next.line.saturating_sub(1).max(start_line))
                .unwrap_or(total_lines);

            let start_offset = heading.byte_offset;
            let end_offset = if end_line < total_lines {
                line_offsets.get(end_line).copied().unwrap_or(source.len())
            } else {
                source.len()
            };
            let section_body = source
                .get(start_offset..end_offset)
                .unwrap_or("")
                .to_string();
            let section_id = SymbolId {
                file: file_path.to_string(),
                name: heading.title.clone(),
                byte_offset: start_offset.saturating_add(1),
            };

            links.extend(extract_links(&section_id, file_path, &section_body));
            symbols.push(Symbol {
                id: section_id,
                kind: SymbolKind::Section,
                name: heading.title.clone(),
                signature: format!("section {}", heading.title),
                body: section_body.clone(),
                file: file_path.to_string(),
                line: start_line,
                end_line,
                is_exported: true,
                language: Language::Markdown,
                references: extract_inline_code_references(&section_body),
                imports: Vec::new(),
            });
        }
    }

    Ok(ParsedFile {
        file: file_path.to_string(),
        language: Language::Markdown,
        symbols,
        imports: Vec::new(),
        links,
    })
}

fn parse_heading_title(line: &str) -> Option<String> {
    let hash_count = line.chars().take_while(|ch| *ch == '#').count();
    if !(1..=6).contains(&hash_count) {
        return None;
    }

    let rest = line.get(hash_count..)?.trim();
    if rest.is_empty() {
        return None;
    }

    Some(rest.trim_end_matches('#').trim().to_string())
}

fn file_stem(file_path: &str) -> String {
    Path::new(file_path)
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or(file_path)
        .to_string()
}

fn line_offsets(source: &str) -> Vec<usize> {
    let mut offsets = vec![0];
    for (index, ch) in source.char_indices() {
        if ch == '\n' {
            offsets.push(index + 1);
        }
    }
    offsets
}

fn extract_links(from: &SymbolId, file_path: &str, body: &str) -> Vec<LinkInfo> {
    let mut links = extract_markdown_links(from, file_path, body);
    links.extend(extract_wiki_links(from, file_path, body));
    links
}

fn extract_markdown_links(from: &SymbolId, file_path: &str, body: &str) -> Vec<LinkInfo> {
    let mut links = Vec::new();
    let mut cursor = 0;

    while cursor < body.len() {
        let Some(start_rel) = body[cursor..].find('[') else {
            break;
        };
        let start = cursor + start_rel;
        let Some(label_end) = find_balanced_delimiter(body, start, '[', ']') else {
            cursor = start + 1;
            continue;
        };
        let target_start = label_end + 1;
        if body.as_bytes().get(target_start) != Some(&b'(') {
            cursor = target_start;
            continue;
        }
        let Some(target_end) = find_balanced_delimiter(body, target_start, '(', ')') else {
            cursor = target_start + 1;
            continue;
        };

        let label = body[start + 1..label_end].trim();
        let target = body[target_start + 1..target_end].trim();
        if let Some((resolved_target, heading)) = parse_link_target(file_path, target) {
            links.push(LinkInfo {
                from: from.clone(),
                target: resolved_target,
                heading,
                text: if label.is_empty() {
                    None
                } else {
                    Some(label.to_string())
                },
                is_wiki: false,
            });
        }

        cursor = target_end + 1;
    }

    links
}

fn find_balanced_delimiter(body: &str, start: usize, open: char, close: char) -> Option<usize> {
    let mut iter = body[start..].char_indices();
    let (_, first) = iter.next()?;
    if first != open {
        return None;
    }

    let mut depth = 1usize;
    let mut escaped = false;

    for (offset, ch) in iter {
        if escaped {
            escaped = false;
            continue;
        }

        if ch == '\\' {
            escaped = true;
            continue;
        }

        if ch == open {
            depth += 1;
            continue;
        }

        if ch == close {
            depth -= 1;
            if depth == 0 {
                return Some(start + offset);
            }
        }
    }

    None
}

fn extract_wiki_links(from: &SymbolId, file_path: &str, body: &str) -> Vec<LinkInfo> {
    let mut links = Vec::new();
    let mut cursor = 0;

    while cursor < body.len() {
        let Some(start_rel) = body[cursor..].find("[[") else {
            break;
        };
        let start = cursor + start_rel;
        let content_start = start + 2;
        let Some(end_rel) = body[content_start..].find("]]") else {
            break;
        };
        let end = content_start + end_rel;
        let raw = body[content_start..end].trim();

        let (target_part, text) = match raw.split_once('|') {
            Some((target, alias)) => (target.trim(), Some(alias.trim().to_string())),
            None => (raw, None),
        };

        if let Some((resolved_target, heading)) = parse_link_target(file_path, target_part) {
            links.push(LinkInfo {
                from: from.clone(),
                target: resolved_target,
                heading,
                text,
                is_wiki: true,
            });
        }

        cursor = end + 2;
    }

    links
}

fn parse_link_target(file_path: &str, raw_target: &str) -> Option<(String, Option<String>)> {
    let target = raw_target.trim();
    if target.is_empty()
        || target.starts_with("http://")
        || target.starts_with("https://")
        || target.starts_with("mailto:")
        || target.starts_with("tel:")
    {
        return None;
    }

    let (path_part, heading) = match target.split_once('#') {
        Some((path, heading)) => (path.trim(), Some(heading.trim().to_string())),
        None => (target, None),
    };

    let resolved_path = if path_part.is_empty() {
        file_path.to_string()
    } else {
        path_part.to_string()
    };

    Some((resolved_path, heading.filter(|value| !value.is_empty())))
}

fn extract_inline_code_references(body: &str) -> Vec<String> {
    let mut refs = Vec::new();
    let mut cursor = 0;

    while cursor < body.len() {
        let Some(start_rel) = body[cursor..].find('`') else {
            break;
        };
        let start = cursor + start_rel + 1;
        let Some(end_rel) = body[start..].find('`') else {
            break;
        };
        let end = start + end_rel;
        let candidate = body[start..end].trim();
        if looks_like_reference(candidate) {
            refs.push(candidate.to_string());
        }
        cursor = end + 1;
    }

    refs.sort();
    refs.dedup();
    refs
}

fn looks_like_reference(candidate: &str) -> bool {
    if candidate.is_empty() || candidate.contains('\n') || candidate.len() > 120 {
        return false;
    }

    let has_alpha = candidate.chars().any(|ch| ch.is_ascii_alphabetic());
    let allowed = candidate
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '/' | '.' | ':' | '#'));

    has_alpha && allowed
}
