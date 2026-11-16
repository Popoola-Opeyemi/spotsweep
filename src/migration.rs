//! Safe migration from the old SpotlightDevExclusions layout to SpotSweep.
//!
//! The migration is deliberately conservative: it never overwrites a populated
//! new directory, validates the copied SQLite database before switching, keeps
//! the old directory as a timestamped backup, and records an in-progress marker
//! so a crash between the two directory renames can be recovered.

use crate::database::Db;
use crate::paths::{AppPaths, DB_FILENAME, LEGACY_DB_FILENAME};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const MIGRATION_MARKER: &str = ".spotsweep-migration.json";
const STAGING_PREFIX: &str = ".spotsweep-migration-";

#[derive(Debug, Clone, Serialize)]
pub struct MigrationReport {
    pub old_data: PathBuf,
    pub new_data: PathBuf,
    pub backup: Option<PathBuf>,
    pub logs_copied: bool,
    pub logs_note: Option<String>,
    pub recovered_interrupted: bool,
}

#[derive(Debug, Clone, Serialize)]
pub enum MigrationOutcome {
    None,
    Migrated(MigrationReport),
    Recovered(MigrationReport),
    Conflict {
        old_data: PathBuf,
        new_data: PathBuf,
    },
}

impl MigrationOutcome {
    pub fn is_noop(&self) -> bool {
        matches!(self, MigrationOutcome::None)
    }

    pub fn summary(&self) -> String {
        match self {
            MigrationOutcome::None => "no legacy data to migrate".into(),
            MigrationOutcome::Migrated(report) => format!(
                "migrated legacy data from {} to {}{}",
                report.old_data.display(),
                report.new_data.display(),
                report
                    .backup
                    .as_ref()
                    .map(|path| format!("; backup kept at {}", path.display()))
                    .unwrap_or_default()
            ),
            MigrationOutcome::Recovered(report) => format!(
                "recovered an interrupted migration into {}; backup: {}",
                report.new_data.display(),
                report
                    .backup
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "none".into())
            ),
            MigrationOutcome::Conflict { old_data, new_data } => format!(
                "both legacy and SpotSweep data exist ({} and {}); nothing was changed",
                old_data.display(),
                new_data.display()
            ),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Marker {
    source: String,
    backup: String,
    created_at: i64,
}

/// Explicit paths used by the migration implementation and its tests.
#[derive(Debug, Clone)]
pub struct MigrationPaths {
    pub old_data: PathBuf,
    pub new_data: PathBuf,
    pub old_db: PathBuf,
    pub new_db: PathBuf,
    pub old_logs: PathBuf,
    pub new_logs: PathBuf,
}

impl MigrationPaths {
    pub fn from_app_paths(paths: &AppPaths) -> Option<Self> {
        let old_data = paths.legacy_data_dir.clone()?;
        let old_logs = paths.legacy_logs_dir.clone()?;
        Some(Self {
            old_db: paths
                .legacy_db_path
                .clone()
                .unwrap_or_else(|| old_data.join(LEGACY_DB_FILENAME)),
            new_data: paths.data_dir.clone(),
            new_db: paths.db_path.clone(),
            old_logs,
            new_logs: paths.logs_dir.clone(),
            old_data,
        })
    }
}

pub fn migrate_if_needed(paths: &AppPaths) -> Result<MigrationOutcome> {
    let Some(migration) = MigrationPaths::from_app_paths(paths) else {
        return Ok(MigrationOutcome::None);
    };
    if has_meaningful_data(&migration.old_data) {
        let legacy_agent_present = paths
            .legacy_plist_path
            .as_ref()
            .map(|path| path.exists())
            .unwrap_or(false);
        if legacy_agent_present {
            // The old daemon may have the SQLite WAL open.  Stop it before
            // copying so the migration sees a clean, checkpointed database.
            crate::launchagent::stop_legacy_agent();
            for _ in 0..30 {
                if !crate::launchagent::legacy_agent_loaded() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }
    migrate_paths(&migration)
}

pub fn migrate_paths(migration: &MigrationPaths) -> Result<MigrationOutcome> {
    let old_marker = migration.new_data.join(MIGRATION_MARKER);
    if old_marker.exists() {
        return recover_interrupted(migration, &old_marker);
    }

    if !has_meaningful_data(&migration.old_data) {
        return Ok(MigrationOutcome::None);
    }
    if has_significant_entries(&migration.new_data) {
        return Ok(MigrationOutcome::Conflict {
            old_data: migration.old_data.clone(),
            new_data: migration.new_data.clone(),
        });
    }

    let parent = migration
        .new_data
        .parent()
        .context("new data directory has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    cleanup_stale_staging(parent);

    let staging = unique_staging_path(parent);
    if staging.exists() {
        bail!(
            "migration staging directory already exists: {}",
            staging.display()
        );
    }
    copy_tree(&migration.old_data, &staging)
        .with_context(|| format!("copying {} to staging", migration.old_data.display()))?;

    let staged_db = staging.join(DB_FILENAME);
    if migration.old_db.is_file() {
        let staged_old_db = staging.join(LEGACY_DB_FILENAME);
        if !staged_old_db.is_file() {
            let _ = fs::remove_dir_all(&staging);
            bail!(
                "legacy database was not copied into staging ({})",
                staged_old_db.display()
            );
        }
        if staged_db.exists() {
            let _ = fs::remove_dir_all(&staging);
            bail!(
                "staging already contains {}; refusing to overwrite it",
                staged_db.display()
            );
        }
        fs::rename(&staged_old_db, &staged_db)
            .context("renaming legacy database inside staging")?;
        // WAL/SHM/journal files must follow the database name.  Leaving them
        // named state.sqlite3-wal would make the copied database look older or
        // inconsistent to SQLite.
        for suffix in ["-wal", "-shm", "-journal"] {
            let old_sidecar = staging.join(format!("{LEGACY_DB_FILENAME}{suffix}"));
            if old_sidecar.exists() {
                let new_sidecar = staging.join(format!("{DB_FILENAME}{suffix}"));
                fs::rename(&old_sidecar, &new_sidecar).with_context(|| {
                    format!("renaming legacy database sidecar {suffix} inside staging")
                })?;
            }
        }
        if let Err(error) = Db::validate_migration_db(&staged_db) {
            let _ = fs::remove_dir_all(&staging);
            return Err(error.context("the legacy database failed validation"));
        }
    }

    let backup = unique_backup_path(&migration.old_data);
    let marker = Marker {
        source: migration.old_data.to_string_lossy().into_owned(),
        backup: backup.to_string_lossy().into_owned(),
        created_at: now_ms(),
    };
    write_marker(&staging.join(MIGRATION_MARKER), &marker)?;

    if migration.new_data.exists() {
        // It existed but had no significant entries.  Remove it only after the
        // staged copy has been created and validated.
        remove_insignificant_entries(&migration.new_data)?;
        fs::remove_dir(&migration.new_data).with_context(|| {
            format!(
                "removing empty new data directory {}",
                migration.new_data.display()
            )
        })?;
    }
    fs::rename(&staging, &migration.new_data)
        .with_context(|| format!("installing staged data at {}", migration.new_data.display()))?;

    if let Err(error) = fs::rename(&migration.old_data, &backup) {
        // Keep the marker for a later recovery attempt; do not delete either
        // copy.  The user can restore by moving the backup or the new data.
        return Err(error).with_context(|| {
            format!(
                "could not move old data to backup {}; migration marker kept at {}",
                backup.display(),
                migration.new_data.join(MIGRATION_MARKER).display()
            )
        });
    }

    let _ = fs::remove_file(migration.new_data.join(MIGRATION_MARKER));
    let (logs_copied, logs_note) = copy_legacy_logs(&migration.old_logs, &migration.new_logs);
    Ok(MigrationOutcome::Migrated(MigrationReport {
        old_data: migration.old_data.clone(),
        new_data: migration.new_data.clone(),
        backup: Some(backup),
        logs_copied,
        logs_note,
        recovered_interrupted: false,
    }))
}

fn recover_interrupted(migration: &MigrationPaths, marker_path: &Path) -> Result<MigrationOutcome> {
    let marker: Marker = serde_json::from_slice(
        &fs::read(marker_path)
            .with_context(|| format!("reading migration marker {}", marker_path.display()))?,
    )
    .context("parsing migration marker")?;
    if migration.new_db.is_file() {
        Db::validate_migration_db(&migration.new_db)
            .context("the migrated database failed validation during recovery")?;
    }
    if migration.old_data.exists() {
        let backup = PathBuf::from(&marker.backup);
        if backup.exists() {
            return Ok(MigrationOutcome::Conflict {
                old_data: migration.old_data.clone(),
                new_data: migration.new_data.clone(),
            });
        }
        fs::rename(&migration.old_data, &backup).with_context(|| {
            format!(
                "recovery could not move {} to {}",
                migration.old_data.display(),
                backup.display()
            )
        })?;
    }
    let _ = fs::remove_file(marker_path);
    let (logs_copied, logs_note) = copy_legacy_logs(&migration.old_logs, &migration.new_logs);
    Ok(MigrationOutcome::Recovered(MigrationReport {
        old_data: migration.old_data.clone(),
        new_data: migration.new_data.clone(),
        backup: Some(PathBuf::from(marker.backup)),
        logs_copied,
        logs_note,
        recovered_interrupted: true,
    }))
}

fn write_marker(path: &Path, marker: &Marker) -> Result<()> {
    let data = serde_json::to_vec_pretty(marker)?;
    fs::write(path, data).with_context(|| format!("writing marker {}", path.display()))?;
    let permissions = fs::Permissions::from_mode(0o600);
    let _ = fs::set_permissions(path, permissions);
    Ok(())
}

fn has_meaningful_data(data_dir: &Path) -> bool {
    if !data_dir.is_dir() {
        return false;
    }
    for entry in [LEGACY_DB_FILENAME, DB_FILENAME, "config.toml", "exports"] {
        let path = data_dir.join(entry);
        if path.is_file() {
            return true;
        }
        if path.is_dir() && has_significant_entries(&path) {
            return true;
        }
    }
    has_significant_entries(data_dir)
}

fn has_significant_entries(path: &Path) -> bool {
    let Ok(entries) = fs::read_dir(path) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let name = entry.file_name();
        name != MIGRATION_MARKER && name != ".DS_Store"
    })
}

fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    let metadata =
        fs::symlink_metadata(source).with_context(|| format!("reading {}", source.display()))?;
    if metadata.file_type().is_symlink() {
        let target = fs::read_link(source)?;
        symlink(target, destination)
            .with_context(|| format!("recreating symlink {}", destination.display()))?;
        return Ok(());
    }
    if metadata.is_dir() {
        fs::create_dir_all(destination)?;
        fs::set_permissions(
            destination,
            fs::Permissions::from_mode(metadata.permissions().mode()),
        )?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            copy_tree(&entry.path(), &destination.join(entry.file_name()))?;
        }
        return Ok(());
    }
    if metadata.is_file() {
        fs::copy(source, destination).with_context(|| {
            format!("copying {} to {}", source.display(), destination.display())
        })?;
        fs::set_permissions(
            destination,
            fs::Permissions::from_mode(metadata.permissions().mode()),
        )?;
        return Ok(());
    }
    bail!(
        "refusing to migrate unsupported file type: {}",
        source.display()
    )
}

fn copy_legacy_logs(old_logs: &Path, new_logs: &Path) -> (bool, Option<String>) {
    if !old_logs.is_dir() {
        return (false, None);
    }
    if new_logs.exists() && has_significant_entries(new_logs) {
        return (
            false,
            Some(format!(
                "new log directory already has files; old logs left at {}",
                old_logs.display()
            )),
        );
    }
    let result = (|| -> Result<()> {
        if new_logs.exists() {
            remove_insignificant_entries(new_logs)?;
            fs::remove_dir_all(new_logs)?;
        }
        copy_tree(old_logs, new_logs)?;
        Ok(())
    })();
    match result {
        Ok(()) => (true, None),
        Err(error) => (
            false,
            Some(format!(
                "could not copy old logs from {}: {error}",
                old_logs.display()
            )),
        ),
    }
}

fn unique_staging_path(parent: &Path) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    parent.join(format!("{STAGING_PREFIX}{}-{nanos:x}", std::process::id()))
}

fn unique_backup_path(old_data: &Path) -> PathBuf {
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let base = old_data
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "SpotlightDevExclusions".into());
    let parent = old_data.parent().unwrap_or_else(|| Path::new("."));
    let mut candidate = parent.join(format!("{base}.backup-{stamp}"));
    let mut suffix = 1;
    while candidate.exists() {
        candidate = parent.join(format!("{base}.backup-{stamp}-{suffix}"));
        suffix += 1;
    }
    candidate
}

fn cleanup_stale_staging(parent: &Path) {
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with(STAGING_PREFIX) {
            let path = entry.path();
            // Do not touch a concurrent migration's fresh staging directory.
            // An old staging directory is safe to remove because the source
            // data has not been moved yet.
            let old_enough = entry
                .metadata()
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .and_then(|modified| modified.elapsed().ok())
                .map(|age| age.as_secs() > 3600)
                .unwrap_or(false);
            if old_enough {
                let _ = fs::remove_dir_all(path);
            }
        }
    }
}

fn remove_insignificant_entries(path: &Path) -> Result<()> {
    if !path.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_name() == ".DS_Store" {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Db;
    use crate::model::RuleKind;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    fn migration(temp: &TempDir) -> MigrationPaths {
        let old = temp.path().join("SpotlightDevExclusions");
        let new = temp.path().join("SpotSweep");
        MigrationPaths {
            old_db: old.join(LEGACY_DB_FILENAME),
            new_db: new.join(DB_FILENAME),
            old_logs: temp.path().join("LogsOld"),
            new_logs: temp.path().join("LogsNew"),
            old_data: old,
            new_data: new,
        }
    }

    fn seed_old(paths: &MigrationPaths) -> i64 {
        fs::create_dir_all(&paths.old_data).unwrap();
        let mut db = Db::open_path(&paths.old_db).unwrap();
        db.sync_roots(&[PathBuf::from("/Users/a/Projects")])
            .unwrap();
        let root = db.enabled_roots().unwrap().remove(0);
        db.upsert_directories(
            &[crate::model::NewDirectory {
                path: "/Users/a/Projects/app/node_modules".into(),
                name: "node_modules".into(),
                rule: "node_modules".into(),
                rule_kind: RuleKind::Dependencies,
                scan_root_id: root.id,
                project_root: "/Users/a/Projects/app".into(),
            }],
            now_ms(),
        )
        .unwrap();
        db.promote_discovered().unwrap();
        db.count_all().unwrap()
    }

    #[test]
    fn migrates_database_config_and_preserves_rows() {
        let temp = TempDir::new().unwrap();
        let paths = migration(&temp);
        let count = seed_old(&paths);
        fs::write(paths.old_data.join("config.toml"), "scan = {}\n").unwrap();
        fs::create_dir_all(paths.old_data.join("exports")).unwrap();
        fs::write(paths.old_data.join("exports/old.txt"), "old export\n").unwrap();
        fs::set_permissions(paths.old_db.clone(), fs::Permissions::from_mode(0o600)).unwrap();

        let outcome = migrate_paths(&paths).unwrap();
        let MigrationOutcome::Migrated(report) = outcome else {
            panic!("unexpected outcome: {outcome:?}");
        };
        assert!(paths.new_db.is_file());
        assert!(!paths.old_data.exists());
        assert!(report.backup.as_ref().unwrap().is_dir());
        assert!(
            report
                .backup
                .as_ref()
                .unwrap()
                .join(LEGACY_DB_FILENAME)
                .is_file()
        );
        assert_eq!(
            fs::read_to_string(paths.new_data.join("exports/old.txt")).unwrap(),
            "old export\n"
        );
        let db = Db::open_path(&paths.new_db).unwrap();
        assert_eq!(db.count_all().unwrap(), count);
        assert_eq!(
            fs::metadata(&paths.new_db).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn wal_sidecars_follow_the_database_name() {
        let temp = TempDir::new().unwrap();
        let paths = migration(&temp);
        let mut db = Db::open_path(&paths.old_db).unwrap();
        db.sync_roots(&[PathBuf::from("/Users/a/Projects")])
            .unwrap();
        drop(db);

        // Leave a WAL behind with a recent write that has not been checkpointed.
        let conn = rusqlite::Connection::open(&paths.old_db).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.execute(
            "INSERT INTO meta (key, value) VALUES ('wal-test', 'present')
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [],
        )
        .unwrap();
        assert!(paths.old_data.join("state.sqlite3-wal").exists());
        assert!(matches!(
            migrate_paths(&paths).unwrap(),
            MigrationOutcome::Migrated(_)
        ));
        drop(conn);

        let db = Db::open_path(&paths.new_db).unwrap();
        assert_eq!(db.meta_get("wal-test").unwrap().as_deref(), Some("present"));
    }

    #[test]
    fn an_empty_new_directory_with_ds_store_is_used() {
        let temp = TempDir::new().unwrap();
        let paths = migration(&temp);
        seed_old(&paths);
        fs::create_dir_all(&paths.new_data).unwrap();
        fs::write(paths.new_data.join(".DS_Store"), b"finder").unwrap();
        assert!(matches!(
            migrate_paths(&paths).unwrap(),
            MigrationOutcome::Migrated(_)
        ));
        assert!(paths.new_db.is_file());
    }

    #[test]
    fn migration_is_idempotent() {
        let temp = TempDir::new().unwrap();
        let paths = migration(&temp);
        seed_old(&paths);
        assert!(matches!(
            migrate_paths(&paths).unwrap(),
            MigrationOutcome::Migrated(_)
        ));
        assert!(matches!(
            migrate_paths(&paths).unwrap(),
            MigrationOutcome::None
        ));
    }

    #[test]
    fn conflict_leaves_both_directories_untouched() {
        let temp = TempDir::new().unwrap();
        let paths = migration(&temp);
        seed_old(&paths);
        fs::create_dir_all(&paths.new_data).unwrap();
        let new_db = Db::open_path(&paths.new_db).unwrap();
        new_db.meta_set("new", "yes").unwrap();
        let old_before = fs::read(&paths.old_db).unwrap();
        let new_before = fs::read(&paths.new_db).unwrap();

        assert!(matches!(
            migrate_paths(&paths).unwrap(),
            MigrationOutcome::Conflict { .. }
        ));
        assert_eq!(fs::read(&paths.old_db).unwrap(), old_before);
        assert_eq!(fs::read(&paths.new_db).unwrap(), new_before);
    }

    #[test]
    fn corrupt_database_is_not_migrated() {
        let temp = TempDir::new().unwrap();
        let paths = migration(&temp);
        fs::create_dir_all(&paths.old_data).unwrap();
        fs::write(&paths.old_db, b"not sqlite").unwrap();
        let error = migrate_paths(&paths).unwrap_err();
        assert!(
            format!("{error:#}").contains("validation")
                || format!("{error:#}").contains("integrity")
                || format!("{error:#}").contains("file is not a database")
        );
        assert!(paths.old_data.exists());
        assert!(!paths.new_data.exists());
    }

    #[test]
    fn interrupted_marker_is_recovered() {
        let temp = TempDir::new().unwrap();
        let paths = migration(&temp);
        seed_old(&paths);
        let backup = temp.path().join("SpotlightDevExclusions.backup-test");
        fs::create_dir_all(&paths.new_data).unwrap();
        fs::copy(&paths.old_db, &paths.new_db).unwrap();
        let marker = Marker {
            source: paths.old_data.to_string_lossy().into_owned(),
            backup: backup.to_string_lossy().into_owned(),
            created_at: now_ms(),
        };
        write_marker(&paths.new_data.join(MIGRATION_MARKER), &marker).unwrap();

        assert!(matches!(
            migrate_paths(&paths).unwrap(),
            MigrationOutcome::Recovered(_)
        ));
        assert!(!paths.old_data.exists());
        assert!(backup.is_dir());
        assert!(!paths.new_data.join(MIGRATION_MARKER).exists());
    }

    #[test]
    fn logs_are_copied_when_new_log_directory_is_empty() {
        let temp = TempDir::new().unwrap();
        let paths = migration(&temp);
        seed_old(&paths);
        fs::create_dir_all(&paths.old_logs).unwrap();
        fs::write(paths.old_logs.join("daemon.log"), "log\n").unwrap();
        let outcome = migrate_paths(&paths).unwrap();
        let MigrationOutcome::Migrated(report) = outcome else {
            panic!("unexpected outcome");
        };
        assert!(report.logs_copied);
        assert_eq!(
            fs::read_to_string(paths.new_logs.join("daemon.log")).unwrap(),
            "log\n"
        );
    }
}
