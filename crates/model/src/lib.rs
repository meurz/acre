//! Acre's data layer: compact directory trees, synthetic data, treemap layout, and bitmap rendering.
//! Independent of the UI framework; views use only [`TreeRead`]. Synthetic data and live scans use [`Snapshot`],
//! while completed scans use acre-core's directory tree ([`RealTree`]).

pub mod ext;
pub mod format;
pub mod generate;
pub mod layout;
pub mod live;
pub mod read;
pub mod real;
pub mod render;
pub mod scan;
pub mod tree;

/// Scan core, also used by the UI for volume listing, elevation, and the Recycle Bin.
pub use acre_core as core;
pub use ext::{CATEGORY_COUNT, Category};
pub use generate::GenSpec;
pub use layout::{Layout, LayoutParams, TmRect};
pub use read::{ExtStat, TreeRead};
pub use real::RealTree;
pub use tree::{NONE, Snapshot, TreeBuilder};

#[cfg(test)]
mod tests;
