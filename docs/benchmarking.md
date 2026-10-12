# Benchmarking

The model benchmark exercises generation, snapshots, child sorting, large
directory expansion, treemap layout, bitmap rendering, and scan publication.
It does not measure GPU presentation or end-to-end mouse latency.

Build the benchmark from a native Windows terminal:

```powershell
./build.ps1 -Bench
./tools/benchmark.ps1 -Nodes 1000000 -Runs 3
```

The wrapper writes `benchmarks/model.json`, including the executable's SHA-256,
arguments, timestamp, and each run's measured values. Use `-Output` to choose
another file and `-Width` / `-Height` to vary the treemap dimensions. Large
synthetic trees can consume substantial CPU and memory; start with a small
tier before increasing the node count.

For a real scan, provide a directory or volume explicitly:

```powershell
./tools/benchmark.ps1 -ScanPath C:\ -Runs 3 -Output benchmarks/ntfs.json
./tools/benchmark.ps1 -ScanPath C:\ -Walk -Runs 3 -Output benchmarks/walk.json
```

NTFS metadata scanning needs an elevated terminal. `-Walk` forces normal
directory enumeration. Scan errors and permissions affect the set of entries
that can be counted. These commands only scan; they do not delete files.

For comparisons, use the same executable, directory, permission level, screen
dimensions, and power settings. Record the CPU, storage type, Windows version,
and whether the filesystem cache was warm. Retain individual runs and report a
median rather than selecting the fastest result.

`benchmarks/` is ignored by Git. Real-scan results include the requested path;
review them before sharing. Reports from one machine should not be treated as
performance guarantees for another.

The GUI also accepts `--synthetic=1000000` (or `ACRE_SYNTHETIC=1000000`) to
explore a generated tree without scanning a drive. This is a development mode,
not a source of real disk usage figures.
