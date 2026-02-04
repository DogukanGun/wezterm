#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

echo "==> Building wezterm-gui"
cargo build -p wezterm-gui

echo "==> Running wezterm-gui"
echo "If no config exists, the first-run wizard will appear."
export RUST_BACKTRACE=1
export RUST_LOG=${RUST_LOG:-wezterm_gui=debug,window=debug,mux=debug,config=debug}
pkill -f "target/debug/wezterm-gui" >/dev/null 2>&1 || true
cargo run -p wezterm-gui -- start --always-new-process
