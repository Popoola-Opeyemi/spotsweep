//! Path handling and application locations.
//!
//! The executable deliberately has no compile-time knowledge of the source
//! repository.  Every location is derived from `$HOME` or an explicit override.

use anyhow::{Context, Result, bail};
use std::path::{Component, Path, PathBuf};

pub const APP_NAME: &str = "SpotlightDevExclusions";
pub const LAUNCH_AGENT_LABEL: &str = "dev.spotlightdevexclusions.agent";

pub const ENV_DATA_DIR: &str = "SPOTLIGHT_DEV_EXCLUSIONS_DATA_DIR";
pub const ENV_CONFIG: &str = "SPOTLIGHT_DEV_EXCLUSIONS_CONFIG";
pub const ENV_LAUNCH_AGENTS_DIR: &str = "SPOTLIGHT_DEV_EXCLUSIONS_LAUNCH_AGENTS_DIR";

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub data_dir: PathBuf,
    pub config_path: PathBuf,
    pub db_path: PathBuf,
    pub exports_dir: PathBuf,
    pub logs_dir: PathBuf,
    pub launch_agents_dir: PathBuf,
    pub plist_path: PathBuf,
}

impl AppPaths {
    /// Resolve all mutable locations.  Tests use `SPOTLIGHT_DEV_EXCLUSIONS_DATA_DIR`
    /// to keep every write inside a temporary directory.
    pub fn resolve(data_dir: Option<&Path>, config_override: Option<&Path>) -> Result<Self> {
        let home = home_dir()?;

        let data_dir = match data_dir {
            Some(p) => normalize_path(p)?,
            None => match std::env::var_os(ENV_DATA_DIR) {
                Some(v) => normalize_path(Path::new(&v))?,
                None => home.join("Library/Application Support").join(APP_NAME),
            },
        };

        let config_path = match config_override {
            Some(p) => normalize_path(p)?,
            None => match std::env::var_os(ENV_CONFIG) {
                Some(v) => normalize_path(Path::new(&v))?,
                None => data_dir.join("config.toml"),
            },
        };

        let using_default_data_dir =
            data_dir == home.join("Library/Application Support").join(APP_NAME);
        let logs_dir = if using_default_data_dir {
            home.join("Library/Logs").join(APP_NAME)
        } else {
            data_dir.join("logs")
        };

        let launch_agents_dir = match std::env::var_os(ENV_LAUNCH_AGENTS_DIR) {
            Some(v) => normalize_path(Path::new(&v))?,
            None => home.join("Library/LaunchAgents"),
        };

        let exports_dir = data_dir.join("exports");

        Ok(Self {
            db_path: data_dir.join("state.sqlite3"),
            plist_path: launch_agents_dir.join(format!("{LAUNCH_AGENT_LABEL}.plist")),
            data_dir,
            config_path,
            exports_dir,
            logs_dir,
            launch_agents_dir,
        })
    }

    /// Everything the installer, daemon and CLI may write lives below these
    /// directories.  Used by uninstall/state diagnostics.
    pub fn writable_dirs(&self) -> Vec<PathBuf> {
        vec![
            self.data_dir.clone(),
            self.exports_dir.clone(),
            self.logs_dir.clone(),
        ]
    }
}

pub fn home_dir() -> Result<PathBuf> {
    match std::env::var_os("HOME") {
        Some(h) if !h.is_empty() => Ok(PathBuf::from(h)),
        _ => bail!("HOME is not set; cannot resolve application paths"),
    }
}

/// Expand a leading `~` and return an absolute, lexically normalised path.
/// Symlinks are intentionally not resolved: the configured root must remain the
/// path the user chose, and symlink safety is handled by the scanner.
pub fn normalize_path(input: &Path) -> Result<PathBuf> {
    let expanded = expand_tilde(input);
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir()
            .context("cannot resolve a relative path without a current directory")?
            .join(expanded)
    };
    Ok(lexical_normalize(&absolute))
}

/// Expand only a leading `~/` (or a bare `~`).  Other `~user` forms are left
/// alone because they are not needed and are platform-specific.
pub fn expand_tilde(input: &Path) -> PathBuf {
    let Some(s) = input.to_str() else {
        return input.to_path_buf();
    };
    if s == "~" {
        return home_dir().unwrap_or_else(|_| input.to_path_buf());
    }
    if let Some(rest) = s.strip_prefix("~/") {
        if let Ok(home) = home_dir() {
            return home.join(rest);
        }
    }
    input.to_path_buf()
}

/// Collapse `.` and `..` without touching the filesystem.
pub fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push("/"),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            Component::Normal(part) => out.push(part),
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from("/")
    } else {
        out
    }
}

/// Render a path with `~` when it is inside the home directory.  This keeps the
/// config portable and keeps personal paths out of committed files.
pub fn display_path(path: &Path) -> String {
    if let Ok(home) = home_dir() {
        if let Ok(rest) = path.strip_prefix(&home) {
            if rest.as_os_str().is_empty() {
                return "~".to_string();
            }
            return format!("~/{}", rest.display());
        }
    }
    path.display().to_string()
}

pub fn path_within(path: &Path, root: &Path) -> bool {
    path == root || path.starts_with(root)
}

/// Roots that are clearly part of macOS itself.  This is intentionally
/// conservative: /Volumes and user-chosen /opt folders remain allowed.
pub fn is_system_path(path: &Path) -> bool {
    if path == Path::new("/") {
        return true;
    }
    const DENIED: &[&str] = &[
        "/System",
        "/Library",
        "/usr",
        "/bin",
        "/sbin",
        "/Applications",
        "/cores",
        "/dev",
        "/etc",
        "/private/etc",
        "/private/var/db",
        "/private/var/root",
        "/private/var/vm",
        "/opt/homebrew",
    ];
    DENIED.iter().any(|d| {
        let denied = Path::new(d);
        path == denied || path.starts_with(denied)
    })
}

/// Validate a user-supplied scan root.  The path does not have to exist yet;
/// callers warn about missing paths instead of failing the whole scan.
pub fn validate_scan_root(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        bail!("scan root must be an absolute path: {}", path.display());
    }
    if is_system_path(path) {
        bail!(
            "refusing to scan a macOS system location: {}",
            path.display()
        );
    }
    if path == home_dir()? {
        bail!("refusing to scan the entire home directory; add project-specific folders instead");
    }
    if let Ok(meta) = std::fs::metadata(path) {
        if !meta.is_dir() {
            bail!("scan root is not a directory: {}", path.display());
        }
    }
    Ok(())
}

pub fn is_hidden_name(name: &str) -> bool {
    name.starts_with('.') && name != "." && name != ".."
}

/// Current user id, used for `launchctl gui/<uid>`.
pub fn uid() -> u32 {
    // SAFETY: getuid is a side-effect-free libc query.
    unsafe { libc::getuid() }
}

pub fn is_root() -> bool {
    // SAFETY: geteuid is a side-effect-free libc query.
    unsafe { libc::geteuid() == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_dot_segments() {
        assert_eq!(
            lexical_normalize(Path::new("/a/./b/../c")),
            PathBuf::from("/a/c")
        );
    }

    #[test]
    fn rejects_system_roots() {
        assert!(validate_scan_root(Path::new("/usr/local")).is_err());
        assert!(validate_scan_root(Path::new("/")).is_err());
    }

    #[test]
    fn path_within_is_component_aware() {
        assert!(path_within(
            Path::new("/Users/a/Projects/x"),
            Path::new("/Users/a/Projects")
        ));
        assert!(!path_within(
            Path::new("/Users/a/Projects2/x"),
            Path::new("/Users/a/Projects")
        ));
    }
}
