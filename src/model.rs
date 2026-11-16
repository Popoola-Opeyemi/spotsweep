//! Plain data types shared by the database, scanner, CLI and exporter.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Persisted but not yet promoted at the end of a scan.  Transient: an
    /// interrupted scan can leave rows here and the next open promotes them.
    Discovered,
    /// Exists and needs a manual Spotlight Search Privacy exclusion.
    PendingManualExclusion,
    /// Included in an export file; still awaiting manual action.
    Exported,
    /// The user confirmed the directory was added to Search Privacy.
    ManuallyApplied,
    /// Exclusion confirmed by an explicit verification method.
    Verified,
    /// The directory no longer exists at its recorded path.
    Missing,
}

impl Status {
    pub const ALL: [Status; 6] = [
        Status::Discovered,
        Status::PendingManualExclusion,
        Status::Exported,
        Status::ManuallyApplied,
        Status::Verified,
        Status::Missing,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Status::Discovered => "discovered",
            Status::PendingManualExclusion => "pending_manual_exclusion",
            Status::Exported => "exported",
            Status::ManuallyApplied => "manually_applied",
            Status::Verified => "verified",
            Status::Missing => "missing",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|status| status.as_str() == value)
    }

    /// Directories that still require the user to act in System Settings.
    /// `exported` is included: exporting never means excluded.
    pub fn is_pending(self) -> bool {
        matches!(
            self,
            Status::Discovered | Status::PendingManualExclusion | Status::Exported
        )
    }

    pub fn is_active(self) -> bool {
        self != Status::Missing
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleKind {
    Dependencies,
    Build,
    Generated,
    Optional,
    DotDirectory,
    Glob,
}

impl RuleKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RuleKind::Dependencies => "dependencies",
            RuleKind::Build => "build",
            RuleKind::Generated => "generated",
            RuleKind::Optional => "optional",
            RuleKind::DotDirectory => "dot_directory",
            RuleKind::Glob => "glob",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            RuleKind::Dependencies => "dependency",
            RuleKind::Build => "build artifact",
            RuleKind::Generated => "generated",
            RuleKind::Optional => "optional",
            RuleKind::DotDirectory => "dot-directory",
            RuleKind::Glob => "glob",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanKind {
    Initial,
    Incremental,
    Reconcile,
    Targeted,
}

impl ScanKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ScanKind::Initial => "initial",
            ScanKind::Incremental => "incremental",
            ScanKind::Reconcile => "reconcile",
            ScanKind::Targeted => "targeted",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "initial" => Some(ScanKind::Initial),
            "incremental" => Some(ScanKind::Incremental),
            "reconcile" => Some(ScanKind::Reconcile),
            "targeted" => Some(ScanKind::Targeted),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DirectoryRecord {
    pub id: i64,
    pub path: String,
    pub name: String,
    pub rule: String,
    pub rule_kind: String,
    pub scan_root_id: Option<i64>,
    pub project_root: String,
    pub first_seen_at: i64,
    pub last_seen_at: i64,
    pub status: String,
    pub status_before_missing: Option<String>,
    pub last_export_id: Option<i64>,
    pub exported_at: Option<i64>,
    pub manually_applied_at: Option<i64>,
    pub verified_at: Option<i64>,
    pub missing_at: Option<i64>,
    pub retry_count: i64,
    pub last_error: Option<String>,
}

impl DirectoryRecord {
    pub fn parsed_status(&self) -> Status {
        Status::parse(&self.status).unwrap_or(Status::PendingManualExclusion)
    }

    pub fn path_buf(&self) -> PathBuf {
        PathBuf::from(&self.path)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RootRecord {
    pub id: i64,
    pub path: String,
    pub enabled: bool,
    pub created_at: i64,
    pub last_scanned_at: Option<i64>,
    pub last_scan_ok: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct NewDirectory {
    pub path: String,
    pub name: String,
    pub rule: String,
    pub rule_kind: RuleKind,
    pub scan_root_id: i64,
    pub project_root: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ScanSummary {
    pub kind: String,
    pub roots_configured: usize,
    pub roots_scanned: usize,
    pub roots_missing: usize,
    pub dirs_visited: u64,
    pub matched: u64,
    pub new_dirs: u64,
    pub missing_dirs: u64,
    pub errors: u64,
    pub duration_ms: u64,
    pub interrupted: bool,
}

impl ScanSummary {
    pub fn log_line(&self) -> String {
        format!(
            "{} scan: roots={}/{} visited={} matched={} new={} missing={} errors={} in {}ms",
            self.kind,
            self.roots_scanned,
            self.roots_configured,
            self.dirs_visited,
            self.matched,
            self.new_dirs,
            self.missing_dirs,
            self.errors,
            self.duration_ms
        )
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct TargetedSummary {
    pub event_paths: usize,
    pub dirs_visited: u64,
    pub matched: u64,
    pub new_dirs: u64,
    pub missing_dirs: u64,
    pub errors: u64,
    pub ignored: u64,
    pub duration_ms: u64,
}

#[derive(Debug, Clone)]
pub struct ScanSession {
    pub id: i64,
    pub started_at: i64,
    pub kind: ScanKind,
}
