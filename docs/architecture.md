# Architecture

Acre separates Windows scanning, data preparation, localization, and rendering.

| Layer | Responsibility |
| --- | --- |
| [Acre Core](https://github.com/meurz/acre-core) | NTFS metadata scanning, directory enumeration, disk usage accounting, and file operations |
| `crates/model` | A shared tree-reading interface, scan progress, sorted children, synthetic data, and treemap layout/rendering |
| `crates/i18n` | English and Simplified Chinese translations, locale selection, and saved preferences |
| `crates/app` | The GPUI desktop window, navigation, selection, commands, and drawing |

The UI reads both completed scans and live snapshots through `TreeRead`.
Completed scans retain Acre Core's tree with indexes for sorting and dominant
file categories. During a directory scan, chunked copy-on-write snapshots let
the worker publish progress while the UI continues reading its current tree.

The directory tree displays visible rows only. Treemap layout and bitmap
rendering run outside the UI thread. Cached GPUI views keep pointer movement
from rebuilding the main tree and treemap. The selected entry is shared by the
tree and treemap, so selecting either view reveals the corresponding entry in
the other.

English and Simplified Chinese strings live in the localization crate. Acre
detects the Windows UI language on first launch and falls back to English for
unsupported languages. The visible language controls change the interface and
save the choice in `%APPDATA%\Acre\settings.json`. `ACRE_CONFIG_DIR` can point to
a separate settings directory for isolated runs.

The scanning core is pinned to a Git revision in `crates/model/Cargo.toml`.
Update that revision and `Cargo.lock` together when upgrading the core.
