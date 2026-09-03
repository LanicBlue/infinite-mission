#!/usr/bin/env bash
# Full local gate: format, lints, tests. Run before every commit.
# Usage: ./check.sh [--fix]   (--fix applies cargo fmt instead of checking)
set -euo pipefail
cd "$(dirname "$0")"

CARGO="${CARGO:-$(command -v cargo || echo "$HOME/.cargo/bin/cargo")}"
RUSTUP="$(command -v rustup || echo "$HOME/.cargo/bin/rustup")"

# A fresh toolchain may lack the lint components — bootstrap them on demand.
for component in rustfmt clippy; do
    if ! "$CARGO" "$component" --version >/dev/null 2>&1; then
        echo "==> installing missing component: $component"
        "$RUSTUP" component add "$component"
    fi
done

if [ "${1:-}" = "--fix" ]; then
    echo "==> cargo fmt (apply)"
    "$CARGO" fmt
else
    echo "==> cargo fmt --check"
    "$CARGO" fmt --check
fi

echo "==> cargo clippy --all-targets -- -D warnings"
"$CARGO" clippy --all-targets -- -D warnings

echo "==> cargo test"
"$CARGO" test

echo "==> all green"
