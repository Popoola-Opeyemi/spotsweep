//! Plain-text export generation for the manual Spotlight Search Privacy step.

use crate::config::Config;
use crate::database::{Db, ExportSelection};
use crate::model::DirectoryRecord;
use crate::paths::AppPaths;
use crate::util::{atomic_write, ensure_dir, timestamp_to_string};
use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportRequest {
    Pending,
    All,
    Rule(String),
}

impl ExportRequest {
    pub fn label(&self) -> String {
        match self {
            ExportRequest::Pending => "pending".into(),
            ExportRequest::All => "all".into(),
            ExportRequest::Rule(rule) => format!("rule:{rule}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ExportOutcome {
    pub path: PathBuf,
    pub export_id: i64,
    pub count: usize,
    pub selection: String,
}

pub fn render(
    directories: &[DirectoryRecord],
    group_by_project: bool,
    include_header: bool,
    generated_at: i64,
) -> Result<String> {
    let mut by_project: BTreeMap<&str, Vec<&DirectoryRecord>> = BTreeMap::new();
    for dir in directories {
        if dir.path.contains('\n') || dir.path.contains('\r') {
            bail!(
                "refusing to export a path containing a newline: {:?}",
                dir.path
            );
        }
        by_project
            .entry(dir.project_root.as_str())
            .or_default()
            .push(dir);
    }

    let mut out = String::new();
    if include_header {
        out.push_str("# Spotlight Development Exclusions\n");
        out.push_str(&format!(
            "# Generated: {}\n",
            timestamp_to_string(generated_at)
        ));
        out.push_str("#\n");
        out.push_str("# These are absolute paths on this Mac.  Add them manually in:\n");
        out.push_str("#   System Settings -> Spotlight -> Search Privacy\n");
        out.push_str("#\n");
        out.push_str("# The exporter never changes Spotlight configuration.  A path is not\n");
        out.push_str("# excluded until you add it in System Settings and mark it applied:\n");
        out.push_str("#   spotlight-dev mark --all          (after adding the folders)\n");
        out.push_str("#   spotlight-dev verify --confirm    (after visual confirmation)\n");
        out.push_str("#\n");
        out.push_str(&format!("# Directories: {}\n", directories.len()));
        out.push('\n');
    }

    if group_by_project {
        for (project, entries) in by_project {
            out.push_str(&format!("# Project: {project} ({})\n", entries.len()));
            let mut paths: Vec<&str> = entries.iter().map(|d| d.path.as_str()).collect();
            paths.sort_unstable();
            for path in paths {
                out.push_str(path);
                out.push('\n');
            }
            out.push('\n');
        }
    } else {
        let mut paths: Vec<&str> = directories.iter().map(|d| d.path.as_str()).collect();
        paths.sort_unstable();
        for path in paths {
            out.push_str(path);
            out.push('\n');
        }
    }

    Ok(out)
}

pub fn export(
    db: &mut Db,
    config: &Config,
    paths: &AppPaths,
    request: &ExportRequest,
    group_by_project: bool,
    include_header: bool,
    output: Option<&Path>,
) -> Result<ExportOutcome> {
    let (_, directories) = match request {
        ExportRequest::Pending => (
            ExportSelection::Pending,
            db.export_candidates(ExportSelection::Pending, None)?,
        ),
        ExportRequest::All => (
            ExportSelection::All,
            db.export_candidates(ExportSelection::All, None)?,
        ),
        ExportRequest::Rule(rule) => (
            ExportSelection::Rule,
            db.export_candidates(ExportSelection::Rule, Some(rule))?,
        ),
    };

    if directories.is_empty() {
        bail!(
            "no directories match the export selection ({}); run `spotlight-dev scan` first",
            request.label()
        );
    }

    let generated_at = crate::util::now_ms();
    let content = render(&directories, group_by_project, include_header, generated_at)?;

    let export_dir = config.export_dir(paths)?;
    ensure_dir(&export_dir, 0o700)?;
    let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S-%3f").to_string();
    let filename = format!("spotlight-exclusions-{timestamp}.txt");
    let destination = match output {
        Some(path) => {
            let path = crate::paths::normalize_path(path)?;
            if path.is_dir() || path.extension().is_none() {
                path.join(filename)
            } else {
                path
            }
        }
        None => export_dir.join(filename),
    };

    if let Some(parent) = destination.parent() {
        ensure_dir(parent, 0o700)?;
    }
    atomic_write(&destination, content.as_bytes(), 0o600)?;

    let export_id = match db.record_export(
        &destination.to_string_lossy(),
        &request.label(),
        &directories,
        None,
    ) {
        Ok(id) => id,
        Err(error) => {
            let _ = std::fs::remove_file(&destination);
            return Err(error).context("recording export in SQLite");
        }
    };

    Ok(ExportOutcome {
        path: destination,
        export_id,
        count: directories.len(),
        selection: request.label(),
    })
}

pub fn instructions(path: &Path) -> String {
    format!(
        "\
Export written to:
  {}

Next steps (manual; no private APIs are used):
  1. Open System Settings -> Spotlight -> Search Privacy.
  2. Click + (or drag folders from Finder) and add each exported folder.
  3. After adding them, record the manual action:
       spotlight-dev mark --all
  4. After visually confirming the folders are listed, record verification:
       spotlight-dev verify --confirm

Exporting does not change Spotlight configuration and is not proof of exclusion.",
        path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::DirectoryFilter;
    use crate::model::{NewDirectory, RuleKind, ScanKind, Status};

    fn sample(path: &str, project: &str) -> DirectoryRecord {
        DirectoryRecord {
            id: 1,
            path: path.into(),
            name: path.rsplit('/').next().unwrap().into(),
            rule: "node_modules".into(),
            rule_kind: "dependencies".into(),
            scan_root_id: Some(1),
            project_root: project.into(),
            first_seen_at: 0,
            last_seen_at: 0,
            status: "pending_manual_exclusion".into(),
            status_before_missing: None,
            last_export_id: None,
            exported_at: None,
            manually_applied_at: None,
            verified_at: None,
            missing_at: None,
            retry_count: 0,
            last_error: None,
            exclusion_state: None,
            exclusion_managed: None,
            exclusion_verified_at: None,
            exclusion_error: None,
        }
    }

    #[test]
    fn renders_flat_and_grouped_exports() {
        let rows = vec![
            sample("/Users/a/Projects/b/node_modules", "/Users/a/Projects/b"),
            sample("/Users/a/Projects/a/node_modules", "/Users/a/Projects/a"),
        ];
        let flat = render(&rows, false, true, 0).unwrap();
        assert!(flat.contains("# Spotlight Development Exclusions"));
        assert!(flat.contains("/Users/a/Projects/b/node_modules"));
        assert!(!flat.contains("# Project:"));

        let grouped = render(&rows, true, true, 0).unwrap();
        assert!(grouped.contains("# Project: /Users/a/Projects/a (1)"));
        assert!(grouped.contains("# Project: /Users/a/Projects/b (1)"));

        let bare = render(&rows, false, false, 0).unwrap();
        assert_eq!(
            bare,
            "/Users/a/Projects/a/node_modules\n/Users/a/Projects/b/node_modules\n"
        );
    }

    #[test]
    fn export_records_status_without_claiming_verification() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.export.directory = temp.path().to_string_lossy().to_string();
        let paths = AppPaths {
            data_dir: temp.path().to_path_buf(),
            config_path: temp.path().join("config.toml"),
            db_path: temp.path().join("db.sqlite3"),
            exports_dir: temp.path().join("exports"),
            logs_dir: temp.path().join("logs"),
            launch_agents_dir: temp.path().join("LaunchAgents"),
            plist_path: temp.path().join("LaunchAgents/x.plist"),
        };
        let mut db = Db::open_in_memory().unwrap();
        db.sync_roots(&[PathBuf::from("/Users/a/Projects")])
            .unwrap();
        let root = db.enabled_roots().unwrap().remove(0);
        let started = crate::util::now_ms();
        db.upsert_directories(
            &[NewDirectory {
                path: "/Users/a/Projects/app/node_modules".into(),
                name: "node_modules".into(),
                rule: "node_modules".into(),
                rule_kind: RuleKind::Dependencies,
                scan_root_id: root.id,
                project_root: "/Users/a/Projects/app".into(),
            }],
            started,
        )
        .unwrap();
        db.promote_discovered().unwrap();
        let _ = ScanKind::Initial;
        let outcome = export(
            &mut db,
            &config,
            &paths,
            &ExportRequest::Pending,
            false,
            true,
            None,
        )
        .unwrap();
        assert_eq!(outcome.count, 1);
        assert!(outcome.path.exists());
        assert_eq!(db.count_status(Status::Exported).unwrap(), 1);
        assert_eq!(db.count_status(Status::Verified).unwrap(), 0);
        assert_eq!(
            db.list_directories(&DirectoryFilter {
                statuses: vec![Status::Exported],
                ..Default::default()
            })
            .unwrap()
            .len(),
            1
        );
    }
}
