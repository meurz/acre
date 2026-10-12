//! Synthetic data shaped like a real Windows system drive.
//!
//! - Deep directories with random budgets and occasional single-child chains, up to about 30-48 levels.
//! - A few directories with over 100,000 direct children, such as thumbnail caches, installer caches, and WinSxS.
//! - Long-tailed file sizes: category-specific log-normal distributions and a few files of tens of GB.
//! - Hundreds of extensions: Zipf-distributed common extensions, split archives, and random three-letter suffixes;
//!   file types tend to be similar within a directory.
//!
//! Generate depth-first with parents before children, matching arrival order in a directory walk.

use std::fmt::Write as _;

use crate::ext::{Category, KNOWN};
use crate::tree::{NONE, TreeBuilder};

/// Small, fast wyrand PRNG; a fixed seed makes generation reproducible.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed ^ 0x9e37_79b9_7f4a_7c15)
    }
    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0xa076_1d64_78bd_642f);
        let t = (self.0 as u128).wrapping_mul((self.0 ^ 0xe703_7ed1_a0b4_28db) as u128);
        ((t >> 64) ^ t) as u64
    }
    #[inline]
    pub fn f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }
    #[inline]
    pub fn below(&mut self, n: u64) -> u64 {
        ((self.next_u64() as u128 * n as u128) >> 64) as u64
    }
    #[inline]
    pub fn chance(&mut self, p: f64) -> bool {
        self.f64() < p
    }
    /// Standard normal sample using Box-Muller.
    pub fn normal(&mut self) -> f64 {
        let u1 = self.f64().max(1e-300);
        let u2 = self.f64();
        (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
    }
    pub fn lognormal(&mut self, median: f64, sigma: f64) -> f64 {
        median * (sigma * self.normal()).exp()
    }
}

/// Data size preset.
#[derive(Clone, Debug)]
pub struct GenSpec {
    pub nodes: u64,
    pub seed: u64,
    /// Direct child counts for each very large directory.
    pub thumbcache: u64,
    pub blobs: u64,
    pub winsxs: u64,
    pub dataset: u64,
    pub npm_content: u64,
}

impl GenSpec {
    pub fn tier(nodes: u64) -> Self {
        if nodes >= 3_000_000 {
            Self {
                nodes,
                seed: 5,
                thumbcache: 100_000,
                blobs: 150_000,
                winsxs: 100_000,
                dataset: 300_000,
                npm_content: 120_000,
            }
        } else {
            Self {
                nodes,
                seed: 1,
                thumbcache: 100_000,
                blobs: 100_000,
                winsxs: 20_000,
                dataset: 0,
                npm_content: 0,
            }
        }
    }
}

/// Extension table: ID, category, and cumulative Zipf weight.
struct ExtTable {
    names: Vec<String>,
    ids: Vec<u16>,
    cats: Vec<Category>,
    cdf: Vec<f64>,
    by_cat: Vec<Vec<usize>>,
}

const SIZE_MODEL: [(f64, f64); 9] = [
    // (Median bytes, logarithmic standard deviation), in Category order.
    (420_000.0, 1.6),     // Programs and libraries
    (140_000_000.0, 0.9), // Video
    (380_000.0, 1.2),     // Images
    (6_000_000.0, 1.7),   // Archives and disk images
    (4_500_000.0, 0.8),   // Audio
    (280_000.0, 1.4),     // Documents
    (6_500.0, 1.3),       // Code
    (60_000.0, 2.2),      // System and data
    (18_000.0, 2.4),      // Other
];

/// Relative category frequencies in unbiased directories. Large-file categories (video, archives, audio)
/// are uncommon here and mostly occur in biased folders such as Videos, Downloads, and Music.
const FREQ: [f64; 9] = [1.0, 0.03, 0.7, 0.2, 0.2, 0.7, 1.0, 1.0, 1.0];

const SYL: &[&str] = &[
    "app", "core", "data", "img", "cache", "util", "lib", "main", "test", "log", "win", "sys", "net",
    "ui", "res", "font", "shader", "tex", "snd", "doc", "api", "cfg", "kb", "msvc", "setup",
    "update", "patch", "thumb", "model", "save", "user", "shell", "media", "edge", "chrome",
    "office", "steam", "unity", "unreal", "vulkan", "dx", "audio", "video", "photo", "backup",
    "index", "store", "local", "remote", "sync", "agent", "host", "svc", "task", "help", "intl",
];

const DIRS: &[&str] = &[
    "src", "lib", "bin", "obj", "cache", "Cache", "data", "assets", "images", "logs", "temp",
    "config", "x64", "x86", "en-US", "zh-CN", "ja-JP", "Microsoft", "Google", "Mozilla",
    "node_modules", "packages", "modules", "plugins", "resources", "shaders", "textures",
    "sounds", "fonts", "docs", "build", "dist", "out", "backup", "snapshots", "blobs",
    "objects", "refs", "Release", "Debug", "target", "deps", "incremental", "Code Cache",
    "GPUCache", "IndexedDB", "Local Storage", "Service Worker", "Extensions", "Profiles",
    "Crashpad", "reports", "screenshots", "thumbnails", "drivers", "en", "zh-Hans",
    "runtimes", "native", "win-x64", "tools", "share", "include", "examples", "tests",
    "vendor", "third_party", "generated", "intermediate", "Saved", "Content", "Paks",
];

/// Generator. Call `hook` every 2,048 nodes; stop early when it returns false.
pub struct Generator<'a> {
    rng: Rng,
    tree: &'a mut TreeBuilder,
    exts: ExtTable,
    buf: String,
    since_hook: u32,
    stopped: bool,
    hook: &'a mut dyn FnMut(&mut TreeBuilder) -> bool,
}

impl<'a> Generator<'a> {
    fn new(
        seed: u64,
        tree: &'a mut TreeBuilder,
        hook: &'a mut dyn FnMut(&mut TreeBuilder) -> bool,
    ) -> Self {
        let mut rng = Rng::new(seed);
        let mut names: Vec<(String, Category)> =
            KNOWN.iter().map(|(e, c)| ((*e).to_string(), *c)).collect();
        for n in 1..=160 {
            names.push((format!("{n:03}"), Category::Archive));
        }
        // Random three- or four-letter suffixes bring the total to about 640 extensions.
        while names.len() < 640 {
            let len = 3 + rng.below(2) as usize;
            let mut s = String::new();
            for i in 0..len {
                let b = if i > 0 && rng.chance(0.25) {
                    b'0' + rng.below(10) as u8
                } else {
                    b'a' + rng.below(26) as u8
                };
                s.push(b as char);
            }
            if !names.iter().any(|(e, _)| *e == s) {
                names.push((s, Category::Other));
            }
        }
        let mut ids = Vec::new();
        let mut cats = Vec::new();
        let mut cdf = Vec::new();
        let mut by_cat = vec![Vec::new(); 9];
        let mut acc = 0.0;
        for (rank, (e, c)) in names.iter().enumerate() {
            ids.push(tree.intern_ext(e));
            cats.push(*c);
            by_cat[c.index()].push(rank);
            acc += FREQ[c.index()] / ((rank + 2) as f64).powf(1.08);
            cdf.push(acc);
        }
        // Files without extensions get their own entry.
        let none = tree.intern_ext("");
        ids.push(none);
        cats.push(Category::Other);
        by_cat[Category::Other.index()].push(names.len());
        acc += 0.06 * acc;
        cdf.push(acc);
        let mut ext_names: Vec<String> = names.into_iter().map(|(e, _)| e).collect();
        ext_names.push(String::new());
        Self {
            rng,
            tree,
            exts: ExtTable { names: ext_names, ids, cats, cdf, by_cat },
            buf: String::with_capacity(96),
            since_hook: 0,
            stopped: false,
            hook,
        }
    }

    #[inline]
    fn tick(&mut self) {
        self.since_hook += 1;
        if self.since_hook >= 2048 {
            self.since_hook = 0;
            if !(self.hook)(self.tree) {
                self.stopped = true;
            }
        }
    }

    fn pick_ext(&mut self) -> usize {
        let total = *self.exts.cdf.last().unwrap();
        let x = self.rng.f64() * total;
        self.exts.cdf.partition_point(|&c| c < x).min(self.exts.cdf.len() - 1)
    }

    fn pick_ext_in(&mut self, cat: Category) -> usize {
        let list = &self.exts.by_cat[cat.index()];
        // Prefer earlier, more common extensions within each category as well.
        let n = list.len() as f64;
        let r = (self.rng.f64().powf(2.2) * n) as usize;
        list[r.min(list.len() - 1)]
    }

    fn file_size(&mut self, cat: Category) -> u64 {
        if self.rng.chance(0.04) {
            return 0;
        }
        let (median, sigma) = SIZE_MODEL[cat.index()];
        let logical = self.rng.lognormal(median, sigma).min(180e9) as u64;
        alloc_size(logical)
    }

    fn file_name(&mut self, rank: usize) {
        self.buf.clear();
        let r = &mut self.rng;
        match r.below(6) {
            0 => {
                let _ = write!(self.buf, "{:08x}", r.next_u64() as u32);
            }
            1 => {
                let _ = write!(self.buf, "IMG_2024{:04}_{:06}", r.below(1231), r.below(1_000_000));
            }
            2 => {
                let a = SYL[r.below(SYL.len() as u64) as usize];
                let b = SYL[r.below(SYL.len() as u64) as usize];
                let _ = write!(self.buf, "{a}_{b}");
            }
            _ => {
                let a = SYL[r.below(SYL.len() as u64) as usize];
                let b = SYL[r.below(SYL.len() as u64) as usize];
                let _ = write!(self.buf, "{a}{b}{}", r.below(1000));
            }
        }
        let ext = &self.exts.names[rank];
        if !ext.is_empty() {
            self.buf.push('.');
            self.buf.push_str(ext);
        }
    }

    fn dir_name(&mut self) {
        self.buf.clear();
        let r = &mut self.rng;
        match r.below(10) {
            0 => {
                let _ = write!(
                    self.buf,
                    "{{{:08X}-{:04X}-{:04X}-{:012X}}}",
                    r.next_u64() as u32,
                    r.below(0x10000),
                    r.below(0x10000),
                    r.next_u64() & 0xffff_ffff_ffff
                );
            }
            1 | 2 => {
                let d = DIRS[r.below(DIRS.len() as u64) as usize];
                let _ = write!(self.buf, "{d}{}", r.below(100));
            }
            3 => {
                let a = SYL[r.below(SYL.len() as u64) as usize];
                let _ = write!(self.buf, "{a}-{}.{}.{}", r.below(20), r.below(40), r.below(300));
            }
            _ => {
                self.buf.push_str(DIRS[r.below(DIRS.len() as u64) as usize]);
            }
        }
    }

    fn emit_file(&mut self, parent: u32, rank: usize) {
        if self.stopped {
            return;
        }
        let cat = self.exts.cats[rank];
        let size = self.file_size(cat);
        self.file_name(rank);
        let id = self.exts.ids[rank];
        self.tree.add_file(parent, &self.buf, id, size);
        self.tick();
    }

    fn emit_dir(&mut self, parent: u32) -> u32 {
        self.dir_name();
        let id = self.tree.add_dir(parent, &self.buf);
        self.tick();
        id
    }

    fn named_dir(&mut self, parent: u32, name: &str) -> u32 {
        let id = self.tree.add_dir(parent, name);
        self.tick();
        id
    }

    /// Distribute `budget` nodes randomly under `parent`.
    fn fill(&mut self, parent: u32, budget: u64, depth: u32, bias: Option<Category>) {
        if budget == 0 || self.stopped {
            return;
        }
        let dominant = if self.rng.chance(0.5) {
            Some(match bias {
                Some(c) if self.rng.chance(0.8) => self.pick_ext_in(c),
                _ => self.pick_ext(),
            })
        } else {
            None
        };
        let mut nfiles = self.rng.lognormal(5.0, 1.45).round() as u64;
        if depth >= 44 || budget <= 3 || nfiles + 1 >= budget {
            nfiles = budget;
        }
        for _ in 0..nfiles {
            let rank = match dominant {
                Some(d) if self.rng.chance(0.75) => d,
                _ => match bias {
                    Some(c) if self.rng.chance(0.35) => self.pick_ext_in(c),
                    _ => self.pick_ext(),
                },
            };
            self.emit_file(parent, rank);
        }
        let rem = budget - nfiles;
        if rem == 0 {
            return;
        }
        let mut ndirs = if self.rng.chance(0.12) {
            1
        } else {
            self.rng.lognormal(3.0, 0.95).ceil() as u64
        };
        ndirs = ndirs.clamp(1, rem.min(400));
        let mut weights: Vec<f64> = (0..ndirs).map(|_| self.rng.lognormal(1.0, 1.35)).collect();
        let wsum: f64 = weights.iter().sum();
        for w in &mut weights {
            *w /= wsum;
        }
        let spare = rem - ndirs;
        let mut shares: Vec<u64> = weights.iter().map(|w| (w * spare as f64) as u64).collect();
        let used: u64 = shares.iter().sum();
        shares[0] += spare - used;
        for share in shares {
            if self.stopped {
                return;
            }
            let child = self.emit_dir(parent);
            self.fill(child, share, depth + 1, bias);
        }
    }

    /// Many direct child files from the same category.
    fn flat_files(&mut self, parent: u32, count: u64, kind: FlatKind) {
        let (prefix, ext, median, sigma) = match kind {
            FlatKind::Thumb => ("thumb_", "db", 24_000.0, 0.8),
            FlatKind::Blob => ("", "", 2_000_000.0, 1.5),
            FlatKind::Dataset => ("img_", "jpg", 180_000.0, 0.6),
            FlatKind::Npm => ("", "", 20_000.0, 1.5),
        };
        let ext_id = self.tree.intern_ext(ext);
        for i in 0..count {
            if self.stopped {
                return;
            }
            self.buf.clear();
            match kind {
                FlatKind::Thumb | FlatKind::Dataset => {
                    let _ = write!(self.buf, "{prefix}{i:07}.{ext}");
                }
                FlatKind::Blob => {
                    let _ = write!(self.buf, "{:016x}", self.rng.next_u64());
                }
                FlatKind::Npm => {
                    let _ = write!(self.buf, "{:016x}{:016x}", self.rng.next_u64(), self.rng.next_u64());
                }
            }
            let size = alloc_size(self.rng.lognormal(median, sigma) as u64);
            self.tree.add_file(parent, &self.buf, ext_id, size);
            self.tick();
        }
    }

    /// WinSxS: many subdirectories with 1-6 files each.
    fn winsxs(&mut self, parent: u32, count: u64) -> u64 {
        let dll = self.tree.intern_ext("dll");
        let mui = self.tree.intern_ext("mui");
        let man = self.tree.intern_ext("manifest");
        let mut made = 0;
        for i in 0..count {
            if self.stopped {
                break;
            }
            self.buf.clear();
            let w = SYL[self.rng.below(SYL.len() as u64) as usize];
            let _ = write!(
                self.buf,
                "amd64_microsoft-windows-{w}_31bf3856ad364e35_10.0.{}.{}_none_{:016x}",
                19041 + self.rng.below(4000),
                i % 5000,
                self.rng.next_u64()
            );
            let d = self.tree.add_dir(parent, &self.buf);
            self.tick();
            made += 1;
            let files = 1 + self.rng.below(6);
            for f in 0..files {
                let (ext, name) = match f % 3 {
                    0 => (dll, "dll"),
                    1 => (mui, "mui"),
                    _ => (man, "manifest"),
                };
                self.buf.clear();
                let _ = write!(self.buf, "{w}{f}.{name}");
                let size = self.file_size(Category::Program);
                self.tree.add_file(d, &self.buf, ext, size);
                self.tick();
                made += 1;
            }
        }
        made
    }
}

#[derive(Clone, Copy)]
enum FlatKind {
    Thumb,
    Blob,
    Dataset,
    Npm,
}

/// Convert logical size to allocation: 4 KiB clusters; tiny resident files use zero allocated bytes.
pub fn alloc_size(logical: u64) -> u64 {
    if logical < 700 { 0 } else { logical.div_ceil(4096) * 4096 }
}

/// Generate a complete tree for the preset.
pub fn generate(spec: &GenSpec, tree: &mut TreeBuilder, hook: &mut dyn FnMut(&mut TreeBuilder) -> bool) {
    let mut g = Generator::new(spec.seed, tree, hook);
    let root = g.named_dir(NONE, "C:");
    // A few large files at the root.
    let sys = g.tree.intern_ext("sys");
    for (name, size) in [
        ("pagefile.sys", 16u64 << 30),
        ("hiberfil.sys", 6_871_947_674),
        ("swapfile.sys", 256u64 << 20),
    ] {
        g.tree.add_file(root, name, sys, size);
    }
    let tmp = g.tree.intern_ext("tmp");
    g.tree.add_file(root, "DumpStack.log.tmp", tmp, 12_288);

    let huge = spec.thumbcache + spec.blobs + spec.dataset + spec.npm_content + spec.winsxs * 4;
    let random_budget = spec.nodes.saturating_sub(huge + 64);

    // (Path, random budget weight, preferred category).
    let plan: &[(&str, f64, Option<Category>)] = &[
        ("Windows", 0.08, Some(Category::Program)),
        ("Windows\\System32", 0.05, Some(Category::Program)),
        ("Windows\\WinSxS", 0.0, None),
        ("Program Files", 0.10, Some(Category::Program)),
        ("Program Files (x86)", 0.06, Some(Category::Program)),
        ("ProgramData", 0.035, Some(Category::System)),
        ("ProgramData\\Package Cache\\blobs", 0.0, None),
        ("Users\\Example\\AppData\\Local", 0.13, Some(Category::System)),
        ("Users\\Example\\AppData\\Local\\Temp\\thumbcache", 0.0, None),
        ("Users\\Example\\AppData\\Local\\npm-cache\\_cacache\\content-v2", 0.0, None),
        ("Users\\Example\\AppData\\Roaming", 0.05, None),
        ("Users\\Example\\Documents", 0.05, Some(Category::Document)),
        ("Users\\Example\\Pictures", 0.06, Some(Category::Image)),
        ("Users\\Example\\Videos", 0.003, Some(Category::Video)),
        ("Users\\Example\\Music", 0.03, Some(Category::Audio)),
        ("Users\\Example\\Downloads", 0.008, Some(Category::Archive)),
        ("Users\\Example\\.cargo", 0.04, Some(Category::Code)),
        ("Projects", 0.19, Some(Category::Code)),
        ("Projects\\dataset\\images", 0.0, None),
        ("Games", 0.012, Some(Category::Archive)),
        ("VMs", 0.0005, Some(Category::Archive)),
        ("$Recycle.Bin", 0.005, None),
    ];
    let wsum: f64 = plan.iter().map(|p| p.1).sum();
    let mut made: std::collections::HashMap<String, u32> = Default::default();
    for (path, weight, bias) in plan {
        if g.stopped {
            break;
        }
        // Create named directories along each path.
        let mut parent = root;
        let mut acc = String::new();
        for seg in path.split('\\') {
            if !acc.is_empty() {
                acc.push('\\');
            }
            acc.push_str(seg);
            parent = match made.get(&acc) {
                Some(&id) => id,
                None => {
                    let id = g.named_dir(parent, seg);
                    made.insert(acc.clone(), id);
                    id
                }
            };
        }
        match *path {
            "Windows\\WinSxS" => {
                g.winsxs(parent, spec.winsxs);
            }
            "ProgramData\\Package Cache\\blobs" => g.flat_files(parent, spec.blobs, FlatKind::Blob),
            "Users\\Example\\AppData\\Local\\Temp\\thumbcache" => {
                g.flat_files(parent, spec.thumbcache, FlatKind::Thumb)
            }
            "Users\\Example\\AppData\\Local\\npm-cache\\_cacache\\content-v2" => {
                g.flat_files(parent, spec.npm_content, FlatKind::Npm)
            }
            "Projects\\dataset\\images" => g.flat_files(parent, spec.dataset, FlatKind::Dataset),
            "VMs" => {
                let vhdx = g.tree.intern_ext("vhdx");
                g.tree.add_file(parent, "ubuntu-24.04.vhdx", vhdx, 61_203_283_968);
                g.tree.add_file(parent, "win11-dev.vhdx", vhdx, 42_949_672_960);
                let budget = (random_budget as f64 * weight / wsum) as u64;
                g.fill(parent, budget, 1, *bias);
            }
            _ => {
                let budget = (random_budget as f64 * weight / wsum) as u64;
                let depth = path.split('\\').count() as u32;
                g.fill(parent, budget, depth, *bias);
            }
        }
    }
    // Add rounding leftovers to Projects to keep the total close to the target.
    let have = g.tree.node_count() as u64;
    if !g.stopped && have < spec.nodes {
        let p = made["Projects"];
        g.fill(p, spec.nodes - have, 1, Some(Category::Code));
    }
    if !g.stopped {
        (g.hook)(g.tree);
    }
}
