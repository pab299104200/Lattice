pub mod workspace;

#[cfg(test)]
mod tests;

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::path::{Path, PathBuf};

/// Manages file exclusions from `.gitignore`, `.lattice_ignore`, `.latticeignore`,
/// and default patterns.
pub struct SecurityFilter {
    gitignore: Option<Gitignore>,
    root: PathBuf,
    default_patterns: Vec<String>,
    excluded_dirs: Vec<String>,
}

impl SecurityFilter {
    pub fn new(workspace_root: &Path) -> Self {
        let mut builder = GitignoreBuilder::new(workspace_root);

        // Load .gitignore if it exists
        let gitignore_path = workspace_root.join(".gitignore");
        if std::fs::symlink_metadata(&gitignore_path).is_ok_and(|m| m.is_file()) {
            builder.add(&gitignore_path);
        }

        // Load the canonical ignore file plus the no-underscore alias that some
        // plans and operators already use in the wild.
        for path in [
            workspace_root.join(".lattice_ignore"),
            workspace_root.join(".latticeignore"),
        ] {
            if std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_file()) {
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
            ".codex-home",
            ".agents",
            ".playwright-mcp",
            ".pytest_cache",
            ".ruff_cache",
            "worktrees",
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
            root: workspace_root.to_path_buf(),
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
        let mut ignored = false;
        // Check .gitignore + Lattice ignore files.
        if let Some(gi) = &self.gitignore {
            ignored = gi.matched_path_or_any_parents(rel_path, false).is_ignore();
        }

        // Apply each ancestor's local ignore rules. This also covers direct
        // watcher and verification reads that do not go through a walker.
        let relative = Path::new(rel_path);
        if relative.is_absolute()
            || relative
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
        {
            return true;
        }
        let mut ancestors = relative.ancestors().skip(1).collect::<Vec<_>>();
        ancestors.reverse();
        for ancestor in ancestors {
            if ancestor.as_os_str().is_empty() {
                continue;
            }
            let directory = self.root.join(ancestor);
            if std::fs::symlink_metadata(&directory).is_ok_and(|m| m.file_type().is_symlink()) {
                return true;
            }
            let mut builder = GitignoreBuilder::new(&directory);
            for name in [".gitignore", ".lattice_ignore", ".latticeignore"] {
                let path = directory.join(name);
                if std::fs::symlink_metadata(&path)
                    .is_ok_and(|meta| meta.is_file() && !meta.file_type().is_symlink())
                {
                    builder.add(path);
                }
            }
            if let Ok(rules) = builder.build() {
                let nested_relative = self
                    .root
                    .join(relative)
                    .strip_prefix(&directory)
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|_| relative.to_path_buf());
                let matched = rules.matched_path_or_any_parents(nested_relative, false);
                if matched.is_ignore() {
                    ignored = true;
                }
                if matched.is_whitelist() {
                    ignored = false;
                }
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

        ignored
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
