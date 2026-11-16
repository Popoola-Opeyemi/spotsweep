//! User-facing command line interface.

use crate::config::Config;
use crate::database::{Db, DirectoryFilter, MarkTarget};
use crate::exporter::{self, ExportRequest};
use crate::launchagent;
use crate::matcher::RuleSet;
use crate::model::{DirectoryRecord, ScanKind, Status};
use crate::paths::{AppPaths, display_path, home_dir, normalize_path, validate_scan_root};
use crate::scanner;
use crate::util::{
    atomic_write, human_bytes, json_pretty, latest_log_file, now_ms, opt_timestamp, print_table,
    tail_file, timestamp_to_string,
};
use crate::verifier;
use crate::watcher;
use anyhow::{Context, Result, bail};
use clap::{ArgAction, Parser, Subcommand};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Parser)]
#[command(
    name = "spotlight-dev",
    version,
    about = "Discover development build directories and prepare a manual Spotlight Search Privacy list",
    long_about = "spotlight-dev never modifies Spotlight configuration, mds, or private stores. \
It discovers build artifacts, persists them in SQLite, watches for new ones with native FSEvents, \
and exports a plain-text list for manual addition in System Settings -> Spotlight -> Search Privacy."
)]
struct Cli {
    /// Override the application data directory (default: ~/Library/Application Support/SpotlightDevExclusions)
    #[arg(long, global = true, value_name = "DIR")]
    data_dir: Option<PathBuf>,

    /// Override the configuration file
    #[arg(long, global = true, value_name = "FILE")]
    config: Option<PathBuf>,

    /// Increase log verbosity (-v info, -vv debug, -vvv trace)
    #[arg(short = 'v', long, global = true, action = ArgAction::Count)]
    verbose: u8,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run an incremental discovery scan
    Scan {
        /// Force a complete reconciliation walk
        #[arg(long)]
        full: bool,
        /// Scan only one configured root
        #[arg(long)]
        root: Option<String>,
    },
    /// Show overall statistics and service state
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Show discovered directories
    List {
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        rule: Option<String>,
        #[arg(long)]
        root: Option<String>,
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long)]
        json: bool,
    },
    /// Show directories awaiting manual Spotlight exclusion
    Pending {
        #[arg(long, default_value_t = 100)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Generate a detailed inventory grouped by project
    Report {
        #[arg(long, value_name = "FILE")]
        output: Option<PathBuf>,
    },
    /// Generate a plain-text exclusion list (does not modify Spotlight)
    Export {
        /// Export all existing discovered directories
        #[arg(long)]
        all: bool,
        /// Export only pending directories (default)
        #[arg(long)]
        pending: bool,
        /// Export only one rule name or category (e.g. node_modules, build)
        #[arg(long = "type", value_name = "RULE")]
        rule_type: Option<String>,
        /// Group paths by project directory
        #[arg(long)]
        group: bool,
        /// Always produce a flat path list
        #[arg(long, conflicts_with = "group")]
        flat: bool,
        /// Omit comments and instructions
        #[arg(long)]
        no_header: bool,
        #[arg(long, value_name = "FILE")]
        output: Option<PathBuf>,
        /// Reveal the exported file in Finder
        #[arg(long)]
        open: bool,
    },
    /// Check available exclusion information without modifying Spotlight
    Verify {
        /// Probe mdutil -P (root only; diagnostic; never treated as proof)
        #[arg(long)]
        mdutil: bool,
        /// Record your explicit visual confirmation from System Settings
        #[arg(long)]
        confirm: bool,
        /// Restrict confirmation to these absolute paths
        #[arg(long = "path", value_name = "PATH")]
        paths: Vec<String>,
        /// Exit with status 3 unless verification succeeded
        #[arg(long)]
        strict: bool,
    },
    /// Inspect SQLite, FSEvents cursor and scan state
    State {
        #[arg(long)]
        json: bool,
    },
    /// Re-check missing directories and clear recoverable failures
    Retry,
    /// Force a complete reconciliation scan
    Rescan {
        #[arg(long)]
        root: Option<String>,
    },
    /// Display the background service log
    Logs {
        #[arg(long, default_value_t = 100)]
        lines: usize,
        #[arg(long)]
        follow: bool,
    },
    /// Add a scanning root
    AddPath {
        path: String,
        /// Only update configuration; do not scan the new root now
        #[arg(long)]
        no_scan: bool,
    },
    /// Remove a scanning root (history and project files are preserved)
    RemovePath { path: String },
    /// Display or modify configuration
    Config {
        #[command(subcommand)]
        cmd: Option<ConfigCommand>,
    },
    /// Manage the user LaunchAgent
    Service {
        #[command(subcommand)]
        cmd: ServiceCommand,
    },
    /// Mark exported directories as manually added to Search Privacy
    Mark {
        #[arg(value_name = "PATH")]
        paths: Vec<String>,
        #[arg(long)]
        all: bool,
        #[arg(long, value_name = "ID")]
        export: Option<i64>,
    },
    /// Remove the LaunchAgent and optionally application data
    Uninstall {
        /// Delete the database, configuration, exports and logs
        #[arg(long)]
        purge_data: bool,
        /// Delete the installed executable
        #[arg(long)]
        remove_binary: bool,
        /// Required with --purge-data
        #[arg(long)]
        yes: bool,
    },
    /// Run the background service in the foreground (LaunchAgent entry point)
    Daemon,
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Print the active configuration and its path
    Show,
    /// Print the configuration file path
    Path,
    /// Create the default configuration file
    Init {
        #[arg(long)]
        force: bool,
    },
    /// Set a supported key (see README)
    Set { key: String, value: String },
    /// Open the configuration in $EDITOR
    Edit,
    /// Ask a running daemon to reload configuration
    Reload,
}

#[derive(Debug, Subcommand)]
enum ServiceCommand {
    /// Install and start the LaunchAgent
    Install {
        /// Executable to install (default: current executable)
        #[arg(long, value_name = "FILE")]
        binary: Option<PathBuf>,
    },
    /// Show LaunchAgent state
    Status {
        #[arg(long)]
        json: bool,
    },
    Start,
    Stop,
    Restart,
    /// Remove the LaunchAgent; data is preserved
    Uninstall,
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let paths = AppPaths::resolve(cli.data_dir.as_deref(), cli.config.as_deref())?;
    // The daemon installs its own file-backed subscriber.  Installing the CLI
    // subscriber first would make the daemon's `try_init` fail silently and
    // send all service logs to stderr instead of the rotating log file.
    if !matches!(cli.command, Command::Daemon) {
        crate::logging::init_cli(cli.verbose)?;
    }

    match cli.command {
        Command::Config { cmd } => cmd_config(&paths, cmd),
        Command::Service { cmd } => cmd_service(&paths, cmd),
        Command::Uninstall {
            purge_data,
            remove_binary,
            yes,
        } => cmd_uninstall(&paths, purge_data, remove_binary, yes),
        Command::Daemon => crate::daemon::run(paths),
        Command::Logs { lines, follow } => cmd_logs(&paths, lines, follow),
        other => {
            let (config, mut db, matcher) = open_state(&paths)?;
            match other {
                Command::Scan { full, root } => {
                    cmd_scan(&paths, &config, &mut db, &matcher, full, root)
                }
                Command::Status { json } => cmd_status(&paths, &config, &db, json),
                Command::List {
                    status,
                    rule,
                    root,
                    project,
                    limit,
                    json,
                } => cmd_list(&paths, &db, status, rule, root, project, limit, json),
                Command::Pending { limit, json } => cmd_pending(&db, limit, json),
                Command::Report { output } => cmd_report(&paths, &config, &db, output),
                Command::Export {
                    all,
                    pending,
                    rule_type,
                    group,
                    flat,
                    no_header,
                    output,
                    open,
                } => cmd_export(
                    &paths, &config, &mut db, all, pending, rule_type, group, flat, no_header,
                    output, open,
                ),
                Command::Verify {
                    mdutil,
                    confirm,
                    paths: verify_paths,
                    strict,
                } => cmd_verify(
                    &paths,
                    &config,
                    &mut db,
                    mdutil,
                    confirm,
                    verify_paths,
                    strict,
                ),
                Command::State { json } => cmd_state(&paths, &db, json),
                Command::Retry => cmd_retry(&mut db),
                Command::Rescan { root } => cmd_rescan(&paths, &config, &mut db, &matcher, root),
                Command::AddPath { path, no_scan } => {
                    cmd_add_path(&paths, &mut db, &matcher, &path, no_scan)
                }
                Command::RemovePath { path } => cmd_remove_path(&paths, &mut db, &path),
                Command::Mark {
                    paths: mark_paths,
                    all,
                    export,
                } => cmd_mark(&mut db, mark_paths, all, export),
                Command::Config { .. }
                | Command::Service { .. }
                | Command::Uninstall { .. }
                | Command::Daemon
                | Command::Logs { .. } => unreachable!(),
            }
        }
    }
}

fn open_state(paths: &AppPaths) -> Result<(Config, Db, RuleSet)> {
    let config = Config::load_or_init(paths)?;
    let mut db = Db::open(paths)?;
    let matcher = RuleSet::from_config(&config.rules)?;
    db.sync_roots(&config.root_paths()?)?;
    db.sync_rules(matcher.all_rules())?;
    Ok((config, db, matcher))
}

fn cmd_scan(
    paths: &AppPaths,
    config: &Config,
    db: &mut Db,
    matcher: &RuleSet,
    force_full: bool,
    only_root: Option<String>,
) -> Result<()> {
    let only_root = match only_root {
        Some(root) => Some(normalize_path(Path::new(&root))?),
        None => None,
    };

    let stored_cursor = db.meta_get_u64("fsevent_last_event_id")?;
    let cursor_at = db.meta_get_i64("fsevent_last_event_at")?;
    let cursor_stale = cursor_at
        .map(|at| now_ms() - at > config.scan.max_event_age_hours as i64 * 3600 * 1000)
        .unwrap_or(true);

    if force_full || only_root.is_some() || stored_cursor.is_none() || cursor_stale {
        if stored_cursor.is_none() || cursor_stale {
            let id = watcher::current_event_id();
            let id = if id == 0 { watcher::SINCE_NOW } else { id };
            db.meta_set_u64("fsevent_last_event_id", id)?;
            db.meta_set_i64("fsevent_last_event_at", now_ms())?;
        }
        let kind = if db.count_all()? == 0 {
            ScanKind::Initial
        } else {
            ScanKind::Reconcile
        };
        let summary = scanner::full_scan(db, config, matcher, kind, only_root.as_deref())?;
        print_scan_summary(&summary);
        println!("Next: `spotlight-dev export` to produce the manual Search Privacy list.");
        let _ = paths;
        return Ok(());
    }

    let roots: Vec<PathBuf> = config
        .root_paths()?
        .into_iter()
        .filter(|root| root.is_dir())
        .collect();
    if roots.is_empty() {
        println!(
            "No configured scan roots exist yet. Add one with `spotlight-dev add-path ~/Projects`."
        );
        return Ok(());
    }

    let cursor = stored_cursor.unwrap();
    let catch_up = watcher::catch_up(&roots, cursor, Duration::from_millis(1500), 0.05)?;

    if catch_up.lost {
        println!("FSEvents history was incomplete; running a reconciliation walk.");
        let summary = scanner::full_scan(db, config, matcher, ScanKind::Reconcile, None)?;
        print_scan_summary(&summary);
    } else {
        let event_paths: Vec<PathBuf> = catch_up
            .events
            .into_iter()
            .filter(|event| !event.is_history_done())
            .map(|event| event.path)
            .collect();
        let summary = scanner::targeted_discover(db, config, matcher, &event_paths)?;
        if let Some(latest) = catch_up.latest_event_id {
            if latest > cursor {
                db.meta_set_u64("fsevent_last_event_id", latest)?;
                db.meta_set_i64("fsevent_last_event_at", now_ms())?;
            }
        }
        if summary.matched == 0 && summary.missing_dirs == 0 && summary.new_dirs == 0 {
            println!(
                "No new matching directories ({} event paths inspected, {} directories visited, {} ms).",
                summary.event_paths, summary.dirs_visited, summary.duration_ms
            );
        } else {
            println!(
                "Incremental scan: {} event paths, {} directories visited, {} matched, {} new, {} missing, {} errors, {} ms.",
                summary.event_paths,
                summary.dirs_visited,
                summary.matched,
                summary.new_dirs,
                summary.missing_dirs,
                summary.errors,
                summary.duration_ms
            );
            println!("Next: `spotlight-dev pending` or `spotlight-dev export`.");
        }
    }
    Ok(())
}

fn print_scan_summary(summary: &crate::model::ScanSummary) {
    println!("{} scan complete.", summary.kind);
    println!(
        "  roots: {}/{} scanned, {} missing",
        summary.roots_scanned, summary.roots_configured, summary.roots_missing
    );
    println!("  directories visited: {}", summary.dirs_visited);
    println!("  matches seen: {}", summary.matched);
    println!("  new directories: {}", summary.new_dirs);
    println!("  newly missing: {}", summary.missing_dirs);
    if summary.errors > 0 {
        println!(
            "  walk errors: {} (missing detection skipped for affected roots)",
            summary.errors
        );
    }
    println!("  duration: {} ms", summary.duration_ms);
}

fn cmd_status(paths: &AppPaths, config: &Config, db: &Db, json_output: bool) -> Result<()> {
    let counts = db.count_by_status()?;
    let mut by_status: BTreeMap<String, i64> = Status::ALL
        .iter()
        .map(|s| (s.as_str().to_string(), 0))
        .collect();
    let mut total = 0;
    for (status, count) in counts {
        total += count;
        by_status.insert(status, count);
    }
    let roots = db.all_roots()?;
    let enabled_roots: Vec<_> = roots.iter().filter(|r| r.enabled).collect();
    let existing_roots = enabled_roots
        .iter()
        .filter(|r| Path::new(&r.path).is_dir())
        .count();
    let pending = db.count_pending()?;
    let service = launchagent::status(paths).ok();
    let verification = db.latest_verification()?;
    let exports = db.recent_exports(1)?;
    let last_scan_at = db.meta_get_i64("last_scan_at")?;
    let last_scan_kind = db.meta_get("last_scan_kind")?;
    let last_reconcile = db.meta_get_i64("last_reconcile_at")?;
    let cursor = db.meta_get_u64("fsevent_last_event_id")?;
    let cursor_at = db.meta_get_i64("fsevent_last_event_at")?;
    let db_size = db.db_size_bytes();

    if json_output {
        let status_map: serde_json::Map<String, serde_json::Value> = by_status
            .iter()
            .map(|(k, v)| (k.clone(), json!(v)))
            .collect();
        json_pretty(&json!({
            "data_dir": paths.data_dir,
            "config": paths.config_path,
            "database": paths.db_path,
            "database_bytes": db_size,
            "total_directories": total,
            "by_status": status_map,
            "pending": pending,
            "roots_configured": enabled_roots.len(),
            "roots_existing": existing_roots,
            "last_scan_at": last_scan_at,
            "last_scan_kind": last_scan_kind,
            "last_reconcile_at": last_reconcile,
            "fsevent_cursor": cursor,
            "fsevent_cursor_at": cursor_at,
            "daemon": service,
            "latest_verification": verification,
            "latest_export": exports.first(),
        }))?;
        return Ok(());
    }

    println!("Spotlight Dev Exclusions");
    println!("  Data:      {}", paths.data_dir.display());
    println!("  Config:    {}", paths.config_path.display());
    println!(
        "  Database:  {} ({}, schema v{})",
        paths.db_path.display(),
        human_bytes(db_size),
        db.schema_version()?
    );
    println!(
        "  Roots:     {} configured, {} existing",
        config.scan.roots.len(),
        existing_roots
    );
    println!("  Directories: {total} tracked, {pending} pending manual exclusion");
    for status in Status::ALL {
        let count = by_status.get(status.as_str()).copied().unwrap_or(0);
        println!("    {:<24} {count}", status.as_str());
    }
    println!(
        "  Last scan: {} ({})",
        opt_timestamp(last_scan_at),
        last_scan_kind.unwrap_or_else(|| "—".into())
    );
    println!("  Last reconciliation: {}", opt_timestamp(last_reconcile));
    println!(
        "  FSEvents cursor: {} ({})",
        cursor
            .map(|c| c.to_string())
            .unwrap_or_else(|| "not initialised".into()),
        opt_timestamp(cursor_at)
    );
    match service {
        Some(service) => {
            let state = if !service.installed {
                "not installed"
            } else if service.running {
                "running"
            } else if service.loaded {
                "loaded, not running"
            } else {
                "installed, stopped"
            };
            println!(
                "  Service:   {state}{}",
                service
                    .pid
                    .map(|pid| format!(" (pid {pid})"))
                    .unwrap_or_default()
            );
        }
        None => println!("  Service:   unknown (launchctl unavailable)"),
    }
    match exports.first() {
        Some(export) => println!(
            "  Latest export: {} ({} paths, {})",
            export.path,
            export.item_count,
            timestamp_to_string(export.created_at)
        ),
        None => println!("  Latest export: none"),
    }
    match verification {
        Some(run) => println!(
            "  Verification: {} ({}, {})",
            run.outcome,
            run.method,
            timestamp_to_string(run.started_at)
        ),
        None => println!("  Verification: not run"),
    }

    if pending > 0 {
        println!(
            "\n{pending} {} manual Spotlight exclusion.\nNext: `spotlight-dev export`",
            if pending == 1 {
                "directory needs"
            } else {
                "directories need"
            }
        );
    } else if total == 0 {
        println!("\nNo directories discovered yet. Run `spotlight-dev scan`.");
    } else {
        println!(
            "\nNothing is pending. Exported directories still require manual confirmation if any are marked exported."
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_list(
    _paths: &AppPaths,
    db: &Db,
    status: Option<String>,
    rule: Option<String>,
    root: Option<String>,
    project: Option<String>,
    limit: Option<usize>,
    json_output: bool,
) -> Result<()> {
    let statuses = match status.as_deref() {
        Some("pending") => vec![
            Status::Discovered,
            Status::PendingManualExclusion,
            Status::Exported,
        ],
        Some("active") => Status::ALL
            .into_iter()
            .filter(|s| *s != Status::Missing)
            .collect(),
        Some(value) => {
            let parsed = Status::parse(value)
                .ok_or_else(|| anyhow::anyhow!("unknown status {value:?}; expected pending, active, discovered, pending_manual_exclusion, exported, manually_applied, verified or missing"))?;
            vec![parsed]
        }
        None => Vec::new(),
    };
    let filter = DirectoryFilter {
        statuses,
        rule,
        root: root
            .map(|r| normalize_path(Path::new(&r)).map(|p| p.to_string_lossy().to_string()))
            .transpose()?,
        project: project
            .map(|r| normalize_path(Path::new(&r)).map(|p| p.to_string_lossy().to_string()))
            .transpose()?,
        limit,
    };
    let rows = db.list_directories(&filter)?;
    if json_output {
        return json_pretty(&rows);
    }
    if rows.is_empty() {
        println!("No directories match.");
        return Ok(());
    }
    let table: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            vec![
                row.status.clone(),
                format!("{}:{}", row.rule_kind, row.rule),
                row.path.clone(),
            ]
        })
        .collect();
    print_table(&["STATUS", "RULE", "PATH"], &table);
    println!(
        "\n{} director{}.",
        rows.len(),
        if rows.len() == 1 { "y" } else { "ies" }
    );
    Ok(())
}

fn cmd_pending(db: &Db, limit: usize, json_output: bool) -> Result<()> {
    let mut rows = db.pending_directories()?;
    let total = rows.len();
    let shown = total.min(limit);
    rows.truncate(limit);
    if json_output {
        return json_pretty(&rows);
    }
    if rows.is_empty() {
        println!("No directories are awaiting manual exclusion.");
        println!("Run `spotlight-dev scan` to discover changes.");
        return Ok(());
    }
    let table: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            vec![
                row.status.clone(),
                format!("{}:{}", row.rule_kind, row.rule),
                row.path.clone(),
            ]
        })
        .collect();
    print_table(&["STATUS", "RULE", "PATH"], &table);
    if shown < total {
        println!("\n(limited to {shown} of {total} rows)");
    }
    println!(
        "\nNext: `spotlight-dev export` (or `spotlight-dev export --group`).\nExporting does not change Spotlight and does not mean a directory is excluded."
    );
    Ok(())
}

fn cmd_report(paths: &AppPaths, _config: &Config, db: &Db, output: Option<PathBuf>) -> Result<()> {
    let rows = db.list_directories(&DirectoryFilter {
        limit: None,
        ..Default::default()
    })?;
    let mut by_project: BTreeMap<String, Vec<&DirectoryRecord>> = BTreeMap::new();
    for row in &rows {
        by_project
            .entry(row.project_root.clone())
            .or_default()
            .push(row);
    }
    let counts = db.count_by_status()?;
    let mut text = String::new();
    text.push_str("# Spotlight Development Directory Inventory\n");
    text.push_str(&format!("# Generated: {}\n", timestamp_to_string(now_ms())));
    text.push_str(&format!("# Data: {}\n", paths.data_dir.display()));
    text.push_str(&format!("# Total directories: {}\n", rows.len()));
    for (status, count) in &counts {
        text.push_str(&format!("#   {status}: {count}\n"));
    }
    text.push('\n');
    if rows.is_empty() {
        text.push_str("No matching directories were found.\n");
    }
    for (project, entries) in by_project {
        text.push_str(&format!("## {project} ({})\n", entries.len()));
        let mut sorted = entries;
        sorted.sort_by(|a, b| a.path.cmp(&b.path));
        for row in sorted {
            text.push_str(&format!(
                "- {} — status={} rule={} ({}) first_seen={} last_seen={}\n",
                row.path,
                row.status,
                row.rule,
                row.rule_kind,
                timestamp_to_string(row.first_seen_at),
                timestamp_to_string(row.last_seen_at)
            ));
        }
        text.push('\n');
    }

    match output {
        Some(file) => {
            let file = normalize_path(&file)?;
            if let Some(parent) = file.parent() {
                std::fs::create_dir_all(parent)?;
            }
            atomic_write(&file, text.as_bytes(), 0o600)?;
            println!("Inventory written to {}", file.display());
        }
        None => {
            print!("{text}");
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_export(
    paths: &AppPaths,
    config: &Config,
    db: &mut Db,
    all: bool,
    pending: bool,
    rule_type: Option<String>,
    group: bool,
    flat: bool,
    no_header: bool,
    output: Option<PathBuf>,
    open: bool,
) -> Result<()> {
    if all && pending {
        bail!("--all and --pending are mutually exclusive");
    }
    if rule_type.is_some() && (all || pending) {
        bail!("--type cannot be combined with --all or --pending");
    }
    let request = if let Some(rule) = rule_type {
        ExportRequest::Rule(rule)
    } else if all {
        ExportRequest::All
    } else {
        ExportRequest::Pending
    };
    let group_by_project = if flat {
        false
    } else if group {
        true
    } else {
        config.export.group_by_project
    };
    let include_header = !no_header && config.export.include_header;
    let outcome = exporter::export(
        db,
        config,
        paths,
        &request,
        group_by_project,
        include_header,
        output.as_deref(),
    )?;
    println!(
        "Exported {} director{} (selection: {}) to:\n  {}",
        outcome.count,
        if outcome.count == 1 { "y" } else { "ies" },
        outcome.selection,
        outcome.path.display()
    );
    println!();
    println!("{}", exporter::instructions(&outcome.path));
    if open {
        let _ = std::process::Command::new("/usr/bin/open")
            .arg("-R")
            .arg(&outcome.path)
            .status();
    }
    Ok(())
}

fn cmd_verify(
    _paths: &AppPaths,
    config: &Config,
    db: &mut Db,
    mdutil: bool,
    confirm: bool,
    paths: Vec<String>,
    strict: bool,
) -> Result<()> {
    let normalized: Vec<String> = paths
        .iter()
        .map(|p| normalize_path(Path::new(p)).map(|p| p.to_string_lossy().to_string()))
        .collect::<Result<Vec<_>>>()?;
    let report = verifier::verify(db, config, confirm, &normalized, mdutil)?;
    println!("Verification outcome: {}", report.outcome.as_str());
    println!("Method: {}", report.method);
    println!(
        "Checked: {}, verified this run: {}",
        report.checked, report.verified
    );
    println!();
    println!("{}", report.message);
    if report.outcome != verifier::VerifyOutcome::Verified {
        println!();
        println!("{}", verifier::manual_confirmation_hint());
    }
    if strict && report.outcome != verifier::VerifyOutcome::Verified {
        std::process::exit(3);
    }
    Ok(())
}

fn cmd_state(paths: &AppPaths, db: &Db, json_output: bool) -> Result<()> {
    let meta = db.all_meta()?;
    let scans = db.recent_scans(8)?;
    let exports = db.recent_exports(5)?;
    let pending = db.count_pending()?;
    let tables = [
        "scan_roots",
        "rules",
        "directories",
        "scans",
        "exports",
        "export_items",
        "verification_runs",
        "event_log",
    ];
    let mut table_counts = BTreeMap::new();
    for table in tables {
        table_counts.insert(table.to_string(), db.table_count(table)?);
    }
    let schema = db.schema_version()?;
    if json_output {
        return json_pretty(&json!({
            "database": db.path().display().to_string(),
            "database_bytes": db.db_size_bytes(),
            "schema_version": schema,
            "meta": meta,
            "table_counts": table_counts,
            "pending": pending,
            "recent_scans": scans,
            "recent_exports": exports,
        }));
    }
    println!("Database: {}", db.path().display());
    println!("Size: {} bytes (main + WAL + SHM)", db.db_size_bytes());
    println!("Schema version: {schema}");
    println!("Pending manual exclusion: {pending}");
    println!("\nMeta:");
    for (key, value) in &meta {
        println!("  {key} = {value}");
    }
    println!("\nTables:");
    for (table, count) in &table_counts {
        println!("  {table}: {count}");
    }
    println!("\nRecent scans:");
    if scans.is_empty() {
        println!("  (none)");
    }
    for scan in &scans {
        println!(
            "  #{} {} at {} finished={} visited={} matched={} new={} missing={} errors={}{}",
            scan.id,
            scan.kind,
            timestamp_to_string(scan.started_at),
            scan.finished_at
                .map(timestamp_to_string)
                .unwrap_or_else(|| "—".into()),
            scan.dirs_visited,
            scan.matched,
            scan.new_dirs,
            scan.missing_dirs,
            scan.errors,
            if scan.interrupted { " INTERRUPTED" } else { "" }
        );
    }
    println!("\nRecent exports:");
    if exports.is_empty() {
        println!("  (none)");
    }
    for export in &exports {
        println!(
            "  #{} {} ({} paths) {}",
            export.id,
            timestamp_to_string(export.created_at),
            export.item_count,
            export.path
        );
    }
    println!("\nData directory: {}", paths.data_dir.display());
    Ok(())
}

fn cmd_retry(db: &mut Db) -> Result<()> {
    let (missing, restored) = scanner::retry_missing(db)?;
    println!(
        "Re-checked {missing} missing director{}; restored {restored}.",
        if missing == 1 { "y" } else { "ies" }
    );
    if let Some(run) = db.latest_verification()? {
        if run.outcome != "verified" {
            println!(
                "The last verification ({}) remains inconclusive. Use `spotlight-dev verify --confirm` after checking System Settings.",
                run.outcome
            );
        }
    }
    Ok(())
}

fn cmd_rescan(
    _paths: &AppPaths,
    config: &Config,
    db: &mut Db,
    matcher: &RuleSet,
    root: Option<String>,
) -> Result<()> {
    let only_root = match root {
        Some(root) => Some(normalize_path(Path::new(&root))?),
        None => None,
    };
    let summary = scanner::full_scan(
        db,
        config,
        matcher,
        ScanKind::Reconcile,
        only_root.as_deref(),
    )?;
    print_scan_summary(&summary);
    Ok(())
}

fn cmd_add_path(
    paths: &AppPaths,
    db: &mut Db,
    matcher: &RuleSet,
    raw: &str,
    no_scan: bool,
) -> Result<()> {
    let normalized = normalize_path(Path::new(raw))?;
    validate_scan_root(&normalized)?;
    let mut config = Config::load_or_init(paths)?;
    let added = config.add_root(&normalized)?;
    config.save(&paths.config_path)?;
    db.sync_roots(&config.root_paths()?)?;
    if !normalized.is_dir() {
        println!(
            "Warning: {} does not exist yet; it will be skipped until created.",
            normalized.display()
        );
    }
    if added {
        println!("Added scan root: {}", display_path(&normalized));
        launchagent::signal_reload();
    } else {
        println!("Already configured: {}", display_path(&normalized));
    }
    if !no_scan && normalized.is_dir() {
        let summary = scanner::full_scan(
            db,
            &config,
            matcher,
            ScanKind::Incremental,
            Some(&normalized),
        )?;
        print_scan_summary(&summary);
    }
    Ok(())
}

fn cmd_remove_path(paths: &AppPaths, db: &mut Db, raw: &str) -> Result<()> {
    let normalized = normalize_path(Path::new(raw))?;
    let mut config = Config::load_or_init(paths)?;
    let removed = config.remove_root(&normalized)?;
    if !removed {
        bail!("not a configured scan root: {}", normalized.display());
    }
    config.save(&paths.config_path)?;
    db.sync_roots(&config.root_paths()?)?;
    launchagent::signal_reload();
    println!(
        "Removed scan root: {}\nProject files and historical database records were not deleted.",
        display_path(&normalized)
    );
    Ok(())
}

fn cmd_mark(db: &mut Db, paths: Vec<String>, all: bool, export_id: Option<i64>) -> Result<()> {
    if all && !paths.is_empty() {
        bail!("specify paths or --all, not both");
    }
    if !all && paths.is_empty() && export_id.is_none() {
        bail!("specify one or more paths, --all, or --export <id>");
    }
    let target = if let Some(id) = export_id {
        MarkTarget::Export(id)
    } else if all {
        MarkTarget::All
    } else {
        let normalized: Vec<String> = paths
            .iter()
            .map(|p| normalize_path(Path::new(p)).map(|p| p.to_string_lossy().to_string()))
            .collect::<Result<Vec<_>>>()?;
        MarkTarget::Paths(normalized)
    };
    let changed = db.mark_applied(&target)?;
    println!(
        "Marked {changed} director{} as manually applied.",
        if changed == 1 { "y" } else { "ies" }
    );
    println!("This records your assertion; it does not verify or modify Spotlight.");
    println!("After visually confirming in System Settings, run: spotlight-dev verify --confirm");
    Ok(())
}

fn cmd_logs(paths: &AppPaths, lines: usize, follow: bool) -> Result<()> {
    let log = latest_log_file(&paths.logs_dir);
    let Some(log) = log else {
        println!(
            "No daemon log found in {}.\nLaunchAgent output (if any):\n  {}/launchd.err.log",
            paths.logs_dir.display(),
            paths.logs_dir.display()
        );
        return Ok(());
    };
    let tail = tail_file(&log, lines)?;
    for line in &tail {
        println!("{line}");
    }
    if follow {
        println!("--- following {} (Ctrl-C to stop) ---", log.display());
        let mut printed = tail.len();
        loop {
            std::thread::sleep(Duration::from_millis(500));
            let all = std::fs::read_to_string(&log).unwrap_or_default();
            let all_lines: Vec<&str> = all.lines().collect();
            if all_lines.len() > printed {
                for line in &all_lines[printed..] {
                    println!("{line}");
                }
                printed = all_lines.len();
            }
        }
    }
    Ok(())
}

fn cmd_config(paths: &AppPaths, cmd: Option<ConfigCommand>) -> Result<()> {
    match cmd.unwrap_or(ConfigCommand::Show) {
        ConfigCommand::Show => {
            let _ = Config::load_or_init(paths)?;
            let text = std::fs::read_to_string(&paths.config_path)?;
            println!("# {}", paths.config_path.display());
            print!("{text}");
            if !text.ends_with('\n') {
                println!();
            }
            Ok(())
        }
        ConfigCommand::Path => {
            println!("{}", paths.config_path.display());
            Ok(())
        }
        ConfigCommand::Init { force } => {
            if paths.config_path.exists() && !force {
                bail!(
                    "configuration already exists: {} (use --force to replace it)",
                    paths.config_path.display()
                );
            }
            if force {
                atomic_write(
                    &paths.config_path,
                    crate::config::DEFAULT_CONFIG.as_bytes(),
                    0o600,
                )?;
            } else {
                let _ = Config::load_or_init(paths)?;
            }
            println!("Configuration ready: {}", paths.config_path.display());
            Ok(())
        }
        ConfigCommand::Set { key, value } => {
            let mut config = Config::load_or_init(paths)?;
            config.set_key(&key, &value)?;
            config.save(&paths.config_path)?;
            // Keep the database's root/rule mirror in sync when it exists.
            if paths.db_path.exists() {
                let mut db = Db::open(paths)?;
                db.sync_roots(&config.root_paths()?)?;
                let matcher = RuleSet::from_config(&config.rules)?;
                db.sync_rules(matcher.all_rules())?;
            }
            launchagent::signal_reload();
            println!("{key} = {value}");
            println!("Configuration saved: {}", paths.config_path.display());
            Ok(())
        }
        ConfigCommand::Edit => {
            let _ = Config::load_or_init(paths)?;
            let editor = std::env::var("VISUAL")
                .or_else(|_| std::env::var("EDITOR"))
                .ok();
            match editor {
                Some(editor) => {
                    let status = std::process::Command::new(&editor)
                        .arg(&paths.config_path)
                        .status()
                        .with_context(|| format!("launching editor {editor}"))?;
                    if !status.success() {
                        bail!("editor exited with {status}");
                    }
                }
                None => {
                    let status = std::process::Command::new("/usr/bin/open")
                        .arg("-W")
                        .arg("-e")
                        .arg(&paths.config_path)
                        .status()
                        .context("opening configuration in TextEdit (set $EDITOR to override)")?;
                    if !status.success() {
                        bail!("could not open the configuration editor");
                    }
                }
            }
            let config = Config::load(&paths.config_path)?;
            config.validate()?;
            launchagent::signal_reload();
            println!("Configuration is valid: {}", paths.config_path.display());
            println!("A running service was asked to reload; `service restart` is the fallback.");
            Ok(())
        }
        ConfigCommand::Reload => {
            launchagent::signal_reload();
            println!("Requested a configuration reload from the running service.");
            println!("If it is not running, this is a no-op; start it with `service start`.");
            Ok(())
        }
    }
}

fn cmd_service(paths: &AppPaths, cmd: ServiceCommand) -> Result<()> {
    match cmd {
        ServiceCommand::Install { binary } => {
            let _ = Config::load_or_init(paths)?;
            let _ = Db::open(paths)?;
            let binary = match binary {
                Some(binary) => normalize_path(&binary)?,
                None => std::env::current_exe()
                    .context("resolving current executable")?
                    .canonicalize()
                    .context("canonicalising current executable")?,
            };
            let plist = launchagent::install(&binary, paths, &paths.config_path)?;
            println!("Installed LaunchAgent: {}", plist.display());
            println!("Executable: {}", binary.display());
            println!("The service runs as your user, not root.");
            if let Ok(status) = launchagent::status(paths) {
                println!(
                    "State: {}",
                    if status.running {
                        "running"
                    } else {
                        status.detail.as_str()
                    }
                );
            }
            Ok(())
        }
        ServiceCommand::Status { json } => {
            let status = launchagent::status(paths)?;
            if json {
                return json_pretty(&status);
            }
            println!("LaunchAgent: {}", status.plist);
            println!("Installed: {}", status.installed);
            println!("Loaded:    {}", status.loaded);
            println!("Running:   {}", status.running);
            if let Some(pid) = status.pid {
                println!("PID:       {pid}");
            }
            println!("Detail:    {}", status.detail);
            Ok(())
        }
        ServiceCommand::Start => {
            launchagent::start(paths)?;
            println!("LaunchAgent start requested.");
            Ok(())
        }
        ServiceCommand::Stop => {
            launchagent::stop()?;
            println!("LaunchAgent stopped (plist preserved).");
            Ok(())
        }
        ServiceCommand::Restart => {
            launchagent::restart(paths)?;
            println!("LaunchAgent restarted.");
            Ok(())
        }
        ServiceCommand::Uninstall => {
            let removed = launchagent::uninstall(paths)?;
            println!(
                "{}",
                if removed {
                    "LaunchAgent removed. Application data was preserved."
                } else {
                    "LaunchAgent was not installed."
                }
            );
            Ok(())
        }
    }
}

fn cmd_uninstall(paths: &AppPaths, purge_data: bool, remove_binary: bool, yes: bool) -> Result<()> {
    if purge_data && !yes {
        bail!("--purge-data is destructive; re-run with --purge-data --yes");
    }
    if let Err(error) = launchagent::uninstall(paths) {
        eprintln!("warning: could not remove LaunchAgent: {error:#}");
    } else {
        println!("LaunchAgent stopped and removed (if it was installed).");
    }

    if remove_binary {
        let exe = std::env::current_exe().context("resolving current executable")?;
        match std::fs::remove_file(&exe) {
            Ok(()) => println!("Removed executable: {}", exe.display()),
            Err(error) => eprintln!("warning: could not remove {}: {error}", exe.display()),
        }
    }

    if purge_data {
        let data = paths.data_dir.clone();
        let home = home_dir()?;
        if data == home || data == Path::new("/") {
            bail!(
                "refusing to delete unsafe data directory {}",
                data.display()
            );
        }
        if data.exists() {
            std::fs::remove_dir_all(&data)
                .with_context(|| format!("removing {}", data.display()))?;
            println!("Removed application data: {}", data.display());
        }
        if paths.logs_dir.exists() && !paths.logs_dir.starts_with(&data) {
            let _ = std::fs::remove_dir_all(&paths.logs_dir);
            println!("Removed logs: {}", paths.logs_dir.display());
        }
    } else {
        println!(
            "Application data preserved: {}\nUse `--purge-data --yes` to remove it.",
            paths.data_dir.display()
        );
    }
    println!(
        "The source repository, project files and Spotlight Search Privacy settings were not touched."
    );
    Ok(())
}
