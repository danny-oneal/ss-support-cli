//! The configuration reference in `docs/configuration.md`: every example in
//! it is a whole file the program accepts, and every key and variable the
//! program reads is named in it.

use std::fs;
use std::path::{Path, PathBuf};

use ss_core::config::{self, DiffDefaults, EnvSpec, TraceConfig, TraceDomain};
use ss_core::preset;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn reference() -> String {
    fs::read_to_string(repo().join("docs/configuration.md")).expect("read docs/configuration.md")
}

/// The fenced blocks of `doc` opened with exactly ```` ```{tag} ````.
fn blocks(doc: &str, tag: &str) -> Vec<String> {
    let opening = format!("```{tag}");
    let mut found = Vec::new();
    let mut current: Option<String> = None;
    for line in doc.lines() {
        match &mut current {
            None if line == opening => current = Some(String::new()),
            None => {}
            Some(_) if line == "```" => found.extend(current.take()),
            Some(block) => {
                block.push_str(line);
                block.push('\n');
            }
        }
    }
    found
}

#[test]
fn every_configuration_example_is_a_file_the_program_accepts() {
    let doc = reference();
    let dir = tempfile::tempdir().expect("tempdir");

    let configs = blocks(&doc, "yaml config");
    assert!(configs.len() >= 2, "{} config examples", configs.len());
    for (i, block) in configs.iter().enumerate() {
        let path = dir.path().join(format!("config-{i}.yaml"));
        fs::write(&path, block).expect("write example");
        if let Err(e) = config::load(&path) {
            panic!("config example {i} is refused: {e:#?}\n{block}");
        }
    }

    let presets = blocks(&doc, "yaml presets");
    assert!(!presets.is_empty(), "no presets example");
    for (i, block) in presets.iter().enumerate() {
        let path = dir.path().join(format!("presets-{i}.yaml"));
        fs::write(&path, block).expect("write example");
        let saved = preset::Store::new(&path)
            .all()
            .unwrap_or_else(|e| panic!("presets example {i} is refused: {e:#?}\n{block}"));
        assert!(!saved.is_empty(), "presets example {i} saves nothing");
    }
}

#[test]
fn the_complete_example_sets_every_key() {
    let doc = reference();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.yaml");
    let complete = blocks(&doc, "yaml config")
        .into_iter()
        .max_by_key(String::len)
        .expect("a config example");
    fs::write(&path, &complete).expect("write example");

    // Destructured without `..`, so that a key added to the program stops
    // this from compiling until the reference has it.
    let config::File {
        default_env,
        environments,
        diff: DiffDefaults {
            ignore,
            array_match,
        },
        trace: TraceConfig { domains },
    } = config::load(&path).expect("the complete example parses");

    assert!(!default_env.is_empty(), "default_env");
    assert!(!ignore.is_empty() && !array_match.is_empty(), "diff");
    let set = |value: &str| !value.is_empty();
    assert!(
        environments.values().any(|env| {
            let EnvSpec {
                profile,
                region,
                account_id,
                kube_context,
            } = env;
            [profile, region, account_id, kube_context]
                .iter()
                .all(|v| set(v))
        }),
        "no environment sets every key"
    );
    assert!(
        domains.values().any(|domain| {
            let TraceDomain {
                table,
                tables,
                bucket,
                buckets,
                hash_attr,
                sort_attr,
                sort_format,
                version_attr,
                s3_key_attr,
                s3_version_attr,
                labels,
                namespace,
            } = domain;
            [
                table,
                bucket,
                hash_attr,
                sort_attr,
                sort_format,
                version_attr,
                s3_key_attr,
                s3_version_attr,
                namespace,
            ]
            .iter()
            .all(|v| set(v))
                && !tables.is_empty()
                && !buckets.is_empty()
                && !labels.is_empty()
        }),
        "no trace domain sets every key"
    );
}

#[test]
fn the_reference_names_every_key_and_variable() {
    let doc = reference();
    let keys = [
        "default_env",
        "environments",
        "profile",
        "region",
        "account_id",
        "kube_context",
        "diff",
        "ignore",
        "array_match",
        "trace",
        "domains",
        "table",
        "tables",
        "bucket",
        "buckets",
        "hash_attr",
        "sort_attr",
        "sort_format",
        "version_attr",
        "s3_key_attr",
        "s3_version_attr",
        "labels",
        "namespace",
        "presets",
    ];
    let variables = [
        "SS_SUPPORT_CONFIG",
        "SS_SUPPORT_PRESETS",
        "SS_SUPPORT_ENV",
        "SS_SUPPORT_PROFILE",
        "SS_SUPPORT_REGION",
        "SS_SUPPORT_S3_ENDPOINT",
        "SS_SUPPORT_DDB_ENDPOINT",
        "XDG_CONFIG_HOME",
        "XDG_STATE_HOME",
        "XDG_CACHE_HOME",
    ];
    for name in keys.iter().chain(&variables) {
        assert!(
            doc.contains(&format!("`{name}`")),
            "the reference does not name `{name}`"
        );
    }
}

#[test]
fn the_example_config_file_is_one_the_program_accepts() {
    let loaded = config::load(&repo().join("docs/config.example.yaml"))
        .expect("docs/config.example.yaml parses");

    assert!(!loaded.environments.is_empty());
}
