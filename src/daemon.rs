//! Background service: native FSEvents + debounced targeted discovery + a
//! periodic reconciliation safety net.  Idle state is a blocking channel wait;
//! there is no polling loop.

use crate::config::Config;
use crate::database::Db;
use crate::exclusion::{self, ExclusionBackend, SelftestOutcome};
use crate::matcher::RuleSet;
use crate::metadata::MetadataBackend;
use crate::model::ScanKind;
use crate::paths::{AppPaths, home_dir, is_hidden_name, is_root};
use crate::scanner;
use crate::util::now_ms;
use crate::watcher::{self, RawEvent, StreamHandle};
use anyhow::{Context, Result, bail};
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use signal_hook::iterator::Signals;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

enum DaemonMsg {
    Fs(RawEvent),
    Reload,
    Shutdown,
}

pub fn run(paths: AppPaths) -> Result<()> {
    if is_root() {
        bail!("the background service must run as your user, not root");
    }

    let mut config = Config::load_or_init(&paths)?;
    let _guard = crate::logging::init_daemon(&paths.logs_dir, &config.daemon.log_level)?;

    info!(
        version = env!("CARGO_PKG_VERSION"),
        pid = std::process::id(),
        data_dir = %paths.data_dir.display(),
        "spotlight-dev daemon starting"
    );

    let mut db = Db::open(&paths)?;
    let recovered = db.recover_interrupted()?;
    if recovered > 0 {
        warn!(
            count = recovered,
            "recovered interrupted scans from a previous process"
        );
    }
    sync_config(&mut db, &config)?;
    db.log_event("daemon_start", &format!("pid={}", std::process::id()))?;

    let mut exclusion_backend: Option<MetadataBackend> = None;
    if config.exclusions.auto {
        match MetadataBackend::open() {
            Ok(backend) => {
                exclusion_backend = Some(backend);
                if config.exclusions.run_selftest_on_os_change {
                    let build = exclusion::os_build();
                    let already_passed = db.meta_get("exclusion_selftest_build")?.as_deref()
                        == Some(build.as_str())
                        && db.meta_get("exclusion_selftest_result")?.as_deref() == Some("pass");
                    if !already_passed {
                        info!(build = %build, "running Spotlight exclusion selftest after OS change");
                        let report = match home_dir() {
                            Ok(home) => {
                                exclusion::selftest(exclusion_backend.as_ref().unwrap(), &home)
                            }
                            Err(error) => Err(error),
                        };
                        match report {
                            Ok(report) => {
                                exclusion::record_selftest(&mut db, &report)?;
                                match report.outcome {
                                    SelftestOutcome::Pass => {
                                        info!(build = %build, "Spotlight exclusion selftest passed")
                                    }
                                    SelftestOutcome::Inconclusive => warn!(
                                        build = %build,
                                        message = %report.message,
                                        "Spotlight exclusion selftest was inconclusive; automatic exclusions stay enabled for now"
                                    ),
                                    _ => {
                                        warn!(
                                            build = %build,
                                            message = %report.message,
                                            "Spotlight exclusion selftest failed; automatic exclusions disabled"
                                        );
                                        db.log_event("exclusion_selftest_failed", &report.message)?;
                                        exclusion_backend = None;
                                    }
                                }
                            }
                            Err(error) => {
                                warn!(error = %error, "Spotlight exclusion selftest could not run; automatic exclusions disabled");
                                exclusion_backend = None;
                            }
                        }
                    }
                }
            }
            Err(error) => {
                warn!(error = %error, "Spotlight exclusion API unavailable; using the manual export workflow");
                db.log_event("exclusion_api_unavailable", &error.to_string())?;
            }
        }
    }

    let stored_cursor = db.meta_get_u64("fsevent_last_event_id")?;
    let cursor_at = db.meta_get_i64("fsevent_last_event_at")?;
    let total_dirs = db.count_all()?;
    let last_reconcile = db.meta_get_i64("last_reconcile_at")?;
    let max_age_ms = config.scan.max_event_age_hours as i64 * 60 * 60 * 1000;
    let cursor_stale = stored_cursor.is_none()
        || cursor_at
            .map(|at| now_ms() - at > max_age_ms)
            .unwrap_or(true);
    let reconcile_due = last_reconcile
        .map(|at| now_ms() - at >= config.scan.reconcile_interval_hours as i64 * 60 * 60 * 1000)
        .unwrap_or(true);

    let mut cursor;
    if cursor_stale || total_dirs == 0 {
        // Capture the current id *before* scanning.  Events that occur during
        // the scan will be replayed by the stream afterwards.
        let captured = current_cursor();
        db.meta_set_u64("fsevent_last_event_id", captured)?;
        db.meta_set_i64("fsevent_last_event_at", now_ms())?;
        cursor = captured;
        info!(cursor, "starting initial/recovery reconciliation");
        let kind = if total_dirs == 0 {
            ScanKind::Initial
        } else {
            ScanKind::Reconcile
        };
        let matcher = RuleSet::from_config(&config.rules)?;
        let summary = scanner::full_scan(&mut db, &config, &matcher, kind, None)?;
        info!(%summary.duration_ms, new = summary.new_dirs, matched = summary.matched, "startup scan complete");
        run_auto_sync(
            &mut db,
            &config,
            backend_ref(&exclusion_backend),
            "startup scan",
        );
    } else if reconcile_due {
        info!("reconciliation is due; scanning before live monitoring");
        let matcher = RuleSet::from_config(&config.rules)?;
        let summary = scanner::full_scan(&mut db, &config, &matcher, ScanKind::Reconcile, None)?;
        info!(matched = summary.matched, "startup reconciliation complete");
        run_auto_sync(
            &mut db,
            &config,
            backend_ref(&exclusion_backend),
            "startup reconcile",
        );
        cursor = stored_cursor.unwrap();
    } else {
        cursor = stored_cursor.unwrap();
    }

    let (msg_tx, msg_rx) = mpsc::channel::<DaemonMsg>();
    start_signal_thread(msg_tx.clone())?;

    // Bridge raw FSEvents into the same message channel as signals so the main
    // loop can block with recv_timeout (zero idle CPU) without polling.
    let (fs_tx, fs_rx) = mpsc::channel::<RawEvent>();
    let bridge_tx = msg_tx.clone();
    thread::Builder::new()
        .name("fs-bridge".into())
        .spawn(move || {
            for event in fs_rx {
                let _ = bridge_tx.send(DaemonMsg::Fs(event));
            }
        })
        .context("spawning FSEvents bridge thread")?;

    let mut matcher = RuleSet::from_config(&config.rules)?;
    let mut watcher = match start_watcher(&config, cursor, &fs_tx) {
        Ok(watcher) => watcher,
        Err(error) => {
            warn!(error = %error, "FSEvents could not start; continuing with periodic scans");
            None
        }
    };
    let mut next_reconcile = next_reconcile_deadline(&config);
    let mut config_mtime = read_config_mtime(&paths);

    info!(watched = watcher.is_some(), cursor, "daemon ready");

    loop {
        let now = now_ms();
        if now >= next_reconcile {
            run_reconcile(
                &mut db,
                &config,
                &matcher,
                backend_ref(&exclusion_backend),
                &mut cursor,
            )?;
            if watcher.is_none() {
                watcher = match start_watcher(&config, cursor, &fs_tx) {
                    Ok(watcher) => watcher,
                    Err(error) => {
                        warn!(error = %error, "FSEvents retry failed; periodic scans remain active");
                        None
                    }
                };
            }
            next_reconcile = next_reconcile_deadline(&config);
            continue;
        }
        let wait = Duration::from_millis((next_reconcile - now).max(1) as u64);
        match msg_rx.recv_timeout(wait) {
            Ok(DaemonMsg::Fs(first)) => {
                let mut events = vec![first];
                let batch_start = Instant::now();
                let debounce = Duration::from_millis(config.daemon.event_debounce_ms);
                let mut reload = false;
                loop {
                    let remaining = debounce.saturating_sub(batch_start.elapsed().min(debounce));
                    if remaining.is_zero() || batch_start.elapsed() >= Duration::from_secs(2) {
                        break;
                    }
                    match msg_rx.recv_timeout(remaining) {
                        Ok(DaemonMsg::Fs(event)) => events.push(event),
                        Ok(DaemonMsg::Reload) => {
                            reload = true;
                            break;
                        }
                        Ok(DaemonMsg::Shutdown) => return shutdown(&mut db, "signal"),
                        Err(RecvTimeoutError::Timeout) => break,
                        Err(RecvTimeoutError::Disconnected) => {
                            bail!("daemon message channel disconnected")
                        }
                    }
                }

                let batch = process_batch(&mut db, &config, &matcher, &events, cursor)?;
                cursor = batch.latest_cursor;
                if !batch.lost && batch.changed {
                    run_auto_sync(
                        &mut db,
                        &config,
                        backend_ref(&exclusion_backend),
                        "filesystem event",
                    );
                }

                if batch.lost {
                    warn!("FSEvents reported dropped history; reconciling");
                    run_reconcile(
                        &mut db,
                        &config,
                        &matcher,
                        backend_ref(&exclusion_backend),
                        &mut cursor,
                    )?;
                    next_reconcile = next_reconcile_deadline(&config);
                }

                if reload {
                    let changed = reload_config(
                        &mut db,
                        &paths,
                        &mut config,
                        &mut matcher,
                        &mut watcher,
                        &fs_tx,
                        &mut cursor,
                        &mut config_mtime,
                    )?;
                    if changed {
                        run_reconcile(
                            &mut db,
                            &config,
                            &matcher,
                            backend_ref(&exclusion_backend),
                            &mut cursor,
                        )?;
                        next_reconcile = next_reconcile_deadline(&config);
                    }
                }
            }
            Ok(DaemonMsg::Reload) => {
                let changed = reload_config(
                    &mut db,
                    &paths,
                    &mut config,
                    &mut matcher,
                    &mut watcher,
                    &fs_tx,
                    &mut cursor,
                    &mut config_mtime,
                )?;
                if changed {
                    run_reconcile(
                        &mut db,
                        &config,
                        &matcher,
                        backend_ref(&exclusion_backend),
                        &mut cursor,
                    )?;
                    next_reconcile = next_reconcile_deadline(&config);
                }
            }
            Ok(DaemonMsg::Shutdown) => {
                return shutdown(&mut db, "signal");
            }
            Err(RecvTimeoutError::Timeout) => {
                if watch_roots(&config)?.is_empty() {
                    // Nothing to watch yet.  Keep the service idle; a later
                    // add-path/config change or the periodic reconciliation
                    // will pick roots up.
                    debug!("no existing scan roots; waiting for configuration changes");
                }
                if config_mtime != read_config_mtime(&paths) {
                    let changed = reload_config(
                        &mut db,
                        &paths,
                        &mut config,
                        &mut matcher,
                        &mut watcher,
                        &fs_tx,
                        &mut cursor,
                        &mut config_mtime,
                    )?;
                    if changed {
                        run_reconcile(
                            &mut db,
                            &config,
                            &matcher,
                            backend_ref(&exclusion_backend),
                            &mut cursor,
                        )?;
                        next_reconcile = next_reconcile_deadline(&config);
                    }
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                bail!("daemon message channel disconnected")
            }
        }
    }
}

struct BatchOutcome {
    latest_cursor: u64,
    lost: bool,
    changed: bool,
}

fn process_batch(
    db: &mut Db,
    config: &Config,
    matcher: &RuleSet,
    events: &[RawEvent],
    cursor: u64,
) -> Result<BatchOutcome> {
    let mut latest = cursor;
    let mut lost = false;
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut seen = HashSet::new();
    let roots = config.root_paths()?;

    for event in events {
        latest = latest.max(event.event_id);
        if event.lost_history() {
            lost = true;
            continue;
        }
        if event.flags & watcher::FLAG_HISTORY_DONE != 0 {
            continue;
        }
        if !watcher::flags_interesting(event.flags) {
            continue;
        }
        if !event_relevant(&event.path, event.flags, matcher, &roots) {
            continue;
        }
        if seen.insert(event.path.clone()) {
            paths.push(event.path.clone());
        }
    }

    if latest > cursor {
        db.meta_set_u64("fsevent_last_event_id", latest)?;
        db.meta_set_i64("fsevent_last_event_at", now_ms())?;
    }

    if lost {
        return Ok(BatchOutcome {
            latest_cursor: latest,
            lost: true,
            changed: false,
        });
    }

    if paths.len() > config.daemon.max_batch_events {
        warn!(
            paths = paths.len(),
            cap = config.daemon.max_batch_events,
            "event batch too large; reconciling instead of walking each path"
        );
        return Ok(BatchOutcome {
            latest_cursor: latest,
            lost: true,
            changed: false,
        });
    }

    if paths.is_empty() {
        return Ok(BatchOutcome {
            latest_cursor: latest,
            lost: false,
            changed: false,
        });
    }

    // Keep a small prefix-coalesced set so a copied project does not cause
    // repeated subtree walks.
    paths.sort_by_key(|p| (p.components().count(), p.clone()));
    let mut coalesced: Vec<PathBuf> = Vec::new();
    for path in paths {
        if coalesced.iter().any(|kept| path.starts_with(kept)) {
            continue;
        }
        coalesced.push(path);
    }

    let summary = scanner::targeted_discover(db, config, matcher, &coalesced)?;
    if summary.matched > 0 || summary.missing_dirs > 0 || summary.errors > 0 {
        info!(
            events = summary.event_paths,
            visited = summary.dirs_visited,
            matched = summary.matched,
            new = summary.new_dirs,
            missing = summary.missing_dirs,
            errors = summary.errors,
            "targeted discovery"
        );
    }
    let changed = summary.matched > 0
        || summary.missing_dirs > 0
        || summary.new_dirs > 0
        || summary.errors > 0;
    Ok(BatchOutcome {
        latest_cursor: latest,
        lost: false,
        changed,
    })
}

/// Cheap prefilter before events reach the database.  Directory events are
/// preserved; file events are kept only when their name or parent name can be a
/// rule, and events inside an already-matching artifact are dropped.
fn event_relevant(
    path: &std::path::Path,
    flags: u32,
    matcher: &RuleSet,
    roots: &[PathBuf],
) -> bool {
    if path.as_os_str().is_empty() {
        return false;
    }
    // Only names *inside* a configured root matter.  Ancestors above the root
    // (for example a home directory beginning with a dot) must not cause every
    // event to be discarded.
    let Some(root) = roots
        .iter()
        .filter(|root| path.starts_with(root))
        .max_by_key(|root| root.components().count())
    else {
        return false;
    };
    if path == root {
        return false;
    }
    let scan_dot = matcher.scan_dot_directories();
    if !scan_dot {
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if is_hidden_name(name) {
                return false;
            }
        }
    }
    let mut ancestor = path.parent();
    while let Some(parent) = ancestor {
        if parent == root || !parent.starts_with(root) {
            break;
        }
        if let Some(name) = parent.file_name().and_then(|n| n.to_str()) {
            if RuleSet::is_vcs_metadata(name) {
                return false;
            }
            if !scan_dot && is_hidden_name(name) {
                return false;
            }
            // Do not traverse/observe inside an artifact directory.  The
            // artifact itself (path, not ancestor) remains relevant.
            if matcher.is_match(name) {
                return false;
            }
        }
        ancestor = parent.parent();
    }
    // Removal/rename needs to reach the database even for a file event so
    // directories below a removed project can be marked missing.
    if flags & (watcher::FLAG_ITEM_IS_DIR | watcher::FLAG_ITEM_REMOVED | watcher::FLAG_ITEM_RENAMED)
        != 0
    {
        return true;
    }
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if matcher.is_match(name) {
        return true;
    }
    path.parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .map(|parent| matcher.is_match(parent))
        .unwrap_or(false)
}

fn start_watcher(
    config: &Config,
    cursor: u64,
    sender: &Sender<RawEvent>,
) -> Result<Option<StreamHandle>> {
    let roots = watch_roots(config)?;
    if roots.is_empty() {
        info!("no existing scan roots to watch yet");
        return Ok(None);
    }
    let latency = config.daemon.event_latency_ms as f64 / 1000.0;
    let handle = watcher::start(&roots, cursor, latency, sender.clone())?;
    info!(
        roots = roots.len(),
        cursor,
        latency_ms = config.daemon.event_latency_ms,
        "FSEvents stream started"
    );
    Ok(Some(handle))
}

fn current_cursor() -> u64 {
    let id = watcher::current_event_id();
    if id == 0 { watcher::SINCE_NOW } else { id }
}

fn watch_roots(config: &Config) -> Result<Vec<PathBuf>> {
    let mut roots = Vec::new();
    for root in config.root_paths()? {
        if root.is_dir() {
            roots.push(root);
        }
    }
    roots.sort();
    roots.dedup();
    Ok(roots)
}

fn next_reconcile_deadline(config: &Config) -> i64 {
    now_ms() + config.scan.reconcile_interval_hours as i64 * 60 * 60 * 1000
}

fn read_config_mtime(paths: &AppPaths) -> Option<i64> {
    std::fs::metadata(&paths.config_path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
}

fn sync_config(db: &mut Db, config: &Config) -> Result<()> {
    let roots = config.root_paths()?;
    db.sync_roots(&roots)?;
    let matcher = RuleSet::from_config(&config.rules)?;
    db.sync_rules(matcher.all_rules())?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn reload_config(
    db: &mut Db,
    paths: &AppPaths,
    config: &mut Config,
    matcher: &mut RuleSet,
    watcher_handle: &mut Option<StreamHandle>,
    fs_tx: &Sender<RawEvent>,
    cursor: &mut u64,
    config_mtime: &mut Option<i64>,
) -> Result<bool> {
    let old_roots = config.root_paths()?;
    let old_rules = config.rules.clone();
    let fresh = match Config::load(&paths.config_path) {
        Ok(fresh) => fresh,
        Err(error) => {
            error!(error = %error, "configuration reload failed; keeping previous configuration");
            return Ok(false);
        }
    };
    let new_roots = fresh.root_paths()?;
    let roots_changed = old_roots != new_roots;
    let rules_changed = old_rules != fresh.rules;
    *config = fresh;
    *matcher = RuleSet::from_config(&config.rules)?;
    sync_config(db, config)?;
    *config_mtime = read_config_mtime(paths);
    *cursor = db
        .meta_get_u64("fsevent_last_event_id")?
        .unwrap_or_else(current_cursor);

    if roots_changed {
        // Restarting from the persisted cursor replays anything that happened
        // between the old and new stream; duplicate targeted discovery is
        // idempotent, and the caller runs a reconciliation.
        *watcher_handle = None;
        *watcher_handle = match start_watcher(config, *cursor, fs_tx) {
            Ok(watcher) => watcher,
            Err(error) => {
                warn!(error = %error, "FSEvents restart failed; relying on periodic scans");
                None
            }
        };
    }
    info!(roots_changed, rules_changed, "configuration reloaded");
    Ok(roots_changed || rules_changed)
}

fn backend_ref(backend: &Option<MetadataBackend>) -> Option<&dyn ExclusionBackend> {
    backend
        .as_ref()
        .map(|backend| backend as &dyn ExclusionBackend)
}

/// Automatic exclusion is best-effort: a failure is logged and leaves the
/// directory in the manual/export path, but it never stops discovery.
fn run_auto_sync(
    db: &mut Db,
    config: &Config,
    backend: Option<&dyn ExclusionBackend>,
    reason: &str,
) {
    match exclusion::auto_sync(db, config, backend, reason) {
        Ok(Some(report)) => {
            if !report.add.is_empty()
                || !report.remove.is_empty()
                || !report.failed_adds.is_empty()
                || !report.failed_removes.is_empty()
            {
                info!(
                    reason,
                    added = report.add.len(),
                    removed = report.remove.len(),
                    failed = report.failed_adds.len() + report.failed_removes.len(),
                    desired = report.desired_count,
                    "automatic exclusion sync"
                );
                for path in &report.add {
                    info!(path = %path, "excluded");
                }
                for path in &report.remove {
                    info!(path = %path, "unexcluded stale entry");
                }
                for path in report
                    .failed_adds
                    .iter()
                    .chain(report.failed_removes.iter())
                {
                    warn!(path = %path, "Spotlight did not apply the exclusion change");
                }
            }
        }
        Ok(None) => {}
        Err(error) => {
            warn!(reason, error = %error, "automatic exclusion sync failed; manual export remains available");
        }
    }
}

fn run_reconcile(
    db: &mut Db,
    config: &Config,
    matcher: &RuleSet,
    backend: Option<&dyn ExclusionBackend>,
    cursor: &mut u64,
) -> Result<()> {
    info!("starting periodic reconciliation");
    let summary = scanner::full_scan(db, config, matcher, ScanKind::Reconcile, None)?;
    info!(
        visited = summary.dirs_visited,
        matched = summary.matched,
        new = summary.new_dirs,
        missing = summary.missing_dirs,
        errors = summary.errors,
        "reconciliation complete"
    );
    run_auto_sync(db, config, backend, "periodic reconcile");
    *cursor = db.meta_get_u64("fsevent_last_event_id")?.unwrap_or(*cursor);
    Ok(())
}

fn shutdown(db: &mut Db, reason: &str) -> Result<()> {
    info!(reason, "daemon shutting down");
    let _ = db.log_event("daemon_stop", reason);
    let _ = db.checkpoint();
    Ok(())
}

fn start_signal_thread(sender: Sender<DaemonMsg>) -> Result<()> {
    let mut signals = Signals::new([SIGTERM, SIGINT, SIGHUP])?;
    thread::Builder::new()
        .name("signals".into())
        .spawn(move || {
            for signal in signals.forever() {
                let message = match signal {
                    SIGHUP => DaemonMsg::Reload,
                    _ => DaemonMsg::Shutdown,
                };
                if sender.send(message).is_err() {
                    break;
                }
            }
        })
        .context("spawning signal thread")?;
    Ok(())
}
