//! `sleepwalk.toml` configuration.
//!
//! Every key is optional; an absent file or an absent key falls back to the
//! documented default (see `sleepwalk.example.toml`). Unknown keys are rejected
//! so a typo never silently does nothing.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

/// The full configuration.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Where per-VM state directories live.
    pub state_dir: PathBuf,
    /// Quiescence-detector thresholds.
    pub quiescence: Quiescence,
    /// Migration behaviour.
    pub migration: Migration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            state_dir: PathBuf::from("/var/lib/sleepwalk"),
            quiescence: Quiescence::default(),
            migration: Migration::default(),
        }
    }
}

/// Quiescence thresholds (the `[quiescence]` table).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Quiescence {
    /// A vCPU sample below this percent counts as quiet.
    pub cpu_pct: f64,
    /// Consecutive quiet samples required before the infra layer is quiescent.
    pub samples: usize,
    /// Milliseconds between samples.
    pub sample_interval_ms: u64,
}

impl Default for Quiescence {
    fn default() -> Self {
        Self {
            cpu_pct: 5.0,
            samples: 5,
            sample_interval_ms: 200,
        }
    }
}

/// Migration behaviour (the `[migration]` table).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Migration {
    /// How long to wait for an in-flight turn before aborting a drain.
    pub drain_deadline_ms: u64,
}

impl Default for Migration {
    fn default() -> Self {
        Self {
            drain_deadline_ms: 5000,
        }
    }
}

/// A failure loading configuration.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// The config file could not be read.
    #[error("reading config {path}: {source}")]
    Read {
        /// The path that failed.
        path: String,
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// The config file was not valid TOML / had unknown or mistyped keys.
    #[error("parsing config {path}: {source}")]
    Parse {
        /// The path that failed.
        path: String,
        /// The underlying parse error.
        source: toml::de::Error,
    },
}

impl Config {
    /// Load from `path`. A missing file yields the defaults (configuration is
    /// optional); a present-but-invalid file is an error.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.display().to_string(),
            source,
        })?;
        Self::from_toml(&text, path)
    }

    /// Parse from a TOML string, tagging errors with `path` for the message.
    pub fn from_toml(text: &str, path: &Path) -> Result<Self, ConfigError> {
        toml::from_str(text).map_err(|source| ConfigError::Parse {
            path: path.display().to_string(),
            source,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Config, ConfigError> {
        Config::from_toml(text, Path::new("test.toml"))
    }

    #[test]
    fn empty_config_is_all_defaults() {
        assert_eq!(parse("").expect("parse"), Config::default());
    }

    #[test]
    fn partial_config_keeps_other_defaults() {
        let c = parse("state_dir = \"/srv/sw\"\n[quiescence]\ncpu_pct = 2.5\n").expect("parse");
        assert_eq!(c.state_dir, PathBuf::from("/srv/sw"));
        assert_eq!(c.quiescence.cpu_pct, 2.5);
        // Untouched keys keep their defaults.
        assert_eq!(c.quiescence.samples, 5);
        assert_eq!(c.migration.drain_deadline_ms, 5000);
    }

    #[test]
    fn unknown_key_is_rejected() {
        let err = parse("totally_made_up = 1\n").expect_err("must reject");
        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    #[test]
    fn missing_file_loads_defaults() {
        let c = Config::load(Path::new("/no/such/sleepwalk.toml")).expect("missing is ok");
        assert_eq!(c, Config::default());
    }
}
