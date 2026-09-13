# Maintainer: LapSphere Team <hungaryam@gmail.com>
pkgname=lapsphere
pkgver=0.1.0
pkgrel=1
pkgdesc="Hardware control application for Uniwill/Clevo laptops"
arch=('x86_64')
url="https://github.com/weter11/lapsphere"
license=('GPL2')
depends=('dbus' 'polkit' 'libxkbcommon-x11' 'dmidecode' 'pciutils' 'ethtool' 'iw' 'gtk3' 'libadwaita')
optdepends=('optimus-manager: GPU switching support on Arch Linux')
makedepends=('cargo' 'pkgconf')
source=("lapsphere-$pkgver.tar.gz") # This will be handled by the CI or manual packaging
sha256sums=('SKIP')

build() {
  cd "$pkgname-$pkgver"
  # Arch's rustup toolchain defaults to rust-lld, which fails to resolve
  # the bundled mimalloc archive during the release link.
  export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-C link-arg=-fuse-ld=bfd"
  cargo build --release --all
}

package() {
  cd "$pkgname-$pkgver"
  # Binaries
  install -Dm755 target/release/lapsphere-daemon "$pkgdir/usr/bin/lapsphere-daemon"
  install -Dm755 target/release/lapsphere "$pkgdir/usr/bin/lapsphere"

  # DBus
  install -Dm644 data/io.lapsphere.Control.conf "$pkgdir/usr/share/dbus-1/system.d/io.lapsphere.Control.conf"
  install -Dm644 data/io.lapsphere.Control.service "$pkgdir/usr/share/dbus-1/system-services/io.lapsphere.Control.service"

  # systemd unit: supervision + resource caps for the daemon
  install -Dm644 data/lapsphere-daemon.service "$pkgdir/usr/lib/systemd/system/lapsphere-daemon.service"

  # Desktop & Autostart
  install -Dm644 data/io.lapsphere.LapSphere.desktop "$pkgdir/usr/share/applications/io.lapsphere.LapSphere.desktop"
  install -Dm644 data/io.lapsphere.LapSphere.desktop "$pkgdir/etc/xdg/autostart/io.lapsphere.LapSphere.desktop"
  sed -i 's/Exec=lapsphere/Exec=lapsphere --tray/' "$pkgdir/etc/xdg/autostart/io.lapsphere.LapSphere.desktop"

  # Icon
  install -Dm644 data/icon.svg "$pkgdir/usr/share/icons/hicolor/scalable/apps/lapsphere.svg"
}
