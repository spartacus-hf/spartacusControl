#!/usr/bin/env bash
# Everything that must pass before a commit: formatting, lints, tests.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all
