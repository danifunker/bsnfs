#!/usr/bin/env bash
# Build and package bsnfs for Windows: an installer (Inno Setup) that can
# register the Windows service, and a plain zip.
#
#   scripts/dist/package-windows.sh [options]
#
# Run it from Git Bash (what GitHub's Windows runners use for `shell: bash`).
# Needs 7-Zip (7z) and Inno Setup 6; set ISCC to the compiler's path if it is
# not in the default location.
source "$(dirname "$0")/lib.sh"

parse_common_args "$@"
set -- ${REST[@]+"${REST[@]}"}
case "${1:-}" in
    -h|--help) sed -n '2,9p' "$0" | sed 's/^# \{0,1\}//'; common_usage; exit 0 ;;
    "") ;;
    *) die "unknown option: $1" ;;
esac

case "$TARGET" in
    i686-pc-windows-*)    ARCH=x86 ;;
    x86_64-pc-windows-*)  ARCH=x64 ;;
    aarch64-pc-windows-*) ARCH=arm64 ;;
    *) die "unsupported Windows target $TARGET" ;;
esac

need 7z cygpath
ISCC="${ISCC:-/c/Program Files (x86)/Inno Setup 6/ISCC.exe}"
[ -f "$ISCC" ] || command -v "$ISCC" >/dev/null 2>&1 \
    || die "Inno Setup compiler not found at '$ISCC' (choco install innosetup, or set ISCC)"

build "$TARGET"
REL=$(release_dir "$TARGET")
[ -f "$REL/bsnfs.exe" ] || die "$REL/bsnfs.exe was not built"
"$REL/bsnfs.exe" --version

winpath() { cygpath -w "$1"; }

# Windows file versions are four numbers of at most 65535: YYYY.MM.DD.HHMMSS
# becomes YYYY.MM.DD.HHMM, X.Y.Z becomes X.Y.Z.0, anything else 0.0.0.0.
if [[ "$VERSION" =~ ^([0-9]+)\.([0-9]+)\.([0-9]+)(\.([0-9]{1,4})[0-9]*)?$ ]]; then
    FILE_VERSION="$((10#${BASH_REMATCH[1]})).$((10#${BASH_REMATCH[2]})).$((10#${BASH_REMATCH[3]})).$((10#${BASH_REMATCH[5]:-0}))"
else
    FILE_VERSION=0.0.0.0
fi

group_start "installer"
# Git Bash rewrites arguments that look like POSIX paths, and ISCC's /D, /O
# and /F switches do; without this it sees them as extra script filenames.
MSYS2_ARG_CONV_EXCL='*' "$ISCC" "/DMyAppVersion=$VERSION" "/DFileVersion=$FILE_VERSION" "/DTargetArch=$ARCH" \
    "/DSourceDir=$(winpath "$REL")" \
    "/DDocDir=$(winpath "$DIST_ROOT")" \
    "/O$(winpath "$OUT_DIR")" \
    "/Fbsnfs-setup-windows-${ARCH}-${VERSION}" \
    "$(winpath "$DIST_ROOT/installer/windows/bsnfs.iss")"
[ -f "$OUT_DIR/bsnfs-setup-windows-${ARCH}-${VERSION}.exe" ] || die "the installer was not produced"
group_end

group_start "zip"
ZIP="$OUT_DIR/bsnfs-windows-${ARCH}-${VERSION}.zip"
S=$(stage_dir)
cp "$REL/bsnfs.exe" "${DOC_FILES[@]}" "$S/"
(cd "$S" && 7z a -bso0 "$(winpath "$ZIP")" ./*)
rm -rf "$S"
verify_members "$ZIP" bsnfs.exe "${DOC_FILES[@]}"
group_end

log "done:"; ls -lh "$OUT_DIR" >&2
