use std::sync::Arc;

use crate::generate::{GenSpec, generate};
use crate::layout::{KIND_DIR, LayoutParams, layout};
use crate::render::{Palette, RenderParams, render};
use crate::tree::{NONE, TreeBuilder};

fn small_spec(nodes: u64) -> GenSpec {
    GenSpec { nodes, seed: 7, thumbcache: 3000, blobs: 2000, winsxs: 500, dataset: 0, npm_content: 0 }
}

#[test]
fn totals_match_children() {
    let mut t = TreeBuilder::new();
    generate(&small_spec(40_000), &mut t, &mut |_| true);
    let snap = t.snapshot();
    assert!(snap.node_count() > 35_000, "nodes {}", snap.node_count());
    for d in 0..snap.dir_count() {
        let node = snap.dir(d).node;
        let sum: u64 = snap.children(node).map(|c| snap.size(c)).sum();
        let files: u64 = snap.children(node).map(|c| snap.files(c)).sum();
        assert_eq!(sum, snap.size(node));
        assert_eq!(files, snap.files(node));
        assert_eq!(snap.children(node).count() as u32, snap.child_count(node));
    }
}

#[test]
fn snapshot_is_isolated_from_later_writes() {
    let mut t = TreeBuilder::new();
    let root = t.add_dir(NONE, "C:");
    let ext = t.intern_ext("txt");
    t.add_file(root, "a.txt", ext, 4096);
    let s1 = t.snapshot();
    for i in 0..70_000 {
        t.add_file(root, &format!("f{i}.txt"), ext, 4096);
    }
    let s2 = t.snapshot();
    assert_eq!(s1.node_count(), 2);
    assert_eq!(s1.size(root), 4096);
    assert_eq!(s1.child_count(root), 1);
    assert_eq!(s1.name(1), "a.txt");
    assert_eq!(s2.size(root), 4096 * 70_001);
    assert_eq!(s2.child_count(root), 70_001);
}

#[test]
fn sorted_index_matches_on_demand_sort() {
    let mut t = TreeBuilder::new();
    generate(&small_spec(20_000), &mut t, &mut |_| true);
    let snap = t.snapshot();
    let index = Arc::new(snap.build_sorted_index(4));
    let with = snap.clone().with_sorted_index(index);
    for d in 0..snap.dir_count() {
        let node = snap.dir(d).node;
        let a: Vec<u64> = snap.sorted_children(node).iter().map(|&c| snap.size(c)).collect();
        let b: Vec<u64> = with.sorted_children(node).iter().map(|&c| with.size(c)).collect();
        assert_eq!(a, b);
    }
}

#[test]
fn layout_tiles_and_stays_inside() {
    let mut t = TreeBuilder::new();
    generate(&small_spec(30_000), &mut t, &mut |_| true);
    let snap = t.snapshot();
    let (w, h) = (800.0, 500.0);
    let l = layout(&snap, 0, w, h, &LayoutParams::default());
    assert!(l.rects.len() > 100);
    for r in &l.rects {
        assert!(r.x >= -0.01 && r.y >= -0.01 && r.x + r.w <= w + 0.05 && r.y + r.h <= h + 0.05, "{r:?}");
    }
    // Each subdivided directory's child areas sum to its own area.
    let mut child_area = vec![0f64; l.rects.len()];
    for r in &l.rects[1..] {
        child_area[r.parent as usize] += (r.w * r.h) as f64;
    }
    for (i, r) in l.rects.iter().enumerate() {
        if r.kind == KIND_DIR {
            let a = (r.w * r.h) as f64;
            assert!((child_area[i] - a).abs() < a * 1e-3 + 0.5, "{i} {} vs {a}", child_area[i]);
        }
    }
    // Hit testing returns the deepest block.
    let hit = l.hit(400.0, 250.0).unwrap();
    assert!(l.rects[hit].is_leaf());
}

#[test]
fn render_covers_every_pixel() {
    let mut t = TreeBuilder::new();
    generate(&small_spec(30_000), &mut t, &mut |_| true);
    let snap = t.snapshot();
    let (w, h) = (640usize, 400usize);
    for header in [0.0, 16.0] {
        let p = LayoutParams { header, header_min_w: 80.0, header_min_h: 48.0, ..Default::default() };
        let l = layout(&snap, 0, w as f32, h as f32, &p);
        let pal = Palette { categories: [[200, 10, 10]; 9], dir_leaf: [200, 10, 10], rest: [200, 10, 10] };
        let mut buf = vec![0u8; w * h * 4];
        let params = RenderParams { cushion: false, background: [0, 255, 0], header: [10, 10, 200], ..Default::default() };
        render(&l, &pal, &params, &mut buf, w, h, 4);
        // Flat rendering must leave no pure background green; darkened red edges and blue headers are expected.
        let green = buf.chunks_exact(4).filter(|p| p[1] == 255 && p[2] == 0).count();
        assert_eq!(green, 0, "header {header}");
    }
}

#[test]
fn layout_reserves_header_rows_for_big_folders() {
    let mut t = TreeBuilder::new();
    generate(&small_spec(30_000), &mut t, &mut |_| true);
    let snap = t.snapshot();
    let (w, h) = (1200.0, 700.0);
    let p = LayoutParams { header: 16.0, header_min_w: 80.0, header_min_h: 48.0, ..Default::default() };
    let l = layout(&snap, 0, w, h, &p);
    assert_eq!(l.rects[0].header, 0.0, "the current view itself has no header");
    let with_header: Vec<usize> = (0..l.rects.len()).filter(|&i| l.rects[i].header > 0.0).collect();
    assert!(!with_header.is_empty());
    let mut child_area = vec![0f64; l.rects.len()];
    for r in &l.rects[1..] {
        child_area[r.parent as usize] += (r.w * r.h) as f64;
        let parent = l.rects[r.parent as usize];
        // All children lie below their parent's header.
        assert!(r.y >= parent.y + parent.header - 0.01, "{r:?} under {parent:?}");
    }
    for &i in &with_header {
        let r = l.rects[i];
        assert_eq!(r.kind, KIND_DIR);
        assert!(r.depth >= 1 && r.w >= 80.0 && r.h >= 48.0);
        let a = (r.w * (r.h - r.header)) as f64;
        assert!((child_area[i] - a).abs() < a * 1e-3 + 0.5, "{i} {} vs {a}", child_area[i]);
    }
    // A point in the header hits the folder itself.
    let r = l.rects[with_header[0]];
    let hit = l.hit(r.x + 2.0, r.y + 2.0).unwrap();
    assert_eq!(l.rects[hit].node, r.node);
}

#[test]
fn format_helpers() {
    assert_eq!(crate::format::bytes(1536), "1.50 KB");
    assert_eq!(crate::format::count(1234567), "1,234,567");
    assert_eq!(crate::format::percent(1, 3), "33.3%");
}
