#!/usr/bin/env bash
# Build and package bsnfs for Linux as a static (musl) binary, so one build
# runs on every distribution: a tarball, deb, rpm and Arch packages that
# install the systemd service (not enabled; see installer/linux/postinst),
# and an AppImage.
#
#   scripts/dist/package-linux.sh [options] [--formats tar,deb,rpm,pkg,appimage]
#
# --target defaults to the host's architecture with musl, e.g.
# x86_64-unknown-linux-musl. For another architecture (riscv64 from x86_64)
# the script cross-links with <triple>-gcc, strips with <triple>-strip, and
# runs the new binary under qemu-<arch>-static (it runs the binary to write
# its starter config).
#
# Needs: musl-tools, dpkg-deb, rpmbuild, zstd, file, curl; for a cross build
# also gcc-<triple> and qemu-user-static.
source "$(dirname "$0")/lib.sh"

FORMATS="tar,deb,rpm,pkg,appimage"
parse_common_args "$@"
set -- ${REST[@]+"${REST[@]}"}
while [ $# -gt 0 ]; do
    case "$1" in
        --formats) FORMATS="$2"; shift 2 ;;
        -h|--help) sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//'; common_usage; exit 0 ;;
        *) die "unknown option: $1" ;;
    esac
done
[ "$(uname -s)" = Linux ] || die "package-linux.sh runs on Linux"
case "$TARGET" in
    *-linux-gnu) TARGET="${TARGET%-gnu}-musl" ;;
esac
case "$TARGET" in
    x86_64-unknown-linux-musl)    ARCH=x64;     DEB_ARCH=amd64;   PKG_ARCH=x86_64;  GNU=x86_64-linux-gnu ;;
    aarch64-unknown-linux-musl)   ARCH=arm64;   DEB_ARCH=arm64;   PKG_ARCH=aarch64; GNU=aarch64-linux-gnu ;;
    riscv64gc-unknown-linux-musl) ARCH=riscv64; DEB_ARCH=riscv64; PKG_ARCH=riscv64; GNU=riscv64-linux-gnu ;;
    *) die "unsupported Linux target $TARGET (want x86_64, aarch64 or riscv64gc -unknown-linux-musl)" ;;
esac

# Always link statically: the x86_64 and aarch64 musl targets do by default,
# but riscv64gc-unknown-linux-musl doesn't.
var="CARGO_TARGET_$(echo "$TARGET" | tr 'a-z-' 'A-Z_')_RUSTFLAGS"
export "$var=-C target-feature=+crt-static"

# Cross build: link, strip and run with the target's tools.
STRIP=strip
RUN=()
if [ "$(uname -m)" != "$PKG_ARCH" ]; then
    need "$GNU-gcc" "$GNU-strip" "qemu-$PKG_ARCH-static"
    var="CARGO_TARGET_$(echo "$TARGET" | tr 'a-z-' 'A-Z_')_LINKER"
    export "$var=$GNU-gcc"
    STRIP="$GNU-strip"
    RUN=("qemu-$PKG_ARCH-static")
fi

build "$TARGET"
BIN="$(release_dir "$TARGET")/bsnfs"
[ -x "$BIN" ] || die "$BIN was not built"
"$STRIP" "$BIN"
file "$BIN" 2>/dev/null | grep -q "statically linked\|static-pie" || die "$BIN is not statically linked"
${RUN[@]+"${RUN[@]}"} "$BIN" --version

PKGVER=$(pkg_version "$VERSION")
BASE="bsnfs-linux-${ARCH}-${VERSION}"
has_format() { case ",$FORMATS," in *",$1,"*) return 0 ;; *) return 1 ;; esac; }

# The installed file tree shared by every package format.
stage_tree() {  # <dir>
    install -Dm755 "$BIN" "$1/usr/bin/bsnfs"
    install -Dm644 installer/linux/bsnfs.service "$1/usr/lib/systemd/system/bsnfs.service"
    install -Dm644 installer/linux/sysusers.conf "$1/usr/lib/sysusers.d/bsnfs.conf"
    install -d "$1/etc/bsnfs"
    ${RUN[@]+"${RUN[@]}"} "$BIN" service starter-config > "$1/etc/bsnfs/bsnfs.conf"
    chmod 0640 "$1/etc/bsnfs/bsnfs.conf"
    for f in "${DOC_FILES[@]}"; do install -Dm644 "$f" "$1/usr/share/doc/bsnfs/$f"; done
}

SUMMARY="Share a folder with old and new NFS clients"
DESCRIPTION="bsnfs shares host folders over NFS v2 and v3 (UDP and TCP) with a
built-in portmapper, mount service and lock manager. Clients see synthetic
ownership and permissions; the host's own permissions still apply. Access is
confined to the shared folders."

PRERM='if [ "$1" = remove ] || [ "$1" = 0 ]; then systemctl disable --now bsnfs >/dev/null 2>&1 || true; fi'

if has_format tar; then
    group_start "tarball"
    S=$(stage_dir)
    cp "$BIN" "${DOC_FILES[@]}" "$S/"
    tar -C "$S" -czf "$OUT_DIR/$BASE.tar.gz" bsnfs "${DOC_FILES[@]}"
    rm -rf "$S"
    verify_members "$OUT_DIR/$BASE.tar.gz" bsnfs "${DOC_FILES[@]}"
    group_end
fi

if has_format deb; then
    group_start "deb"
    need dpkg-deb
    S=$(stage_dir)
    stage_tree "$S"
    mkdir -p "$S/DEBIAN"
    cat > "$S/DEBIAN/control" <<CONTROL
Package: bsnfs
Version: $PKGVER
Architecture: $DEB_ARCH
Maintainer: danifunker <danifunker@users.noreply.github.com>
Section: net
Priority: optional
Installed-Size: $(du -sk "$S/usr" | cut -f1)
Description: $SUMMARY
$(printf '%s\n' "$DESCRIPTION" | sed 's/^/ /')
CONTROL
    echo /etc/bsnfs/bsnfs.conf > "$S/DEBIAN/conffiles"
    install -m755 installer/linux/postinst "$S/DEBIAN/postinst"
    printf '#!/bin/sh\n%s\n' "$PRERM" > "$S/DEBIAN/prerm"
    chmod 755 "$S/DEBIAN/prerm"
    dpkg-deb --root-owner-group --build "$S" "$OUT_DIR/$BASE.deb"
    rm -rf "$S"
    dpkg-deb -f "$OUT_DIR/$BASE.deb" Package Version Architecture
    group_end
fi

if has_format rpm; then
    group_start "rpm"
    need rpmbuild
    S=$(stage_dir)
    stage_tree "$S/root"
    mkdir -p "$S/top"
    cat > "$S/bsnfs.spec" <<SPEC
%global debug_package %{nil}
%global __os_install_post %{nil}
Name: bsnfs
Version: $(echo "$PKGVER" | tr '-' '_')
Release: 1
Summary: $SUMMARY
License: MIT AND BSD-3-Clause
BuildArch: $PKG_ARCH

%description
$DESCRIPTION

%install
cp -a "$S/root/." "%{buildroot}/"

%post
$(sed '1,/^set -e$/d' installer/linux/postinst)

%preun
$PRERM

%files
/usr/bin/bsnfs
/usr/lib/systemd/system/bsnfs.service
/usr/lib/sysusers.d/bsnfs.conf
%dir /etc/bsnfs
%config(noreplace) /etc/bsnfs/bsnfs.conf
%doc /usr/share/doc/bsnfs
SPEC
    rpmbuild --quiet --define "_topdir $S/top" --define "_build_id_links none" \
        --define "_binaries_in_noarch_packages_terminate_build 0" --target "$PKG_ARCH-linux" -bb "$S/bsnfs.spec"
    mv "$S"/top/RPMS/*/*.rpm "$OUT_DIR/$BASE.rpm"
    rm -rf "$S"
    rpm -qp --qf '%{NAME} %{VERSION} %{ARCH}\n' "$OUT_DIR/$BASE.rpm"
    group_end
fi

if has_format pkg; then
    group_start "Arch pkg"
    need zstd
    S=$(stage_dir)
    stage_tree "$S"
    # pacman runs systemd-sysusers itself for files in /usr/lib/sysusers.d.
    cat > "$S/.INSTALL" <<'INSTALL'
post_install() {
    chgrp bsnfs /etc/bsnfs/bsnfs.conf 2>/dev/null || true
    echo "bsnfs: add 'share NAME=/path' to /etc/bsnfs/bsnfs.conf, then: systemctl enable --now bsnfs"
}
pre_remove() {
    systemctl disable --now bsnfs >/dev/null 2>&1 || true
}
INSTALL
    # pacman's pkgver may not contain '-' or ':'; ours never do.
    cat > "$S/.PKGINFO" <<PKGINFO
pkgname = bsnfs
pkgbase = bsnfs
pkgver = ${VERSION}-1
pkgdesc = $SUMMARY
builddate = $(date -u +%s)
packager = bsnfs release builds
size = $(du -sb "$S/usr" | cut -f1)
arch = ${PKG_ARCH}
license = MIT
license = BSD-3-Clause
backup = etc/bsnfs/bsnfs.conf
PKGINFO
    (cd "$S" && tar --zstd -cf "$OUT_DIR/$BASE.pkg.tar.zst" .PKGINFO .INSTALL usr etc)
    rm -rf "$S"
    group_end
fi

if has_format appimage; then
    group_start "AppImage"
    need curl
    case "$PKG_ARCH" in
        x86_64|aarch64) ;;
        *) die "appimagetool has no $PKG_ARCH build; leave appimage out of --formats" ;;
    esac
    S=$(stage_dir)
    A="$S/bsnfs.AppDir"
    install -Dm755 "$BIN" "$A/usr/bin/bsnfs"
    for f in "${DOC_FILES[@]}"; do install -Dm644 "$f" "$A/usr/share/doc/bsnfs/$f"; done
    install -Dm644 installer/linux/bsnfs.png "$A/bsnfs.png"
    install -Dm644 installer/linux/bsnfs.png "$A/usr/share/icons/hicolor/256x256/apps/bsnfs.png"
    cat > "$A/bsnfs.desktop" <<'DESKTOP'
[Desktop Entry]
Type=Application
Name=bsnfs
Comment=Share a folder with old and new NFS clients
Exec=bsnfs
Icon=bsnfs
Terminal=true
Categories=Network;FileTransfer;
DESKTOP
    printf '#!/bin/sh\nexec "$(dirname "$(readlink -f "$0")")/usr/bin/bsnfs" "$@"\n' > "$A/AppRun"
    chmod 755 "$A/AppRun"
    # appimagetool for the host; it fetches the runtime for $ARCH itself.
    TOOL="$S/appimagetool"
    curl -fsSL --retry 5 -o "$TOOL" \
        "https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-$(uname -m).AppImage"
    chmod +x "$TOOL"
    APPIMAGE="$OUT_DIR/$BASE.AppImage"
    ARCH="$PKG_ARCH" APPIMAGE_EXTRACT_AND_RUN=1 "$TOOL" --no-appstream "$A" "$APPIMAGE"
    rm -rf "$S"
    APPIMAGE_EXTRACT_AND_RUN=1 ${RUN[@]+"${RUN[@]}"} "$APPIMAGE" --version
    group_end
fi

log "done:"; ls -lh "$OUT_DIR" >&2
