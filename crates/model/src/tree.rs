//! Compact directory tree: a node array with parent/sibling/first-child indices and pooled names.
//!
//! Storage uses fixed-size `Arc` chunks: publishing snapshots copies only chunk pointers. A later
//! write to a shared chunk copies that chunk on demand. Snapshots published every 100-250 ms therefore
//! cost roughly the same regardless of tree size; the UI receives a complete, immutable, lock-free tree.
//!
//! Nodes are immutable after creation (parent, sibling link, file size, name, and extension). Only directory
//! aggregates change (total size, file count, child count, and first child); they live in a separate array
//! with roughly one tenth as many entries, keeping copy-on-write costs low.

use std::collections::HashMap;
use std::sync::Arc;

use crate::ext::{Category, category_of};

/// Sentinel for a missing index.
pub const NONE: u32 = u32::MAX;
/// Extension sentinel for directories.
pub const EXT_DIR: u16 = u16::MAX;

/// A file or directory; immutable after creation.
#[derive(Clone, Copy, Debug, Default)]
#[repr(C)]
pub struct Node {
    pub parent: u32,
    /// Next child in the parent's linked list; new children are prepended.
    pub next_sibling: u32,
    /// File allocation in bytes; zero for directories, whose size is [`Dir::total`].
    pub size: u64,
    /// Name offset in the string pool.
    pub name: u32,
    pub name_len: u16,
    /// File extension index; [`EXT_DIR`] for directories.
    pub ext: u16,
    /// Index in the directory array; [`NONE`] for files.
    pub dir: u32,
}

/// Directory aggregates, updated during scanning.
#[derive(Clone, Copy, Debug, Default)]
#[repr(C)]
pub struct Dir {
    pub node: u32,
    pub first_child: u32,
    pub child_count: u32,
    /// Recursive file count.
    pub files: u32,
    /// Recursive allocated size.
    pub total: u64,
}

/// Append-only array shared in chunks.
#[derive(Clone)]
pub struct Chunked<T: Copy> {
    chunks: Vec<Arc<Vec<T>>>,
    len: u32,
    bits: u32,
}

impl<T: Copy> Chunked<T> {
    pub fn new(bits: u32) -> Self {
        Self { chunks: Vec::new(), len: 0, bits }
    }
    #[inline]
    pub fn len(&self) -> u32 {
        self.len
    }
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    #[inline]
    fn cap(&self) -> usize {
        1usize << self.bits
    }
    /// Get a writable chunk, cloning shared chunks with their full capacity to avoid repeated growth.
    #[inline]
    fn chunk_mut(arc: &mut Arc<Vec<T>>, cap: usize) -> &mut Vec<T> {
        if Arc::get_mut(arc).is_none() {
            let mut fresh = Vec::with_capacity(cap);
            fresh.extend_from_slice(arc);
            *arc = Arc::new(fresh);
        }
        Arc::get_mut(arc).expect("unique chunk")
    }
    pub fn push(&mut self, value: T) -> u32 {
        let cap = self.cap();
        if self.len as usize == self.chunks.len() * cap {
            self.chunks.push(Arc::new(Vec::with_capacity(cap)));
        }
        let last = self.chunks.last_mut().expect("chunk");
        Self::chunk_mut(last, cap).push(value);
        self.len += 1;
        self.len - 1
    }
    #[inline]
    pub fn get(&self, i: u32) -> &T {
        let mask = (1u32 << self.bits) - 1;
        &self.chunks[(i >> self.bits) as usize][(i & mask) as usize]
    }
    #[inline]
    pub fn get_mut(&mut self, i: u32) -> &mut T {
        let mask = (1u32 << self.bits) - 1;
        let cap = self.cap();
        let chunk = &mut self.chunks[(i >> self.bits) as usize];
        &mut Self::chunk_mut(chunk, cap)[(i & mask) as usize]
    }
    /// Allocated bytes, based on chunk capacity.
    pub fn allocated_bytes(&self) -> usize {
        self.chunks.iter().map(|c| c.capacity()).sum::<usize>() * std::mem::size_of::<T>()
    }
}

const NAME_CHUNK_BITS: u32 = 20;
const NAME_CHUNK: usize = 1 << NAME_CHUNK_BITS;

/// Append-only name pool shared in 1 MiB chunks.
#[derive(Clone, Default)]
pub struct Names {
    chunks: Vec<Arc<Vec<u8>>>,
}

impl Names {
    pub fn push(&mut self, s: &str) -> (u32, u16) {
        assert!(s.len() < u16::MAX as usize, "name too long");
        let need_new = match self.chunks.last() {
            None => true,
            Some(c) => c.len() + s.len() > NAME_CHUNK,
        };
        if need_new {
            self.chunks.push(Arc::new(Vec::with_capacity(NAME_CHUNK)));
        }
        let ci = self.chunks.len() - 1;
        let chunk = Chunked::<u8>::chunk_mut(&mut self.chunks[ci], NAME_CHUNK);
        let off = chunk.len();
        chunk.extend_from_slice(s.as_bytes());
        (((ci as u32) << NAME_CHUNK_BITS) | off as u32, s.len() as u16)
    }
    #[inline]
    pub fn get(&self, r: u32, len: u16) -> &str {
        let ci = (r >> NAME_CHUNK_BITS) as usize;
        let off = (r & (NAME_CHUNK as u32 - 1)) as usize;
        let bytes = &self.chunks[ci][off..off + len as usize];
        // Only &str values enter the pool, and slices use their original boundaries.
        unsafe { std::str::from_utf8_unchecked(bytes) }
    }
    pub fn allocated_bytes(&self) -> usize {
        self.chunks.iter().map(|c| c.capacity()).sum()
    }
    pub fn used_bytes(&self) -> usize {
        self.chunks.iter().map(|c| c.len()).sum()
    }
}

pub use crate::read::ExtStat;
use crate::read::TreeRead;

/// Post-scan index of each directory's children in descending size order (CSR format).
pub struct SortedIndex {
    offsets: Vec<u32>,
    items: Vec<u32>,
}

impl SortedIndex {
    pub fn children(&self, dir: u32) -> &[u32] {
        let a = self.offsets[dir as usize] as usize;
        let b = self.offsets[dir as usize + 1] as usize;
        &self.items[a..b]
    }
    pub fn allocated_bytes(&self) -> usize {
        (self.offsets.capacity() + self.items.capacity()) * 4
    }
}

/// A complete immutable tree, published periodically by the scan thread and read by the UI.
#[derive(Clone)]
pub struct Snapshot {
    nodes: Chunked<Node>,
    dirs: Chunked<Dir>,
    names: Names,
    ext_names: Arc<Vec<Arc<str>>>,
    ext_cats: Arc<Vec<Category>>,
    ext_stats: Arc<Vec<ExtStat>>,
    sorted: Option<Arc<SortedIndex>>,
    /// Largest file category per directory, computed once after scanning for treemap colors.
    dir_cats: Option<Arc<Vec<u8>>>,
    /// Snapshot generation.
    pub generation: u64,
    /// Whether scanning has finished.
    pub complete: bool,
}

/// Child linked-list iterator, in list order without sorting.
pub struct Children<'a> {
    snap: &'a Snapshot,
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
        self.cur = self.snap.nodes.get(id).next_sibling;
        Some(id)
    }
}

/// Memory usage breakdown based on allocated capacity.
#[derive(Clone, Copy, Debug, Default)]
pub struct MemoryReport {
    pub nodes: usize,
    pub dirs: usize,
    pub names: usize,
    pub sorted: usize,
    pub node_count: u32,
    pub dir_count: u32,
}

impl MemoryReport {
    pub fn total(&self) -> usize {
        self.nodes + self.dirs + self.names + self.sorted
    }
}

impl Snapshot {
    #[inline]
    pub fn node_count(&self) -> u32 {
        self.nodes.len()
    }
    #[inline]
    pub fn dir_count(&self) -> u32 {
        self.dirs.len()
    }
    #[inline]
    pub fn node(&self, id: u32) -> &Node {
        self.nodes.get(id)
    }
    #[inline]
    pub fn dir(&self, slot: u32) -> &Dir {
        self.dirs.get(slot)
    }
    #[inline]
    pub fn is_dir(&self, id: u32) -> bool {
        self.nodes.get(id).dir != NONE
    }
    /// Allocated size: the file's allocation or a directory's recursive total.
    #[inline]
    pub fn size(&self, id: u32) -> u64 {
        let n = self.nodes.get(id);
        if n.dir != NONE { self.dirs.get(n.dir).total } else { n.size }
    }
    /// Recursive file count; a file counts as one.
    #[inline]
    pub fn files(&self, id: u32) -> u64 {
        let n = self.nodes.get(id);
        if n.dir != NONE { self.dirs.get(n.dir).files as u64 } else { 1 }
    }
    #[inline]
    pub fn child_count(&self, id: u32) -> u32 {
        let n = self.nodes.get(id);
        if n.dir != NONE { self.dirs.get(n.dir).child_count } else { 0 }
    }
    #[inline]
    pub fn name(&self, id: u32) -> &str {
        let n = self.nodes.get(id);
        self.names.get(n.name, n.name_len)
    }
    #[inline]
    pub fn parent(&self, id: u32) -> u32 {
        self.nodes.get(id).parent
    }
    pub fn children(&self, id: u32) -> Children<'_> {
        let n = self.nodes.get(id);
        let cur = if n.dir != NONE { self.dirs.get(n.dir).first_child } else { NONE };
        Children { snap: self, cur }
    }
    pub fn has_sorted_index(&self) -> bool {
        self.sorted.is_some()
    }
    /// Sorted children, available without extra work when the post-scan index exists.
    pub fn sorted_children_slice(&self, id: u32) -> Option<&[u32]> {
        let n = self.nodes.get(id);
        if n.dir == NONE {
            return Some(&[]);
        }
        self.sorted.as_ref().map(|s| s.children(n.dir))
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
    pub fn depth(&self, mut id: u32) -> u32 {
        let mut d = 0;
        while self.parent(id) != NONE {
            id = self.parent(id);
            d += 1;
        }
        d
    }
    /// Full path from the root to this node.
    pub fn path(&self, id: u32) -> String {
        let mut parts = Vec::new();
        let mut cur = id;
        while cur != NONE {
            parts.push(self.name(cur));
            cur = self.parent(cur);
        }
        parts.reverse();
        let mut s = String::new();
        for (i, p) in parts.iter().enumerate() {
            if i > 0 {
                s.push('\\');
            }
            s.push_str(p);
        }
        s
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
    pub fn ext_count(&self) -> usize {
        self.ext_names.len()
    }
    pub fn ext_name(&self, ext: u16) -> &str {
        &self.ext_names[ext as usize]
    }
    pub fn ext_category(&self, ext: u16) -> Category {
        if ext == EXT_DIR { Category::Other } else { self.ext_cats[ext as usize] }
    }
    pub fn ext_stats(&self) -> &[ExtStat] {
        &self.ext_stats
    }
    /// Node color category: extension-based for files, Other for directories.
    pub fn category(&self, id: u32) -> Category {
        self.ext_category(self.nodes.get(id).ext)
    }
    pub fn memory(&self) -> MemoryReport {
        MemoryReport {
            nodes: self.nodes.allocated_bytes(),
            dirs: self.dirs.allocated_bytes(),
            names: self.names.allocated_bytes(),
            sorted: self.sorted.as_ref().map(|s| s.allocated_bytes()).unwrap_or(0)
                + self.dir_cats.as_ref().map(|v| v.capacity()).unwrap_or(0),
            node_count: self.node_count(),
            dir_count: self.dir_count(),
        }
    }
    /// Build the sorted-child index in parallel, only after scanning has finished.
    pub fn build_sorted_index(&self, threads: usize) -> SortedIndex {
        let dcount = self.dirs.len() as usize;
        let mut offsets = Vec::with_capacity(dcount + 1);
        let mut acc = 0u32;
        offsets.push(0);
        for d in 0..dcount as u32 {
            acc += self.dirs.get(d).child_count;
            offsets.push(acc);
        }
        let mut items = vec![0u32; acc as usize];
        let threads = threads.max(1);
        // Split directory ranges evenly by child count.
        let per = (acc as usize).div_ceil(threads).max(1);
        let mut bounds = vec![0usize];
        let mut next = per;
        for (d, &o) in offsets.iter().enumerate().skip(1) {
            if o as usize >= next && d < dcount {
                bounds.push(d);
                next = o as usize + per;
            }
        }
        bounds.push(dcount);
        bounds.dedup();
        std::thread::scope(|scope| {
            let mut rest: &mut [u32] = &mut items;
            let mut consumed = 0usize;
            for w in bounds.windows(2) {
                let (d0, d1) = (w[0], w[1]);
                let start = offsets[d0] as usize;
                let end = offsets[d1] as usize;
                let (_, tail) = std::mem::take(&mut rest).split_at_mut(start - consumed);
                let (part, tail) = tail.split_at_mut(end - start);
                rest = tail;
                consumed = end;
                let offsets = &offsets;
                scope.spawn(move || {
                    let mut scratch: Vec<(u64, u32)> = Vec::new();
                    for d in d0..d1 {
                        let a = offsets[d] as usize - start;
                        let b = offsets[d + 1] as usize - start;
                        if b == a {
                            continue;
                        }
                        scratch.clear();
                        let mut cur = self.dirs.get(d as u32).first_child;
                        while cur != NONE {
                            scratch.push((self.size(cur), cur));
                            cur = self.nodes.get(cur).next_sibling;
                        }
                        scratch.sort_unstable_by(|x, y| y.0.cmp(&x.0).then(x.1.cmp(&y.1)));
                        for (slot, (_, id)) in part[a..b].iter_mut().zip(scratch.iter()) {
                            *slot = *id;
                        }
                    }
                });
            }
        });
        SortedIndex { offsets, items }
    }
    pub fn with_sorted_index(mut self, index: Arc<SortedIndex>) -> Self {
        self.sorted = Some(index);
        self
    }

    /// Dominant directory category, available after scanning.
    #[inline]
    pub fn dir_category_slot(&self, dir_slot: u32) -> Option<u8> {
        self.dir_cats.as_ref().map(|v| v[dir_slot as usize])
    }

    /// Compute the largest file category per directory in one pass: assign files to their parent by extension,
    /// and subdirectories to their own dominant category. Parent indices precede child indices, so iterate in reverse.
    pub fn build_dir_categories(&self) -> Vec<u8> {
        let dcount = self.dirs.len() as usize;
        let mut acc = vec![[0f32; crate::ext::CATEGORY_COUNT]; dcount];
        for id in 0..self.nodes.len() {
            let n = self.nodes.get(id);
            if n.dir == NONE && n.parent != NONE {
                let p = self.nodes.get(n.parent).dir as usize;
                acc[p][self.ext_category(n.ext) as usize] += n.size.max(1) as f32;
            }
        }
        let mut out = vec![Category::Other as u8; dcount];
        for d in (0..dcount).rev() {
            let a = &acc[d];
            let mut best = Category::Other as usize;
            let mut bv = 0.0f32;
            for (i, &v) in a.iter().enumerate() {
                if v > bv {
                    bv = v;
                    best = i;
                }
            }
            out[d] = best as u8;
            let node = self.dirs.get(d as u32).node;
            let parent = self.nodes.get(node).parent;
            if parent != NONE {
                let p = self.nodes.get(parent).dir as usize;
                acc[p][best] += self.dirs.get(d as u32).total.max(1) as f32;
            }
        }
        out
    }

    pub fn with_dir_categories(mut self, cats: Arc<Vec<u8>>) -> Self {
        self.dir_cats = Some(cats);
        self
    }
}

/// Mutable tree owned by the scan thread.
pub struct TreeBuilder {
    data: Snapshot,
    /// Parent directory index, used only to propagate aggregates before publishing.
    dir_parent: Vec<u32>,
    /// Unpropagated size and file-count deltas.
    pending: Vec<(u64, u32)>,
    dirty_max: Option<u32>,
    ext_lookup: HashMap<Box<str>, u16>,
    ext_stats: Vec<ExtStat>,
}

impl Default for TreeBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl TreeBuilder {
    pub fn new() -> Self {
        Self {
            data: Snapshot {
                nodes: Chunked::new(16),
                dirs: Chunked::new(12),
                names: Names::default(),
                ext_names: Arc::new(Vec::new()),
                ext_cats: Arc::new(Vec::new()),
                ext_stats: Arc::new(Vec::new()),
                sorted: None,
                dir_cats: None,
                generation: 0,
                complete: false,
            },
            dir_parent: Vec::new(),
            pending: Vec::new(),
            dirty_max: None,
            ext_lookup: HashMap::new(),
            ext_stats: Vec::new(),
        }
    }
    pub fn node_count(&self) -> u32 {
        self.data.nodes.len()
    }
    pub fn dir_count(&self) -> u32 {
        self.data.dirs.len()
    }
    /// Intern an extension (lowercase, without the dot), returning its index. New extensions beyond the limit
    /// share the last index; real disks can have tens of thousands of random extensions, and this tree is temporary.
    pub fn intern_ext(&mut self, ext: &str) -> u16 {
        if let Some(&id) = self.ext_lookup.get(ext) {
            return id;
        }
        if self.ext_stats.len() as u16 >= EXT_DIR - 2 {
            return EXT_DIR - 3;
        }
        let id = self.ext_stats.len() as u16;
        Arc::make_mut(&mut self.data.ext_names).push(Arc::from(ext));
        Arc::make_mut(&mut self.data.ext_cats).push(category_of(ext));
        self.ext_lookup.insert(ext.into(), id);
        self.ext_stats.push(ExtStat::default());
        id
    }
    fn link(&mut self, parent: u32, id: u32) -> (u32, u32) {
        if parent == NONE {
            return (NONE, NONE);
        }
        let pslot = self.data.nodes.get(parent).dir;
        debug_assert!(pslot != NONE, "parent must be a directory");
        let pd = self.data.dirs.get_mut(pslot);
        let next = pd.first_child;
        pd.first_child = id;
        pd.child_count += 1;
        (next, pslot)
    }
    pub fn add_dir(&mut self, parent: u32, name: &str) -> u32 {
        let id = self.data.nodes.len();
        let slot = self.data.dirs.len();
        let (next, pslot) = self.link(parent, id);
        let (nr, nl) = self.data.names.push(name);
        self.data.nodes.push(Node {
            parent,
            next_sibling: next,
            size: 0,
            name: nr,
            name_len: nl,
            ext: EXT_DIR,
            dir: slot,
        });
        self.data.dirs.push(Dir { node: id, first_child: NONE, child_count: 0, files: 0, total: 0 });
        self.dir_parent.push(pslot);
        self.pending.push((0, 0));
        id
    }
    pub fn add_file(&mut self, parent: u32, name: &str, ext: u16, size: u64) -> u32 {
        let id = self.data.nodes.len();
        let (next, pslot) = self.link(parent, id);
        let (nr, nl) = self.data.names.push(name);
        self.data.nodes.push(Node {
            parent,
            next_sibling: next,
            size,
            name: nr,
            name_len: nl,
            ext,
            dir: NONE,
        });
        if pslot != NONE {
            let p = &mut self.pending[pslot as usize];
            p.0 += size;
            p.1 += 1;
            self.dirty_max = Some(self.dirty_max.map_or(pslot, |m| m.max(pslot)));
        }
        let e = &mut self.ext_stats[ext as usize];
        e.files += 1;
        e.bytes += size;
        id
    }
    /// Propagate deltas up the parent chain in one reverse pass; parent directory indices precede their children.
    pub fn flush(&mut self) {
        let Some(max) = self.dirty_max.take() else { return };
        for d in (0..=max).rev() {
            let (s, f) = self.pending[d as usize];
            if s == 0 && f == 0 {
                continue;
            }
            self.pending[d as usize] = (0, 0);
            let dir = self.data.dirs.get_mut(d);
            dir.total += s;
            dir.files += f;
            let p = self.dir_parent[d as usize];
            if p != NONE {
                let pp = &mut self.pending[p as usize];
                pp.0 += s;
                pp.1 += f;
            }
        }
    }
    /// Aggregate and publish a snapshot.
    pub fn snapshot(&mut self) -> Snapshot {
        self.flush();
        self.data.generation += 1;
        self.data.ext_stats = Arc::new(self.ext_stats.clone());
        self.data.clone()
    }
    pub fn set_complete(&mut self) {
        self.data.complete = true;
    }
}

impl TreeRead for Snapshot {
    fn root(&self) -> u32 {
        if self.nodes.is_empty() { NONE } else { 0 }
    }
    fn generation(&self) -> u64 {
        self.generation
    }
    fn complete(&self) -> bool {
        self.complete
    }
    fn slot_count(&self) -> u32 {
        self.nodes.len()
    }
    fn dir_count(&self) -> u64 {
        self.dirs.len() as u64
    }
    fn file_count(&self) -> u64 {
        (self.nodes.len() - self.dirs.len()) as u64
    }
    fn contains(&self, id: u32) -> bool {
        id < self.nodes.len()
    }
    #[inline]
    fn is_dir(&self, id: u32) -> bool {
        Snapshot::is_dir(self, id)
    }
    #[inline]
    fn size(&self, id: u32) -> u64 {
        Snapshot::size(self, id)
    }
    fn files(&self, id: u32) -> u64 {
        Snapshot::files(self, id)
    }
    fn child_count(&self, id: u32) -> u32 {
        Snapshot::child_count(self, id)
    }
    fn name(&self, id: u32) -> &str {
        Snapshot::name(self, id)
    }
    #[inline]
    fn parent(&self, id: u32) -> u32 {
        Snapshot::parent(self, id)
    }
    #[inline]
    fn first_child(&self, id: u32) -> u32 {
        let n = self.nodes.get(id);
        if n.dir != NONE { self.dirs.get(n.dir).first_child } else { NONE }
    }
    #[inline]
    fn next_sibling(&self, id: u32) -> u32 {
        self.nodes.get(id).next_sibling
    }
    fn sorted_children_slice(&self, id: u32) -> Option<&[u32]> {
        Snapshot::sorted_children_slice(self, id)
    }
    fn has_sorted_index(&self) -> bool {
        Snapshot::has_sorted_index(self)
    }
    #[inline]
    fn ext_of(&self, id: u32) -> u32 {
        let e = self.nodes.get(id).ext;
        if e == EXT_DIR { NONE } else { e as u32 }
    }
    fn ext_count(&self) -> usize {
        self.ext_names.len()
    }
    fn ext_name(&self, ext: u32) -> &str {
        &self.ext_names[ext as usize]
    }
    #[inline]
    fn ext_category(&self, ext: u32) -> Category {
        self.ext_cats[ext as usize]
    }
    fn ext_stat(&self, ext: u32) -> ExtStat {
        self.ext_stats.get(ext as usize).copied().unwrap_or_default()
    }
    #[inline]
    fn dir_category(&self, id: u32) -> Option<u8> {
        let d = self.nodes.get(id).dir;
        if d == NONE { None } else { self.dir_category_slot(d) }
    }
    fn is_special(&self, _id: u32) -> bool {
        false
    }
    fn is_unreadable(&self, _id: u32) -> bool {
        false
    }
    fn memory_bytes(&self) -> usize {
        self.memory().total()
    }
}
