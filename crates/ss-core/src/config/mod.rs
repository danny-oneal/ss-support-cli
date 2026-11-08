//! The `ss` config file: environment allowlist and diff defaults, plus the
//! precedence chain flag > env var > file value > default.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::vars::{self, Vars};

/// The fully resolved runtime environment an operation runs in.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Environment {
    /// Environment name.
    pub name: String,
    /// Cloud credentials profile.
    pub profile: String,
    /// Cloud region.
    pub region: String,
    /// Cloud account ID.
    pub account_id: String,
    /// Kubernetes context.
    pub kube_context: String,
    /// Whether the environment appears in the config file's allowlist.
    /// Mutating operations are refused when false.
    pub allowed: bool,
}

/// One entry in the environment allowlist.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct EnvSpec {
    /// Cloud credentials profile.
    pub profile: String,
    /// Cloud region.
    pub region: String,
    /// Cloud account ID.
    pub account_id: String,
    /// Kubernetes context.
    pub kube_context: String,
}

/// Per-installation diff rules.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DiffDefaults {
    /// Globs for fields that are pure noise.
    pub ignore: Vec<String>,
    /// Identity field per array path, for keyed array matching.
    pub array_match: BTreeMap<String, String>,
}

/// The parsed config file.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct File {
    /// Environment used when none is chosen explicitly.
    pub default_env: String,
    /// The environment allowlist, by name.
    pub environments: BTreeMap<String, EnvSpec>,
    /// Diff rules.
    pub diff: DiffDefaults,
}

/// Failure to load the config file.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file exists but could not be read.
    #[error("read config {}", .path.display())]
    Read {
        /// The config file path.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: io::Error,
    },
    /// The file is not valid config YAML.
    #[error("parse config {}", .path.display())]
    Parse {
        /// The config file path.
        path: PathBuf,
        /// The underlying YAML error.
        #[source]
        source: serde_yaml_ng::Error,
    },
}

/// Returns the config file location: `SS_CONFIG`, else
/// `$XDG_CONFIG_HOME/ss/config.yaml`, else `~/.config/ss/config.yaml`.
pub fn default_path(v: &Vars) -> PathBuf {
    if let Some(p) = vars::get(v, "SS_CONFIG") {
        return PathBuf::from(p);
    }
    vars::base_dir(v, "XDG_CONFIG_HOME", ".config")
        .join("ss")
        .join("config.yaml")
}

/// Reads the config file. A missing, empty, or comment-only file yields the
/// default config, under which no environment is allowlisted and only
/// read-only operations may run.
pub fn load(path: &Path) -> Result<File, ConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(File::default()),
        Err(source) => {
            return Err(ConfigError::Read {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let parsed: Option<File> =
        serde_yaml_ng::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
    Ok(parsed.unwrap_or_default())
}

/// Applies the precedence chain: the first non-empty of flag, env var, file
/// value wins, else `default`.
pub fn resolve(flag: Option<&str>, env_val: Option<&str>, file_val: &str, default: &str) -> String {
    [flag.unwrap_or(""), env_val.unwrap_or(""), file_val, default]
        .into_iter()
        .find(|s| !s.is_empty())
        .unwrap_or("")
        .to_owned()
}

impl File {
    /// Resolves the named environment against the allowlist. A non-empty
    /// `profile` or `region` overrides the allowlist entry's value.
    pub fn env(&self, name: &str, profile: &str, region: &str) -> Environment {
        let spec = self.environments.get(name);
        let mut e = Environment {
            name: name.to_owned(),
            allowed: !name.is_empty() && spec.is_some(),
            ..Environment::default()
        };
        if let Some(spec) = spec {
            e.profile.clone_from(&spec.profile);
            e.region.clone_from(&spec.region);
            e.account_id.clone_from(&spec.account_id);
            e.kube_context.clone_from(&spec.kube_context);
        }
        if !profile.is_empty() {
            profile.clone_into(&mut e.profile);
        }
        if !region.is_empty() {
            region.clone_into(&mut e.region);
        }
        e
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> Vars {
        pairs
            .iter()
            .map(|(k, val)| ((*k).to_owned(), (*val).to_owned()))
            .collect()
    }

    fn write(dir: &tempfile::TempDir, body: &str) -> PathBuf {
        let p = dir.path().join("config.yaml");
        std::fs::write(&p, body).expect("write config");
        p
    }

    #[test]
    fn resolve_precedence() {
        assert_eq!(resolve(Some("flag"), Some("env"), "file", "def"), "flag");
        assert_eq!(resolve(None, Some("env"), "file", "def"), "env");
        assert_eq!(resolve(Some(""), Some(""), "file", "def"), "file");
        assert_eq!(resolve(None, None, "", "def"), "def");
    }

    #[test]
    fn default_path_honors_ss_config_then_xdg_then_home() {
        assert_eq!(
            default_path(&vars(&[("SS_CONFIG", "/x/c.yaml")])),
            PathBuf::from("/x/c.yaml")
        );
        assert_eq!(
            default_path(&vars(&[("XDG_CONFIG_HOME", "/xdg")])),
            PathBuf::from("/xdg/ss/config.yaml")
        );
        assert_eq!(
            default_path(&vars(&[("HOME", "/h")])),
            PathBuf::from("/h/.config/ss/config.yaml")
        );
        assert_eq!(
            default_path(&vars(&[("SS_CONFIG", ""), ("HOME", "/h")])),
            PathBuf::from("/h/.config/ss/config.yaml")
        );
    }

    #[test]
    fn missing_file_is_empty_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = load(&dir.path().join("nope.yaml")).expect("load");
        assert_eq!(f, File::default());
    }

    #[test]
    fn empty_and_comment_only_files_are_empty_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        for body in ["", "# just a comment\n"] {
            let f = load(&write(&dir, body)).expect("load");
            assert_eq!(f, File::default(), "body {body:?}");
        }
    }

    #[test]
    fn unreadable_path_is_a_read_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = load(dir.path()).expect_err("directory is not a file");
        assert!(matches!(err, ConfigError::Read { .. }));
        assert!(err.to_string().starts_with("read config "));
    }

    #[test]
    fn load_and_env_resolution() {
        let dir = tempfile::tempdir().expect("tempdir");
        let body = "\
default_env: dev
environments:
  dev: {profile: acme-dev, region: us-west-2, account_id: \"111122223333\", kube_context: dev-cluster}
diff:
  ignore: [\"**.updatedAt\"]
  array_match: {\"$.rules[*]\": id}
";
        let f = load(&write(&dir, body)).expect("load");
        assert_eq!(f.default_env, "dev");
        assert_eq!(f.diff.ignore, vec!["**.updatedAt".to_owned()]);
        assert_eq!(f.diff.array_match["$.rules[*]"], "id");

        assert_eq!(
            f.env("dev", "", ""),
            Environment {
                name: "dev".into(),
                profile: "acme-dev".into(),
                region: "us-west-2".into(),
                account_id: "111122223333".into(),
                kube_context: "dev-cluster".into(),
                allowed: true,
            }
        );
        let over = f.env("dev", "other-profile", "eu-west-1");
        assert_eq!(over.profile, "other-profile");
        assert_eq!(over.region, "eu-west-1");
        assert!(over.allowed);

        assert!(!f.env("prod", "", "").allowed);
        assert!(!f.env("", "", "").allowed);
    }

    #[test]
    fn unknown_key_is_rejected_with_its_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = load(&write(&dir, "envs: {}\n")).expect_err("unknown key");
        assert!(matches!(err, ConfigError::Parse { .. }));
        let chain = format!("{:#}", anyhow::Error::from(err));
        assert!(chain.starts_with("parse config "), "{chain}");
        assert!(chain.contains("unknown field `envs`"), "{chain}");
    }
}
