//! Shared test data for the DynamoDB operations: a `tenants` table of
//! versioned tenant configurations with one global secondary index.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Value, json};

use crate::config::{DiffDefaults, Environment};
use crate::ops::Deps;
use crate::svc::ddb::{FakeDdb, Item, contract};

/// The table every fixture item lives in.
pub(crate) const TABLE: &str = "tenants";

/// The tenant item `tenant` at `version`, with the given tier and rate limit.
fn tenant(tenant: &str, version: &str, tier: &str, rps: i64) -> Item {
    let Value::Object(item) = json!({
        "TenantId": tenant,
        "Version": version,
        "Tier": tier,
        "limits": {"rps": rps},
        "updatedAt": format!("2026-07-{version}"),
    }) else {
        unreachable!("a JSON object literal is an object")
    };
    item
}

/// A fake holding [`TABLE`] with `acme` at `V0` (platinum, 200 rps), `V1`
/// (gold, 100 rps) and `V2` (gold, 50 rps), and `beta` at `V0` (bronze,
/// 10 rps). The `TierIndex` entry for `acme` `V0` is stale: it still projects
/// 100 rps.
pub(crate) fn fake() -> FakeDdb {
    let ddb = FakeDdb::new();
    ddb.create_table(contract::table_info(TABLE));
    for item in [
        tenant("acme", "V0", "platinum", 200),
        tenant("acme", "V1", "gold", 100),
        tenant("acme", "V2", "gold", 50),
        tenant("beta", "V0", "bronze", 10),
    ] {
        ddb.put(TABLE, item);
    }
    ddb.set_index_entry(TABLE, "TierIndex", tenant("acme", "V0", "platinum", 100));
    ddb
}

/// Dependencies for the `dev` environment backed by `ddb`, with the diff
/// defaults the operations are tested against.
pub(crate) fn deps(ddb: FakeDdb) -> Deps {
    Deps {
        env: Environment {
            name: "dev".into(),
            ..Environment::default()
        },
        diff: DiffDefaults {
            ignore: vec!["**.updatedAt".into()],
            array_match: BTreeMap::new(),
        },
        ddb: Some(Arc::new(ddb)),
        ..Deps::default()
    }
}
