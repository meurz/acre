# Acre

A fast Windows disk usage explorer built with Rust and GPUI. Find large files
with a directory tree, an extension summary, and an interactive treemap.

[简体中文](README.zh-CN.md) · [Download](https://github.com/meurz/acre/releases) · [Acre Core](https://github.com/meurz/acre-core)

## Features

- Scan NTFS metadata with administrator access, or enumerate directories with
  normal permissions. Progress stays visible and scans can be cancelled.
- Browse volumes or choose a folder. Compare allocated sizes and count hard
  links once.
- Keep the tree and treemap selection in sync. Double-click folders to zoom in
  and use Backspace or the mouse back button to go up.
- Reveal an entry in Explorer, copy its path, rescan it, or delete it after
  confirmation. Permanent deletion is available separately from the Recycle Bin.
- Switch between English and Simplified Chinese without restarting. Acre
  detects the Windows UI language on first launch and remembers your choice.

## Download and run

Download `acre-<version>-windows-x86_64.zip` from
[Releases](https://github.com/meurz/acre/releases), extract it, and run `acre.exe`.
The archive includes the executable and license notices. Windows 10/11 x64 is
supported; no installer is required.

Administrator accounts may receive a Windows elevation prompt on startup to
allow fast NTFS scanning. To use normal permissions explicitly:

```powershell
./acre.exe --no-elevate
```

The release's `SHA256SUMS` file lists hashes for the archive and standalone
executable. Verify a download with:

```powershell
Get-FileHash ./acre-0.2.0-windows-x86_64.zip -Algorithm SHA256
```

Language preferences are stored in `%APPDATA%\Acre\settings.json`. English is
used when the system language is unsupported. Windows file dialogs and system
error details use the operating system's language.

## Build

Install [Rust](https://rustup.rs/) and Visual Studio Build Tools with the
**Desktop development with C++** workload and Windows SDK. Build in a native
Windows terminal from a local checkout:

```powershell
git clone https://github.com/meurz/acre.git
cd acre
./build.ps1                 # dist/acre.exe
./build.ps1 -Test           # Model and localization tests
./build.ps1 -Check          # Check the desktop application
./build.ps1 -Bench          # dist/model-bench.exe
```

`rust-toolchain.toml` pins Rust 1.98.1. The script uses `cargo` from `PATH`,
limits Cargo to two concurrent jobs by default, and puts build artifacts in
`target/`. Set `CARGO_TARGET_DIR` to use another directory or pass `-Jobs` to
change the concurrency. Standard builds use the committed lockfile.

Acre Core is a public dependency pinned to a Git revision. To develop against a
local copy of its crate:

```powershell
./build.ps1 -CorePath ../acre-core/crates/acre-core
```

A local override can update `Cargo.lock`; restore the lockfile before committing
unrelated changes. No access token is needed for public builds.

## Development

- [Architecture](docs/architecture.md)
- [Benchmarking](docs/benchmarking.md)
- `crates/model`: data preparation and treemap calculations
- `crates/i18n`: localization and language preferences
- `crates/app`: the GPUI desktop application

`ACRE_NO_ELEVATE=1` also disables automatic elevation.
`ACRE_CONFIG_DIR` selects an alternative settings directory for isolated runs.
For synthetic data, run `acre.exe --synthetic=1000000`.

Windows CI runs the model and localization tests, builds the application, and
packages it. A matching `v<version>` tag publishes the Windows x64 ZIP,
standalone executable, and SHA-256 checksums.

## License

Acre's original code is dual-licensed under [MIT](LICENSE-MIT) or
[Apache-2.0](LICENSE-APACHE), at your option. Embedded fonts, GPUI, and other
third-party components retain their own licenses. See
[Third-party notices](THIRD_PARTY_NOTICES.md).
