//! The closed set of result shapes renderers visit.

use crate::diff::Delta;

/// One renderable result. Operations compose these shapes so adding an
/// operation needs no renderer changes.
#[derive(Debug, Clone, PartialEq)]
pub enum View {
    /// Ordered key/value pairs.
    Summary {
        /// The fields, in display order.
        fields: Vec<(String, String)>,
    },
    /// A semantic comparison of two labelled sides.
    Diff {
        /// Label for the left side, such as a version label.
        left: String,
        /// Label for the right side.
        right: String,
        /// The computed changes.
        delta: Delta,
    },
    /// A titled table.
    Table {
        /// Table title.
        title: String,
        /// Column headings.
        cols: Vec<String>,
        /// Row cells, one inner vector per row.
        rows: Vec<Vec<String>>,
    },
    /// A titled block of text.
    Doc {
        /// Document title.
        title: String,
        /// Document text.
        body: String,
        /// Fence language for report export, such as `json` or `diff`.
        lang: String,
    },
    /// A titled tree.
    Tree {
        /// Tree title.
        title: String,
        /// The root node.
        root: Node,
    },
}

/// One node of a [`View::Tree`].
#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    /// Text shown for this node.
    pub label: String,
    /// Child nodes, in display order.
    pub children: Vec<Node>,
}

/// What a successful operation produces: the views to render, in order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OpResult {
    /// The views, in display order.
    pub views: Vec<View>,
}

impl View {
    /// Builds a [`View::Summary`] from key/value pairs, preserving order.
    pub fn summary<K: Into<String>, V: Into<String>>(
        fields: impl IntoIterator<Item = (K, V)>,
    ) -> View {
        View::Summary {
            fields: fields
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_builds_ordered_fields() {
        assert_eq!(
            View::summary([("A", "1"), ("B", "2")]),
            View::Summary {
                fields: vec![("A".into(), "1".into()), ("B".into(), "2".into())]
            }
        );
    }
}
