//! SpotSweep: discover development build directories and
//! automatically add them to Spotlight's Search Privacy list.
//!
//! Automatic exclusion uses the dynamically resolved private
//! `_MDCopyExclusionList` / `_MDSetExclusion` calls in Metadata.framework.
//! Every change is verified against Spotlight's reported list, and a
//! per-macOS-build selftest gates unattended use.  The crate never edits
//! Spotlight's private plist/database and never calls undocumented XPC
//! services.  All state is local SQLite under Application Support; all
//! discovery is Rust + documented FSEvents.

pub mod cli;
pub mod config;
pub mod daemon;
pub mod database;
pub mod exclusion;
pub mod exporter;
pub mod launchagent;
pub mod logging;
pub mod matcher;
pub mod metadata;
pub mod migration;
pub mod model;
pub mod paths;
pub mod scanner;
pub mod util;
pub mod verifier;
pub mod watcher;

pub fn run() -> anyhow::Result<()> {
    cli::run()
}
