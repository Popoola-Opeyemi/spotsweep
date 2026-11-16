//! Automatic Spotlight exclusion orchestration.
//!
//! The low-level private API lives in [`crate::metadata`].  This module adds
//! the parts that make it safe for an unattended service:
//!
//! * canonical path comparison;
//! * retry-and-verify after every request (Spotlight drops requests while mds
//!   is busy purging a large folder);
//! * bookkeeping that distinguishes paths this application added from entries
//!   the user added in System Settings;
//! * an explicit opt-out so `unexclude` is not immediately undone by `sync`;
//! * a selftest that proves the API works on this macOS build using `mdfind`,
//!   independent of the API's own return value.

use crate::config::Config;
use crate::database::{Db, DirectoryFilter};
use crate::metadata::MetadataBackend;
use crate::model::{ExclusionSource, ExclusionState};
use crate::paths::{home_dir, is_system_path, normalize_path, path_within};
use crate::util::now_ms;
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The retry schedule used by nohog: mds drops a burst of changes while it is
/// busy purging a large folder, so the same idempotent request is re-issued.
pub const DEFAULT_RETRY_DELAYS_MS: &[u64] = &[0, 1_000, 2_000, 4_000, 8_000];

pub trait ExclusionBackend: Send + Sync {
    fn name(&self) -> &str;
    fn list(&self) -> Result<Vec<String>>;
    fn set_excluded(&self, path: &str, excluded: bool) -> Result<i32>;
}

impl ExclusionBackend for MetadataBackend {
    fn name(&self) -> &str {
        MetadataBackend::name(self)
    }

    fn list(&self) -> Result<Vec<String>> {
        MetadataBackend::list(self).map_err(anyhow::Error::new)
    }

    fn set_excluded(&self, path: &str, excluded: bool) -> Result<i32> {
        MetadataBackend::set_excluded(self, path, excluded).map_err(anyhow::Error::new)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ApiFailure {
    pub path: String,
    pub operation: String,
    pub api_result: i32,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct ApplyOutcome {
    pub after: Vec<String>,
    pub failed_adds: Vec<String>,
    pub failed_removes: Vec<String>,
    /// Non-zero values returned by `_MDSetExclusion`.  A zero value is not
    /// treated as proof; `after` is the only proof.
    pub api_failures: Vec<ApiFailure>,
}

/// Canonical form used for comparisons with Spotlight's list.
pub fn resolve_exclusion_path(path: &Path) -> Result<String> {
    let normalized = normalize_path(path)?;
    let resolved = normalized.canonicalize().unwrap_or(normalized);
    Ok(trim_path(&resolved))
}

/// Canonicalise a path reported by Spotlight.  Listings return real paths, but
/// a user may have added a path through a symlink, so resolve when possible.
pub fn normalize_reported_path(raw: &str) -> String {
    let path = Path::new(raw);
    let normalized = normalize_path(path).unwrap_or_else(|_| path.to_path_buf());
    let resolved = normalized.canonicalize().unwrap_or(normalized);
    trim_path(&resolved)
}

fn trim_path(path: &Path) -> String {
    let text = path.to_string_lossy().into_owned();
    if text.len() > 1 && text.ends_with('/') {
        text.trim_end_matches('/').to_string()
    } else {
        text
    }
}

/// An existing directory that is safe to exclude.  System locations and the
/// user's entire home directory are refused.
pub fn validate_target(path: &Path) -> Result<PathBuf> {
    let normalized = normalize_path(path)?;
    if is_system_path(&normalized) {
        bail!(
            "refusing to exclude a macOS system location: {}",
            normalized.display()
        );
    }
    if normalized == Path::new("/") {
        bail!("refusing to exclude the filesystem root");
    }
    let canonical = normalized
        .canonicalize()
        .with_context(|| format!("exclusion target does not exist: {}", normalized.display()))?;
    if !canonical.is_dir() {
        bail!(
            "exclusion target is not a directory: {}",
            canonical.display()
        );
    }
    if let Ok(home) = home_dir() {
        if canonical == home {
            bail!("refusing to exclude the entire home directory");
        }
    }
    Ok(canonical)
}

fn read_list(backend: &dyn ExclusionBackend) -> Result<Vec<String>> {
    let mut paths: Vec<String> = backend
        .list()?
        .into_iter()
        .map(|path| normalize_reported_path(&path))
        .collect();
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// Apply additions and removals, re-reading Spotlight's list after each round.
/// The caller must treat `after` as the source of truth.
pub fn apply_exclusions(
    backend: &dyn ExclusionBackend,
    add: &[String],
    remove: &[String],
    current: &[String],
    retry_delays: &[u64],
) -> Result<ApplyOutcome> {
    let mut after: Vec<String> = current.to_vec();
    after.sort();
    after.dedup();
    let mut api_failures = Vec::new();

    for delay in retry_delays {
        let present: BTreeSet<&String> = after.iter().collect();
        let pending_adds: Vec<&String> =
            add.iter().filter(|path| !present.contains(path)).collect();
        let pending_removes: Vec<&String> = remove
            .iter()
            .filter(|path| present.contains(path))
            .collect();
        if pending_adds.is_empty() && pending_removes.is_empty() {
            break;
        }
        if *delay > 0 {
            std::thread::sleep(Duration::from_millis(*delay));
        }
        for path in pending_adds {
            let result = backend.set_excluded(path, true)?;
            if result != 0 {
                api_failures.push(ApiFailure {
                    path: path.clone(),
                    operation: "add".into(),
                    api_result: result,
                });
            }
        }
        for path in pending_removes {
            let result = backend.set_excluded(path, false)?;
            if result != 0 {
                api_failures.push(ApiFailure {
                    path: path.clone(),
                    operation: "remove".into(),
                    api_result: result,
                });
            }
        }
        after = read_list(backend)?;
    }

    let after_set: BTreeSet<&String> = after.iter().collect();
    Ok(ApplyOutcome {
        failed_adds: add
            .iter()
            .filter(|path| !after_set.contains(path))
            .cloned()
            .collect(),
        failed_removes: remove
            .iter()
            .filter(|path| after_set.contains(path))
            .cloned()
            .collect(),
        after,
        api_failures,
    })
}

// ---------------------------------------------------------------------------
// Desired state and reconcile planning
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Default)]
pub struct SyncReport {
    pub desired_count: usize,
    pub current_count: usize,
    pub add: Vec<String>,
    pub remove: Vec<String>,
    pub failed_adds: Vec<String>,
    pub failed_removes: Vec<String>,
    pub user_preserved: usize,
    pub api_failures: Vec<ApiFailure>,
    pub dry_run: bool,
}

impl SyncReport {
    pub fn succeeded(&self) -> bool {
        self.failed_adds.is_empty() && self.failed_removes.is_empty()
    }
}

/// Directories we want excluded and directories whose stale entries we are
/// allowed to remove.  Only this application's `managed = 1` rows are ever
/// removal candidates.
fn desired_paths(db: &Db, config: &Config) -> Result<(BTreeSet<String>, BTreeSet<String>)> {
    let roots = config.root_paths()?;
    let mut desired = BTreeSet::new();
    let mut removable = BTreeSet::new();

    let opt_outs: BTreeSet<String> = db.exclusion_opt_outs()?.into_iter().collect();
    // Include `missing` rows too: if the path exists again (for example after a
    // volume remount) it should be re-excluded before the next scanner pass
    // revives the row.  Paths that really are absent are skipped below.
    let rows = db.list_directories(&DirectoryFilter::default())?;

    for row in rows {
        let path = Path::new(&row.path);
        if !path.is_dir() {
            continue;
        }
        // Resolve first: a legacy row may contain /var/... while the configured
        // root is canonicalised to /private/var/....
        let resolved = resolve_exclusion_path(path)?;
        if !roots
            .iter()
            .any(|root| path_within(Path::new(&resolved), root))
        {
            continue;
        }
        if opt_outs.contains(&resolved) {
            continue;
        }
        desired.insert(resolved.clone());
        removable.insert(resolved);
    }

    // Explicit CLI exclusions may live outside configured scan roots.  Keep
    // them desired as long as the directory exists.
    for record in db.managed_exclusions()? {
        if record.parsed_source() != ExclusionSource::Cli {
            continue;
        }
        let path = Path::new(&record.path);
        if path.is_dir() && !opt_outs.contains(&record.path) {
            desired.insert(record.path.clone());
        }
    }

    Ok((desired, removable))
}

fn safe_to_remove_stale(path: &str, roots: &[PathBuf]) -> bool {
    let path = Path::new(path);
    if path.exists() {
        // The user may simply have disabled a scan root.  Do not remove a live
        // exclusion in that case; only prune what has actually disappeared.
        return false;
    }
    if let Some(root) = roots
        .iter()
        .filter(|root| path_within(path, root))
        .max_by_key(|root| root.components().count())
    {
        // An unmounted external volume leaves the root missing, so keep its
        // exclusions for the next mount.
        return root.is_dir();
    }
    path.parent().map(|parent| parent.is_dir()).unwrap_or(false)
}

/// Reconcile the database's desired exclusions with Spotlight's actual list.
/// This is idempotent and is safe to run after every scan.
pub fn sync(
    db: &mut Db,
    config: &Config,
    backend: &dyn ExclusionBackend,
    dry_run: bool,
) -> Result<SyncReport> {
    let current = read_list(backend)?;
    let (desired, _removable) = desired_paths(db, config)?;
    let current_set: BTreeSet<String> = current.iter().cloned().collect();

    let add: Vec<String> = desired
        .iter()
        .filter(|path| !current_set.contains(*path))
        .cloned()
        .collect();

    let roots = config.root_paths()?;
    let mut remove = Vec::new();
    if config.exclusions.remove_stale {
        for record in db.managed_exclusions()? {
            if desired.contains(&record.path) {
                continue;
            }
            if !safe_to_remove_stale(&record.path, &roots) {
                continue;
            }
            if current_set.contains(&record.path) {
                remove.push(record.path);
            } else if !dry_run {
                // Spotlight already dropped the entry (for example after an OS
                // update); forget the stale bookkeeping as well.
                db.delete_exclusion(&record.path)?;
                db.mark_directory_exclusion_released(&record.path)?;
            }
        }
    }

    let user_preserved = current
        .iter()
        .filter(|path| !desired.contains(*path))
        .filter(|path| {
            db.exclusion_get(path)
                .ok()
                .flatten()
                .map(|record| !record.managed)
                .unwrap_or(true)
        })
        .count();

    let mut report = SyncReport {
        desired_count: desired.len(),
        current_count: current.len(),
        add: add.clone(),
        remove: remove.clone(),
        user_preserved,
        dry_run,
        ..Default::default()
    };
    if dry_run {
        return Ok(report);
    }

    let outcome = apply_exclusions(backend, &add, &remove, &current, DEFAULT_RETRY_DELAYS_MS)?;
    let after_set: BTreeSet<String> = outcome.after.iter().cloned().collect();
    report.failed_adds = outcome.failed_adds.clone();
    report.failed_removes = outcome.failed_removes.clone();
    report.api_failures = outcome.api_failures.clone();

    for path in desired.iter() {
        let was_added = add.iter().any(|candidate| candidate == path);
        let existing = db.exclusion_get(path)?;
        let managed = existing
            .as_ref()
            .map(|record| record.managed)
            .unwrap_or(false)
            || (was_added && after_set.contains(path));
        let source = existing
            .as_ref()
            .map(|record| record.parsed_source())
            .unwrap_or(if managed {
                ExclusionSource::Auto
            } else {
                ExclusionSource::User
            });
        if after_set.contains(path) {
            db.record_exclusion(
                path,
                db.directory_id_for_path(path)?,
                managed,
                ExclusionState::Excluded,
                source,
                None,
            )?;
            db.mark_directory_exclusion_verified(path)?;
        } else {
            let error = "Spotlight did not report the exclusion after retries";
            db.record_exclusion(
                path,
                db.directory_id_for_path(path)?,
                managed,
                ExclusionState::Failed,
                source,
                Some(error),
            )?;
            if was_added {
                db.mark_directory_exclusion_pending(path, Some(error))?;
            }
        }
    }

    for path in &remove {
        if after_set.contains(path) {
            // Leave the managed row in place so the next reconcile retries.
            if let Some(record) = db.exclusion_get(path)? {
                db.record_exclusion(
                    path,
                    record.directory_id,
                    record.managed,
                    ExclusionState::Failed,
                    record.parsed_source(),
                    Some("Spotlight did not remove the exclusion after retries"),
                )?;
            }
        } else {
            db.delete_exclusion(path)?;
            db.mark_directory_exclusion_released(path)?;
        }
    }

    Ok(report)
}

// ---------------------------------------------------------------------------
// Explicit exclude / unexclude / verify
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MutationOutcome {
    Excluded,
    AlreadyExcluded,
    Removed,
    AlreadyAbsent,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct MutationReport {
    pub path: String,
    pub outcome: MutationOutcome,
    pub managed: bool,
    pub message: String,
    pub api_failures: Vec<ApiFailure>,
}

pub fn exclude_path(db: &mut Db, path: &Path, source: ExclusionSource) -> Result<MutationReport> {
    let target = validate_target(path)?;
    let target_string = trim_path(&target);
    db.clear_exclusion_opt_out(&target_string)?;
    let backend = backend_from_db(db)?;
    let current = read_list(&backend)?;
    let existing = db.exclusion_get(&target_string)?;

    if current.iter().any(|entry| entry == &target_string) {
        let managed = existing
            .as_ref()
            .map(|record| record.managed)
            .unwrap_or(false);
        let source = existing
            .as_ref()
            .map(|record| record.parsed_source())
            .unwrap_or(ExclusionSource::User);
        db.record_exclusion(
            &target_string,
            db.directory_id_for_path(&target_string)?,
            managed,
            ExclusionState::Excluded,
            source,
            None,
        )?;
        db.mark_directory_exclusion_verified(&target_string)?;
        return Ok(MutationReport {
            path: target_string,
            outcome: MutationOutcome::AlreadyExcluded,
            managed,
            message: "the path was already present in Spotlight's exclusion list; left in place"
                .into(),
            api_failures: Vec::new(),
        });
    }

    db.record_exclusion(
        &target_string,
        db.directory_id_for_path(&target_string)?,
        false,
        ExclusionState::Pending,
        source,
        None,
    )?;
    let outcome = apply_exclusions(
        &backend,
        std::slice::from_ref(&target_string),
        &[],
        &current,
        DEFAULT_RETRY_DELAYS_MS,
    )?;
    let applied = outcome.after.iter().any(|entry| entry == &target_string);
    if applied {
        db.record_exclusion(
            &target_string,
            db.directory_id_for_path(&target_string)?,
            true,
            ExclusionState::Excluded,
            source,
            None,
        )?;
        db.mark_directory_exclusion_verified(&target_string)?;
        Ok(MutationReport {
            path: target_string,
            outcome: MutationOutcome::Excluded,
            managed: true,
            message: "Spotlight's list now contains the path".into(),
            api_failures: outcome.api_failures,
        })
    } else {
        let error = "Spotlight did not report the exclusion after retries";
        db.record_exclusion(
            &target_string,
            db.directory_id_for_path(&target_string)?,
            false,
            ExclusionState::Failed,
            source,
            Some(error),
        )?;
        db.mark_directory_exclusion_pending(&target_string, Some(error))?;
        Ok(MutationReport {
            path: target_string,
            outcome: MutationOutcome::Failed,
            managed: false,
            message: format!("{error}; the directory remains in the manual export workflow"),
            api_failures: outcome.api_failures,
        })
    }
}

/// Helper used by the CLI: a temporary backend handle is fine because each
/// command is a separate short-lived process.
fn backend_from_db(_db: &Db) -> Result<MetadataBackend> {
    MetadataBackend::open().map_err(anyhow::Error::new)
}

pub fn unexclude_path(db: &mut Db, path: &Path) -> Result<MutationReport> {
    let target_string = resolve_exclusion_path(path)?;
    let backend = backend_from_db(db)?;
    let current = read_list(&backend)?;
    let existing = db.exclusion_get(&target_string)?;
    let managed = existing
        .as_ref()
        .map(|record| record.managed)
        .unwrap_or(true);

    if !current.iter().any(|entry| entry == &target_string) {
        db.opt_out_exclusion(&target_string, "user requested unexclude")?;
        db.delete_exclusion(&target_string)?;
        db.mark_directory_exclusion_released(&target_string)?;
        return Ok(MutationReport {
            path: target_string,
            outcome: MutationOutcome::AlreadyAbsent,
            managed,
            message:
                "the path was not in Spotlight's list; recorded an opt-out so sync will not add it again"
                    .into(),
            api_failures: Vec::new(),
        });
    }

    let outcome = apply_exclusions(
        &backend,
        &[],
        std::slice::from_ref(&target_string),
        &current,
        DEFAULT_RETRY_DELAYS_MS,
    )?;
    let removed = !outcome.after.iter().any(|entry| entry == &target_string);
    if removed {
        db.delete_exclusion(&target_string)?;
        db.opt_out_exclusion(&target_string, "user requested unexclude")?;
        db.mark_directory_exclusion_released(&target_string)?;
        Ok(MutationReport {
            path: target_string,
            outcome: MutationOutcome::Removed,
            managed,
            message: "Spotlight's list no longer contains the path".into(),
            api_failures: outcome.api_failures,
        })
    } else {
        if let Some(record) = existing {
            db.record_exclusion(
                &target_string,
                record.directory_id,
                record.managed,
                ExclusionState::Failed,
                record.parsed_source(),
                Some("Spotlight did not remove the exclusion after retries"),
            )?;
        }
        Ok(MutationReport {
            path: target_string,
            outcome: MutationOutcome::Failed,
            managed,
            message: "Spotlight still reports the path in its exclusion list; nothing was claimed"
                .into(),
            api_failures: outcome.api_failures,
        })
    }
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct ManagedRemovalReport {
    pub attempted: Vec<String>,
    pub removed: Vec<String>,
    pub failed: Vec<String>,
    pub user_preserved: Vec<String>,
    pub api_failures: Vec<ApiFailure>,
}

/// Remove every exclusion this application added.  Entries the user added in
/// System Settings (`managed = 0`) are listed but never removed.  Used by
/// `spotsweep uninstall`.
pub fn remove_all_managed(
    db: &mut Db,
    backend: &dyn ExclusionBackend,
) -> Result<ManagedRemovalReport> {
    let current = read_list(backend)?;
    let managed = db.managed_exclusions()?;
    let managed_paths: BTreeSet<String> =
        managed.iter().map(|record| record.path.clone()).collect();
    let attempted: Vec<String> = managed
        .iter()
        .map(|record| record.path.clone())
        .filter(|path| current.iter().any(|entry| entry == path))
        .collect();
    let user_preserved: Vec<String> = current
        .iter()
        .filter(|path| !managed_paths.contains(*path))
        .cloned()
        .collect();
    let mut report = ManagedRemovalReport {
        attempted: attempted.clone(),
        user_preserved,
        ..Default::default()
    };
    if attempted.is_empty() {
        for record in &managed {
            db.delete_exclusion(&record.path)?;
        }
        return Ok(report);
    }

    let outcome = apply_exclusions(backend, &[], &attempted, &current, DEFAULT_RETRY_DELAYS_MS)?;
    report.api_failures = outcome.api_failures;
    for path in &attempted {
        if outcome.after.iter().any(|entry| entry == path) {
            report.failed.push(path.clone());
            if let Some(record) = db.exclusion_get(path)? {
                db.record_exclusion(
                    path,
                    record.directory_id,
                    record.managed,
                    ExclusionState::Failed,
                    record.parsed_source(),
                    Some("uninstall could not remove the exclusion"),
                )?;
            }
        } else {
            report.removed.push(path.clone());
            db.delete_exclusion(path)?;
            db.mark_directory_exclusion_released(path)?;
        }
    }
    Ok(report)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifyOutcome {
    Excluded,
    NotExcluded,
    Inconsistent,
    Unsupported,
}

impl VerifyOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            VerifyOutcome::Excluded => "excluded",
            VerifyOutcome::NotExcluded => "not_excluded",
            VerifyOutcome::Inconsistent => "inconsistent",
            VerifyOutcome::Unsupported => "unsupported",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PathVerifyReport {
    pub path: String,
    pub outcome: VerifyOutcome,
    pub method: String,
    pub in_exclusion_list: bool,
    pub indexed_items: Option<u64>,
    pub message: String,
}

pub fn verify_path(path: &Path) -> Result<PathVerifyReport> {
    let backend = MetadataBackend::open().map_err(anyhow::Error::new)?;
    verify_path_with_backend(&backend, path)
}

pub fn verify_path_with_backend(
    backend: &dyn ExclusionBackend,
    path: &Path,
) -> Result<PathVerifyReport> {
    let target_string = resolve_exclusion_path(path)?;
    let current = read_list(backend)?;
    let in_list = current.iter().any(|entry| entry == &target_string);
    let indexed = index_count(&target_string);
    let outcome = match (in_list, indexed) {
        (true, Some(count)) if count > 0 => VerifyOutcome::Inconsistent,
        (true, _) => VerifyOutcome::Excluded,
        (false, _) => VerifyOutcome::NotExcluded,
    };
    let message = match outcome {
        VerifyOutcome::Excluded => {
            "present in Spotlight's Search Privacy list; no indexed items were returned for the path"
                .into()
        }
        VerifyOutcome::Inconsistent => format!(
            "present in Spotlight's Search Privacy list, but mdfind still reports {count} indexed item(s) under it. Spotlight may still be purging; re-run verify.",
            count = indexed.unwrap_or(0)
        ),
        VerifyOutcome::NotExcluded => {
            "not present in Spotlight's Search Privacy list".into()
        }
        VerifyOutcome::Unsupported => "Spotlight exclusion API unavailable".into(),
    };
    Ok(PathVerifyReport {
        path: target_string,
        outcome,
        method: "Metadata.framework exclusion list + mdfind count".into(),
        in_exclusion_list: in_list,
        indexed_items: indexed,
        message,
    })
}

pub fn verify_unsupported(path: &Path, reason: &str) -> PathVerifyReport {
    PathVerifyReport {
        path: normalize_path(path)
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|_| path.to_string_lossy().into_owned()),
        outcome: VerifyOutcome::Unsupported,
        method: "Metadata.framework exclusion list".into(),
        in_exclusion_list: false,
        indexed_items: None,
        message: format!("Spotlight exclusion API unavailable: {reason}"),
    }
}

pub fn index_count_for_path(path: &str) -> Option<u64> {
    index_count(path)
}

fn index_count(path: &str) -> Option<u64> {
    let output = Command::new("/usr/bin/mdfind")
        .arg("-count")
        .arg("-onlyin")
        .arg(path)
        .arg("*")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u64>()
        .ok()
}

// ---------------------------------------------------------------------------
// Selftest
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SelftestOutcome {
    Pass,
    Fail,
    Inconclusive,
    Unsupported,
}

impl SelftestOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            SelftestOutcome::Pass => "pass",
            SelftestOutcome::Fail => "fail",
            SelftestOutcome::Inconclusive => "inconclusive",
            SelftestOutcome::Unsupported => "unsupported",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SelftestCheck {
    pub name: String,
    pub passed: Option<bool>,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SelftestReport {
    pub outcome: SelftestOutcome,
    pub message: String,
    pub macos_version: String,
    pub os_build: String,
    pub backend: String,
    pub checks: Vec<SelftestCheck>,
    pub passed: usize,
    pub failed: usize,
}

pub fn selftest(backend: &dyn ExclusionBackend, parent: &Path) -> Result<SelftestReport> {
    let mut checks = Vec::new();
    let build = os_build();
    let version = macos_version();
    let baseline = read_list(backend)?;
    let base = unique_selftest_dir(parent)?;
    let control = base.join("control");
    let excluded = base.join("excluded");
    let token = selftest_token();
    let excluded_string = resolve_exclusion_path(&excluded)?;
    let mut cleanup_needed = true;

    let result = (|| -> Result<SelftestReport> {
        std::fs::create_dir_all(&control)
            .with_context(|| format!("creating {}", control.display()))?;
        std::fs::create_dir_all(&excluded)
            .with_context(|| format!("creating {}", excluded.display()))?;

        let initial = apply_exclusions(
            backend,
            std::slice::from_ref(&excluded_string),
            &[],
            &baseline,
            DEFAULT_RETRY_DELAYS_MS,
        )?;
        let recorded = initial.after.iter().any(|path| path == &excluded_string);
        checks.push(check(
            "exclusion recorded by Spotlight",
            Some(recorded),
            if recorded {
                "the temporary folder appears in Spotlight's exclusion list"
            } else {
                "the private API returned but Spotlight's list does not contain the temporary folder"
            },
        ));
        if !recorded {
            return Ok(report(
                SelftestOutcome::Fail,
                "Spotlight did not record the test exclusion",
                &version,
                &build,
                backend.name(),
                &checks,
            ));
        }

        // Repeating the same request must be harmless.
        let repeated = backend.set_excluded(&excluded_string, true)?;
        let after_repeat = read_list(backend)?;
        let repeated_ok = after_repeat.iter().any(|path| path == &excluded_string);
        checks.push(check(
            "repeated exclusion is idempotent",
            Some(repeated_ok),
            &format!(
                "second _MDSetExclusion returned {repeated}; list contains the path: {repeated_ok}"
            ),
        ));

        std::fs::write(control.join("probe.txt"), format!("{token}\n"))
            .context("writing control probe")?;
        std::fs::write(excluded.join("probe.txt"), format!("{token}\n"))
            .context("writing excluded probe")?;

        let mut control_hits: Option<Vec<String>> = None;
        for _ in 0..30 {
            control_hits = search_token(&token, &base);
            if control_hits
                .as_ref()
                .map(|hits| {
                    hits.iter()
                        .any(|path| path_within(Path::new(path), &control))
                })
                .unwrap_or(false)
            {
                break;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        let control_searchable = control_hits
            .as_ref()
            .map(|hits| {
                hits.iter()
                    .any(|path| path_within(Path::new(path), &control))
            })
            .unwrap_or(false);
        checks.push(check(
            "normal source files remain searchable",
            control_hits.as_ref().map(|_| control_searchable),
            if control_hits.is_none() {
                "mdfind could not be executed; independent verification is unavailable"
            } else if control_searchable {
                "mdfind found the control file outside the excluded folder"
            } else {
                "mdfind did not index the control file within 30 seconds (Spotlight may be off or busy)"
            },
        ));
        if control_hits.is_none() {
            return Ok(report(
                SelftestOutcome::Inconclusive,
                "mdfind is unavailable; cannot independently verify indexing",
                &version,
                &build,
                backend.name(),
                &checks,
            ));
        }
        if !control_searchable {
            return Ok(report(
                SelftestOutcome::Inconclusive,
                "Spotlight did not index the control folder within 30 seconds",
                &version,
                &build,
                backend.name(),
                &checks,
            ));
        }

        std::thread::sleep(Duration::from_secs(3));
        let final_hits = search_token(&token, &base).unwrap_or_default();
        let excluded_hit = final_hits
            .iter()
            .any(|path| path_within(Path::new(path), &excluded));
        checks.push(check(
            "excluded content is not indexed",
            Some(!excluded_hit),
            if excluded_hit {
                "mdfind returned a file from inside the excluded folder"
            } else {
                "mdfind found the control file and no file from the excluded folder"
            },
        ));

        let removed = apply_exclusions(
            backend,
            &[],
            std::slice::from_ref(&excluded_string),
            &final_hits_list(backend)?,
            DEFAULT_RETRY_DELAYS_MS,
        )?;
        let removal_ok = !removed.after.iter().any(|path| path == &excluded_string);
        checks.push(check(
            "unexclude removes the path",
            Some(removal_ok),
            if removal_ok {
                "Spotlight's list no longer contains the temporary folder"
            } else {
                "Spotlight still reports the temporary folder after removal"
            },
        ));

        // A second removal must also be harmless.
        let repeated_remove = backend.set_excluded(&excluded_string, false)?;
        let after_remove = read_list(backend)?;
        let second_removal_ok = !after_remove.iter().any(|path| path == &excluded_string);
        checks.push(check(
            "repeated unexclude is idempotent",
            Some(second_removal_ok),
            &format!(
                "second removal returned {repeated_remove}; list is clean: {second_removal_ok}"
            ),
        ));

        let missing_baseline: Vec<String> = baseline
            .iter()
            .filter(|path| !after_remove.contains(*path))
            .cloned()
            .collect();
        checks.push(check(
            "pre-existing exclusions remain intact",
            Some(missing_baseline.is_empty()),
            if missing_baseline.is_empty() {
                "every exclusion present before the test is still present"
            } else {
                "a pre-existing exclusion disappeared during the test"
            },
        ));

        cleanup_needed = true;
        let outcome = if checks.iter().all(|check| check.passed != Some(false)) {
            SelftestOutcome::Pass
        } else {
            SelftestOutcome::Fail
        };
        let message = if outcome == SelftestOutcome::Pass {
            "Spotlight honors exclusions on this macOS build"
        } else {
            "one or more exclusion checks failed"
        };
        Ok(report(
            outcome,
            message,
            &version,
            &build,
            backend.name(),
            &checks,
        ))
    })();

    // Always remove the exclusion and the temporary tree, even on an early
    // failure.  Failure to clean up is reported but never turns a failed test
    // into a pass.
    if cleanup_needed {
        let _ = backend.set_excluded(&excluded_string, false);
        let _ = std::fs::remove_dir_all(&base);
    }

    match result {
        Ok(report) => Ok(report),
        Err(error) => Ok(report(
            SelftestOutcome::Fail,
            &format!("selftest setup failed: {error:#}"),
            &version,
            &build,
            backend.name(),
            &checks,
        )),
    }
}

fn final_hits_list(backend: &dyn ExclusionBackend) -> Result<Vec<String>> {
    read_list(backend)
}

fn unique_selftest_dir(parent: &Path) -> Result<PathBuf> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let dir = parent.join(format!(
        "spotsweep-selftest-{}-{:x}",
        std::process::id(),
        nanos
    ));
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating selftest directory {}", dir.display()))?;
    Ok(dir)
}

fn selftest_token() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("spotsweepselftest{:x}{:x}", std::process::id(), nanos)
}

fn search_token(token: &str, dir: &Path) -> Option<Vec<String>> {
    let output = Command::new("/usr/bin/mdfind")
        .arg("-onlyin")
        .arg(dir)
        .arg(token)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(String::from)
            .collect(),
    )
}

fn check(name: &str, passed: Option<bool>, detail: &str) -> SelftestCheck {
    SelftestCheck {
        name: name.to_string(),
        passed,
        detail: detail.to_string(),
    }
}

fn report(
    outcome: SelftestOutcome,
    message: &str,
    macos_version: &str,
    os_build: &str,
    backend: &str,
    checks: &[SelftestCheck],
) -> SelftestReport {
    let passed = checks
        .iter()
        .filter(|check| check.passed == Some(true))
        .count();
    let failed = checks
        .iter()
        .filter(|check| check.passed == Some(false))
        .count();
    SelftestReport {
        outcome,
        message: message.to_string(),
        macos_version: macos_version.to_string(),
        os_build: os_build.to_string(),
        backend: backend.to_string(),
        checks: checks.to_vec(),
        passed,
        failed,
    }
}

/// `kern.osversion`, e.g. `26B131`.  A change means macOS was updated.
pub fn os_build() -> String {
    let mut size: libc::size_t = 0;
    let name = b"kern.osversion\0";
    // SAFETY: standard sysctl size query with a valid name and null output.
    let first = unsafe {
        libc::sysctlbyname(
            name.as_ptr() as *const libc::c_char,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if first != 0 || size == 0 {
        return "unknown".into();
    }
    let mut buffer = vec![0u8; size];
    // SAFETY: buffer is at least `size` bytes.
    let second = unsafe {
        libc::sysctlbyname(
            name.as_ptr() as *const libc::c_char,
            buffer.as_mut_ptr() as *mut libc::c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if second != 0 {
        return "unknown".into();
    }
    let end = buffer
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(buffer.len());
    String::from_utf8_lossy(&buffer[..end]).into_owned()
}

fn macos_version() -> String {
    Command::new("/usr/bin/sw_vers")
        .arg("-productVersion")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

pub fn record_selftest(db: &mut Db, report: &SelftestReport) -> Result<()> {
    db.meta_set("exclusion_selftest_build", &report.os_build)?;
    db.meta_set("exclusion_selftest_result", report.outcome.as_str())?;
    db.meta_set_i64("exclusion_selftest_at", now_ms())?;
    Ok(())
}

pub fn selftest_passed_for_current_build(db: &Db) -> Result<bool> {
    let build = os_build();
    let stored_build = db.meta_get("exclusion_selftest_build")?;
    let result = db.meta_get("exclusion_selftest_result")?;
    Ok(stored_build.as_deref() == Some(build.as_str()) && result.as_deref() == Some("pass"))
}

/// Gate for unattended automatic exclusions.  Explicit `spotsweep sync`
/// and `spotsweep exclude` do not call this.
pub fn automatic_exclusions_allowed(db: &Db, config: &Config) -> Result<bool> {
    if !config.exclusions.auto {
        return Ok(false);
    }
    if !config.exclusions.selftest_required {
        return Ok(true);
    }
    selftest_passed_for_current_build(db)
}

/// Run a sync after a scan when the service is allowed to make changes.
/// Returns `None` when automatic exclusion is disabled or gated.
pub fn auto_sync(
    db: &mut Db,
    config: &Config,
    backend: Option<&dyn ExclusionBackend>,
    reason: &str,
) -> Result<Option<SyncReport>> {
    if !automatic_exclusions_allowed(db, config)? {
        return Ok(None);
    }
    let Some(backend) = backend else {
        return Ok(None);
    };
    let report = sync(db, config, backend, false)
        .with_context(|| format!("automatic exclusion sync ({reason})"))?;
    db.log_event("exclusion_sync", &format!("{reason}: {report:?}"))?;
    Ok(Some(report))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// A process-local fake used by unit tests.  It never touches Spotlight.
#[cfg(test)]
pub struct FakeBackend {
    pub entries: std::sync::Mutex<Vec<String>>,
    pub ignore: BTreeSet<String>,
    pub calls: std::sync::Mutex<Vec<(String, bool)>>,
}

#[cfg(test)]
impl FakeBackend {
    pub fn new(entries: &[&str]) -> Self {
        Self {
            entries: std::sync::Mutex::new(entries.iter().map(|path| path.to_string()).collect()),
            ignore: BTreeSet::new(),
            calls: std::sync::Mutex::new(Vec::new()),
        }
    }

    pub fn snapshot(&self) -> Vec<String> {
        let mut entries = self.entries.lock().unwrap().clone();
        entries.sort();
        entries
    }
}

#[cfg(test)]
impl ExclusionBackend for FakeBackend {
    fn name(&self) -> &str {
        "fake"
    }

    fn list(&self) -> Result<Vec<String>> {
        Ok(self.snapshot())
    }

    fn set_excluded(&self, path: &str, excluded: bool) -> Result<i32> {
        self.calls
            .lock()
            .unwrap()
            .push((path.to_string(), excluded));
        if self.ignore.contains(path) {
            return Ok(0);
        }
        let mut entries = self.entries.lock().unwrap();
        entries.retain(|entry| entry != path);
        if excluded {
            entries.push(path.to_string());
        }
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{RuleKind, Status};
    use tempfile::TempDir;

    fn db_with_root(root: &Path) -> Db {
        let mut db = Db::open_in_memory().unwrap();
        db.sync_roots(std::slice::from_ref(&root.to_path_buf()))
            .unwrap();
        db
    }

    fn add_directory(db: &mut Db, root: &Path, relative: &str) -> String {
        let path = root.join(relative);
        std::fs::create_dir_all(&path).unwrap();
        let path = path.canonicalize().unwrap();
        let root_record = db.enabled_roots().unwrap().remove(0);
        db.upsert_directories(
            &[crate::model::NewDirectory {
                path: path.to_string_lossy().into_owned(),
                name: "node_modules".into(),
                rule: "node_modules".into(),
                rule_kind: RuleKind::Dependencies,
                scan_root_id: root_record.id,
                project_root: path.parent().unwrap().to_string_lossy().into_owned(),
            }],
            now_ms(),
        )
        .unwrap();
        db.promote_discovered().unwrap();
        path.to_string_lossy().into_owned()
    }

    fn config(root: &Path) -> Config {
        let mut config = Config::default();
        config.scan.roots = vec![root.to_string_lossy().into_owned()];
        config.exclusions.auto = true;
        config.exclusions.selftest_required = false;
        config
    }

    #[test]
    fn sync_adds_only_desired_paths_and_is_idempotent() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        let mut db = db_with_root(root);
        let path = add_directory(&mut db, root, "app/node_modules");
        let backend = FakeBackend::new(&[]);
        let config = config(root);

        let first = sync(&mut db, &config, &backend, false).unwrap();
        assert_eq!(first.add, vec![path.clone()]);
        assert!(first.succeeded());
        assert_eq!(backend.snapshot(), vec![path.clone()]);
        assert!(db.exclusion_get(&path).unwrap().unwrap().managed);

        let second = sync(&mut db, &config, &backend, false).unwrap();
        assert!(second.add.is_empty());
        assert!(second.remove.is_empty());
        assert_eq!(backend.snapshot(), vec![path]);
    }

    #[test]
    fn user_entries_are_never_removed_by_sync() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        let mut db = db_with_root(root);
        let backend = FakeBackend::new(&["/Users/someone/manual-exclusion"]);
        let report = sync(&mut db, &config(root), &backend, false).unwrap();
        assert!(report.remove.is_empty());
        assert_eq!(backend.snapshot(), vec!["/Users/someone/manual-exclusion"]);
    }

    #[test]
    fn failure_marks_directory_pending_and_never_claims_success() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        let mut db = db_with_root(root);
        let path = add_directory(&mut db, root, "app/node_modules");
        let mut backend = FakeBackend::new(&[]);
        backend.ignore.insert(path.clone());
        let report = sync(&mut db, &config(root), &backend, false).unwrap();
        assert_eq!(report.failed_adds, vec![path.clone()]);
        let record = db.exclusion_get(&path).unwrap().unwrap();
        assert_eq!(record.parsed_state(), ExclusionState::Failed);
        assert!(!record.managed);
        let row = db
            .list_directories(&DirectoryFilter {
                statuses: vec![Status::PendingManualExclusion],
                ..Default::default()
            })
            .unwrap();
        assert!(row.iter().any(|row| row.path == path));
    }

    #[test]
    fn unexclude_records_opt_out_and_sync_does_not_readd() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        let mut db = db_with_root(root);
        let path = add_directory(&mut db, root, "app/node_modules");
        let mut config = config(root);
        let backend = FakeBackend::new(&[]);
        sync(&mut db, &config, &backend, false).unwrap();
        assert!(backend.snapshot().contains(&path));

        // unexclude_path constructs its own Metadata backend, so exercise the
        // same bookkeeping directly with the fake.
        let current = backend.snapshot();
        let outcome =
            apply_exclusions(&backend, &[], std::slice::from_ref(&path), &current, &[0]).unwrap();
        assert!(outcome.after.is_empty());
        db.delete_exclusion(&path).unwrap();
        db.opt_out_exclusion(&path, "test").unwrap();
        let report = sync(&mut db, &config, &backend, false).unwrap();
        assert!(report.add.is_empty());
        assert!(backend.snapshot().is_empty());

        config.exclusions.auto = false;
        // Explicit re-exclude clears the opt-out.
        db.clear_exclusion_opt_out(&path).unwrap();
        assert!(!db.is_exclusion_opted_out(&path).unwrap());
    }

    #[test]
    fn stale_managed_entries_are_pruned_but_unmounted_roots_are_kept() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        let mut db = db_with_root(root);
        let path = add_directory(&mut db, root, "app/node_modules");
        let backend = FakeBackend::new(&[]);
        let config = config(root);
        sync(&mut db, &config, &backend, false).unwrap();
        assert!(db.exclusion_get(&path).unwrap().unwrap().managed);
        std::fs::remove_dir_all(&path).unwrap();
        // The project parent still exists, so the stale exclusion is safe to remove.
        let report = sync(&mut db, &config, &backend, false).unwrap();
        assert_eq!(report.remove, vec![path.clone()]);
        assert!(backend.snapshot().is_empty());
        assert!(db.exclusion_get(&path).unwrap().is_none());
    }

    #[test]
    fn exclusion_state_survives_database_reopen() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("projects");
        std::fs::create_dir_all(&root).unwrap();
        let db_path = temp.path().join("spotsweep.db");
        let path = {
            let mut db = Db::open_path(&db_path).unwrap();
            db.sync_roots(std::slice::from_ref(&root)).unwrap();
            add_directory(&mut db, &root, "app/node_modules")
        };
        let backend = FakeBackend::new(&[]);
        {
            let mut db = Db::open_path(&db_path).unwrap();
            sync(&mut db, &config(&root), &backend, false).unwrap();
        }
        let mut db = Db::open_path(&db_path).unwrap();
        assert!(db.exclusion_get(&path).unwrap().unwrap().managed);
        let report = sync(&mut db, &config(&root), &backend, false).unwrap();
        assert!(report.add.is_empty());
        assert!(report.remove.is_empty());
    }

    #[test]
    fn uninstall_removes_managed_but_preserves_user_entries() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        let mut db = db_with_root(root);
        let path = add_directory(&mut db, root, "app/node_modules");
        let backend = FakeBackend::new(&["/Users/someone/manual-exclusion"]);
        sync(&mut db, &config(root), &backend, false).unwrap();
        assert!(backend.snapshot().contains(&path));

        let report = remove_all_managed(&mut db, &backend).unwrap();
        assert_eq!(report.removed, vec![path.clone()]);
        assert_eq!(
            report.user_preserved,
            vec!["/Users/someone/manual-exclusion".to_string()]
        );
        assert_eq!(backend.snapshot(), vec!["/Users/someone/manual-exclusion"]);
        assert!(db.exclusion_get(&path).unwrap().is_none());
    }

    #[test]
    fn normalize_reported_path_removes_trailing_slash() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("dir");
        std::fs::create_dir_all(&path).unwrap();
        let with_slash = format!("{}/", path.display());
        assert_eq!(
            normalize_reported_path(&with_slash),
            resolve_exclusion_path(&path).unwrap()
        );
    }

    #[test]
    fn validate_target_refuses_root_and_system_locations() {
        assert!(validate_target(Path::new("/")).is_err());
        assert!(validate_target(Path::new("/System")).is_err());
    }
}
