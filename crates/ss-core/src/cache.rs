//! Content-addressed byte cache for immutable fetched bodies.

use std::fs::{self, DirBuilder};
use std::io::{self, Write};
use std::path::PathBuf;

use sha2::{Digest, Sha256};

use crate::vars::{self, Vars};

/// A byte cache keyed by a caller-built content address, e.g.
/// `s3/<bucket>/<key>/<version>`.
pub trait Store: Send + Sync {
    /// The stored bytes for `key`, if any.
    fn get(&self, key: &str) -> Option<Vec<u8>>;
    /// Stores `val` under `key`, replacing any previous value.
    fn put(&self, key: &str, val: &[u8]) -> io::Result<()>;
}

/// Never hits and never stores.
#[derive(Clone, Copy, Debug, Default)]
pub struct Nop;

impl Store for Nop {
    fn get(&self, _key: &str) -> Option<Vec<u8>> {
        None
    }
    fn put(&self, _key: &str, _val: &[u8]) -> io::Result<()> {
        Ok(())
    }
}

/// A disk store under a root directory. Keys are hashed to file names, so
/// callers can use any readable key shape without worrying about path
/// characters.
#[derive(Clone, Debug)]
pub struct Dir {
    root: PathBuf,
}

impl Dir {
    /// A store rooted at `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// `root/<hex[..2]>/<hex[2..]>` where `hex` is the lowercase SHA-256 of
    /// the key. The two-level fan-out keeps directories small as the cache
    /// grows.
    fn path(&self, key: &str) -> PathBuf {
        let hex: String = Sha256::digest(key.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        self.root.join(&hex[..2]).join(&hex[2..])
    }
}

impl Store for Dir {
    fn get(&self, key: &str) -> Option<Vec<u8>> {
        fs::read(self.path(key)).ok()
    }

    /// Writes to a temporary file and renames it into place, so an
    /// interrupted write never leaves a torn entry.
    fn put(&self, key: &str, val: &[u8]) -> io::Result<()> {
        let path = self.path(key);
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("cache path has no parent"))?;

        let mut dirs = DirBuilder::new();
        dirs.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut dirs, 0o700);
        dirs.create(parent)?;

        let mut tmp = tempfile::Builder::new()
            .prefix(".tmp-")
            .tempfile_in(parent)?;
        tmp.write_all(val)?;
        tmp.persist(&path).map_err(|e| e.error)?;
        Ok(())
    }
}

/// The default cache location: `$XDG_CACHE_HOME/ss`, else `~/.cache/ss`.
pub fn default_dir(v: &Vars) -> PathBuf {
    vars::base_dir(v, "XDG_CACHE_HOME", ".cache").join("ss")
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::fs;

    #[test]
    fn nop_never_hits() {
        let c = Nop;
        c.put("k", b"v").unwrap();
        assert_eq!(c.get("k"), None);
    }

    #[test]
    fn dir_round_trips_and_overwrites() {
        let tmp = tempfile::tempdir().unwrap();
        let c = Dir::new(tmp.path());
        c.put("s3/b/k/v1", b"one").unwrap();
        assert_eq!(c.get("s3/b/k/v1"), Some(b"one".to_vec()));
        c.put("s3/b/k/v1", b"two").unwrap();
        assert_eq!(c.get("s3/b/k/v1"), Some(b"two".to_vec()));
    }

    #[test]
    fn dir_misses_unknown_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let c = Dir::new(tmp.path());
        assert_eq!(c.get("s3/b/k/absent"), None);
    }

    #[test]
    fn dir_fans_out_by_sha256() {
        let tmp = tempfile::tempdir().unwrap();
        let c = Dir::new(tmp.path());
        let key = "s3/b/k/v1";
        c.put(key, b"body").unwrap();

        let hex: String = Sha256::digest(key.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let dir = tmp.path().join(&hex[..2]);
        assert_eq!(fs::read(dir.join(&hex[2..])).unwrap(), b"body");

        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "temp files remain: {leftovers:?}");
    }

    #[test]
    fn default_dir_follows_xdg_then_home() {
        let v: Vars = [("XDG_CACHE_HOME", "/c"), ("HOME", "/h")]
            .iter()
            .map(|(k, val)| ((*k).to_owned(), (*val).to_owned()))
            .collect();
        assert_eq!(default_dir(&v), PathBuf::from("/c/ss"));
        let v: Vars = [("HOME", "/h")]
            .iter()
            .map(|(k, val)| ((*k).to_owned(), (*val).to_owned()))
            .collect();
        assert_eq!(default_dir(&v), PathBuf::from("/h/.cache/ss"));
    }
}
