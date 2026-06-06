#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"

echo "[1/3] Installing musl target..."
rustup target add x86_64-unknown-linux-musl

echo "[2/3] Building rinit (static musl)..."
cargo build --release --target x86_64-unknown-linux-musl
# Strip to shrink binary
strip target/x86_64-unknown-linux-musl/release/rinit
ls -lh target/x86_64-unknown-linux-musl/release/rinit

echo "[3/3] Verify: no dynamic deps..."
ldd target/x86_64-unknown-linux-musl/release/rinit 2>&1 || true
file target/x86_64-unknown-linux-musl/release/rinit

echo ""
echo "Done! Binary at: target/x86_64-unknown-linux-musl/release/rinit"
