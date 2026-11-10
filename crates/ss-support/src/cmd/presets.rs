//! `ss-support presets list`, `show`, and `delete`.

use ss_core::ops::View;
use ss_core::preset::{PresetError, Store};
use ss_core::render;

use super::{print, table};
use crate::cli::PresetsCmd;
use crate::io::Io;
use crate::run::Failure;

pub(super) fn run(cmd: &PresetsCmd, store: &Store, io: &Io) -> Result<(), Failure> {
    match cmd {
        PresetsCmd::List { op } => list(store, op.as_deref(), io),
        PresetsCmd::Show { op, name } => show(store, op, name, io),
        PresetsCmd::Delete { op, name } => {
            store.delete(op, name)?;
            print(io, &format!("deleted preset {name:?} for {op}\n"))
        }
    }
}

/// One row per preset, by operation and then name.
fn list(store: &Store, op: Option<&str>, io: &Io) -> Result<(), Failure> {
    let rows: Vec<Vec<String>> = match op {
        Some(op) => store
            .list(op)?
            .into_iter()
            .map(|name| vec![op.to_owned(), name])
            .collect(),
        None => store
            .all()?
            .into_iter()
            .flat_map(|(op, names)| names.into_iter().map(move |name| vec![op.clone(), name]))
            .collect(),
    };
    if rows.is_empty() {
        return print(
            io,
            "no presets saved yet — use `ss-support run <op> ... --save-preset <name>`\n",
        );
    }
    print(
        io,
        &render::text(&[table("", &["OPERATION", "PRESET"], rows)]),
    )
}

/// The preset's params as `--<param> : <value>`, sorted by param.
fn show(store: &Store, op: &str, name: &str, io: &Io) -> Result<(), Failure> {
    let params = store.get(op, name)?.ok_or_else(|| PresetError::NotFound {
        op: op.to_owned(),
        name: name.to_owned(),
    })?;
    let fields = params.into_iter().map(|(k, v)| (format!("--{k}"), v));
    print(io, &render::text(&[View::summary(fields)]))
}
