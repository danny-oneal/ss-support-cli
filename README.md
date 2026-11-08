# ss

Support operations CLI for non-production AWS and Kubernetes resources: a
Rust port of the Go `ss` tool in the parent directory, feature for feature.

```bash
make rust-check            # fmt + clippy (-D warnings) + tests, from the repo root
cargo run -p ss -- --version
```

Crates: `crates/ss-core` (diff engine, operations layer, ports; no AWS
dependencies) and `crates/ss` (the binary: CLI, TUI, AWS adapters).
