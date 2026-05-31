#!/usr/bin/env bash
# Workspace quality gate: format, lint, build, test.
# Run before every commit / in CI. Any failure aborts (exit non-zero).
set -euo pipefail

cd "$(dirname "$0")"

# No member crates yet -> nothing to check.
if ! ls crates/*/Cargo.toml >/dev/null 2>&1; then
  echo "No crates in crates/* yet — add one to run the full gate. Skipping."
  exit 0
fi

echo "==> rustfmt (check)"
cargo fmt --all -- --check

echo "==> clippy (deny warnings)"
cargo clippy --workspace --all-targets --all-features -- -D warnings

echo "==> build"
cargo build --workspace --all-targets --all-features

echo "==> test (nextest)"
cargo nextest run --workspace --all-features

echo "==> doctests (nextest does not run these)"
cargo test --workspace --doc --all-features

echo "All checks passed."
