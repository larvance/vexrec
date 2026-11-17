#!/usr/bin/env bash
set -euo pipefail

VERSION="${1:-0.3.0}"
ARCH="amd64"
PKG_DIR="target/debian/vexrec_${VERSION}_${ARCH}"

echo "Building Debian/Ubuntu package for Vexrec v${VERSION}..."

rm -rf "${PKG_DIR}"
mkdir -p "${PKG_DIR}/DEBIAN"
mkdir -p "${PKG_DIR}/usr/bin"
mkdir -p "${PKG_DIR}/usr/share/applications"
mkdir -p "${PKG_DIR}/usr/share/icons/hicolor/32x32/apps"
mkdir -p "${PKG_DIR}/usr/share/icons/hicolor/128x128/apps"
mkdir -p "${PKG_DIR}/usr/share/icons/hicolor/256x256/apps"
mkdir -p "${PKG_DIR}/usr/share/doc/vexrec"
mkdir -p "${PKG_DIR}/usr/share/bash-completion/completions"

cat <<EOF > "${PKG_DIR}/DEBIAN/control"
Package: vexrec
Version: ${VERSION}
Section: utils
Priority: optional
Architecture: ${ARCH}
Maintainer: Aerovex HQ <dev@aerovex.net>
Depends: libc6 (>= 2.31)
Recommends: pipewire, xdg-desktop-portal, libva2, libvulkan1
Homepage: https://github.com/aerovexhq/vexrec
Description: Pure-Rust bare-metal screenshot and screen recorder for Linux
 Vexrec is an ultra-low-latency Linux screenshot and screen recording suite
 written purely in Rust targeting X11 and Wayland (Hyprland, Sway, KDE, GNOME)
 with zero GTK, GStreamer, WebKit, or tokio runtime dependencies.
EOF

# Copy binary and strip debug symbols
cp target/release/vexrec "${PKG_DIR}/usr/bin/vexrec"
chmod 755 "${PKG_DIR}/usr/bin/vexrec"
strip "${PKG_DIR}/usr/bin/vexrec" 2>/dev/null || true
ln -sf vexrec "${PKG_DIR}/usr/bin/luxrec"

cp data/vexrec.desktop "${PKG_DIR}/usr/share/applications/vexrec.desktop"
chmod 644 "${PKG_DIR}/usr/share/applications/vexrec.desktop"

cp crates/vexrec-ui/icons/32x32.png "${PKG_DIR}/usr/share/icons/hicolor/32x32/apps/vexrec.png"
cp crates/vexrec-ui/icons/128x128.png "${PKG_DIR}/usr/share/icons/hicolor/128x128/apps/vexrec.png"
cp crates/vexrec-ui/icons/128x128@2x.png "${PKG_DIR}/usr/share/icons/hicolor/256x256/apps/vexrec.png"

# Generate bash completion
if [ -x target/release/vexrec ]; then
    target/release/vexrec completions bash > "${PKG_DIR}/usr/share/bash-completion/completions/vexrec" || true
fi

cp LICENSE "${PKG_DIR}/usr/share/doc/vexrec/copyright"
cp README.md "${PKG_DIR}/usr/share/doc/vexrec/README"

# Build debian package
dpkg-deb --build --root-owner-group "${PKG_DIR}" "target/vexrec_${VERSION}_${ARCH}.deb"

echo "Debian package successfully built: target/vexrec_${VERSION}_${ARCH}.deb"
