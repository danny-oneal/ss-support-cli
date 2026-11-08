# ss (Rust port)

Work in progress: a feature-for-feature Rust port of the Go `ss` CLI in the
parent directory. Plan and status: `../docs/superpowers/plans/2026-11-07-rust-port-master-plan.md`.

```bash
make rust-check            # fmt + clippy (-D warnings) + tests, from the repo root
cd rust && cargo run -p ss -- --version
```

Crates: `crates/ss-core` (diff engine, operations layer, ports — no AWS
dependencies) and `crates/ss` (the binary: CLI, TUI, AWS adapters).
