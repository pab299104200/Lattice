use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashSet};

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct IndexEntry {
    pub path: String,
    pub priority: IndexPriority,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum IndexPriority {
    OpenInEditor = 5, // Highest
    SameDirectory = 4,
    DirectImport = 3,
    RecentlyModified = 2,
    Background = 1, // Lowest
}

impl Ord for IndexEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.priority as u8).cmp(&(other.priority as u8))
    }
}

impl PartialOrd for IndexEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Priority queue for lazy indexing of large repos.
pub struct LazyIndexQueue {
    queue: BinaryHeap<IndexEntry>,
    indexed: HashSet<String>,
}

impl LazyIndexQueue {
    pub fn new() -> Self {
        Self {
            queue: BinaryHeap::new(),
            indexed: HashSet::new(),
        }
    }

    pub fn enqueue(&mut self, path: String, priority: IndexPriority) {
        if !self.indexed.contains(&path) {
            self.queue.push(IndexEntry { path, priority });
        }
    }

    pub fn dequeue(&mut self) -> Option<IndexEntry> {
        while let Some(entry) = self.queue.pop() {
            if !self.indexed.contains(&entry.path) {
                self.indexed.insert(entry.path.clone());
                return Some(entry);
            }
        }
        None
    }

    pub fn boost_priority(&mut self, path: &str, new_priority: IndexPriority) {
        // Re-add with higher priority (old entry will be skipped by dequeue)
        if !self.indexed.contains(path) {
            self.queue.push(IndexEntry {
                path: path.to_string(),
                priority: new_priority,
            });
        }
    }

    pub fn is_indexed(&self, path: &str) -> bool {
        self.indexed.contains(path)
    }

    pub fn indexed_count(&self) -> usize {
        self.indexed.len()
    }

    pub fn pending_count(&self) -> usize {
        self.queue.len()
    }

    pub fn progress(&self) -> (usize, usize) {
        (self.indexed.len(), self.indexed.len() + self.queue.len())
    }
}
