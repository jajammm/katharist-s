//! Largest and stale files, read from a finished (or cancelled) scan tree.

use std::time::{Duration, SystemTime};

use crate::scan::{EntryClass, NodeId, ScanTree};

/// Default threshold for "large file" in the GUI.
pub const LARGE_MIN: u64 = 100 * 1024 * 1024;

/// Every file node reachable from the root. Detached subtrees stay in
/// `nodes` after `remove_subtree`, so this walks `children`.
pub(crate) fn reachable_files(tree: &ScanTree) -> Vec<NodeId> {
    let mut out = Vec::new();
    let mut stack = vec![tree.root];
    while let Some(id) = stack.pop() {
        let node = &tree.nodes[id];
        match node.class {
            EntryClass::File => out.push(id),
            EntryClass::Dir => stack.extend(&node.children),
            EntryClass::Link => {}
        }
    }
    out
}

/// Files whose allocated size is at least `min_bytes`, biggest first.
pub fn large_files(tree: &ScanTree, min_bytes: u64) -> Vec<NodeId> {
    let mut ids: Vec<NodeId> = reachable_files(tree)
        .into_iter()
        .filter(|&id| tree.nodes[id].size >= min_bytes)
        .collect();
    ids.sort_by(|&a, &b| {
        let (a, b) = (&tree.nodes[a], &tree.nodes[b]);
        b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name))
    });
    ids
}

/// Files last modified before `now - older_than`, oldest first. An unknown
/// date is never stale.
pub fn stale_files(tree: &ScanTree, older_than: Duration, now: SystemTime) -> Vec<NodeId> {
    let Some(cutoff) = now.checked_sub(older_than) else {
        return Vec::new();
    };
    let mut ids: Vec<NodeId> = reachable_files(tree)
        .into_iter()
        .filter(|&id| tree.nodes[id].modified.is_some_and(|m| m < cutoff))
        .collect();
    ids.sort_by(|&a, &b| {
        let (a, b) = (&tree.nodes[a], &tree.nodes[b]);
        a.modified
            .cmp(&b.modified)
            .then_with(|| a.name.cmp(&b.name))
    });
    ids
}
