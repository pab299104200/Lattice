#[cfg(test)]
mod tests;

use std::path::Path;
use ignore::gitignore::{Gitignore, GitignoreBuilder};

/// Manages file exclusions from .lattice_ignore and default patterns.
pub struct SecurityFilter {
    gitignore: Option<Gitignore>,
    default_patterns: Vec<String>,
}

impl SecurityFilter {
    pub fn new(workspace_root: &Path) -> Self {
        let ignore_path = workspace_root.join(".lattice_ignore");
        let gitignore = if ignore_path.exists() {
            let mut builder = GitignoreBuilder::new(workspace_root);
            builder.add(&ignore_path);
            builder.build().ok()
        } else {
            None
        };

        let default_patterns = vec![
            "*.env*", "*credentials*", "*.pem", "*.key", "*.pfx",
            "*id_rsa*", "*id_ed25519*", "*.p12", "*.jks",
        ].into_iter().map(String::from).collect();

        Self { gitignore, default_patterns }
    }

    /// Check if a file should be excluded from indexing.
    pub fn is_excluded(&self, rel_path: &str) -> bool {
        // Check .lattice_ignore
        if let Some(gi) = &self.gitignore {
            if gi.matched(rel_path, false).is_ignore() {
                return true;
            }
        }

        // Check default patterns
        let filename = Path::new(rel_path).file_name()
            .and_then(|f| f.to_str())
            .unwrap_or("")
            .to_lowercase();

        for pattern in &self.default_patterns {
            let p = pattern.trim_matches('*').to_lowercase();
            if filename.contains(&p) {
                return true;
            }
        }

        false
    }

    /// Redact sensitive content patterns from source code.
    pub fn redact_content(source: &str) -> String {
        let mut result = source.to_string();
        let patterns = [
            "password=", "secret=", "api_key=", "token=",
            "aws_access_key_id=", "aws_secret_access_key=",
            "AKIA",  // AWS access key prefix
        ];

        for pattern in &patterns {
            if result.to_lowercase().find(&pattern.to_lowercase()).is_some() {
                // Redact the line containing the pattern
                let lines: Vec<&str> = result.lines().collect();
                let redacted_lines: Vec<String> = lines.iter().map(|line| {
                    if line.to_lowercase().contains(&pattern.to_lowercase()) {
                        "[REDACTED]".to_string()
                    } else {
                        line.to_string()
                    }
                }).collect();
                result = redacted_lines.join("\n");
            }
        }

        result
    }
}
