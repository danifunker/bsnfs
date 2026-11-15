#!/usr/bin/env bash
# Build bsnfs for macOS as universal (arm64 + x86_64) binaries and package:
#   - bsnfs.app, a menu-bar app around the server, in a .dmg (macOS 11+)
#   - the bare command-line program in a tarball: macOS 10.13+ on Intel
#     (utimensat, which bsnfs uses to set file times, arrived in 10.13) and
#     11+ on Apple silicon (the first release for it)
#
#   scripts/dist/package-macos.sh [options]
#
# Everything is signed with the hardened runtime and notarized; the app and
# the .dmg get their tickets stapled. Signing and notarization run when their
# credentials are present and are skipped (ad-hoc signature) when not:
#
#   MACOS_SIGN_IDENTITY   a "Developer ID Application: ..." identity in your
#                         keychain. Without it the binary is signed ad hoc.
#   Notarization, either an App Store Connect API key
#     NOTARY_KEY_ID, NOTARY_ISSUER_ID, NOTARY_KEY_PATH (the .p8 file)
#   or an Apple ID
#     NOTARY_APPLE_ID, NOTARY_PASSWORD (app-specific), NOTARY_TEAM_ID
#
# Set REQUIRE_NOTARIZATION=1 to fail instead of shipping an unnotarized build.
source "$(dirname "$0")/lib.sh"

parse_common_args "$@"
set -- ${REST[@]+"${REST[@]}"}
case "${1:-}" in
    -h|--help) sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'; common_usage; exit 0 ;;
    "") ;;
    *) die "unknown option: $1" ;;
esac
[ "$(uname -s)" = Darwin ] || die "package-macos.sh runs on macOS only"
need lipo codesign ditto

IDENTITY="${MACOS_SIGN_IDENTITY:--}"

NOTARY=()
if [ -n "${NOTARY_KEY_ID:-}" ] && [ -n "${NOTARY_ISSUER_ID:-}" ] && [ -n "${NOTARY_KEY_PATH:-}" ]; then
    NOTARY=(--key "$NOTARY_KEY_PATH" --key-id "$NOTARY_KEY_ID" --issuer "$NOTARY_ISSUER_ID")
elif [ -n "${NOTARY_APPLE_ID:-}" ] && [ -n "${NOTARY_PASSWORD:-}" ] && [ -n "${NOTARY_TEAM_ID:-}" ]; then
    NOTARY=(--apple-id "$NOTARY_APPLE_ID" --password "$NOTARY_PASSWORD" --team-id "$NOTARY_TEAM_ID")
fi
if [ "$IDENTITY" = "-" ]; then
    NOTARY=()   # Apple refuses to notarize ad-hoc signatures
    msg="no Developer ID identity: signing ad hoc, not notarizing"
    [ "${REQUIRE_NOTARIZATION:-0}" = 1 ] && die "$msg"
    warn "$msg"
elif [ "${#NOTARY[@]}" = 0 ]; then
    msg="signing as '$IDENTITY' but no notarization credentials are set: not notarizing"
    [ "${REQUIRE_NOTARIZATION:-0}" = 1 ] && die "$msg"
    warn "$msg"
fi

# Submit and wait. notarytool exits 0 even for a rejected submission, so read
# the status and print Apple's log when it isn't Accepted.
notarize() {  # <zip>
    [ "${#NOTARY[@]}" -gt 0 ] || return 0
    local out id status
    group_start "notarize $(basename "$1")"
    out=$(xcrun notarytool submit "$1" "${NOTARY[@]}" --wait --output-format json)
    echo "$out"
    id=$(printf '%s' "$out" | plutil -extract id raw -o - - 2>/dev/null || true)
    status=$(printf '%s' "$out" | plutil -extract status raw -o - - 2>/dev/null || true)
    if [ "$status" != "Accepted" ]; then
        [ -n "$id" ] && xcrun notarytool log "$id" "${NOTARY[@]}" || true
        die "notarization of $(basename "$1") finished as '${status:-unknown}'"
    fi
    group_end
}

MIN_X86_64=10.13
MIN_ARM64=11.0
MACOSX_DEPLOYMENT_TARGET=$MIN_ARM64 build aarch64-apple-darwin
MACOSX_DEPLOYMENT_TARGET=$MIN_X86_64 build x86_64-apple-darwin

WORK=$(stage_dir)
trap 'rm -rf "$WORK"' EXIT
lipo -create -output "$WORK/bsnfs" \
    "$(release_dir aarch64-apple-darwin)/bsnfs" "$(release_dir x86_64-apple-darwin)/bsnfs"
strip -x "$WORK/bsnfs"

# Check each slice really carries the minimum OS we built for.
minos() {  # <arch>
    otool -arch "$1" -l "$WORK/bsnfs" | awk '/LC_BUILD_VERSION/ {b=1} b && /minos/ {print $2; exit} /LC_VERSION_MIN_MACOSX/ {v=1} v && /version/ {print $2; exit}'
}
[ "$(minos x86_64)" = "$MIN_X86_64" ] || die "x86_64 slice targets macOS $(minos x86_64), not $MIN_X86_64"
[ "$(minos arm64)" = "$MIN_ARM64" ] || die "arm64 slice targets macOS $(minos arm64), not $MIN_ARM64"
log "minimum macOS: $(minos x86_64) (Intel), $(minos arm64) (Apple silicon)"

# ── bsnfs.app ────────────────────────────────────────────────────────────────
# A menu-bar app (macos/main.swift) that runs the same server, bundled inside
# as Contents/MacOS/bsnfs-cli. Needs macOS 11 (SF Symbols for its menu icon).
APP_MIN=11.0
need xcrun hdiutil
group_start "bsnfs.app"
for arch in arm64 x86_64; do
    xcrun --sdk macosx swiftc -O -swift-version 5 -target "$arch-apple-macos$APP_MIN" \
        -o "$WORK/menu-$arch" macos/main.swift
done
lipo -create -output "$WORK/menu" "$WORK/menu-arm64" "$WORK/menu-x86_64"
APP="$WORK/app/bsnfs.app"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$WORK/menu" "$APP/Contents/MacOS/bsnfs"
cp macos/Info.plist "$APP/Contents/Info.plist"
cp macos/AppIcon.icns LICENSE LICENSE-iris.txt "$APP/Contents/Resources/"
# Bundle versions are at most three numbers: YYYY.MM.DD.HHMMSS becomes
# 2026.10.28 (shown) and 20261028.192448 (build); X.Y.Z stays X.Y.Z.
if [[ "$VERSION" =~ ^([0-9]+)\.([0-9]+)\.([0-9]+)(\.([0-9]+))?$ ]]; then
    SHORT="$((10#${BASH_REMATCH[1]})).$((10#${BASH_REMATCH[2]})).$((10#${BASH_REMATCH[3]}))"
    if [ -n "${BASH_REMATCH[5]}" ]; then
        BUILD="${BASH_REMATCH[1]}${BASH_REMATCH[2]}${BASH_REMATCH[3]}.$((10#${BASH_REMATCH[5]}))"
    else
        BUILD="$SHORT"
    fi
else
    SHORT=0.0.0; BUILD=0
fi
plist() { /usr/libexec/PlistBuddy -c "$1" "$APP/Contents/Info.plist"; }
plist "Set :CFBundleShortVersionString $SHORT"
plist "Set :CFBundleVersion $BUILD"
plist "Add :BSNFSVersion string $VERSION"
group_end

# ── sign, inside out ────────────────────────────────────────────────────────
sign() {  # <path> [identifier]
    if [ "$IDENTITY" = "-" ]; then
        codesign --force --sign - ${2:+--identifier "$2"} "$1"
    else
        codesign --force --options runtime --timestamp ${2:+--identifier "$2"} --sign "$IDENTITY" "$1"
    fi
}
group_start "sign"
sign "$WORK/bsnfs" io.github.danifunker.bsnfs.cli
cp "$WORK/bsnfs" "$APP/Contents/MacOS/bsnfs-cli"
sign "$APP"   # identifier from Info.plist: io.github.danifunker.bsnfs
codesign --verify --strict --verbose "$WORK/bsnfs"
codesign --verify --deep --strict --verbose "$APP"
group_end
"$WORK/bsnfs" --version
"$APP/Contents/MacOS/bsnfs-cli" --version
lipo -archs "$APP/Contents/MacOS/bsnfs" | sed 's/^/app architectures: /'

# Stapling fetches the ticket from Apple's CDN, which can drop the connection
# or not have it yet right after acceptance: try a few times. Unstapled, a
# notarized file still opens; Gatekeeper looks the ticket up online.
staple() {  # <path>
    [ "${#NOTARY[@]}" -gt 0 ] || return 0
    local attempt
    for attempt in 1 2 3 4 5 6; do
        xcrun stapler staple "$1" && return 0
        log "staple attempt $attempt for $(basename "$1") failed; retrying in 30 s"
        sleep 30
    done
    warn "$(basename "$1") is notarized but not stapled: Apple's ticket service didn't answer"
}

# One submission covers the app and the bare command-line binary. The app
# gets the ticket stapled (so it opens offline); a bare executable can't hold
# one, and Gatekeeper finds its ticket online.
mkdir "$WORK/notarize"
cp -R "$APP" "$WORK/bsnfs" "$WORK/notarize/"
ditto -c -k "$WORK/notarize" "$WORK/notarize.zip"
notarize "$WORK/notarize.zip"
staple "$APP"

# ── outputs ──────────────────────────────────────────────────────────────────
group_start "dmg"
DMG="$OUT_DIR/bsnfs-macos-universal-${VERSION}.dmg"
mkdir "$WORK/dmg"
cp -R "$APP" "$WORK/dmg/"
ln -s /Applications "$WORK/dmg/Applications"
rm -f "$DMG"
hdiutil create -volname "bsnfs" -srcfolder "$WORK/dmg" -fs HFS+ -format UDZO -ov "$DMG"
[ "$IDENTITY" = "-" ] || codesign --force --timestamp --sign "$IDENTITY" "$DMG"
group_end
notarize "$DMG"
staple "$DMG"

OUT="$OUT_DIR/bsnfs-macos-universal-${VERSION}.tar.gz"
cp "${DOC_FILES[@]}" "$WORK/"
tar -C "$WORK" -czf "$OUT" bsnfs "${DOC_FILES[@]}"
verify_members "$OUT" bsnfs "${DOC_FILES[@]}"

log "done:"; ls -lh "$OUT_DIR" >&2
