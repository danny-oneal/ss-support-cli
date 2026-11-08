//! Named parameter sets per operation, kept in a small user-editable YAML
//! file so a frequently used bucket, key, or table combination is saved once
//! and replayed by name.

use std::collections::BTreeMap;
use std::fs::{self, DirBuilder};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize};

use crate::vars::{self, Vars};

/// Failure to read, parse, change, or write the presets file.
#[derive(Debug, thiserror::Error)]
pub enum PresetError {
    /// The file exists but could not be read.
    #[error("read presets {}", .path.display())]
    Read {
        /// The presets file path.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: io::Error,
    },
    /// The file is not valid presets YAML.
    #[error("parse presets {}", .path.display())]
    Parse {
        /// The presets file path.
        path: PathBuf,
        /// The underlying YAML error.
        #[source]
        source: serde_yaml_ng::Error,
    },
    /// The file could not be written.
    #[error("write presets {}", .path.display())]
    Write {
        /// The presets file path.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: io::Error,
    },
    /// The operation ID or preset name was empty.
    #[error("operation and preset name are required")]
    MissingName,
    /// Every parameter value was empty.
    #[error("refusing to save an empty preset")]
    Empty,
    /// The operation has no preset with that name.
    #[error("{op} has no preset {name:?}")]
    NotFound {
        /// The operation ID.
        op: String,
        /// The requested preset name.
        name: String,
    },
}

impl PresetError {
    /// Reports whether the failure is the caller's input or the user's file
    /// content (as opposed to an I/O failure).
    pub fn is_usage(&self) -> bool {
        matches!(
            self,
            Self::Parse { .. } | Self::MissingName | Self::Empty | Self::NotFound { .. }
        )
    }
}

/// Checks that `params` could be saved as preset `name` of `op`: both
/// names are non-empty and at least one value is.
pub fn validate(
    op: &str,
    name: &str,
    params: &BTreeMap<String, String>,
) -> Result<(), PresetError> {
    if op.is_empty() || name.is_empty() {
        return Err(PresetError::MissingName);
    }
    if params.values().all(String::is_empty) {
        return Err(PresetError::Empty);
    }
    Ok(())
}

/// The presets file location: `SS_PRESETS`, else
/// `$XDG_CONFIG_HOME/ss/presets.yaml`, else `~/.config/ss/presets.yaml`.
pub fn default_path(v: &Vars) -> PathBuf {
    if let Some(p) = vars::get(v, "SS_PRESETS") {
        return PathBuf::from(p);
    }
    vars::base_dir(v, "XDG_CONFIG_HOME", ".config")
        .join("ss")
        .join("presets.yaml")
}

/// Operation ID, then preset name, then parameter name and value.
type Presets = BTreeMap<String, BTreeMap<String, BTreeMap<String, String>>>;

/// The on-disk shape: `presets: {<op>: {<name>: {<param>: <value>}}}`.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct File {
    #[serde(deserialize_with = "null_as_default")]
    presets: Presets,
}

/// Treats an explicit YAML null (a bare `presets:` line) as an empty map.
fn null_as_default<'de, D>(d: D) -> Result<Presets, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Option::<Presets>::deserialize(d)?.unwrap_or_default())
}

/// Reads and writes the presets file. The file is read on every call, so
/// hand edits are picked up immediately.
#[derive(Debug, Clone)]
pub struct Store {
    path: PathBuf,
}

impl Store {
    /// Creates a store backed by the file at `path`.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The backing file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the named preset's parameters for an operation.
    pub fn get(
        &self,
        op: &str,
        name: &str,
    ) -> Result<Option<BTreeMap<String, String>>, PresetError> {
        let mut f = self.load()?;
        Ok(f.presets
            .remove(op)
            .and_then(|mut named| named.remove(name)))
    }

    /// Returns the preset names saved for an operation, sorted.
    pub fn list(&self, op: &str) -> Result<Vec<String>, PresetError> {
        let f = self.load()?;
        Ok(f.presets
            .get(op)
            .map(|named| named.keys().cloned().collect())
            .unwrap_or_default())
    }

    /// Returns every operation that has at least one preset, with its sorted
    /// preset names.
    pub fn all(&self) -> Result<BTreeMap<String, Vec<String>>, PresetError> {
        let f = self.load()?;
        Ok(f.presets
            .into_iter()
            .filter(|(_, named)| !named.is_empty())
            .map(|(op, named)| (op, named.into_keys().collect()))
            .collect())
    }

    /// Saves or replaces a preset. Empty parameter values are dropped: they
    /// carry no information and would shadow operation defaults on replay.
    pub fn save(
        &self,
        op: &str,
        name: &str,
        params: &BTreeMap<String, String>,
    ) -> Result<(), PresetError> {
        validate(op, name, params)?;
        let clean: BTreeMap<String, String> = params
            .iter()
            .filter(|(_, v)| !v.is_empty())
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let mut f = self.load()?;
        f.presets
            .entry(op.to_owned())
            .or_default()
            .insert(name.to_owned(), clean);
        self.write(&f)
    }

    /// Removes a preset; removing an operation's last preset removes the
    /// operation's section too.
    pub fn delete(&self, op: &str, name: &str) -> Result<(), PresetError> {
        let mut f = self.load()?;
        let named = f.presets.get_mut(op);
        if named.as_ref().is_none_or(|n| !n.contains_key(name)) {
            return Err(PresetError::NotFound {
                op: op.to_owned(),
                name: name.to_owned(),
            });
        }
        if let Some(named) = named {
            named.remove(name);
            if named.is_empty() {
                f.presets.remove(op);
            }
        }
        self.write(&f)
    }

    /// A missing, empty, or comment-only file holds no presets.
    fn load(&self) -> Result<File, PresetError> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(File::default()),
            Err(source) => {
                return Err(PresetError::Read {
                    path: self.path.clone(),
                    source,
                });
            }
        };
        let parsed: Option<File> =
            serde_yaml_ng::from_str(&text).map_err(|source| PresetError::Parse {
                path: self.path.clone(),
                source,
            })?;
        Ok(parsed.unwrap_or_default())
    }

    /// Writes to a temporary file and renames it into place, so an
    /// interrupted write never corrupts saved presets.
    fn write(&self, f: &File) -> Result<(), PresetError> {
        let fail = |source: io::Error| PresetError::Write {
            path: self.path.clone(),
            source,
        };
        let text = serde_yaml_ng::to_string(f)
            .map_err(|e| fail(io::Error::new(io::ErrorKind::InvalidData, e)))?;
        let parent = self
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));

        let mut dirs = DirBuilder::new();
        dirs.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut dirs, 0o700);
        dirs.create(parent).map_err(fail)?;

        let mut tmp = tempfile::Builder::new()
            .prefix(".presets-")
            .tempfile_in(parent)
            .map_err(fail)?;
        tmp.write_all(text.as_bytes()).map_err(fail)?;
        tmp.persist(&self.path).map_err(|e| fail(e.error))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = Store::new(dir.path().join("presets.yaml"));
        (dir, s)
    }

    fn params(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn vars(pairs: &[(&str, &str)]) -> Vars {
        params(pairs)
    }

    #[test]
    fn round_trip_drops_empty_values() {
        let (_d, s) = store();
        s.save(
            "s3.version-diff",
            "acme",
            &params(&[
                ("bucket", "cfg-nonprod"),
                ("key", "tenants/acme.json"),
                ("empty", ""),
            ]),
        )
        .expect("save");
        let got = s
            .get("s3.version-diff", "acme")
            .expect("get")
            .expect("preset exists");
        assert_eq!(
            got,
            params(&[("bucket", "cfg-nonprod"), ("key", "tenants/acme.json")])
        );
        assert!(!got.contains_key("empty"));
    }

    #[test]
    fn list_all_delete() {
        let (_d, s) = store();
        let p = params(&[("k", "v")]);
        s.save("s3.version-diff", "beta", &p).expect("save");
        s.save("s3.version-diff", "acme", &p).expect("save");
        s.save("ddb.item-history", "acme", &p).expect("save");

        assert_eq!(
            s.list("s3.version-diff").expect("list"),
            vec!["acme".to_owned(), "beta".to_owned()]
        );
        assert_eq!(s.all().expect("all").len(), 2);

        s.delete("s3.version-diff", "acme").expect("delete");
        assert_eq!(s.get("s3.version-diff", "acme").expect("get"), None);

        let err = s.delete("s3.version-diff", "nope").expect_err("not found");
        assert!(matches!(err, PresetError::NotFound { .. }));
        assert_eq!(err.to_string(), r#"s3.version-diff has no preset "nope""#);
        assert!(err.is_usage());

        s.delete("s3.version-diff", "beta").expect("delete");
        let all = s.all().expect("all");
        assert!(!all.contains_key("s3.version-diff"));
        assert!(all.contains_key("ddb.item-history"));
        let text = fs::read_to_string(s.path()).expect("read file");
        assert!(!text.contains("s3.version-diff:"), "file:\n{text}");
    }

    #[test]
    fn missing_file_is_empty() {
        let (_d, s) = store();
        assert_eq!(s.list("anything").expect("list"), Vec::<String>::new());
        assert_eq!(s.get("anything", "x").expect("get"), None);
        assert!(s.all().expect("all").is_empty());
    }

    #[test]
    fn empty_and_comment_only_files_are_empty() {
        let (_d, s) = store();
        fs::write(s.path(), "").expect("write");
        assert!(s.all().expect("all").is_empty());
        fs::write(s.path(), "# nothing saved yet\n").expect("write");
        assert!(s.all().expect("all").is_empty());
        fs::write(s.path(), "presets:\n").expect("write");
        assert!(s.all().expect("all").is_empty());
    }

    #[test]
    fn rejects_empty_presets() {
        let (_d, s) = store();
        let err = s
            .save("op", "name", &params(&[("a", "")]))
            .expect_err("empty");
        assert!(matches!(err, PresetError::Empty));
        assert_eq!(err.to_string(), "refusing to save an empty preset");
        assert!(err.is_usage());

        let err = s
            .save("", "name", &params(&[("a", "1")]))
            .expect_err("missing op");
        assert!(matches!(err, PresetError::MissingName));
        let err = s
            .save("op", "", &params(&[("a", "1")]))
            .expect_err("missing name");
        assert!(matches!(err, PresetError::MissingName));
        assert!(
            !s.path().exists(),
            "rejected saves must not create the file"
        );
    }

    #[test]
    fn file_is_human_editable() {
        let (_d, s) = store();
        s.save("demo.hello", "world", &params(&[("name", "world")]))
            .expect("save");
        let text = fs::read_to_string(s.path()).expect("read file");
        for want in ["presets:", "demo.hello:", "world:", "name: world"] {
            assert!(text.contains(want), "missing {want:?} in:\n{text}");
        }
    }

    #[test]
    fn hand_edited_file_is_read() {
        let (_d, s) = store();
        fs::write(
            s.path(),
            "# mine\npresets:\n  demo.hello:\n    world:\n      name: world\n",
        )
        .expect("write");
        assert_eq!(
            s.get("demo.hello", "world").expect("get"),
            Some(params(&[("name", "world")]))
        );
    }

    #[test]
    fn validate_matches_what_save_refuses() {
        let ok = params(&[("a", "1"), ("b", "")]);
        assert!(validate("op", "n", &ok).is_ok());
        assert!(matches!(
            validate("op", "", &ok),
            Err(PresetError::MissingName)
        ));
        assert!(matches!(
            validate("", "n", &ok),
            Err(PresetError::MissingName)
        ));
        let empty = params(&[("a", "")]);
        assert!(matches!(
            validate("op", "n", &empty),
            Err(PresetError::Empty)
        ));
        assert!(matches!(
            validate("op", "n", &BTreeMap::new()),
            Err(PresetError::Empty)
        ));
    }

    #[test]
    fn save_replaces_an_existing_preset() {
        let (_d, s) = store();
        s.save("op", "n", &params(&[("a", "1"), ("b", "2")]))
            .expect("save");
        s.save("op", "n", &params(&[("a", "3")])).expect("save");
        assert_eq!(s.get("op", "n").expect("get"), Some(params(&[("a", "3")])));
    }

    #[test]
    fn corrupt_file_is_a_usage_error() {
        let (_d, s) = store();
        fs::write(s.path(), "presets: [1,2\n").expect("write");
        let err = s.list("x").expect_err("parse error");
        assert!(matches!(err, PresetError::Parse { .. }));
        assert!(err.is_usage());
        assert!(err.to_string().starts_with("parse presets "));
    }

    #[test]
    fn unknown_top_level_keys_are_rejected() {
        let (_d, s) = store();
        fs::write(s.path(), "preset: {}\n").expect("write");
        assert!(matches!(
            s.all().expect_err("unknown key"),
            PresetError::Parse { .. }
        ));
    }

    #[test]
    fn unreadable_file_is_not_a_usage_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = Store::new(dir.path());
        let err = s.list("x").expect_err("a directory cannot be read");
        assert!(matches!(err, PresetError::Read { .. }));
        assert!(!err.is_usage());
        assert!(err.to_string().starts_with("read presets "));
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_write_reports_the_path() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o500)).expect("chmod");
        let s = Store::new(dir.path().join("presets.yaml"));
        let res = s.save("op", "n", &params(&[("a", "1")]));
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).expect("chmod");
        match res {
            // Privileged users can write into a read-only directory.
            Ok(()) => {}
            Err(err) => {
                assert!(matches!(err, PresetError::Write { .. }));
                assert!(!err.is_usage());
                assert!(err.to_string().starts_with("write presets "));
            }
        }
    }

    #[test]
    fn save_creates_parent_directories() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = Store::new(dir.path().join("a").join("b").join("presets.yaml"));
        s.save("op", "n", &params(&[("a", "1")])).expect("save");
        assert_eq!(s.list("op").expect("list"), vec!["n".to_owned()]);
        let leftovers: Vec<_> = fs::read_dir(s.path().parent().expect("parent"))
            .expect("read_dir")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(leftovers, vec![std::ffi::OsString::from("presets.yaml")]);
    }

    #[cfg(unix)]
    #[test]
    fn new_directories_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let sub = dir.path().join("ss");
        let s = Store::new(sub.join("presets.yaml"));
        s.save("op", "n", &params(&[("a", "1")])).expect("save");
        let mode = fs::metadata(&sub).expect("metadata").permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn default_path_honors_ss_presets() {
        assert_eq!(
            default_path(&vars(&[("SS_PRESETS", "/x/p.yaml"), ("HOME", "/h")])),
            PathBuf::from("/x/p.yaml")
        );
        assert_eq!(
            default_path(&vars(&[("SS_PRESETS", ""), ("XDG_CONFIG_HOME", "/c")])),
            PathBuf::from("/c/ss/presets.yaml")
        );
        assert_eq!(
            default_path(&vars(&[("HOME", "/h")])),
            PathBuf::from("/h/.config/ss/presets.yaml")
        );
    }
}
