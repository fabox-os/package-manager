#!/bin/sh
# Builds dist/package-manager_<version>_<arch>.deb from a release build.
#
#   cargo build --release
#   ./packaging/build-deb.sh
#   sudo apt install ./dist/package-manager_*.deb
#
# Packaging only: no application logic lives here. Needs dpkg-deb and fakeroot.
set -eu

here=$(cd "$(dirname "$0")/.." && pwd)
cd "$here"

for f in target/release/package-manager-app target/release/package-manager-helper; do
    [ -f "$f" ] || { echo "Missing $f. Run: cargo build --release" >&2; exit 1; }
done

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
arch=$(dpkg --print-architecture)
pkg=package-manager
root=$(mktemp -d)
trap 'rm -rf "$root"' EXIT

# The helper runs as root, so it must live in a root-owned folder (fakeroot makes it so).
install -d -m 0755 "$root/usr/bin" "$root/usr/libexec/package-manager" \
    "$root/usr/share/polkit-1/actions" "$root/usr/share/applications" \
    "$root/usr/share/icons/hicolor/512x512/apps" "$root/usr/share/icons/hicolor/256x256/apps" \
    "$root/usr/share/doc/$pkg" "$root/DEBIAN"
install -m 0755 target/release/package-manager-app "$root/usr/bin/package-manager"
install -m 0755 target/release/package-manager-helper "$root/usr/libexec/package-manager/package-manager-helper"
install -m 0644 data/polkit/org.packagemanager.helper.policy "$root/usr/share/polkit-1/actions/"
install -m 0644 data/org.packagemanager.PackageManager.desktop "$root/usr/share/applications/"
install -m 0644 data/icons/package-manager.png "$root/usr/share/icons/hicolor/512x512/apps/org.packagemanager.PackageManager.png"
install -m 0644 data/icons/package-manager-256.png "$root/usr/share/icons/hicolor/256x256/apps/org.packagemanager.PackageManager.png"
strip --strip-unneeded "$root/usr/bin/package-manager" "$root/usr/libexec/package-manager/package-manager-helper"
printf '%s (%s) unstable; urgency=low\n\n  * Initial alpha release.\n\n -- fabox-os <85080530+fabox-os@users.noreply.github.com>  %s\n' "$pkg" "$version" "$(date -R)" | gzip -9n > "$root/usr/share/doc/$pkg/changelog.gz"
chmod 0644 "$root/usr/share/doc/$pkg/changelog.gz"
# Debian policy: the copyright file names the license and points at the system copy of it.
cat > "$root/usr/share/doc/$pkg/copyright" <<COPYRIGHT
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: Package Manager

Files: *
Copyright: 2026 Package Manager contributors
License: GPL-3.0-or-later

License: GPL-3.0-or-later
 This program is free software: you can redistribute it and/or modify it under
 the terms of the GNU General Public License as published by the Free Software
 Foundation, either version 3 of the License, or (at your option) any later
 version.
 .
 On Debian and Ubuntu systems the full text is in
 /usr/share/common-licenses/GPL-3.
COPYRIGHT
chmod 0644 "$root/usr/share/doc/$pkg/copyright"
install -m 0644 README.md "$root/usr/share/doc/$pkg/README.md"

# No folder may be group- or world-writable: the app refuses a helper whose folder is.
find "$root" -type d -exec chmod 0755 {} +
chmod 0755 "$root/DEBIAN"

size=$(du -sk "$root" | cut -f1)
cat > "$root/DEBIAN/control" <<CONTROL
Package: $pkg
Version: $version
Section: admin
Priority: optional
Architecture: $arch
Installed-Size: $size
Depends: libc6, libgcc-s1, pkexec | policykit-1
Recommends: libwayland-client0, libxkbcommon0, libvulkan1 | mesa-vulkan-drivers
Suggests: flatpak
Maintainer: fabox-os <85080530+fabox-os@users.noreply.github.com>
Homepage: https://github.com/fabox-os/package-manager
Description: One window to update APT, Flatpak and GitHub apps
 Package Manager (command: package-manager) lists updates for system packages,
 Flatpak apps and apps you installed from GitHub .deb releases, shows where
 each one comes from, and installs them through a small privileged helper
 that asks for your password.
 .
 Alpha software: the helper runs as root and has not had an independent
 review.
CONTROL

cat > "$root/DEBIAN/postinst" <<'POSTINST'
#!/bin/sh
set -e
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -q -t /usr/share/icons/hicolor || true
fi
exit 0
POSTINST
chmod 0755 "$root/DEBIAN/postinst"

mkdir -p dist
out="dist/${pkg}_${version}_${arch}.deb"
fakeroot dpkg-deb --build --root-owner-group "$root" "$out" >/dev/null
echo "Built $out"
