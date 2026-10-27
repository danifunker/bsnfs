#!/usr/bin/env bash
# Build bsnfs for macOS as one universal (arm64 + x86_64) binary and package
# it as a tarball. It runs on macOS 10.13+ on Intel (utimensat, which bsnfs
# uses to set file times, arrived in 10.13) and macOS 11+ on Apple silicon
# (the first release for it).
#
#   scripts/dist/package-macos.sh [options]
#
# bsnfs is a command-line program, so there is no .app: the binary itself is
# signed with the hardened runtime and notarized, which is what Gatekeeper
# checks on a downloaded executable. Signing and notarization run when their
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

group_start "sign"
if [ "$IDENTITY" = "-" ]; then
    codesign --force --sign - "$WORK/bsnfs"
else
    codesign --force --options runtime --timestamp --identifier io.github.danifunker.bsnfs --sign "$IDENTITY" "$WORK/bsnfs"
fi
codesign --verify --verbose "$WORK/bsnfs"
group_end
"$WORK/bsnfs" --version

# A bare executable can't carry a stapled ticket; notarizing still registers
# it, so Gatekeeper's online check passes on first run.
ditto -c -k "$WORK/bsnfs" "$WORK/notarize.zip"
notarize "$WORK/notarize.zip"

OUT="$OUT_DIR/bsnfs-macos-universal-${VERSION}.tar.gz"
cp "${DOC_FILES[@]}" "$WORK/"
tar -C "$WORK" -czf "$OUT" bsnfs "${DOC_FILES[@]}"
verify_members "$OUT" bsnfs "${DOC_FILES[@]}"

log "done:"; ls -lh "$OUT_DIR" >&2
