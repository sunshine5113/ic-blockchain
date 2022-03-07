#!/usr/bin/env bash
set -xeuo pipefail

cd "$CI_PROJECT_DIR/rs"
cargo fmt -- --check
cargo clippy --locked --all-features --tests --benches -- \
    -D warnings \
    -D clippy::all \
    -D clippy::mem_forget \
    -A clippy::redundant_closure \
    -A clippy::too_many_arguments \
    -C debug-assertions=off

if cargo tree -e features | grep -q 'serde feature "rc"'; then
    echo 'The serde "rc" feature seems to be enabled. Instead, the module "serde_arc" in "ic-utils" should be used.'
    exit 1
fi

cargo run -q -p depcheck

cd "$CI_PROJECT_DIR/rs/replica"
cargo check --features malicious_code --locked
