//! Filesystem discovery: full/reconciliation walks and targeted event-driven
//! discovery.  The scanner never writes project files and never follows a
//! symlink outside a configured root.

use crate::config::Config;
use crate::database::Db;
use crate::matcher::RuleSet;
use crate::model::{NewDirectory, RootRecord, ScanKind, ScanSummary, TargetedSummary};
use crate::paths::{is_hidden_name, path_within};
use crate::util::now_ms;
use anyhow::Result;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use tracing::{debug, warn};
use walkdir::WalkDir;

const BATCH_SIZE: usize = 256;

fn excluded_paths(config: &Config) -> Result<Vec<PathBuf>> {
    config.excluded_paths()
}

fn find_root<'a>(roots: &'a [RootRecord], path: &Path) -> Option<&'a RootRecord> {
    roots
        .iter()
        .filter(|root| path_within(path, Path::new(&root.path)))
        .max_by_key(|root| Path::new(&root.path).components().count())
}

fn relative_depth(root: &Path, path: &Path) -> usize {
    path.strip_prefix(root)
        .map(|rest| rest.components().count())
        .unwrap_or(0)
}

fn hidden_or_vcs(name: &str, matcher: &RuleSet) -> bool {
    (!matcher.scan_dot_directories() && is_hidden_name(name)) || RuleSet::is_vcs_metadata(name)
}

/// True when a component *above* `path` (not `path` itself) is an artifact or a
/// pruned hidden directory.
fn has_pruned_ancestor(path: &Path, root: &Path, matcher: &RuleSet) -> bool {
    let mut current = path.parent();
    while let Some(parent) = current {
        if parent == root || !parent.starts_with(root) {
            break;
        }
        if let Some(name) = parent.file_name().and_then(|n| n.to_str()) {
            if hidden_or_vcs(name, matcher) || matcher.is_match(name) {
                return true;
            }
        }
        current = parent.parent();
    }
    false
}

struct WalkResult {
    visited: u64,
    matched: u64,
    errors: u64,
    new_dirs: usize,
    revived: usize,
}

/// Walk `start` and everything below it, recording matching directories.
/// `start` itself is only considered for pruning, not recorded (callers handle
/// the event/root node explicitly).
#[allow(clippy::too_many_arguments)]
fn walk_subtree(
    db: &mut Db,
    config: &Config,
    matcher: &RuleSet,
    excluded: &[PathBuf],
    root: &RootRecord,
    start: &Path,
    seen_at: i64,
    seen_matches: &mut HashSet<PathBuf>,
) -> Result<WalkResult> {
    let root_path = Path::new(&root.path);
    let canonical_root = if config.scan.follow_symlinks {
        root_path.canonicalize().ok()
    } else {
        None
    };
    let follow = config.scan.follow_symlinks;
    let scan_dot = matcher.scan_dot_directories();
    let max_depth = if config.scan.max_depth == 0 {
        usize::MAX
    } else {
        config
            .scan
            .max_depth
            .saturating_sub(relative_depth(root_path, start))
    };

    if max_depth == 0 {
        return Ok(WalkResult {
            visited: 0,
            matched: 0,
            errors: 0,
            new_dirs: 0,
            revived: 0,
        });
    }

    let walker = WalkDir::new(start)
        .follow_links(follow)
        .same_file_system(true)
        .max_depth(max_depth)
        .into_iter();

    let mut batch: Vec<NewDirectory> = Vec::with_capacity(BATCH_SIZE);
    let mut result = WalkResult {
        visited: 0,
        matched: 0,
        errors: 0,
        new_dirs: 0,
        revived: 0,
    };

    let mut iterator = walker;
    loop {
        let next = match iterator.next() {
            None => break,
            Some(Ok(entry)) => entry,
            Some(Err(error)) => {
                result.errors += 1;
                debug!(error = %error, "walk error (permissions, broken symlink, or disappeared entry)");
                continue;
            }
        };

        if next.depth() > 0 {
            let name = next.file_name().to_string_lossy();
            let path = next.path();
            let skip = (!scan_dot && is_hidden_name(&name))
                || RuleSet::is_vcs_metadata(&name)
                || excluded
                    .iter()
                    .any(|excluded| path == excluded || path.starts_with(excluded))
                || (next.path_is_symlink() && !follow)
                || (follow
                    && fs::symlink_metadata(path)
                        .map(|meta| {
                            meta.file_type().is_symlink()
                                && !matches!(
                                    (path.canonicalize(), &canonical_root),
                                    (Ok(target), Some(root)) if target.starts_with(root)
                                )
                        })
                        .unwrap_or(true));
            if skip {
                if next.file_type().is_dir() {
                    iterator.skip_current_dir();
                }
                continue;
            }
        }

        if !next.file_type().is_dir() {
            continue;
        }
        result.visited += 1;

        if next.depth() == 0 {
            continue;
        }

        let name = next.file_name().to_string_lossy().to_string();
        let Some(rule) = matcher.classify(&name) else {
            continue;
        };

        // A symlink is not a useful Spotlight exclusion target: the user sees
        // the target path in Finder.  Skip it and do not descend through it.
        if next.path_is_symlink() && !follow {
            iterator.skip_current_dir();
            continue;
        }

        let path = next.path().to_path_buf();
        if !seen_matches.insert(path.clone()) {
            iterator.skip_current_dir();
            continue;
        }
        let project_root = path
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        batch.push(NewDirectory {
            path: path.to_string_lossy().to_string(),
            name,
            rule: rule.pattern,
            rule_kind: rule.kind,
            scan_root_id: root.id,
            project_root,
        });
        result.matched += 1;
        iterator.skip_current_dir();

        if batch.len() >= BATCH_SIZE {
            let stats = db.upsert_directories(&batch, seen_at)?;
            result.new_dirs += stats.new_dirs;
            result.revived += stats.revived;
            batch.clear();
        }
    }

    if !batch.is_empty() {
        let stats = db.upsert_directories(&batch, seen_at)?;
        result.new_dirs += stats.new_dirs;
        result.revived += stats.revived;
    }

    Ok(result)
}

/// Full or reconciliation scan over all configured (or one selected) roots.
pub fn full_scan(
    db: &mut Db,
    config: &Config,
    matcher: &RuleSet,
    kind: ScanKind,
    only_root: Option<&Path>,
) -> Result<ScanSummary> {
    let started = now_ms();
    let roots = db.enabled_roots()?;
    let only_root_canonical =
        only_root.map(|path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf()));
    let selected: Vec<RootRecord> = match only_root {
        Some(path) => roots
            .iter()
            .filter(|root| {
                let root_path = Path::new(&root.path);
                root_path == path
                    || only_root_canonical
                        .as_ref()
                        .map(|canonical| root_path == canonical)
                        .unwrap_or(false)
            })
            .cloned()
            .collect(),
        None => roots.clone(),
    };
    if let Some(path) = only_root {
        if selected.is_empty() {
            anyhow::bail!(
                "{} is not an enabled scan root; add it with `spotlight-dev add-path`",
                path.display()
            );
        }
    }

    let session = db.start_scan(kind, selected.len())?;
    let excluded = excluded_paths(config)?;
    let mut summary = ScanSummary {
        kind: kind.as_str().to_string(),
        roots_configured: selected.len(),
        ..Default::default()
    };
    let mut seen_matches = HashSet::new();

    for root in &selected {
        let root_path = Path::new(&root.path);
        let metadata = match fs::metadata(root_path) {
            Ok(meta) => meta,
            Err(error) => {
                summary.roots_missing += 1;
                debug!(root = %root.path, error = %error, "scan root is missing or unreadable");
                continue;
            }
        };
        if !metadata.is_dir() {
            summary.roots_missing += 1;
            continue;
        }

        let walked = walk_subtree(
            db,
            config,
            matcher,
            &excluded,
            root,
            root_path,
            session.started_at,
            &mut seen_matches,
        );
        match walked {
            Ok(result) => {
                summary.roots_scanned += 1;
                summary.dirs_visited += result.visited;
                summary.matched += result.matched;
                summary.new_dirs += result.new_dirs as u64;
                summary.errors += result.errors;

                // Never infer "missing" from an incomplete walk.  A permission
                // error or unmounted subtree could otherwise hide every record.
                if result.errors == 0 {
                    summary.missing_dirs +=
                        db.mark_missing_for_root(&root.path, session.started_at)? as u64;
                } else {
                    warn!(
                        root = %root.path,
                        errors = result.errors,
                        "skipping missing-directory reconciliation for this root (incomplete walk)"
                    );
                }
                db.mark_root_scanned(root.id, result.errors == 0)?;
            }
            Err(error) => {
                summary.errors += 1;
                db.mark_root_scanned(root.id, false)?;
                warn!(root = %root.path, error = %error, "root scan failed");
            }
        }
    }

    db.promote_discovered()?;
    summary.duration_ms = (now_ms() - started).max(0) as u64;
    db.finish_scan(&session, &summary)?;
    db.prune_old_events(30)?;
    Ok(summary)
}

/// Apply FSEvents paths without walking the whole scan root.
pub fn targeted_discover(
    db: &mut Db,
    config: &Config,
    matcher: &RuleSet,
    events: &[PathBuf],
) -> Result<TargetedSummary> {
    let started = now_ms();
    let roots = db.enabled_roots()?;
    let excluded = excluded_paths(config)?;
    let mut summary = TargetedSummary::default();

    // Coalesce duplicates and descendants.  Walking a parent event path already
    // covers its children; this is what keeps an npm install from causing
    // repeated traversals.
    let mut unique: Vec<PathBuf> = events.to_vec();
    unique.sort_by_key(|p| (p.components().count(), p.clone()));
    unique.dedup();
    let mut coalesced: Vec<PathBuf> = Vec::new();
    for path in unique {
        if coalesced.iter().any(|kept| path_within(&path, kept)) {
            continue;
        }
        coalesced.push(path);
    }
    summary.event_paths = coalesced.len();

    let mut direct: Vec<NewDirectory> = Vec::new();
    let mut deletion_prefixes: Vec<PathBuf> = Vec::new();
    let mut seen_matches: HashSet<PathBuf> = HashSet::new();
    let seen_at = started;

    for path in &coalesced {
        let Some(root) = find_root(&roots, path) else {
            summary.ignored += 1;
            continue;
        };
        let root_path = Path::new(&root.path);

        if path == root_path {
            summary.ignored += 1;
            continue;
        }
        if excluded
            .iter()
            .any(|excluded| path == excluded || path.starts_with(excluded))
        {
            summary.ignored += 1;
            continue;
        }

        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if hidden_or_vcs(name, matcher) {
            summary.ignored += 1;
            continue;
        }
        if has_pruned_ancestor(path, root_path, matcher) {
            summary.ignored += 1;
            continue;
        }

        match fs::symlink_metadata(path) {
            Ok(meta) if meta.is_dir() => {
                if meta.file_type().is_symlink() {
                    summary.ignored += 1;
                    continue;
                }
                if let Some(rule) = matcher.classify(name) {
                    if seen_matches.insert(path.clone()) {
                        direct.push(NewDirectory {
                            path: path.to_string_lossy().to_string(),
                            name: name.to_string(),
                            rule: rule.pattern,
                            rule_kind: rule.kind,
                            scan_root_id: root.id,
                            project_root: path
                                .parent()
                                .map(|p| p.to_string_lossy().to_string())
                                .unwrap_or_default(),
                        });
                        summary.matched += 1;
                    }
                } else {
                    let walked = walk_subtree(
                        db,
                        config,
                        matcher,
                        &excluded,
                        root,
                        path,
                        seen_at,
                        &mut seen_matches,
                    )?;
                    summary.dirs_visited += walked.visited;
                    summary.matched += walked.matched;
                    summary.new_dirs += walked.new_dirs as u64;
                    summary.errors += walked.errors;
                }
            }
            Ok(_) => {
                // File events are useful only when the file's parent is a newly
                // created matching directory (event coalescing can omit the
                // directory-created event).
                if let Some(parent) = path.parent() {
                    if let Some(parent_name) = parent.file_name().and_then(|n| n.to_str()) {
                        if let Some(rule) = matcher.classify(parent_name) {
                            if let Ok(meta) = fs::symlink_metadata(parent) {
                                if meta.is_dir()
                                    && !meta.file_type().is_symlink()
                                    && seen_matches.insert(parent.to_path_buf())
                                {
                                    direct.push(NewDirectory {
                                        path: parent.to_string_lossy().to_string(),
                                        name: parent_name.to_string(),
                                        rule: rule.pattern,
                                        rule_kind: rule.kind,
                                        scan_root_id: root.id,
                                        project_root: parent
                                            .parent()
                                            .map(|p| p.to_string_lossy().to_string())
                                            .unwrap_or_default(),
                                    });
                                    summary.matched += 1;
                                }
                            }
                        }
                    }
                }
            }
            Err(_) => {
                deletion_prefixes.push(path.clone());
            }
        }
    }

    let mut new_dirs = 0;
    let mut revived = 0;
    if !direct.is_empty() {
        let stats = db.upsert_directories(&direct, seen_at)?;
        new_dirs += stats.new_dirs;
        revived += stats.revived;
    }
    summary.new_dirs += new_dirs as u64;
    if new_dirs > 0 || revived > 0 {
        db.promote_discovered()?;
    }

    for prefix in &deletion_prefixes {
        let prefix_string = prefix.to_string_lossy().to_string();
        let candidates = db.directories_under(&prefix_string)?;
        let missing: Vec<String> = candidates
            .into_iter()
            .filter(|path| fs::symlink_metadata(path).is_err())
            .collect();
        if !missing.is_empty() {
            summary.missing_dirs += db.mark_missing_exact(&missing)? as u64;
        }
    }

    summary.duration_ms = (now_ms() - started).max(0) as u64;
    if summary.matched > 0 || summary.missing_dirs > 0 || summary.errors > 0 {
        let _ = db.log_event("targeted", &format!("{summary:?}"));
    }
    Ok(summary)
}

/// Re-check `missing` rows after a volume remount or app restart.
pub fn retry_missing(db: &mut Db) -> Result<(usize, usize)> {
    let missing = db.missing_directories()?;
    let mut restored_paths = Vec::new();
    for record in &missing {
        if fs::symlink_metadata(&record.path).is_ok() {
            restored_paths.push(record.path.clone());
        }
    }
    let restored = db.restore_missing(&restored_paths)?;
    if restored > 0 {
        db.promote_discovered()?;
    }
    Ok((missing.len(), restored))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matcher::RuleSet;
    use crate::model::Status;
    use std::fs;

    fn setup() -> (tempfile::TempDir, Config) {
        let temp = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.scan.roots = vec![temp.path().to_string_lossy().to_string()];
        config.scan.reconcile_interval_hours = 6;
        (temp, config)
    }

    #[test]
    fn full_scan_finds_nested_and_deduplicates() {
        let (temp, config) = setup();
        let root = temp.path();
        fs::create_dir_all(root.join("a/node_modules/pkg")).unwrap();
        fs::create_dir_all(root.join("b/c/dist")).unwrap();
        fs::create_dir_all(root.join("b/src")).unwrap();
        fs::write(root.join("b/src/main.rs"), "fn main() {}").unwrap();

        let mut db = Db::open_in_memory().unwrap();
        db.sync_roots(&[root.to_path_buf()]).unwrap();
        let matcher = RuleSet::from_config(&config.rules).unwrap();
        let summary = full_scan(&mut db, &config, &matcher, ScanKind::Initial, None).unwrap();

        assert_eq!(summary.new_dirs, 2);
        assert_eq!(db.count_pending().unwrap(), 2);
        let rows = db.pending_directories().unwrap();
        assert!(rows.iter().any(|r| r.path.ends_with("a/node_modules")));
        assert!(rows.iter().any(|r| r.path.ends_with("b/c/dist")));
        // Contents of matched directories are never traversed or recorded.
        assert!(!rows.iter().any(|r| r.path.contains("node_modules/pkg")));
    }

    #[test]
    fn nested_scan_roots_and_overlapping_roots_do_not_duplicate() {
        let (temp, mut config) = setup();
        let root = temp.path();
        fs::create_dir_all(root.join("p/node_modules")).unwrap();
        config.scan.roots = vec![
            root.to_string_lossy().to_string(),
            root.join("p").to_string_lossy().to_string(),
        ];
        let mut db = Db::open_in_memory().unwrap();
        db.sync_roots(&config.root_paths().unwrap()).unwrap();
        let matcher = RuleSet::from_config(&config.rules).unwrap();
        full_scan(&mut db, &config, &matcher, ScanKind::Initial, None).unwrap();
        assert_eq!(db.count_all().unwrap(), 1);
    }

    #[test]
    fn top_level_build_artifact_is_not_confused_with_source() {
        let (temp, config) = setup();
        let root = temp.path();
        fs::create_dir_all(root.join("project/build/generated")).unwrap();
        fs::create_dir_all(root.join("project/src/build")).unwrap();
        fs::write(root.join("project/src/build/mod.rs"), "").unwrap();
        let mut db = Db::open_in_memory().unwrap();
        db.sync_roots(&[root.to_path_buf()]).unwrap();
        let matcher = RuleSet::from_config(&config.rules).unwrap();
        full_scan(&mut db, &config, &matcher, ScanKind::Initial, None).unwrap();
        let rows = db.pending_directories().unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|r| r.path.ends_with("project/build")));
        assert!(rows.iter().any(|r| r.path.ends_with("project/src/build")));
        assert!(
            !rows
                .iter()
                .any(|r| r.path.ends_with("project/build/generated"))
        );
    }

    #[test]
    fn dot_directories_are_skipped_by_default_and_enabled_on_request() {
        let (temp, mut config) = setup();
        let root = temp.path();
        fs::create_dir_all(root.join("project/.next/cache")).unwrap();
        let mut db = Db::open_in_memory().unwrap();
        db.sync_roots(&[root.to_path_buf()]).unwrap();
        let matcher = RuleSet::from_config(&config.rules).unwrap();
        full_scan(&mut db, &config, &matcher, ScanKind::Initial, None).unwrap();
        assert_eq!(db.count_all().unwrap(), 0);

        config.rules.scan_dot_directories = true;
        let matcher = RuleSet::from_config(&config.rules).unwrap();
        full_scan(&mut db, &config, &matcher, ScanKind::Reconcile, None).unwrap();
        let rows = db.pending_directories().unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].path.ends_with(".next"));
    }

    #[test]
    fn missing_directories_are_marked_and_revived() {
        let (temp, config) = setup();
        let root = temp.path();
        let nm = root.join("app/node_modules");
        fs::create_dir_all(&nm).unwrap();
        let mut db = Db::open_in_memory().unwrap();
        db.sync_roots(&[root.to_path_buf()]).unwrap();
        let matcher = RuleSet::from_config(&config.rules).unwrap();
        full_scan(&mut db, &config, &matcher, ScanKind::Initial, None).unwrap();
        assert_eq!(db.count_pending().unwrap(), 1);

        fs::remove_dir_all(&nm).unwrap();
        full_scan(&mut db, &config, &matcher, ScanKind::Reconcile, None).unwrap();
        assert_eq!(db.count_status(Status::Missing).unwrap(), 1);

        fs::create_dir_all(&nm).unwrap();
        full_scan(&mut db, &config, &matcher, ScanKind::Reconcile, None).unwrap();
        assert_eq!(db.count_status(Status::Missing).unwrap(), 0);
        assert_eq!(db.count_pending().unwrap(), 1);
    }

    #[test]
    fn interrupted_scan_does_not_mark_everything_missing() {
        let (temp, config) = setup();
        let root = temp.path();
        fs::create_dir_all(root.join("app/node_modules")).unwrap();
        let mut db = Db::open_in_memory().unwrap();
        db.sync_roots(&[root.to_path_buf()]).unwrap();
        let matcher = RuleSet::from_config(&config.rules).unwrap();
        full_scan(&mut db, &config, &matcher, ScanKind::Initial, None).unwrap();

        // Simulate a crashed scan row.
        let _session = db.start_scan(ScanKind::Reconcile, 1).unwrap();
        db.recover_interrupted().unwrap();
        assert_eq!(db.count_status(Status::Missing).unwrap(), 0);
    }

    #[test]
    fn targeted_discovery_finds_new_directory_without_full_walk() {
        let (temp, config) = setup();
        let root = temp.path();
        fs::create_dir_all(root.join("projects/existing")).unwrap();
        let mut db = Db::open_in_memory().unwrap();
        db.sync_roots(&[root.to_path_buf()]).unwrap();
        let matcher = RuleSet::from_config(&config.rules).unwrap();
        full_scan(&mut db, &config, &matcher, ScanKind::Initial, None).unwrap();

        let new_nm = root.join("projects/existing/node_modules");
        fs::create_dir_all(&new_nm).unwrap();
        let summary =
            targeted_discover(&mut db, &config, &matcher, std::slice::from_ref(&new_nm)).unwrap();
        assert_eq!(summary.new_dirs, 1);
        assert!(
            summary.dirs_visited < 5,
            "targeted discovery should not walk the whole root"
        );
        assert_eq!(db.count_pending().unwrap(), 1);
    }

    #[test]
    fn targeted_discovery_handles_moves_and_deletions() {
        let (temp, config) = setup();
        let root = temp.path();
        let old = root.join("projects/old/node_modules");
        fs::create_dir_all(&old).unwrap();
        let mut db = Db::open_in_memory().unwrap();
        db.sync_roots(&[root.to_path_buf()]).unwrap();
        let matcher = RuleSet::from_config(&config.rules).unwrap();
        full_scan(&mut db, &config, &matcher, ScanKind::Initial, None).unwrap();

        let new = root.join("projects/new/node_modules");
        fs::create_dir_all(new.parent().unwrap()).unwrap();
        fs::rename(&old, &new).unwrap();
        targeted_discover(&mut db, &config, &matcher, &[old.clone(), new.clone()]).unwrap();

        let missing = db.missing_directories().unwrap();
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].path, old.to_string_lossy());
        let pending = db.pending_directories().unwrap();
        assert!(pending.iter().any(|r| r.path == new.to_string_lossy()));
    }

    #[test]
    fn targeted_discovery_marks_deleted_artifact_missing() {
        let (temp, config) = setup();
        let root = temp.path();
        let nm = root.join("app/node_modules");
        fs::create_dir_all(&nm).unwrap();
        let mut db = Db::open_in_memory().unwrap();
        db.sync_roots(&[root.to_path_buf()]).unwrap();
        let matcher = RuleSet::from_config(&config.rules).unwrap();
        full_scan(&mut db, &config, &matcher, ScanKind::Initial, None).unwrap();
        fs::remove_dir_all(&nm).unwrap();
        targeted_discover(&mut db, &config, &matcher, &[nm]).unwrap();
        assert_eq!(db.count_status(Status::Missing).unwrap(), 1);
    }

    #[test]
    fn targeted_discovery_does_not_walk_inside_artifact_directories() {
        let (temp, config) = setup();
        let root = temp.path();
        let nm = root.join("app/node_modules");
        fs::create_dir_all(nm.join("pkg/build")).unwrap();
        let mut db = Db::open_in_memory().unwrap();
        db.sync_roots(&[root.to_path_buf()]).unwrap();
        let matcher = RuleSet::from_config(&config.rules).unwrap();
        full_scan(&mut db, &config, &matcher, ScanKind::Initial, None).unwrap();
        let summary =
            targeted_discover(&mut db, &config, &matcher, &[nm.join("pkg/build/out")]).unwrap();
        assert_eq!(summary.matched, 0);
        assert_eq!(summary.ignored, 1);
        assert_eq!(db.count_all().unwrap(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_outside_roots_are_never_followed() {
        use std::os::unix::fs::symlink;
        let (temp, config) = setup();
        let root = temp.path().join("root");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(outside.join("node_modules")).unwrap();
        fs::create_dir_all(root.join("app")).unwrap();
        symlink(&outside, root.join("linked")).unwrap();
        symlink(outside.join("node_modules"), root.join("app/node_modules")).unwrap();

        let mut db = Db::open_in_memory().unwrap();
        db.sync_roots(std::slice::from_ref(&root)).unwrap();
        let matcher = RuleSet::from_config(&config.rules).unwrap();
        full_scan(&mut db, &config, &matcher, ScanKind::Initial, None).unwrap();
        assert_eq!(db.count_all().unwrap(), 0);

        // With following enabled, the target of the first symlink is still
        // outside the configured root, so it is pruned; the second symlink is
        // skipped as an exclusion target.
        let mut follow = config.clone();
        follow.scan.follow_symlinks = true;
        let matcher = RuleSet::from_config(&follow.rules).unwrap();
        full_scan(&mut db, &follow, &matcher, ScanKind::Reconcile, None).unwrap();
        assert_eq!(db.count_all().unwrap(), 0);
    }

    #[test]
    fn excluded_paths_are_pruned() {
        let (temp, mut config) = setup();
        let root = temp.path();
        fs::create_dir_all(root.join("keep/node_modules")).unwrap();
        fs::create_dir_all(root.join("skip/node_modules")).unwrap();
        config.scan.exclude_paths = vec![root.join("skip").to_string_lossy().to_string()];
        let mut db = Db::open_in_memory().unwrap();
        db.sync_roots(&[root.to_path_buf()]).unwrap();
        let matcher = RuleSet::from_config(&config.rules).unwrap();
        full_scan(&mut db, &config, &matcher, ScanKind::Initial, None).unwrap();
        let rows = db.pending_directories().unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].path.contains("keep"));
    }

    #[test]
    fn max_depth_limits_traversal() {
        let (temp, mut config) = setup();
        let root = temp.path();
        fs::create_dir_all(root.join("a/b/c/node_modules")).unwrap();
        config.scan.max_depth = 2;
        let mut db = Db::open_in_memory().unwrap();
        db.sync_roots(&[root.to_path_buf()]).unwrap();
        let matcher = RuleSet::from_config(&config.rules).unwrap();
        full_scan(&mut db, &config, &matcher, ScanKind::Initial, None).unwrap();
        assert_eq!(db.count_all().unwrap(), 0);
        config.scan.max_depth = 0;
        full_scan(&mut db, &config, &matcher, ScanKind::Reconcile, None).unwrap();
        assert_eq!(db.count_all().unwrap(), 1);
    }
}
