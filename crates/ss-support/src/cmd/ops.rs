//! `ss-support ops list` and `ss-support ops describe`.

use anyhow::anyhow;
use ss_core::ops::{Registry, View};
use ss_core::render;

use super::{print, table};
use crate::cli::OpsCmd;
use crate::cli::dynamic::ParamDoc;
use crate::io::Io;
use crate::run::Failure;

pub(super) fn run(cmd: &OpsCmd, registry: &Registry, io: &Io) -> Result<(), Failure> {
    match cmd {
        OpsCmd::List => list(registry, io),
        OpsCmd::Describe { id } => describe(registry, id, io),
    }
}

fn list(registry: &Registry, io: &Io) -> Result<(), Failure> {
    let rows = registry
        .all()
        .iter()
        .map(|op| {
            let meta = op.meta();
            [meta.id, meta.category, meta.class.as_str(), meta.summary]
                .map(str::to_owned)
                .to_vec()
        })
        .collect();
    let view = table("", &["ID", "CATEGORY", "CLASS", "SUMMARY"], rows);
    print(io, &render::text(&[view]))
}

fn describe(registry: &Registry, id: &str, io: &Io) -> Result<(), Failure> {
    let op = registry.get(id).ok_or_else(|| {
        Failure::Usage(anyhow!(
            "unknown operation {id:?} (try `ss-support ops list`)"
        ))
    })?;
    let meta = op.meta();
    let rows = op
        .params()
        .iter()
        .map(|param| ParamDoc::of(param).row())
        .collect();
    let views = [
        View::summary([
            ("ID", meta.id),
            ("Name", meta.name),
            ("Category", meta.category),
            ("Class", meta.class.as_str()),
            ("Summary", meta.summary),
        ]),
        table(
            "Params",
            &["FLAG", "TYPE", "REQUIRED", "DEFAULT", "HELP"],
            rows,
        ),
    ];
    print(io, &render::text(&views))
}
