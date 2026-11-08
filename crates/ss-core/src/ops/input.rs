//! Resolved parameter values as strings, with lenient typed accessors.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::jsonutil;

/// Resolved parameter values. Both front-ends produce strings (flag values,
/// form fields); the typed accessors parse on demand and never fail.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Input(BTreeMap<String, String>);

impl From<BTreeMap<String, String>> for Input {
    fn from(map: BTreeMap<String, String>) -> Self {
        Input(map)
    }
}

impl<K: Into<String>, V: Into<String>> FromIterator<(K, V)> for Input {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        Input(
            iter.into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        )
    }
}

impl Input {
    /// Sets a value, replacing any previous one.
    pub fn insert(&mut self, k: impl Into<String>, v: impl Into<String>) {
        self.0.insert(k.into(), v.into());
    }

    /// Reports whether a value is present, even if empty.
    pub fn contains(&self, k: &str) -> bool {
        self.0.contains_key(k)
    }

    /// Borrows the underlying map.
    pub fn as_map(&self) -> &BTreeMap<String, String> {
        &self.0
    }

    /// Takes the underlying map.
    pub fn into_map(self) -> BTreeMap<String, String> {
        self.0
    }

    /// The raw value, or `""` when missing.
    pub fn str(&self, k: &str) -> &str {
        self.0.get(k).map_or("", String::as_str)
    }

    /// The trimmed value as an integer; missing or unparsable gives 0.
    pub fn int(&self, k: &str) -> i64 {
        self.str(k).trim().parse().unwrap_or(0)
    }

    /// The trimmed value as a boolean. Accepts `1`, `t`, `T`, `TRUE`, `true`,
    /// `True` and their false counterparts; anything else is false.
    pub fn bool(&self, k: &str) -> bool {
        matches!(
            self.str(k).trim(),
            "1" | "t" | "T" | "TRUE" | "true" | "True"
        )
    }

    /// A glob list given as a JSON array of strings or a comma-separated
    /// list. Empty gives an empty list.
    pub fn globs(&self, k: &str) -> Vec<String> {
        let v = self.str(k).trim();
        if v.is_empty() {
            return Vec::new();
        }
        if v.starts_with('[')
            && let Ok(list) = serde_json::from_str::<Vec<String>>(v)
        {
            return list;
        }
        v.split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(String::from)
            .collect()
    }

    /// The value parsed as JSON with numbers normalized; empty or invalid
    /// gives `None`.
    pub fn json(&self, k: &str) -> Option<Value> {
        let v = self.str(k).trim();
        if v.is_empty() {
            return None;
        }
        jsonutil::parse(v.as_bytes()).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_accessors_are_lenient() {
        let in_: Input = [
            ("n", " 7 "),
            ("bad", "abc"),
            ("t", "TRUE"),
            ("f", "0"),
            ("one", "1"),
        ]
        .into_iter()
        .collect();
        assert_eq!(in_.int("n"), 7);
        assert_eq!(in_.int("bad"), 0);
        assert_eq!(in_.int("missing"), 0);
        assert!(in_.bool("t"));
        assert!(in_.bool("one"));
        assert!(!in_.bool("f"));
        assert!(!in_.bool("bad"));
        assert!(!in_.bool("missing"));
        assert_eq!(in_.str("missing"), "");
        assert_eq!(in_.str("n"), " 7 ");
    }

    #[test]
    fn bool_accepts_exactly_the_documented_spellings() {
        for t in ["1", "t", "T", "TRUE", "true", "True", " true "] {
            let in_: Input = [("k", t)].into_iter().collect();
            assert!(in_.bool("k"), "{t:?}");
        }
        for f in ["0", "f", "F", "FALSE", "false", "False", "tRuE", "yes", ""] {
            let in_: Input = [("k", f)].into_iter().collect();
            assert!(!in_.bool("k"), "{f:?}");
        }
    }

    #[test]
    fn int_rejects_floats() {
        let in_: Input = [("k", "1.5")].into_iter().collect();
        assert_eq!(in_.int("k"), 0);
    }

    #[test]
    fn globs_accept_json_arrays_and_csv() {
        let g = |v: &str| -> Vec<String> {
            let in_: Input = [("g", v)].into_iter().collect();
            in_.globs("g")
        };
        assert_eq!(g(r#"["**.a","**.b"]"#), ["**.a", "**.b"]);
        assert_eq!(g("**.a, **.b,,"), ["**.a", "**.b"]);
        assert_eq!(g("[not json"), ["[not json"]);
        assert!(g("").is_empty());
        assert!(Input::default().globs("missing").is_empty());
    }

    #[test]
    fn json_is_none_when_empty_or_invalid() {
        let j = |v: &str| {
            let in_: Input = [("j", v)].into_iter().collect();
            in_.json("j")
        };
        assert_eq!(j(""), None);
        assert_eq!(j("{"), None);
        assert_eq!(j(r#"{"a":1}"#), jsonutil::parse(br#"{"a":1}"#).ok());
        assert!(j(r#"{"a":1}"#).is_some());
    }

    #[test]
    fn map_accessors_round_trip() {
        let mut in_ = Input::default();
        in_.insert("k", "v");
        assert!(in_.contains("k"));
        assert!(!in_.contains("x"));
        assert_eq!(in_.as_map().get("k").map(String::as_str), Some("v"));
        let m = in_.clone().into_map();
        assert_eq!(Input::from(m), in_);
    }
}
