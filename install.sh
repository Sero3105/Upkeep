#!/bin/sh
# Build Upkeep and install it as a global `upkeep` command.
#   ./install.sh               build + install to ~/.local/bin (override with PREFIX=/path)
#   ./install.sh --uninstall   remove the binary (keeps ~/.config/upkeep data)
set -eu
cd "$(dirname "$0")"
PREFIX="${PREFIX:-$HOME/.local/bin}"
DATA="${UPKEEP_HOME:-$HOME/.config/upkeep}"

if [ "${1:-}" = "--uninstall" ]; then
  rm -f "$PREFIX/upkeep"
  echo "Removed $PREFIX/upkeep (data kept in $DATA)."
  exit 0
fi

# rustup from Homebrew is keg-only: use it even if it is not on PATH yet.
command -v cargo >/dev/null 2>&1 || PATH="/opt/homebrew/opt/rustup/bin:$HOME/.cargo/bin:$PATH"
command -v cargo >/dev/null 2>&1 || { echo "cargo not found. Install Rust: brew install rustup && rustup default stable" >&2; exit 1; }

cargo build --release --locked
mkdir -p "$PREFIX" "$DATA"
# Keep the previous binary (manual recovery: mv ~/.local/bin/.upkeep.prev ~/.local/bin/upkeep).
[ -f "$PREFIX/upkeep" ] && cp -p "$PREFIX/upkeep" "$PREFIX/.upkeep.prev"
# Copy then rename: replacing a running binary in place is safe this way.
install -m 755 target/release/upkeep "$PREFIX/.upkeep.new"
mv -f "$PREFIX/.upkeep.new" "$PREFIX/upkeep"
pwd > "$DATA/source-dir"

echo "Installed $("$PREFIX/upkeep" --version) to $PREFIX/upkeep"
case ":$PATH:" in
  *":$PREFIX:"*) ;;
  *) echo "Note: $PREFIX is not on PATH. Add to ~/.zshrc:  export PATH=\"$PREFIX:\$PATH\"" ;;
esac
