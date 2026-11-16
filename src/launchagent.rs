//! User-level LaunchAgent management.
//!
//! The plist is generated at install time because it contains the installation
//! path and the user's data directory.  No generated plist is ever committed.
//! The agent runs as the logged-in user; this module refuses to operate as root.

use crate::paths::{
    AppPaths, LAUNCH_AGENT_LABEL, LEGACY_LAUNCH_AGENT_LABEL, home_dir, is_root, uid,
};
use crate::util::{atomic_write, ensure_dir, xml_escape};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

#[derive(Debug, Clone, Serialize)]
pub struct ServiceStatus {
    pub installed: bool,
    pub loaded: bool,
    pub running: bool,
    pub pid: Option<u32>,
    pub plist: String,
    pub detail: String,
    pub legacy_installed: bool,
    pub legacy_loaded: bool,
}

pub fn render_plist(binary: &Path, paths: &AppPaths, config_path: &Path) -> Result<String> {
    let home = home_dir()?;
    let binary = binary
        .to_str()
        .context("installation path is not valid UTF-8")?;
    let data_dir = paths
        .data_dir
        .to_str()
        .context("data directory is not valid UTF-8")?;
    let config = config_path
        .to_str()
        .context("config path is not valid UTF-8")?;
    let logs = paths
        .logs_dir
        .to_str()
        .context("log directory is not valid UTF-8")?;
    let home = home.to_str().context("home directory is not valid UTF-8")?;

    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{binary}</string>
        <string>--data-dir</string>
        <string>{data_dir}</string>
        <string>--config</string>
        <string>{config}</string>
        <string>daemon</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>ProcessType</key>
    <string>Background</string>
    <key>Nice</key>
    <integer>10</integer>
    <key>ThrottleInterval</key>
    <integer>10</integer>
    <key>StandardOutPath</key>
    <string>{logs}/launchd.out.log</string>
    <key>StandardErrorPath</key>
    <string>{logs}/launchd.err.log</string>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>/usr/bin:/bin:/usr/sbin:/sbin</string>
        <key>HOME</key>
        <string>{home}</string>
    </dict>
</dict>
</plist>
"#,
        label = xml_escape(LAUNCH_AGENT_LABEL),
        binary = xml_escape(binary),
        data_dir = xml_escape(data_dir),
        config = xml_escape(config),
        logs = xml_escape(logs),
        home = xml_escape(home),
    ))
}

fn domain() -> String {
    format!("gui/{}", uid())
}

fn service_target() -> String {
    format!("{}/{}", domain(), LAUNCH_AGENT_LABEL)
}

fn legacy_service_target() -> String {
    format!("{}/{}", domain(), LEGACY_LAUNCH_AGENT_LABEL)
}

fn target_loaded(target: &str) -> bool {
    launchctl(&["print", target])
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn ensure_not_root() -> Result<()> {
    if is_root() {
        bail!(
            "LaunchAgent management must run as your logged-in user, not root. Re-run without sudo."
        );
    }
    Ok(())
}

fn launchctl(args: &[&str]) -> Result<Output> {
    Command::new("/bin/launchctl")
        .args(args)
        .output()
        .with_context(|| format!("running /bin/launchctl {}", args.join(" ")))
}

fn bootout_target(target: &str) {
    let _ = launchctl(&["bootout", target]);
}

fn bootout() {
    bootout_target(&service_target());
}

fn bootout_legacy() {
    bootout_target(&legacy_service_target());
}

fn remove_legacy_plist(paths: &AppPaths) {
    if let Some(path) = &paths.legacy_plist_path {
        if path.exists() {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn restore_legacy_if_available(paths: &AppPaths) {
    if let Some(path) = &paths.legacy_plist_path {
        if path.exists() {
            let _ = bootstrap_target(path, &legacy_service_target());
        }
    }
}

/// Bootstrap the agent, retrying while launchd finishes terminating the old
/// instance.  `bootout` followed immediately by `bootstrap` can otherwise
/// report success through the legacy `load` fallback without actually loading
/// the job.
fn bootstrap(plist: &Path) -> Result<()> {
    bootstrap_target(plist, &service_target())
}

fn bootstrap_target(plist: &Path, target: &str) -> Result<()> {
    let domain = domain();
    let plist_str = plist.to_string_lossy();
    let mut last_error = String::new();

    for attempt in 0..12 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(300));
        }
        let output = launchctl(&["bootstrap", &domain, &plist_str])?;
        if output.status.success() {
            let _ = launchctl(&["enable", target]);
            let _ = launchctl(&["kickstart", "-k", target]);
            if target_loaded(target) {
                return Ok(());
            }
        } else {
            last_error = String::from_utf8_lossy(&output.stderr).trim().to_string();
        }
    }

    // Older macOS releases and some session configurations prefer `load`.
    for attempt in 0..4 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(300));
        }
        let output = launchctl(&["load", "-w", &plist_str])?;
        if output.status.success() && target_loaded(target) {
            return Ok(());
        }
        if !output.status.success() {
            last_error = String::from_utf8_lossy(&output.stderr).trim().to_string();
        }
    }

    bail!("launchctl could not load the agent: {last_error}")
}

pub fn install(binary: &Path, paths: &AppPaths, config_path: &Path) -> Result<PathBuf> {
    ensure_not_root()?;
    if !binary.is_file() {
        bail!("installed executable does not exist: {}", binary.display());
    }
    let plist = render_plist(binary, paths, config_path)?;
    ensure_dir(&paths.launch_agents_dir, 0o755)?;
    ensure_dir(&paths.logs_dir, 0o700)?;
    bootout();
    bootout_legacy();
    atomic_write(&paths.plist_path, plist.as_bytes(), 0o644)?;
    if let Err(error) = bootstrap(&paths.plist_path) {
        restore_legacy_if_available(paths);
        return Err(error);
    }
    // Only remove the old agent after the new one is confirmed loaded.
    remove_legacy_plist(paths);
    Ok(paths.plist_path.clone())
}

pub fn uninstall(paths: &AppPaths) -> Result<bool> {
    ensure_not_root()?;
    bootout();
    bootout_legacy();
    let had_new = paths.plist_path.exists();
    let had_legacy = paths
        .legacy_plist_path
        .as_ref()
        .map(|path| path.exists())
        .unwrap_or(false);
    if had_new {
        std::fs::remove_file(&paths.plist_path)
            .with_context(|| format!("removing {}", paths.plist_path.display()))?;
    }
    remove_legacy_plist(paths);
    Ok(had_new || had_legacy)
}

pub fn start(paths: &AppPaths) -> Result<()> {
    ensure_not_root()?;
    if !paths.plist_path.exists() {
        bail!(
            "LaunchAgent is not installed at {}; run `spotsweep service install`",
            paths.plist_path.display()
        );
    }
    // Never leave the legacy daemon running beside the new one.
    bootout_legacy();
    remove_legacy_plist(paths);
    if let Ok(status) = status(paths) {
        if status.loaded {
            let _ = launchctl(&["kickstart", "-k", &service_target()]);
            return Ok(());
        }
    }
    bootstrap(&paths.plist_path)
}

pub fn stop() -> Result<()> {
    ensure_not_root()?;
    bootout();
    bootout_legacy();
    Ok(())
}

/// Stop the pre-rename agent before its database is copied.  Called by the
/// migration path so a direct `spotsweep` command cannot race an old daemon.
pub fn stop_legacy_agent() {
    if !is_root() {
        bootout_legacy();
    }
}

pub fn legacy_agent_loaded() -> bool {
    !is_root() && target_loaded(&legacy_service_target())
}

pub fn restart(paths: &AppPaths) -> Result<()> {
    ensure_not_root()?;
    if !paths.plist_path.exists() {
        bail!(
            "LaunchAgent is not installed at {}; run `spotsweep service install`",
            paths.plist_path.display()
        );
    }
    bootout();
    bootout_legacy();
    if let Err(error) = bootstrap(&paths.plist_path) {
        restore_legacy_if_available(paths);
        return Err(error);
    }
    remove_legacy_plist(paths);
    Ok(())
}

pub fn status(paths: &AppPaths) -> Result<ServiceStatus> {
    let installed = paths.plist_path.exists();
    let legacy_installed = paths
        .legacy_plist_path
        .as_ref()
        .map(|path| path.exists())
        .unwrap_or(false);
    let legacy_loaded = target_loaded(&legacy_service_target());
    let output = launchctl(&["print", &service_target()])?;
    if !output.status.success() {
        return Ok(ServiceStatus {
            installed,
            loaded: false,
            running: false,
            pid: None,
            plist: paths.plist_path.to_string_lossy().to_string(),
            detail: "not loaded".into(),
            legacy_installed,
            legacy_loaded,
        });
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut running = false;
    let mut pid = None;
    for line in text.lines() {
        let line = line.trim();
        if line == "state = running" {
            running = true;
        }
        if let Some(value) = line.strip_prefix("pid = ") {
            pid = value.trim().parse::<u32>().ok();
        }
    }
    Ok(ServiceStatus {
        installed,
        loaded: true,
        running: running || pid.is_some(),
        pid,
        plist: paths.plist_path.to_string_lossy().to_string(),
        detail: if running {
            "running".into()
        } else {
            "loaded, not running".into()
        },
        legacy_installed,
        legacy_loaded,
    })
}

/// Ask a running daemon to reload config after add-path/remove-path/config set.
/// Failure is intentionally silent: the service may not be installed yet.
pub fn signal_reload() {
    if is_root() {
        return;
    }
    let _ = launchctl(&["kill", "SIGHUP", &service_target()]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> AppPaths {
        let data = PathBuf::from("/Users/test/Library/Application Support/SpotSweep");
        let agents = PathBuf::from("/Users/test/Library/LaunchAgents");
        AppPaths {
            config_path: data.join("config.toml"),
            db_path: data.join("spotsweep.db"),
            exports_dir: data.join("exports"),
            logs_dir: PathBuf::from("/Users/test/Library/Logs/SpotSweep"),
            plist_path: agents.join("com.spotsweep.agent.plist"),
            data_dir: data,
            launch_agents_dir: agents.clone(),
            legacy_data_dir: Some(PathBuf::from(
                "/Users/test/Library/Application Support/SpotlightDevExclusions",
            )),
            legacy_db_path: Some(PathBuf::from(
                "/Users/test/Library/Application Support/SpotlightDevExclusions/state.sqlite3",
            )),
            legacy_config_path: Some(PathBuf::from(
                "/Users/test/Library/Application Support/SpotlightDevExclusions/config.toml",
            )),
            legacy_logs_dir: Some(PathBuf::from(
                "/Users/test/Library/Logs/SpotlightDevExclusions",
            )),
            legacy_plist_path: Some(agents.join("dev.spotlightdevexclusions.agent.plist")),
        }
    }

    #[test]
    fn plist_uses_spotsweep_identifiers() {
        let paths = paths();
        let plist = render_plist(
            Path::new("/Users/test/.local/bin/spotsweep"),
            &paths,
            &paths.config_path,
        )
        .unwrap();
        assert!(plist.contains("com.spotsweep.agent"));
        assert!(plist.contains("spotsweep.db") || plist.contains("SpotSweep"));
        assert!(!plist.contains("dev.spotlightdevexclusions.agent"));
        assert!(plist.contains("/Users/test/Library/Logs/SpotSweep"));
    }
}
