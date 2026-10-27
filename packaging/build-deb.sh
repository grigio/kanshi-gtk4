#!/usr/bin/env bash
# Build a Debian/Ubuntu package from this checkout.
#
# No debhelper and no dh-cargo: Debian 13 ships a rustc older than the
# rust-version declared in Cargo.toml, so the binary is compiled with whatever
# cargo is on PATH and the package tree is assembled by hand with dpkg-deb.
#
# Usage: packaging/build-deb.sh [version]
# Output: dist/kanshi-gtk4_<version>_<arch>.deb
#
# The package is built against the GTK / libadwaita development packages
# installed on the machine, so run it in a container matching the oldest
# supported distro (Debian 13 / trixie) to keep the dependencies honest.

set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"

version=${1:-$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)}
[ -n "$version" ] || { echo "build-deb: cannot read version from Cargo.toml" >&2; exit 1; }
pkg=kanshi-gtk4
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT

for cmd in cargo dpkg-deb readelf strip; do
  command -v "$cmd" >/dev/null || {
    echo "build-deb: '$cmd' not found; run this on Debian/Ubuntu with cargo and binutils installed" >&2
    exit 1
  }
done

arch=${DEB_ARCH:-$(dpkg --print-architecture)}
out=$root/dist/${pkg}_${version}_${arch}.deb

echo "build-deb: building $pkg $version for $arch"
cargo build --release --locked

install -Dm755 "target/release/$pkg" "$stage/usr/bin/$pkg"
# Debian requires stripped binaries; lintian flags them otherwise.
strip --remove-section=.comment --remove-section=.note --strip-unneeded \
  "$stage/usr/bin/$pkg"
install -Dm644 data/io.github.kanshi_gtk4.desktop \
  "$stage/usr/share/applications/io.github.kanshi_gtk4.desktop"
install -Dm644 data/icons/hicolor/scalable/apps/io.github.kanshi_gtk4.svg \
  "$stage/usr/share/icons/hicolor/scalable/apps/io.github.kanshi_gtk4.svg"

# The binary is built against the build host's glibc, so declare the oldest
# symbol version it actually references instead of hardcoding a guess.
libc_version=$(readelf --version-info "$stage/usr/bin/$pkg" \
  | grep -o 'GLIBC_[0-9.]*' | sed 's/^GLIBC_//' | sort -Vu | tail -n1)
libc_version=${libc_version:-2.39}

docdir=$stage/usr/share/doc/$pkg
mkdir -p "$docdir"
cat > "$docdir/copyright" <<EOF
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: $pkg
Source: https://github.com/grigio/$pkg

Files: *
Copyright: $(date +%Y) grigio
License: GPL-3.0-only
 On Debian systems, the complete text of the GNU General Public License
 version 3 can be found in /usr/share/common-licenses/GPL-3.
EOF

{
  echo "$pkg ($version-1) unstable; urgency=medium"
  echo
  echo "  * Upstream release $version."
  echo
  echo " -- grigio <grigio@users.noreply.github.com>  $(date -R)"
} | gzip -9n > "$docdir/changelog.Debian.gz"

installed_size=$(du -sk --exclude=DEBIAN "$stage" | cut -f1)

mkdir -p "$stage/DEBIAN"
cat > "$stage/DEBIAN/control" <<EOF
Package: $pkg
Version: $version-1
Section: x11
Priority: optional
Architecture: $arch
Maintainer: grigio <grigio@users.noreply.github.com>
Installed-Size: $installed_size
Depends: libc6 (>= $libc_version), libgtk-4-1 (>= 4.18), libadwaita-1-0 (>= 1.7), wlr-randr, hicolor-icon-theme
Recommends: kanshi
Homepage: https://github.com/grigio/$pkg
Description: arrange displays on Wayland and save the layout as a kanshi profile
 A small GTK4 / libadwaita app for Wayland compositors that expose
 zwlr_output_manager_v1 (labwc, niri, river, wayfire, ...). It arranges
 displays with one-click presets or per-output controls and saves the result
 as a kanshi profile, so the layout survives reboot and hotplug.
 .
 A layout is applied immediately; a detached watchdog restores the previous
 one after ten seconds unless it is kept.
EOF

mkdir -p "$root/dist"
dpkg-deb --root-owner-group --build "$stage" "$out"
echo "build-deb: wrote $out"
