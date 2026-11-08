//! A snapshot of environment variables.
//!
//! Library code reads settings from a [`Vars`] snapshot instead of the process
//! environment, so tests can supply exactly the variables they care about.

use std::collections::BTreeMap;
use std::path::PathBuf;

/// Environment variables captured at one point in time.
pub type Vars = BTreeMap<String, String>;

/// Returns the value of `key`, treating an empty value as unset.
pub fn get<'a>(v: &'a Vars, key: &str) -> Option<&'a str> {
    v.get(key).map(String::as_str).filter(|s| !s.is_empty())
}

/// Captures the process environment. Entries whose name or value is not valid
/// UTF-8 are skipped.
pub fn process() -> Vars {
    std::env::vars_os()
        .filter_map(|(k, val)| Some((k.into_string().ok()?, val.into_string().ok()?)))
        .collect()
}

/// Resolves a base directory: `$xdg_key` when set, else `$HOME/<home_rel>`,
/// else `./<home_rel>`.
pub fn base_dir(v: &Vars, xdg_key: &str, home_rel: &str) -> PathBuf {
    if let Some(dir) = get(v, xdg_key) {
        return PathBuf::from(dir);
    }
    let home = get(v, "HOME").unwrap_or(".");
    PathBuf::from(home).join(home_rel)
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

    #[test]
    fn get_treats_empty_as_absent() {
        let v = vars(&[("A", "x"), ("B", "")]);
        assert_eq!(get(&v, "A"), Some("x"));
        assert_eq!(get(&v, "B"), None);
        assert_eq!(get(&v, "C"), None);
    }

    #[test]
    fn base_dir_prefers_the_xdg_variable() {
        let v = vars(&[("XDG_STATE_HOME", "/s"), ("HOME", "/h")]);
        assert_eq!(
            base_dir(&v, "XDG_STATE_HOME", ".local/state"),
            PathBuf::from("/s")
        );
    }

    #[test]
    fn base_dir_falls_back_to_home() {
        let v = vars(&[("HOME", "/h"), ("XDG_STATE_HOME", "")]);
        assert_eq!(
            base_dir(&v, "XDG_STATE_HOME", ".local/state"),
            PathBuf::from("/h/.local/state")
        );
    }

    #[test]
    fn base_dir_falls_back_to_the_working_directory() {
        assert_eq!(
            base_dir(&Vars::new(), "XDG_STATE_HOME", ".local/state"),
            PathBuf::from("./.local/state")
        );
    }

    #[test]
    fn process_snapshot_contains_set_variables() {
        // PATH is set in every environment the tests run in.
        assert!(get(&process(), "PATH").is_some());
    }
}
