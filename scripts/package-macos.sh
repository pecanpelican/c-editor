#!/bin/sh
# Build cedit.app for macOS.
#
#   scripts/package-macos.sh            # app for this Mac's CPU
#   scripts/package-macos.sh universal  # Apple Silicon + Intel in one app
#
# The app ends up in dist/cedit.app. Drag it to /Applications.
set -eu
cd "$(dirname "$0")/.."

VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
APP=dist/cedit.app

if [ "${1:-}" = "universal" ]; then
    # The extra target is installed through rustup, so build with rustup's cargo
    # even when another Rust (e.g. Homebrew's) comes first on PATH.
    PATH="$(dirname "$(rustup which cargo)"):$PATH"
    export PATH
    rustup target add aarch64-apple-darwin x86_64-apple-darwin
    cargo build --release --target aarch64-apple-darwin
    cargo build --release --target x86_64-apple-darwin
    BIN=target/cedit-universal
    lipo -create -output "$BIN" \
        target/aarch64-apple-darwin/release/cedit \
        target/x86_64-apple-darwin/release/cedit
else
    cargo build --release
    BIN=target/release/cedit
fi

# Make sure debug info is gone even if cargo's own strip step failed. Apple's strip,
# because GNU strip (e.g. Homebrew binutils) can't read universal binaries.
/usr/bin/strip -x "$BIN" 2>/dev/null || true

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/cedit"
sed "s/@VERSION@/$VERSION/g" packaging/Info.plist > "$APP/Contents/Info.plist"

# Ad-hoc signature so Gatekeeper lets a locally built app run.
codesign --force --deep --sign - "$APP" >/dev/null 2>&1 || true

echo "Built $APP (version $VERSION)"
echo "Install:  cp -R $APP /Applications/"
echo "Command line:  ln -sf /Applications/cedit.app/Contents/MacOS/cedit /usr/local/bin/cedit"
