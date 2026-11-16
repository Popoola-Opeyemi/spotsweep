//! User-level LaunchAgent management.
//!
//! The plist is generated at install time because it contains the installation
//! path and the user's data directory.  No generated plist is ever committed.
//! The agent runs as the logged-in user; this module refuses to operate as root.

use crate::paths::{AppPaths, LAUNCH_AGENT_LABEL, home_dir, is_root, uid};
use crate::util::{atomic_write, ensure_dir, xml_escape};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[derive(Debug, Clone, Serialize)]
pub struct ServiceStatus {
    pub installed: bool,
    pub loaded: bool,
    pub running: bool,
    pub pid: Option<u32>,
    pub plist: String,
    pub detail: String,
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

fn bootout() {
    let target = service_target();
    let _ = launchctl(&["bootout", &target]);
}

fn bootstrap(plist: &Path) -> Result<()> {
    let domain = domain();
    let plist_str = plist.to_string_lossy();
    let output = launchctl(&["bootstrap", &domain, &plist_str])?;
    if output.status.success() {
        let _ = launchctl(&["enable", &service_target()]);
        let _ = launchctl(&["kickstart", "-k", &service_target()]);
        return Ok(());
    }
    // Older macOS releases and some session configurations prefer `load`.
    let output = launchctl(&["load", "-w", &plist_str])?;
    if output.status.success() {
        return Ok(());
    }
    bail!(
        "launchctl could not load the agent: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    )
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
    atomic_write(&paths.plist_path, plist.as_bytes(), 0o644)?;
    bootstrap(&paths.plist_path)?;
    Ok(paths.plist_path.clone())
}

pub fn uninstall(paths: &AppPaths) -> Result<bool> {
    ensure_not_root()?;
    bootout();
    if paths.plist_path.exists() {
        std::fs::remove_file(&paths.plist_path)
            .with_context(|| format!("removing {}", paths.plist_path.display()))?;
        Ok(true)
    } else {
        Ok(false)
    }
}

pub fn start(paths: &AppPaths) -> Result<()> {
    ensure_not_root()?;
    if !paths.plist_path.exists() {
        bail!(
            "LaunchAgent is not installed at {}; run `spotlight-dev service install`",
            paths.plist_path.display()
        );
    }
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
    Ok(())
}

pub fn restart(paths: &AppPaths) -> Result<()> {
    ensure_not_root()?;
    if !paths.plist_path.exists() {
        bail!(
            "LaunchAgent is not installed at {}; run `spotlight-dev service install`",
            paths.plist_path.display()
        );
    }
    bootout();
    bootstrap(&paths.plist_path)
}

pub fn status(paths: &AppPaths) -> Result<ServiceStatus> {
    let installed = paths.plist_path.exists();
    let output = launchctl(&["print", &service_target()])?;
    if !output.status.success() {
        return Ok(ServiceStatus {
            installed,
            loaded: false,
            running: false,
            pid: None,
            plist: paths.plist_path.to_string_lossy().to_string(),
            detail: "not loaded".into(),
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
