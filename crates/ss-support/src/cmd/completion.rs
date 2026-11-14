//! `ss-support completion <shell>`: a completion script for the whole
//! command line, every operation and its flags included.

use ss_core::ops::Registry;

use crate::cli::{self, Shell};
use crate::io::Io;
use crate::run::Failure;

/// The name the scripts complete.
const BIN: &str = "ss-support";

/// Writes the completion script for `shell` to stdout. The script is
/// generated from the same command tree that parses the command line, so it
/// names exactly the subcommands, operations, and flags that exist. Values
/// of parameters (bucket names, table names) are not completed.
pub(crate) fn run(shell: Shell, registry: &Registry, io: &Io) -> Result<(), Failure> {
    let generator = match shell {
        Shell::Bash => clap_complete::Shell::Bash,
        Shell::Zsh => clap_complete::Shell::Zsh,
        Shell::Fish => clap_complete::Shell::Fish,
    };
    let mut script = Vec::new();
    clap_complete::generate(generator, &mut cli::command(registry), BIN, &mut script);
    let script = String::from_utf8_lossy(&script);
    let script = match shell {
        Shell::Bash => same_root_everywhere(&script),
        Shell::Zsh | Shell::Fish => script.into_owned(),
    };
    super::print(io, &script)
}

/// The bash script with one spelling of the program's name throughout.
///
/// The generator names the program `ss__support` where the script works out
/// which subcommand the line is in, but `ss__subcmd__support` in the labels
/// that hold each nested subcommand's words, so nothing below the first
/// level would ever be offered. The labels are given the first spelling.
fn same_root_everywhere(script: &str) -> String {
    let looked_up = BIN.replace('-', "__");
    let labelled = BIN.replace('-', "__subcmd__");
    script.replace(&labelled, &looked_up)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_labels_take_the_spelling_the_script_looks_up() {
        let script = "ss__support,run)\n cmd=\"ss__support__subcmd__run\"\n ss__subcmd__support__subcmd__run)\n ss__support)\n";

        assert_eq!(
            same_root_everywhere(script),
            "ss__support,run)\n cmd=\"ss__support__subcmd__run\"\n ss__support__subcmd__run)\n ss__support)\n"
        );
    }
}
