#!/bin/sh
# Installs cedit for the current user: ~/.local/bin/cedit and an app menu entry.
# Run it from the unpacked folder:  ./install-linux.sh
# Remove it again with:             ./install-linux.sh --uninstall
set -eu
cd "$(dirname "$0")"
BIN="$HOME/.local/bin"
APPS="${XDG_DATA_HOME:-$HOME/.local/share}/applications"

if [ "${1:-}" = "--uninstall" ]; then
    rm -f "$BIN/cedit" "$APPS/cedit.desktop"
    echo "Removed cedit."
    exit 0
fi

mkdir -p "$BIN" "$APPS"
cp cedit "$BIN/cedit"
chmod +x "$BIN/cedit"
sed "s|^Exec=cedit|Exec=$BIN/cedit|" cedit.desktop > "$APPS/cedit.desktop"
command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$APPS" >/dev/null 2>&1 || true

echo "Installed $BIN/cedit"
case ":$PATH:" in
    *":$BIN:"*) ;;
    *) echo "Add $BIN to your PATH to run 'cedit' from a terminal." ;;
esac
