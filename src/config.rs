//! Configuration loading, validation and mutation.

use crate::matcher::{RuleSet, RulesError};
use crate::paths::{AppPaths, display_path, expand_tilde, normalize_path, validate_scan_root};
use crate::util::atomic_write;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use thiserror::Error;

pub const DEFAULT_CONFIG: &str = include_str!("../config/default.toml");

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("unknown configuration key: {0}")]
    UnknownKey(String),
    #[error("invalid value for {key}: {message}")]
    InvalidValue { key: String, message: String },
    #[error("invalid configuration: {0}")]
    Invalid(String),
    #[error("invalid matching rule: {0}")]
    Rule(#[from] RulesError),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub scan: ScanConfig,
    pub rules: RulesConfig,
    pub export: ExportConfig,
    pub daemon: DaemonConfig,
    pub verify: VerifyConfig,
    pub exclusions: ExclusionsConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ScanConfig {
    pub roots: Vec<String>,
    pub exclude_paths: Vec<String>,
    pub follow_symlinks: bool,
    pub max_depth: usize,
    pub reconcile_interval_hours: u64,
    pub max_event_age_hours: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct RulesConfig {
    pub dependencies: Vec<String>,
    pub build: Vec<String>,
    pub generated: Vec<String>,
    pub optional: Vec<String>,
    pub dot_directories: Vec<String>,
    pub scan_dot_directories: bool,
    pub globs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ExportConfig {
    pub group_by_project: bool,
    pub include_header: bool,
    pub directory: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct DaemonConfig {
    pub event_latency_ms: u64,
    pub event_debounce_ms: u64,
    pub max_batch_events: usize,
    pub log_level: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct VerifyConfig {
    pub mdutil_volume: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ExclusionsConfig {
    /// Add newly discovered build directories to Spotlight's Search Privacy
    /// list automatically.  Requires a passing selftest on the current macOS
    /// build when `selftest_required` is true.
    pub auto: bool,
    /// Run the end-to-end selftest once per macOS build before enabling
    /// automatic exclusions in the background service.
    pub run_selftest_on_os_change: bool,
    /// If true, automatic exclusion does nothing until a selftest has passed
    /// for the current macOS build.  The CLI `sync` command can still be run
    /// explicitly by the user.
    pub selftest_required: bool,
    /// Remove managed exclusions whose directory no longer exists.  Entries
    /// added by the user in System Settings are never removed.
    pub remove_stale: bool,
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            roots: vec![
                "~/Projects".into(),
                "~/Developer".into(),
                "~/Code".into(),
                "~/Documents".into(),
                "~/Desktop".into(),
                "~/Work".into(),
            ],
            exclude_paths: Vec::new(),
            follow_symlinks: false,
            max_depth: 0,
            reconcile_interval_hours: 6,
            max_event_age_hours: 24,
        }
    }
}

impl Default for RulesConfig {
    fn default() -> Self {
        Self {
            dependencies: vec!["node_modules".into()],
            build: vec!["dist".into(), "build".into(), "out".into()],
            generated: vec!["coverage".into()],
            optional: vec!["vendor".into()],
            dot_directories: [
                ".next",
                ".nuxt",
                ".output",
                ".cache",
                ".turbo",
                ".vercel",
                ".parcel-cache",
                ".vite",
                ".svelte-kit",
                ".angular",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
            scan_dot_directories: false,
            globs: Vec::new(),
        }
    }
}

impl Default for ExportConfig {
    fn default() -> Self {
        Self {
            group_by_project: false,
            include_header: true,
            directory: String::new(),
        }
    }
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            event_latency_ms: 300,
            event_debounce_ms: 400,
            max_batch_events: 2000,
            log_level: "info".into(),
        }
    }
}

impl Default for VerifyConfig {
    fn default() -> Self {
        Self {
            mdutil_volume: "/".into(),
        }
    }
}

impl Default for ExclusionsConfig {
    fn default() -> Self {
        Self {
            auto: true,
            run_selftest_on_os_change: true,
            selftest_required: true,
            remove_stale: true,
        }
    }
}

impl Config {
    /// Load and validate the config.  If it does not exist, install the embedded
    /// default file first.  Existing user files are never overwritten.
    pub fn load_or_init(paths: &AppPaths) -> Result<Self> {
        if !paths.config_path.exists() {
            if let Some(parent) = paths.config_path.parent() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("creating configuration directory {}", parent.display())
                })?;
            }
            atomic_write(&paths.config_path, DEFAULT_CONFIG.as_bytes(), 0o600)?;
        }
        let text = std::fs::read_to_string(&paths.config_path)
            .with_context(|| format!("reading {}", paths.config_path.display()))?;
        let config: Config = toml::from_str(&text)
            .with_context(|| format!("parsing {}", paths.config_path.display()))?;
        config
            .validate()
            .with_context(|| format!("validating {}", paths.config_path.display()))?;
        Ok(config)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let config: Config =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        let text = toml::to_string_pretty(self).context("serialising configuration")?;
        atomic_write(path, text.as_bytes(), 0o600)
    }

    pub fn validate(&self) -> Result<()> {
        if self.scan.max_depth > 256 {
            return Err(ConfigError::InvalidValue {
                key: "scan.max_depth".into(),
                message: "must be 0 (unlimited) or <= 256".into(),
            }
            .into());
        }
        if !(1..=24 * 30).contains(&self.scan.reconcile_interval_hours) {
            return Err(ConfigError::InvalidValue {
                key: "scan.reconcile_interval_hours".into(),
                message: "must be between 1 and 720".into(),
            }
            .into());
        }
        if !(1..=24 * 90).contains(&self.scan.max_event_age_hours) {
            return Err(ConfigError::InvalidValue {
                key: "scan.max_event_age_hours".into(),
                message: "must be between 1 and 2160".into(),
            }
            .into());
        }
        if self.daemon.event_latency_ms > 5_000 {
            return Err(ConfigError::InvalidValue {
                key: "daemon.event_latency_ms".into(),
                message: "must be <= 5000".into(),
            }
            .into());
        }
        if self.daemon.event_debounce_ms > 10_000 {
            return Err(ConfigError::InvalidValue {
                key: "daemon.event_debounce_ms".into(),
                message: "must be <= 10000".into(),
            }
            .into());
        }
        if !(100..=100_000).contains(&self.daemon.max_batch_events) {
            return Err(ConfigError::InvalidValue {
                key: "daemon.max_batch_events".into(),
                message: "must be between 100 and 100000".into(),
            }
            .into());
        }
        if self.daemon.log_level.is_empty() {
            return Err(ConfigError::InvalidValue {
                key: "daemon.log_level".into(),
                message: "must not be empty".into(),
            }
            .into());
        }

        let mut seen_roots = HashSet::new();
        for root in &self.scan.roots {
            if root.trim().is_empty() {
                return Err(
                    ConfigError::Invalid("scan.roots contains an empty entry".into()).into(),
                );
            }
            if root.contains('\0') {
                return Err(ConfigError::Invalid(format!(
                    "scan root contains a NUL byte: {root:?}"
                ))
                .into());
            }
            let path = normalize_path(Path::new(root))
                .with_context(|| format!("invalid scan root {root:?}"))?;
            validate_scan_root(&path)?;
            let path = canonical_if_exists(&path);
            if !seen_roots.insert(path.clone()) {
                return Err(ConfigError::Invalid(format!(
                    "duplicate scan root: {}",
                    path.display()
                ))
                .into());
            }
        }

        for excluded in &self.scan.exclude_paths {
            if excluded.trim().is_empty() {
                return Err(ConfigError::Invalid(
                    "scan.exclude_paths contains an empty entry".into(),
                )
                .into());
            }
            normalize_path(Path::new(excluded))
                .with_context(|| format!("invalid excluded path {excluded:?}"))?;
        }

        // Compiles globs and validates all exact rule names, including
        // cross-category duplicate detection.
        let _ = RuleSet::from_config(&self.rules)?;
        Ok(())
    }

    /// Normalised scan roots in user order, deduplicated.  Existing roots are
    /// canonicalised so FSEvents event paths (which resolve symlinks such as
    /// /var -> /private/var) compare correctly with configured roots.
    pub fn root_paths(&self) -> Result<Vec<PathBuf>> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for root in &self.scan.roots {
            let path = canonical_if_exists(&normalize_path(Path::new(root))?);
            if seen.insert(path.clone()) {
                out.push(path);
            }
        }
        Ok(out)
    }

    pub fn excluded_paths(&self) -> Result<Vec<PathBuf>> {
        self.scan
            .exclude_paths
            .iter()
            .map(|p| normalize_path(Path::new(p)))
            .collect()
    }

    /// Add a root, storing a portable `~/...` representation where possible.
    /// Returns false for duplicates.
    pub fn add_root(&mut self, path: &Path) -> Result<bool> {
        let normalized = normalize_path(path)?;
        validate_scan_root(&normalized)?;
        let canonical = canonical_if_exists(&normalized);
        let existing = self.root_paths()?;
        if existing.iter().any(|p| p == &canonical) {
            return Ok(false);
        }
        let stored = display_path(&normalized);
        self.scan.roots.push(stored);
        Ok(true)
    }

    /// Remove a configured root.  Database history is intentionally untouched.
    pub fn remove_root(&mut self, path: &Path) -> Result<bool> {
        let normalized = canonical_if_exists(&normalize_path(path)?);
        let before = self.scan.roots.len();
        self.scan.roots.retain(|root| {
            normalize_path(&expand_tilde(Path::new(root)))
                .map(|p| canonical_if_exists(&p) != normalized)
                .unwrap_or(true)
        });
        Ok(self.scan.roots.len() != before)
    }

    pub fn export_dir(&self, paths: &AppPaths) -> Result<PathBuf> {
        if self.export.directory.trim().is_empty() {
            Ok(paths.exports_dir.clone())
        } else {
            normalize_path(Path::new(&self.export.directory))
        }
    }

    /// Small, explicit key/value API used by `spotlight-dev config set`.
    pub fn set_key(&mut self, key: &str, value: &str) -> Result<()> {
        let key = key.trim();
        let value = value.trim();
        let bool_value = || -> Result<bool> {
            match value.to_ascii_lowercase().as_str() {
                "true" | "yes" | "1" => Ok(true),
                "false" | "no" | "0" => Ok(false),
                _ => Err(ConfigError::InvalidValue {
                    key: key.to_string(),
                    message: "expected true or false".into(),
                }
                .into()),
            }
        };
        let u64_value = |min: u64, max: u64| -> Result<u64> {
            let parsed: u64 = value.parse().map_err(|_| ConfigError::InvalidValue {
                key: key.to_string(),
                message: "expected an integer".into(),
            })?;
            if !(min..=max).contains(&parsed) {
                return Err(ConfigError::InvalidValue {
                    key: key.to_string(),
                    message: format!("must be between {min} and {max}"),
                }
                .into());
            }
            Ok(parsed)
        };
        let list = || -> Vec<String> {
            value
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect()
        };

        match key {
            "scan.follow_symlinks" => self.scan.follow_symlinks = bool_value()?,
            "scan.max_depth" => {
                self.scan.max_depth = u64_value(0, 256)? as usize;
            }
            "scan.reconcile_interval_hours" => {
                self.scan.reconcile_interval_hours = u64_value(1, 720)?;
            }
            "scan.max_event_age_hours" => {
                self.scan.max_event_age_hours = u64_value(1, 2160)?;
            }
            "scan.exclude_paths" => self.scan.exclude_paths = list(),
            "rules.dependencies" => self.rules.dependencies = list(),
            "rules.build" => self.rules.build = list(),
            "rules.generated" => self.rules.generated = list(),
            "rules.optional" => self.rules.optional = list(),
            "rules.dot_directories" => self.rules.dot_directories = list(),
            "rules.globs" => self.rules.globs = list(),
            "rules.scan_dot_directories" => self.rules.scan_dot_directories = bool_value()?,
            "export.group_by_project" => self.export.group_by_project = bool_value()?,
            "export.include_header" => self.export.include_header = bool_value()?,
            "export.directory" => self.export.directory = value.to_string(),
            "daemon.event_latency_ms" => self.daemon.event_latency_ms = u64_value(0, 5000)?,
            "daemon.event_debounce_ms" => self.daemon.event_debounce_ms = u64_value(0, 10000)?,
            "daemon.max_batch_events" => {
                self.daemon.max_batch_events = u64_value(100, 100_000)? as usize;
            }
            "daemon.log_level" => {
                if value.is_empty() {
                    bail!("daemon.log_level must not be empty");
                }
                self.daemon.log_level = value.to_string();
            }
            "verify.mdutil_volume" => self.verify.mdutil_volume = value.to_string(),
            "exclusions.auto" => self.exclusions.auto = bool_value()?,
            "exclusions.run_selftest_on_os_change" => {
                self.exclusions.run_selftest_on_os_change = bool_value()?
            }
            "exclusions.selftest_required" => {
                self.exclusions.selftest_required = bool_value()?
            }
            "exclusions.remove_stale" => self.exclusions.remove_stale = bool_value()?,
            other => return Err(ConfigError::UnknownKey(other.to_string()).into()),
        }
        self.validate()?;
        Ok(())
    }

    pub fn redacted_toml(&self) -> Result<String> {
        toml::to_string_pretty(self).context("serialising configuration")
    }
}

fn canonical_if_exists(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid() {
        Config::default().validate().unwrap();
        let parsed: Config = toml::from_str(DEFAULT_CONFIG).unwrap();
        assert_eq!(parsed, Config::default());
    }

    #[test]
    fn glob_rules_are_validated() {
        let mut config = Config::default();
        config.rules.globs = vec!["*.egg-info".into()];
        config.validate().unwrap();
        config.rules.globs = vec!["dist/*".into()];
        assert!(config.validate().is_err());
    }

    #[test]
    fn duplicate_rule_names_are_rejected() {
        let mut config = Config::default();
        config.rules.build.push("node_modules".into());
        assert!(config.validate().is_err());
    }

    #[test]
    fn add_and_remove_roots_use_normalised_paths() {
        let mut config = Config::default();
        config.scan.roots.clear();
        let home = crate::paths::home_dir().unwrap();
        assert!(config.add_root(&home.join("Code")).unwrap());
        assert!(!config.add_root(&home.join("Code/")).unwrap());
        assert!(config.remove_root(&home.join("Code")).unwrap());
        assert!(config.scan.roots.is_empty());
    }

    #[test]
    fn set_key_changes_values_and_rejects_unknowns() {
        let mut config = Config::default();
        config.set_key("rules.build", "dist, build").unwrap();
        assert_eq!(config.rules.build, vec!["dist", "build"]);
        config.set_key("daemon.event_latency_ms", "500").unwrap();
        assert_eq!(config.daemon.event_latency_ms, 500);
        assert!(config.set_key("nope.nope", "1").is_err());
        assert!(config.set_key("scan.max_depth", "999").is_err());
    }
}
