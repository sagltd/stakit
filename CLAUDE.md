# stakit

Rust backend/API library. Cargo **workspace**, edition **2024**.

## Layout

```
Cargo.toml          # [workspace] — members = ["crates/*"], shared package/deps/lints
rustfmt.toml        # format config (edition 2024, width 100)
code-check.sh       # quality gate: fmt + clippy + build + test
crates/             # workspace members go here (empty — add crates as needed)
```

No crates yet. Create the first with `cargo new --lib crates/<name>` (then set
`edition.workspace = true` + `lints.workspace = true` in its `Cargo.toml`).
Add new crates under `crates/*`; the glob picks them up automatically. Shared
metadata (edition, version, license, lints) is inherited from the root via
`*.workspace = true` / `lints.workspace = true`. External deps live in
`[workspace.dependencies]` and are pulled into crates with `dep.workspace = true`.

## Testing — nextest

Tests run with [`cargo-nextest`](https://nexte.st), not the built-in runner.

```bash
cargo nextest run --workspace          # run all tests
cargo nextest run --workspace -E 'test(registry)'   # filter by name
cargo test --workspace --doc           # doctests (nextest does NOT run these)
```

Install once: `cargo install cargo-nextest --locked`.

Unit tests live in `#[cfg(test)] mod tests` alongside the code. Integration
tests go in a crate's `tests/` dir.

## Quality gate

Run before committing:

```bash
./code-check.sh
```

It runs `cargo fmt --check`, `cargo clippy -D warnings`, `cargo build`,
`cargo nextest run`, and doctests. Clippy is configured workspace-wide
(`pedantic` + `nursery`, `unsafe_code = "forbid"`) in the root `Cargo.toml`.

## Conventions

- `unsafe` is forbidden workspace-wide.
- Public items require docs (`missing_docs = "warn"`).
- Keep modules focused; prefer adding a new crate over a sprawling one.
