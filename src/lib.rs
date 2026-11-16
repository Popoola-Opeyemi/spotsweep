//! spotlight-dev-exclusions: discover development build directories and prepare
//! a manual Spotlight Search Privacy exclusion list.
//!
//! The crate never writes to Spotlight configuration, private stores, `mds`, or
//! undocumented XPC services.  All state is local SQLite under Application
//! Support; all discovery is Rust + documented FSEvents.

pub mod cli;
pub mod config;
pub mod daemon;
pub mod database;
pub mod exporter;
pub mod launchagent;
pub mod logging;
pub mod matcher;
pub mod model;
pub mod paths;
pub mod scanner;
pub mod util;
pub mod verifier;
pub mod watcher;

pub fn run() -> anyhow::Result<()> {
    cli::run()
}
