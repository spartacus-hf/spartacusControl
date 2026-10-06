#!/bin/sh
# Install S-CTRL for this user: the release build as ~/.local/bin/sctrl, its
# icon and a desktop entry (rofi, application menus). Nothing system-wide,
# no root. `scripts/install.sh --uninstall` removes the three again.
set -eu
cd "$(dirname "$0")/.."
data=${XDG_DATA_HOME:-$HOME/.local/share}
bin=$HOME/.local/bin/sctrl
app=$data/applications/sctrl.desktop
icon=$data/icons/hicolor/scalable/apps/sctrl.svg

if [ "${1:-}" = --uninstall ]; then
    rm -f "$bin" "$app" "$icon"
    echo "removed $bin, $app, $icon"
    exit 0
fi

cargo build --release -p sctrl
mkdir -p "$(dirname "$bin")" "$(dirname "$app")" "$(dirname "$icon")"
# Copied beside, then renamed over: a running S-CTRL keeps its own file.
cp "${CARGO_TARGET_DIR:-target}/release/sctrl" "$bin.new"
# The release profile keeps debug info for profiling; the installed copy not.
if command -v strip >/dev/null; then strip --strip-debug "$bin.new"; fi
mv -f "$bin.new" "$bin"
cp packaging/sctrl.svg "$icon"
sed "s|@BIN@|$bin|g" packaging/sctrl.desktop >"$app.new"
mv -f "$app.new" "$app"
# Only where an icon cache already exists: a stale one hides the new icon.
if [ -f "$data/icons/hicolor/icon-theme.cache" ] && command -v gtk-update-icon-cache >/dev/null; then
    gtk-update-icon-cache -q -t -f "$data/icons/hicolor" || true
fi
echo "installed $bin, $app, $icon"
case ":$PATH:" in
*":$(dirname "$bin"):"*) ;;
*) echo "note: $(dirname "$bin") is not on PATH; the menu entry works, 'sctrl' in a shell needs the full path" ;;
esac
