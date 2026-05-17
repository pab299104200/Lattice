#[cfg(test)]
mod tests;

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::path::Path;

/// Manages file exclusions from `.gitignore`, `.lattice_ignore`, `.latticeignore`,
/// and default patterns.
pub struct SecurityFilter {
    gitignore: Option<Gitignore>,
    default_patterns: Vec<String>,
    excluded_dirs: Vec<String>,
}

impl SecurityFilter {
    pub fn new(workspace_root: &Path) -> Self {
        let mut builder = GitignoreBuilder::new(workspace_root);

        // Load .gitignore if it exists
        let gitignore_path = workspace_root.join(".gitignore");
        if gitignore_path.exists() {
            builder.add(&gitignore_path);
        }

        // Load the canonical ignore file plus the no-underscore alias that some
        // plans and operators already use in the wild.
        for path in [
            workspace_root.join(".lattice_ignore"),
            workspace_root.join(".latticeignore"),
        ] {
            if path.exists() {
                builder.add(&path);
            }
        }

        let gitignore = builder.build().ok();

        let default_patterns = vec![
            "*.env*",
            "*credentials*",
            "*.pem",
            "*.key",
            "*.pfx",
            "*id_rsa*",
            "*id_ed25519*",
            "*.p12",
            "*.jks",
        ]
        .into_iter()
        .map(String::from)
        .collect();

        let excluded_dirs = vec![
            "node_modules",
            ".git",
            "target",
            "dist",
            "build",
            "out",
            "__pycache__",
            ".venv",
            "venv",
            ".tox",
            ".mypy_cache",
            ".next",
            ".nuxt",
            ".svelte-kit",
            "coverage",
            ".lattice",
            ".claude",
            ".codex",
            "lib",
            "lib64",
            ".eggs",
            "vendor",
            "third_party",
            "site-packages",
            "bower_components",
            ".cargo",
            ".gradle",
        ]
        .into_iter()
        .map(String::from)
        .collect();

        Self {
            gitignore,
            default_patterns,
            excluded_dirs,
        }
    }

    /// Check if a directory name should be skipped during traversal.
    pub fn is_excluded_dir(&self, dir_name: &str) -> bool {
        self.excluded_dirs.iter().any(|d| d == dir_name)
    }

    /// Check if a file should be excluded from indexing.
    /// Combines `.gitignore`, `.lattice_ignore`, `.latticeignore`, excluded dirs,
    /// and default security patterns.
    pub fn is_excluded(&self, rel_path: &str) -> bool {
        // Check .gitignore + Lattice ignore files.
        if let Some(gi) = &self.gitignore {
            if gi.matched(rel_path, false).is_ignore() {
                return true;
            }
        }

        // Check excluded directory components
        let normalized = rel_path.replace('\\', "/");
        for component in normalized.split('/') {
            if self.excluded_dirs.iter().any(|d| d == component) {
                return true;
            }
        }

        // Check default security patterns
        let filename = Path::new(rel_path)
            .file_name()
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
            "password=",
            "secret=",
            "api_key=",
            "token=",
            "aws_access_key_id=",
            "aws_secret_access_key=",
            "AKIA", // AWS access key prefix
        ];

        for pattern in &patterns {
            if result
                .to_lowercase()
                .find(&pattern.to_lowercase())
                .is_some()
            {
                // Redact the line containing the pattern
                let lines: Vec<&str> = result.lines().collect();
                let redacted_lines: Vec<String> = lines
                    .iter()
                    .map(|line| {
                        if line.to_lowercase().contains(&pattern.to_lowercase()) {
                            "[REDACTED]".to_string()
                        } else {
                            line.to_string()
                        }
                    })
                    .collect();
                result = redacted_lines.join("\n");
            }
        }

        result
    }
}
