#!/bin/bash
# Compile (release) and install `im` to ~/.local/bin.
#
# The static core makes this safe at any time: state lives in .im/im.db,
# not in the process — replacing the binary never disturbs running
# waiters/consoles (they pick up the new version on their next start).
set -euo pipefail
cd "$(dirname "$0")"

BIN_DIR="${IM_INSTALL_BIN:-$HOME/.local/bin}"
BIN="$BIN_DIR/im"

# cargo lives in rustup's dir, which non-interactive shells don't get.
export PATH="$HOME/.cargo/bin:$PATH"
command -v cargo >/dev/null || { echo "error: cargo not found (install rustup first)" >&2; exit 1; }

echo "==> cargo build --release"
cargo build --release

echo "==> installing to $BIN"
mkdir -p "$BIN_DIR"
install -m 0755 target/release/im "$BIN"

case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *)
    cat >&2 <<EOF
warning: $BIN_DIR is not in this shell's PATH. Add it to your shell config:
  echo 'export PATH="$BIN_DIR:\$PATH"' >> ~/.zshrc
EOF
    ;;
esac

echo "==> $($BIN --version)"
echo "done. Long-running waiters/consoles keep the old build until restarted."
