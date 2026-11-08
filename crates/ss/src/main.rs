//! Binary shim: parse the CLI, run it, exit with its code. Logic lives in the lib.

use ss::Exit;

#[tokio::main]
async fn main() {
    let cli = match ss::parse_from(&ss_core::ops::builtin(), std::env::args_os()) {
        Ok(cli) => cli,
        Err(e) => e.exit(),
    };
    let code: Exit = ss::run(cli).await;
    std::process::exit(code.code());
}
