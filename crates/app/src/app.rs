//! Main window: drives on the home screen; after scanning, a tree at top left, extension statistics at top right, and a treemap below.
//!
//! Data flow: the acre-core scan thread periodically publishes progress and an immutable tree to a mailbox that keeps only the latest value.
//! The UI polls every 50 ms, then computes visible rows and extension statistics in the background before swapping them in together.
//! Treemap layout and bitmap rendering use a separate background channel; the UI only displays the bitmap and hover outlines.
//! The UI never accesses data being mutated by the scan thread or performs work proportional to the total node count.
//!
//! Recycle Bin deletion, permanent deletion, and folder rescans run in the background and replace the tree on completion ([`RealTree::with_removed`]).
//! If permanent deletion leaves failed items, recount the folder with [`RealTree::with_replaced`].
//!
//! Developer options, hidden from the normal UI:
//! - `--synthetic[=nodes]` or `ACRE_SYNTHETIC`: load synthetic benchmark data and show dataset and scan-simulation controls.
//! - `--no-elevate` or `ACRE_NO_ELEVATE=1`: disable automatic elevation at startup.
//! - Test bridge: enabled only for the Windows user named test when `ACRE_TEST_DIR` is set.

use std::cell::Cell;
use std::collections::HashSet;
use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{
    Disableable as _, Icon, IconName, Selectable as _, Sizable as _, TitleBar, WindowExt as _,
    button::{Button, ButtonGroup, ButtonVariant, ButtonVariants as _},
    h_resizable, resizable_panel,
    skeleton::Skeleton,
    v_resizable,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use acre_model::core::BackendUsed;
use acre_model::core::volume::DriveInfo;
use acre_model::format;
use acre_model::generate::GenSpec;
use acre_model::layout::{KIND_DIR, KIND_DIR_LEAF, KIND_FILE, KIND_REST, Layout, LayoutParams, TmRect, layout};
use acre_model::live::{self, LiveDone, LiveEvent, LiveProgress};
use acre_model::real::RealTree;
use acre_model::render::{RenderParams, render};
use acre_model::scan::{LoadStats, PublishInfo, load, simulate};
use acre_model::{ExtStat, NONE, Snapshot, TreeRead};
use serde_json::{Value, json};
use acre_i18n::{self as i18n, Language, Message, msg, tr, t};

use crate::perf::{self, Perf, ms_since_start};
use crate::theme::*;

actions!(acre, [
    /// Permanently delete the selected item (Shift+Delete, matching Explorer).
    DeletePermanently,
    /// Navigate one level up in the treemap (Backspace).
    ZoomOut,
]);

/// Keyboard context for the main window.
const KEY_CONTEXT: &str = "Acre";

pub const ROW_H: f32 = 26.;
const HEADER_H: f32 = 28.;
const POLL_MS: u64 = 50;
/// Flag passed to the elevated child process to prevent repeated relaunches.
pub const RELAUNCHED_FLAG: &str = "--relaunched";

type Tree = dyn TreeRead;

// ====================================================================== Launch arguments
#[derive(Clone, Debug, Default)]
pub struct Launch {
    /// Synthetic data, for development.
    pub synthetic: Option<u64>,
    pub no_elevate: bool,
    pub relaunched: bool,
}

impl Launch {
    pub fn from_args(args: &[String]) -> Self {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let mut l = Launch {
            synthetic: env("ACRE_SYNTHETIC").map(|v| v.parse().unwrap_or(1_000_000)),
            no_elevate: env("ACRE_NO_ELEVATE").is_some_and(|v| v != "0"),
            relaunched: false,
        };
        for a in args {
            if a == "--synthetic" {
                l.synthetic = Some(1_000_000);
            } else if let Some(n) = a.strip_prefix("--synthetic=") {
                l.synthetic = Some(n.parse().unwrap_or(1_000_000));
            } else if a == "--no-elevate" {
                l.no_elevate = true;
            } else if a == RELAUNCHED_FLAG {
                l.relaunched = true;
            }
        }
        l
    }

    /// Relaunch elevated when the current user is an administrator but the process is not elevated, unless disabled.
    /// Do not attempt elevation for standard users, such as the test account, because it would prompt for credentials.
    #[cfg(windows)]
    pub fn should_elevate(&self) -> bool {
        use acre_model::core::volume;
        self.synthetic.is_none() && !self.no_elevate && !self.relaunched && !volume::is_elevated() && volume::can_elevate()
    }
}

fn is_elevated() -> bool {
    #[cfg(windows)]
    {
        acre_model::core::volume::is_elevated()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn list_drives() -> Vec<DriveInfo> {
    #[cfg(windows)]
    {
        acre_model::core::volume::list_drives()
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

// ====================================================================== State
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RenderMode {
    /// Bitmap with cushion shading.
    Cushion,
    /// Bitmap with flat colors.
    Flat,
    /// One GPU rectangle per block; a synthetic-mode benchmark comparison without bitmaps.
    Quads,
}

impl RenderMode {
    fn key(self) -> &'static str {
        match self {
            RenderMode::Cushion => "cushion",
            RenderMode::Flat => "flat",
            RenderMode::Quads => "quads",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Row {
    pub node: u32,
    pub depth: u16,
    pub expanded: bool,
}

enum Incoming {
    Loaded(Snapshot, LoadStats),
    Sim(Snapshot, PublishInfo),
    Live(LiveEvent),
}

/// Block targeted by the treemap context menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TmTarget {
    /// A file or folder.
    Node(u32),
    /// Aggregated smaller items: containing folder and item count. This is not a real file and cannot be deleted.
    Rest { dir: u32, count: u32 },
}

/// Deletion or folder-rescan result. An absent `tree` means no change, such as a single file that could not be deleted.
enum OpResult {
    Mutated {
        epoch: u64,
        tree: Option<Arc<RealTree>>,
        note: Option<Message>,
        fallback: u32,
        op: &'static str,
        ms: f64,
        delete: Option<DeleteSummary>,
    },
    Failed { epoch: u64, message: Message },
}

/// Permanent-deletion progress: updated by the worker and read by the status bar every 50 ms.
#[derive(Default)]
struct DeleteJob {
    #[cfg(windows)]
    progress: acre_model::core::delete::DeleteProgress,
    /// File count recorded in the tree, used as the progress denominator.
    total_files: u64,
    /// Deletion has finished; recounting after some items could not be deleted.
    rescanning: AtomicBool,
}

impl DeleteJob {
    /// Deleted (file count, byte count).
    fn deleted(&self) -> (u64, u64) {
        #[cfg(windows)]
        {
            (self.progress.files(), self.progress.bytes())
        }
        #[cfg(not(windows))]
        {
            (0, 0)
        }
    }
}

/// Permanent-deletion result for the UI.
#[derive(Clone, Debug, Default)]
struct DeleteSummary {
    name: String,
    removed: bool,
    files: u64,
    bytes: u64,
    failed: u64,
    /// Failed items as (reason, path), capped at [`acre_model::core::delete::MAX_LISTED_FAILURES`].
    items: Vec<(Message, String)>,
    delete_ms: f64,
    /// Reason why the recount after deletion failed.
    rescan_error: Option<String>,
}

/// Permanently delete on the current thread, called from a background worker.
fn delete_permanently(path: &str, job: &DeleteJob) -> DeleteSummary {
    let t0 = Instant::now();
    #[cfg(windows)]
    let mut s = {
        let r = acre_model::core::delete::delete_permanently(Path::new(path), &job.progress);
        DeleteSummary {
            removed: r.removed,
            files: r.files,
            bytes: r.bytes,
            failed: r.failed,
            items: r
                .failures
                .iter()
                .map(|f| {
                    let why = delete_reason(f.kind, &f.detail);
                    (why, f.path.display().to_string())
                })
                .collect(),
            ..DeleteSummary::default()
        }
    };
    #[cfg(not(windows))]
    let mut s = {
        let _ = job;
        DeleteSummary { failed: 1, items: vec![("Windows only".into(), path.to_string())], ..DeleteSummary::default() }
    };
    s.delete_ms = t0.elapsed().as_secs_f64() * 1000.0;
    s
}

#[cfg(windows)]
fn delete_reason(kind: acre_model::core::recycle::DeleteFailure, detail: &str) -> Message {
    use acre_model::core::recycle::DeleteFailure;
    match kind {
        DeleteFailure::InUse => "File is in use".into(),
        DeleteFailure::AccessDenied => "Access denied".into(),
        DeleteFailure::NotFound => "File not found".into(),
        DeleteFailure::Other if detail.is_empty() => "Other error".into(),
        DeleteFailure::Other => Message::new("{detail}", vec![("detail", detail.to_string())]),
    }
}

/// Mailbox that keeps only the latest value.
#[derive(Default)]
struct Mailbox {
    slot: Mutex<Option<(u64, Incoming)>>,
    published: AtomicU64,
    max_snapshot_us: AtomicU64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    /// Home screen: choose a drive or folder.
    Home,
    /// Generating synthetic data.
    Loading,
    Ready,
    Scanning,
}

#[derive(Clone, Copy, Debug)]
struct ScanResult {
    backend: BackendUsed,
    secs: f64,
    index_ms: f64,
    unreadable: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct TmKey {
    generation: u64,
    epoch: u64,
    root: u32,
    w: u32,
    h: u32,
    mode: RenderMode,
    min_area_x10: u32,
    /// Name labels are enabled, so the layout reserves folder headers.
    labels: bool,
    scale_x100: u32,
}

struct TmResult {
    key: TmKey,
    snap: Arc<Tree>,
    layout: Arc<Layout>,
    image: Option<Arc<RenderImage>>,
    layout_us: u64,
    render_us: u64,
}

struct DataResult {
    epoch: u64,
    snap: Arc<Tree>,
    rows: Vec<Row>,
    ext_rows: Vec<(u32, ExtStat)>,
    rows_us: u64,
    ext_us: u64,
    info: Option<PublishInfo>,
    marker: Option<&'static str>,
}

/// Sorted visible rows: traverse from the root, listing expanded directories' children by descending size.
pub fn build_rows(snap: &Tree, expanded: &HashSet<u32>) -> Vec<Row> {
    let mut rows = Vec::new();
    let root = snap.root();
    if root == NONE {
        return rows;
    }
    let mut stack: Vec<(u32, u16)> = vec![(root, 0)];
    while let Some((node, depth)) = stack.pop() {
        let open = snap.is_dir(node) && expanded.contains(&node);
        rows.push(Row { node, depth, expanded: open });
        if open {
            if let Some(slice) = snap.sorted_children_slice(node) {
                stack.extend(slice.iter().rev().map(|&c| (c, depth + 1)));
            } else {
                let kids = snap.sorted_children(node);
                stack.extend(kids.iter().rev().map(|&c| (c, depth + 1)));
            }
        }
    }
    rows
}

pub fn build_ext_rows(snap: &Tree) -> Vec<(u32, ExtStat)> {
    let mut v: Vec<(u32, ExtStat)> =
        (0..snap.ext_count() as u32).map(|e| (e, snap.ext_stat(e))).filter(|(_, s)| s.files > 0).collect();
    v.sort_unstable_by(|a, b| b.1.bytes.cmp(&a.1.bytes).then(a.0.cmp(&b.0)));
    v
}

pub struct DiskApp {
    launch: Launch,
    elevated: bool,
    phase: Phase,
    dataset: u64,
    epoch: u64,
    /// Drive or folder currently being viewed.
    target: Option<String>,
    drives: Vec<DriveInfo>,
    snap: Option<Arc<Tree>>,
    /// Completed scan tree, cloned as the starting point for deletion and folder rescans.
    real: Option<Arc<RealTree>>,
    load_stats: Option<LoadStats>,
    load_started: Instant,
    load_pending: bool,
    mailbox: Arc<Mailbox>,
    ops: Arc<Mutex<Vec<OpResult>>>,
    cancel: Arc<AtomicBool>,
    scan_rate: Option<u64>,
    scan_started: Option<Instant>,
    scan_last: Option<PublishInfo>,
    live: LiveProgress,
    result: Option<ScanResult>,
    /// Status-bar message for cancellation or errors.
    message: Option<(Message, bool)>,
    /// Deletion or rescan currently in progress.
    busy: Option<(Message, Instant)>,
    /// Permanent deletion currently in progress, with counts shown in the status bar.
    delete_job: Option<Arc<DeleteJob>>,
    /// Main-window focus handle for shortcuts such as Shift+Delete.
    focus: FocusHandle,
    progress_text: String,
    scan_applied: u32,
    data_inflight: bool,
    data_dirty: bool,
    pending_snapshot: Option<(Arc<Tree>, Option<PublishInfo>)>,

    // Directory tree.
    expanded: HashSet<u32>,
    rows: Arc<Vec<Row>>,
    /// Selection shared by the tree and treemap; an explicit click, separate from hover state.
    selected: Option<u32>,
    /// Selected aggregate of smaller items within a folder; `selected` is absent in this case.
    sel_rest: Option<u32>,
    tree_scroll: UniformListScrollHandle,
    tree_bounds: Rc<Cell<Bounds<Pixels>>>,

    // Extensions.
    ext_rows: Arc<Vec<(u32, ExtStat)>>,
    ext_scroll: UniformListScrollHandle,

    // Treemap.
    zoom: u32,
    mode: RenderMode,
    min_area: f32,
    tm_bounds: Rc<Cell<Bounds<Pixels>>>,
    tm_scale: Rc<Cell<f32>>,
    tm_want_size: Rc<Cell<(u32, u32)>>,
    tm_result: Option<TmResult>,
    tm_inflight: bool,
    tm_pending: Option<TmKey>,
    tm_stale: Vec<Arc<RenderImage>>,
    tm_resize_seq: u64,
    hover: Option<usize>,
    mouse: Point<Pixels>,
    sel_rect: Option<usize>,
    /// Block highlighted while the context menu is open.
    tm_menu: Option<usize>,
    /// Treemap name labels; can be disabled in development to compare timings.
    labels: bool,
    /// Last label build: (count, microseconds).
    label_stats: Rc<Cell<(u32, u64)>>,

    perf: Perf,
    weak: WeakEntity<DiskApp>,
    views: Views,
    last_hud: (u32, f64, f64),
    hud_at: Instant,
    test_dir: Option<std::path::PathBuf>,
    last_command: String,
    _poll: Task<()>,
}

impl DiskApp {
    pub fn new(launch: Launch, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let language = i18n::initialize();
        gpui_kit::component::set_locale(language.tag());
        let dataset = launch.synthetic.unwrap_or(1_000_000);
        let test_dir = std::env::var("ACRE_TEST_DIR")
            .ok()
            .filter(|v| !v.is_empty())
            .filter(|_| std::env::var("USERNAME").unwrap_or_default().eq_ignore_ascii_case("test"))
            .map(std::path::PathBuf::from);
        let poll = cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(POLL_MS)).await;
                if this.update_in(cx, |this, window, cx| this.tick(window, cx)).is_err() {
                    break;
                }
            }
        });
        let weak = cx.weak_entity();
        let views = Views::new(&weak, cx);
        let synthetic = launch.synthetic.is_some();
        cx.bind_keys([
            KeyBinding::new("shift-delete", DeletePermanently, Some(KEY_CONTEXT)),
            KeyBinding::new("backspace", ZoomOut, Some(KEY_CONTEXT)),
        ]);
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let mut app = Self {
            launch,
            elevated: is_elevated(),
            phase: Phase::Home,
            dataset,
            epoch: 0,
            target: None,
            drives: if synthetic { Vec::new() } else { list_drives() },
            snap: None,
            real: None,
            load_stats: None,
            load_started: Instant::now(),
            load_pending: false,
            mailbox: Arc::new(Mailbox::default()),
            ops: Arc::new(Mutex::new(Vec::new())),
            cancel: Arc::new(AtomicBool::new(false)),
            scan_rate: None,
            scan_started: None,
            scan_last: None,
            live: LiveProgress::default(),
            result: None,
            message: None,
            busy: None,
            delete_job: None,
            focus,
            progress_text: String::new(),
            scan_applied: 0,
            data_inflight: false,
            data_dirty: false,
            pending_snapshot: None,
            expanded: HashSet::new(),
            rows: Arc::new(Vec::new()),
            selected: None,
            sel_rest: None,
            tree_scroll: UniformListScrollHandle::new(),
            tree_bounds: Rc::new(Cell::new(Bounds::default())),
            ext_rows: Arc::new(Vec::new()),
            ext_scroll: UniformListScrollHandle::new(),
            zoom: NONE,
            mode: RenderMode::Cushion,
            min_area: 3.0,
            tm_bounds: Rc::new(Cell::new(Bounds::default())),
            tm_scale: Rc::new(Cell::new(1.0)),
            tm_want_size: Rc::new(Cell::new((0, 0))),
            tm_result: None,
            tm_inflight: false,
            tm_pending: None,
            tm_stale: Vec::new(),
            tm_resize_seq: 0,
            hover: None,
            mouse: Point::default(),
            sel_rect: None,
            tm_menu: None,
            labels: true,
            label_stats: Rc::new(Cell::new((0, 0))),
            perf: Perf::new(),
            weak,
            views,
            last_hud: (0, 0.0, 0.0),
            hud_at: Instant::now(),
            test_dir,
            last_command: String::new(),
            _poll: poll,
        };
        if synthetic {
            app.start_load(dataset, cx);
        }
        app
    }

    fn change_language(&mut self, language: Language, window: &mut Window, cx: &mut Context<Self>) {
        i18n::set_language(language);
        gpui_kit::component::set_locale(language.tag());
        if let Err(error) = i18n::persist(language) {
            window.push_notification(Notification::error(tr!("Could not save language preference: {error}", error = error)), cx);
        }
        if self.synthetic() {
            self.target = Some(tr!("Synthetic data: {count} items", count = format::count(self.dataset)));
        }
        self.progress_text = self.progress_line();
        self.changed(cx);
    }

    fn synthetic(&self) -> bool {
        self.launch.synthetic.is_some()
    }

    /// Show frame timing only in developer modes: synthetic data or the test bridge.
    fn dev(&self) -> bool {
        self.synthetic() || self.test_dir.is_some()
    }

    fn threads() -> usize {
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4)
    }

    fn reset_view(&mut self) {
        self.snap = None;
        self.real = None;
        self.rows = Arc::new(Vec::new());
        self.ext_rows = Arc::new(Vec::new());
        self.expanded.clear();
        self.selected = None;
        self.sel_rest = None;
        self.zoom = NONE;
        if let Some(r) = self.tm_result.take()
            && let Some(img) = r.image
        {
            self.tm_stale.push(img);
        }
        self.tm_pending = None;
        self.hover = None;
        self.sel_rect = None;
        self.tm_menu = None;
        self.pending_snapshot = None;
        self.data_dirty = false;
        self.scan_last = None;
        self.scan_applied = 0;
        self.live = LiveProgress::default();
        self.result = None;
        self.message = None;
        self.busy = None;
        self.delete_job = None;
        self.mailbox.published.store(0, Ordering::Relaxed);
        self.mailbox.max_snapshot_us.store(0, Ordering::Relaxed);
        self.tree_scroll.scroll_to_item(0, ScrollStrategy::Top);
    }

    fn stop_worker(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.cancel = Arc::new(AtomicBool::new(false));
        *self.mailbox.slot.lock().unwrap() = None;
    }

    // ------------------------------------------------------------------ Synthetic data (development)
    pub fn start_load(&mut self, nodes: u64, cx: &mut Context<Self>) {
        self.stop_worker();
        self.epoch += 1;
        self.dataset = nodes;
        self.phase = Phase::Loading;
        self.reset_view();
        self.target = Some(tr!("Synthetic data: {count} items", count = format::count(nodes)));
        self.load_stats = None;
        self.load_started = Instant::now();
        self.load_pending = true;
        self.perf.begin("load", self.load_started, json!({"nodes": nodes}));
        let epoch = self.epoch;
        let mailbox = self.mailbox.clone();
        std::thread::Builder::new()
            .name("acre-load".into())
            .spawn(move || {
                let (snap, stats) = load(&GenSpec::tier(nodes), Self::threads());
                *mailbox.slot.lock().unwrap() = Some((epoch, Incoming::Loaded(snap, stats)));
            })
            .expect("load thread");
        self.changed(cx);
    }

    pub fn start_sim_scan(&mut self, rate: Option<u64>, cx: &mut Context<Self>) {
        self.stop_worker();
        self.epoch += 1;
        self.phase = Phase::Scanning;
        self.reset_view();
        self.load_stats = None;
        self.scan_rate = rate;
        self.scan_started = Some(Instant::now());
        let epoch = self.epoch;
        let mailbox = self.mailbox.clone();
        let cancel = self.cancel.clone();
        let nodes = self.dataset;
        std::thread::Builder::new()
            .name("acre-sim".into())
            .spawn(move || {
                simulate(&GenSpec::tier(nodes), rate, Duration::from_millis(200), Self::threads(), &cancel, &mut |snap, info| {
                    mailbox.published.fetch_add(1, Ordering::Relaxed);
                    if !info.final_publish {
                        mailbox.max_snapshot_us.fetch_max(info.snapshot_us, Ordering::Relaxed);
                    }
                    *mailbox.slot.lock().unwrap() = Some((epoch, Incoming::Sim(snap, info)));
                });
            })
            .expect("sim thread");
        self.changed(cx);
    }

    // ------------------------------------------------------------------ Real scans
    /// Scan a drive (`C:\`) or folder.
    pub fn start_scan(&mut self, path: String, cx: &mut Context<Self>) {
        if self.synthetic() {
            return;
        }
        self.stop_worker();
        self.epoch += 1;
        self.phase = Phase::Scanning;
        self.reset_view();
        self.target = Some(path.clone());
        self.scan_started = Some(Instant::now());
        self.perf.push_op(json!({"name": "scan-start", "path": path, "elevated": self.elevated, "at_ms": ms_since_start(Instant::now())}));
        let epoch = self.epoch;
        let mailbox = self.mailbox.clone();
        let cancel = self.cancel.clone();
        let opts = live::options(&path, self.elevated);
        std::thread::Builder::new()
            .name("acre-scan".into())
            .spawn(move || {
                live::run(opts, Duration::from_millis(200), &cancel, &mut |ev| {
                    let mut slot = mailbox.slot.lock().unwrap();
                    match ev {
                        // A progress-only update must not overwrite an unconsumed intermediate tree.
                        LiveEvent::Progress(p) => {
                            if let Some((e, Incoming::Live(LiveEvent::Partial(_, old)))) = slot.as_mut()
                                && *e == epoch
                            {
                                *old = p;
                            } else {
                                *slot = Some((epoch, Incoming::Live(LiveEvent::Progress(p))));
                            }
                        }
                        LiveEvent::Partial(s, p) => {
                            mailbox.published.fetch_add(1, Ordering::Relaxed);
                            *slot = Some((epoch, Incoming::Live(LiveEvent::Partial(s, p))));
                        }
                        other => *slot = Some((epoch, Incoming::Live(other))),
                    }
                });
            })
            .expect("scan thread");
        self.changed(cx);
    }

    fn rescan_target(&mut self, cx: &mut Context<Self>) {
        if self.synthetic() {
            let rate = Some(if self.dataset >= 3_000_000 { 400_000 } else { 150_000 });
            self.start_sim_scan(rate, cx);
        } else if let Some(t) = self.target.clone() {
            self.start_scan(t, cx);
        }
    }

    fn cancel_scan(&mut self, cx: &mut Context<Self>) {
        if self.synthetic() {
            self.stop_worker();
            self.phase = Phase::Ready;
            self.changed(cx);
            return;
        }
        // For real scans, request cancellation and wait for the scan thread's cancelled event.
        self.cancel.store(true, Ordering::Relaxed);
        self.changed(cx);
    }

    fn choose_folder(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions { files: false, directories: true, multiple: false, prompt: None });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await
                && let Some(p) = paths.into_iter().next()
            {
                let s = p.to_string_lossy().into_owned();
                let _ = this.update(cx, |this, cx| this.start_scan(s, cx));
            }
        })
        .detach();
    }

    fn go_home(&mut self, cx: &mut Context<Self>) {
        self.stop_worker();
        self.epoch += 1;
        self.reset_view();
        self.target = None;
        self.phase = Phase::Home;
        self.drives = list_drives();
        self.changed(cx);
    }

    // ------------------------------------------------------------------ Timer
    fn tick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.perf.poll();
        let incoming = self.mailbox.slot.lock().unwrap().take();
        if let Some((epoch, msg)) = incoming
            && epoch == self.epoch
        {
            match msg {
                Incoming::Loaded(snap, stats) => {
                    let snap: Arc<Tree> = Arc::new(snap);
                    self.load_stats = Some(stats);
                    self.perf.push_op(json!({
                        "name": "data-ready",
                        "at_ms": ms_since_start(Instant::now()),
                        "nodes": stats.nodes, "dirs": stats.dirs,
                        "generate_ms": stats.generate_ms, "snapshot_ms": stats.snapshot_ms,
                        "sort_index_ms": stats.sort_index_ms,
                        "since_request_ms": self.load_started.elapsed().as_secs_f64() * 1000.0,
                    }));
                    self.phase = Phase::Ready;
                    self.apply_snapshot_sync(snap, None, cx);
                }
                Incoming::Sim(snap, info) => {
                    self.scan_last = Some(info);
                    let snap: Arc<Tree> = Arc::new(snap);
                    if info.final_publish {
                        self.phase = Phase::Ready;
                        self.perf.push_op(json!({
                            "name": "scan-complete", "nodes": info.nodes, "elapsed_ms": info.elapsed_ms,
                            "publishes": self.mailbox.published.load(Ordering::Relaxed),
                            "max_snapshot_us": self.mailbox.max_snapshot_us.load(Ordering::Relaxed),
                            "applied": self.scan_applied,
                        }));
                        self.apply_snapshot_sync(snap, Some(info), cx);
                    } else {
                        self.queue_data(snap, Some(info), cx);
                    }
                }
                Incoming::Live(ev) => self.on_live(ev, cx),
            }
        }
        let ops: Vec<OpResult> = std::mem::take(&mut *self.ops.lock().unwrap());
        for op in ops {
            self.apply_op(op, window, cx);
        }
        if self.phase == Phase::Scanning || self.busy.is_some() {
            let t = self.progress_line();
            if t != self.progress_text {
                self.progress_text = t;
                self.touch(V_TITLE | V_STATUS, cx);
            }
        }
        if self.dev() && self.hud_at.elapsed() >= Duration::from_millis(500) {
            self.hud_at = Instant::now();
            let hud = self.perf.hud();
            if hud != self.last_hud {
                self.last_hud = hud;
                self.touch(V_STATUS, cx);
            }
        }
        if self.test_dir.is_some() {
            self.poll_bridge(window, cx);
        }
    }

    fn on_live(&mut self, ev: LiveEvent, cx: &mut Context<Self>) {
        match ev {
            LiveEvent::Progress(p) => self.live = p,
            LiveEvent::Partial(s, p) => {
                self.live = p;
                self.queue_data(Arc::new(s), None, cx);
            }
            LiveEvent::Done(d) => self.on_scan_done(*d, cx),
            LiveEvent::Failed(e) => {
                self.message = Some((msg!("Scan failed: {error}", error = e), true));
                self.phase = if self.snap.is_some() { Phase::Ready } else { Phase::Home };
                self.changed(cx);
            }
            LiveEvent::Cancelled => {
                self.message = Some(("Cancelled".into(), false));
                self.phase = if self.snap.is_some() { Phase::Ready } else { Phase::Home };
                if self.phase == Phase::Home {
                    self.target = None;
                    self.drives = list_drives();
                }
                self.changed(cx);
            }
        }
    }

    fn on_scan_done(&mut self, d: LiveDone, cx: &mut Context<Self>) {
        let index_ms = (d.index.sort_us + d.index.categories_us) as f64 / 1000.0;
        let secs = self.scan_started.map(|t| t.elapsed().as_secs_f64()).unwrap_or(d.scan.as_secs_f64());
        self.result = Some(ScanResult { backend: d.backend, secs, index_ms, unreadable: d.unreadable });
        let real = Arc::new(d.tree);
        self.perf.push_op(json!({
            "name": "scan-complete",
            "at_ms": ms_since_start(Instant::now()),
            "backend": format!("{:?}", d.backend),
            "scan_ms": d.scan.as_secs_f64() * 1000.0,
            "sort_index_us": d.index.sort_us, "categories_us": d.index.categories_us,
            "total_ms": secs * 1000.0,
            "files": real.file_count(), "dirs": real.dir_count(),
            "tree_bytes": real.memory_bytes(),
            "partials": self.mailbox.published.load(Ordering::Relaxed),
            "notes": d.notes,
        }));
        self.real = Some(real.clone());
        self.phase = Phase::Ready;
        self.apply_snapshot_sync(real, None, cx);
    }

    /// For complete trees with a sorted index, compute visible rows on the UI thread by copying pre-sorted slices.
    fn apply_snapshot_sync(&mut self, snap: Arc<Tree>, info: Option<PublishInfo>, cx: &mut Context<Self>) {
        let t = Instant::now();
        self.fix_view_state(&*snap);
        let rows = build_rows(&*snap, &self.expanded);
        let rows_us = t.elapsed().as_micros() as u64;
        let t = Instant::now();
        let ext = build_ext_rows(&*snap);
        let ext_us = t.elapsed().as_micros() as u64;
        self.apply_data(
            DataResult { epoch: self.epoch, snap, rows, ext_rows: ext, rows_us, ext_us, info, marker: None },
            cx,
        );
    }

    /// On a new tree, expand the root by default and clear invalid expansion/selection state or reset treemap navigation to the root.
    fn fix_view_state(&mut self, snap: &Tree) {
        let root = snap.root();
        if root == NONE {
            return;
        }
        self.expanded.retain(|&n| snap.contains(n) && snap.is_dir(n));
        if self.expanded.is_empty() {
            self.expanded.insert(root);
        }
        if self.zoom == NONE || !snap.contains(self.zoom) || !snap.is_dir(self.zoom) {
            self.zoom = root;
        }
        if let Some(s) = self.selected
            && !snap.contains(s)
        {
            self.selected = None;
        }
        if let Some(d) = self.sel_rest
            && !snap.contains(d)
        {
            self.sel_rest = None;
        }
    }

    /// During scanning, compute visible rows and extension statistics in the background, then swap them in together; keep only the latest pending tree.
    fn queue_data(&mut self, snap: Arc<Tree>, info: Option<PublishInfo>, cx: &mut Context<Self>) {
        if self.data_inflight {
            self.pending_snapshot = Some((snap, info));
            self.data_dirty = true;
            return;
        }
        self.data_inflight = true;
        self.fix_view_state(&*snap);
        let expanded = self.expanded.clone();
        let epoch = self.epoch;
        let marker = if info.is_none() { Some("expand") } else { None };
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let t = Instant::now();
                    let rows = build_rows(&*snap, &expanded);
                    let rows_us = t.elapsed().as_micros() as u64;
                    let t = Instant::now();
                    let ext_rows = build_ext_rows(&*snap);
                    let ext_us = t.elapsed().as_micros() as u64;
                    DataResult { epoch, snap, rows, ext_rows, rows_us, ext_us, info, marker }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.data_inflight = false;
                // The final tree is already installed; discard a late intermediate result.
                let stale = !result.snap.complete() && this.snap.as_ref().is_some_and(|s| s.complete()) && result.info.is_none();
                if !stale {
                    this.apply_data(result, cx);
                }
                if let Some((snap, info)) = this.pending_snapshot.take() {
                    this.data_dirty = false;
                    this.queue_data(snap, info, cx);
                }
            });
        })
        .detach();
    }

    fn apply_data(&mut self, r: DataResult, cx: &mut Context<Self>) {
        if r.epoch != self.epoch {
            return;
        }
        let rows_len = r.rows.len();
        self.snap = Some(r.snap.clone());
        self.rows = Arc::new(r.rows);
        self.ext_rows = Arc::new(r.ext_rows);
        if self.phase == Phase::Scanning {
            self.scan_applied += 1;
            self.perf.push_op(json!({
                "name": "scan-tick",
                "at_ms": ms_since_start(Instant::now()),
                "nodes": r.snap.slot_count(), "rows": rows_len, "rows_us": r.rows_us, "ext_us": r.ext_us,
                "snapshot_us": r.info.map(|i| i.snapshot_us),
            }));
        }
        if let Some(name) = r.marker {
            self.perf.ready(name, json!({"rows": rows_len, "rows_us": r.rows_us, "background": true}));
        }
        if let Some(sel) = self.selected
            && !r.snap.contains(sel)
        {
            self.selected = None;
        }
        self.request_layout(cx);
        self.changed(cx);
    }

    fn progress_line(&self) -> String {
        if let Some((what, t)) = &self.busy {
            let secs = t.elapsed().as_secs_f64().floor();
            if let Some(job) = &self.delete_job {
                if job.rescanning.load(Ordering::Relaxed) {
                    return tr!("{what} · Recounting · {seconds} s", what = what, seconds = format!("{secs:.0}"));
                }
                let (files, bytes) = job.deleted();
                return tr!("{what} · Deleted {files} / {total} files · {size} · {seconds} s",
                    what = what, files = format::count(files), total = format::count(job.total_files),
                    size = format::bytes(bytes), seconds = format!("{secs:.0}"));
            }
            return tr!("{what} · {seconds} s", what = what, seconds = format!("{secs:.0}"));
        }
        if self.synthetic() {
            let info = self.scan_last.unwrap_or_default();
            return tr!("Found {count} items", count = format::count(info.nodes as u64));
        }
        let secs = self.scan_started.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0);
        if self.cancel.load(Ordering::Relaxed) {
            return t("Cancelling…").into();
        }
        tr!("Scanned {count} items · {seconds} s", count = format::count(self.live.items), seconds = format!("{secs:.1}"))
    }

    // ------------------------------------------------------------------ Context-menu actions
    fn node_path(&self, node: u32) -> Option<String> {
        let snap = self.snap.as_ref()?;
        snap.contains(node).then(|| snap.path(node))
    }

    fn reveal_in_explorer(&mut self, node: u32, cx: &mut Context<Self>) {
        let Some(path) = self.node_path(node) else { return };
        let is_root = self.snap.as_ref().is_some_and(|s| s.root() == node);
        if is_root {
            cx.open_with_system(Path::new(&path));
        } else {
            cx.reveal_path(Path::new(&path));
        }
    }

    /// Open the folder itself in Explorer, used for the containing folder of an aggregate block.
    fn open_folder(&mut self, node: u32, cx: &mut Context<Self>) {
        let Some(path) = self.node_path(node) else { return };
        cx.open_with_system(Path::new(&path));
    }

    fn copy_path(&mut self, node: u32, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.node_path(node) else { return };
        cx.write_to_clipboard(ClipboardItem::new_string(path));
        window.push_notification(Notification::success(t("Path copied")), cx);
    }

    fn confirm_delete(&mut self, node: u32, window: &mut Window, cx: &mut Context<Self>) {
        let Some(snap) = self.snap.clone() else { return };
        if self.real.is_none() || self.busy.is_some() || !snap.contains(node) || node == snap.root() || snap.is_special(node) {
            return;
        }
        let path = snap.path(node);
        let name = display_name(snap.name(node), snap.is_special(node));
        let mut detail = format::bytes(snap.size(node));
        if snap.is_dir(node) {
            detail.push_str(&tr!(" · {count} files", count = format::count(snap.files(node))));
        }
        let app = self.weak.clone();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let app = app.clone();
            let path_ok = path.clone();
            dialog
                .title(t("Move to Recycle Bin?"))
                .description(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(4.))
                        .child(txt(name.clone(), 13., TEXT))
                        .child(txt(detail.clone(), 12., MUTED))
                        .child(txt(path.clone(), 11., DIM).overflow_hidden().text_ellipsis()),
                )
                .show_cancel(true)
                .ok_text(t("Delete"))
                .ok_variant(ButtonVariant::Danger)
                .cancel_text(t("Cancel"))
                .on_ok(move |_, _, cx| {
                    let p = path_ok.clone();
                    let _ = app.update(cx, |this, cx| this.start_delete(node, p, cx));
                    true
                })
        });
    }

    fn start_delete(&mut self, node: u32, path: String, cx: &mut Context<Self>) {
        let Some(real) = self.real.clone() else { return };
        if self.busy.is_some() {
            return;
        }
        let name = real.name(node).to_string();
        let size = format::bytes(real.size(node));
        let fallback = real.parent(node);
        self.busy = Some((msg!("Deleting {name}", name = name), Instant::now()));
        let ops = self.ops.clone();
        let epoch = self.epoch;
        std::thread::Builder::new()
            .name("acre-delete".into())
            .spawn(move || {
                let t0 = Instant::now();
                #[cfg(windows)]
                let r = acre_model::core::recycle::move_to_recycle_bin(Path::new(&path)).map_err(|e| delete_reason(e.kind, &e.detail));
                #[cfg(not(windows))]
                let r: Result<(), Message> = Err("Windows only".into());
                let gone = std::fs::symlink_metadata(&path).is_err();
                let res = if r.is_ok() || gone {
                    let (tree, _) = real.with_removed(node);
                    OpResult::Mutated {
                        epoch,
                        tree: Some(Arc::new(tree)),
                        note: Some(msg!("Moved to Recycle Bin: {name} ({size})", name = name, size = size)),
                        fallback,
                        op: "delete",
                        ms: t0.elapsed().as_secs_f64() * 1000.0,
                        delete: None,
                    }
                } else {
                    OpResult::Failed { epoch, message: msg!("Could not delete {path}: {reason}", path = path).with_message("reason", r.err().unwrap_or_else(|| "Other error".into())) }
                };
                ops.lock().unwrap().push(res);
            })
            .expect("delete thread");
        self.changed(cx);
    }

    /// Whether this node can be deleted, shared by Recycle Bin and permanent deletion.
    fn can_delete(&self, node: u32) -> bool {
        let Some(snap) = &self.snap else { return false };
        self.real.is_some()
            && self.busy.is_none()
            && self.phase == Phase::Ready
            && snap.contains(node)
            && node != snap.root()
            && !snap.is_special(node)
    }

    /// Shift+Delete: permanently delete the selected item.
    fn delete_selected_permanently(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(node) = self.selected {
            self.confirm_delete_permanently(node, window, cx);
        }
    }

    fn confirm_delete_permanently(&mut self, node: u32, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_delete(node) {
            return;
        }
        let Some(snap) = self.snap.clone() else { return };
        let path = snap.path(node);
        let name = display_name(snap.name(node), snap.is_special(node));
        let mut detail = format::bytes(snap.size(node));
        if snap.is_dir(node) {
            detail.push_str(&tr!(" · {count} files", count = format::count(snap.files(node))));
        }
        let app = self.weak.clone();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let app = app.clone();
            let path_ok = path.clone();
            dialog
                .title(t("Delete permanently?"))
                .description(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(4.))
                        .child(txt(name.clone(), 13., TEXT))
                        .child(txt(detail.clone(), 12., MUTED))
                        .child(txt(path.clone(), 11., DIM).overflow_hidden().text_ellipsis())
                        .child(div().pt(px(6.)).child(txt(t("This bypasses the Recycle Bin and cannot be undone."), 12., TEXT))),
                )
                .show_cancel(true)
                .ok_text(t("Delete permanently"))
                .ok_variant(ButtonVariant::Danger)
                .cancel_text(t("Cancel"))
                .on_ok(move |_, _, cx| {
                    let p = path_ok.clone();
                    let _ = app.update(cx, |this, cx| this.start_delete_permanently(node, p, cx));
                    true
                })
        });
    }

    fn start_delete_permanently(&mut self, node: u32, path: String, cx: &mut Context<Self>) {
        if !self.can_delete(node) {
            return;
        }
        let Some(real) = self.real.clone() else { return };
        let name = real.name(node).to_string();
        let size = format::bytes(real.size(node));
        let is_dir = real.is_dir(node);
        let fallback = real.parent(node);
        let job = Arc::new(DeleteJob { total_files: (&*real as &Tree).files(node), ..DeleteJob::default() });
        self.busy = Some((msg!("Permanently deleting {name}", name = name), Instant::now()));
        self.delete_job = Some(job.clone());
        self.perf.push_op(json!({"name": "hard-delete-start", "path": path, "files": job.total_files, "at_ms": ms_since_start(Instant::now())}));
        let ops = self.ops.clone();
        let epoch = self.epoch;
        let elevated = self.elevated;
        let cancel = self.cancel.clone();
        std::thread::Builder::new()
            .name("acre-hard-delete".into())
            .spawn(move || {
                let t0 = Instant::now();
                let mut summary = delete_permanently(&path, &job);
                summary.name = name.clone();
                let res = if summary.removed {
                    let (tree, _) = real.with_removed(node);
                    OpResult::Mutated {
                        epoch,
                        tree: Some(Arc::new(tree)),
                        note: Some(msg!("Permanently deleted: {name} ({size})", name = name, size = size)),
                        fallback,
                        op: "hard-delete",
                        ms: t0.elapsed().as_secs_f64() * 1000.0,
                        delete: Some(summary),
                    }
                } else if is_dir && Path::new(&path).is_dir() {
                    // Deletion partially succeeded; recount this folder.
                    job.rescanning.store(true, Ordering::Relaxed);
                    let tree = match live::scan_folder(&path, elevated, &cancel) {
                        Ok(fresh) => Some(Arc::new(real.with_replaced(node, &fresh))),
                        Err(e) => {
                            summary.rescan_error = Some(e);
                            None
                        }
                    };
                    OpResult::Mutated {
                        epoch,
                        tree,
                        note: None,
                        fallback,
                        op: "hard-delete",
                        ms: t0.elapsed().as_secs_f64() * 1000.0,
                        delete: Some(summary),
                    }
                } else {
                    OpResult::Mutated { epoch, tree: None, note: None, fallback, op: "hard-delete", ms: t0.elapsed().as_secs_f64() * 1000.0, delete: Some(summary) }
                };
                ops.lock().unwrap().push(res);
            })
            .expect("hard delete thread");
        self.changed(cx);
    }

    /// Explain which items could not be permanently deleted and why.
    fn show_delete_failures(&mut self, s: &DeleteSummary, window: &mut Window, cx: &mut Context<Self>) {
        const SHOWN: usize = 6;
        let title = if s.files == 0 && s.items.len() <= 1 {
            t("Could not delete").to_string()
        } else {
            tr!("{count} items could not be deleted", count = format::count(s.failed))
        };
        let mut rows: Vec<(String, String)> = s.items.iter().take(SHOWN).map(|(why, path)| (why.to_string(), path.clone())).collect();
        if rows.is_empty() {
            rows.push((t("Other error").into(), s.name.clone()));
        }
        let more = s.failed.saturating_sub(rows.len() as u64);
        let mut tail = Vec::new();
        if more > 0 {
            tail.push(tr!("{count} more items not listed", count = format::count(more)));
        }
        if s.files > 0 {
            tail.push(tr!("Deleted the rest: {count} files, {size}", count = format::count(s.files), size = format::bytes(s.bytes)));
        }
        match &s.rescan_error {
            Some(e) => tail.push(tr!("Recount failed ({error}). Right-click and choose “Rescan this folder”.", error = e)),
            None if s.files > 0 || s.failed > 1 => tail.push(t("Folder recounted").into()),
            None => {}
        }
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let mut list = div().flex().flex_col().gap(px(6.));
            for (why, path) in &rows {
                list = list.child(
                    div()
                        .flex()
                        .flex_col()
                        .child(txt(why.clone(), 12., TEXT))
                        .child(txt(path.clone(), 11., DIM).overflow_hidden().whitespace_nowrap().text_ellipsis()),
                );
            }
            for t in &tail {
                list = list.child(txt(t.clone(), 12., MUTED));
            }
            dialog.title(title.clone()).description(list).ok_text(t("OK"))
        });
    }

    fn rescan_folder(&mut self, node: u32, cx: &mut Context<Self>) {
        let Some(snap) = self.snap.clone() else { return };
        if node == snap.root() {
            self.rescan_target(cx);
            return;
        }
        let Some(real) = self.real.clone() else { return };
        if self.busy.is_some() || !real.contains(node) || !real.is_dir(node) {
            return;
        }
        let path = (&*real as &Tree).path(node);
        let name = real.name(node).to_string();
        self.busy = Some((msg!("Rescanning {name}", name = name), Instant::now()));
        let ops = self.ops.clone();
        let epoch = self.epoch;
        let elevated = self.elevated;
        let cancel = self.cancel.clone();
        std::thread::Builder::new()
            .name("acre-rescan".into())
            .spawn(move || {
                let t0 = Instant::now();
                let res = match live::scan_folder(&path, elevated, &cancel) {
                    Ok(fresh) => OpResult::Mutated {
                        epoch,
                        tree: Some(Arc::new(real.with_replaced(node, &fresh))),
                        note: None,
                        fallback: real.parent(node),
                        op: "rescan-folder",
                        ms: t0.elapsed().as_secs_f64() * 1000.0,
                        delete: None,
                    },
                    Err(e) => OpResult::Failed { epoch, message: msg!("Rescan failed: {error}", error = e) },
                };
                ops.lock().unwrap().push(res);
            })
            .expect("rescan thread");
        self.changed(cx);
    }

    fn apply_op(&mut self, op: OpResult, window: &mut Window, cx: &mut Context<Self>) {
        match op {
            OpResult::Mutated { epoch, tree, note, fallback, op, ms, delete } => {
                if epoch != self.epoch {
                    return;
                }
                self.busy = None;
                self.delete_job = None;
                let mut entry = json!({"name": op, "ms": ms, "at_ms": ms_since_start(Instant::now())});
                if let Some(d) = &delete {
                    entry["delete_ms"] = json!(d.delete_ms);
                    entry["files"] = json!(d.files);
                    entry["bytes"] = json!(d.bytes);
                    entry["failed"] = json!(d.failed);
                    entry["removed"] = json!(d.removed);
                }
                self.perf.push_op(entry);
                if let Some(tree) = tree {
                    if !tree.contains(self.zoom) && tree.contains(fallback) {
                        self.zoom = fallback;
                    }
                    self.real = Some(tree.clone());
                    self.apply_snapshot_sync(tree, None, cx);
                } else {
                    self.changed(cx);
                }
                if let Some(n) = note {
                    window.push_notification(Notification::success(n.to_string()), cx);
                }
                if let Some(d) = delete
                    && d.failed > 0
                {
                    self.show_delete_failures(&d, window, cx);
                }
            }
            OpResult::Failed { epoch, message } => {
                if epoch != self.epoch {
                    return;
                }
                self.busy = None;
                self.delete_job = None;
                window.push_notification(Notification::error(message.to_string()), cx);
                self.changed(cx);
            }
        }
    }

    // ------------------------------------------------------------------ Treemap
    fn current_key(&self) -> Option<TmKey> {
        let snap = self.snap.as_ref()?;
        let (w, h) = self.tm_want_size.get();
        if w < 8 || h < 8 || self.zoom == NONE {
            return None;
        }
        Some(TmKey {
            generation: snap.generation(),
            epoch: self.epoch,
            root: self.zoom,
            w,
            h,
            mode: self.mode,
            min_area_x10: (self.min_area * 10.0) as u32,
            labels: self.labels,
            scale_x100: (self.tm_scale.get() * 100.0).round() as u32,
        })
    }

    fn request_layout(&mut self, cx: &mut Context<Self>) {
        let Some(key) = self.current_key() else { return };
        if self.tm_result.as_ref().map(|r| r.key) == Some(key) {
            return;
        }
        if self.tm_inflight {
            self.tm_pending = Some(key);
            return;
        }
        let Some(snap) = self.snap.clone() else { return };
        self.tm_inflight = true;
        let t0 = Instant::now();
        let scale = self.tm_scale.get();
        let params = if key.labels {
            LayoutParams {
                min_area: self.min_area,
                header: (LABEL_H * scale).round(),
                header_min_w: HEADER_MIN_W * scale,
                header_min_h: HEADER_MIN_H * scale,
                ..Default::default()
            }
        } else {
            LayoutParams { min_area: self.min_area, ..Default::default() }
        };
        let palette = palette();
        let threads = Self::threads();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let lt = Instant::now();
                    let l = layout(&*snap, key.root, key.w as f32, key.h as f32, &params);
                    let layout_us = lt.elapsed().as_micros() as u64;
                    let rt = Instant::now();
                    let image = if key.mode == RenderMode::Quads {
                        None
                    } else {
                        let (w, h) = (key.w as usize, key.h as usize);
                        let mut buf = vec![0u8; w * h * 4];
                        let rp = RenderParams { cushion: key.mode == RenderMode::Cushion, header: header_rgb(), ..Default::default() };
                        render(&l, &palette, &rp, &mut buf, w, h, threads);
                        let img = image::RgbaImage::from_raw(key.w, key.h, buf).expect("bitmap");
                        Some(Arc::new(RenderImage::new(vec![image::Frame::new(img)])))
                    };
                    let render_us = rt.elapsed().as_micros() as u64;
                    TmResult { key, snap, layout: Arc::new(l), image, layout_us, render_us }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.tm_inflight = false;
                this.apply_layout(result, t0, cx);
                // State changed again during the job (navigation, snapshot, or size); recompute using the current state.
                this.tm_pending = None;
                this.request_layout(cx);
            });
        })
        .detach();
    }

    fn apply_layout(&mut self, r: TmResult, t0: Instant, cx: &mut Context<Self>) {
        if r.key.epoch != self.epoch {
            if let Some(img) = r.image {
                self.tm_stale.push(img);
            }
            return;
        }
        let first = self.load_pending && self.phase != Phase::Loading;
        let extra = json!({
            "layout_us": r.layout_us, "render_us": r.render_us,
            "rects": r.layout.rects.len(), "leaves": r.layout.leaf_count(),
            "dirs_subdivided": r.layout.stats.dirs_subdivided,
            "children_scanned": r.layout.stats.children_scanned,
            "w": r.key.w, "h": r.key.h, "mode": r.key.mode.key(), "generation": r.key.generation,
            "root": r.key.root,
        });
        self.perf.markers.push(perf::Marker { name: "treemap".into(), t0, ready: Some(Instant::now()), extra: extra.clone() });
        self.perf.ready("zoom", extra.clone());
        self.perf.ready("relayout", extra);
        if let Some(old) = self.tm_result.take()
            && let Some(img) = old.image
        {
            self.tm_stale.push(img);
        }
        self.tm_result = Some(r);
        self.hover = None;
        self.tm_menu = None;
        self.update_sel_rect();
        if first {
            self.load_pending = false;
            if self.perf.content_marker.is_none() {
                self.perf.content_marker = Some(Instant::now());
            }
            self.perf.ready("load", json!({"nodes": self.snap.as_ref().map(|s| s.slot_count())}));
        }
        self.changed(cx);
    }

    /// Find the selected node's treemap block; if it is too small and merged with smaller items, highlight that aggregate.
    /// If it is inside an unsubdivided folder block, highlight that folder instead.
    fn update_sel_rect(&mut self) {
        self.sel_rect = None;
        let Some(r) = &self.tm_result else { return };
        let rects = &r.layout.rects;
        if let Some(dir) = self.sel_rest {
            self.sel_rect = rects.iter().position(|x| x.kind == KIND_REST && x.node == dir);
            return;
        }
        let Some(sel) = self.selected else { return };
        if let Some(i) = rects.iter().position(|x| x.node == sel && x.kind != KIND_REST) {
            self.sel_rect = Some(i);
            return;
        }
        let snap = &r.snap;
        if !snap.contains(sel) {
            return;
        }
        // Follow the root-to-selection chain to find the deepest node with a block.
        let chain = snap.ancestry(sel);
        let mut best: Option<(usize, usize)> = None;
        for (i, x) in rects.iter().enumerate() {
            if x.kind == KIND_REST {
                continue;
            }
            if let Some(ci) = chain.iter().position(|&n| n == x.node)
                && best.is_none_or(|(bc, _)| ci > bc)
            {
                best = Some((ci, i));
            }
        }
        let Some((_, ri)) = best else { return };
        if rects[ri].kind == KIND_DIR
            && let Some(rest) = rects.iter().position(|x| x.kind == KIND_REST && x.parent as usize == ri)
        {
            self.sel_rect = Some(rest);
            return;
        }
        self.sel_rect = Some(ri);
    }

    fn on_tm_resized(&mut self, cx: &mut Context<Self>) {
        // Debounce window resizing: relayout after 120 ms without changes; the first layout runs immediately.
        if self.tm_result.is_none() {
            self.request_layout(cx);
            return;
        }
        self.tm_resize_seq += 1;
        let seq = self.tm_resize_seq;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(120)).await;
            let _ = this.update(cx, |this, cx| {
                if this.tm_resize_seq == seq {
                    this.perf.begin("relayout", Instant::now(), json!({"reason": "resize"}));
                    this.request_layout(cx);
                }
            });
        })
        .detach();
    }

    pub fn zoom_to(&mut self, node: u32, cx: &mut Context<Self>) {
        let Some(snap) = &self.snap else { return };
        if node == NONE || !snap.contains(node) {
            return;
        }
        let target = if snap.is_dir(node) { node } else { snap.parent(node) };
        if target == NONE || target == self.zoom {
            return;
        }
        self.perf.begin("zoom", Instant::now(), json!({"target": target, "from": self.zoom}));
        self.zoom = target;
        self.hover = None;
        self.request_layout(cx);
        self.changed(cx);
    }

    fn zoom_up(&mut self, cx: &mut Context<Self>) {
        let Some(snap) = &self.snap else { return };
        if self.zoom == NONE {
            return;
        }
        let p = snap.parent(self.zoom);
        if p != NONE {
            self.navigate_to(p, cx);
        }
    }

    /// Navigating into or back to a folder in the treemap also selects it in the directory tree.
    fn navigate_to(&mut self, dir: u32, cx: &mut Context<Self>) {
        self.zoom_to(dir, cx);
        if self.zoom == dir {
            self.reveal(dir, false, ScrollStrategy::Nearest, cx);
        }
    }

    /// If the selection is outside the current treemap, navigate to its parent, or to the root for a root selection.
    fn keep_in_treemap(&mut self, node: u32, cx: &mut Context<Self>) {
        let Some(snap) = self.snap.clone() else { return };
        if self.zoom == NONE || !snap.contains(node) || snap.ancestry(node).contains(&self.zoom) {
            return;
        }
        let p = snap.parent(node);
        self.zoom_to(if p == NONE { node } else { p }, cx);
    }

    fn can_zoom_up(&self) -> bool {
        self.snap.as_ref().is_some_and(|s| self.zoom != NONE && s.contains(self.zoom) && s.parent(self.zoom) != NONE)
    }

    /// Whether deletion and rescanning are allowed: a completed real scan with no operation in progress.
    fn editable(&self) -> bool {
        self.real.is_some() && self.busy.is_none() && self.phase == Phase::Ready
    }

    fn tm_target(&self, i: usize) -> Option<TmTarget> {
        let rect = self.tm_result.as_ref()?.layout.rects.get(i)?;
        Some(match rect.kind {
            KIND_REST => TmTarget::Rest { dir: rect.node, count: rect.rest_count },
            _ => TmTarget::Node(rect.node),
        })
    }

    fn tm_local(&self, pos: Point<Pixels>) -> Point<Pixels> {
        let b = self.tm_bounds.get();
        point(pos.x - b.origin.x, pos.y - b.origin.y)
    }

    fn tm_hit(&self, local: Point<Pixels>) -> Option<usize> {
        let r = self.tm_result.as_ref()?;
        let s = self.tm_scale.get();
        let (x, y) = (local.x.as_f32() * s, local.y.as_f32() * s);
        if x < 0.0 || y < 0.0 || x >= r.layout.width || y >= r.layout.height {
            return None;
        }
        r.layout.hit(x, y)
    }

    // ------------------------------------------------------------------ Directory tree
    fn rebuild_rows(&mut self, marker: &'static str, t0: Instant, cx: &mut Context<Self>) {
        let Some(snap) = self.snap.clone() else { return };
        if snap.has_sorted_index() {
            let t = Instant::now();
            let rows = build_rows(&*snap, &self.expanded);
            let rows_us = t.elapsed().as_micros() as u64;
            let n = rows.len();
            self.rows = Arc::new(rows);
            self.perf.instant(marker, t0, json!({"rows": n, "rows_us": rows_us, "background": false}));
            self.changed(cx);
        } else {
            self.perf.begin(marker, t0, json!({}));
            self.queue_data(snap, None, cx);
        }
    }

    pub fn toggle(&mut self, node: u32, cx: &mut Context<Self>) {
        let t0 = Instant::now();
        let opening = !self.expanded.contains(&node);
        if opening {
            self.expanded.insert(node);
        } else {
            self.expanded.remove(&node);
        }
        let children = self.snap.as_ref().map(|s| s.child_count(node)).unwrap_or(0);
        let name = if opening { "expand" } else { "collapse" };
        self.perf.push_op(json!({"name": format!("{name}-request"), "children": children, "at_ms": ms_since_start(t0)}));
        self.rebuild_rows(if opening { "expand" } else { "collapse" }, t0, cx);
    }

    /// A tree selection also highlights the treemap block; navigate to its parent if outside the current view.
    fn select(&mut self, node: u32, cx: &mut Context<Self>) {
        self.selected = Some(node);
        self.sel_rest = None;
        self.keep_in_treemap(node, cx);
        self.update_sel_rect();
        self.changed(cx);
    }

    /// Expand the path to a node, select it, scroll it into view, and synchronize the treemap.
    pub fn reveal(&mut self, node: u32, expand_self: bool, scroll: ScrollStrategy, cx: &mut Context<Self>) -> Option<usize> {
        let snap = self.snap.clone()?;
        let t0 = Instant::now();
        for a in snap.ancestry(node) {
            if a != node || expand_self {
                self.expanded.insert(a);
            }
        }
        self.rebuild_rows("expand", t0, cx);
        self.selected = Some(node);
        self.sel_rest = None;
        self.keep_in_treemap(node, cx);
        self.update_sel_rect();
        let ix = self.rows.iter().position(|r| r.node == node);
        if let Some(ix) = ix {
            self.tree_scroll.scroll_to_item(ix, scroll);
        }
        self.changed(cx);
        ix
    }

    fn path_to_node(&self, path: &str) -> Option<u32> {
        self.snap.as_ref()?.find_path(path)
    }

    fn tree_scroll_top(&self) -> usize {
        let st = self.tree_scroll.0.borrow();
        st.deferred_scroll_to_item
            .as_ref()
            .map(|d| d.item_index)
            .unwrap_or_else(|| ((-st.base_handle.offset().y.as_f32()) / ROW_H).floor().max(0.0) as usize)
    }
}

// ====================================================================== Rendering
//
// Split the UI into subviews, each using GPUI's `Entity::cached`; views that are not notified reuse the previous frame.
// Treemap hover only notifies the overlaid hover view for outlines and tooltips.
// The bitmap, selection outline, labels, tree, extension table, title bar, and status bar do not rebuild their element trees.
// Rebuilding a cached view also rebuilds nested cached views, so the hover layer is a sibling overlay outside the treemap view.
// All state lives in DiskApp; subviews take an immutable snapshot when rendering.

pub const V_TITLE: u16 = 1;
pub const V_TREE: u16 = 2;
pub const V_EXT: u16 = 4;
pub const V_TM: u16 = 8;
pub const V_STATUS: u16 = 16;
pub const V_TM_HEAD: u16 = 32;
pub const V_HOME: u16 = 64;
pub const V_TM_LABELS: u16 = 128;
pub const V_TM_HOVER: u16 = 256;
pub const V_ALL: u16 = 511;

/// Treemap path-bar height and surrounding padding, also used to align the hover overlay.
const TM_HEAD_H: f32 = 36.;
const TM_PAD: f32 = 6.;

pub struct Views {
    title: Entity<TitleView>,
    tree: Entity<TreeView>,
    ext: Entity<ExtView>,
    tm: Entity<TreemapView>,
    tm_head: Entity<TmHeaderView>,
    status: Entity<StatusView>,
    home: Entity<HomeView>,
    tm_labels: Entity<TmLabelsView>,
    tm_hover: Entity<TmHoverView>,
}

impl Views {
    fn new(app: &WeakEntity<DiskApp>, cx: &mut Context<DiskApp>) -> Self {
        let tm_head = cx.new(|_| TmHeaderView { app: app.clone() });
        Self {
            title: cx.new(|_| TitleView { app: app.clone() }),
            tree: cx.new(|_| TreeView { app: app.clone(), data: None }),
            ext: cx.new(|_| ExtView { app: app.clone(), data: None }),
            tm: cx.new(|_| TreemapView { app: app.clone(), header: tm_head.clone() }),
            tm_head,
            status: cx.new(|_| StatusView { app: app.clone() }),
            home: cx.new(|_| HomeView { app: app.clone() }),
            tm_labels: cx.new(|_| TmLabelsView { app: app.clone() }),
            tm_hover: cx.new(|_| TmHoverView { app: app.clone() }),
        }
    }
}

/// Forward event callbacks from subviews to DiskApp.
fn act<E: ?Sized + 'static>(
    app: &WeakEntity<DiskApp>,
    f: impl Fn(&mut DiskApp, &E, &mut Context<DiskApp>) + 'static,
) -> impl Fn(&E, &mut Window, &mut App) + 'static {
    let app = app.clone();
    move |e, _w, cx| {
        let _ = app.update(cx, |this, cx| f(this, e, cx));
    }
}

fn txt(s: impl Into<SharedString>, size: f32, color: u32) -> Div {
    div().text_size(px(size)).line_height(px((size * 1.45).round())).text_color(c(color)).child(s.into())
}

fn num(s: impl Into<SharedString>, color: u32) -> Div {
    txt(s, 12., color).whitespace_nowrap()
}

impl DiskApp {
    pub fn touch(&self, which: u16, cx: &mut Context<Self>) {
        if which & V_TITLE != 0 {
            self.views.title.update(cx, |_, cx| cx.notify());
        }
        if which & V_TREE != 0 {
            self.views.tree.update(cx, |_, cx| cx.notify());
        }
        if which & V_EXT != 0 {
            self.views.ext.update(cx, |_, cx| cx.notify());
        }
        if which & V_TM != 0 {
            self.views.tm.update(cx, |_, cx| cx.notify());
        }
        if which & V_STATUS != 0 {
            self.views.status.update(cx, |_, cx| cx.notify());
        }
        if which & V_TM_HEAD != 0 {
            self.views.tm_head.update(cx, |_, cx| cx.notify());
        }
        if which & V_TM_LABELS != 0 {
            self.views.tm_labels.update(cx, |_, cx| cx.notify());
        }
        if which & V_TM_HOVER != 0 {
            self.views.tm_hover.update(cx, |_, cx| cx.notify());
        }
        if which & V_HOME != 0 {
            self.views.home.update(cx, |_, cx| cx.notify());
        }
    }

    /// Data or selection changed; rebuild all subviews.
    pub fn changed(&self, cx: &mut Context<Self>) {
        self.touch(V_ALL, cx);
        cx.notify();
    }

    fn on_tm_mouse_move(&mut self, pos: Point<Pixels>, cx: &mut Context<Self>) {
        if self.tm_menu.is_some() {
            // While a menu is open, keep its target block highlighted instead of following the pointer.
            return;
        }
        let local = self.tm_local(pos);
        self.mouse = local;
        self.hover = self.tm_hit(local);
        // Redraw only the hover overlay as its outline and tooltip follow the pointer.
        self.touch(V_TM_HOVER, cx);
    }

    /// On right-click, remember the block under the pointer and highlight it while the menu is open; [`treemap_menu`] uses this target.
    fn on_tm_right_down(&mut self, pos: Point<Pixels>, cx: &mut Context<Self>) {
        let local = self.tm_local(pos);
        self.mouse = local;
        self.tm_menu = self.tm_hit(local);
        self.hover = None;
        if let Some(i) = self.tm_menu {
            // The context-menu target also becomes the shared selection for menu actions and Shift+Delete.
            self.select_tm_rect(i, cx);
        }
        self.touch(V_TM | V_TM_HOVER, cx);
    }

    /// Single-click selects and reveals the block in the tree; double-click enters its containing folder one level below the current view.
    fn on_tm_click(&mut self, pos: Point<Pixels>, click_count: usize, cx: &mut Context<Self>) {
        let local = self.tm_local(pos);
        let Some(hit) = self.tm_hit(local) else { return };
        if click_count < 2 {
            self.select_tm_rect(hit, cx);
            return;
        }
        let Some(r) = &self.tm_result else { return };
        let rects = &r.layout.rects;
        let mut i = hit;
        while rects[i].depth > 1 && rects[i].parent != NONE {
            i = rects[i].parent as usize;
        }
        let top = rects[i];
        if matches!(top.kind, KIND_DIR | KIND_DIR_LEAF) && top.depth == 1 {
            self.navigate_to(top.node, cx);
        }
    }

    /// Selecting a treemap block expands, selects, and reveals it in the tree. For an aggregate, select the aggregate and expand its folder,
    /// then scroll to the first merged item without selecting a tree row; Shift+Delete must not act on aggregate blocks.
    fn select_tm_rect(&mut self, i: usize, cx: &mut Context<Self>) {
        let t0 = Instant::now();
        let Some(r) = &self.tm_result else { return };
        let rects = &r.layout.rects;
        let Some(rect) = rects.get(i).copied() else { return };
        let kept = if rect.kind == KIND_REST {
            rects.iter().filter(|x| x.parent == rect.parent && x.kind != KIND_REST).count()
        } else {
            0
        };
        let Some(snap) = self.snap.clone() else { return };
        if !snap.contains(rect.node) {
            return;
        }
        let ix = if rect.kind == KIND_REST {
            for a in snap.ancestry(rect.node) {
                self.expanded.insert(a);
            }
            self.rebuild_rows("expand", t0, cx);
            self.selected = None;
            self.sel_rest = Some(rect.node);
            self.update_sel_rect();
            let first = snap.sorted_children(rect.node).get(kept).copied().unwrap_or(rect.node);
            let ix = self.rows.iter().position(|row| row.node == first);
            if let Some(ix) = ix {
                self.tree_scroll.scroll_to_item(ix, ScrollStrategy::Nearest);
            }
            self.changed(cx);
            ix
        } else {
            self.reveal(rect.node, false, ScrollStrategy::Nearest, cx)
        };
        self.perf.instant(
            "tm-locate",
            t0,
            json!({"row": ix, "rows": self.rows.len(), "depth": snap.depth(rect.node), "kind": rect.kind}),
        );
    }

    /// Title-bar menu for drives and folders.
    fn target_menu(&self) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static {
        let app = self.weak.clone();
        let drives = self.drives.clone();
        move |mut menu, _, _| {
            for d in &drives {
                let mount = d.mount.clone();
                let label = drive_label(d);
                let app = app.clone();
                menu = menu.item(
                    PopupMenuItem::new(format!("{label}    {} / {}", format::bytes(d.used()), format::bytes(d.total)))
                        .icon(IconName::HardDrive)
                        .on_click(move |_, _, cx| {
                            let m = mount.clone();
                            let _ = app.update(cx, |this, cx| this.start_scan(m, cx));
                        }),
                );
            }
            let app = app.clone();
            menu.separator().item(PopupMenuItem::new(t("Choose folder…")).icon(IconName::FolderOpen).on_click(move |_, _, cx| {
                let _ = app.update(cx, |this, cx| this.choose_folder(cx));
            }))
        }
    }
}

fn drive_label(d: &DriveInfo) -> String {
    let letter = d.mount.trim_end_matches('\\');
    if d.label.is_empty() { letter.to_string() } else { format!("{letter}  {}", d.label) }
}

// ---------------------------------------------------------------- Title bar
pub struct TitleView {
    app: WeakEntity<DiskApp>,
}

impl Render for TitleView {
    fn render(&mut self, _w: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(entity) = self.app.upgrade() else { return TitleBar::new().h(px(38.)) };
        let a = entity.read(cx);
        let phase = a.phase;
        let scanning = phase == Phase::Scanning;
        let synthetic = a.synthetic();
        let ds = a.dataset;
        let target = a.target.clone();
        let is_drive = target.as_deref().is_some_and(|t| t.len() <= 3);
        let progress = a.progress_text.clone();
        let busy = a.busy.is_some();
        let menu = a.target_menu();
        let app = &self.app;
        let mut right = div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px(8.))
            .mr(px(8.))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation());
        let language_app = self.app.clone();
        right = right.child(
            ButtonGroup::new("language")
                .small()
                .outline()
                .child(Button::new("language-en").label(t("English")).selected(i18n::language() == Language::English))
                .child(Button::new("language-zh").label(t("Simplified Chinese")).selected(i18n::language() == Language::Chinese))
                .on_click(move |clicks: &Vec<usize>, window, cx| {
                    let _ = language_app.update(cx, |this, cx| {
                        this.change_language(if clicks.contains(&1) { Language::Chinese } else { Language::English }, window, cx);
                    });
                }),
        );
        if synthetic {
            right = right
                .child(
                    ButtonGroup::new("dataset")
                        .small()
                        .outline()
                        .child(Button::new("ds-1m").label(t("1 million items")).selected(ds < 3_000_000))
                        .child(Button::new("ds-5m").label(t("5 million items")).selected(ds >= 3_000_000))
                        .on_click(act(app, |this, clicks: &Vec<usize>, cx| {
                            let n = if clicks.contains(&1) { 5_000_000 } else { 1_000_000 };
                            if this.phase == Phase::Scanning {
                                this.dataset = n;
                                this.start_sim_scan(this.scan_rate, cx);
                            } else {
                                this.start_load(n, cx);
                            }
                        })),
                )
                .child(
                    Button::new("sim")
                        .small()
                        .when(scanning, |b| b.outline())
                        .when(!scanning, |b| b.primary())
                        .disabled(phase == Phase::Loading)
                        .icon(if scanning { IconName::Pause } else { IconName::Play })
                        .label(if scanning { t("Stop scan") } else { t("Simulate scan") })
                        .on_click(act(app, |this, _: &ClickEvent, cx| {
                            if this.phase == Phase::Scanning {
                                this.cancel_scan(cx);
                            } else {
                                this.rescan_target(cx);
                            }
                        })),
                );
        } else if scanning {
            right = right.child(txt(progress, 12., MUTED).whitespace_nowrap()).child(
                Button::new("cancel")
                    .small()
                    .outline()
                    .label(t("Cancel"))
                    .on_click(act(app, |this, _: &ClickEvent, cx| this.cancel_scan(cx))),
            );
        } else if phase == Phase::Ready {
            right = right.child(
                Button::new("rescan")
                    .small()
                    .ghost()
                    .icon(IconName::RefreshCw)
                    .label(t("Rescan"))
                    .disabled(busy)
                    .on_click(act(app, |this, _: &ClickEvent, cx| this.rescan_target(cx))),
            );
        }
        TitleBar::new().h(px(38.)).child(
            div()
                .flex()
                .items_center()
                .w_full()
                .h_full()
                .gap(px(10.))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(Icon::new(IconName::HardDrive).size(px(15.)).text_color(c(ACCENT)))
                        .child(txt(PRODUCT_NAME, 13., TEXT).font_weight(FontWeight::SEMIBOLD)),
                )
                .when_some(target.filter(|_| phase != Phase::Home), |d, t| {
                    d.child(
                        div().min_w_0().max_w(px(300.)).overflow_hidden().on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation()).child(
                            Button::new("target")
                                .ghost()
                                .small()
                                .icon(if is_drive { IconName::HardDrive } else { IconName::Folder })
                                .label(t)
                                .disabled(synthetic)
                                .dropdown_menu(menu),
                        ),
                    )
                })
                .child(div().flex_1())
                .child(right),
        )
    }
}

// ---------------------------------------------------------------- Home screen
pub struct HomeView {
    app: WeakEntity<DiskApp>,
}

impl Render for HomeView {
    fn render(&mut self, _w: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(entity) = self.app.upgrade() else { return div() };
        let a = entity.read(cx);
        let app = &self.app;
        let mut list = div().flex().flex_col().gap(px(6.)).w(px(560.));
        for (i, d) in a.drives.iter().enumerate() {
            let mount = d.mount.clone();
            let frac = if d.total > 0 { d.used() as f32 / d.total as f32 } else { 0.0 };
            list = list.child(
                div()
                    .id(("drive", i))
                    .flex()
                    .items_center()
                    .gap(px(14.))
                    .px(px(16.))
                    .py(px(12.))
                    .rounded(px(8.))
                    .bg(c(SURFACE))
                    .cursor_pointer()
                    .hover(|d| d.bg(c(HOVER)))
                    .active(|d| d.bg(c(PRESS)))
                    .on_click(act(app, move |this, _: &ClickEvent, cx| this.start_scan(mount.clone(), cx)))
                    .child(Icon::new(IconName::HardDrive).size(px(20.)).text_color(c(MUTED)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(6.))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(10.))
                                    .child(txt(drive_label(d), 14., TEXT).font_weight(FontWeight::MEDIUM).whitespace_nowrap())
                                    .child(txt(d.filesystem.clone(), 11., DIM))
                                    .child(div().flex_1())
                                    .child(num(
                                        tr!("Used {used} / {total}", used = format::bytes(d.used()), total = format::bytes(d.total)),
                                        MUTED,
                                    )),
                            )
                            .child(
                                div()
                                    .w_full()
                                    .h(px(4.))
                                    .rounded(px(2.))
                                    .bg(bar_track())
                                    .child(div().h_full().rounded(px(2.)).w(relative(frac.clamp(0.0, 1.0))).bg(bar_fill())),
                            ),
                    ),
            );
        }
        let message = a.message.clone();
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .pt(px(72.))
            .gap(px(14.))
            .bg(c(BG))
            .child(div().w(px(560.)).child(txt(t("Drives"), 18., TEXT).font_weight(FontWeight::SEMIBOLD)))
            .child(list)
            .child(
                div().w(px(560.)).flex().items_center().gap(px(12.)).child(
                    Button::new("pick-folder")
                        .outline()
                        .icon(IconName::FolderOpen)
                        .label(t("Choose folder…"))
                        .on_click(act(app, |this, _: &ClickEvent, cx| this.choose_folder(cx))),
                ),
            )
            .when(!a.elevated, |d| {
                d.child(div().w(px(560.)).child(txt(t("Standard access: scans take longer and some folders may be inaccessible."), 12., DIM)))
            })
            .when_some(message, |d, (m, err)| d.child(div().w(px(560.)).child(txt(m.to_string(), 12., if err { ERR } else { MUTED }))))
    }
}

// ---------------------------------------------------------------- Status bar
pub struct StatusView {
    app: WeakEntity<DiskApp>,
}

impl Render for StatusView {
    fn render(&mut self, _w: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(app) = self.app.upgrade() else { return div() };
        let a = app.read(cx);
        let (fps, last, max) = a.last_hud;
        let mut left: Option<SharedString> = match (&a.snap, a.phase) {
            (_, Phase::Loading) => Some(t("Generating synthetic data…").into()),
            (_, Phase::Home) => None,
            (Some(s), _) => {
                let mut t = tr!("{files} files · {folders} folders · {size}", files = format::count(s.file_count()),
                    folders = format::count(s.dir_count().saturating_sub(1)), size = format::bytes(s.size(s.root())));
                if a.phase == Phase::Ready
                    && let Some(r) = a.result
                {
                    t.push_str(&tr!(" · {seconds} s", seconds = format!("{:.1}", r.secs)));
                    if r.backend == BackendUsed::Walk && r.unreadable > 0 {
                        t.push_str(&tr!(" · {count} inaccessible locations", count = format::count(r.unreadable)));
                    }
                }
                if a.dev() {
                    t.push_str(&tr!(" · Tree memory {size}", size = format::bytes(s.memory_bytes() as u64)));
                }
                Some(t.into())
            }
            (None, _) => None,
        };
        let mut err = false;
        if let Some((m, e)) = a.message.clone()
            && a.phase == Phase::Ready
        {
            left = Some(match left {
                Some(l) => format!("{m} · {l}").into(),
                None => m.to_string().into(),
            });
            err = e;
        }
        let middle: Option<SharedString> = a.busy.as_ref().map(|_| a.progress_text.clone().into());
        let dev = a.dev();
        div()
            .flex()
            .items_center()
            .size_full()
            .px(px(12.))
            .gap(px(16.))
            .bg(c(NAV))
            .border_t_1()
            .border_color(border())
            .when_some(left, |d, l| d.child(txt(l, 11.5, if err { ERR } else { MUTED }).min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis()))
            .when_some(middle, |d, m| d.child(txt(m, 11.5, TEXT).min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis()))
            .child(div().flex_1())
            .when(dev, |d| {
                d.child(txt(tr!("Frame {last} ms · Peak (2 s) {max} ms · {fps} fps", last = format!("{last:.1}"), max = format!("{max:.1}"), fps = fps), 11.5, DIM).whitespace_nowrap())
            })
    }
}

// ---------------------------------------------------------------- Directory tree
#[derive(Clone)]
struct TreeData {
    snap: Option<Arc<Tree>>,
    rows: Arc<Vec<Row>>,
    selected: Option<u32>,
    scroll: UniformListScrollHandle,
    bounds: Rc<Cell<Bounds<Pixels>>>,
    /// Whether deletion and rescanning are allowed: a completed real scan with no operation in progress.
    editable: bool,
}

pub struct TreeView {
    app: WeakEntity<DiskApp>,
    data: Option<TreeData>,
}

impl DiskApp {
    fn tree_data(&self) -> TreeData {
        TreeData {
            snap: self.snap.clone(),
            rows: self.rows.clone(),
            selected: self.selected,
            scroll: self.tree_scroll.clone(),
            bounds: self.tree_bounds.clone(),
            editable: self.editable(),
        }
    }
}

fn header_cell(w: f32, s: &'static str) -> Div {
    div().w(px(w)).flex().justify_end().child(s)
}

fn tree_header() -> Div {
    div()
        .flex()
        .items_center()
        .h(px(HEADER_H))
        .px(px(10.))
        .gap(px(8.))
        .border_b_1()
        .border_color(border())
        .text_size(px(11.5))
        .text_color(c(DIM))
        .child(div().flex_1().min_w_0().child(t("Name")))
        .child(div().w(px(128.)).child(t("% of parent")))
        .child(header_cell(84., t("Allocated")))
        .child(header_cell(92., t("Files")))
}

/// Context menu for a directory-tree row.
fn row_menu(
    app: &WeakEntity<DiskApp>,
    snap: &Arc<Tree>,
    node: u32,
    editable: bool,
) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static {
    let app = app.clone();
    let item = MenuNode::of(&**snap, node, editable);
    move |menu, _, _| node_menu_items(menu, &app, item)
}

/// Node details needed to construct a context menu.
#[derive(Clone, Copy)]
struct MenuNode {
    node: u32,
    is_dir: bool,
    is_root: bool,
    special: bool,
    /// Scanning is complete and mutations are allowed.
    real: bool,
}

impl MenuNode {
    fn of(snap: &Tree, node: u32, editable: bool) -> Self {
        MenuNode {
            node,
            is_dir: snap.is_dir(node),
            is_root: snap.root() == node,
            special: snap.is_special(node),
            real: snap.complete() && editable,
        }
    }
}

/// File/folder menu items, shared by the tree and treemap.
fn node_menu_items(menu: PopupMenu, app: &WeakEntity<DiskApp>, item: MenuNode) -> PopupMenu {
    let MenuNode { node, is_dir, is_root, special, real } = item;
    {
        let call = |f: fn(&mut DiskApp, u32, &mut Window, &mut Context<DiskApp>)| {
            let app = app.clone();
            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                let _ = app.update(cx, |this, cx| f(this, node, window, cx));
            }
        };
        let mut menu = menu
            .item(
                PopupMenuItem::new(t("Show in Explorer"))
                    .icon(IconName::FolderOpen)
                    .disabled(special || !real)
                    .on_click(call(|this, n, _, cx| this.reveal_in_explorer(n, cx))),
            )
            .item(PopupMenuItem::new(t("Copy path")).icon(IconName::Copy).on_click(call(|this, n, w, cx| this.copy_path(n, w, cx))));
        if is_dir {
            menu = menu.item(
                PopupMenuItem::new(t("Rescan this folder"))
                    .icon(IconName::RefreshCw)
                    .disabled(special || !real)
                    .on_click(call(|this, n, _, cx| this.rescan_folder(n, cx))),
            );
        }
        menu.separator()
            .item(
                PopupMenuItem::new(t("Move to Recycle Bin…"))
                    .icon(IconName::Delete)
                    .disabled(special || is_root || !real)
                    .on_click(call(|this, n, w, cx| this.confirm_delete(n, w, cx))),
            )
            .item(
                PopupMenuItem::new(t("Delete permanently…"))
                    .icon(IconName::CircleX)
                    .action(Box::new(DeletePermanently))
                    .disabled(special || is_root || !real)
                    .on_click(call(|this, n, w, cx| this.confirm_delete_permanently(n, w, cx))),
            )
    }
}

/// Treemap context menu for the block under the pointer when right-clicked ([`DiskApp::tm_menu`]).
fn treemap_menu(app: &WeakEntity<DiskApp>) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static {
    let app = app.clone();
    move |menu, _window, cx| {
        let Some(entity) = app.upgrade() else { return menu };
        let (target, snap, editable, can_up) = {
            let a = entity.read(cx);
            (a.tm_menu.and_then(|i| a.tm_target(i)), a.snap.clone(), a.editable(), a.can_zoom_up())
        };
        {
            // Clear the highlight when the menu closes after a selection, Escape, or an outside click.
            let app = app.clone();
            cx.subscribe_self(move |_, _: &DismissEvent, cx| {
                let _ = app.update(cx, |this, cx| {
                    this.tm_menu = None;
                    this.touch(V_TM_HOVER, cx);
                });
            })
            .detach();
        }
        let up = app.clone();
        let menu = menu.item(
            PopupMenuItem::new(t("Go up"))
                .icon(IconName::ArrowUp)
                .action(Box::new(ZoomOut))
                .disabled(!can_up)
                .on_click(move |_, _, cx| {
                    let _ = up.update(cx, |this, cx| this.zoom_up(cx));
                }),
        );
        let Some(snap) = snap else { return menu };
        match target {
            Some(TmTarget::Node(node)) if snap.contains(node) => node_menu_items(menu.separator(), &app, MenuNode::of(&*snap, node, editable)),
            Some(TmTarget::Rest { dir, count }) if snap.contains(dir) => {
                let open = app.clone();
                menu.separator().item(PopupMenuItem::label(tr!("{count} smaller items", count = format::count(count as u64)))).item(
                    PopupMenuItem::new(t("Open containing folder"))
                        .icon(IconName::FolderOpen)
                        .disabled(!snap.complete() || snap.is_special(dir))
                        .on_click(move |_, _, cx| {
                            let _ = open.update(cx, |this, cx| this.open_folder(dir, cx));
                        }),
                )
            }
            _ => menu,
        }
    }
}

/// Translate synthesized metadata labels while preserving every actual file name.
fn display_name(name: &str, special: bool) -> String {
    if special {
        if name == "<orphaned records>" {
            return t("<orphaned records>").to_string();
        }
        if let Some(number) = name.strip_prefix("<record ").and_then(|s| s.strip_suffix('>')) {
            return tr!("<record {number}>", number = number);
        }
    }
    name.to_string()
}

fn tree_row(app: &WeakEntity<DiskApp>, d: &TreeData, ix: usize, row: Row) -> AnyElement {
    let snap = d.snap.as_ref().expect("snapshot");
    let node = row.node;
    let is_dir = snap.is_dir(node);
    let has_kids = is_dir && snap.child_count(node) > 0;
    let size = snap.size(node);
    let parent = snap.parent(node);
    let psize = if parent == NONE { size } else { snap.size(parent) };
    let frac = if psize > 0 { size as f32 / psize as f32 } else { 0.0 };
    let selected = d.selected == Some(node);
    let name: SharedString = display_name(snap.name(node), snap.is_special(node)).into();
    let files = if is_dir { format::count(snap.files(node)) } else { String::new() };
    let indent = 10.0 + row.depth as f32 * 14.0;
    div()
        .id(("row", ix))
        .relative()
        .w_full()
        .flex()
        .items_center()
        .h(px(ROW_H))
        .pr(px(10.))
        .gap(px(8.))
        .text_size(px(12.5))
        .text_color(c(TEXT))
        .cursor_pointer()
        .when(selected, |d| d.bg(row_selected()))
        .when(!selected && ix % 2 == 1, |d| d.bg(row_alt()))
        .when(!selected, |d| d.hover(|d| d.bg(row_hover())))
        .when(selected, |d| d.child(div().absolute().left_0().top_0().bottom_0().w(px(2.)).bg(c(ACCENT))))
        .on_click(act(app, move |this, e: &ClickEvent, cx| {
            this.select(node, cx);
            if e.click_count() >= 2 {
                this.zoom_to(node, cx);
            }
        }))
        .on_mouse_down(MouseButton::Right, act(app, move |this, _: &MouseDownEvent, cx| this.select(node, cx)))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .items_center()
                .pl(px(indent))
                .gap(px(4.))
                .child(
                    div()
                        .id(("chev", ix))
                        .size(px(18.))
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(4.))
                        .when(has_kids, |d| {
                            d.hover(|d| d.bg(c(HOVER)))
                                .child(
                                    Icon::new(if row.expanded { IconName::ChevronDown } else { IconName::ChevronRight })
                                        .size(px(13.))
                                        .text_color(c(MUTED)),
                                )
                                .on_mouse_down(MouseButton::Left, {
                                    let app = app.clone();
                                    move |_, _, cx| {
                                        cx.stop_propagation();
                                        let _ = app.update(cx, |this, cx| this.toggle(node, cx));
                                    }
                                })
                        }),
                )
                .child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .when(!is_dir, |d| d.text_color(c(MUTED)))
                        .child(name),
                ),
        )
        .child(
            div()
                .w(px(128.))
                .flex()
                .items_center()
                .gap(px(8.))
                .child(
                    div()
                        .w(px(56.))
                        .h(px(4.))
                        .rounded(px(2.))
                        .bg(bar_track())
                        .child(div().h_full().rounded(px(2.)).w(px(56. * frac.clamp(0.0, 1.0))).bg(bar_fill())),
                )
                .child(num(format::percent(size, psize), MUTED)),
        )
        .child(div().w(px(84.)).flex().justify_end().child(num(format::bytes(size), TEXT)))
        .child(div().w(px(92.)).flex().justify_end().child(num(files, MUTED)))
        .context_menu(row_menu(app, snap, node, d.editable))
        .into_any_element()
}

impl Render for TreeView {
    fn render(&mut self, _w: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(app) = self.app.upgrade() else { return div().into_any_element() };
        let d = app.read(cx).tree_data();
        self.data = Some(d.clone());
        let body: AnyElement = if d.snap.is_none() || d.rows.is_empty() {
            skeleton_rows(18).into_any_element()
        } else {
            let weak = self.app.clone();
            div()
                .id("tree-wrap")
                .relative()
                .flex_1()
                .min_h_0()
                .child(
                    uniform_list(
                        "tree-rows",
                        d.rows.len(),
                        cx.processor(move |this: &mut TreeView, range: std::ops::Range<usize>, _window, _cx| {
                            let d = this.data.as_ref().expect("tree data");
                            range.map(|ix| tree_row(&weak, d, ix, d.rows[ix])).collect::<Vec<_>>()
                        }),
                    )
                    .track_scroll(&d.scroll)
                    .size_full(),
                )
                .vertical_scrollbar(&d.scroll)
                .into_any_element()
        };
        let bounds = d.bounds.clone();
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(c(BG))
            .child(tree_header())
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(canvas(move |b, _, _| bounds.set(b), |_, _, _, _| {}).absolute().inset_0())
                    .child(body),
            )
            .into_any_element()
    }
}

// ---------------------------------------------------------------- Extension table
#[derive(Clone)]
struct ExtData {
    snap: Option<Arc<Tree>>,
    rows: Arc<Vec<(u32, ExtStat)>>,
    scroll: UniformListScrollHandle,
}

pub struct ExtView {
    app: WeakEntity<DiskApp>,
    data: Option<ExtData>,
}

fn ext_row(d: &ExtData, ix: usize) -> Stateful<Div> {
    let snap = d.snap.as_ref().expect("snapshot");
    let total = snap.size(snap.root());
    let (ext, st) = d.rows[ix];
    let cat = snap.ext_category(ext);
    let name = snap.ext_name(ext);
    let label: SharedString = if name.is_empty() { t("No extension").into() } else { format!(".{name}").into() };
    div()
        .id(("ext", ix))
        .w_full()
        .flex()
        .items_center()
        .h(px(ROW_H))
        .px(px(10.))
        .gap(px(8.))
        .text_size(px(12.5))
        .when(ix % 2 == 1, |d| d.bg(row_alt()))
        .hover(|d| d.bg(row_hover()))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(div().size(px(9.)).flex_shrink_0().rounded(px(2.)).bg(c(CATEGORY_RGB[cat.index()])))
                .child(div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(c(TEXT)).child(label)),
        )
        .child(div().w(px(76.)).flex().justify_end().child(num(format::bytes(st.bytes), TEXT)))
        .child(div().w(px(52.)).flex().justify_end().child(num(format::percent(st.bytes, total), MUTED)))
        .child(div().w(px(76.)).flex().justify_end().child(num(format::count(st.files), MUTED)))
}

fn legend() -> Div {
    let mut d = div().flex().flex_wrap().gap_x(px(12.)).gap_y(px(4.)).px(px(10.)).py(px(8.)).border_t_1().border_color(border());
    for cat in acre_model::Category::ALL {
        d = d.child(
            div()
                .flex()
                .items_center()
                .gap(px(5.))
                .child(div().size(px(8.)).rounded(px(2.)).bg(c(CATEGORY_RGB[cat.index()])))
                .child(txt(t(cat.label()), 11., MUTED)),
        );
    }
    d
}

impl Render for ExtView {
    fn render(&mut self, _w: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(app) = self.app.upgrade() else { return div().into_any_element() };
        let d = {
            let a = app.read(cx);
            ExtData { snap: a.snap.clone(), rows: a.ext_rows.clone(), scroll: a.ext_scroll.clone() }
        };
        self.data = Some(d.clone());
        let header = div()
            .flex()
            .items_center()
            .h(px(HEADER_H))
            .px(px(10.))
            .gap(px(8.))
            .border_b_1()
            .border_color(border())
            .text_size(px(11.5))
            .text_color(c(DIM))
            .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().child(tr!("Extensions ({count})", count = d.rows.len())))
            .child(header_cell(76., t("Allocated")))
            .child(header_cell(52., t("Share")))
            .child(header_cell(76., t("Files")));
        let body: AnyElement = if d.rows.is_empty() || d.snap.is_none() {
            skeleton_rows(12).into_any_element()
        } else {
            div()
                .id("ext-wrap")
                .relative()
                .flex_1()
                .min_h_0()
                .child(
                    uniform_list(
                        "ext-rows",
                        d.rows.len(),
                        cx.processor(move |this: &mut ExtView, range: std::ops::Range<usize>, _window, _cx| {
                            let d = this.data.as_ref().expect("ext data");
                            range.map(|ix| ext_row(d, ix)).collect::<Vec<_>>()
                        }),
                    )
                    .track_scroll(&d.scroll)
                    .size_full(),
                )
                .vertical_scrollbar(&d.scroll)
                .into_any_element()
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(c(BG))
            .border_l_1()
            .border_color(border())
            .child(header)
            .child(div().flex_1().min_h_0().flex().flex_col().child(body))
            .child(legend())
            .into_any_element()
    }
}

// ---------------------------------------------------------------- Treemap
pub struct TreemapView {
    app: WeakEntity<DiskApp>,
    header: Entity<TmHeaderView>,
}

/// Separately cached treemap toolbar with Up, path, and rendering mode; hover does not rebuild it.
pub struct TmHeaderView {
    app: WeakEntity<DiskApp>,
}

impl DiskApp {
    fn breadcrumb(&self) -> Div {
        let mut d = div().flex().items_center().gap(px(2.)).min_w_0().overflow_hidden();
        let app = self.weak.clone();
        if let Some(snap) = &self.snap
            && self.zoom != NONE
            && snap.contains(self.zoom)
        {
            let chain = snap.ancestry(self.zoom);
            let n = chain.len();
            // For long paths, keep only the root and the last five components.
            let skip_from = if n > 7 { 1 } else { n };
            let skip_to = if n > 7 { n - 5 } else { n };
            for (i, node) in chain.into_iter().enumerate() {
                if i >= skip_from && i < skip_to {
                    if i == skip_from {
                        d = d.child(txt("…", 12., DIM));
                    }
                    continue;
                }
                if i > 0 {
                    d = d.child(Icon::new(IconName::ChevronRight).size(px(11.)).text_color(c(FAINT)));
                }
                let last = i + 1 == n;
                let name = display_name(snap.name(node).trim_end_matches('\\'), snap.is_special(node));
                d = d.child(
                    div()
                        .id(("crumb", i))
                        .px(px(5.))
                        .py(px(2.))
                        .rounded(px(4.))
                        .whitespace_nowrap()
                        .text_size(px(12.))
                        .text_color(c(if last { TEXT } else { MUTED }))
                        .when(last, |d| d.font_weight(FontWeight::MEDIUM))
                        .when(!last, |d| {
                            d.cursor_pointer()
                                .hover(|d| d.bg(c(HOVER)))
                                .on_click(act(&app, move |this, _: &ClickEvent, cx| this.navigate_to(node, cx)))
                        })
                        .child(name),
                );
            }
        }
        d
    }

    fn treemap_canvas(&self) -> impl IntoElement {
        let app = self.weak.clone();
        let bounds_cell = self.tm_bounds.clone();
        let scale_cell = self.tm_scale.clone();
        let want = self.tm_want_size.clone();
        let weak = app.clone();
        let result = self.tm_result.as_ref().map(|r| (r.layout.clone(), r.image.clone()));
        let sel = self.sel_rect;
        let labels = self.views.tm_labels.clone();
        let loading = self.tm_result.is_none();
        div()
            .id("treemap")
            .relative()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .m(px(TM_PAD))
            .rounded(px(4.))
            .bg(c(SURFACE))
            .on_mouse_move(act(&app, |this, e: &MouseMoveEvent, cx| this.on_tm_mouse_move(e.position, cx)))
            .on_hover(act(&app, |this, hovered: &bool, cx| {
                if !*hovered && this.hover.is_some() {
                    this.hover = None;
                    this.touch(V_TM_HOVER, cx);
                }
            }))
            .on_mouse_down(MouseButton::Left, act(&app, |this, e: &MouseDownEvent, cx| this.on_tm_click(e.position, e.click_count, cx)))
            .on_mouse_down(MouseButton::Right, act(&app, |this, e: &MouseDownEvent, cx| this.on_tm_right_down(e.position, cx)))
            .child(
                canvas(
                    move |bounds, window, cx| {
                        let scale = window.scale_factor();
                        bounds_cell.set(bounds);
                        scale_cell.set(scale);
                        let w = (bounds.size.width.as_f32() * scale).round().max(0.0) as u32;
                        let h = (bounds.size.height.as_f32() * scale).round().max(0.0) as u32;
                        if want.get() != (w, h) {
                            want.set((w, h));
                            let weak = weak.clone();
                            cx.defer(move |cx| {
                                let _ = weak.update(cx, |this, cx| this.on_tm_resized(cx));
                            });
                        }
                    },
                    move |bounds, _, window, _cx| {
                        let Some((layout, image)) = result else { return };
                        let scale = window.scale_factor();
                        let to_bounds = |x: f32, y: f32, w: f32, h: f32| {
                            Bounds::new(
                                point(bounds.origin.x + px(x / scale), bounds.origin.y + px(y / scale)),
                                size(px(w / scale), px(h / scale)),
                            )
                        };
                        window.with_content_mask(Some(ContentMask { bounds }), |window| {
                            if let Some(img) = image {
                                let ib = to_bounds(0.0, 0.0, layout.width, layout.height);
                                let _ = window.paint_image(bounds, ib, Corners::default(), img, 0, false);
                            } else {
                                // One GPU rectangle per leaf, with a one-pixel gap on the right and bottom.
                                for r in layout.rects.iter().filter(|r| r.is_leaf()) {
                                    let cat = r.cat as usize % 9;
                                    let (rgb, a) = match r.kind {
                                        KIND_DIR_LEAF if cat != 8 => (CATEGORY_RGB[cat], 0.72),
                                        KIND_REST if cat != 8 => (CATEGORY_RGB[cat], 0.55),
                                        KIND_DIR_LEAF => (DIR_LEAF_RGB, 1.0),
                                        KIND_REST => (REST_RGB, 1.0),
                                        _ => (CATEGORY_RGB[cat], 1.0),
                                    };
                                    let (w, h) = if r.w >= 3.0 && r.h >= 3.0 { (r.w - 1.0, r.h - 1.0) } else { (r.w, r.h) };
                                    window.paint_quad(fill(to_bounds(r.x, r.y, w, h), c(rgb).opacity(a)));
                                }
                            }
                            // Do not outline the current view's root folder, which covers the whole treemap.
                            if let Some(i) = sel.filter(|&i| i != 0)
                                && let Some(r) = layout.rects.get(i)
                            {
                                // Selection uses a light fill, white outline, and dark inner line for all backgrounds; hover uses only a white outline.
                                let b = to_bounds(r.x, r.y, r.w, r.h);
                                window.paint_quad(fill(b, tm_selected_fill()));
                                let mut q = outline(b, tm_selected(), BorderStyle::Solid);
                                q.border_widths = (2.0).into();
                                window.paint_quad(q);
                                if b.size.width > px(6.) && b.size.height > px(6.) {
                                    let inner = Bounds::new(
                                        point(b.origin.x + px(2.), b.origin.y + px(2.)),
                                        size(b.size.width - px(4.), b.size.height - px(4.)),
                                    );
                                    window.paint_quad(outline(inner, tm_ring(), BorderStyle::Solid));
                                }
                            }
                        });
                    },
                )
                .absolute()
                .inset_0(),
            )
            .child(labels.cached(StyleRefinement::default().absolute().inset_0()))
            .when(loading, |d| d.child(treemap_skeleton()))
            .context_menu(treemap_menu(&app))
    }

    fn tooltip(&self) -> Option<AnyElement> {
        let i = self.hover?;
        let r = self.tm_result.as_ref()?;
        let rect = r.layout.rects.get(i)?;
        let snap = &r.snap;
        let node = rect.node;
        let root_size = snap.size(r.layout.root).max(1);
        let (title, size, files, path): (String, u64, u64, String) = match rect.kind {
            KIND_REST => {
                let shown_size: u64 = r
                    .layout
                    .rects
                    .iter()
                    .filter(|x| x.parent == rect.parent && x.kind != KIND_REST)
                    .map(|x| snap.size(x.node))
                    .sum();
                let total = snap.size(node);
                (tr!("{count} smaller items", count = format::count(rect.rest_count as u64)), total.saturating_sub(shown_size), 0, snap.path(node))
            }
            _ => (display_name(snap.name(node), snap.is_special(node)), snap.size(node), snap.files(node), snap.path(node)),
        };
        let kind_label = match rect.kind {
            KIND_FILE => t(snap.category(node).label()).to_string(),
            KIND_REST => t("Grouped items").to_string(),
            _ => t("Folder").to_string(),
        };
        let b = self.tm_bounds.get();
        let tw = 340.0f32;
        let th = 92.0f32;
        let mut x = self.mouse.x.as_f32() + 16.0;
        let mut y = self.mouse.y.as_f32() + 18.0;
        if x + tw > b.size.width.as_f32() - 4.0 {
            x = (self.mouse.x.as_f32() - tw - 12.0).max(4.0);
        }
        if y + th > b.size.height.as_f32() - 4.0 {
            y = (self.mouse.y.as_f32() - th - 12.0).max(4.0);
        }
        let mut info = tr!("{size} · {share} of this view", size = format::bytes(size), share = format::percent(size, root_size));
        if rect.kind != KIND_FILE && rect.kind != KIND_REST {
            info.push_str(&tr!(" · {count} files", count = format::count(files)));
        }
        Some(
            div()
                .absolute()
                .left(px(x))
                .top(px(y))
                .w(px(tw))
                .p(px(10.))
                .rounded(px(8.))
                .bg(c(POPOVER))
                .border_1()
                .border_color(border())
                .shadow_lg()
                .flex()
                .flex_col()
                .gap(px(3.))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .child(
                            txt(title, 13., TEXT)
                                .font_weight(FontWeight::SEMIBOLD)
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis(),
                        )
                        .child(txt(kind_label, 11., DIM).whitespace_nowrap()),
                )
                .child(txt(info, 12., MUTED).whitespace_nowrap())
                .child(txt(path, 11., DIM).overflow_hidden().whitespace_nowrap().text_ellipsis())
                .into_any_element(),
        )
    }
}

/// Treemap hover overlay: block and parent-folder outlines, context-menu target highlight, and tooltip.
/// Pointer movement only notifies this overlay; the bitmap, selection outline, and labels do not rebuild.
pub struct TmHoverView {
    app: WeakEntity<DiskApp>,
}

impl Render for TmHoverView {
    fn render(&mut self, _w: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let root = div().size_full().relative().overflow_hidden();
        let Some(app) = self.app.upgrade() else { return root };
        let a = app.read(cx);
        let Some(layout) = a.tm_result.as_ref().map(|r| r.layout.clone()) else { return root };
        let hover = a.hover;
        let menu_rect = a.tm_menu;
        if hover.is_none() && menu_rect.is_none() {
            return root;
        }
        let tip = if menu_rect.is_none() { a.tooltip() } else { None };
        root.child(
            canvas(
                |_, _, _| {},
                move |bounds, _, window, _cx| {
                    let scale = window.scale_factor();
                    let to_bounds = |x: f32, y: f32, w: f32, h: f32| {
                        Bounds::new(
                            point(bounds.origin.x + px(x / scale), bounds.origin.y + px(y / scale)),
                            size(px(w / scale), px(h / scale)),
                        )
                    };
                    window.with_content_mask(Some(ContentMask { bounds }), |window| {
                        if let Some(i) = hover
                            && let Some(r) = layout.rects.get(i)
                        {
                            // Outline both the hovered block and its containing folder.
                            if r.parent != NONE
                                && r.parent != 0
                                && let Some(p) = layout.rects.get(r.parent as usize)
                            {
                                window.paint_quad(outline(to_bounds(p.x, p.y, p.w, p.h), tm_hover_parent(), BorderStyle::Solid));
                            }
                            let mut q = outline(to_bounds(r.x, r.y, r.w, r.h), tm_hover(), BorderStyle::Solid);
                            q.border_widths = (2.0).into();
                            window.paint_quad(q);
                        }
                        if let Some(i) = menu_rect
                            && let Some(r) = layout.rects.get(i)
                        {
                            let mut q = outline(to_bounds(r.x, r.y, r.w, r.h), tm_hover(), BorderStyle::Solid);
                            q.border_widths = (2.0).into();
                            window.paint_quad(q);
                        }
                    });
                },
            )
            .absolute()
            .inset_0(),
        )
        .when_some(tip, |d, t| d.child(t))
    }
}

/// Treemap name labels; header heights and minimum block sizes are in logical pixels.
const LABEL_H: f32 = 16.;
const LABEL_MIN_W: f32 = 36.;
/// Minimum folder block dimensions required for a header.
const HEADER_MIN_W: f32 = 80.;
const HEADER_MIN_H: f32 = 52.;
/// Maximum labels per treemap, prioritizing larger blocks.
const LABEL_MAX: usize = 800;

/// Separately cached treemap labels rebuild only when layout or data changes, never on hover.
/// Label only blocks with room (at least 36 by 20 logical pixels), naturally limiting the count; GPUI handles text layout.
pub struct TmLabelsView {
    app: WeakEntity<DiskApp>,
}

impl Render for TmLabelsView {
    fn render(&mut self, _w: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Cached views lay out their root separately. Fill the available space; an absolute root would have zero size and be clipped.
        let mut root = div().size_full().relative().overflow_hidden();
        let Some(app) = self.app.upgrade() else { return root };
        let a = app.read(cx);
        let Some(r) = a.tm_result.as_ref().filter(|r| a.labels && r.key.labels) else {
            a.label_stats.set((0, 0));
            return root;
        };
        let t0 = Instant::now();
        let s = a.tm_scale.get().max(0.5);
        let snap = &*r.snap;
        let mut n = 0u32;
        for rect in r.layout.rects.iter().skip(1) {
            if n as usize >= LABEL_MAX {
                break;
            }
            let (x, y, w, h) = (rect.x / s, rect.y / s, rect.w / s, rect.h / s);
            if rect.kind == KIND_DIR {
                if rect.header > 0.0 {
                    root = root.child(header_label(snap, rect.node, x, y, w, rect.header / s));
                    n += 1;
                }
            } else if w >= LABEL_MIN_W && h >= LABEL_H + 4. {
                root = root.child(block_label(snap, rect, x, y, w));
                n += 1;
            }
        }
        a.label_stats.set((n, t0.elapsed().as_micros() as u64));
        root
    }
}

/// Name in a folder header, whose background is already rendered in the bitmap.
fn header_label(snap: &Tree, node: u32, x: f32, y: f32, w: f32, hh: f32) -> Div {
    let show_size = w >= 120.;
    div()
        .absolute()
        .left(px(x))
        .top(px(y))
        .w(px(w))
        .h(px(hh))
        .px(px(4.))
        .flex()
        .items_center()
        .gap(px(6.))
        .overflow_hidden()
        .text_size(px(11.))
        .line_height(px(hh))
        .child(
            div()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_color(c(TEXT))
                .font_weight(FontWeight::MEDIUM)
                .child(display_name(snap.name(node).trim_end_matches('\\'), snap.is_special(node))),
        )
        .when(show_size, |d| d.child(div().flex_shrink_0().whitespace_nowrap().text_color(label_dim()).child(format::bytes(snap.size(node)))))
}

/// Top-left block label: light text on a dark fill for contrast, truncated with an ellipsis when needed.
fn block_label(snap: &Tree, rect: &TmRect, x: f32, y: f32, w: f32) -> Div {
    let name = if rect.kind == KIND_REST {
        tr!("{count} smaller items", count = format::count(rect.rest_count as u64))
    } else {
        display_name(snap.name(rect.node), snap.is_special(rect.node))
    };
    let show_size = w >= 110. && rect.kind != KIND_REST;
    div()
        .absolute()
        .left(px(x + 2.))
        .top(px(y + 2.))
        .max_w(px(w - 4.))
        .h(px(LABEL_H))
        .px(px(3.))
        .rounded(px(3.))
        .bg(label_bg())
        .flex()
        .items_center()
        .gap(px(5.))
        .overflow_hidden()
        .text_size(px(11.))
        .line_height(px(LABEL_H))
        .child(div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(c(TEXT)).child(name))
        .when(show_size, |d| d.child(div().flex_shrink_0().whitespace_nowrap().text_color(label_dim()).child(format::bytes(snap.size(rect.node)))))
}

impl Render for TmHeaderView {
    fn render(&mut self, _w: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(app) = self.app.upgrade() else { return div() };
        let a = app.read(cx);
        let mode = a.mode;
        let synthetic = a.synthetic();
        let can_up = a.snap.as_ref().map(|s| a.zoom != NONE && s.contains(a.zoom) && s.parent(a.zoom) != NONE).unwrap_or(false);
        let weak = self.app.clone();
        let mut modes = ButtonGroup::new("mode")
            .small()
            .outline()
            .child(Button::new("m-cushion").label(t("Cushion")).selected(mode == RenderMode::Cushion))
            .child(Button::new("m-flat").label(t("Flat")).selected(mode == RenderMode::Flat));
        if synthetic {
            modes = modes.child(Button::new("m-quads").label(t("Quads")).selected(mode == RenderMode::Quads));
        }
        div()
            .flex()
            .items_center()
            .size_full()
            .px(px(8.))
            .gap(px(6.))
            .border_b_1()
            .border_color(border())
            .child(
                Button::new("zoom-up")
                    .ghost()
                    .small()
                    .icon(IconName::ArrowUp)
                    .label(t("Up"))
                    .disabled(!can_up)
                    .on_click(act(&weak, |this, _: &ClickEvent, cx| this.zoom_up(cx))),
            )
            .child(a.breadcrumb())
            .child(div().flex_1())
            .child(modes.on_click(act(&weak, |this, clicks: &Vec<usize>, cx| {
                let m = match clicks.first() {
                    Some(1) => RenderMode::Flat,
                    Some(2) => RenderMode::Quads,
                    _ => RenderMode::Cushion,
                };
                if m != this.mode {
                    this.mode = m;
                    this.perf.begin("relayout", Instant::now(), json!({"reason": "mode"}));
                    this.request_layout(cx);
                    this.changed(cx);
                }
            })))
    }
}

impl Render for TreemapView {
    fn render(&mut self, _w: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(app) = self.app.upgrade() else { return div().into_any_element() };
        let a = app.read(cx);
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(c(BG))
            .child(self.header.clone().cached(StyleRefinement::default().w_full().h(px(TM_HEAD_H)).flex_shrink_0()))
            .child(a.treemap_canvas())
            .into_any_element()
    }
}

fn skeleton_rows(n: usize) -> impl IntoElement {
    let mut d = div().flex().flex_col().px(px(10.)).pt(px(8.)).gap(px(10.));
    for i in 0..n {
        let w = [0.62, 0.48, 0.55, 0.38, 0.7, 0.44][i % 6];
        d = d.child(
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .child(Skeleton::new().h(px(10.)).w(relative(w)))
                .child(div().flex_1())
                .child(Skeleton::new().h(px(10.)).w(px(56.))),
        );
    }
    d
}

fn treemap_skeleton() -> impl IntoElement {
    div()
        .absolute()
        .inset_0()
        .p(px(4.))
        .flex()
        .gap(px(4.))
        .child(Skeleton::new().h_full().w(relative(0.46)))
        .child(
            div()
                .flex_1()
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(Skeleton::new().w_full().h(relative(0.58)))
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .gap(px(4.))
                        .child(Skeleton::new().h_full().w(relative(0.6)))
                        .child(Skeleton::new().h_full().flex_1()),
                ),
        )
}

impl Render for DiskApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        perf::phase("first-render");
        for img in self.tm_stale.drain(..) {
            let _ = window.drop_image(img);
        }
        let v = &self.views;
        let full = || StyleRefinement::default().size_full();
        let body: AnyElement = if self.phase == Phase::Home {
            div().flex_1().min_h_0().child(v.home.clone().cached(full())).into_any_element()
        } else {
            div()
                .flex_1()
                .min_h_0()
                .child(
                    v_resizable("main-split")
                        .child(
                            resizable_panel().size(px(430.)).size_range(px(180.)..px(2000.)).child(
                                h_resizable("top-split")
                                    .child(resizable_panel().child(v.tree.clone().cached(full())))
                                    .child(
                                        resizable_panel()
                                            .size(px(400.))
                                            .size_range(px(360.)..px(900.))
                                            .child(v.ext.clone().cached(full())),
                                    ),
                            ),
                        )
                        .child(
                            resizable_panel().size_range(px(200.)..px(3000.)).child(
                                div().relative().size_full().child(v.tm.clone().cached(full())).child(
                                    v.tm_hover.clone().cached(
                                        StyleRefinement::default()
                                            .absolute()
                                            .top(px(TM_HEAD_H + TM_PAD))
                                            .left(px(TM_PAD))
                                            .right(px(TM_PAD))
                                            .bottom(px(TM_PAD)),
                                    ),
                                ),
                            ),
                        ),
                )
                .into_any_element()
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &DeletePermanently, window, cx| this.delete_selected_permanently(window, cx)))
            .on_action(cx.listener(|this, _: &ZoomOut, _, cx| this.zoom_up(cx)))
            .on_mouse_down(MouseButton::Navigate(NavigationDirection::Back), cx.listener(|this, _: &MouseDownEvent, _, cx| this.zoom_up(cx)))
            .bg(c(BG))
            .text_color(c(TEXT))
            .font_family("Noto Sans SC")
            .child(v.title.clone().cached(StyleRefinement::default().w_full().h(px(38.)).flex_shrink_0()))
            .child(body)
            .child(v.status.clone().cached(StyleRefinement::default().w_full().h(px(26.)).flex_shrink_0()))
    }
}

// ====================================================================== Test bridge
impl DiskApp {
    fn poll_bridge(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dir) = self.test_dir.clone() else { return };
        let Ok(text) = std::fs::read_to_string(dir.join("command.json")) else { return };
        let Ok(cmd) = serde_json::from_str::<Value>(text.trim_start_matches('\u{feff}')) else { return };
        let id = cmd["id"].as_str().unwrap_or_default().to_string();
        if id.is_empty() || id == self.last_command {
            return;
        }
        self.last_command = id.clone();
        let action = cmd["action"].as_str().unwrap_or_default();
        let value = cmd["value"].as_str().unwrap_or_default().to_string();
        let result = self.run_command(action, &value, window, cx);
        let out = match result {
            Ok(v) => json!({"id": id, "ok": true, "result": v, "state": self.state_json(window)}),
            Err(e) => json!({"id": id, "ok": false, "error": e}),
        };
        let tmp = dir.join("state.json.tmp");
        if std::fs::write(&tmp, serde_json::to_vec_pretty(&out).unwrap_or_default()).is_ok() {
            let _ = std::fs::rename(&tmp, dir.join("state.json"));
        }
    }

    fn run_command(&mut self, action: &str, value: &str, window: &mut Window, cx: &mut Context<Self>) -> Result<Value, String> {
        match action {
            "inspect" => Ok(Value::Null),
            "language" => {
                let language = Language::from_tag(value).ok_or("unsupported language")?;
                self.change_language(language, window, cx);
                Ok(json!({"language": language.tag()}))
            },
            "dataset" => {
                let n: u64 = value.parse().map_err(|_| "bad number")?;
                self.start_load(n, cx);
                Ok(Value::Null)
            }
            "scan" => {
                if self.synthetic() {
                    let rate: u64 = value.parse().unwrap_or(0);
                    self.start_sim_scan((rate > 0).then_some(rate), cx);
                } else {
                    self.start_scan(value.to_string(), cx);
                }
                Ok(Value::Null)
            }
            "scan-stop" | "cancel" => {
                self.cancel_scan(cx);
                Ok(Value::Null)
            }
            "home" => {
                self.go_home(cx);
                Ok(Value::Null)
            }
            "drives" => Ok(json!(self
                .drives
                .iter()
                .map(|d| json!({"mount": d.mount, "label": d.label, "fs": d.filesystem, "total": d.total, "free": d.free}))
                .collect::<Vec<_>>())),
            "hard-delete-path" => {
                // Match Shift+Delete: select the item and show a confirmation dialog that must be confirmed in the UI.
                let node = self.path_to_node(value).ok_or("no such path")?;
                self.reveal(node, false, ScrollStrategy::Top, cx);
                self.delete_selected_permanently(window, cx);
                Ok(json!({"node": node}))
            }
            "focus" => {
                window.focus(&self.focus, cx);
                Ok(Value::Null)
            }
            "rescan-path" => {
                let node = self.path_to_node(value).ok_or("no such path")?;
                self.rescan_folder(node, cx);
                Ok(json!({"node": node}))
            }
            "expand-path" => {
                let node = self.path_to_node(value).ok_or("no such path")?;
                let ix = self.reveal(node, true, ScrollStrategy::Top, cx);
                Ok(json!({"node": node, "row": ix}))
            }
            "expand-biggest" => {
                let snap = self.snap.clone().ok_or("no data")?;
                let node = snap.biggest_dir().ok_or("no data")?;
                let ix = self.reveal(node, true, ScrollStrategy::Top, cx);
                Ok(json!({"node": node, "row": ix, "path": snap.path(node), "children": snap.child_count(node)}))
            }
            "biggest" => {
                let snap = self.snap.clone().ok_or("no data")?;
                let node = snap.biggest_dir().ok_or("no data")?;
                Ok(json!({"node": node, "path": snap.path(node), "children": snap.child_count(node), "depth": snap.depth(node)}))
            }
            "reveal-path" => {
                let node = self.path_to_node(value).ok_or("no such path")?;
                let ix = self.reveal(node, false, ScrollStrategy::Top, cx);
                Ok(json!({"node": node, "row": ix}))
            }
            "row-of" => {
                let node = self.path_to_node(value).ok_or("no such path")?;
                let ix = self.rows.iter().position(|r| r.node == node);
                let depth = ix.map(|i| self.rows[i].depth);
                Ok(json!({"node": node, "row": ix, "depth": depth, "expanded": self.expanded.contains(&node)}))
            }
            "collapse-all" => {
                self.expanded.clear();
                if let Some(s) = &self.snap {
                    self.expanded.insert(s.root());
                }
                self.rebuild_rows("collapse", Instant::now(), cx);
                self.tree_scroll.scroll_to_item(0, ScrollStrategy::Top);
                Ok(Value::Null)
            }
            "toggle-path" => {
                let node = self.path_to_node(value).ok_or("no such path")?;
                self.toggle(node, cx);
                Ok(json!({"node": node}))
            }
            "zoom-path" => {
                let node = self.path_to_node(value).ok_or("no such path")?;
                self.zoom_to(node, cx);
                Ok(json!({"node": node}))
            }
            "zoom-root" => {
                let root = self.snap.as_ref().map(|s| s.root()).ok_or("no data")?;
                self.zoom_to(root, cx);
                Ok(Value::Null)
            }
            "zoom-up" => {
                self.zoom_up(cx);
                Ok(Value::Null)
            }
            "mode" => {
                self.mode = match value {
                    "flat" => RenderMode::Flat,
                    "quads" => RenderMode::Quads,
                    _ => RenderMode::Cushion,
                };
                self.perf.begin("relayout", Instant::now(), json!({"reason": "mode"}));
                self.request_layout(cx);
                self.changed(cx);
                Ok(Value::Null)
            }
            "min-area" => {
                self.min_area = value.parse().map_err(|_| "bad number")?;
                self.perf.begin("relayout", Instant::now(), json!({"reason": "min-area"}));
                self.request_layout(cx);
                Ok(Value::Null)
            }
            "select-path" => {
                let node = self.path_to_node(value).ok_or("no such path")?;
                self.select(node, cx);
                Ok(json!({"node": node}))
            }
            "scroll-tree" => {
                let ix: usize = value.parse().map_err(|_| "bad number")?;
                self.tree_scroll.scroll_to_item(ix, ScrollStrategy::Top);
                self.changed(cx);
                Ok(Value::Null)
            }
            "perf-begin" => {
                self.perf.start_recording(value, window);
                Ok(Value::Null)
            }
            "perf-end" => Ok(self.perf.stop_recording(window)),
            "ops" => Ok(json!(self.perf.ops.iter().cloned().collect::<Vec<_>>())),
            "ops-clear" => {
                self.perf.ops.clear();
                Ok(Value::Null)
            }
            "tm-dir-rects" => {
                // Largest directory blocks in the current treemap, for navigation click tests.
                let r = self.tm_result.as_ref().ok_or("no layout")?;
                let s = self.tm_scale.get();
                let b = self.tm_bounds.get();
                let mut v: Vec<(f32, usize)> = r
                    .layout
                    .rects
                    .iter()
                    .enumerate()
                    .filter(|(_, x)| x.depth == 1 && (x.kind == KIND_DIR || x.kind == KIND_DIR_LEAF))
                    .map(|(i, x)| (x.w * x.h, i))
                    .collect();
                v.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
                Ok(json!(v
                    .iter()
                    .take(12)
                    .map(|&(_, i)| {
                        let x = r.layout.rects[i];
                        json!({
                            "node": x.node, "path": r.snap.path(x.node),
                            "cx": b.origin.x.as_f32() + (x.x + x.w / 2.0) / s,
                            "cy": b.origin.y.as_f32() + (x.y + x.h / 2.0) / s,
                            "w": x.w / s, "h": x.h / s,
                        })
                    })
                    .collect::<Vec<_>>()))
            }
            "tm-rects" => {
                // Largest current treemap blocks of a given kind (file, dir, rest), with window-local logical-pixel coordinates.
                let r = self.tm_result.as_ref().ok_or("no layout")?;
                let s = self.tm_scale.get();
                let b = self.tm_bounds.get();
                let want: &[u8] = match value {
                    "file" => &[KIND_FILE],
                    "rest" => &[KIND_REST],
                    _ => &[KIND_DIR, KIND_DIR_LEAF],
                };
                let mut v: Vec<(f32, usize)> = r
                    .layout
                    .rects
                    .iter()
                    .enumerate()
                    .filter(|(i, x)| *i > 0 && want.contains(&x.kind))
                    .map(|(i, x)| (x.w * x.h, i))
                    .collect();
                v.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
                Ok(json!(v
                    .iter()
                    .take(12)
                    .map(|&(_, i)| {
                        let x = r.layout.rects[i];
                        json!({
                            "index": i, "node": x.node, "kind": x.kind, "path": r.snap.path(x.node), "rest": x.rest_count,
                            "cx": b.origin.x.as_f32() + (x.x + x.w / 2.0) / s,
                            "cy": b.origin.y.as_f32() + (x.y + x.h / 2.0) / s,
                            "w": x.w / s, "h": x.h / s,
                        })
                    })
                    .collect::<Vec<_>>()))
            }
            "labels" => {
                self.labels = value != "0";
                self.perf.begin("relayout", Instant::now(), json!({"reason": "labels", "labels": self.labels}));
                self.request_layout(cx);
                self.changed(cx);
                Ok(Value::Null)
            }
            "close" => {
                window.remove_window();
                Ok(Value::Null)
            }
            other => Err(format!("unknown action {other}")),
        }
    }

    fn state_json(&self, window: &Window) -> Value {
        let clock = perf::clock();
        let snap = self.snap.as_ref();
        let tm = self.tm_result.as_ref();
        let b = self.tm_bounds.get();
        let tb = self.tree_bounds.get();
        let first_draw = self.perf.first_draw.map(|d| ms_since_start(d.end));
        let first_content = self.perf.first_content.map(|(d, p)| (ms_since_start(d.end), p.map(|p| ms_since_start(p.end))));
        json!({
            "language": i18n::language().tag(),
            "phase": format!("{:?}", self.phase),
            "elevated": self.elevated,
            "target": self.target,
            "dataset": self.dataset,
            "epoch": self.epoch,
            "real": self.real.is_some(),
            "slots": snap.map(|s| s.slot_count()),
            "files": snap.map(|s| s.file_count()),
            "dirs": snap.map(|s| s.dir_count()),
            "generation": snap.map(|s| s.generation()),
            "complete": snap.map(|s| s.complete()),
            "indexed": snap.map(|s| s.has_sorted_index()),
            "total_bytes": snap.map(|s| s.size(s.root())),
            "tree_bytes": snap.map(|s| s.memory_bytes()),
            "live": {"items": self.live.items, "bytes": self.live.bytes, "elapsed_ms": self.live.elapsed.as_secs_f64() * 1000.0},
            "result": self.result.map(|r| json!({"backend": format!("{:?}", r.backend), "secs": r.secs, "index_ms": r.index_ms, "unreadable": r.unreadable})),
            "busy": self.busy.as_ref().map(|b| b.0.to_string()),
            "focused": self.focus.is_focused(window),
            "message": self.message.as_ref().map(|m| m.0.to_string()),
            "progress_text": self.progress_text,
            "rows": self.rows.len(),
            "expanded": self.expanded.len(),
            "selected": self.selected,
            "selected_path": snap.and_then(|s| self.selected.filter(|&n| s.contains(n)).map(|n| s.path(n))),
            "sel_rest": self.sel_rest,
            "sel_rect": self.sel_rect,
            "zoom": snap.filter(|s| self.zoom != NONE && s.contains(self.zoom)).map(|s| s.path(self.zoom)),
            "mode": self.mode.key(),
            "min_area": self.min_area,
            "scale": window.scale_factor(),
            "treemap_bounds": {"x": b.origin.x.as_f32(), "y": b.origin.y.as_f32(), "w": b.size.width.as_f32(), "h": b.size.height.as_f32()},
            "tree_bounds": {"x": tb.origin.x.as_f32(), "y": tb.origin.y.as_f32(), "w": tb.size.width.as_f32(), "h": tb.size.height.as_f32()},
            "row_height": ROW_H,
            "tree_scroll_top": self.tree_scroll_top(),
            "layout": tm.map(|r| json!({
                "rects": r.layout.rects.len(), "leaves": r.layout.leaf_count(),
                "layout_us": r.layout_us, "render_us": r.render_us,
                "w": r.key.w, "h": r.key.h, "generation": r.key.generation,
                "root": r.snap.path(r.key.root),
            })),
            "hover": self.hover,
            "tm_menu": self.tm_menu,
            "labels": self.labels,
            "label_count": self.label_stats.get().0,
            "label_us": self.label_stats.get().1,
            "timing": {
                "phases": perf::phases_json(),
                "process_start_unix_us": clock.start_unix_us.to_string(),
                "first_draw_end_ms": first_draw,
                "first_content_draw_end_ms": first_content.map(|x| x.0),
                "first_content_present_end_ms": first_content.and_then(|x| x.1),
                "load": self.load_stats.map(|l| json!({"generate_ms": l.generate_ms, "snapshot_ms": l.snapshot_ms, "sort_index_ms": l.sort_index_ms})),
            },
            "scan": {
                "published": self.mailbox.published.load(Ordering::Relaxed),
                "max_snapshot_us": self.mailbox.max_snapshot_us.load(Ordering::Relaxed),
                "applied": self.scan_applied,
                "last": self.scan_last.map(|i| json!({"nodes": i.nodes, "elapsed_ms": i.elapsed_ms, "final": i.final_publish})),
            },
            "draws": self.perf.total_draws,
            "pending_markers": self.perf.markers.len(),
        })
    }
}
