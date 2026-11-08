//! Per-domain record locators: how to find a record by key and follow it to
//! object storage.

use std::collections::BTreeMap;

use serde::Deserialize;

/// The `trace` section of the config file.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct TraceConfig {
    /// Record locators by domain name.
    pub domains: BTreeMap<String, TraceDomain>,
}

/// Describes how to find a record and follow it to object storage.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct TraceDomain {
    /// Table name; `{env}` is replaced with the active environment name.
    pub table: String,
    /// Per-environment table names; they win over `table`.
    pub tables: BTreeMap<String, String>,
    /// Bucket name; `{env}` is replaced with the active environment name.
    pub bucket: String,
    /// Per-environment bucket names; they win over `bucket`.
    pub buckets: BTreeMap<String, String>,
    /// Partition key attribute.
    pub hash_attr: String,
    /// Sort key attribute; empty means the table has no sort key.
    pub sort_attr: String,
    /// Template building the sort key from vars, such as `{locale}#{channel}`.
    pub sort_format: String,
    /// Attribute holding the record version.
    pub version_attr: String,
    /// Attribute holding the object key.
    pub s3_key_attr: String,
    /// Attribute holding the object version ID.
    pub s3_version_attr: String,
    /// Record attributes worth echoing in summaries.
    pub labels: Vec<String>,
    /// Default Kubernetes namespace for drift checks.
    pub namespace: String,
}

/// Failure to build a sort key.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TraceError {
    /// The sort key template references variables that were not supplied.
    #[error(
        "sort key {format:?} needs vars: {} (pass --vars {}=…)",
        .missing.join(", "),
        .missing.join("=… ")
    )]
    MissingVars {
        /// The sort key template.
        format: String,
        /// The unresolved variable names, in order of appearance.
        missing: Vec<String>,
    },
}

impl TraceDomain {
    /// Fills the common-shape attribute names where unset.
    #[must_use]
    pub fn with_defaults(mut self) -> Self {
        fn fill(slot: &mut String, default: &str) {
            if slot.is_empty() {
                default.clone_into(slot);
            }
        }
        fill(&mut self.hash_attr, "hash");
        fill(&mut self.version_attr, "version");
        fill(&mut self.s3_key_attr, "key");
        fill(&mut self.s3_version_attr, "versionId");
        if self.sort_format.is_empty() && !self.sort_attr.is_empty() {
            self.sort_format = format!("{{{}}}", self.version_attr);
        }
        self
    }

    /// Returns the table for an environment: the per-environment override,
    /// else the `{env}` template.
    #[must_use]
    pub fn resolve_table(&self, env: &str) -> String {
        resolve_name(&self.tables, &self.table, env)
    }

    /// Returns the bucket for an environment, resolved like the table.
    #[must_use]
    pub fn resolve_bucket(&self, env: &str) -> String {
        resolve_name(&self.buckets, &self.bucket, env)
    }

    /// Renders `sort_format` with `vars`, naming every unresolved placeholder.
    pub fn build_sort(&self, vars: &BTreeMap<String, String>) -> Result<String, TraceError> {
        if self.sort_format.is_empty() {
            return Ok(String::new());
        }
        let mut out = String::with_capacity(self.sort_format.len());
        let mut missing = Vec::new();
        let mut rest = self.sort_format.as_str();
        while let Some(open) = rest.find('{') {
            out.push_str(&rest[..open]);
            let tail = &rest[open + 1..];
            match tail.find(['{', '}']) {
                Some(end) if end > 0 && tail.as_bytes()[end] == b'}' => {
                    let name = &tail[..end];
                    if let Some(v) = vars.get(name) {
                        out.push_str(v);
                    } else {
                        missing.push(name.to_owned());
                        out.push('{');
                        out.push_str(name);
                        out.push('}');
                    }
                    rest = &tail[end + 1..];
                }
                _ => {
                    out.push('{');
                    rest = tail;
                }
            }
        }
        out.push_str(rest);
        if missing.is_empty() {
            Ok(out)
        } else {
            Err(TraceError::MissingVars {
                format: self.sort_format.clone(),
                missing,
            })
        }
    }
}

fn resolve_name(overrides: &BTreeMap<String, String>, template: &str, env: &str) -> String {
    overrides
        .get(env)
        .cloned()
        .unwrap_or_else(|| template.replace("{env}", env))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn defaults_fill_the_common_shape() {
        let d = TraceDomain::default().with_defaults();
        assert_eq!(d.hash_attr, "hash");
        assert_eq!(d.version_attr, "version");
        assert_eq!(d.s3_key_attr, "key");
        assert_eq!(d.s3_version_attr, "versionId");
        assert_eq!(d.sort_format, "");

        let d = TraceDomain {
            sort_attr: "version".into(),
            ..TraceDomain::default()
        }
        .with_defaults();
        assert_eq!(d.sort_format, "{version}");

        let d = TraceDomain {
            sort_attr: "sk".into(),
            version_attr: "rev".into(),
            ..TraceDomain::default()
        }
        .with_defaults();
        assert_eq!(d.sort_format, "{rev}");

        let explicit = TraceDomain {
            hash_attr: "storeNumber".into(),
            sort_attr: "sk".into(),
            sort_format: "{locale}#{channel}".into(),
            version_attr: "v".into(),
            s3_key_attr: "uri".into(),
            s3_version_attr: "vid".into(),
            ..TraceDomain::default()
        };
        assert_eq!(explicit.clone().with_defaults(), explicit);
    }

    #[test]
    fn table_and_bucket_resolve_override_then_template() {
        let d = TraceDomain {
            table: "ss-menu-{env}".into(),
            tables: BTreeMap::from([("prod".to_owned(), "menu-prod-v2".to_owned())]),
            bucket: "ss-menu-{env}-assets".into(),
            buckets: BTreeMap::from([("prod".to_owned(), "menu-prod-bucket".to_owned())]),
            ..TraceDomain::default()
        };
        assert_eq!(d.resolve_table("dev"), "ss-menu-dev");
        assert_eq!(d.resolve_table("prod"), "menu-prod-v2");
        assert_eq!(d.resolve_bucket("dev"), "ss-menu-dev-assets");
        assert_eq!(d.resolve_bucket("prod"), "menu-prod-bucket");
        assert_eq!(TraceDomain::default().resolve_table("dev"), "");
    }

    #[test]
    fn build_sort_renders_and_names_missing_vars() {
        let d = |f: &str| TraceDomain {
            sort_format: f.into(),
            ..TraceDomain::default()
        };
        assert_eq!(d("").build_sort(&vars(&[])), Ok(String::new()));
        assert_eq!(
            d("{locale}#{channel}").build_sort(&vars(&[("locale", "en-US"), ("channel", "web")])),
            Ok("en-US#web".to_owned())
        );
        assert_eq!(
            d("v{version}").build_sort(&vars(&[("version", "7"), ("extra", "x")])),
            Ok("v7".to_owned())
        );

        let err = d("{locale}#{channel}")
            .build_sort(&vars(&[("locale", "en-US")]))
            .expect_err("channel is missing");
        assert_eq!(
            err.to_string(),
            r#"sort key "{locale}#{channel}" needs vars: channel (pass --vars channel=…)"#
        );

        let err = d("{a}{b}")
            .build_sort(&vars(&[]))
            .expect_err("both missing");
        assert!(
            err.to_string()
                .ends_with("needs vars: a, b (pass --vars a=… b=…)"),
            "{err}"
        );

        let err = d("{a}-{a}").build_sort(&vars(&[])).expect_err("a repeats");
        assert_eq!(
            err,
            TraceError::MissingVars {
                format: "{a}-{a}".into(),
                missing: vec!["a".into(), "a".into()],
            }
        );
    }

    #[test]
    fn build_sort_leaves_unbalanced_braces_alone() {
        let d = TraceDomain {
            sort_format: "{{a}}-{b".into(),
            ..TraceDomain::default()
        };
        // "{a}" is the only well-formed placeholder; "{{a}}" keeps its outer braces.
        assert_eq!(d.build_sort(&vars(&[("a", "1")])), Ok("{1}-{b".to_owned()));
        let d = TraceDomain {
            sort_format: "{}x".into(),
            ..TraceDomain::default()
        };
        assert_eq!(d.build_sort(&vars(&[])), Ok("{}x".to_owned()));
    }

    #[test]
    fn trace_section_parses_from_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("config.yaml");
        std::fs::write(
            &p,
            "\
trace:
  domains:
    menu:
      table: \"ss-menu-{env}\"
      hash_attr: storeNumber
      sort_attr: sk
      sort_format: \"{locale}#{channel}\"
      labels: [storeNumber]
",
        )
        .expect("write config");
        let f = crate::config::load(&p).expect("load");
        let menu = &f.trace.domains["menu"];
        assert_eq!(menu.table, "ss-menu-{env}");
        assert_eq!(menu.hash_attr, "storeNumber");
        assert_eq!(menu.sort_attr, "sk");
        assert_eq!(menu.sort_format, "{locale}#{channel}");
        assert_eq!(menu.labels, vec!["storeNumber".to_owned()]);
        assert_eq!(menu.s3_key_attr, "");
    }

    #[test]
    fn unknown_domain_key_is_rejected() {
        let err = serde_yaml_ng::from_str::<TraceConfig>("domains: {menu: {tabel: x}}")
            .expect_err("typo");
        assert!(err.to_string().contains("unknown field `tabel`"), "{err}");
    }
}
