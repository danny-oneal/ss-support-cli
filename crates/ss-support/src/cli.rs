//! The command-line surface: global flags, the fixed subcommands, and one
//! `run` subcommand per registered operation.

use std::ffi::OsString;

use clap::{ArgMatches, Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use ss_core::config::{self, Environment};
use ss_core::ops::{Input, Registry};
use ss_core::vars::{self, Vars};

pub mod dynamic;

/// Command-line surface. With no subcommand, an operator at a terminal gets
/// the interactive front-end; anywhere else a bare invocation prints help.
#[derive(Parser, Debug, Clone)]
#[command(
    name = "ss-support",
    version,
    about = "Support operations for non-production AWS and Kubernetes"
)]
pub struct Cli {
    /// The chosen subcommand, if any.
    #[command(subcommand)]
    pub command: Option<Command>,
    /// Flags shared by every subcommand.
    #[command(flatten)]
    pub global: GlobalFlags,
}

/// Flags accepted anywhere on the command line, before or after the
/// subcommand.
#[derive(Args, Debug, Clone, Default)]
pub struct GlobalFlags {
    /// Override the environment's AWS profile (SS_SUPPORT_PROFILE)
    #[arg(long, global = true, value_name = "NAME")]
    pub profile: Option<String>,
    /// Override the environment's AWS region (SS_SUPPORT_REGION)
    #[arg(long, global = true, value_name = "REGION")]
    pub region: Option<String>,
    /// Target environment; writes require it in the config allowlist (SS_SUPPORT_ENV) [default: config default_env]
    #[arg(long, global = true, value_name = "NAME")]
    pub env: Option<String>,
    /// Override the environment's Kubernetes context
    #[arg(long, global = true, value_name = "KUBE_CONTEXT")]
    pub context: Option<String>,
    /// Output format: text or json [default: text]
    #[arg(short, long, global = true, value_name = "FORMAT")]
    pub output: Option<String>,
    /// Write a markdown report to FILE
    #[arg(long, global = true, value_name = "FILE")]
    pub export: Option<String>,
    /// Stop before the first mutating step
    #[arg(long, global = true)]
    pub dry_run: bool,
    /// Auto-confirm Mutate operations; never Destructive
    #[arg(long, global = true)]
    pub yes: bool,
    /// Print step progress to stderr
    #[arg(short, long, global = true)]
    pub verbose: bool,
    /// Bypass the local object-version cache
    #[arg(long, global = true)]
    pub no_cache: bool,
}

/// How results are printed on stdout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    /// Human-readable tables and fields.
    Text,
    /// The `{"views": [...]}` document.
    Json,
}

impl GlobalFlags {
    /// Validates `--output`; absent, `text`, and `table` mean [`Output::Text`].
    pub fn output_format(&self) -> Result<Output, String> {
        match self.output.as_deref() {
            None | Some("text" | "table") => Ok(Output::Text),
            Some("json") => Ok(Output::Json),
            Some(s) => Err(format!("unknown output format {s:?} (want text or json)")),
        }
    }

    /// Resolves the target environment: flag, then `SS_*` variable, then the
    /// config file, with `--context` replacing the Kubernetes context.
    pub fn resolve_env(&self, cfg: &config::File, vars: &Vars) -> Environment {
        let name = config::resolve(
            self.env.as_deref(),
            vars::get(vars, "SS_SUPPORT_ENV"),
            &cfg.default_env,
            "",
        );
        let profile = config::resolve(
            self.profile.as_deref(),
            vars::get(vars, "SS_SUPPORT_PROFILE"),
            "",
            "",
        );
        let region = config::resolve(
            self.region.as_deref(),
            vars::get(vars, "SS_SUPPORT_REGION"),
            "",
            "",
        );
        let mut env = cfg.env(&name, &profile, &region);
        if let Some(context) = self.context.as_deref().filter(|c| !c.is_empty()) {
            context.clone_into(&mut env.kube_context);
        }
        env
    }
}

/// The fixed subcommands.
#[derive(Subcommand, Debug, Clone)]
pub enum Command {
    /// List and describe registered operations
    #[command(subcommand)]
    Ops(OpsCmd),
    /// Run an operation headless
    Run(RunCmd),
    /// List, inspect, and delete saved parameter presets
    #[command(subcommand)]
    Presets(PresetsCmd),
}

/// `ss-support ops ...`
#[derive(Subcommand, Debug, Clone)]
pub enum OpsCmd {
    /// List every registered operation
    List,
    /// Show an operation's params, defaults, and help
    Describe {
        /// Operation to describe
        #[arg(value_name = "OPERATION")]
        id: String,
    },
}

/// `ss-support presets ...`
#[derive(Subcommand, Debug, Clone)]
pub enum PresetsCmd {
    /// List saved presets, optionally for one operation
    List {
        /// Only presets saved for this operation
        #[arg(value_name = "OPERATION")]
        op: Option<String>,
    },
    /// Show a preset's parameters
    Show {
        /// Operation the preset belongs to
        #[arg(value_name = "OPERATION")]
        op: String,
        /// Preset name
        #[arg(value_name = "NAME")]
        name: String,
    },
    /// Delete a preset
    Delete {
        /// Operation the preset belongs to
        #[arg(value_name = "OPERATION")]
        op: String,
        /// Preset name
        #[arg(value_name = "NAME")]
        name: String,
    },
}

/// `ss-support run <operation> ...`
#[derive(Args, Debug, Clone)]
pub struct RunCmd {
    /// Load a saved parameter preset; explicit flags override it
    #[arg(long, global = true, value_name = "NAME")]
    pub preset: Option<String>,
    /// After a successful run, save the given params under NAME
    #[arg(long, global = true, value_name = "NAME")]
    pub save_preset: Option<String>,
    /// The operation chosen on the command line, filled in by
    /// [`parse_from`].
    #[arg(skip)]
    pub call: Option<OpCall>,
}

/// One operation invocation: its id and the params given explicitly.
#[derive(Debug, Clone)]
pub struct OpCall {
    /// Operation id.
    pub id: String,
    /// Params the user set on the command line; defaults and presets are
    /// applied later.
    pub input: Input,
}

impl Cli {
    /// The prefix of every one-line error: the command that failed.
    pub fn prefix(&self) -> String {
        match &self.command {
            None => "ss-support".to_owned(),
            Some(Command::Ops(OpsCmd::List)) => "ss-support ops list".to_owned(),
            Some(Command::Ops(OpsCmd::Describe { .. })) => "ss-support ops describe".to_owned(),
            Some(Command::Run(RunCmd {
                call: Some(call), ..
            })) => format!("ss-support run {}", call.id),
            Some(Command::Run(_)) => "ss-support run".to_owned(),
            Some(Command::Presets(PresetsCmd::List { .. })) => "ss-support presets list".to_owned(),
            Some(Command::Presets(PresetsCmd::Show { .. })) => "ss-support presets show".to_owned(),
            Some(Command::Presets(PresetsCmd::Delete { .. })) => {
                "ss-support presets delete".to_owned()
            }
        }
    }
}

/// The full clap tree: [`Cli`] with one `run` subcommand per registered
/// operation.
pub fn command(registry: &Registry) -> clap::Command {
    Cli::command().mut_subcommand("run", |run| {
        let mut run = run.subcommand_required(true);
        for op in registry.all() {
            run = run.subcommand(dynamic::op_command(op.as_ref()));
        }
        run
    })
}

/// Parses `args` against the tree for `registry`, recording the chosen
/// operation and its explicit params on `run`.
pub fn parse_from<I, T>(registry: &Registry, args: I) -> Result<Cli, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let matches = command(registry).try_get_matches_from(args)?;
    let mut cli = Cli::from_arg_matches(&matches)?;
    if let Some(Command::Run(run)) = &mut cli.command {
        run.call = matches
            .subcommand_matches("run")
            .and_then(ArgMatches::subcommand)
            .map(|(id, op_matches)| {
                let op = registry.get(id).expect("matched subcommand is registered");
                OpCall {
                    id: id.to_owned(),
                    input: dynamic::collect_input(op.as_ref(), op_matches),
                }
            });
    }
    Ok(cli)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use ss_core::config::EnvSpec;
    use ss_core::ops::builtin;
    use ss_core::ops::demo::Hello;

    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        parse_from(&builtin(), args.iter().copied())
    }

    fn call(cli: &Cli) -> &OpCall {
        match &cli.command {
            Some(Command::Run(RunCmd {
                call: Some(call), ..
            })) => call,
            other => panic!("not a run command: {other:?}"),
        }
    }

    #[test]
    fn command_tree_is_valid() {
        command(&builtin()).debug_assert();
    }

    #[test]
    fn run_collects_only_explicit_flags() {
        let cli = parse(&["ss-support", "run", "demo.hello", "--name", "x"]).expect("parses");
        let call = call(&cli);
        assert_eq!(call.id, "demo.hello");
        assert_eq!(call.input, [("name", "x")].into_iter().collect::<Input>());
    }

    #[test]
    fn bool_params_accept_bare_and_explicit() {
        let cli = parse(&["ss-support", "run", "demo.hello", "--shout"]).expect("parses");
        assert_eq!(call(&cli).input.str("shout"), "true");
        let cli = parse(&["ss-support", "run", "demo.hello", "--shout=false"]).expect("parses");
        assert_eq!(call(&cli).input.str("shout"), "false");
        assert!(call(&cli).input.contains("shout"));
    }

    #[test]
    fn op_params_take_hyphen_leading_values() {
        let cli = parse(&[
            "ss-support",
            "run",
            "demo.hello",
            "--repeat",
            "-1",
            "--name",
            "-x",
        ])
        .expect("parses");
        assert_eq!(call(&cli).input.str("repeat"), "-1");
        assert_eq!(call(&cli).input.str("name"), "-x");
    }

    #[test]
    fn repeated_op_params_take_the_last_value() {
        let cli = parse(&[
            "ss-support",
            "run",
            "demo.hello",
            "--name",
            "a",
            "--name",
            "b",
        ])
        .expect("parses");
        assert_eq!(call(&cli).input.str("name"), "b");
        let cli = parse(&[
            "ss-support",
            "run",
            "demo.hello",
            "--shout",
            "--shout=false",
        ])
        .expect("parses");
        assert_eq!(call(&cli).input.str("shout"), "false");
    }

    #[test]
    fn globals_parse_after_the_op_id() {
        let cli = parse(&[
            "ss-support",
            "run",
            "demo.hello",
            "--name",
            "x",
            "-o",
            "json",
            "--env",
            "dev",
            "--preset",
            "p",
            "--yes",
        ])
        .expect("parses");
        assert_eq!(cli.global.output.as_deref(), Some("json"));
        assert_eq!(cli.global.env.as_deref(), Some("dev"));
        assert!(cli.global.yes);
        let Some(Command::Run(run)) = &cli.command else {
            panic!("not a run command");
        };
        assert_eq!(run.preset.as_deref(), Some("p"));
        assert_eq!(call(&cli).input.str("name"), "x");
    }

    #[test]
    fn unknown_op_and_missing_op_are_usage_errors() {
        for args in [
            &["ss-support", "run", "nope"][..],
            &["ss-support", "run"][..],
        ] {
            let err = parse(args).expect_err("usage error");
            assert_eq!(err.exit_code(), 2, "{args:?}");
        }
    }

    #[test]
    fn param_help_shows_default_and_required() {
        let cmd = dynamic::op_command(&Hello);
        let arg = |name: &str| {
            cmd.get_arguments()
                .find(|a| a.get_id() == name)
                .unwrap_or_else(|| panic!("no arg {name}"))
                .clone()
        };
        assert_eq!(
            arg("name").get_help().map(ToString::to_string).as_deref(),
            Some("Who to greet [required]")
        );
        let repeat = arg("repeat");
        assert_eq!(
            repeat.get_help().map(ToString::to_string).as_deref(),
            Some("Rows to generate [default: 3]")
        );
        let value_names: Vec<_> = repeat
            .get_value_names()
            .expect("value name")
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(value_names, ["INT"]);
    }

    #[test]
    fn output_format_validation() {
        let with = |o: Option<&str>| GlobalFlags {
            output: o.map(String::from),
            ..GlobalFlags::default()
        };
        assert_eq!(with(None).output_format(), Ok(Output::Text));
        assert_eq!(with(Some("text")).output_format(), Ok(Output::Text));
        assert_eq!(with(Some("table")).output_format(), Ok(Output::Text));
        assert_eq!(with(Some("json")).output_format(), Ok(Output::Json));
        assert_eq!(
            with(Some("xml")).output_format(),
            Err(r#"unknown output format "xml" (want text or json)"#.to_owned())
        );
    }

    #[test]
    fn resolve_env_precedence() {
        let spec = |profile: &str, kube_context: &str| EnvSpec {
            profile: profile.into(),
            kube_context: kube_context.into(),
            ..EnvSpec::default()
        };
        let cfg = config::File {
            default_env: "dev".into(),
            environments: BTreeMap::from([
                ("dev".to_owned(), spec("p-dev", "k-dev")),
                ("stage".to_owned(), EnvSpec::default()),
            ]),
            ..config::File::default()
        };
        let none = Vars::new();
        let env = GlobalFlags::default().resolve_env(&cfg, &none);
        assert_eq!(env.name, "dev");
        assert!(env.allowed);
        assert_eq!(env.profile, "p-dev");

        let stage = Vars::from([("SS_SUPPORT_ENV".to_owned(), "stage".to_owned())]);
        assert_eq!(
            GlobalFlags::default().resolve_env(&cfg, &stage).name,
            "stage"
        );

        let prod = GlobalFlags {
            env: Some("prod".into()),
            ..GlobalFlags::default()
        };
        let env = prod.resolve_env(&cfg, &stage);
        assert_eq!(env.name, "prod");
        assert!(!env.allowed);

        let profile = GlobalFlags {
            profile: Some("x".into()),
            ..GlobalFlags::default()
        };
        assert_eq!(profile.resolve_env(&cfg, &none).profile, "x");

        let context = GlobalFlags {
            context: Some("k2".into()),
            ..GlobalFlags::default()
        };
        assert_eq!(context.resolve_env(&cfg, &none).kube_context, "k2");

        let from_var = Vars::from([("SS_SUPPORT_PROFILE".to_owned(), "y".to_owned())]);
        assert_eq!(
            GlobalFlags::default().resolve_env(&cfg, &from_var).profile,
            "y"
        );
    }

    #[test]
    fn prefix_names_the_subcommand() {
        let prefix = |args: &[&str]| parse(args).expect("parses").prefix();
        assert_eq!(
            prefix(&["ss-support", "run", "demo.hello"]),
            "ss-support run demo.hello"
        );
        assert_eq!(
            prefix(&["ss-support", "ops", "describe", "x"]),
            "ss-support ops describe"
        );
        assert_eq!(
            prefix(&["ss-support", "ops", "list"]),
            "ss-support ops list"
        );
        assert_eq!(
            prefix(&["ss-support", "presets", "show", "a", "b"]),
            "ss-support presets show"
        );
        assert_eq!(prefix(&["ss-support"]), "ss-support");
    }
}
