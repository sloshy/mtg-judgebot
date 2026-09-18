<!-- One change per pull request. Say what it does and why. Link the issue if there is one. -->

## Checklist

- [ ] `cargo fmt --all --check`, `SQLX_OFFLINE=true cargo clippy --workspace --all-targets` and `cargo test --workspace` pass locally
- [ ] SQL changed → `cargo sqlx prepare --workspace -- --all-targets` re-run and `.sqlx` committed
- [ ] Prompt, schema or `data/categories.yaml` changed → golden fixtures / prompt digest updated on purpose and reviewed
- [ ] Anything an operator or user would notice has a line under *Unreleased* in `CHANGELOG.md`
- [ ] No `.env`, `judge.toml`, dumps or `eval/runs/` included
