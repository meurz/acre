//! Data-layer benchmark: generation, snapshots, sorted indices, 100,000-child expansion, layout, rendering, and scan simulation.
//! Print one JSON line. Usage: model-bench <nodes> [layout-width layout-height]
//! Real scans use the same path as the UI: core scan, indexing, root layout, and bitmap. Usage: model-bench real <drive-or-folder> [walk]

use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use acre_model::generate::GenSpec;
use acre_model::layout::{LayoutParams, layout};
use acre_model::render::{Palette, RenderParams, render};
use acre_model::scan::{load, simulate};

fn real(path: &str, walk: bool) {
    use acre_model::TreeRead;
    use acre_model::live::{LiveEvent, options, run};
    let cancel = AtomicBool::new(false);
    let t0 = Instant::now();
    let mut partials = 0u32;
    let mut first_partial_ms = None;
    let mut done = None;
    run(options(path, !walk), Duration::from_millis(200), &cancel, &mut |ev| match ev {
        LiveEvent::Partial(..) => {
            partials += 1;
            first_partial_ms.get_or_insert(t0.elapsed().as_secs_f64() * 1000.0);
        }
        LiveEvent::Done(d) => done = Some(d),
        LiveEvent::Failed(e) => eprintln!("failed: {e}"),
        _ => {}
    });
    let total_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let d = done.expect("scan finished");
    let t = &d.tree as &dyn TreeRead;
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let lt = Instant::now();
    let l = layout(t, t.root(), 1572.0, 398.0, &LayoutParams::default());
    let layout_ms = lt.elapsed().as_secs_f64() * 1000.0;
    let rt = Instant::now();
    let mut buf = vec![0u8; 1572 * 398 * 4];
    let palette = Palette { categories: [[90, 120, 200]; 9], dir_leaf: [70, 70, 80], rest: [50, 50, 55] };
    render(&l, &palette, &RenderParams::default(), &mut buf, 1572, 398, threads);
    let render_ms = rt.elapsed().as_secs_f64() * 1000.0;
    println!(
        "{}",
        serde_like(&[
            ("path", format!("\"{}\"", path.replace('\\', "\\\\"))),
            ("backend", format!("\"{:?}\"", d.backend)),
            ("scan_ms", format!("{:.0}", d.scan.as_secs_f64() * 1000.0)),
            ("sort_index_ms", format!("{:.1}", d.index.sort_us as f64 / 1000.0)),
            ("categories_ms", format!("{:.1}", d.index.categories_us as f64 / 1000.0)),
            ("total_ms", format!("{total_ms:.0}")),
            ("partials", partials.to_string()),
            ("first_partial_ms", first_partial_ms.map_or("null".into(), |v| format!("{v:.0}"))),
            ("files", t.file_count().to_string()),
            ("dirs", t.dir_count().to_string()),
            ("alloc", t.size(t.root()).to_string()),
            ("tree_bytes", t.memory_bytes().to_string()),
            ("unreadable", d.unreadable.to_string()),
            ("layout_root_ms", format!("{layout_ms:.1}")),
            ("render_root_ms", format!("{render_ms:.1}")),
            ("rects", l.rects.len().to_string()),
        ])
    );
}

fn serde_like(kv: &[(&str, String)]) -> String {
    let body: Vec<String> = kv.iter().map(|(k, v)| format!("\"{k}\": {v}")).collect();
    format!("{{{}}}", body.join(", "))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("real") {
        real(args.get(2).map(String::as_str).unwrap_or("C:\\"), args.get(3).map(String::as_str) == Some("walk"));
        return;
    }
    let nodes: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(1_000_000);
    let w: f32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1580.0);
    let h: f32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(430.0);
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let spec = GenSpec::tier(nodes);

    let (snap, ls) = load(&spec, threads);
    let mem = snap.memory();

    // Directory with the most direct children.
    let mut biggest = (0u32, 0u32);
    for d in 0..snap.dir_count() {
        let c = snap.dir(d).child_count;
        if c > biggest.1 {
            biggest = (snap.dir(d).node, c);
        }
    }
    // Shape: maximum depth, directories with >=10,000 or >=100,000 children, and file-size quantiles.
    let mut max_depth = 0u32;
    let mut depth = vec![0u32; snap.node_count() as usize];
    for id in 1..snap.node_count() {
        let p = snap.parent(id);
        depth[id as usize] = depth[p as usize] + 1;
        max_depth = max_depth.max(depth[id as usize]);
    }
    let mut big_dirs: Vec<(u32, String)> = (0..snap.dir_count())
        .filter(|&d| snap.dir(d).child_count >= 10_000)
        .map(|d| (snap.dir(d).child_count, snap.path(snap.dir(d).node)))
        .collect();
    big_dirs.sort_by(|a, b| b.0.cmp(&a.0));
    let mut sizes: Vec<u64> = (0..snap.node_count()).filter(|&i| !snap.is_dir(i)).map(|i| snap.size(i)).collect();
    sizes.sort_unstable();
    let q = |p: f64| sizes[((sizes.len() - 1) as f64 * p) as usize];
    eprintln!(
        "shape: max_depth={max_depth} files={} size_p50={} p90={} p99={} p999={} max={} big_dirs={:?}",
        sizes.len(), q(0.5), q(0.9), q(0.99), q(0.999), sizes.last().unwrap(), big_dirs
    );
    let t = Instant::now();
    let rows = snap.sorted_children(biggest.0);
    let expand_indexed_us = t.elapsed().as_micros();
    // On-demand sorting time without an index, as during scanning.
    let raw = {
        let mut b = acre_model::TreeBuilder::new();
        acre_model::generate::generate(&spec, &mut b, &mut |_| true);
        b.snapshot()
    };
    let t = Instant::now();
    let rows2 = raw.sorted_children(biggest.0);
    let expand_unindexed_us = t.elapsed().as_micros();
    assert_eq!(rows.len(), rows2.len());

    let params = LayoutParams::default();
    let mut layout_us = Vec::new();
    let mut l = layout(&snap, 0, w, h, &params);
    for _ in 0..5 {
        l = layout(&snap, 0, w, h, &params);
        layout_us.push(l.stats.micros);
    }
    let t = Instant::now();
    let l_raw = layout(&raw, 0, w, h, &params);
    let layout_unindexed_us = t.elapsed().as_micros();
    let pal = Palette { categories: [[57, 135, 229]; 9], dir_leaf: [60, 60, 60], rest: [50, 50, 50] };
    let (wi, hi) = (w as usize, h as usize);
    let mut buf = vec![0u8; wi * hi * 4];
    let mut cushion_us = Vec::new();
    for _ in 0..5 {
        let t = Instant::now();
        render(&l, &pal, &RenderParams::default(), &mut buf, wi, hi, threads);
        cushion_us.push(t.elapsed().as_micros());
    }
    let t = Instant::now();
    render(&l, &pal, &RenderParams::default(), &mut buf, wi, hi, 1);
    let cushion_1t_us = t.elapsed().as_micros();
    let t = Instant::now();
    render(&l, &pal, &RenderParams { cushion: false, ..Default::default() }, &mut buf, wi, hi, threads);
    let flat_us = t.elapsed().as_micros();

    // Unlimited simulated scan with a snapshot every 200 ms.
    let cancel = AtomicBool::new(false);
    let mut pubs = Vec::new();
    let t = Instant::now();
    simulate(&spec, None, Duration::from_millis(200), threads, &cancel, &mut |s, info| {
        pubs.push((info.snapshot_us, info.nodes, info.final_publish));
        drop(s);
    });
    let scan_ms = t.elapsed().as_millis();
    let max_pub = pubs.iter().filter(|p| !p.2).map(|p| p.0).max().unwrap_or(0);
    let final_pub = pubs.iter().find(|p| p.2).map(|p| p.0).unwrap_or(0);

    println!(
        "{{\"nodes\":{},\"total_bytes\":{},\"dirs\":{},\"generate_ms\":{},\"snapshot_ms\":{},\"sort_index_ms\":{},\
\"mem_nodes\":{},\"mem_dirs\":{},\"mem_names\":{},\"mem_sorted\":{},\"mem_total\":{},\
\"biggest_dir_children\":{},\"expand_indexed_us\":{},\"expand_unindexed_us\":{},\
\"layout_w\":{},\"layout_h\":{},\"layout_rects\":{},\"layout_leaves\":{},\"layout_dirs_subdivided\":{},\"layout_children_scanned\":{},\
\"layout_us\":{:?},\"layout_unindexed_us\":{},\"layout_unindexed_rects\":{},\"cushion_us\":{:?},\"cushion_1thread_us\":{},\"flat_us\":{},\
\"threads\":{},\"scan_unthrottled_ms\":{},\"scan_publishes\":{},\"scan_max_snapshot_us\":{},\"scan_final_snapshot_us\":{}}}",
        ls.nodes, snap.size(0), ls.dirs, ls.generate_ms, ls.snapshot_ms, ls.sort_index_ms,
        mem.nodes, mem.dirs, mem.names, mem.sorted, mem.total(),
        biggest.1, expand_indexed_us, expand_unindexed_us,
        w, h, l.rects.len(), l.leaf_count(), l.stats.dirs_subdivided, l.stats.children_scanned,
        layout_us, layout_unindexed_us, l_raw.rects.len(), cushion_us, cushion_1t_us, flat_us,
        threads, scan_ms, pubs.len(), max_pub, final_pub
    );
}
