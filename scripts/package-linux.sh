#!/bin/sh
# Build a Linux tarball: the binary, a desktop entry, an install script and the docs.
#
#   scripts/package-linux.sh                 # -> dist/cedit-linux.tar.gz
#   scripts/package-linux.sh out.tar.gz
set -eu
cd "$(dirname "$0")/.."
OUT=${1:-dist/cedit-linux.tar.gz}

cargo build --release
rm -rf dist/cedit
mkdir -p dist/cedit
cp target/release/cedit README.md LICENSE packaging/cedit.desktop packaging/install-linux.sh dist/cedit/
chmod +x dist/cedit/install-linux.sh
mkdir -p "$(dirname "$OUT")"
tar -C dist -czf "$OUT" cedit
echo "Built $OUT"
