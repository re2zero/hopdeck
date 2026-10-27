#!/bin/sh
# Build a local .deb from the workspace release binaries.
#
# Usage: packaging/debian/build-deb.sh [--skip-build]
#
# The package installs both executables, the desktop entry, the icon,
# and AppStream metadata under /usr, so a normal desktop launcher entry
# runs `hopdeck`, which in turn launches the sibling `hopdeck-console`.

set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
cd "$repo_root"

skip_build=0
for arg in "$@"; do
    case "$arg" in
        --skip-build) skip_build=1 ;;
        -h|--help)
            sed -n '2,10p' "$0"
            exit 0
            ;;
        *)
            echo "unknown argument: $arg" >&2
            exit 2
            ;;
    esac
done

version=$(awk '
    /^\[workspace.package\]/ { in_section = 1; next }
    /^\[/ { in_section = 0 }
    in_section && /^version = "/ { gsub(/^version = "|"$/, ""); print; exit }
' Cargo.toml)
if [ -z "$version" ]; then
    echo "could not read the workspace version from Cargo.toml" >&2
    exit 1
fi

arch=${DEB_ARCH:-$(dpkg --print-architecture)}
maintainer_name=$(git config user.name 2>/dev/null || echo re2zero)
maintainer_email=$(git config user.email 2>/dev/null || echo re2zero@users.noreply.github.com)
maintainer="$maintainer_name <$maintainer_email>"

if [ "$skip_build" -eq 0 ]; then
    echo "==> building release binaries"
    cargo build --release --locked
    cargo build --release --locked -p hopdeck-console
fi

for bin in target/release/hopdeck target/release/hopdeck-console; do
    if [ ! -x "$bin" ]; then
        echo "missing $bin; run without --skip-build first" >&2
        exit 1
    fi
done

stage="target/deb/hopdeck_${version}_${arch}"
rm -rf "$stage"
install -d \
    "$stage/DEBIAN" \
    "$stage/usr/bin" \
    "$stage/usr/share/applications" \
    "$stage/usr/share/icons/hicolor/scalable/apps" \
    "$stage/usr/share/metainfo" \
    "$stage/usr/share/doc/hopdeck"

install -m 0755 target/release/hopdeck "$stage/usr/bin/hopdeck"
install -m 0755 target/release/hopdeck-console "$stage/usr/bin/hopdeck-console"
install -m 0644 apps/hopdeck/com.hopdeck.HopDeck.desktop \
    "$stage/usr/share/applications/com.hopdeck.HopDeck.desktop"
install -m 0644 apps/hopdeck/assets/com.hopdeck.HopDeck.svg \
    "$stage/usr/share/icons/hicolor/scalable/apps/com.hopdeck.HopDeck.svg"
install -m 0644 build-aux/com.hopdeck.HopDeck.metainfo.xml \
    "$stage/usr/share/metainfo/com.hopdeck.HopDeck.metainfo.xml"
install -m 0644 LICENSE NOTICE "$stage/usr/share/doc/hopdeck/"
install -m 0644 packaging/debian/copyright "$stage/usr/share/doc/hopdeck/copyright"
install -m 0755 packaging/debian/postinst "$stage/DEBIAN/postinst"
install -m 0755 packaging/debian/postrm "$stage/DEBIAN/postrm"

# Raster icon ladder for hosts that prefer fixed-size icons. SVG is the
# source of truth and always installed.
if command -v rsvg-convert >/dev/null 2>&1; then
    for size in 16 22 24 32 48 64 128 256 512; do
        dir="$stage/usr/share/icons/hicolor/${size}x${size}/apps"
        install -d "$dir"
        rsvg-convert -w "$size" -h "$size" \
            apps/hopdeck/assets/com.hopdeck.HopDeck.svg \
            -o "$dir/com.hopdeck.HopDeck.png"
    done
else
    echo "warning: rsvg-convert not found; installing the SVG icon only" >&2
fi

installed_size=$(du -sk "$stage/usr" | cut -f1)
cat > "$stage/DEBIAN/control" <<EOF
Package: hopdeck
Version: $version
Architecture: $arch
Maintainer: $maintainer
Installed-Size: $installed_size
Depends: libc6, libgcc-s1, libx11-6, libxtst6, libxkbcommon0, libxkbcommon-x11-0
Recommends: libwayland-client0, libwayland-cursor0, libvulkan1
Section: net
Priority: optional
Homepage: https://github.com/re2zero/hopdeck
Description: mouse and keyboard sharing over the local network
 HopDeck (桌联) shares one mouse and keyboard across machines on a
 local network, including clipboard and optional screen sharing.
 The package provides the daemon/CLI and the GPUI control console.
EOF

{
    echo "hopdeck ($version) unstable; urgency=medium"
    echo
    echo "  * Local build from the HopDeck workspace."
    echo
    echo " -- $maintainer  $(date -R)"
} > "$stage/usr/share/doc/hopdeck/changelog.Debian"
gzip -9n "$stage/usr/share/doc/hopdeck/changelog.Debian"

deb="target/deb/hopdeck_${version}_${arch}.deb"
dpkg-deb --root-owner-group -Zxz --build "$stage" "$deb"
echo "==> built $deb"
