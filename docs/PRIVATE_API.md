# Automatic Spotlight exclusions: investigation and implementation notes

This document records the investigation of
[`rohiitgit/nohog`](https://github.com/rohiitgit/nohog), the exact private API
it uses, and how `spotlight-dev` implements the same mechanism safely.

Investigated commit: **`e31bd296aa750d60265977966cc301546b614944`**
(`e31bd29`, nohog `0.1.2`, inspected 2026-11-16).

Relevant nohog files:

- `Sources/NohogCore/Reconcile.swift` — `SpotlightExclusionStore`, `Planner`,
  `applyExclusions`, `reconcile`
- `Sources/NohogCore/Selftest.swift` — control/excluded `mdfind` test
- `Sources/nohog/System.swift` — selftest wiring, install gate, state paths
- `Sources/nohog/Watch.swift` — FSEvents reconcile loop
- `README.md` — tested macOS versions, permissions, safety claims

---

## 1. Exact API used by nohog

### Framework and load path

```swift
private static let frameworkPath =
    "/System/Library/Frameworks/CoreServices.framework/Frameworks/Metadata.framework/Metadata"

guard let handle = dlopen(Self.frameworkPath, RTLD_NOW) else { ... }
```

Import in Swift is only CoreFoundation/Foundation; the private framework is
loaded dynamically. There is no link-time private SDK dependency, no
`Metadata.framework` umbrella header, and no XPC call in nohog.

On macOS 27.2.1 (build 26B131) the nested framework executable is supplied by
the dyld shared cache. The loose path is not visible in Finder/`ls`, but
`dlopen` resolves it and `dladdr` reports:

```text
/System/Library/Frameworks/CoreServices.framework/Versions/A/Frameworks/Metadata.framework/Versions/A/Metadata
```

### Symbols and nohog's declarations

```swift
private typealias CopyFn = @convention(c) () -> Unmanaged<CFArray>?
private typealias SetFn  = @convention(c) (CFString, UInt32) -> Unmanaged<CFTypeRef>?

let copy = dlsym(handle, "_MDCopyExclusionList")
let set  = dlsym(handle, "_MDSetExclusion")
```

```swift
public func list() throws -> [String] {
    guard let array = copyExclusionList()?.takeRetainedValue() else { return [] }
    guard let paths = (array as NSArray) as? [String] else { ... }
    return paths
}

public func setExcluded(_ path: String, _ excluded: Bool) throws {
    // The return value is undocumented (nil in practice); callers verify by re-listing.
    _ = setExclusion(path as CFString, excluded ? 1 : 0)
}
```

Semantics:

- `_MDCopyExclusionList()` — returns a **+1 retained `CFArray` of `CFString`
  paths**. “Copy” follows CoreFoundation ownership rules.
- `_MDSetExclusion(path, 1)` — add `path` to the same list shown in
  **System Settings → Spotlight → Search Privacy**.
- `_MDSetExclusion(path, 0)` — remove `path`.
- Changes apply live and purge already-indexed content under the path.
- The call is idempotent: setting the same state twice is harmless.
- The return value is **not** used as proof. nohog re-lists with
  `_MDCopyExclusionList` after each round of changes.

### Observed ABI on the test machine

Build: macOS 27.2.1, **26B131**, arm64.

`nm`/`strings` cannot open the loose framework file because it lives in the
dyld shared cache. Runtime probing and `lldb` disassembly confirmed the ABI:

- `_MDCopyExclusionList` returns a pointer to a CF object; type ID check passes
  for `CFArray`.
- `_MDSetExclusion` takes the CFString in `x0` and a 32-bit value in `w1`.
  The implementation stores the low 32 bits into its `mdsSetPathExclusion`
  request (`str w22, [x8, #0x28]`), so nohog's `UInt32` is correct.
- The function returns an `int32_t` in `w0`: `0` on success in live probes;
  negative/small positive error/status values on failure paths. nohog's
  `Unmanaged<CFTypeRef>?` return declaration and ignored result are therefore
  harmless but not a success contract.
- Re-reading the list immediately after a successful set showed the path. A
  second set returned `0` and left exactly one entry. A second removal returned
  `0` and left zero entries.

Our Rust FFI uses the observed ABI:

```rust
type CopyFn = unsafe extern "C" fn() -> CFArrayRef;
type SetFn  = unsafe extern "C" fn(CFStringRef, u32) -> i32;
```

Symbols are resolved with `dlopen`/`dlsym`; missing symbols are reported as
`Unavailable`, never guessed or hard-coded to a boolean.

---

## 2. Framework dependencies

- `Metadata.framework` (nested under `CoreServices.framework`) for the two
  private symbols.
- `CoreFoundation.framework` for `CFStringCreateWithBytes`,
  `CFStringGetCString`, `CFArrayGetCount`, `CFArrayGetValueAtIndex`,
  `CFGetTypeID`, `CFRelease`.
- `libSystem`/`libdl` for `dlopen`, `dlsym`, `dlerror`.
- No AppKit, no Swift runtime requirement for the Rust implementation.
- No private headers, no `.tbd` stubs, no link-time `-framework Metadata`.

---

## 3. Permissions

Tested and documented by nohog: **normal user, no sudo, no Full Disk Access**.

- The API writes the per-user Search Privacy list and talks to `mds` over its
  normal mach service; no root-only `mdutil -P` or direct plist write is used.
- Operations on protected locations can still trigger macOS privacy prompts
  (for example a removable volume); those are OS permissions, not a
  requirement of the API itself.
- Running the CLI as root is refused because the exclusion list belongs to the
  logged-in user, not root.

---

## 4. nohog's verification mechanism

Two layers:

1. **Actual state**: after every change, re-read
   `_MDCopyExclusionList()` and compare. `applyExclusions` retries the same
   idempotent requests with delays `[0, 1, 2, 4, 8]` seconds because `mds`
   drops requests while it is busy purging a large folder.
2. **Independent end-to-end selftest** (`Sources/NohogCore/Selftest.swift`):

   - create `<home>/nohog-selftest-<random>/control` and `.../excluded`;
   - add only `excluded` to Spotlight's list;
   - confirm the list contains it;
   - write the same unique token into `probe.txt` in both folders;
   - poll `mdfind -onlyin <dir> <token>` until the **control** file is indexed
     (up to 30 s);
   - wait 3 s as a grace period, then require that the **excluded** file is not
     returned;
   - remove the exclusion and delete the temporary tree in all paths;
   - only a pass is recorded per macOS build; after an OS update it runs again.

A failure is treated as “Spotlight did not honor exclusions”, not as success.
noohog gates `install` on this test.

Our Rust selftest implements the same control/excluded `mdfind` check, with
additional checks for repeated add/remove and preservation of pre-existing
exclusions.

---

## 5. What `spotlight-dev` implements

### Modules

- `src/metadata.rs` — dynamically resolved FFI, CFArray/CFString conversion,
  type-ID validation, `Unavailable` errors.
- `src/exclusion.rs` — canonical paths, retry-and-verify apply, reconcile
  planning, managed/user bookkeeping, selftest, opt-outs.
- `src/database.rs` + `migrations/002_exclusions.sql` — `exclusions` and
  `exclusion_opt_outs`, schema v2 (additive migration from v1).
- `src/daemon.rs` — runs the selftest once per macOS build when automatic
  exclusions are enabled, then syncs after startup scans, FSEvents discovery,
  and periodic reconciliation.
- `src/cli.rs` — `exclude`, `unexclude`, `verify <path>`, `sync`, `selftest`,
  plus the existing manual workflow.

### Safety properties

- **Never claim success from the API return value.** A path is `excluded` only
  when `_MDCopyExclusionList` contains it after the retry schedule.
- **Never remove user entries.** The `exclusions` table records `managed = 1`
  only for paths this application added. `sync` removes only `managed = 1`
  rows whose directory disappeared, and only when the containing root is still
  mounted/exists.
- **Idempotent.** Repeated `exclude`, `unexclude`, and `sync` calls re-read the
  list and send only pending changes.
- **Explicit opt-out.** `unexclude` writes `exclusion_opt_outs`; `sync` skips
  that path until an explicit `exclude` clears the opt-out.
- **Safe fallback.** Failure marks the directory `pending_manual_exclusion`
  with an error, so `export` / `mark` / `verify --confirm` still work.
- **Graceful unsupported state.** `dlopen`/`dlsym`/type-ID failures produce
  `Spotlight exclusion API unavailable`; the daemon logs it and continues with
  discovery/export. No root is requested.
- **No direct Spotlight store changes.** The only write is `_MDSetExclusion`;
  no plist or database is edited.

### CLI exit behavior

- `exclude` / `unexclude`: non-zero if Spotlight does not confirm the change.
- `sync`: non-zero if any requested change is not confirmed; user entries are
  reported but never changed.
- `selftest`: `0` pass, `1` fail/unsupported, `2` inconclusive (Spotlight did
  not index the control within 30 s).
- `verify PATH`: `0` for `excluded`; `--strict` exits `3` otherwise.

---

## 6. Test evidence

### Machine

- macOS 27.2.1 (26B131), Apple Silicon
- Rust 1.99.1
- no root, no Full Disk Access

### Manual API probe (before implementation)

- `_MDCopyExclusionList` returned an empty array.
- `_MDSetExclusion(tempdir, 1)` returned `0`; re-list contained the path.
- `_MDSetExclusion(tempdir, 1)` again returned `0`; list still had one entry.
- Control file indexed by `mdfind` after 3 s; excluded file never appeared.
- `_MDSetExclusion(tempdir, 0)` twice returned `0`; list empty.
- No user exclusions existed on this machine; the selftest's baseline
  preservation check passed.

### `spotlight-dev selftest` (live)

```
Result: pass
  macOS:   27.2.1 (26B131)
  backend: Metadata.framework (_MDCopyExclusionList/_MDSetExclusion)
  [PASS] exclusion recorded by Spotlight
  [PASS] repeated exclusion is idempotent
  [PASS] normal source files remain searchable
  [PASS] excluded content is not indexed
  [PASS] unexclude removes the path
  [PASS] repeated unexclude is idempotent
  [PASS] pre-existing exclusions remain intact
Checks: 7 passed, 0 failed.
```

### Daemon recovery

- Started the daemon with a temporary data directory and scan root.
- Created `projects/app/node_modules`; it was excluded within ~3 FSEvents
  polls (debounce 200 ms).
- Stopped and restarted the daemon; the exclusion and SQLite state persisted.
- Removed the exclusion behind the daemon's back with a raw API probe, then
  restarted; startup reconcile re-added it and re-verified it.
- `unexclude` + `sync` did not re-add the path (opt-out worked).
- `exclude` again cleared the opt-out and re-added the path.
- All temporary exclusions/directories were removed; final `_MDCopyExclusionList`
  was empty.

A full machine reboot was not performed in this environment. The same startup
reconcile path exercised by the process-restart test is what runs at login;
Spotlight's exclusion list is persistent, and `sync` re-adds anything a macOS
update drops. The code re-runs the selftest when `kern.osversion` changes.

---

## 7. Risks and limits

- **Private, undocumented API.** Apple can remove or change it. Dynamic symbol
  detection and the per-build selftest are the mitigation; there is no public
  contract.
- **Already-indexed content is purged** when a path is excluded. This is the
  intended behavior but is a system state change.
- **Small race window.** A brand-new `node_modules` may be indexed for a few
  seconds before FSEvents + debounce + exclusion complete. nohog documents the
  same ~3 s window.
- **Volume/unmount safety.** Stale managed entries are removed only when the
  containing configured root still exists. An unmounted drive's exclusions are
  kept for the next mount.
- **Ad-hoc signed release.** Releases are not notarized (no paid Apple
  account). Browser-downloaded zips may need
  `xattr -d com.apple.quarantine`.
- **No direct plist edits.** `VolumeConfiguration.plist` and
  `.metadata_never_index` are not used; current macOS ignores them for this
  purpose.
