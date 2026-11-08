//! Golden tests over the fixture corpus at `internal/diff/testdata`: each
//! case directory holds `a.json`, `b.json`, and the `expected.txt` rendering
//! of their diff under the options in [`golden_opts`].

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use ss_core::diff::{self, Delta, Kind, Opts};
use ss_core::jsonutil;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../internal/diff/testdata")
}

fn golden_opts() -> Vec<(&'static str, Opts)> {
    vec![
        ("basic", Opts::default()),
        ("typechange", Opts::default()),
        (
            "ignore",
            Opts {
                ignore: vec!["**.updatedAt".into(), "$.metadata.revision".into()],
                ..Opts::default()
            },
        ),
        (
            "keyed",
            Opts {
                array_match: BTreeMap::from([("$.rules[*]".to_string(), "id".to_string())]),
                ..Opts::default()
            },
        ),
        ("lcsinsert", Opts::default()),
        (
            "tolerance",
            Opts {
                tolerance: 0.001,
                ..Opts::default()
            },
        ),
        (
            "nullabsent",
            Opts {
                treat_null_as_absent: true,
                ..Opts::default()
            },
        ),
    ]
}

/// The stable text form the goldens are written in.
fn render_delta(d: &Delta) -> String {
    let mut out = String::new();
    for c in &d.changes {
        let p = c.path.dotted();
        let (old, new) = (
            c.old.as_ref().map(jsonutil::compact).unwrap_or_default(),
            c.new.as_ref().map(jsonutil::compact).unwrap_or_default(),
        );
        match c.kind {
            Kind::Added => out.push_str(&format!("+ {p} = {new}\n")),
            Kind::Removed => out.push_str(&format!("- {p} = {old}\n")),
            Kind::Changed => out.push_str(&format!("~ {p}: {old} -> {new}\n")),
            Kind::TypeChanged => out.push_str(&format!("! {p}: {old} -> {new}\n")),
        }
    }
    out.push_str(&format!("stats: {}\n", d.stats));
    out
}

/// CRLF → LF, trailing whitespace stripped, exactly one trailing newline,
/// empty stays empty.
fn normalize(s: &str) -> String {
    let lf = s.replace("\r\n", "\n");
    let body = lf
        .split('\n')
        .map(|l| l.trim_end_matches([' ', '\t']))
        .collect::<Vec<_>>()
        .join("\n");
    let body = body.trim_end_matches('\n');
    if body.is_empty() {
        String::new()
    } else {
        format!("{body}\n")
    }
}

#[test]
fn fixtures_render_as_expected() {
    let dir = fixtures_dir();
    assert!(dir.is_dir(), "fixtures not found at {}", dir.display());
    let mut seen = 0;
    for (name, opts) in golden_opts() {
        let case = dir.join(name);
        let a = jsonutil::parse(&fs::read(case.join("a.json")).unwrap()).unwrap();
        let b = jsonutil::parse(&fs::read(case.join("b.json")).unwrap()).unwrap();
        let want = normalize(&fs::read_to_string(case.join("expected.txt")).unwrap());
        let got = normalize(&render_delta(&diff::json(&a, &b, &opts)));
        assert_eq!(got, want, "fixture {name}");
        seen += 1;
    }
    // Every fixture directory on disk must be in the table, so a new case
    // cannot be silently skipped.
    let on_disk = fs::read_dir(&dir)
        .unwrap()
        .filter(|e| e.as_ref().unwrap().path().is_dir())
        .count();
    assert_eq!(
        seen,
        on_disk,
        "fixture table out of sync with {}",
        dir.display()
    );
}
