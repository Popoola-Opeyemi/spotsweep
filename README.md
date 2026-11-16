# spotlight-dev-exclusions

A small, native Rust utility that discovers development directories which should
be excluded from Spotlight indexing, tracks them in SQLite, watches for new ones
with macOS FSEvents, and adds them to the same **Spotlight Search Privacy** list
used by System Settings.

Automatic exclusion uses the private `_MDCopyExclusionList` / `_MDSetExclusion`
calls in `Metadata.framework`, the same mechanism used by
[`rohiitgit/nohog`](https://github.com/rohiitgit/nohog). Every change is verified
against Spotlight's reported list, and a self-test proves the API works on the
current macOS build before it is enabled. If the API is missing or a change is
not confirmed, the directory stays in the manual export workflow instead.

It never edits `VolumeConfiguration.plist`, never writes to Spotlight's private
databases, never modifies project files, and never requires root or Full Disk
Access for exclusion operations.

- **Project path (example):** `~/work/spotlight-dev-exclusions`
- **Binary name:** `spotlight-dev`
- **Target:** universal macOS build (Apple Silicon + Intel)
- **Runtime:** no Docker, Node.js, Python, or database server

---

## Install

### Homebrew

```bash
brew tap popoola-opeyemi/tap https://github.com/Popoola-Opeyemi/spotlight-dev-exclusions
brew install spotlight-dev-exclusions
brew services start spotlight-dev-exclusions
spotlight-dev selftest
```

### curl (universal binary)

```bash
curl -fsSL https://raw.githubusercontent.com/Popoola-Opeyemi/spotlight-dev-exclusions/main/scripts/install-release.sh | bash
```

The installer downloads the latest universal zip, verifies `SHA256SUMS` when
available, installs to `~/.local/bin/spotlight-dev`, runs the self-test,
performs an initial scan, and installs/starts the LaunchAgent. Overrides:
`PREFIX`, `VERSION`, `DATA_DIR`, `SPOTLIGHT_DEV_NO_SERVICE`,
`SPOTLIGHT_DEV_NO_SELFTEST`, `SPOTLIGHT_DEV_NO_SCAN`.

### Build from source

```bash
git clone https://github.com/Popoola-Opeyemi/spotlight-dev-exclusions
cd spotlight-dev-exclusions
cargo build --release
./target/release/spotlight-dev --version
```

Or build the universal release for both Apple Silicon and Intel:

```bash
rustup target add aarch64-apple-darwin x86_64-apple-darwin
./scripts/build-release.sh
```

Artifacts are written to `dist/`:

```
spotlight-dev-macos-universal.zip
spotlight-dev-aarch64-apple-darwin.tar.gz
spotlight-dev-x86_64-apple-darwin.tar.gz
SHA256SUMS
```

The universal binary is ad-hoc signed, not notarized. If a browser download is
quarantined:

```bash
xattr -d com.apple.quarantine ~/Downloads/spotlight-dev-macos-universal.zip
```

---

## Automatic exclusion and the fallback

Apple does not document a public API for adding individual folders to
Spotlight's Search Privacy list. The private API in `Metadata.framework` is
nevertheless used by the System Settings implementation itself:

```text
dlopen("/System/Library/Frameworks/CoreServices.framework/Frameworks/Metadata.framework/Metadata")
_MDCopyExclusionList()                    -> CFArrayRef
_MDSetExclusion(CFStringRef, uint32_t)    -> int32_t
```

`spotlight-dev` resolves those symbols dynamically (no private SDK headers, no
link-time dependency), calls them as your user, and **re-reads the exclusion
list after every request**. A non-zero API return value is never treated as
proof; a path is recorded as excluded only when Spotlight's list contains it.
The selftest additionally proves the behavior with `mdfind`:

1. create a control folder and an excluded folder under `$HOME`;
2. add only the excluded folder to Search Privacy;
3. wait for Spotlight to index a normal file in the control folder;
4. confirm `mdfind` finds the control file and does **not** find the file in the
   excluded folder;
5. remove the test exclusion and verify pre-existing user exclusions are still
   present.

If the API is unavailable, the selftest fails, a macOS update changes behavior,
or Spotlight does not confirm a request after retries, the affected directory is
marked `pending_manual_exclusion` and remains in the export workflow. The
export workflow is unchanged and always available.

The exact investigation, symbols, ABI observations and risks are documented in
[`docs/PRIVATE_API.md`](docs/PRIVATE_API.md).

### What about `sudo mdutil -P /`?

Researched and tested. `mdutil -P <volume>` dumps the volume's
**`VolumeConfiguration.plist`** (index store configuration). It does **not**
expose the Search Privacy exclusion list, it requires root, and its output is
not a stable public interface. It is therefore never used as proof of an
exclusion. `spotlight-dev verify --mdutil` shows the diagnostic result but
remains inconclusive.

### Risks of the private API

- Apple can change or remove the symbols in any macOS release.
- The API writes the same list System Settings uses; adding an exclusion causes
  Spotlight to purge already-indexed content under that path.
- The API is not covered by Apple's compatibility guarantees and may require
  re-verification after upgrades. This project records a passing selftest per
  macOS build and re-runs it when the build changes.
- It does not require root or Full Disk Access. Operations you request on
  protected folders may still trigger normal macOS privacy prompts.
- No direct plist/database edits and no undocumented XPC calls are used.

---

## Features

- Recursive discovery with pruning: matches are recorded but never descended into
- Defaults: `node_modules`, `dist`, `build`, `out`, `coverage`, `vendor`
- Dot-directories (`.next`, `.nuxt`, `.cache`, …) are not scanned by default
- Optional glob rules (`*.egg-info`, `dist-20??`, …)
- Configurable scan roots, with missing roots skipped safely
- Never follows symlinks outside a configured root
- SQLite state with WAL, indexes, transactions and interrupted-scan recovery
- Native FSEvents monitoring with persisted event IDs and dropped-history handling
- Debounced, targeted discovery — an `npm install` does not trigger a full walk
- Periodic reconciliation as a safety net (default every 6 hours)
- Automatic Spotlight exclusion through private `Metadata.framework` calls
- Post-change verification against Spotlight's actual exclusion list
- User- and CLI-managed exclusions, with explicit `unexclude` opt-outs
- Manual export grouped by project or flat, retained as the safe fallback
- User-level LaunchAgent; idle state is a blocking wait (no busy polling)
- Universal release build for Apple Silicon and Intel, Homebrew and curl installers
- Fully reversible install, upgrade and uninstall scripts

---

## Architecture

```
src/
  cli.rs          clap commands and user-facing output
  config.rs       TOML load/validate/mutate; default config embedded
  database.rs     SQLite schema, migrations, queries, transactions
  matcher.rs      exact-name + glob rules, dot-directory policy
  scanner.rs      initial/reconciliation walks and targeted discovery
  watcher.rs      documented CoreServices FSEvents wrapper (objc2 bindings)
  metadata.rs     dynamic private Metadata.framework exclusion API (FFI)
  exclusion.rs    retry/verify reconcile, managed bookkeeping, selftest
  daemon.rs       LaunchAgent service loop, debounce, reconciliation, signals
  exporter.rs     plain-text exclusion lists and project grouping
  verifier.rs     mdutil research/probe; explicit manual confirmation
  launchagent.rs  plist generation and launchctl lifecycle
  logging.rs      CLI stderr logging + rotating daemon log
  model.rs        statuses, rules, scan summaries
  paths.rs        Application Support/Logs/LaunchAgents locations
  util.rs         atomic writes, timestamps, tables, small helpers
migrations/       SQL schema (compiled into the executable)
config/           default TOML configuration
launchagent/      plist template (real plist is generated at install time)
scripts/          install, binary install, update, uninstall
tests/            end-to-end tests using temporary directory trees
```

### FSEvents choice

`notify` is a good cross-platform watcher, but its FSEvents backend does not
expose `FSEventStreamEventId`. Without event IDs you cannot persist a resumable
cursor or distinguish dropped history. This project uses the documented
`FSEventStreamCreate` API through `objc2-core-services` directly. It still uses
one run-loop thread and no async runtime.

FSEvents is treated as a *hint*, not a journal: event flags such as
`MustScanSubDirs`, `UserDropped`, `KernelDropped`, `EventIdsWrapped`,
`RootChanged`, and `Unmount` trigger a safe reconciliation. A normal event batch
only walks the affected subtree. If too many paths arrive at once, a
reconciliation replaces per-path walks.

Event IDs are per volume. The daemon stores one cursor and is intended for roots
on the boot volume; multi-volume setups are still recovered by reconciliation.

### Status lifecycle

| Status | Meaning |
|---|---|
| `discovered` | Transient state while a scan is being committed; promoted at scan end |
| `pending_manual_exclusion` | Exists and needs manual addition in Search Privacy |
| `exported` | Included in an export file, still awaiting manual action |
| `manually_applied` | You recorded that you added it in System Settings |
| `verified` | Spotlight's actual exclusion list contains the path (automatic), or you explicitly confirmed it |
| `missing` | The path no longer exists |

Automatic state is also recorded per path in the `exclusions` table:
`pending`, `excluded`, `failed` or `released`, plus whether this application
added the entry (`managed = 1`) or it was already present (`managed = 0`).
`sync` never removes `managed = 0` entries.

A directory is **never** marked excluded or verified merely because it was
discovered, exported, or included in a report. The only automatic proof is a
re-read of Spotlight's exclusion list, and the selftest adds an independent
`mdfind` content check.

---

## Requirements

- macOS (tested on macOS 27.2.1 build 26B131, Apple Silicon)
- Universal release supports both Apple Silicon and Intel
- Rust toolchain with Cargo (only when building)
- Xcode Command Line Tools (`xcode-select --install`) for the linker
- No administrative privileges, root, or Full Disk Access for exclusion
  operations; the LaunchAgent runs as your user

---

## Installation

From the repository:

```bash
./scripts/install.sh
```

Custom prefix:

```bash
./scripts/install.sh --prefix "$HOME/.local"
```

What it does:

1. Detects the Mac architecture and checks the release binary.
2. Builds with `cargo build --release` if needed.
3. Creates `~/.local/bin` (or `<prefix>/bin`) and installs `spotlight-dev`.
4. Creates `~/Library/Application Support/SpotlightDevExclusions/`.
5. Installs the default TOML config **only if missing**.
6. Initialises/migrates SQLite.
7. Runs the Spotlight exclusion selftest (10–35 seconds) when automatic
   exclusion is enabled and no pass is recorded for this macOS build.
8. Adds existing common development folders (`~/development`, `~/work`, …)
   that are not already configured.
9. Runs the initial discovery scan (automatically excluding new matches when
   the selftest passed) and writes an inventory report.
10. Installs and starts the user LaunchAgent.
11. Prints status and, if automatic exclusion is gated, the manual fallback
    instructions.

It never overwrites an existing config or database during an upgrade.

### Make the command available

If `~/.local/bin` is not on your `PATH`:

```bash
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.zshrc
source ~/.zshrc
```

### Install only the binary (relocation / another Mac)

```bash
# From anywhere:
~/work/spotlight-dev-exclusions/scripts/install-binary.sh \
  --prefix "$HOME/.local" --build --restart

# Or install a binary built elsewhere:
./scripts/install-binary.sh --binary /path/to/spotlight-dev --prefix "$HOME/.local"
```

This copies only the executable. Configuration, SQLite state, exports and logs
are untouched. The installed binary has no dependency on the repository, so the
repository may be moved or deleted after installation.

### Manual relocation

```bash
cp target/release/spotlight-dev /some/other/place/spotlight-dev
/some/other/place/spotlight-dev --data-dir "$HOME/Library/Application Support/SpotlightDevExclusions" status
```

If you relocate the installed binary, reinstall the LaunchAgent so its
`ProgramArguments` point at the new path:

```bash
spotlight-dev service install --binary /some/other/place/spotlight-dev
```

---

## Upgrade

```bash
cd ~/work/spotlight-dev-exclusions
./scripts/update.sh
```

This performs `git pull --ff-only` when a remote is configured, builds the
release binary, replaces only the installed executable atomically, and restarts
the LaunchAgent. The database and config are preserved. To build the current
checkout without pulling:

```bash
./scripts/update.sh --no-pull
```

---

## Automatic workflow

Run the self-test once for the current macOS build, then scan:

```bash
spotlight-dev selftest
spotlight-dev scan
spotlight-dev status
```

The daemon does the same automatically at startup (after an OS update it
re-runs the selftest). Newly discovered matches are excluded within a few
seconds of an FSEvents notification, and every request is verified by
re-reading Spotlight's list. Use `sync` to reconcile immediately:

```bash
spotlight-dev sync              # add missing, prune stale, preserve user entries
spotlight-dev sync --dry-run    # preview only
```

Explicit path operations:

```bash
spotlight-dev exclude ~/work/app/node_modules
spotlight-dev verify ~/work/app/node_modules
spotlight-dev unexclude ~/work/app/node_modules
```

`unexclude` records an opt-out, so the next `sync` does not immediately add the
path back. Running `exclude` again clears that opt-out.

### Manual fallback

If the API is unavailable, the selftest fails, or a change is not confirmed,
the directory remains `pending_manual_exclusion`. The export workflow is
unchanged:

1. Discover directories:

   ```bash
   spotlight-dev scan
   spotlight-dev pending
   ```

2. Generate the list:

   ```bash
   spotlight-dev export
   # or group by project:
   spotlight-dev export --group
   # only one rule/category:
   spotlight-dev export --type node_modules
   # all existing directories, not only pending:
   spotlight-dev export --all
   ```

   Files are written to
   `~/Library/Application Support/SpotlightDevExclusions/exports/` unless
   `--output` is given.

3. Open **System Settings → Spotlight → Search Privacy** and add each folder
   manually. The export file contains the absolute paths and instructions.

4. Record the manual action:

   ```bash
   spotlight-dev mark --all
   # or specific paths:
   spotlight-dev mark ~/work/app/node_modules
   ```

5. After visually confirming each folder is listed in Search Privacy:

   ```bash
   spotlight-dev verify --confirm
   ```

`verify --confirm` remains available for the manual path. `verify` with a path
reads Spotlight's actual exclusion list and reports `excluded`,
`not_excluded`, or `inconsistent`; `--strict` exits 3 unless the path is
confirmed excluded.

---

## CLI reference

Global flags (before or after the subcommand):

```
--data-dir DIR     Override ~/Library/Application Support/SpotlightDevExclusions
--config FILE      Override config.toml
-v, -vv, -vvv      Log verbosity (info, debug, trace)
```

| Command | Description |
|---|---|
| `spotlight-dev scan` | Incremental scan. First run is a full scan; later runs replay FSEvents from the stored cursor and walk only affected subtrees. Stale cursors trigger reconciliation. |
| `spotlight-dev scan --full` | Force a complete reconciliation walk. |
| `spotlight-dev scan --root ~/work` | Walk one configured root. |
| `spotlight-dev rescan` | Force reconciliation of all roots (`--root` for one). |
| `spotlight-dev status` | Totals, roots, service state, cursor, exports, verification. `--json` for machines. |
| `spotlight-dev list` | List directories. Filters: `--status`, `--rule`, `--root`, `--project`, `--limit`, `--json`. |
| `spotlight-dev pending` | Paths awaiting manual exclusion (`--limit`, `--json`). |
| `spotlight-dev report` | Inventory grouped by project (`--output FILE`). |
| `spotlight-dev export` | Generate a plain-text list. Flags: `--all`, `--pending`, `--type RULE`, `--group`, `--flat`, `--no-header`, `--output FILE`, `--open`. |
| `spotlight-dev exclude PATH` | Add a directory to Spotlight's Search Privacy list; verifies by re-reading the list. `--dry-run` previews. |
| `spotlight-dev unexclude PATH` | Remove a directory and record an opt-out so `sync` will not re-add it. |
| `spotlight-dev verify PATH` | Read Spotlight's actual exclusion list and an `mdfind` count. Outcomes: `excluded`, `not_excluded`, `inconsistent`. `--strict` exits 3 unless excluded. |
| `spotlight-dev verify --confirm` | Record visual confirmation for the manual workflow. `--mdutil` probes `mdutil -P` read-only (root only, never proof). |
| `spotlight-dev sync` | Reconcile all discovered directories with Spotlight's list. `--dry-run`, `--json`. Never removes user-managed entries. |
| `spotlight-dev selftest` | Prove the private API works on this macOS build using a control folder and `mdfind` (10–35 s). `--json`. |
| `spotlight-dev state` | SQLite schema, meta, table counts, recent scans/exports, FSEvents cursor. `--json`. |
| `spotlight-dev retry` | Re-check `missing` paths; restore directories that exist again. |
| `spotlight-dev logs` | Tail the rotating daemon log (`--lines N`, `--follow`). |
| `spotlight-dev add-path ~/NewProjects` | Add a root (stored in config + SQLite) and scan it. `--no-scan` to defer. |
| `spotlight-dev remove-path ~/NewProjects` | Remove a root without deleting project files or historical records. |
| `spotlight-dev config` | Show effective TOML (`show`), `path`, `init`, `set KEY VALUE`, `edit`, `reload`. |
| `spotlight-dev mark --all` | Record manual addition to Search Privacy (paths or `--export ID` also supported). |
| `spotlight-dev service install` | Generate and load the user LaunchAgent (`--binary FILE` optional). |
| `spotlight-dev service status` | Installed/loaded/running/PID (`--json`). |
| `spotlight-dev service start` / `stop` / `restart` | launchctl lifecycle. |
| `spotlight-dev service uninstall` | Remove the plist; data preserved. |
| `spotlight-dev uninstall` | Remove the LaunchAgent and app-managed Spotlight exclusions (user entries are preserved). `--keep-exclusions` skips removal; `--purge-data --yes` deletes app data; `--remove-binary` deletes the executable. |
| `spotlight-dev daemon` | Run the service in the foreground (used by LaunchAgent). |

Useful examples:

```bash
spotlight-dev status --json
spotlight-dev list --status pending
spotlight-dev list --rule node_modules --limit 20
spotlight-dev report --output ~/Desktop/inventory.txt
spotlight-dev export --group --output ~/Desktop/exclusions.txt
spotlight-dev config set rules.scan_dot_directories true
spotlight-dev config set rules.build "dist,build,out,storybook-static"
spotlight-dev add-path ~/src
spotlight-dev remove-path ~/Desktop
```

### Configuration keys

`config set` supports:

```
scan.follow_symlinks                 true|false
scan.max_depth                       0..256 (0 = unlimited)
scan.reconcile_interval_hours        1..720
scan.max_event_age_hours             1..2160
scan.exclude_paths                   comma-separated
rules.dependencies                   comma-separated names
rules.build                          comma-separated names
rules.generated                      comma-separated names
rules.optional                       comma-separated names
rules.dot_directories                comma-separated names
rules.globs                          comma-separated globs
rules.scan_dot_directories           true|false
export.group_by_project              true|false
export.include_header                true|false
export.directory                     path (empty = app data/exports)
daemon.event_latency_ms              0..5000
daemon.event_debounce_ms             0..10000
daemon.max_batch_events              100..100000
daemon.log_level                     trace|debug|info|warn|error
verify.mdutil_volume                 volume path
exclusions.auto                      true|false
exclusions.run_selftest_on_os_change true|false
exclusions.selftest_required         true|false
exclusions.remove_stale              true|false
```

Exact names are case-sensitive and only match directory names. Globs match the
directory basename, never a path, and are off by default. Dot-directory names
are only active when `rules.scan_dot_directories = true`.

Editing the file directly also works:

```bash
spotlight-dev config path
spotlight-dev config edit       # uses $EDITOR, else TextEdit
spotlight-dev config reload     # asks a running daemon to reload (SIGHUP)
```

A running daemon also notices config mtime changes at its next reconciliation.

---

## LaunchAgent

- Label: `dev.spotlightdevexclusions.agent`
- Plist: `~/Library/LaunchAgents/dev.spotlightdevexclusions.agent.plist`
- Runs as the logged-in user, never root
- `RunAtLoad` + `KeepAlive` on unsuccessful exit, `ThrottleInterval` 10 s
- `ProcessType=Background`, nice 10
- Standard out/error: `~/Library/Logs/SpotlightDevExclusions/launchd.{out,err}.log`
  (or `<data>/logs` when a custom data directory is used)
- Rotating application log: `daemon.log.YYYY-MM-DD`, seven days retained

```bash
spotlight-dev service status
spotlight-dev service restart
spotlight-dev logs --lines 100
spotlight-dev logs --follow
```

The generated plist contains personal absolute paths and is **never committed**;
only `launchagent/*.plist.template` is in Git.

---

## Data locations

| What | Location |
|---|---|
| Executable (default) | `~/.local/bin/spotlight-dev` |
| Application data | `~/Library/Application Support/SpotlightDevExclusions/` |
| Configuration | `…/SpotlightDevExclusions/config.toml` |
| SQLite database | `…/SpotlightDevExclusions/state.sqlite3` (+ WAL/SHM) |
| Exports | `…/SpotlightDevExclusions/exports/` |
| Inventory reports | `…/SpotlightDevExclusions/inventory-*.txt` |
| Daemon logs | `~/Library/Logs/SpotlightDevExclusions/` |
| LaunchAgent | `~/Library/LaunchAgents/dev.spotlightdevexclusions.agent.plist` |

Overrides for development/tests:

```bash
export SPOTLIGHT_DEV_EXCLUSIONS_DATA_DIR=/tmp/sdx-data
export SPOTLIGHT_DEV_EXCLUSIONS_CONFIG=/tmp/sdx-data/config.toml
export SPOTLIGHT_DEV_EXCLUSIONS_LAUNCH_AGENTS_DIR=/tmp/sdx-agents
```

The source repository, compiled binary, installed binary, configuration and
database are independent. Deleting one does not break the others (except that
deleting the installed binary obviously removes the command).

---

## Uninstall

```bash
./scripts/uninstall.sh                 # remove agent + binary, keep history
./scripts/uninstall.sh --purge-data    # asks before deleting data
./scripts/uninstall.sh --purge-data --yes
```

The uninstaller:

- stops and unloads the LaunchAgent
- removes the Spotlight exclusions this application added (never user-added ones)
- removes the installed executable
- removes the plist
- optionally preserves or removes the SQLite database, config, exports and logs
- never deletes the source repository

Use `--keep-exclusions` to leave app-managed Spotlight exclusions in place.

Or via the CLI:

```bash
spotlight-dev uninstall
spotlight-dev uninstall --keep-exclusions
spotlight-dev uninstall --purge-data --yes
```

---

## Performance

See [`docs/PERFORMANCE.md`](docs/PERFORMANCE.md) for measured release-build
numbers on the development machine (scan wall time, peak RSS, idle CPU/RSS,
binary and database size, incremental discovery latency).

Design choices that keep it light:

- one SQLite connection, WAL, 2 MiB cache, batched transactions
- matches are pruned before descent; no directory sizes are computed
- no async runtime; three long-lived daemon threads at most
  (FSEvents run loop, signal iterator, channel bridge), all blocked when idle
- FSEvents latency 300 ms and 400 ms debounce coalesce bursts
- event paths inside an artifact directory are discarded before SQLite work
- periodic reconciliation every 6 hours, not a polling loop

---

## Troubleshooting

**No directories found**

```bash
spotlight-dev status
spotlight-dev config show
spotlight-dev scan --root "$HOME/work"
```

Check that the roots exist. Missing roots are skipped, not treated as empty.

**Service is installed but not running**

```bash
spotlight-dev service status
spotlight-dev logs --lines 100
cat "$HOME/Library/Logs/SpotlightDevExclusions/launchd.err.log"
spotlight-dev service restart
```

**FSEvents stops delivering events**

The daemon handles dropped-history flags and reconciles. To force recovery:

```bash
spotlight-dev service restart
spotlight-dev rescan
```

**A directory vanished but is still listed**

```bash
spotlight-dev retry
spotlight-dev rescan
```

`retry` restores rows that exist again; `rescan` marks genuinely absent
directories `missing`.

**Automatic exclusion is gated or unsupported**

```bash
spotlight-dev selftest
spotlight-dev verify ~/work/app/node_modules
spotlight-dev sync --dry-run
```

The service only enables automatic exclusions after a passing selftest for the
current macOS build (`kern.osversion`). After an OS update it re-runs the test.
A failure leaves directories pending for the export workflow. The API is
private; see [`docs/PRIVATE_API.md`](docs/PRIVATE_API.md).

**A directory is still in Search Privacy after I removed it**

Spotlight can drop requests while `mds` is busy. `sync` and `unexclude` retry
with backoff and only report success when a re-read confirms the change. Run
`spotlight-dev sync` again, then `spotlight-dev verify PATH`.

**Permissions errors**

The scanner skips unreadable subtrees and does not infer `missing` from an
incomplete walk. Check Console/logs. Full Disk Access is only needed if your scan
roots contain protected locations such as some parts of `~/Library`; it is not
required for the exclusion API. The tool avoids system directories by design.

**I moved the installed binary**

Reinstall the LaunchAgent so it points at the new location:

```bash
spotlight-dev service install --binary /new/path/spotlight-dev
```

**I want to start over**

```bash
spotlight-dev service stop
mv "$HOME/Library/Application Support/SpotlightDevExclusions" \
   "$HOME/Library/Application Support/SpotlightDevExclusions.old"
spotlight-dev config init
spotlight-dev scan
```

---

## Development

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
```

The default config is embedded with `include_str!("../config/default.toml")`, so
the executable works when copied elsewhere. The SQLite schema is embedded from
`migrations/001_init.sql` and versioned with `PRAGMA user_version`.

Safety boundaries are enforced in code and documented in the modules:

- no direct writes to `VolumeConfiguration.plist`, Spotlight's private
  databases, `mds`, or undocumented XPC services
- the only private calls are the dynamically resolved
  `_MDCopyExclusionList` / `_MDSetExclusion` pair documented in
  `docs/PRIVATE_API.md`
- every claimed exclusion is verified by re-reading Spotlight's list, plus a
  `mdfind` control-folder check in the selftest
- user-managed Search Privacy entries are never removed by `sync`
- no modification of project files
- symlinks outside a root are never followed
- uninstall removes only entries this application added, and never deletes the
  repository

## License

MIT. See [LICENSE](LICENSE).
