//! The set of operations front-ends discover instead of hard-coding menus.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::Operation;

/// Why an operation could not be registered.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RegistryError {
    /// The operation's id is empty.
    #[error("operation id is empty")]
    EmptyId,
    /// Another operation already uses the id.
    #[error("duplicate operation id {0:?}")]
    Duplicate(String),
}

/// Operations keyed by id.
#[derive(Clone, Default)]
pub struct Registry {
    by_id: BTreeMap<&'static str, Arc<dyn Operation>>,
}

impl Registry {
    /// An empty registry.
    pub fn new() -> Registry {
        Registry::default()
    }

    /// Adds an operation, rejecting empty and duplicate ids.
    pub fn register(&mut self, op: Arc<dyn Operation>) -> Result<(), RegistryError> {
        let id = op.meta().id;
        if id.is_empty() {
            return Err(RegistryError::EmptyId);
        }
        if self.by_id.contains_key(id) {
            return Err(RegistryError::Duplicate(id.to_string()));
        }
        self.by_id.insert(id, op);
        Ok(())
    }

    /// The operation with this id, if registered.
    pub fn get(&self, id: &str) -> Option<Arc<dyn Operation>> {
        self.by_id.get(id).cloned()
    }

    /// Every operation, sorted by category and then id.
    pub fn all(&self) -> Vec<Arc<dyn Operation>> {
        let mut ops: Vec<_> = self.by_id.values().cloned().collect();
        ops.sort_by_key(|op| {
            let meta = op.meta();
            (meta.category, meta.id)
        });
        ops
    }

    /// The distinct categories, in the order [`Registry::all`] lists them.
    pub fn categories(&self) -> Vec<&'static str> {
        let mut out: Vec<&'static str> = Vec::new();
        for op in self.all() {
            let category = op.meta().category;
            if out.last() != Some(&category) {
                out.push(category);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;

    use super::*;
    use crate::guard::Class;
    use crate::ops::{Input, OpResult, Runner};
    use crate::ops::{Meta, Operation, Param};

    struct Stub {
        id: &'static str,
        category: &'static str,
    }

    #[async_trait]
    impl Operation for Stub {
        fn meta(&self) -> Meta {
            Meta {
                id: self.id,
                name: "stub",
                summary: "stub",
                category: self.category,
                class: Class::Read,
            }
        }
        fn params(&self) -> Vec<Param> {
            Vec::new()
        }
        async fn run(&self, _r: &mut Runner, _input: &Input) -> anyhow::Result<OpResult> {
            Ok(OpResult::default())
        }
    }

    fn stub(id: &'static str, category: &'static str) -> Arc<dyn Operation> {
        Arc::new(Stub { id, category })
    }

    #[test]
    fn sorts_by_category_then_id_and_looks_up() {
        let mut reg = Registry::new();
        reg.register(stub("s3.version-diff", "S3")).unwrap();
        reg.register(stub("ddb.item-compare", "DynamoDB")).unwrap();
        reg.register(stub("s3.object-inspect", "S3")).unwrap();

        let ids: Vec<_> = reg.all().iter().map(|op| op.meta().id).collect();
        assert_eq!(
            ids,
            ["ddb.item-compare", "s3.object-inspect", "s3.version-diff"]
        );
        assert!(reg.get("s3.version-diff").is_some());
        assert!(reg.get("nope").is_none());
        assert_eq!(reg.categories(), ["DynamoDB", "S3"]);
    }

    #[test]
    fn rejects_duplicate_and_empty_ids() {
        let mut reg = Registry::new();
        reg.register(stub("dup.op", "Demo")).unwrap();
        let err = reg.register(stub("dup.op", "Demo")).unwrap_err();
        assert!(matches!(&err, RegistryError::Duplicate(id) if id == "dup.op"));
        assert_eq!(err.to_string(), "duplicate operation id \"dup.op\"");

        let err = reg.register(stub("", "Demo")).unwrap_err();
        assert!(matches!(err, RegistryError::EmptyId));
        assert_eq!(err.to_string(), "operation id is empty");
    }
}
