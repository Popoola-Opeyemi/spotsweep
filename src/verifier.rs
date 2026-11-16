//! Manual fallback verification for Spotlight Search Privacy exclusions.
//!
//! Automatic verification lives in [`crate::exclusion::verify_path`], which
//! re-reads Spotlight's actual exclusion list through Metadata.framework and
//! can cross-check `mdfind`.
//!
//! This module handles the manual workflow and the diagnostic `mdutil` probe.
//! Research result: `mdutil -P` dumps the volume's
//! `VolumeConfiguration.plist`.  It does not expose the Search Privacy list,
//! and its output is not a stable public interface.  Running it requires root.
//! Consequently this application never treats `mdutil` output as proof.

use crate::config::Config;
use crate::database::{Db, DirectoryFilter, MarkTarget};
use crate::model::Status;
use crate::paths::is_root;
use anyhow::{Result, bail};
use serde::Serialize;
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifyOutcome {
    Verified,
    Inconclusive,
    ManualRequired,
    NothingToCheck,
}

impl VerifyOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            VerifyOutcome::Verified => "verified",
            VerifyOutcome::Inconclusive => "inconclusive",
            VerifyOutcome::ManualRequired => "manual_required",
            VerifyOutcome::NothingToCheck => "nothing_to_check",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct VerifyReport {
    pub outcome: VerifyOutcome,
    pub method: String,
    pub message: String,
    pub checked: usize,
    pub verified: usize,
    pub mdutil_probe: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MdutilProbe {
    pub ran: bool,
    pub success: bool,
    pub summary: String,
    pub looks_like_plist: bool,
}

pub fn probe_mdutil(volume: &str) -> MdutilProbe {
    if !is_root() {
        return MdutilProbe {
            ran: false,
            success: false,
            summary: "not run: `mdutil -P` requires root".into(),
            looks_like_plist: false,
        };
    }
    let output = match Command::new("/usr/bin/mdutil")
        .arg("-P")
        .arg(volume)
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            return MdutilProbe {
                ran: false,
                success: false,
                summary: format!("could not execute mdutil: {error}"),
                looks_like_plist: false,
            };
        }
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let looks_like_plist = stdout.contains("<?xml")
        || stdout.contains("<plist")
        || stdout.starts_with("bplist")
        || stdout.contains("VolumeConfiguration");
    let summary = if output.status.success() {
        format!(
            "mdutil -P {volume} exited successfully; {} bytes of VolumeConfiguration data (plist={looks_like_plist}). This is not the Search Privacy list.",
            stdout.len()
        )
    } else {
        format!(
            "mdutil -P {volume} failed ({}): {}",
            output.status,
            stderr.trim()
        )
    };
    MdutilProbe {
        ran: true,
        success: output.status.success(),
        summary,
        looks_like_plist,
    }
}

pub fn verify(
    db: &mut Db,
    config: &Config,
    confirm: bool,
    paths: &[String],
    mdutil: bool,
) -> Result<VerifyReport> {
    let applied = db.list_directories(&DirectoryFilter {
        statuses: vec![Status::ManuallyApplied],
        ..Default::default()
    })?;

    if confirm {
        if applied.is_empty() && paths.is_empty() {
            db.record_verification(
                "manual-confirmation",
                VerifyOutcome::NothingToCheck.as_str(),
                "no manually applied directories to confirm",
                0,
                0,
            )?;
            return Ok(VerifyReport {
                outcome: VerifyOutcome::NothingToCheck,
                method: "manual-confirmation".into(),
                message:
                    "No directories are marked manually applied. Run `spotsweep mark --all` after adding the exported folders in System Settings."
                        .into(),
                checked: 0,
                verified: 0,
                mdutil_probe: None,
            });
        }

        let target = if paths.is_empty() {
            MarkTarget::All
        } else {
            MarkTarget::Paths(paths.to_vec())
        };
        let verified = db.mark_verified(&target)?;
        let checked = if paths.is_empty() {
            applied.len()
        } else {
            paths.len()
        };
        let message = format!(
            "Recorded explicit visual confirmation for {verified} director{}.",
            if verified == 1 { "y" } else { "ies" }
        );
        db.record_verification(
            "manual-confirmation",
            VerifyOutcome::Verified.as_str(),
            &message,
            checked,
            verified,
        )?;
        return Ok(VerifyReport {
            outcome: VerifyOutcome::Verified,
            method: "manual-confirmation".into(),
            message,
            checked,
            verified,
            mdutil_probe: None,
        });
    }

    let probe = if mdutil {
        Some(probe_mdutil(&config.verify.mdutil_volume))
    } else {
        None
    };

    let message = match &probe {
        Some(probe) if probe.ran && probe.success => format!(
            "{}\n\nThis output is not a supported source of Search Privacy exclusions, so no directory was marked verified.",
            probe.summary
        ),
        Some(probe) => format!(
            "{}\n\n`mdutil -P` dumps VolumeConfiguration.plist, not the Search Privacy list. No automatic verification was possible.",
            probe.summary
        ),
        None => "No supported API or CLI exposes the Spotlight Search Privacy list. `mdutil -P` (root-only) dumps VolumeConfiguration.plist and is not evidence of an exclusion. Manual visual confirmation is required.".into(),
    };

    let outcome = if applied.is_empty() {
        VerifyOutcome::NothingToCheck
    } else {
        VerifyOutcome::ManualRequired
    };
    db.record_verification(
        if mdutil {
            "mdutil-probe"
        } else {
            "manual-required"
        },
        outcome.as_str(),
        &message,
        applied.len(),
        0,
    )?;

    Ok(VerifyReport {
        outcome,
        method: if mdutil {
            "mdutil-probe"
        } else {
            "manual-required"
        }
        .into(),
        message,
        checked: applied.len(),
        verified: 0,
        mdutil_probe: probe.map(|p| p.summary),
    })
}

pub fn manual_confirmation_hint() -> &'static str {
    "\
Manual verification:
  1. Open System Settings -> Spotlight -> Search Privacy.
  2. Confirm each exported folder is present.
  3. Record that confirmation:
       spotsweep verify --confirm

Automatic verification is also available: `spotsweep verify PATH` reads
Spotlight's actual exclusion list. This manual path never edits Spotlight
stores or mds state."
}

pub fn require_manual_or_confirm(confirm: bool) -> Result<()> {
    if !confirm {
        bail!(
            "automatic verification is unavailable; use `verify --confirm` after checking System Settings"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::NewDirectory;

    #[test]
    fn default_verification_is_manual_and_never_marks_verified() {
        let mut db = Db::open_in_memory().unwrap();
        db.sync_roots(&[std::path::PathBuf::from("/Users/a/Projects")])
            .unwrap();
        let root = db.enabled_roots().unwrap().remove(0);
        db.upsert_directories(
            &[NewDirectory {
                path: "/Users/a/Projects/app/node_modules".into(),
                name: "node_modules".into(),
                rule: "node_modules".into(),
                rule_kind: crate::model::RuleKind::Dependencies,
                scan_root_id: root.id,
                project_root: "/Users/a/Projects/app".into(),
            }],
            crate::util::now_ms(),
        )
        .unwrap();
        db.promote_discovered().unwrap();
        db.mark_applied(&MarkTarget::All).unwrap();

        let report = verify(&mut db, &Config::default(), false, &[], false).unwrap();
        assert_eq!(report.outcome, VerifyOutcome::ManualRequired);
        assert_eq!(db.count_status(Status::Verified).unwrap(), 0);
        assert_eq!(db.count_status(Status::ManuallyApplied).unwrap(), 1);
    }

    #[test]
    fn explicit_confirmation_marks_verified() {
        let mut db = Db::open_in_memory().unwrap();
        db.sync_roots(&[std::path::PathBuf::from("/Users/a/Projects")])
            .unwrap();
        let root = db.enabled_roots().unwrap().remove(0);
        db.upsert_directories(
            &[NewDirectory {
                path: "/Users/a/Projects/app/node_modules".into(),
                name: "node_modules".into(),
                rule: "node_modules".into(),
                rule_kind: crate::model::RuleKind::Dependencies,
                scan_root_id: root.id,
                project_root: "/Users/a/Projects/app".into(),
            }],
            crate::util::now_ms(),
        )
        .unwrap();
        db.promote_discovered().unwrap();
        db.mark_applied(&MarkTarget::All).unwrap();
        let report = verify(&mut db, &Config::default(), true, &[], false).unwrap();
        assert_eq!(report.outcome, VerifyOutcome::Verified);
        assert_eq!(db.count_status(Status::Verified).unwrap(), 1);
    }
}
