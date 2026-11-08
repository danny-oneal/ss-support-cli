//! Binary shim: parse the CLI, run it, exit with its code. Logic lives in the lib.

use clap::Parser;
use ss::{Cli, Exit};

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let code: Exit = ss::run(cli).await;
    std::process::exit(code.code());
}
