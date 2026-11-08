#!/usr/bin/env bash
# Install runbox for the current user. No sudo, and safe to run again.
#
#   ./install.sh [path/to/runbox]      (default: target/release/runbox in this repo)
#
# A relative path is taken from the folder you run this from, not the repo.
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
# Resolve the binary before the cd below, so a relative path keeps meaning the caller's folder.
if [[ -n "${1:-}" ]]; then
    BIN=$(realpath -m -- "$1")
else
    BIN="$HERE/target/release/runbox"
fi
cd "$HERE"

APP_ID=io.github.ajonesfl2600.runbox
# The app ID before the public release. Its installed files are removed below, so an
# upgrade doesn't leave a second Runbox entry in the menu.
OLD_APP_ID=dev.ajones.runbox

DATA=${XDG_DATA_HOME:-$HOME/.local/share}
ICONS="$DATA/icons/hicolor"
DEST="$HOME/.local/bin/runbox"

[[ -f "$BIN" && -x "$BIN" ]] || { echo "no binary at $BIN (run: cargo build --release)" >&2; exit 1; }
[[ "$DEST" != *$'\n'* ]] || { echo "HOME has a newline in it; can't write a .desktop file" >&2; exit 1; }

# Exec= in a .desktop file: quote the path, backslash-escape " ` $ \ inside the quotes,
# then escape every \ again for the key file and double any % (spec 1.5, "The Exec key").
desktop_exec() {
    local s=$1
    s=${s//\\/\\\\}
    s=${s//\"/\\\"}
    s=${s//\`/\\\`}
    s=${s//\$/\\\$}
    s="\"$s\""
    s=${s//\\/\\\\}
    s=${s//%/%%}
    printf '%s' "$s"
}

install -Dm755 "$BIN" "$DEST"
install -Dm644 "data/icons/hicolor/scalable/apps/$APP_ID.svg" \
    "$ICONS/scalable/apps/$APP_ID.svg"

# The menu starts apps with the session PATH, which may not include ~/.local/bin.
# So the installed copy points Exec= at the binary we just installed.
DESKTOP_TMP=$(mktemp)
trap 'rm -f "$DESKTOP_TMP"' EXIT
EXEC_LINE="Exec=$(desktop_exec "$DEST")"
while IFS= read -r line || [[ -n "$line" ]]; do
    [[ "$line" == Exec=* ]] && line=$EXEC_LINE
    printf '%s\n' "$line"
done <"data/$APP_ID.desktop" >"$DESKTOP_TMP"
grep -qxF -- "$EXEC_LINE" "$DESKTOP_TMP" || { echo "data/$APP_ID.desktop has no Exec= line" >&2; exit 1; }
install -Dm644 "$DESKTOP_TMP" "$DATA/applications/$APP_ID.desktop"

# Only after the new files are in place, so a failed install never leaves no entry at all.
rm -f -- "$DATA/applications/$OLD_APP_ID.desktop" "$ICONS/scalable/apps/$OLD_APP_ID.svg"

if command -v gtk-update-icon-cache >/dev/null; then
    # Without an index.theme the cache tool refuses; --ignore-theme-index covers a fresh home.
    gtk-update-icon-cache --force --ignore-theme-index --quiet "$ICONS" || true
fi
if command -v update-desktop-database >/dev/null; then
    update-desktop-database --quiet "$DATA/applications" || true
fi
echo "installed runbox to $DEST"
