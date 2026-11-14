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
    // change. A path that does not exist would make every build run this
    // again, so only existing ones are watched.
    for flag in ["--git-dir", "--git-common-dir"] {
        let Some(dir) = git(&here, &["rev-parse", flag]) else {
            continue;
        };
        let dir = here.join(dir);
        for name in ["HEAD", "packed-refs", "refs/tags"] {
            let path = dir.join(name);
            if path.exists() {
                println!("cargo:rerun-if-changed={}", path.display());
            }
        }
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
