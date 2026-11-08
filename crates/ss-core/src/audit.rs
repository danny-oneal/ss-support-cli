//! JSONL audit trail.

use std::collections::BTreeMap;
use std::fs::{DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::vars::{self, Vars};

/// How an audited operation ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Outcome {
    /// Completed successfully.
    #[serde(rename = "ok")]
    Ok,
    /// Failed while running.
    #[serde(rename = "error")]
    Error,
    /// Refused by the guard.
    #[serde(rename = "denied")]
    Denied,
    /// Previewed without changing anything.
    #[serde(rename = "dry-run")]
    DryRun,
    /// Declined at confirmation.
    #[serde(rename = "declined")]
    Declined,
}

/// One audit record.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    /// When the operation finished.
    pub time: DateTime<Utc>,
    /// Operator who ran it.
    pub user: String,
    /// Resolved environment.
    pub env: String,
    /// Account the operation targeted; omitted when empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub account: String,
    /// Identifier of the operation.
    #[serde(rename = "op")]
    pub op_id: String,
    /// Parameters it ran with; omitted when empty.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, String>,
    /// How it ended.
    pub outcome: Outcome,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: i64,
    /// Failure message; omitted when there was none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Records audit entries.
pub trait Logger: Send + Sync {
    /// Persists one entry.
    fn record(&self, e: &Entry) -> io::Result<()>;
}

/// Discards every entry.
#[derive(Clone, Copy, Debug, Default)]
pub struct Nop;

impl Logger for Nop {
    fn record(&self, _e: &Entry) -> io::Result<()> {
        Ok(())
    }
}

/// Appends JSONL entries to a file, creating parent directories on first
/// write. Directories are private to the user (0700) and the file is 0600.
#[derive(Debug)]
pub struct FileLogger {
    path: PathBuf,
    lock: Mutex<()>,
}

impl FileLogger {
    /// A logger that appends to `path`.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            lock: Mutex::new(()),
        }
    }
}

impl Logger for FileLogger {
    fn record(&self, e: &Entry) -> io::Result<()> {
        let mut line = serde_json::to_string(e).map_err(io::Error::from)?;
        line.push('\n');

        // A poisoned lock guards nothing, so a panicked writer is harmless.
        let _guard = self.lock.lock().unwrap_or_else(|p| p.into_inner());

        if let Some(parent) = self.path.parent().filter(|p| !p.as_os_str().is_empty()) {
            let mut dirs = DirBuilder::new();
            dirs.recursive(true);
            #[cfg(unix)]
            std::os::unix::fs::DirBuilderExt::mode(&mut dirs, 0o700);
            dirs.create(parent)?;
        }

        let mut opts = OpenOptions::new();
        opts.append(true).create(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        opts.open(&self.path)?.write_all(line.as_bytes())
    }
}

/// The default audit log location: `$XDG_STATE_HOME/ss/audit.log`, else
/// `~/.local/state/ss/audit.log`.
pub fn default_path(v: &Vars) -> PathBuf {
    vars::base_dir(v, "XDG_STATE_HOME", ".local/state")
        .join("ss")
        .join("audit.log")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(op: &str) -> Entry {
        Entry {
            time: Utc::now(),
            user: "u".into(),
            env: "dev".into(),
            account: "123".into(),
            op_id: op.into(),
            params: BTreeMap::from([("k".to_owned(), "v".to_owned())]),
            outcome: Outcome::Ok,
            duration_ms: 7,
            error: Some("boom".into()),
        }
    }

    fn keys(line: &str) -> Vec<String> {
        // serde_json without preserve_order sorts keys, so recover the
        // on-the-wire order by scanning for top-level `"key":` positions.
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        let mut found: Vec<(usize, String)> = v
            .as_object()
            .unwrap()
            .keys()
            .map(|k| (line.find(&format!("\"{k}\":")).unwrap(), k.clone()))
            .collect();
        found.sort();
        found.into_iter().map(|(_, k)| k).collect()
    }

    #[test]
    fn file_logger_appends_jsonl_with_stable_field_order() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a/b/audit.log");
        let log = FileLogger::new(&path);

        log.record(&entry("one")).unwrap();
        let mut bare = entry("two");
        bare.account = String::new();
        bare.params.clear();
        bare.error = None;
        bare.outcome = Outcome::DryRun;
        log.record(&bare).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with(r#"{"time":""#));
        assert_eq!(
            keys(lines[0]),
            [
                "time",
                "user",
                "env",
                "account",
                "op",
                "params",
                "outcome",
                "duration_ms",
                "error"
            ]
        );
        assert_eq!(
            keys(lines[1]),
            ["time", "user", "env", "op", "outcome", "duration_ms"]
        );
        assert!(lines[1].contains(r#""outcome":"dry-run""#));

        let back: Entry = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(back.op_id, "one");
        assert_eq!(back.error.as_deref(), Some("boom"));
        let back: Entry = serde_json::from_str(lines[1]).unwrap();
        assert!(back.account.is_empty() && back.params.is_empty() && back.error.is_none());
    }

    #[test]
    fn every_outcome_has_a_stable_name() {
        let names: Vec<String> = [
            Outcome::Ok,
            Outcome::Error,
            Outcome::Denied,
            Outcome::DryRun,
            Outcome::Declined,
        ]
        .iter()
        .map(|o| serde_json::to_string(o).unwrap())
        .collect();
        assert_eq!(
            names,
            [
                r#""ok""#,
                r#""error""#,
                r#""denied""#,
                r#""dry-run""#,
                r#""declined""#
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn file_logger_creates_private_files() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("state");
        let path = dir.join("audit.log");
        FileLogger::new(&path).record(&entry("x")).unwrap();

        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn file_logger_reports_unwritable_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let blocker = tmp.path().join("file");
        std::fs::write(&blocker, "x").unwrap();
        let log = FileLogger::new(blocker.join("audit.log"));
        assert!(log.record(&entry("x")).is_err());
    }

    #[test]
    fn default_path_honors_xdg_state_home() {
        let v = Vars::from([("XDG_STATE_HOME".to_owned(), "/s".to_owned())]);
        assert_eq!(default_path(&v), PathBuf::from("/s/ss/audit.log"));
        let v = Vars::from([("HOME".to_owned(), "/h".to_owned())]);
        assert_eq!(
            default_path(&v),
            PathBuf::from("/h/.local/state/ss/audit.log")
        );
    }
}
