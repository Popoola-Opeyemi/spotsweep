# Performance measurements

These are real measurements from the development machine used to build and test
the tool. They are indicative, not a controlled benchmark suite. No personal
paths, usernames, or hardware serial numbers are recorded here.

## Environment

- macOS: 27.2.1
- Architecture: Apple Silicon (`arm64`)
- Rust: 1.99.1, release profile (`lto = "thin"`, `codegen-units = 1`, stripped)
- Filesystem: APFS
- Scan roots in the test configuration: 7 configured, 4 existing
- Rules: `node_modules`, `dist`, `build`, `out`, `coverage`, `vendor`;
  dot-directories disabled

## Build and artifact size

| Measurement | Result |
|---|---|
| Clean `cargo build --release` | 27.4 s |
| Incremental release rebuild | ~10.7 s |
| Release binary (`spotsweep`) | 4.2 MiB (4,3xx,xxx bytes) |
| Architecture | Mach-O 64-bit arm64 |
| Embedded SQLite | bundled, no external database process |
| Test suite (`cargo test`) | 61 unit + 6 integration tests (including a real FSEvents daemon child) |

## Full reconciliation scan

Command (LaunchAgent stopped to avoid concurrent writes):

```bash
/usr/bin/time -l ~/.local/bin/spotsweep rescan
```

| Measurement | Result |
|---|---|
| Wall time | 224 ms |
| Directories visited | 5,030 |
| Matching directories | 27 |
| New matches | 0 (already tracked) |
| Peak RSS (from `time -l`) | 9.9 MB |
| User CPU | 0.01 s |
| System CPU | 0.14 s |
| SQLite main database | 112 KiB |

The walk prunes `.git`, dot-directories, excluded paths, and every matched
artifact directory. It does not read file contents or compute directory sizes.
The Flutter SDK and application workspaces were the bulk of the visited tree.

The installer's first-ever scan reported 341 ms for the same roots, 27 matches,
and 0 missing.

## Idle background service

Measured after startup with no filesystem changes:

```bash
top -l 5 -s 2 -pid "$(spotsweep service status --json | jq .pid)" \
    -stats pid,cpu,mem,threads,time
```

| Measurement | Result |
|---|---|
| Idle CPU (repeated 2 s samples) | 0.0% |
| Resident memory (`top`) | 6.8 MB |
| Resident memory (`ps` RSS) | ~13.2 MB |
| Threads | 6 |
| Wakeups | none while blocked; FSEvents callbacks are event-driven |

The idle daemon blocks in `recv_timeout` until the next scheduled reconciliation
(6 hours) or a FSEvents/signal message arrives. There is no polling interval.

The six threads are: main daemon loop, FSEvents run loop, FSEvents→message
bridge, signal iterator, tracing non-blocking log writer, and a short-lived
thread used during startup/shutdown. All are blocked when idle.

## Incremental discovery

While the LaunchAgent was running:

1. Create `~/work/<probe>/node_modules`.
2. Poll `spotsweep list --json` every 50 ms.
3. Record the elapsed time until the directory appears.

| Measurement | Result |
|---|---|
| Discovery latency | **503 ms** |
| Full tree walk required | No |
| Directories visited by targeted discovery | 2–3 (probe subtree only) |
| Cursor persisted | Yes (`fsevent_last_event_id` in `meta`) |

The 503 ms is consistent with the configured 300 ms FSEvents latency plus the
400 ms debounce (some overlap). No global scan is performed for a single new
dependency directory.

A deletion test removed the probe tree; the daemon marked the recorded
`node_modules` row `missing` from the removal events without a full walk.

## Database

| Measurement | Result |
|---|---|
| Main DB after initial scan | 112 KiB |
| DB + WAL + SHM after export | ~289 KiB |
| Rows (27 directories, 16 rules, 7 roots) | small |
| Journal mode | WAL |
| Cache | 2 MiB (`cache_size=-2000`) |
| mmap | disabled (`mmap_size=0`) |
| Transactions | one per discovery batch (up to 256 records) |

## What was intentionally avoided

- `tokio`/async runtime: no benefit for one watcher and one SQLite connection
- polling: FSEvents + blocking channel wait
- directory size calculation: never performed
- reading file contents: never performed
- per-event SQLite writes: events are debounced and coalesced
- walking inside matched directories: pruned immediately
- scanning dot-directories by default: pruned (macOS already skips them)
- repeated full scans for new `node_modules`: targeted subtree walks only

## Reproducing

```bash
cargo build --release
spotsweep service stop
/usr/bin/time -l spotsweep rescan
spotsweep service start
spotsweep status
```

Results depend on cache state, number of projects, filesystem speed, and whether
the roots are on the boot volume.
