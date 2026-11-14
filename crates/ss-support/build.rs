//! Tells the build what git says about the checkout, for the version the
//! program reports. A build outside a git checkout, or without git, gets an
//! empty description and never fails for it.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let here = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default());

    let describe = git(&here, &["describe", "--tags", "--always", "--dirty"]).unwrap_or_default();
    println!("cargo:rustc-env=SS_SUPPORT_GIT_DESCRIBE={describe}");

    // Run again when the checkout moves to another commit or the tags
    // change. `HEAD` only names the branch, so a commit leaves it as it
    // was: the branch's own file and the log of `HEAD` are what change. A
    // path that does not exist would make every build run this again, so
    // only existing ones are watched.
    let dir_of = |flag: &str| git(&here, &["rev-parse", flag]).map(|dir| here.join(dir));
    let mut watched = Vec::new();
    if let Some(dir) = dir_of("--git-dir") {
        watched.extend(["HEAD", "logs/HEAD"].map(|name| dir.join(name)));
    }
    if let Some(dir) = dir_of("--git-common-dir") {
        watched.extend(["packed-refs", "refs/tags"].map(|name| dir.join(name)));
        if let Some(branch) = git(&here, &["symbolic-ref", "-q", "HEAD"]) {
            watched.push(dir.join(branch));
        }
    }
    for path in watched.iter().filter(|path| path.exists()) {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

/// The first line git prints for `args`, run in `dir`; `None` when git is
/// missing, fails, or prints nothing.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().next()?.trim();
    (!line.is_empty()).then(|| line.to_owned())
}
