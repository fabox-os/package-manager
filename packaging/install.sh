#!/bin/sh
# Developer install from a release build (end users should use the .deb from build-deb.sh).
# Installs (or removes) Package Manager system-wide from a release build.
#
#   cargo build --release
#   ./packaging/install.sh            # install
#   ./packaging/install.sh uninstall  # remove
#
# Release builds only run the privileged helper from /usr/libexec/package-manager/, owned by
# root, so this step is required once (and after each rebuild). It asks for your administrator
# password through pkexec. Packaging only: no application logic lives here.
set -eu

here=$(cd "$(dirname "$0")/.." && pwd)
helper_dir=/usr/libexec/package-manager
ID=org.packagemanager.PackageManager

if [ "$(id -u)" -ne 0 ]; then
    exec /usr/bin/pkexec "$here/packaging/install.sh" "$@"
fi

case "${1:-install}" in
install)
    for f in target/release/package-manager-app target/release/package-manager-helper \
        data/polkit/org.packagemanager.helper.policy data/org.packagemanager.PackageManager.desktop \
        data/icons/package-manager.png data/icons/package-manager-256.png; do
        [ -f "$here/$f" ] || { echo "Missing $f. Run: cargo build --release" >&2; exit 1; }
    done
    install -d -m 0755 -o root -g root "$helper_dir"
    install -m 0755 -o root -g root "$here/target/release/package-manager-helper" "$helper_dir/package-manager-helper"
    install -m 0644 -o root -g root "$here/data/polkit/org.packagemanager.helper.policy" /usr/share/polkit-1/actions/org.packagemanager.helper.policy
    install -m 0755 -o root -g root "$here/target/release/package-manager-app" /usr/local/bin/package-manager
    install -m 0644 -o root -g root "$here/data/org.packagemanager.PackageManager.desktop" /usr/share/applications/org.packagemanager.PackageManager.desktop
    install -d -m 0755 /usr/share/icons/hicolor/512x512/apps /usr/share/icons/hicolor/256x256/apps
    install -m 0644 "$here/data/icons/package-manager.png" /usr/share/icons/hicolor/512x512/apps/org.packagemanager.PackageManager.png
    install -m 0644 "$here/data/icons/package-manager-256.png" /usr/share/icons/hicolor/256x256/apps/org.packagemanager.PackageManager.png
    command -v gtk-update-icon-cache >/dev/null 2>&1 && gtk-update-icon-cache -q -t /usr/share/icons/hicolor || true
    # Files of earlier builds, which used other names.
    rm -f /usr/local/bin/upalume /usr/local/bin/update-manager /usr/share/applications/update-manager.desktop \
        /usr/share/applications/org.updatemanager.UpdateManager.desktop /usr/share/polkit-1/actions/org.updatemanager.helper.policy \
        /usr/share/icons/hicolor/*/apps/update-manager.png /usr/share/icons/hicolor/*/apps/org.updatemanager.UpdateManager.png
    rm -rf /usr/libexec/update-manager
    echo "Installed. Start it from your app menu or run: package-manager"
    ;;
uninstall)
    rm -f "$helper_dir/package-manager-helper" /usr/share/polkit-1/actions/org.packagemanager.helper.policy \
        /usr/local/bin/package-manager /usr/share/applications/org.packagemanager.PackageManager.desktop \
        /usr/share/icons/hicolor/512x512/apps/org.packagemanager.PackageManager.png /usr/share/icons/hicolor/256x256/apps/org.packagemanager.PackageManager.png
    rmdir "$helper_dir" 2>/dev/null || true
    echo "Removed."
    ;;
*)
    echo "Usage: $0 [install|uninstall]" >&2
    exit 2
    ;;
esac
