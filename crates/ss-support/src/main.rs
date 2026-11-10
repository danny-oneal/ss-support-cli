//! Binary shim: parse the CLI, run it, exit with its code. Logic lives in the lib.

use ss_support::Services;

#[tokio::main]
async fn main() {
    let services = Services::from_process();
    let cli = match ss_support::parse_from(&services.registry, std::env::args_os()) {
        Ok(cli) => cli,
        Err(e) => e.exit(),
    };
    std::process::exit(ss_support::run(cli, services).await.code());
}
