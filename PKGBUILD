# Builds repackd from this folder: run `makepkg -si` next to this file.
pkgname=repackd
pkgver=0.1.0
pkgrel=1
pkgdesc="Install, update and play local game repacks with Proton (umu) or emulators"
arch=('x86_64')
license=('MIT')
depends=('umu-launcher' 'curl' 'gcc-libs' 'glibc' 'libglvnd' 'libxkbcommon' 'systemd-libs' 'xdg-utils')
makedepends=('cargo')
optdepends=(
  'winetricks: prefix tools and missing runtimes'
  '7zip: scene update archives (.rar, .7z, .zip)'
  'libnotify: a notification when a long job ends'
  'mangohud: the MangoHud switch'
  'gamemode: the GameMode switch'
  'gamescope: the Gamescope switch'
  'xdg-desktop-portal: file and folder pickers'
  'fontconfig: Chinese/Japanese/Korean game names'
)
options=('!lto')

build() {
  cargo build --release --locked --manifest-path "$startdir/Cargo.toml" --target-dir "$srcdir/target"
}

check() {
  cargo test --release --locked --manifest-path "$startdir/Cargo.toml" --target-dir "$srcdir/target"
}

package() {
  install -Dm755 "$srcdir/target/release/repackd" "$pkgdir/usr/bin/repackd"
  install -Dm644 "$startdir/LICENSE" "$pkgdir/usr/share/licenses/repackd/LICENSE"
  install -Dm644 "$startdir/repackd.desktop" "$pkgdir/usr/share/applications/repackd.desktop"
}
