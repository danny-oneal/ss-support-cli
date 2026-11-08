//! Binary shim: parse the CLI, run it, exit with its code. Logic lives in the lib.

use ss::Services;

#[tokio::main]
async fn main() {
    let services = Services::from_process();
    let cli = match ss::parse_from(&services.registry, std::env::args_os()) {
        Ok(cli) => cli,
        Err(e) => e.exit(),
    };
    std::process::exit(ss::run(cli, services).await.code());
}
