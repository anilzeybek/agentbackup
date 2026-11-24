#!/usr/bin/env sh
set -eu

BINARY_NAME="agentbackup"
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"

# Locate cargo (also check ~/.cargo/bin in case it is not on PATH yet)
if command -v cargo >/dev/null 2>&1; then
    CARGO_BIN=$(command -v cargo)
elif [ -x "$HOME/.cargo/bin/cargo" ]; then
    CARGO_BIN="$HOME/.cargo/bin/cargo"
else
    echo "Error: cargo not found. Install Rust from https://rustup.rs and try again." >&2
    exit 1
fi

echo "Building $BINARY_NAME (release)..."
"$CARGO_BIN" build --release --manifest-path "$SCRIPT_DIR/Cargo.toml"

BUILT_BIN="$SCRIPT_DIR/target/release/$BINARY_NAME"
if [ ! -f "$BUILT_BIN" ]; then
    echo "Error: expected binary not found at $BUILT_BIN" >&2
    exit 1
fi

mkdir -p "$INSTALL_DIR"
if [ ! -w "$INSTALL_DIR" ]; then
    echo "Error: cannot write to $INSTALL_DIR. Set INSTALL_DIR to a writable path or re-run with appropriate permissions." >&2
    exit 1
fi

install -m 755 "$BUILT_BIN" "$INSTALL_DIR/$BINARY_NAME"
echo "Installed $BINARY_NAME to $INSTALL_DIR/$BINARY_NAME"

# Check if INSTALL_DIR is already on PATH
case ":$PATH:" in
    *":$INSTALL_DIR:"*)
        echo "Ready! Run '$BINARY_NAME --help' from any directory."
        ;;
    *)
        echo ""
        echo "Note: $INSTALL_DIR is not currently in your PATH."
        SHELL_NAME=$(basename -- "${SHELL:-sh}")
        case "$SHELL_NAME" in
            zsh)  RC_FILE="$HOME/.zshrc" ;;
            bash) RC_FILE="$HOME/.bashrc" ;;
            *)    RC_FILE="$HOME/.profile" ;;
        esac
        echo "Add it by running:"
        echo "  echo 'export PATH=\"$INSTALL_DIR:\$PATH\"' >> $RC_FILE && source $RC_FILE"
        ;;
esac
