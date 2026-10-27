# Maintainer: grigio <https://github.com/grigio>
# Arch Linux local package for kanshi-gtk4 (not published to the AUR).
#
# Run from the repository root:
#   makepkg -si
#   makepkg && sudo pacman -U kanshi-gtk4-*.pkg.tar.zst
#
# This PKGBUILD builds from the checkout in $startdir. Do not pass
# --cleanbuild: makepkg's default $srcdir is ./src, which is the Rust source
# directory.

pkgname=kanshi-gtk4
pkgver=0.3.1
pkgrel=1
pkgdesc="Arrange displays on Wayland and save the layout as a kanshi profile"
arch=('x86_64')
url="https://github.com/grigio/kanshi-gtk4"
license=('GPL-3.0-only')
depends=('gtk4' 'libadwaita' 'wlr-randr' 'hicolor-icon-theme')
makedepends=('cargo' 'pkgconf')
optdepends=('kanshi: persist layouts across reboot and hotplug')
checkdepends=('desktop-file-utils' 'python')
source=()
sha256sums=()

prepare() {
  cd "$startdir"
  export CARGO_HOME="$startdir/.cargo-home"
  cargo fetch --locked
}

build() {
  cd "$startdir"
  export CARGO_HOME="$startdir/.cargo-home"
  cargo build --release --locked
}

check() {
  cd "$startdir"
  export CARGO_HOME="$startdir/.cargo-home"
  desktop-file-validate data/io.github.kanshi_gtk4.desktop
  cargo test --locked
}

package() {
  cd "$startdir"
  install -Dm755 target/release/kanshi-gtk4 "$pkgdir/usr/bin/kanshi-gtk4"
  install -Dm644 data/io.github.kanshi_gtk4.desktop \
    "$pkgdir/usr/share/applications/io.github.kanshi_gtk4.desktop"
  install -Dm644 data/icons/hicolor/scalable/apps/io.github.kanshi_gtk4.svg \
    "$pkgdir/usr/share/icons/hicolor/scalable/apps/io.github.kanshi_gtk4.svg"
  install -Dm644 LICENSE "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
}
