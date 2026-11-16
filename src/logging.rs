//! Minimal logging setup.
//!
//! CLI messages go to stderr.  The daemon writes daily-rotated files under
//! `~/Library/Logs/SpotlightDevExclusions/` (outside the repository) and keeps
//! seven days of history.  `tracing-appender`'s non-blocking writer uses one
//! small background thread; the FSEvents callback never touches disk.

use anyhow::{Context, Result};
use std::io::IsTerminal;
use std::path::Path;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;

pub struct LogGuard {
    _guard: WorkerGuard,
}

pub fn init_cli(verbosity: u8) -> Result<()> {
    let default_level = match verbosity {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let filter = EnvFilter::try_from_env("RUST_LOG")
        .or_else(|_| EnvFilter::try_from_env("SPOTLIGHT_DEV_LOG"))
        .unwrap_or_else(|_| EnvFilter::new(default_level));
    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .with_ansi(std::io::stderr().is_terminal())
        .try_init();
    Ok(())
}

pub fn init_daemon(log_dir: &Path, level: &str) -> Result<LogGuard> {
    crate::util::ensure_dir(log_dir, 0o700)?;
    let appender: RollingFileAppender = tracing_appender::rolling::Builder::new()
        .rotation(Rotation::DAILY)
        .filename_prefix("daemon.log")
        .max_log_files(7)
        .build(log_dir)
        .with_context(|| format!("creating rolling log in {}", log_dir.display()))?;
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let filter = EnvFilter::try_from_env("RUST_LOG")
        .or_else(|_| EnvFilter::try_from_env("SPOTLIGHT_DEV_LOG"))
        .unwrap_or_else(|_| EnvFilter::new(level));
    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        .with_target(false)
        .with_ansi(false)
        .try_init();
    Ok(LogGuard { _guard: guard })
}
