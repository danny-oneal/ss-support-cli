//! Per-operation subcommands generated from each operation's declared
//! params.

use clap::builder::BoolishValueParser;
use clap::parser::ValueSource;
use clap::{Arg, ArgMatches, Command};
use ss_core::ops::{Input, Operation, Param, ParamType};

/// The `run` subcommand for `op`: one `--<param>` flag per declared param.
///
/// Defaults and required checks are deliberately not enforced by clap; the
/// runner applies both after merging any preset, so `--preset` alone is a
/// valid invocation.
pub fn op_command(op: &dyn Operation) -> Command {
    let meta = op.meta();
    let mut cmd = Command::new(meta.id)
        .about(meta.summary)
        .long_about(format!(
            "{} ({}, class: {})\n\n{}",
            meta.name, meta.category, meta.class, meta.summary
        ));
    for param in op.params() {
        cmd = cmd.arg(param_arg(&param));
    }
    cmd
}

fn param_arg(param: &Param) -> Arg {
    let mut help = param.help.unwrap_or_default().to_owned();
    if let Some(default) = param.default {
        help.push_str(&format!(" [default: {default}]"));
    }
    if param.required {
        help.push_str(" [required]");
    }
    let arg = Arg::new(param.name).long(param.name).help(help);
    if param.ty == ParamType::Bool {
        arg.num_args(0..=1)
            .require_equals(true)
            .default_missing_value("true")
            .value_parser(BoolishValueParser::new())
    } else {
        arg.value_name(param.ty.value_name())
    }
}

/// The params set explicitly on the command line, as strings. Unset params
/// are omitted so defaults apply uniformly.
pub fn collect_input(op: &dyn Operation, m: &ArgMatches) -> Input {
    let mut input = Input::default();
    for param in op.params() {
        if m.value_source(param.name) != Some(ValueSource::CommandLine) {
            continue;
        }
        if param.ty == ParamType::Bool {
            if let Some(value) = m.get_one::<bool>(param.name) {
                input.insert(param.name, value.to_string());
            }
        } else if let Some(value) = m.get_one::<String>(param.name) {
            input.insert(param.name, value.as_str());
        }
    }
    input
}
