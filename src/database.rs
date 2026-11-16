//! SQLite persistence.
//!
//! One connection per process, WAL mode, short transactions and small cache.
//! All writes that reflect a filesystem discovery are batched by the scanner.

use crate::model::{
    DirectoryRecord, ExclusionRecord, ExclusionSource, ExclusionState, NewDirectory, RootRecord,
    ScanKind, ScanSession, ScanSummary, Status,
};
use crate::paths::AppPaths;
use crate::util::now_ms;
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

const SCHEMA_VERSION: i64 = 2;
const MIGRATION_001: &str = include_str!("../migrations/001_init.sql");
const MIGRATION_002: &str = include_str!("../migrations/002_exclusions.sql");

const DIRECTORY_COLUMNS: &str = "id, path, name, rule, rule_kind, scan_root_id, project_root, \
     first_seen_at, last_seen_at, status, status_before_missing, last_export_id, exported_at, \
     manually_applied_at, verified_at, missing_at, retry_count, last_error, \
     (SELECT state FROM exclusions WHERE exclusions.path = directories.path) AS exclusion_state, \
     (SELECT managed FROM exclusions WHERE exclusions.path = directories.path) AS exclusion_managed, \
     (SELECT verified_at FROM exclusions WHERE exclusions.path = directories.path) AS exclusion_verified_at, \
     (SELECT error FROM exclusions WHERE exclusions.path = directories.path) AS exclusion_error";

#[derive(Debug, Default, Clone)]
pub struct UpsertStats {
    pub new_dirs: usize,
    pub revived: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScanRecord {
    pub id: i64,
    pub kind: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub roots_scanned: i64,
    pub dirs_visited: i64,
    pub matched: i64,
    pub new_dirs: i64,
    pub missing_dirs: i64,
    pub errors: i64,
    pub interrupted: bool,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExportRecord {
    pub id: i64,
    pub created_at: i64,
    pub path: String,
    pub selection: String,
    pub item_count: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct VerificationRecord {
    pub id: i64,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub method: String,
    pub outcome: String,
    pub message: Option<String>,
    pub checked_count: i64,
    pub verified_count: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportSelection {
    Pending,
    All,
    Rule,
}

#[derive(Debug, Clone, Default)]
pub struct DirectoryFilter {
    pub statuses: Vec<Status>,
    pub rule: Option<String>,
    pub root: Option<String>,
    pub project: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone)]
pub enum MarkTarget {
    All,
    Paths(Vec<String>),
    Export(i64),
}

pub struct Db {
    conn: Connection,
    path: PathBuf,
}

impl Db {
    pub fn open(paths: &AppPaths) -> Result<Self> {
        Self::open_path(&paths.db_path)
    }

    pub fn open_path(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating database directory {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening SQLite database {}", path.display()))?;
        Self::from_connection(conn, path.to_path_buf())
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::from_connection(conn, PathBuf::from(":memory:"))
    }

    fn from_connection(conn: Connection, path: PathBuf) -> Result<Self> {
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA foreign_keys=ON;
             PRAGMA temp_store=MEMORY;
             PRAGMA cache_size=-2000;
             PRAGMA mmap_size=0;
             PRAGMA wal_autocheckpoint=1000;",
        )
        .context("configuring SQLite")?;
        let mut db = Self { conn, path };
        db.migrate()?;
        db.recover_interrupted()?;
        Ok(db)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn schema_version(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))?)
    }

    fn migrate(&mut self) -> Result<()> {
        let mut version: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version > SCHEMA_VERSION {
            bail!(
                "database schema version {version} is newer than this executable supports ({SCHEMA_VERSION}); upgrade the application"
            );
        }
        if version == 0 {
            let tx = self.conn.transaction()?;
            tx.execute_batch(MIGRATION_001)
                .context("running schema migration 001")?;
            tx.pragma_update(None, "user_version", 1)?;
            tx.commit()?;
            version = 1;
        }
        if version == 1 {
            let tx = self.conn.transaction()?;
            tx.execute_batch(MIGRATION_002)
                .context("running schema migration 002")?;
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            tx.commit()?;
        }
        Ok(())
    }

    /// Any scan row without a finish time belongs to a process that exited
    /// abruptly.  Mark it interrupted and promote rows left in the transient
    /// `discovered` state so the next scan/pending view is coherent.
    pub fn recover_interrupted(&mut self) -> Result<usize> {
        let now = now_ms();
        let changed = self.conn.execute(
            "UPDATE scans SET finished_at = ?1, interrupted = 1
             WHERE finished_at IS NULL",
            params![now],
        )?;
        self.conn.execute(
            "UPDATE directories SET status = 'pending_manual_exclusion'
             WHERE status = 'discovered'",
            [],
        )?;
        Ok(changed)
    }

    pub fn meta_get(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn meta_get_i64(&self, key: &str) -> Result<Option<i64>> {
        Ok(self
            .meta_get(key)?
            .and_then(|value| value.parse::<i64>().ok()))
    }

    pub fn meta_get_u64(&self, key: &str) -> Result<Option<u64>> {
        Ok(self
            .meta_get(key)?
            .and_then(|value| value.parse::<u64>().ok()))
    }

    pub fn meta_set(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value
             WHERE meta.value <> excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn meta_set_i64(&self, key: &str, value: i64) -> Result<()> {
        self.meta_set(key, &value.to_string())
    }

    pub fn meta_set_u64(&self, key: &str, value: u64) -> Result<()> {
        self.meta_set(key, &value.to_string())
    }

    pub fn all_meta(&self) -> Result<Vec<(String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT key, value FROM meta ORDER BY key")?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Synchronise configured roots with the database.  Roots removed from the
    /// config are disabled, never deleted, so their history remains available.
    pub fn sync_roots(&mut self, roots: &[PathBuf]) -> Result<()> {
        let now = now_ms();
        let tx = self.conn.transaction()?;
        for root in roots {
            tx.execute(
                "INSERT INTO scan_roots (path, enabled, created_at)
                 VALUES (?1, 1, ?2)
                 ON CONFLICT(path) DO UPDATE SET enabled = 1
                 WHERE scan_roots.enabled = 0",
                params![root.to_string_lossy(), now],
            )?;
        }
        let mut stmt = tx.prepare("SELECT id, path FROM scan_roots WHERE enabled = 1")?;
        let enabled: Vec<(i64, String)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        drop(stmt);
        for (id, path) in enabled {
            let configured = roots.iter().any(|root| root.to_string_lossy() == path);
            if !configured {
                tx.execute(
                    "UPDATE scan_roots SET enabled = 0 WHERE id = ?1",
                    params![id],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn sync_rules(&mut self, rules: &[crate::matcher::RuleMatch]) -> Result<()> {
        let mut existing: Vec<(String, String)> = {
            let mut stmt = self.conn.prepare("SELECT pattern, kind FROM rules")?;
            let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
            rows.collect::<std::result::Result<_, _>>()?
        };
        existing.sort();
        let mut wanted: Vec<(String, String)> = rules
            .iter()
            .map(|rule| (rule.pattern.clone(), rule.kind.as_str().to_string()))
            .collect();
        wanted.sort();
        if existing == wanted {
            return Ok(());
        }
        let now = now_ms();
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM rules", [])?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO rules (pattern, kind, enabled, source, updated_at)
                 VALUES (?1, ?2, 1, 'config', ?3)",
            )?;
            for rule in rules {
                stmt.execute(params![rule.pattern, rule.kind.as_str(), now])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn enabled_roots(&self) -> Result<Vec<RootRecord>> {
        self.roots(true)
    }

    pub fn all_roots(&self) -> Result<Vec<RootRecord>> {
        self.roots(false)
    }

    fn roots(&self, enabled_only: bool) -> Result<Vec<RootRecord>> {
        let sql = if enabled_only {
            "SELECT id, path, enabled, created_at, last_scanned_at, last_scan_ok
             FROM scan_roots WHERE enabled = 1 ORDER BY path"
        } else {
            "SELECT id, path, enabled, created_at, last_scanned_at, last_scan_ok
             FROM scan_roots ORDER BY path"
        };
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map([], |row| {
            Ok(RootRecord {
                id: row.get(0)?,
                path: row.get(1)?,
                enabled: row.get::<_, i64>(2)? != 0,
                created_at: row.get(3)?,
                last_scanned_at: row.get(4)?,
                last_scan_ok: row.get::<_, Option<i64>>(5)?.map(|v| v != 0),
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn mark_root_scanned(&self, root_id: i64, ok: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE scan_roots SET last_scanned_at = ?1, last_scan_ok = ?2 WHERE id = ?3",
            params![now_ms(), i64::from(ok), root_id],
        )?;
        Ok(())
    }

    pub fn start_scan(&mut self, kind: ScanKind, roots_configured: usize) -> Result<ScanSession> {
        let started_at = now_ms();
        self.conn.execute(
            "INSERT INTO scans (kind, started_at, roots_configured)
             VALUES (?1, ?2, ?3)",
            params![kind.as_str(), started_at, roots_configured as i64],
        )?;
        let id = self.conn.last_insert_rowid();
        self.meta_set("last_scan_started_at", &started_at.to_string())?;
        Ok(ScanSession {
            id,
            started_at,
            kind,
        })
    }

    pub fn finish_scan(&mut self, session: &ScanSession, summary: &ScanSummary) -> Result<()> {
        let now = now_ms();
        self.conn.execute(
            "UPDATE scans SET finished_at = ?1, roots_scanned = ?2, roots_missing = ?3,
                dirs_visited = ?4, matched = ?5, new_dirs = ?6, missing_dirs = ?7,
                errors = ?8, interrupted = ?9
             WHERE id = ?10",
            params![
                now,
                summary.roots_scanned as i64,
                summary.roots_missing as i64,
                summary.dirs_visited as i64,
                summary.matched as i64,
                summary.new_dirs as i64,
                summary.missing_dirs as i64,
                summary.errors as i64,
                i64::from(summary.interrupted),
                session.id
            ],
        )?;
        self.promote_discovered()?;
        self.meta_set("last_scan_at", &now.to_string())?;
        self.meta_set("last_scan_kind", session.kind.as_str())?;
        if matches!(session.kind, ScanKind::Initial | ScanKind::Reconcile) {
            self.meta_set("last_reconcile_at", &now.to_string())?;
        }
        Ok(())
    }

    pub fn promote_discovered(&mut self) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE directories SET status = 'pending_manual_exclusion'
             WHERE status = 'discovered'",
            [],
        )?)
    }

    /// Insert/refresh matched directories.  `seen_at` is the scan's start time;
    /// a directory whose `first_seen_at` equals it is new in this scan.
    pub fn upsert_directories(
        &mut self,
        entries: &[NewDirectory],
        seen_at: i64,
    ) -> Result<UpsertStats> {
        if entries.is_empty() {
            return Ok(UpsertStats::default());
        }
        let tx = self.conn.transaction()?;
        let mut stats = UpsertStats::default();
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO directories
                    (path, name, rule, rule_kind, scan_root_id, project_root,
                     first_seen_at, last_seen_at, status)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, 'discovered')
                 ON CONFLICT(path) DO UPDATE SET
                    name = excluded.name,
                    rule = excluded.rule,
                    rule_kind = excluded.rule_kind,
                    scan_root_id = excluded.scan_root_id,
                    project_root = excluded.project_root,
                    last_seen_at = MAX(directories.last_seen_at, excluded.last_seen_at),
                    status = CASE WHEN directories.status = 'missing'
                                  THEN 'discovered' ELSE directories.status END,
                    missing_at = NULL,
                    last_error = NULL
                 RETURNING first_seen_at, status",
            )?;
            for entry in entries {
                let (first_seen, status): (i64, String) = stmt.query_row(
                    params![
                        entry.path,
                        entry.name,
                        entry.rule,
                        entry.rule_kind.as_str(),
                        entry.scan_root_id,
                        entry.project_root,
                        seen_at
                    ],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                if first_seen == seen_at {
                    stats.new_dirs += 1;
                } else if status == Status::Discovered.as_str() {
                    stats.revived += 1;
                }
            }
        }
        tx.commit()?;
        Ok(stats)
    }

    /// Mark everything under a root that this completed scan did not observe.
    /// Prefix matching handles overlapping roots correctly.
    pub fn mark_missing_for_root(
        &mut self,
        root_path: &str,
        scan_started_at: i64,
    ) -> Result<usize> {
        let now = now_ms();
        let changed = self.conn.execute(
            "UPDATE directories
             SET status = 'missing',
                 status_before_missing = status,
                 missing_at = ?1
             WHERE status <> 'missing'
               AND last_seen_at < ?2
               AND (path = ?3 OR substr(path, 1, length(?3) + 1) = ?3 || '/')",
            params![now, scan_started_at, root_path],
        )?;
        Ok(changed)
    }

    pub fn directories_under(&self, prefix: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT path FROM directories
             WHERE status <> 'missing'
               AND (path = ?1 OR substr(path, 1, length(?1) + 1) = ?1 || '/')",
        )?;
        let rows = stmt.query_map(params![prefix], |row| row.get(0))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn mark_missing_exact(&mut self, paths: &[String]) -> Result<usize> {
        if paths.is_empty() {
            return Ok(0);
        }
        let now = now_ms();
        let tx = self.conn.transaction()?;
        let mut changed = 0;
        {
            let mut stmt = tx.prepare_cached(
                "UPDATE directories
                 SET status = 'missing',
                     status_before_missing = status,
                     missing_at = ?1
                 WHERE path = ?2 AND status <> 'missing'",
            )?;
            for path in paths {
                changed += stmt.execute(params![now, path])?;
            }
        }
        tx.commit()?;
        Ok(changed)
    }

    pub fn restore_missing(&mut self, paths: &[String]) -> Result<usize> {
        if paths.is_empty() {
            return Ok(0);
        }
        let now = now_ms();
        let tx = self.conn.transaction()?;
        let mut changed = 0;
        {
            let mut stmt = tx.prepare_cached(
                "UPDATE directories
                 SET status = 'discovered', missing_at = NULL, last_seen_at = ?1
                 WHERE path = ?2 AND status = 'missing'",
            )?;
            for path in paths {
                changed += stmt.execute(params![now, path])?;
            }
        }
        tx.commit()?;
        Ok(changed)
    }

    pub fn count_by_status(&self) -> Result<Vec<(String, i64)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT status, COUNT(*) FROM directories GROUP BY status")?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn count_all(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM directories", [], |row| row.get(0))?)
    }

    pub fn count_status(&self, status: Status) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM directories WHERE status = ?1",
            params![status.as_str()],
            |row| row.get(0),
        )?)
    }

    pub fn count_pending(&self) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM directories
             WHERE status IN ('discovered', 'pending_manual_exclusion', 'exported')",
            [],
            |row| row.get(0),
        )?)
    }

    pub fn list_directories(&self, filter: &DirectoryFilter) -> Result<Vec<DirectoryRecord>> {
        let mut sql = format!("SELECT {DIRECTORY_COLUMNS} FROM directories WHERE 1 = 1");
        let mut params: Vec<String> = Vec::new();
        if !filter.statuses.is_empty() {
            sql.push_str(" AND status IN (");
            for (i, status) in filter.statuses.iter().enumerate() {
                if i > 0 {
                    sql.push(',');
                }
                sql.push('?');
                params.push(status.as_str().to_string());
            }
            sql.push(')');
        }
        if let Some(rule) = &filter.rule {
            sql.push_str(" AND (rule = ? OR rule_kind = ?)");
            params.push(rule.clone());
            params.push(rule.clone());
        }
        if let Some(root) = &filter.root {
            sql.push_str(" AND (path = ? OR substr(path, 1, length(?) + 1) = ? || '/')");
            params.push(root.clone());
            params.push(root.clone());
            params.push(root.clone());
        }
        if let Some(project) = &filter.project {
            sql.push_str(
                " AND (project_root = ? OR substr(project_root, 1, length(?) + 1) = ? || '/')",
            );
            params.push(project.clone());
            params.push(project.clone());
            params.push(project.clone());
        }
        sql.push_str(" ORDER BY path COLLATE NOCASE");
        if let Some(limit) = filter.limit {
            sql.push_str(&format!(" LIMIT {limit}"));
        }
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(
            params_from_iter(params.iter().map(|s| s.as_str())),
            row_to_directory,
        )?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn pending_directories(&self) -> Result<Vec<DirectoryRecord>> {
        self.list_directories(&DirectoryFilter {
            statuses: vec![
                Status::Discovered,
                Status::PendingManualExclusion,
                Status::Exported,
            ],
            ..Default::default()
        })
    }

    pub fn missing_directories(&self) -> Result<Vec<DirectoryRecord>> {
        self.list_directories(&DirectoryFilter {
            statuses: vec![Status::Missing],
            ..Default::default()
        })
    }

    /// Candidates for an export.  Existing directories only; verification and
    /// exclusion are never implied by exporting.
    pub fn export_candidates(
        &self,
        selection: ExportSelection,
        rule: Option<&str>,
    ) -> Result<Vec<DirectoryRecord>> {
        match selection {
            ExportSelection::Pending => self.pending_directories(),
            ExportSelection::All => self.list_directories(&DirectoryFilter {
                statuses: Status::ALL
                    .into_iter()
                    .filter(|s| *s != Status::Missing)
                    .collect(),
                ..Default::default()
            }),
            ExportSelection::Rule => self.list_directories(&DirectoryFilter {
                statuses: Status::ALL
                    .into_iter()
                    .filter(|s| *s != Status::Missing)
                    .collect(),
                rule: rule.map(String::from),
                ..Default::default()
            }),
        }
    }

    pub fn record_export(
        &mut self,
        export_path: &str,
        selection: &str,
        directories: &[DirectoryRecord],
        note: Option<&str>,
    ) -> Result<i64> {
        let now = now_ms();
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO exports (created_at, path, selection, item_count, note)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![now, export_path, selection, directories.len() as i64, note],
        )?;
        let export_id = tx.last_insert_rowid();
        {
            let mut item_stmt = tx.prepare(
                "INSERT OR IGNORE INTO export_items (export_id, directory_id) VALUES (?1, ?2)",
            )?;
            let mut update_stmt = tx.prepare(
                "UPDATE directories SET
                    last_export_id = ?1,
                    exported_at = ?2,
                    status = CASE WHEN status IN ('discovered', 'pending_manual_exclusion')
                                  THEN 'exported' ELSE status END
                 WHERE id = ?3",
            )?;
            for dir in directories {
                item_stmt.execute(params![export_id, dir.id])?;
                update_stmt.execute(params![export_id, now, dir.id])?;
            }
        }
        tx.commit()?;
        Ok(export_id)
    }

    pub fn mark_applied(&mut self, target: &MarkTarget) -> Result<usize> {
        let now = now_ms();
        match target {
            MarkTarget::All => Ok(self.conn.execute(
                "UPDATE directories SET status = 'manually_applied', manually_applied_at = ?1
                 WHERE status IN ('discovered', 'pending_manual_exclusion', 'exported')",
                params![now],
            )?),
            MarkTarget::Paths(paths) => {
                let tx = self.conn.transaction()?;
                let mut changed = 0;
                {
                    let mut stmt = tx.prepare_cached(
                        "UPDATE directories
                         SET status = 'manually_applied', manually_applied_at = ?1
                         WHERE path = ?2
                           AND status IN ('discovered', 'pending_manual_exclusion', 'exported')",
                    )?;
                    for path in paths {
                        changed += stmt.execute(params![now, path])?;
                    }
                }
                tx.commit()?;
                Ok(changed)
            }
            MarkTarget::Export(export_id) => Ok(self.conn.execute(
                "UPDATE directories SET status = 'manually_applied', manually_applied_at = ?1
                 WHERE id IN (SELECT directory_id FROM export_items WHERE export_id = ?2)
                   AND status IN ('discovered', 'pending_manual_exclusion', 'exported')",
                params![now, export_id],
            )?),
        }
    }

    pub fn mark_verified(&mut self, target: &MarkTarget) -> Result<usize> {
        let now = now_ms();
        match target {
            MarkTarget::All => Ok(self.conn.execute(
                "UPDATE directories
                 SET status = 'verified', verified_at = ?1,
                     manually_applied_at = COALESCE(manually_applied_at, ?1)
                 WHERE status = 'manually_applied'",
                params![now],
            )?),
            MarkTarget::Paths(paths) => {
                let tx = self.conn.transaction()?;
                let mut changed = 0;
                {
                    let mut stmt = tx.prepare_cached(
                        "UPDATE directories
                         SET status = 'verified', verified_at = ?1,
                             manually_applied_at = COALESCE(manually_applied_at, ?1)
                         WHERE path = ?2 AND status = 'manually_applied'",
                    )?;
                    for path in paths {
                        changed += stmt.execute(params![now, path])?;
                    }
                }
                tx.commit()?;
                Ok(changed)
            }
            MarkTarget::Export(export_id) => Ok(self.conn.execute(
                "UPDATE directories
                 SET status = 'verified', verified_at = ?1,
                     manually_applied_at = COALESCE(manually_applied_at, ?1)
                 WHERE id IN (SELECT directory_id FROM export_items WHERE export_id = ?2)
                   AND status = 'manually_applied'",
                params![now, export_id],
            )?),
        }
    }

    pub fn record_verification(
        &mut self,
        method: &str,
        outcome: &str,
        message: &str,
        checked: usize,
        verified: usize,
    ) -> Result<i64> {
        let now = now_ms();
        self.conn.execute(
            "INSERT INTO verification_runs
                (started_at, finished_at, method, outcome, message, checked_count, verified_count)
             VALUES (?1, ?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                now,
                method,
                outcome,
                message,
                checked as i64,
                verified as i64
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn latest_verification(&self) -> Result<Option<VerificationRecord>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, started_at, finished_at, method, outcome, message,
                        checked_count, verified_count
                 FROM verification_runs ORDER BY id DESC LIMIT 1",
                [],
                |row| {
                    Ok(VerificationRecord {
                        id: row.get(0)?,
                        started_at: row.get(1)?,
                        finished_at: row.get(2)?,
                        method: row.get(3)?,
                        outcome: row.get(4)?,
                        message: row.get(5)?,
                        checked_count: row.get(6)?,
                        verified_count: row.get(7)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn recent_scans(&self, limit: usize) -> Result<Vec<ScanRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, started_at, finished_at, roots_scanned, dirs_visited,
                    matched, new_dirs, missing_dirs, errors, interrupted, note
             FROM scans ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |row| {
            Ok(ScanRecord {
                id: row.get(0)?,
                kind: row.get(1)?,
                started_at: row.get(2)?,
                finished_at: row.get(3)?,
                roots_scanned: row.get(4)?,
                dirs_visited: row.get(5)?,
                matched: row.get(6)?,
                new_dirs: row.get(7)?,
                missing_dirs: row.get(8)?,
                errors: row.get(9)?,
                interrupted: row.get::<_, i64>(10)? != 0,
                note: row.get(11)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn recent_exports(&self, limit: usize) -> Result<Vec<ExportRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, created_at, path, selection, item_count
             FROM exports ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |row| {
            Ok(ExportRecord {
                id: row.get(0)?,
                created_at: row.get(1)?,
                path: row.get(2)?,
                selection: row.get(3)?,
                item_count: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn log_event(&self, kind: &str, detail: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO event_log (at, kind, detail) VALUES (?1, ?2, ?3)",
            params![now_ms(), kind, detail],
        )?;
        Ok(())
    }

    pub fn prune_old_events(&self, days: i64) -> Result<usize> {
        let cutoff = now_ms() - days.max(1) * 24 * 60 * 60 * 1000;
        Ok(self
            .conn
            .execute("DELETE FROM event_log WHERE at < ?1", params![cutoff])?)
    }

    pub fn checkpoint(&self) -> Result<()> {
        self.conn
            .execute_batch("PRAGMA wal_checkpoint(PASSIVE); PRAGMA optimize;")?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Automatic exclusion bookkeeping (schema v2)
    // ------------------------------------------------------------------

    pub fn exclusion_get(&self, path: &str) -> Result<Option<ExclusionRecord>> {
        Ok(self
            .conn
            .query_row(
                "SELECT path, directory_id, managed, state, source, added_at,
                        verified_at, last_checked_at, error
                 FROM exclusions WHERE path = ?1",
                params![path],
                row_to_exclusion,
            )
            .optional()?)
    }

    pub fn exclusions_all(&self) -> Result<Vec<ExclusionRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, directory_id, managed, state, source, added_at,
                    verified_at, last_checked_at, error
             FROM exclusions ORDER BY path",
        )?;
        let rows = stmt.query_map([], row_to_exclusion)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Paths this application added to Spotlight's list.  User-managed entries
    /// (`managed = 0`) are deliberately excluded.
    pub fn managed_exclusions(&self) -> Result<Vec<ExclusionRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, directory_id, managed, state, source, added_at,
                    verified_at, last_checked_at, error
             FROM exclusions WHERE managed = 1 ORDER BY path",
        )?;
        let rows = stmt.query_map([], row_to_exclusion)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn count_by_exclusion_state(&self) -> Result<Vec<(String, i64)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT state, COUNT(*) FROM exclusions GROUP BY state")?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn managed_exclusion_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM exclusions WHERE managed = 1", [], |row| {
                row.get(0)
            })?)
    }

    /// Record the result of an exclusion operation.  Existing `managed = 1`
    /// bookkeeping is never downgraded to a user entry.
    #[allow(clippy::too_many_arguments)]
    pub fn record_exclusion(
        &mut self,
        path: &str,
        directory_id: Option<i64>,
        managed: bool,
        state: ExclusionState,
        source: ExclusionSource,
        error: Option<&str>,
    ) -> Result<()> {
        let now = now_ms();
        let verified_at = if state == ExclusionState::Excluded {
            Some(now)
        } else {
            None
        };
        self.conn.execute(
            "INSERT INTO exclusions
                (path, directory_id, managed, state, source, added_at,
                 verified_at, last_checked_at, error)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?6, ?8)
             ON CONFLICT(path) DO UPDATE SET
                directory_id = COALESCE(excluded.directory_id, exclusions.directory_id),
                managed = CASE WHEN exclusions.managed = 1 THEN 1 ELSE excluded.managed END,
                state = excluded.state,
                source = excluded.source,
                verified_at = excluded.verified_at,
                last_checked_at = excluded.last_checked_at,
                error = excluded.error",
            params![
                path,
                directory_id,
                i64::from(managed),
                state.as_str(),
                source.as_str(),
                now,
                verified_at,
                error
            ],
        )?;
        Ok(())
    }

    pub fn delete_exclusion(&mut self, path: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM exclusions WHERE path = ?1", params![path])?;
        Ok(())
    }

    pub fn directory_id_for_path(&self, path: &str) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id FROM directories WHERE path = ?1",
                params![path],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Mark a directory as successfully excluded.  The manual workflow's
    /// `verified` status is reused because it already means "exclusion
    /// confirmed"; the exclusion table retains the machine-readable detail.
    pub fn mark_directory_exclusion_verified(&mut self, path: &str) -> Result<usize> {
        let now = now_ms();
        Ok(self.conn.execute(
            "UPDATE directories
             SET status = 'verified',
                 verified_at = COALESCE(verified_at, ?1),
                 manually_applied_at = COALESCE(manually_applied_at, ?1),
                 last_error = NULL
             WHERE path = ?2 AND status <> 'missing'",
            params![now, path],
        )?)
    }

    /// Automatic exclusion failed: keep the directory in the manual/export
    /// fallback workflow rather than claiming success.
    pub fn mark_directory_exclusion_pending(
        &mut self,
        path: &str,
        error: Option<&str>,
    ) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE directories
             SET status = 'pending_manual_exclusion',
                 verified_at = NULL,
                 last_error = ?1,
                 retry_count = retry_count + 1
             WHERE path = ?2 AND status <> 'missing'",
            params![error, path],
        )?)
    }

    pub fn mark_directory_exclusion_released(&mut self, path: &str) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE directories
             SET status = CASE WHEN status = 'missing' THEN status
                               ELSE 'pending_manual_exclusion' END,
                 verified_at = NULL,
                 manually_applied_at = NULL,
                 last_error = NULL
             WHERE path = ?1",
            params![path],
        )?)
    }

    pub fn opt_out_exclusion(&mut self, path: &str, reason: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO exclusion_opt_outs (path, created_at, reason)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(path) DO UPDATE SET
                created_at = excluded.created_at,
                reason = excluded.reason",
            params![path, now_ms(), reason],
        )?;
        Ok(())
    }

    pub fn clear_exclusion_opt_out(&mut self, path: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM exclusion_opt_outs WHERE path = ?1",
            params![path],
        )?;
        Ok(())
    }

    pub fn is_exclusion_opted_out(&self, path: &str) -> Result<bool> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM exclusion_opt_outs WHERE path = ?1",
                params![path],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    pub fn exclusion_opt_outs(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path FROM exclusion_opt_outs ORDER BY path")?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn table_count(&self, table: &str) -> Result<i64> {
        // Only internal table names are passed here.
        let sql = format!("SELECT COUNT(*) FROM {table}");
        Ok(self.conn.query_row(&sql, [], |row| row.get(0))?)
    }

    pub fn db_size_bytes(&self) -> u64 {
        let main = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
        let wal = std::fs::metadata(self.path.with_extension("sqlite3-wal"))
            .map(|m| m.len())
            .unwrap_or(0);
        let shm = std::fs::metadata(self.path.with_extension("sqlite3-shm"))
            .map(|m| m.len())
            .unwrap_or(0);
        main + wal + shm
    }
}

fn row_to_directory(row: &rusqlite::Row<'_>) -> rusqlite::Result<DirectoryRecord> {
    Ok(DirectoryRecord {
        id: row.get(0)?,
        path: row.get(1)?,
        name: row.get(2)?,
        rule: row.get(3)?,
        rule_kind: row.get(4)?,
        scan_root_id: row.get(5)?,
        project_root: row.get(6)?,
        first_seen_at: row.get(7)?,
        last_seen_at: row.get(8)?,
        status: row.get(9)?,
        status_before_missing: row.get(10)?,
        last_export_id: row.get(11)?,
        exported_at: row.get(12)?,
        manually_applied_at: row.get(13)?,
        verified_at: row.get(14)?,
        missing_at: row.get(15)?,
        retry_count: row.get(16)?,
        last_error: row.get(17)?,
        exclusion_state: row.get(18)?,
        exclusion_managed: row
            .get::<_, Option<i64>>(19)?
            .map(|value| value != 0),
        exclusion_verified_at: row.get(20)?,
        exclusion_error: row.get(21)?,
    })
}

fn row_to_exclusion(row: &rusqlite::Row<'_>) -> rusqlite::Result<ExclusionRecord> {
    Ok(ExclusionRecord {
        path: row.get(0)?,
        directory_id: row.get(1)?,
        managed: row.get::<_, i64>(2)? != 0,
        state: row.get(3)?,
        source: row.get(4)?,
        added_at: row.get(5)?,
        verified_at: row.get(6)?,
        last_checked_at: row.get(7)?,
        error: row.get(8)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::RuleKind;

    fn db() -> Db {
        Db::open_in_memory().unwrap()
    }

    fn new_dir(path: &str) -> NewDirectory {
        let p = Path::new(path);
        NewDirectory {
            path: path.to_string(),
            name: p.file_name().unwrap().to_string_lossy().to_string(),
            rule: "node_modules".into(),
            rule_kind: RuleKind::Dependencies,
            scan_root_id: 1,
            project_root: p.parent().unwrap().to_string_lossy().to_string(),
        }
    }

    #[test]
    fn migrations_and_persistence() {
        let db = db();
        assert_eq!(db.schema_version().unwrap(), 2);
        db.meta_set("hello", "world").unwrap();
        assert_eq!(db.meta_get("hello").unwrap().as_deref(), Some("world"));
    }

    #[test]
    fn scan_roots_are_disabled_not_deleted() {
        let mut db = db();
        db.sync_roots(&[PathBuf::from("/Users/a/Projects")])
            .unwrap();
        assert_eq!(db.enabled_roots().unwrap().len(), 1);
        db.sync_roots(&[PathBuf::from("/Users/a/Code")]).unwrap();
        assert_eq!(db.enabled_roots().unwrap().len(), 1);
        assert_eq!(db.all_roots().unwrap().len(), 2);
    }

    #[test]
    fn upsert_counts_new_and_revives_missing() {
        let mut db = db();
        db.sync_roots(&[PathBuf::from("/Users/a/Projects")])
            .unwrap();
        let root = db.enabled_roots().unwrap().remove(0);

        let entry = NewDirectory {
            scan_root_id: root.id,
            ..new_dir("/Users/a/Projects/app/node_modules")
        };
        let started = now_ms();
        let stats = db
            .upsert_directories(std::slice::from_ref(&entry), started)
            .unwrap();
        assert_eq!(stats.new_dirs, 1);
        db.promote_discovered().unwrap();
        assert_eq!(db.count_pending().unwrap(), 1);

        let missing = db
            .mark_missing_for_root("/Users/a/Projects", started + 1)
            .unwrap();
        assert_eq!(missing, 1);
        assert_eq!(db.count_status(Status::Missing).unwrap(), 1);

        let stats = db
            .upsert_directories(std::slice::from_ref(&entry), started + 2)
            .unwrap();
        assert_eq!(stats.revived, 1);
        db.promote_discovered().unwrap();
        assert_eq!(db.count_status(Status::Missing).unwrap(), 0);
        assert_eq!(db.count_pending().unwrap(), 1);
    }

    #[test]
    fn export_does_not_mark_verified() {
        let mut db = db();
        db.sync_roots(&[PathBuf::from("/Users/a/Projects")])
            .unwrap();
        let root = db.enabled_roots().unwrap().remove(0);
        let entry = NewDirectory {
            scan_root_id: root.id,
            ..new_dir("/Users/a/Projects/app/node_modules")
        };
        let started = now_ms();
        db.upsert_directories(&[entry], started).unwrap();
        db.promote_discovered().unwrap();

        let candidates = db
            .export_candidates(ExportSelection::Pending, None)
            .unwrap();
        assert_eq!(candidates.len(), 1);
        db.record_export("/tmp/export.txt", "pending", &candidates, None)
            .unwrap();

        let rows = db
            .list_directories(&DirectoryFilter {
                statuses: vec![Status::Exported],
                ..Default::default()
            })
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(db.count_status(Status::Verified).unwrap(), 0);
        assert_eq!(db.count_pending().unwrap(), 1);
    }

    #[test]
    fn interrupted_scans_are_recovered() {
        let mut db = db();
        let session = db.start_scan(ScanKind::Initial, 1).unwrap();
        // Deliberately no finish_scan.
        db.recover_interrupted().unwrap();
        let scans = db.recent_scans(1).unwrap();
        assert_eq!(scans[0].id, session.id);
        assert!(scans[0].interrupted);
        assert!(scans[0].finished_at.is_some());
    }
}
