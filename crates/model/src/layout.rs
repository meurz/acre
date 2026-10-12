//! Treemap layout: run streemap squarify for each directory, then clip to pixels.
//!
//! - Children below `min_area` pixels are merged into one remaining-items block, preserving their
//!   combined area in the parent so larger blocks keep their proportions.
//! - Subdivide only sufficiently large directories, up to `max_depth` levels.
//! - With headers enabled (`header > 0`), reserve a name strip above the children of sufficiently large folders.
//! - Output is parent-before-child: forward order paints; reverse order performs hit testing.
//! - Precompute van Wijk cushion coefficients so rendering does not need to traverse the tree.

use std::time::Instant;

use crate::ext::{CATEGORY_COUNT, Category};
use crate::read::TreeRead;
use crate::tree::NONE;

pub const KIND_FILE: u8 = 0;
/// Directory too small or too deep to subdivide.
pub const KIND_DIR_LEAF: u8 = 1;
/// Subdivided directory, fully covered by its children.
pub const KIND_DIR: u8 = 2;
/// Aggregated remaining items; `node` is their containing directory.
pub const KIND_REST: u8 = 3;

#[derive(Clone, Copy, Debug)]
pub struct TmRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub node: u32,
    /// Parent block index in `rects`; [`NONE`] for the root.
    pub parent: u32,
    pub depth: u16,
    pub kind: u8,
    pub cat: u8,
    /// Number of aggregated items; zero for other blocks.
    pub rest_count: u32,
    /// Header height reserved above a subdivided folder, in pixels; zero if absent.
    pub header: f32,
}

impl TmRect {
    #[inline]
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.w && y < self.y + self.h
    }
    #[inline]
    pub fn is_leaf(&self) -> bool {
        self.kind != KIND_DIR
    }
}

#[derive(Clone, Copy, Debug)]
pub struct LayoutParams {
    /// Do not draw children separately below this area in square pixels.
    pub min_area: f32,
    /// Do not subdivide directories with sides shorter than this.
    pub min_side: f32,
    pub max_depth: u16,
    /// Cushion shading: initial height and decay per level.
    pub cushion_h: f32,
    pub cushion_f: f32,
    /// Subfolder header height in pixels; zero disables headers. The current view's root has no header.
    pub header: f32,
    /// Minimum folder block width and height required for a header.
    pub header_min_w: f32,
    pub header_min_h: f32,
}

impl Default for LayoutParams {
    fn default() -> Self {
        Self {
            min_area: 3.0,
            min_side: 3.0,
            max_depth: 32,
            cushion_h: 0.5,
            cushion_f: 0.75,
            header: 0.0,
            header_min_w: 0.0,
            header_min_h: 0.0,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct LayoutStats {
    pub dirs_subdivided: u32,
    pub children_scanned: u64,
    pub rest_blocks: u32,
    pub micros: u64,
}

#[derive(Clone, Default)]
pub struct Layout {
    pub rects: Vec<TmRect>,
    /// Cushion coefficients [s1x, s2x, s1y, s2y] for each block.
    pub surf: Vec<[f32; 4]>,
    pub width: f32,
    pub height: f32,
    pub root: u32,
    pub generation: u64,
    pub stats: LayoutStats,
}

impl Layout {
    /// Hit test: return the deepest block containing the point.
    pub fn hit(&self, x: f32, y: f32) -> Option<usize> {
        (0..self.rects.len()).rev().find(|&i| self.rects[i].contains(x, y))
    }
    pub fn leaf_count(&self) -> usize {
        self.rects.iter().filter(|r| r.is_leaf()).count()
    }
}

#[derive(Clone, Copy)]
struct Item {
    size: f32,
    node: u32,
    kind: u8,
    cat: u8,
    count: u32,
    rect: streemap::Rect<f32>,
}

#[inline]
fn ridge(s: &mut [f32; 4], axis: usize, x1: f32, x2: f32, h: f32) {
    let d = x2 - x1;
    if d > 0.0 {
        s[axis * 2] += 4.0 * h * (x2 + x1) / d;
        s[axis * 2 + 1] -= 4.0 * h / d;
    }
}

/// Lay out `root` inside a `w` by `h` pixel rectangle.
pub fn layout(snap: &dyn TreeRead, root: u32, w: f32, h: f32, p: &LayoutParams) -> Layout {
    let t0 = Instant::now();
    let mut out = Layout {
        width: w,
        height: h,
        root,
        generation: snap.generation(),
        ..Default::default()
    };
    if w < 1.0 || h < 1.0 || root == NONE {
        return out;
    }
    let root_kind = if snap.is_dir(root) { KIND_DIR_LEAF } else { KIND_FILE };
    out.rects.push(TmRect {
        x: 0.0,
        y: 0.0,
        w,
        h,
        node: root,
        parent: NONE,
        depth: 0,
        kind: root_kind,
        cat: snap.category(root) as u8,
        rest_count: 0,
        header: 0.0,
    });
    let mut s = [0.0f32; 4];
    ridge(&mut s, 0, 0.0, w, p.cushion_h);
    ridge(&mut s, 1, 0.0, h, p.cushion_h);
    out.surf.push(s);

    let mut stack: Vec<u32> = vec![0];
    let mut items: Vec<Item> = Vec::new();
    let mut scratch: Vec<(u64, u32)> = Vec::new();
    while let Some(ri) = stack.pop() {
        let r = out.rects[ri as usize];
        if r.kind == KIND_FILE || r.kind == KIND_REST {
            continue;
        }
        if r.depth >= p.max_depth || r.w < p.min_side || r.h < p.min_side {
            continue;
        }
        let total = snap.size(r.node);
        if total == 0 {
            continue;
        }
        // Reserve a header above the children of sufficiently large subfolders.
        let hdr = if p.header > 0.0 && r.depth >= 1 && r.w >= p.header_min_w && r.h >= p.header_min_h { p.header } else { 0.0 };
        let area = r.w * (r.h - hdr);
        // Children below this byte threshold have an area smaller than min_area.
        let threshold = (p.min_area as f64 * total as f64 / area as f64).ceil() as u64;
        items.clear();
        let mut kept: u64 = 0;
        let mut scanned = 0u64;
        // Color aggregated blocks by their largest category, using a sample instead of a full scan.
        let mut rest_cats = [0u64; CATEGORY_COUNT];
        if let Some(sorted) = snap.sorted_children_slice(r.node) {
            let mut cut = sorted.len();
            for (k, &c) in sorted.iter().enumerate() {
                scanned += 1;
                let sz = snap.size(c);
                if sz < threshold || sz == 0 {
                    cut = k;
                    break;
                }
                kept += sz;
                items.push(item(snap, c, sz));
            }
            // Use the containing directory's dominant category when available, otherwise sample aggregated children.
            match snap.dir_category(r.node) {
                Some(cat) => rest_cats[cat as usize] = 1,
                None => {
                    for &c in sorted[cut..].iter().take(REST_SAMPLE) {
                        rest_cats[dominant(snap, c) as usize] += snap.size(c).max(1);
                    }
                }
            }
        } else {
            scratch.clear();
            for c in snap.children(r.node) {
                scanned += 1;
                let sz = snap.size(c);
                if sz >= threshold && sz > 0 {
                    scratch.push((sz, c));
                } else if !snap.is_dir(c) {
                    rest_cats[snap.category(c) as usize] += sz.max(1);
                }
            }
            scratch.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
            for &(sz, c) in &scratch {
                kept += sz;
                items.push(item(snap, c, sz));
            }
        }
        out.stats.children_scanned += scanned;
        let rest = total.saturating_sub(kept);
        let child_count = snap.child_count(r.node);
        if rest > 0 {
            let cat = argmax(&rest_cats);
            items.push(Item {
                size: rest as f32,
                node: r.node,
                kind: KIND_REST,
                cat,
                count: child_count.saturating_sub(items.len() as u32),
                rect: streemap::Rect { x: 0.0, y: 0.0, w: 0.0, h: 0.0 },
            });
            out.stats.rest_blocks += 1;
        }
        if items.is_empty() {
            continue;
        }
        out.rects[ri as usize].kind = KIND_DIR;
        out.rects[ri as usize].header = hdr;
        out.stats.dirs_subdivided += 1;
        let rect = streemap::Rect { x: r.x, y: r.y + hdr, w: r.w, h: r.h - hdr };
        streemap::squarify(rect, &mut items, |it| it.size, |it, rc| it.rect = rc);
        let parent_surf = out.surf[ri as usize];
        let hh = p.cushion_h * p.cushion_f.powi(r.depth as i32 + 1);
        for it in &items {
            let rc = it.rect;
            if !(rc.w > 0.0 && rc.h > 0.0) {
                continue;
            }
            let cat = it.cat;
            let kind = if it.kind == KIND_DIR { KIND_DIR_LEAF } else { it.kind };
            let idx = out.rects.len() as u32;
            out.rects.push(TmRect {
                x: rc.x,
                y: rc.y,
                w: rc.w,
                h: rc.h,
                node: it.node,
                parent: ri,
                depth: r.depth + 1,
                kind,
                cat,
                rest_count: it.count,
                header: 0.0,
            });
            let mut s = parent_surf;
            ridge(&mut s, 0, rc.x, rc.x + rc.w, hh);
            ridge(&mut s, 1, rc.y, rc.y + rc.h, hh);
            out.surf.push(s);
            if kind == KIND_DIR_LEAF {
                stack.push(idx);
            }
        }
    }
    // Sample the dominant category of directory leaves.
    for r in out.rects.iter_mut() {
        if r.kind == KIND_DIR_LEAF && (snap.has_sorted_index() || r.w * r.h >= SAMPLE_MIN_AREA) {
            r.cat = dominant(snap, r.node) as u8;
        }
    }
    out.stats.micros = t0.elapsed().as_micros() as u64;
    out
}

/// Number of items sampled in an aggregated block.
const REST_SAMPLE: usize = 12;
/// Number of children sampled in an unsubdivided directory.
const DIR_SAMPLE: usize = 4;
/// During scanning, skip sampling directories smaller than this area and use a neutral color instead.
const SAMPLE_MIN_AREA: f32 = 16.0;

fn argmax(v: &[u64; CATEGORY_COUNT]) -> u8 {
    let mut best = Category::Other as usize;
    let mut bv = 0;
    for (i, &x) in v.iter().enumerate() {
        if x > bv {
            bv = x;
            best = i;
        }
    }
    best as u8
}

/// Node color: extension-based for files; sample the largest children of directories without recursing.
fn dominant(snap: &dyn TreeRead, c: u32) -> Category {
    if !snap.is_dir(c) {
        return snap.category(c);
    }
    // The completed scan already has the dominant directory category: O(1).
    if let Some(cat) = snap.dir_category(c) {
        return Category::ALL[cat as usize];
    }
    let mut acc = [0u64; CATEGORY_COUNT];
    match snap.sorted_children_slice(c) {
        Some(s) => {
            for &k in s.iter().take(DIR_SAMPLE) {
                if !snap.is_dir(k) {
                    acc[snap.category(k) as usize] += snap.size(k).max(1);
                }
            }
        }
        None => {
            for k in snap.children(c).take(DIR_SAMPLE) {
                if !snap.is_dir(k) {
                    acc[snap.category(k) as usize] += snap.size(k).max(1);
                }
            }
        }
    }
    let i = argmax(&acc);
    Category::ALL[i as usize]
}

#[inline]
fn item(snap: &dyn TreeRead, c: u32, sz: u64) -> Item {
    let is_dir = snap.is_dir(c);
    Item {
        size: sz as f32,
        node: c,
        kind: if is_dir { KIND_DIR } else { KIND_FILE },
        // Color directory leaves by the sampled category; subdivided blocks are covered by children anyway.
        cat: if is_dir { Category::Other as u8 } else { snap.category(c) as u8 },
        count: 0,
        rect: streemap::Rect { x: 0.0, y: 0.0, w: 0.0, h: 0.0 },
    }
}
