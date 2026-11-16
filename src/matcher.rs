//! Directory-name matching rules.
//!
//! Exact names are the normal path.  Globs are optional and only ever match a
//! single directory name, never a path, so an accidental `**` cannot make the
//! scanner descend somewhere unexpected.  Matching is case-sensitive; that is
//! the safest choice on a case-sensitive volume and avoids treating `Build`
//! source folders as `build` artifacts.

use crate::config::RulesConfig;
use crate::model::RuleKind;
use globset::{GlobBuilder, GlobMatcher};
use std::collections::HashMap;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RulesError {
    #[error("directory rule name must not be empty")]
    EmptyName,
    #[error("directory rule {0:?} must not contain '/' or NUL")]
    InvalidName(String),
    #[error("dot-directory rule {0:?} must start with '.'")]
    NotDot(String),
    #[error("rule {0:?} starts with '.'; add it to rules.dot_directories instead")]
    UnexpectedDot(String),
    #[error("directory name {name:?} is configured in both {first} and {second}")]
    Duplicate {
        name: String,
        first: String,
        second: String,
    },
    #[error("invalid glob {pattern:?}: {source}")]
    InvalidGlob {
        pattern: String,
        #[source]
        source: globset::Error,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleMatch {
    pub pattern: String,
    pub kind: RuleKind,
}

#[derive(Debug, Clone)]
struct GlobRule {
    pattern: String,
    matcher: GlobMatcher,
    kind: RuleKind,
}

#[derive(Debug, Clone)]
pub struct RuleSet {
    exact: HashMap<String, RuleMatch>,
    globs: Vec<GlobRule>,
    scan_dot_directories: bool,
    all: Vec<RuleMatch>,
}

impl RuleSet {
    pub fn from_config(config: &RulesConfig) -> Result<Self, RulesError> {
        let mut exact: HashMap<String, RuleMatch> = HashMap::new();
        let mut all = Vec::new();

        let categories: [(&str, RuleKind, &Vec<String>); 5] = [
            ("dependencies", RuleKind::Dependencies, &config.dependencies),
            ("build", RuleKind::Build, &config.build),
            ("generated", RuleKind::Generated, &config.generated),
            ("optional", RuleKind::Optional, &config.optional),
            (
                "dot_directories",
                RuleKind::DotDirectory,
                &config.dot_directories,
            ),
        ];

        for (category, kind, names) in categories {
            for raw in names {
                let name = raw.trim();
                if name.is_empty() {
                    return Err(RulesError::EmptyName);
                }
                if name.contains('/') || name.contains('\0') {
                    return Err(RulesError::InvalidName(name.to_string()));
                }
                if kind == RuleKind::DotDirectory {
                    if !name.starts_with('.') {
                        return Err(RulesError::NotDot(name.to_string()));
                    }
                } else if name.starts_with('.') {
                    return Err(RulesError::UnexpectedDot(name.to_string()));
                }

                let rule = RuleMatch {
                    pattern: name.to_string(),
                    kind,
                };
                if let Some(existing) = exact.get(name) {
                    return Err(RulesError::Duplicate {
                        name: name.to_string(),
                        first: existing.kind.as_str().to_string(),
                        second: category.to_string(),
                    });
                }
                exact.insert(name.to_string(), rule.clone());
                all.push(rule);
            }
        }

        let mut globs = Vec::new();
        for raw in &config.globs {
            let pattern = raw.trim();
            if pattern.is_empty() {
                return Err(RulesError::EmptyName);
            }
            if pattern.contains('/') || pattern.contains('\0') {
                return Err(RulesError::InvalidName(pattern.to_string()));
            }
            if pattern.starts_with('.') {
                return Err(RulesError::UnexpectedDot(pattern.to_string()));
            }
            let glob = GlobBuilder::new(pattern)
                .literal_separator(true)
                .backslash_escape(false)
                .case_insensitive(false)
                .build()
                .map_err(|source| RulesError::InvalidGlob {
                    pattern: pattern.to_string(),
                    source,
                })?;
            let rule = RuleMatch {
                pattern: pattern.to_string(),
                kind: RuleKind::Glob,
            };
            globs.push(GlobRule {
                pattern: pattern.to_string(),
                matcher: glob.compile_matcher(),
                kind: RuleKind::Glob,
            });
            all.push(rule);
        }

        Ok(Self {
            exact,
            globs,
            scan_dot_directories: config.scan_dot_directories,
            all,
        })
    }

    pub fn classify(&self, name: &str) -> Option<RuleMatch> {
        if name.starts_with('.') && !self.scan_dot_directories {
            return None;
        }
        if let Some(rule) = self.exact.get(name) {
            return Some(rule.clone());
        }
        self.globs
            .iter()
            .find(|rule| rule.matcher.is_match(name))
            .map(|rule| RuleMatch {
                pattern: rule.pattern.clone(),
                kind: rule.kind,
            })
    }

    pub fn is_match(&self, name: &str) -> bool {
        self.classify(name).is_some()
    }

    pub fn all_rules(&self) -> &[RuleMatch] {
        &self.all
    }

    pub fn scan_dot_directories(&self) -> bool {
        self.scan_dot_directories
    }

    pub fn dot_names(&self) -> Vec<String> {
        self.all
            .iter()
            .filter(|rule| rule.kind == RuleKind::DotDirectory)
            .map(|rule| rule.pattern.clone())
            .collect()
    }

    /// Version-control metadata is never a useful Spotlight exclusion and can
    /// be enormous; prune it even when dot-directory scanning is enabled.
    pub fn is_vcs_metadata(name: &str) -> bool {
        matches!(name, ".git" | ".svn" | ".hg" | ".bzr" | "_darcs")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> RulesConfig {
        RulesConfig::default()
    }

    #[test]
    fn exact_names_match_by_category() {
        let rules = RuleSet::from_config(&config()).unwrap();
        assert_eq!(
            rules.classify("node_modules").unwrap().kind,
            RuleKind::Dependencies
        );
        assert_eq!(rules.classify("dist").unwrap().kind, RuleKind::Build);
        assert_eq!(
            rules.classify("coverage").unwrap().kind,
            RuleKind::Generated
        );
        assert_eq!(rules.classify("vendor").unwrap().kind, RuleKind::Optional);
        assert!(rules.classify("src").is_none());
        assert!(rules.classify("Node_Modules").is_none());
    }

    #[test]
    fn dot_directories_are_disabled_by_default() {
        let rules = RuleSet::from_config(&config()).unwrap();
        assert!(rules.classify(".next").is_none());
        assert!(!rules.scan_dot_directories());
    }

    #[test]
    fn dot_directories_can_be_enabled() {
        let mut config = config();
        config.scan_dot_directories = true;
        let rules = RuleSet::from_config(&config).unwrap();
        assert_eq!(
            rules.classify(".next").unwrap().kind,
            RuleKind::DotDirectory
        );
        assert!(rules.classify(".unknown-cache").is_none());
    }

    #[test]
    fn globs_match_names_not_paths() {
        let mut config = config();
        config.globs = vec!["*-cache".into(), "dist-20??".into()];
        let rules = RuleSet::from_config(&config).unwrap();
        assert_eq!(rules.classify("vite-cache").unwrap().kind, RuleKind::Glob);
        assert_eq!(rules.classify("dist-2024").unwrap().kind, RuleKind::Glob);
        assert!(rules.classify("dist-2024/nested").is_none());
        assert!(rules.classify("other/dist-2024").is_none());
    }

    #[test]
    fn invalid_rules_are_rejected() {
        let mut config = config();
        config.build.push("node_modules".into());
        assert!(matches!(
            RuleSet::from_config(&config),
            Err(RulesError::Duplicate { .. })
        ));

        let config = RulesConfig {
            build: vec!["a/b".into()],
            ..Default::default()
        };
        assert!(matches!(
            RuleSet::from_config(&config),
            Err(RulesError::InvalidName(_))
        ));

        let config = RulesConfig {
            dot_directories: vec!["next".into()],
            ..Default::default()
        };
        assert!(matches!(
            RuleSet::from_config(&config),
            Err(RulesError::NotDot(_))
        ));
    }
}
