#[cfg(test)]
mod tests;

use std::collections::HashMap;
use crate::symbols::Symbol;

/// The kind of change detected between two snapshots of symbols.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Removed,
    Modified,
}

/// A detected change to a symbol between two snapshots.
#[derive(Debug, Clone)]
pub struct SymbolChange {
    pub name: String,
    pub kind: ChangeKind,
    pub file: String,
}

/// Compare two slices of symbols and return a list of changes.
///
/// Symbols are matched by name. A symbol present only in `new` is `Added`,
/// only in `old` is `Removed`, and present in both but with a different body
/// is `Modified`.
pub fn diff_symbols(old: &[Symbol], new: &[Symbol]) -> Vec<SymbolChange> {
    let mut changes = Vec::new();

    // Build lookup maps by name
    let old_map: HashMap<&str, &Symbol> = old.iter().map(|s| (s.name.as_str(), s)).collect();
    let new_map: HashMap<&str, &Symbol> = new.iter().map(|s| (s.name.as_str(), s)).collect();

    // Detect removed and modified
    for (name, old_sym) in &old_map {
        match new_map.get(name) {
            None => {
                changes.push(SymbolChange {
                    name: name.to_string(),
                    kind: ChangeKind::Removed,
                    file: old_sym.file.clone(),
                });
            }
            Some(new_sym) => {
                if old_sym.body != new_sym.body {
                    changes.push(SymbolChange {
                        name: name.to_string(),
                        kind: ChangeKind::Modified,
                        file: new_sym.file.clone(),
                    });
                }
            }
        }
    }

    // Detect added
    for (name, new_sym) in &new_map {
        if !old_map.contains_key(name) {
            changes.push(SymbolChange {
                name: name.to_string(),
                kind: ChangeKind::Added,
                file: new_sym.file.clone(),
            });
        }
    }

    changes
}
