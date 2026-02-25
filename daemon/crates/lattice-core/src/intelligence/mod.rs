use std::collections::HashMap;
use crate::diff::{ChangeKind, SymbolChange};

/// Tracks symbol-level changes during a coding session to detect patterns
/// such as hotspots (frequently edited symbols) and anti-patterns
/// (thrashing, dead ends).
pub struct ChangeTracker {
    /// Number of times each symbol has been edited.
    edit_counts: HashMap<String, u32>,
    /// Chronological record of (symbol_name, change_kind, timestamp).
    session_changes: Vec<(String, String, u64)>,
}

impl ChangeTracker {
    pub fn new() -> Self {
        Self {
            edit_counts: HashMap::new(),
            session_changes: Vec::new(),
        }
    }

    /// Record a symbol change. Increments the edit count for the symbol
    /// and appends to the session change log.
    pub fn record_change(&mut self, change: &SymbolChange) {
        let count = self.edit_counts.entry(change.name.clone()).or_insert(0);
        *count += 1;

        let kind_str = match change.kind {
            ChangeKind::Added => "added",
            ChangeKind::Removed => "removed",
            ChangeKind::Modified => "modified",
        };

        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        self.session_changes.push((
            change.name.clone(),
            kind_str.to_string(),
            timestamp,
        ));
    }

    /// Return the hotspot score (edit count) for a symbol.
    /// A higher score means the symbol has been edited more frequently.
    pub fn get_hotspot_score(&self, symbol_name: &str) -> u32 {
        self.edit_counts.get(symbol_name).copied().unwrap_or(0)
    }

    /// Detect symbols that are being "thrashed" — edited 5 or more times
    /// in a single session, suggesting instability or uncertainty.
    pub fn detect_thrashing(&self) -> Vec<String> {
        self.edit_counts
            .iter()
            .filter(|(_, &count)| count >= 5)
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Detect "dead ends" — symbols that were added and then later removed
    /// within the same session, suggesting abandoned approaches.
    pub fn detect_dead_ends(&self) -> Vec<String> {
        let mut added: HashMap<String, bool> = HashMap::new();
        let mut dead_ends = Vec::new();

        for (name, kind, _) in &self.session_changes {
            match kind.as_str() {
                "added" => {
                    added.insert(name.clone(), true);
                }
                "removed" => {
                    if added.get(name).copied().unwrap_or(false) {
                        dead_ends.push(name.clone());
                    }
                }
                _ => {}
            }
        }

        dead_ends
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{ChangeKind, SymbolChange};

    fn make_change(name: &str, kind: ChangeKind) -> SymbolChange {
        SymbolChange {
            name: name.to_string(),
            kind,
            file: "src/test.ts".to_string(),
        }
    }

    #[test]
    fn test_hotspot_score() {
        let mut tracker = ChangeTracker::new();

        let change = make_change("loginUser", ChangeKind::Modified);
        tracker.record_change(&change);
        tracker.record_change(&change);
        tracker.record_change(&change);

        assert_eq!(tracker.get_hotspot_score("loginUser"), 3);
        assert_eq!(tracker.get_hotspot_score("nonExistent"), 0);
    }

    #[test]
    fn test_detect_thrashing() {
        let mut tracker = ChangeTracker::new();

        let change = make_change("unstableFunc", ChangeKind::Modified);
        for _ in 0..5 {
            tracker.record_change(&change);
        }

        let stable_change = make_change("stableFunc", ChangeKind::Modified);
        tracker.record_change(&stable_change);
        tracker.record_change(&stable_change);

        let thrashing = tracker.detect_thrashing();
        assert_eq!(thrashing.len(), 1);
        assert!(thrashing.contains(&"unstableFunc".to_string()));
    }

    #[test]
    fn test_detect_dead_ends() {
        let mut tracker = ChangeTracker::new();

        // Add a symbol then remove it — dead end
        tracker.record_change(&make_change("tempHelper", ChangeKind::Added));
        tracker.record_change(&make_change("tempHelper", ChangeKind::Removed));

        // Add a symbol and keep it — not a dead end
        tracker.record_change(&make_change("keepThis", ChangeKind::Added));

        let dead_ends = tracker.detect_dead_ends();
        assert_eq!(dead_ends.len(), 1);
        assert!(dead_ends.contains(&"tempHelper".to_string()));
    }
}
