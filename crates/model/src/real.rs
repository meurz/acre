//! Completed scan tree: acre-core's [`DiskTree`] plus the indices required by the UI.
//!
//! - Each folder's children sorted by descending allocation (CSR offsets/counts keyed by core's `aux` folder ID).
//! - Each folder's dominant category for treemap colors.
//! - The category of each extension.
//!
//! Deletion and folder rescans clone the tree, apply changes, rebuild indices, and replace the snapshot,
//! so UI and background layout readers keep a stable tree. Cloning and indexing 4.8 million nodes takes a few hundred milliseconds.

use acre_core::tree::{DiskTree, Flags, NONE as CORE_NONE};
use acre_core::Removed;
use rayon::prelude::*;

use crate::ext::{CATEGORY_COUNT, Category, category_of};
use crate::read::{ExtStat, TreeRead};
use crate::tree::NONE;

const _: () = assert!(CORE_NONE == NONE);

pub struct RealTree {
    tree: DiskTree,
    generation: u64,
    /// Folder ID to starting offset in `items`.
    offsets: Vec<u32>,
    /// Folder ID to direct child count.
    counts: Vec<u32>,
    /// Children sorted by descending allocation, stored contiguously in breadth-first folder order.
    items: Vec<u32>,
    /// Folder ID to dominant category.
    dir_cats: Vec<u8>,
    /// Extension ID to category.
    ext_cats: Vec<Category>,
}

/// Index construction timing.
#[derive(Clone, Copy, Debug, Default)]
pub struct IndexStats {
    pub sort_us: u64,
    pub categories_us: u64,
}

impl RealTree {
    /// Build indices from scan results in parallel.
    pub fn new(tree: DiskTree, generation: u64) -> (Self, IndexStats) {
        let mut t = RealTree {
            tree,
            generation,
            offsets: Vec::new(),
            counts: Vec::new(),
            items: Vec::new(),
            dir_cats: Vec::new(),
            ext_cats: Vec::new(),
        };
        let stats = t.rebuild();
        (t, stats)
    }

    pub fn disk_tree(&self) -> &DiskTree {
        &self.tree
    }

    /// Return a new tree with the entire `id` subtree removed, after moving it to the Recycle Bin.
    pub fn with_removed(&self, id: u32) -> (RealTree, Removed) {
        let mut tree = self.tree.clone();
        let r = tree.remove(id);
        (RealTree::new(tree, self.generation + 1).0, r)
    }

    /// Replace the contents of folder `id` with rescan results.
    pub fn with_replaced(&self, id: u32, fresh: &DiskTree) -> RealTree {
        let mut tree = self.tree.clone();
        tree.replace_subtree(id, fresh);
        RealTree::new(tree, self.generation + 1).0
    }

    fn rebuild(&mut self) -> IndexStats {
        let t0 = std::time::Instant::now();
        let tree = &self.tree;
        let nodes = tree.nodes();
        let root = tree.root();
        // Scan the node array twice for locality: count each folder's children, then fill positions using prefix sums.
        // Core assigns folder IDs (`aux`) breadth-first during finalization; subtree replacements append new folders.
        // A larger ID therefore cannot be an ancestor of a smaller one, so reverse order visits children before parents.
        // Unreachable nodes and parent cycles receive no folder ID (core counts them in summary.unreachable, normally zero).
        // If any exist, traverse from the root to mark reachable nodes and skip the rest.
        let reach: Option<Vec<bool>> = (tree.summary().unreachable > 0).then(|| {
            let mut r = vec![false; nodes.len()];
            let mut stack = vec![root];
            r[root as usize] = true;
            while let Some(d) = stack.pop() {
                let mut c = nodes[d as usize].first_child;
                while c != NONE {
                    if !r[c as usize] {
                        r[c as usize] = true;
                        if nodes[c as usize].flags.is_dir() {
                            stack.push(c);
                        }
                    }
                    c = nodes[c as usize].next_sibling;
                }
            }
            r
        });
        let live = |i: usize| nodes[i].is_used() && reach.as_ref().is_none_or(|r| r[i]);
        let slot_cap = (0..nodes.len())
            .filter(|&i| live(i) && nodes[i].flags.is_dir())
            .map(|i| nodes[i].aux as usize + 1)
            .max()
            .unwrap_or(1);
        let mut counts = vec![0u32; slot_cap];
        let mut dir_node = vec![NONE; slot_cap];
        for (i, n) in nodes.iter().enumerate() {
            if !live(i) {
                continue;
            }
            if n.flags.is_dir() {
                dir_node[n.aux as usize] = i as u32;
            }
            if i as u32 != root && n.parent != NONE {
                counts[nodes[n.parent as usize].aux as usize] += 1;
            }
        }
        let mut offsets = vec![0u32; slot_cap];
        let mut acc_off = 0u32;
        for (o, &c) in offsets.iter_mut().zip(&counts) {
            *o = acc_off;
            acc_off += c;
        }
        let mut items = vec![0u32; acc_off as usize];
        {
            let mut cursor = offsets.clone();
            for (i, n) in nodes.iter().enumerate() {
                if live(i) && i as u32 != root && n.parent != NONE {
                    let ps = nodes[n.parent as usize].aux as usize;
                    items[cursor[ps] as usize] = i as u32;
                    cursor[ps] += 1;
                }
            }
        }
        // Sort each folder's children by descending allocation in parallel.
        let mut slices: Vec<&mut [u32]> = Vec::with_capacity(slot_cap);
        {
            let mut rest: &mut [u32] = &mut items;
            for &c in &counts {
                let (a, b) = std::mem::take(&mut rest).split_at_mut(c as usize);
                slices.push(a);
                rest = b;
            }
        }
        slices.par_iter_mut().for_each(|s| {
            if s.len() > 1 {
                s.sort_unstable_by(|&a, &b| nodes[b as usize].alloc.cmp(&nodes[a as usize].alloc).then(a.cmp(&b)));
            }
        });
        let sort_us = t0.elapsed().as_micros() as u64;

        // Compute direct-file category totals in parallel, then propagate subfolders' dominant categories in reverse order.
        let t1 = std::time::Instant::now();
        let ext_cats: Vec<Category> = tree.extensions().iter().map(|e| category_of(&e.name)).collect();
        let mut acc: Vec<[f32; CATEGORY_COUNT]> = slices
            .par_iter()
            .map(|s| {
                let mut a = [0f32; CATEGORY_COUNT];
                for &c in s.iter() {
                    let cn = &nodes[c as usize];
                    if !cn.flags.is_dir() {
                        let cat = ext_cats.get(cn.aux as usize).copied().unwrap_or(Category::Other);
                        a[cat as usize] += cn.alloc.max(1) as f32;
                    }
                }
                a
            })
            .collect();
        drop(slices);
        let mut dir_cats = vec![Category::Other as u8; slot_cap];
        for slot in (0..slot_cap).rev() {
            let d = dir_node[slot];
            if d == NONE {
                continue;
            }
            let a = acc[slot];
            let mut best = Category::Other as usize;
            let mut bv = 0.0f32;
            for (i, &v) in a.iter().enumerate() {
                if v > bv {
                    bv = v;
                    best = i;
                }
            }
            dir_cats[slot] = best as u8;
            let n = &nodes[d as usize];
            if d != root && n.parent != NONE {
                let ps = nodes[n.parent as usize].aux as usize;
                acc[ps][best] += n.alloc.max(1) as f32;
            }
        }
        let categories_us = t1.elapsed().as_micros() as u64;
        self.offsets = offsets;
        self.counts = counts;
        self.items = items;
        self.dir_cats = dir_cats;
        self.ext_cats = ext_cats;
        IndexStats { sort_us, categories_us }
    }

    #[inline]
    fn slot(&self, id: u32) -> usize {
        self.tree.node(id).aux as usize
    }
}

impl TreeRead for RealTree {
    fn root(&self) -> u32 {
        self.tree.root()
    }
    fn generation(&self) -> u64 {
        self.generation
    }
    fn complete(&self) -> bool {
        true
    }
    fn slot_count(&self) -> u32 {
        self.tree.slots() as u32
    }
    fn dir_count(&self) -> u64 {
        self.tree.summary().dirs
    }
    fn file_count(&self) -> u64 {
        self.tree.summary().files
    }
    fn contains(&self, id: u32) -> bool {
        (id as usize) < self.tree.slots() && self.tree.node(id).is_used()
    }
    #[inline]
    fn is_dir(&self, id: u32) -> bool {
        self.tree.node(id).flags.is_dir()
    }
    #[inline]
    fn size(&self, id: u32) -> u64 {
        self.tree.node(id).alloc
    }
    fn files(&self, id: u32) -> u64 {
        if self.is_dir(id) { self.tree.dir_totals(id).files as u64 } else { 1 }
    }
    fn child_count(&self, id: u32) -> u32 {
        if self.is_dir(id) { self.counts.get(self.slot(id)).copied().unwrap_or(0) } else { 0 }
    }
    fn name(&self, id: u32) -> &str {
        self.tree.name(id)
    }
    #[inline]
    fn parent(&self, id: u32) -> u32 {
        self.tree.node(id).parent
    }
    #[inline]
    fn first_child(&self, id: u32) -> u32 {
        self.tree.node(id).first_child
    }
    #[inline]
    fn next_sibling(&self, id: u32) -> u32 {
        self.tree.node(id).next_sibling
    }
    fn sorted_children_slice(&self, id: u32) -> Option<&[u32]> {
        if !self.is_dir(id) {
            return Some(&[]);
        }
        let s = self.slot(id);
        let (o, n) = (*self.offsets.get(s)? as usize, *self.counts.get(s)? as usize);
        self.items.get(o..o + n)
    }
    fn has_sorted_index(&self) -> bool {
        true
    }
    #[inline]
    fn ext_of(&self, id: u32) -> u32 {
        let n = self.tree.node(id);
        if n.flags.is_dir() { NONE } else { n.aux }
    }
    fn ext_count(&self) -> usize {
        self.tree.extensions().len()
    }
    fn ext_name(&self, ext: u32) -> &str {
        self.tree.extensions().get(ext as usize).map_or("", |e| &e.name)
    }
    #[inline]
    fn ext_category(&self, ext: u32) -> Category {
        self.ext_cats.get(ext as usize).copied().unwrap_or(Category::Other)
    }
    fn ext_stat(&self, ext: u32) -> ExtStat {
        self.tree
            .extensions()
            .get(ext as usize)
            .map(|e| ExtStat { files: e.files, bytes: e.alloc })
            .unwrap_or_default()
    }
    #[inline]
    fn dir_category(&self, id: u32) -> Option<u8> {
        if self.is_dir(id) { self.dir_cats.get(self.slot(id)).copied() } else { None }
    }
    fn is_special(&self, id: u32) -> bool {
        self.tree.node(id).flags.contains(Flags::METAFILE)
    }
    fn is_unreadable(&self, id: u32) -> bool {
        self.tree.node(id).flags.contains(Flags::ERROR)
    }
    fn memory_bytes(&self) -> usize {
        self.tree.memory_bytes()
            + (self.offsets.capacity() + self.counts.capacity() + self.items.capacity()) * 4
            + self.dir_cats.capacity()
            + self.ext_cats.capacity()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use acre_core::tree::TreeBuilder as CoreBuilder;

    fn sample() -> DiskTree {
        let mut b = CoreBuilder::new("C:\\");
        let r = b.root();
        let win = b.add(r, "Windows", Flags::DIR, 0, 0);
        b.add(win, "a.dll", Flags::default(), 100, 4096);
        b.add(win, "big.dll", Flags::default(), 100, 65536);
        let users = b.add(r, "Users", Flags::DIR, 0, 0);
        b.add(users, "movie.mkv", Flags::default(), 1 << 20, 1 << 20);
        b.add(users, "note.txt", Flags::default(), 10, 4096);
        b.add(r, "pagefile.sys", Flags::SYSTEM, 1 << 12, 8192);
        b.finish()
    }

    #[test]
    fn index_sorted_children_counts_and_categories() {
        let (t, _) = RealTree::new(sample(), 1);
        let t = &t as &dyn TreeRead;
        let root = t.root();
        let kids: Vec<&str> = t.sorted_children_slice(root).unwrap().iter().map(|&c| t.name(c)).collect();
        assert_eq!(kids, ["Users", "Windows", "pagefile.sys"]);
        let users = t.find_path("C:\\Users").unwrap();
        assert_eq!(t.child_count(users), 2);
        assert_eq!(t.files(users), 2);
        assert_eq!(t.size(users), (1 << 20) + 4096);
        assert_eq!(t.dir_category(users), Some(Category::Video as u8));
        let win = t.find_path("C:\\Windows").unwrap();
        assert_eq!(t.dir_category(win), Some(Category::Program as u8));
        assert_eq!(t.path(t.find_path("C:\\Windows\\big.dll").unwrap()), "C:\\Windows\\big.dll");
        let dll = (0..t.ext_count() as u32).find(|&e| t.ext_name(e) == "dll").unwrap();
        assert_eq!(t.ext_stat(dll).files, 2);
        assert_eq!(t.ext_category(dll), Category::Program);
    }

    #[test]
    fn removed_and_replaced_trees_are_reindexed() {
        let (t, _) = RealTree::new(sample(), 1);
        let users = (&t as &dyn TreeRead).find_path("C:\\Users").unwrap();
        let (t2, r) = t.with_removed(users);
        assert_eq!(r.files, 2);
        let v = &t2 as &dyn TreeRead;
        assert!(!v.contains(users));
        assert_eq!(v.generation(), 2);
        let kids: Vec<&str> = v.sorted_children_slice(v.root()).unwrap().iter().map(|&c| v.name(c)).collect();
        assert_eq!(kids, ["Windows", "pagefile.sys"]);
        assert_eq!(v.file_count(), 3);
        // The original tree is unchanged because the UI can still be reading it.
        assert!((&t as &dyn TreeRead).contains(users));

        let mut b = CoreBuilder::new("C:\\Windows");
        let r = b.root();
        b.add(r, "huge.iso", Flags::default(), 1 << 22, 1 << 22);
        let win = v.find_path("C:\\Windows").unwrap();
        let t3 = t2.with_replaced(win, &b.finish());
        let v3 = &t3 as &dyn TreeRead;
        let w = v3.find_path("C:\\Windows").unwrap();
        assert_eq!(v3.child_count(w), 1);
        assert_eq!(v3.size(v3.root()), (1 << 22) + 8192);
        assert_eq!(v3.dir_category(w), Some(Category::Archive as u8));
        let first = v3.sorted_children_slice(v3.root()).unwrap()[0];
        assert_eq!(v3.name(first), "Windows");
    }
}
