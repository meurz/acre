//! The UI's sole interface for reading trees.
//!
//! Implemented by [`crate::tree::Snapshot`] for synthetic data and incremental directory walks, and
//! [`crate::real::RealTree`] for completed acre-core trees with UI indices.
//! Treemap layout, tree rows, extension statistics, and tooltips all use this interface.

use crate::ext::Category;
use crate::tree::NONE;

/// Aggregated extension statistics.
#[derive(Clone, Copy, Debug, Default)]
pub struct ExtStat {
    pub files: u64,
    pub bytes: u64,
}

/// Immutable tree. Node IDs are `u32`, and [`NONE`] denotes a missing node.
pub trait TreeRead: Send + Sync {
    /// Root node.
    fn root(&self) -> u32;
    /// Snapshot generation, used in treemap cache keys.
    fn generation(&self) -> u64;
    /// Whether scanning has finished.
    fn complete(&self) -> bool;
    /// Number of node slots; all node IDs are smaller than this.
    fn slot_count(&self) -> u32;
    /// Folder count, including the root.
    fn dir_count(&self) -> u64;
    /// File count.
    fn file_count(&self) -> u64;
    /// Whether a node is still in the tree; deletion and rescanning can invalidate old IDs.
    fn contains(&self, id: u32) -> bool;
    fn is_dir(&self, id: u32) -> bool;
    /// Allocated size: the file's allocation or a folder's subtree total.
    fn size(&self, id: u32) -> u64;
    /// Subtree file count; a file counts as one.
    fn files(&self, id: u32) -> u64;
    fn child_count(&self, id: u32) -> u32;
    fn name(&self, id: u32) -> &str;
    fn parent(&self, id: u32) -> u32;
    fn first_child(&self, id: u32) -> u32;
    fn next_sibling(&self, id: u32) -> u32;
    /// Direct children sorted by descending allocation, when an index is available.
    fn sorted_children_slice(&self, id: u32) -> Option<&[u32]>;
    fn has_sorted_index(&self) -> bool;
    /// File extension index; [`NONE`] for folders.
    fn ext_of(&self, id: u32) -> u32;
    fn ext_count(&self) -> usize;
    fn ext_name(&self, ext: u32) -> &str;
    fn ext_category(&self, ext: u32) -> Category;
    fn ext_stat(&self, ext: u32) -> ExtStat;
    /// Dominant folder category, available after scanning.
    fn dir_category(&self, id: u32) -> Option<u8>;
    /// System metadata and synthetic nodes (such as `$MFT` and orphan buckets) cannot be deleted or opened in Explorer.
    fn is_special(&self, id: u32) -> bool;
    /// Unreadable folder due to insufficient permissions.
    fn is_unreadable(&self, id: u32) -> bool;
    /// Memory used by the tree itself, in bytes.
    fn memory_bytes(&self) -> usize;
}

/// Child linked-list iterator, in list order.
pub struct Children<'a> {
    tree: &'a dyn TreeRead,
    cur: u32,
}

impl Iterator for Children<'_> {
    type Item = u32;
    #[inline]
    fn next(&mut self) -> Option<u32> {
        if self.cur == NONE {
            return None;
        }
        let id = self.cur;
        self.cur = self.tree.next_sibling(id);
        Some(id)
    }
}

impl dyn TreeRead + '_ {
    pub fn children(&self, id: u32) -> Children<'_> {
        Children { tree: self, cur: if self.is_dir(id) { self.first_child(id) } else { NONE } }
    }
    /// Children by descending size, breaking ties by node ID. Copy the index slice or sort on demand.
    pub fn sorted_children(&self, id: u32) -> Vec<u32> {
        if let Some(s) = self.sorted_children_slice(id) {
            return s.to_vec();
        }
        let mut v: Vec<(u64, u32)> = self.children(id).map(|c| (self.size(c), c)).collect();
        v.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        v.into_iter().map(|(_, c)| c).collect()
    }
    /// Node color category: extension-based for files, Other for folders.
    pub fn category(&self, id: u32) -> Category {
        let e = self.ext_of(id);
        if e == NONE { Category::Other } else { self.ext_category(e) }
    }
    pub fn depth(&self, mut id: u32) -> u32 {
        let mut d = 0;
        while self.parent(id) != NONE {
            id = self.parent(id);
            d += 1;
        }
        d
    }
    /// Node chain from the root through this node, inclusive.
    pub fn ancestry(&self, id: u32) -> Vec<u32> {
        let mut v = Vec::new();
        let mut cur = id;
        while cur != NONE {
            v.push(cur);
            cur = self.parent(cur);
        }
        v.reverse();
        v
    }
    /// Full path: the root name (such as `C:\` or `D:\data`) followed by each component.
    pub fn path(&self, id: u32) -> String {
        let mut s = String::new();
        for (i, n) in self.ancestry(id).into_iter().enumerate() {
            let name = self.name(n);
            if i > 0 && !s.ends_with('\\') {
                s.push('\\');
            }
            s.push_str(name);
        }
        s
    }
    /// Find a node by its full path, case-insensitively.
    pub fn find_path(&self, path: &str) -> Option<u32> {
        let root = self.root();
        let rname = self.name(root).trim_end_matches('\\');
        let p = path.trim_end_matches('\\');
        if p.len() < rname.len() || !p.is_char_boundary(rname.len()) || !p[..rname.len()].eq_ignore_ascii_case(rname) {
            return None;
        }
        let rest = &p[rname.len()..];
        if !rest.is_empty() && !rest.starts_with('\\') {
            return None;
        }
        let mut cur = root;
        for part in rest.split('\\').filter(|s| !s.is_empty()) {
            cur = self.children(cur).find(|&c| names_eq(self.name(c), part))?;
        }
        Some(cur)
    }
    /// Folder with the most children, for tests.
    pub fn biggest_dir(&self) -> Option<u32> {
        let mut best = (NONE, 0u32);
        let mut stack = vec![self.root()];
        while let Some(d) = stack.pop() {
            let n = self.child_count(d);
            if n > best.1 {
                best = (d, n);
            }
            for c in self.children(d) {
                if self.is_dir(c) {
                    stack.push(c);
                }
            }
        }
        (best.0 != NONE).then_some(best.0)
    }
}

fn names_eq(a: &str, b: &str) -> bool {
    if a.is_ascii() && b.is_ascii() {
        a.eq_ignore_ascii_case(b)
    } else {
        a.chars().flat_map(char::to_lowercase).eq(b.chars().flat_map(char::to_lowercase))
    }
}
