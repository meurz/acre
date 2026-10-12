//! Live scan driver: run acre-core on a background thread and periodically publish progress and
//! incremental snapshots to the UI, then return an indexed [`RealTree`] on completion.
//!
//! - Directory walking: core emits new nodes every 200 ms in parent-before-child order. Insert them into
//!   [`TreeBuilder`] and publish [`Snapshot`]. Both trees allocate node IDs in discovery order, so IDs stay
//!   stable across the final swap, preserving expansion, selection, and treemap navigation.
//! - File-table scanning (elevated): parent links become available only after the table is read; publish counts until then.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use acre_core::scan::{Backend, Phase, ScanOptions};
use acre_core::tree::{Batch, DiskTree, Flags};
use acre_core::{BackendUsed, ScanError};

use crate::real::{IndexStats, RealTree};
use crate::tree::{NONE, Snapshot, TreeBuilder};

/// Progress during a scan.
#[derive(Clone, Copy, Debug, Default)]
pub struct LiveProgress {
    /// Number of files and folders discovered.
    pub items: u64,
    /// Allocated bytes discovered.
    pub bytes: u64,
    pub elapsed: Duration,
    /// Reading has finished and aggregation is in progress.
    pub linking: bool,
}

/// Completed scan.
pub struct LiveDone {
    pub tree: RealTree,
    pub backend: BackendUsed,
    /// Scan duration, excluding indexing.
    pub scan: Duration,
    /// Indexing duration.
    pub index: IndexStats,
    pub notes: Vec<String>,
    /// Unreadable folders or entries during directory walking.
    pub unreadable: u64,
}

pub enum LiveEvent {
    Progress(LiveProgress),
    /// Intermediate result during directory walking.
    Partial(Snapshot, LiveProgress),
    Done(Box<LiveDone>),
    Failed(String),
    Cancelled,
}

/// Scan options: `root` is a drive root (`C:\`) or folder; false `allow_mft` forces directory walking.
pub fn options(root: &str, allow_mft: bool) -> ScanOptions {
    let mut o = ScanOptions::new(root);
    o.backend = if allow_mft { Backend::Auto } else { Backend::Walk };
    o.batches = true;
    o.batch_size = 50_000;
    o
}

/// Run a scan on this thread. The caller creates the thread; setting `cancel` returns `Cancelled` promptly.
pub fn run(opts: ScanOptions, tick: Duration, cancel: &AtomicBool, publish: &mut dyn FnMut(LiveEvent)) {
    let t0 = Instant::now();
    let root_name = acre_core::scan::root_display_name(&opts.root);
    let handle = acre_core::start(opts);
    let mut builder: Option<Ingest> = None;
    loop {
        let finished = handle.is_finished();
        if cancel.load(Ordering::Relaxed) {
            handle.cancel();
        }
        let batches = handle.take_batches();
        let p = handle.progress();
        let mut prog = LiveProgress {
            items: p.items,
            bytes: p.bytes,
            elapsed: t0.elapsed(),
            linking: p.phase == Phase::Linking,
        };
        if !batches.is_empty() && !cancel.load(Ordering::Relaxed) {
            let b = builder.get_or_insert_with(|| Ingest::new(&root_name));
            for batch in &batches {
                b.add(batch);
            }
            prog.items = prog.items.max(b.tree.node_count() as u64);
            publish(LiveEvent::Partial(b.tree.snapshot(), prog));
        } else {
            publish(LiveEvent::Progress(prog));
        }
        if finished {
            break;
        }
        std::thread::sleep(tick);
    }
    drop(builder);
    let scan = t0.elapsed();
    match handle.join() {
        Ok(out) => {
            let unreadable = out.tree.summary().unreadable_dirs;
            let (tree, index) = RealTree::new(out.tree, 1_000_000);
            publish(LiveEvent::Done(Box::new(LiveDone {
                tree,
                backend: out.backend,
                scan,
                index,
                notes: out.notes,
                unreadable,
            })));
        }
        Err(ScanError::Cancelled) => publish(LiveEvent::Cancelled),
        Err(e) => publish(LiveEvent::Failed(e.to_string())),
    }
}

/// Scan one folder and return the core tree, without intermediate results, for folder rescans.
pub fn scan_folder(root: &str, allow_mft: bool, cancel: &AtomicBool) -> Result<DiskTree, String> {
    let mut o = ScanOptions::new(root);
    o.backend = if allow_mft { Backend::Auto } else { Backend::Walk };
    let handle = acre_core::start(o);
    while !handle.is_finished() {
        if cancel.load(Ordering::Relaxed) {
            handle.cancel();
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    handle.join().map(|o| o.tree).map_err(|e| e.to_string())
}

/// Ingest core batches into the chunked snapshot tree.
struct Ingest {
    tree: TreeBuilder,
    /// Core node ID to snapshot node ID.
    map: Vec<u32>,
}

impl Ingest {
    fn new(root_name: &str) -> Self {
        let mut tree = TreeBuilder::new();
        let root = tree.add_dir(NONE, root_name);
        Ingest { tree, map: vec![root] }
    }

    fn add(&mut self, batch: &Batch) {
        for e in &batch.entries {
            let Some(&parent) = self.map.get(e.parent as usize) else { continue };
            if parent == NONE {
                continue;
            }
            let name = batch.name(e);
            let id = if e.flags.contains(Flags::DIR) {
                self.tree.add_dir(parent, name)
            } else {
                let ext = self.tree.intern_ext(&ext_key(name));
                self.tree.add_file(parent, name, ext, e.alloc)
            };
            let i = e.id as usize;
            if self.map.len() <= i {
                self.map.resize(i + 1, NONE);
            }
            self.map[i] = id;
        }
    }
}

/// Lowercase suffix after the last dot; missing, leading, or trailing dots and suffixes over 32 bytes mean no extension.
/// Matches acre-core's finalization rules.
pub fn ext_key(name: &str) -> String {
    match name.rfind('.') {
        Some(dot) if dot > 0 && dot + 1 < name.len() && name.len() - dot - 1 <= 32 => name[dot + 1..].to_ascii_lowercase(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read::TreeRead;

    #[test]
    fn ext_rules_match_core() {
        assert_eq!(ext_key("a.TXT"), "txt");
        assert_eq!(ext_key("a.tar.GZ"), "gz");
        assert_eq!(ext_key(".gitignore"), "");
        assert_eq!(ext_key("noext"), "");
        assert_eq!(ext_key("trailing."), "");
        assert_eq!(ext_key(&format!("x.{}", "a".repeat(40))), "");
    }

    /// Scan a temporary directory: intermediate and final IDs match, and the final tree has child and extension indices.
    #[test]
    fn walk_scan_publishes_partial_and_final_with_same_ids() {
        let dir = std::env::temp_dir().join(format!("acre-live-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for d in 0..30 {
            let sub = dir.join(format!("d{d:02}"));
            std::fs::create_dir_all(sub.join("inner")).unwrap();
            for f in 0..40 {
                std::fs::write(sub.join(format!("f{f}.bin")), vec![0u8; 100 * (f + 1)]).unwrap();
            }
            std::fs::write(sub.join("inner/x.TXT"), vec![1u8; 5000]).unwrap();
        }
        let root = dir.to_string_lossy().into_owned();
        let cancel = AtomicBool::new(false);
        let mut partials: Vec<Snapshot> = Vec::new();
        let mut done = None;
        run(options(&root, false), Duration::from_millis(1), &cancel, &mut |ev| match ev {
            LiveEvent::Partial(s, _) => partials.push(s),
            LiveEvent::Done(d) => done = Some(d),
            LiveEvent::Failed(e) => panic!("{e}"),
            _ => {}
        });
        let done = done.expect("done");
        let t = &done.tree as &dyn TreeRead;
        assert_eq!(done.backend, BackendUsed::Walk);
        assert_eq!(t.file_count(), 30 * 41);
        assert_eq!(t.dir_count(), 1 + 30 * 2);
        let last = partials.last().expect("at least one partial snapshot");
        let snap = last as &dyn TreeRead;
        // Each intermediate node ID identifies the same path in the final tree.
        for id in 0..snap.slot_count() {
            assert_eq!(snap.path(id), t.path(id), "id {id}");
        }
        // Sorted index: children in descending allocation order.
        let d0 = t.find_path(&format!("{root}\\d00")).unwrap();
        let kids = t.sorted_children_slice(d0).unwrap();
        assert_eq!(kids.len(), 41);
        assert!(kids.windows(2).all(|w| t.size(w[0]) >= t.size(w[1])));
        let txt = (0..t.ext_count() as u32).find(|&e| t.ext_name(e) == "txt").unwrap();
        assert_eq!(t.ext_stat(txt).files, 30);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
