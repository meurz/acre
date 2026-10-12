//! Immediate loading and simulated scans: one generator, with throttling and periodic snapshots for simulation.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::generate::{GenSpec, generate};
use crate::tree::{Snapshot, TreeBuilder};

/// Timing for each snapshot publication.
#[derive(Clone, Copy, Debug, Default)]
pub struct PublishInfo {
    pub nodes: u32,
    /// Time to aggregate deltas and copy chunk pointers.
    pub snapshot_us: u64,
    pub elapsed_ms: u64,
    pub final_publish: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct LoadStats {
    pub generate_ms: u64,
    pub snapshot_ms: u64,
    /// Sorted-child index and dominant directory categories.
    pub sort_index_ms: u64,
    pub nodes: u32,
    pub dirs: u32,
}

/// Generate the entire tree immediately, including its sorted-child index.
pub fn load(spec: &GenSpec, threads: usize) -> (Snapshot, LoadStats) {
    let mut tree = TreeBuilder::new();
    let t0 = Instant::now();
    generate(spec, &mut tree, &mut |_| true);
    let generate_ms = t0.elapsed().as_millis() as u64;
    tree.set_complete();
    let t1 = Instant::now();
    let snap = tree.snapshot();
    drop(tree);
    let snapshot_ms = t1.elapsed().as_millis() as u64;
    let t2 = Instant::now();
    let index = snap.build_sorted_index(threads);
    let cats = snap.build_dir_categories();
    let sort_index_ms = t2.elapsed().as_millis() as u64;
    let stats = LoadStats {
        generate_ms,
        snapshot_ms,
        sort_index_ms,
        nodes: snap.node_count(),
        dirs: snap.dir_count(),
    };
    (snap.with_sorted_index(Arc::new(index)).with_dir_categories(Arc::new(cats)), stats)
}

/// Simulated scan: `rate` is nodes per second (None means unlimited); publish a snapshot every `tick`.
/// Publish a final snapshot with the sorted index on completion.
pub fn simulate(
    spec: &GenSpec,
    rate: Option<u64>,
    tick: Duration,
    threads: usize,
    cancel: &AtomicBool,
    publish: &mut dyn FnMut(Snapshot, PublishInfo),
) {
    let mut tree = TreeBuilder::new();
    let start = Instant::now();
    let mut last = Instant::now();
    let mut hook = |t: &mut TreeBuilder| -> bool {
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        if let Some(rate) = rate {
            let due = Duration::from_secs_f64(t.node_count() as f64 / rate as f64);
            let now = start.elapsed();
            if due > now {
                std::thread::sleep(due - now);
            }
        }
        if last.elapsed() >= tick {
            last = Instant::now();
            let s0 = Instant::now();
            let snap = t.snapshot();
            let info = PublishInfo {
                nodes: snap.node_count(),
                snapshot_us: s0.elapsed().as_micros() as u64,
                elapsed_ms: start.elapsed().as_millis() as u64,
                final_publish: false,
            };
            publish(snap, info);
        }
        true
    };
    generate(spec, &mut tree, &mut hook);
    if cancel.load(Ordering::Relaxed) {
        return;
    }
    tree.set_complete();
    let s0 = Instant::now();
    let snap = tree.snapshot();
    drop(tree);
    let index = snap.build_sorted_index(threads);
    let cats = snap.build_dir_categories();
    let info = PublishInfo {
        nodes: snap.node_count(),
        snapshot_us: s0.elapsed().as_micros() as u64,
        elapsed_ms: start.elapsed().as_millis() as u64,
        final_publish: true,
    };
    publish(snap.with_sorted_index(Arc::new(index)).with_dir_categories(Arc::new(cats)), info);
}
