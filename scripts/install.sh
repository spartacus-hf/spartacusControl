#!/bin/sh
# Install S-CTRL for this user: the release build as ~/.local/bin/sctrl, its
# icon and a desktop entry (rofi, application menus). Nothing system-wide,
# no root. `scripts/install.sh --uninstall` removes the three again.
# `--link`, for working on S-CTRL: ~/.local/bin/sctrl links to this
# checkout's release build instead of a copy, so the menu entry always starts
# the last `cargo build --release`. A plain install makes it a copy again.
set -eu
cd "$(dirname "$0")/.."
data=${XDG_DATA_HOME:-$HOME/.local/share}
bin=$HOME/.local/bin/sctrl
app=$data/applications/sctrl.desktop
icon=$data/icons/hicolor/scalable/apps/sctrl.svg

link=
case "${1:-}" in
"") ;;
--link) link=1 ;;
--uninstall)
    rm -f "$bin" "$app" "$icon"
    echo "removed $bin, $app, $icon"
    exit 0
    ;;
*)
    echo "usage: scripts/install.sh [--link | --uninstall]" >&2
    exit 2
    ;;
esac

cargo build --release -p sctrl
mkdir -p "$(dirname "$bin")" "$(dirname "$app")" "$(dirname "$icon")"
built=$(cd "${CARGO_TARGET_DIR:-target}/release" && pwd)/sctrl
# Made beside, then renamed over: a running S-CTRL keeps its own file.
if [ -n "$link" ]; then
    ln -sf "$built" "$bin.new"
else
    cp "$built" "$bin.new"
    # The release profile keeps debug info for profiling; the installed copy not.
    if command -v strip >/dev/null; then strip --strip-debug "$bin.new"; fi
fi
mv -f "$bin.new" "$bin"
cp packaging/sctrl.svg "$icon"
sed "s|@BIN@|$bin|g" packaging/sctrl.desktop >"$app.new"
mv -f "$app.new" "$app"
# Only where an icon cache already exists: a stale one hides the new icon.
if [ -f "$data/icons/hicolor/icon-theme.cache" ] && command -v gtk-update-icon-cache >/dev/null; then
    gtk-update-icon-cache -q -t -f "$data/icons/hicolor" || true
fi
echo "installed $bin${link:+ -> $built}, $app, $icon"
case ":$PATH:" in
*":$(dirname "$bin"):"*) ;;
*) echo "note: $(dirname "$bin") is not on PATH; the menu entry works, 'sctrl' in a shell needs the full path" ;;
esac
