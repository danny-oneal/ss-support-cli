//! Per-operation subcommands generated from each operation's declared
//! params.

use clap::builder::BoolishValueParser;
use clap::parser::ValueSource;
use clap::{Arg, ArgMatches, Command};
use ss_core::ops::{Input, Operation, Param, ParamType};

/// The `run` subcommand for `op`: one `--<param>` flag per declared param.
/// A value may start with a hyphen (`--repeat -1`), and a flag given more
/// than once takes its last value.
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
        ))
        .args_override_self(true);
    for param in op.params() {
        cmd = cmd.arg(param_arg(&param));
    }
    cmd
}

fn param_arg(param: &Param) -> Arg {
    let arg = Arg::new(param.name)
        .long(param.name)
        .help(ParamDoc::of(param).help_line())
        .value_name(param.ty.value_name());
    if param.ty == ParamType::Bool {
        arg.num_args(0..=1)
            .require_equals(true)
            .default_missing_value("true")
            .value_parser(BoolishValueParser::new())
    } else {
        arg.allow_hyphen_values(true)
    }
}

/// How a param is documented. Both the flag's `--help` text and the
/// `ss-support ops describe` row are built from this, so they always agree.
pub struct ParamDoc {
    name: &'static str,
    ty: ParamType,
    required: bool,
    default: &'static str,
    help: &'static str,
}

impl ParamDoc {
    /// The documented facts of `param`; an absent default or help is empty.
    pub fn of(param: &Param) -> Self {
        Self {
            name: param.name,
            ty: param.ty,
            required: param.required,
            default: param.default.unwrap_or_default(),
            help: param.help.unwrap_or_default(),
        }
    }

    /// The flag's `--help` text: the help, then `[default: …]` and
    /// `[required]` when they apply, space-separated.
    pub fn help_line(&self) -> String {
        let mut parts = Vec::new();
        if !self.help.is_empty() {
            parts.push(self.help.to_owned());
        }
        if !self.default.is_empty() {
            parts.push(format!("[default: {}]", self.default));
        }
        if self.required {
            parts.push("[required]".to_owned());
        }
        parts.join(" ")
    }

    /// The `ss-support ops describe` row: flag, type, `yes` when required, default,
    /// help.
    pub fn row(&self) -> Vec<String> {
        vec![
            format!("--{}", self.name),
            self.ty.to_string(),
            if self.required { "yes" } else { "" }.to_owned(),
            self.default.to_owned(),
            self.help.to_owned(),
        ]
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

#[cfg(test)]
mod tests {
    use ss_core::ops::demo::Hello;

    use super::*;

    fn arg(name: &str) -> Arg {
        op_command(&Hello)
            .get_arguments()
            .find(|a| a.get_id() == name)
            .unwrap_or_else(|| panic!("no arg {name}"))
            .clone()
    }

    #[test]
    fn bool_params_show_their_value_name() {
        let names: Vec<_> = arg("shout")
            .get_value_names()
            .expect("value name")
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(names, ["BOOL"]);
    }

    #[test]
    fn help_line_joins_only_the_parts_present() {
        let line = |p: Param| ParamDoc::of(&p).help_line();
        assert_eq!(line(Param::new("x")), "");
        assert_eq!(line(Param::new("x").default("1")), "[default: 1]");
        assert_eq!(line(Param::new("x").required(true)), "[required]");
        assert_eq!(
            line(Param::new("x").help("H").default("1").required(true)),
            "H [default: 1] [required]"
        );
    }

    #[test]
    fn describe_row_and_help_come_from_the_same_param() {
        let params = Hello.params();
        let row = |name: &str| {
            let p = params.iter().find(|p| p.name == name).expect("param");
            ParamDoc::of(p).row()
        };
        assert_eq!(row("name"), ["--name", "string", "yes", "", "Who to greet"]);
        assert_eq!(
            row("repeat"),
            ["--repeat", "int", "", "3", "Rows to generate"]
        );
        assert_eq!(
            arg("repeat").get_help().map(ToString::to_string).as_deref(),
            Some("Rows to generate [default: 3]")
        );
    }
}
