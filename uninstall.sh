#!/usr/bin/env bash
#
# Uninstall Vireo for the current user: removes everything install.sh placed
# into the XDG user prefix (binary, icons, desktop entry, translations).
#
# Usage:  ./uninstall.sh            # removes from ~/.local
#         PREFIX=/usr ./uninstall.sh   # system-wide (run with sudo)
set -euo pipefail

APP_ID="co.hyprlab.Vireo"
PREFIX="${PREFIX:-$HOME/.local}"

echo "==> Removing binary"
rm -f "$PREFIX/bin/vireo"

echo "==> Removing icons"
for size in 256x256 512x512; do
    rm -f "$PREFIX/share/icons/hicolor/$size/apps/$APP_ID.png"
done
rm -f "$PREFIX/share/icons/hicolor/scalable/apps/$APP_ID.svg"

echo "==> Removing desktop entry"
rm -f "$PREFIX/share/applications/$APP_ID.desktop"

echo "==> Removing translations"
for mo in "$PREFIX"/share/locale/*/LC_MESSAGES/vireo.mo; do
    [ -e "$mo" ] || continue
    rm -f "$mo"
    rmdir --ignore-fail-on-non-empty "$(dirname "$mo")" 2>/dev/null || true
done

echo "==> Updating caches"
gtk-update-icon-cache -f -t "$PREFIX/share/icons/hicolor" 2>/dev/null || true
update-desktop-database "$PREFIX/share/applications" 2>/dev/null || true

echo "==> Done. Vireo has been removed from $PREFIX."
echo "    Note: config/data at \$XDG_CONFIG_HOME and \$XDG_DATA_HOME for $APP_ID were left in place."
