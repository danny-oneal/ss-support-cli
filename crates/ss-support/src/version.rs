//! The version the program reports: the package version, and, when the
//! build is not exactly that release, what git said about the checkout.

use std::sync::LazyLock;

/// What `git describe` printed for the checkout this was built from; empty
/// when it was not built from a git checkout.
const DESCRIBE: &str = env!("SS_SUPPORT_GIT_DESCRIBE");

static VERSION: LazyLock<String> = LazyLock::new(|| compose(env!("CARGO_PKG_VERSION"), DESCRIBE));

/// The version `--version` shows: the package version alone for a build at
/// the tag of that name, or without git; otherwise the package version
/// followed by git's description of the commit in parentheses, such as
/// `0.1.0 (0.1.0-3-gabc1234-dirty)`.
pub fn version() -> &'static str {
    VERSION.as_str()
}

/// `pkg` alone when `describe` is blank or names exactly that version, with
/// or without a leading `v`; otherwise `pkg (describe)`.
pub(crate) fn compose(pkg: &str, describe: &str) -> String {
    let describe = describe.trim();
    if describe.is_empty() || describe == pkg || describe.strip_prefix('v') == Some(pkg) {
        pkg.to_owned()
    } else {
        format!("{pkg} ({describe})")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_build_at_its_own_tag_shows_only_the_version() {
        assert_eq!(compose("0.1.0", "0.1.0"), "0.1.0");
        assert_eq!(compose("0.1.0", "v0.1.0"), "0.1.0");
        assert_eq!(compose("0.1.0", "0.1.0\n"), "0.1.0");
    }

    #[test]
    fn a_build_past_a_tag_shows_what_git_describes() {
        assert_eq!(
            compose("0.1.0", "0.1.0-alpha.7-3-gabc1234"),
            "0.1.0 (0.1.0-alpha.7-3-gabc1234)"
        );
        assert_eq!(compose("0.1.0", "abc1234"), "0.1.0 (abc1234)");
        assert_eq!(compose("0.1.0", "0.1.0-dirty"), "0.1.0 (0.1.0-dirty)");
    }

    #[test]
    fn a_build_without_git_shows_only_the_version() {
        assert_eq!(compose("0.1.0", ""), "0.1.0");
        assert_eq!(compose("0.1.0", "  \n"), "0.1.0");
    }

    #[test]
    fn the_version_of_this_build_starts_with_the_package_version() {
        let pkg = env!("CARGO_PKG_VERSION");
        let shown = version();

        assert!(
            shown == pkg || (shown.starts_with(&format!("{pkg} (")) && shown.ends_with(')')),
            "{shown}"
        );
        assert!(!shown.contains('\n'), "{shown:?}");
    }
}
