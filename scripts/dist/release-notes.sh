#!/usr/bin/env bash
# Write the GitHub release body (with the downloads table) for a version, or
# check that a directory holds every file that table links to.
#
#   scripts/dist/release-notes.sh --version V --tag TAG [--channel nightly|release]
#                                 [--repo owner/name] [--commit SHA] [-o FILE]
#   scripts/dist/release-notes.sh --version V --check DIR
#
# The asset list below is the single source of truth for what a release ships;
# build.yml checks the built files against it before anything is published.
source "$(dirname "$0")/lib.sh"

CHANNEL=release
TAG=""
REPO="${GITHUB_REPOSITORY:-danifunker/bsnfs}"
COMMIT=""
OUT=""
CHECK=""
while [ $# -gt 0 ]; do
    case "$1" in
        --version) VERSION="$2"; shift 2 ;;
        --tag)     TAG="$2"; shift 2 ;;
        --channel) CHANNEL="$2"; shift 2 ;;
        --repo)    REPO="$2"; shift 2 ;;
        --commit)  COMMIT="$2"; shift 2 ;;
        -o)        OUT="$2"; shift 2 ;;
        --check)   CHECK="$2"; shift 2 ;;
        -h|--help) sed -n '2,10p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) die "unknown option: $1" ;;
    esac
done
[ -n "$VERSION" ] || die "--version is required"
V="$VERSION"

assets() {
    local a
    for a in x64 arm64; do
        echo "bsnfs-setup-windows-${a}-${V}.exe"
        echo "bsnfs-windows-${a}-${V}.zip"
        echo "bsnfs-linux-${a}-${V}.tar.gz"
        echo "bsnfs-linux-${a}-${V}.deb"
        echo "bsnfs-linux-${a}-${V}.rpm"
        echo "bsnfs-linux-${a}-${V}.pkg.tar.zst"
    done
    echo "bsnfs-macos-universal-${V}.tar.gz"
}

if [ -n "$CHECK" ]; then
    missing=0
    while read -r f; do
        if [ ! -s "$CHECK/$f" ]; then
            echo "missing: $f" >&2
            missing=$((missing + 1))
        fi
    done < <(assets)
    [ "$missing" = 0 ] || die "$missing expected release file(s) missing from $CHECK"
    log "all $(assets | wc -l | tr -d ' ') release files present"
    exit 0
fi

[ -n "$TAG" ] || die "--tag is required"
D="https://github.com/${REPO}/releases/download/${TAG}"
dl() { echo "[$1](${D}/$2)"; }

{
if [ "$CHANNEL" = nightly ]; then
    echo "**Version \`${V}\`**${COMMIT:+, built from [\`${COMMIT:0:8}\`](https://github.com/${REPO}/commit/${COMMIT})}"
else
    echo "**Version \`${V}\`**"
fi
cat <<EOF

## Downloads

| Platform | Install | Just the program |
|---|---|---|
| Windows x64 | $(dl Installer "bsnfs-setup-windows-x64-${V}.exe") | $(dl zip "bsnfs-windows-x64-${V}.zip") |
| Windows arm64 | $(dl Installer "bsnfs-setup-windows-arm64-${V}.exe") | $(dl zip "bsnfs-windows-arm64-${V}.zip") |
| macOS, Apple silicon and Intel | | $(dl tar.gz "bsnfs-macos-universal-${V}.tar.gz") |
| Linux x64 | $(dl deb "bsnfs-linux-x64-${V}.deb") · $(dl rpm "bsnfs-linux-x64-${V}.rpm") · $(dl "Arch pkg" "bsnfs-linux-x64-${V}.pkg.tar.zst") | $(dl tar.gz "bsnfs-linux-x64-${V}.tar.gz") |
| Linux arm64 | $(dl deb "bsnfs-linux-arm64-${V}.deb") · $(dl rpm "bsnfs-linux-arm64-${V}.rpm") · $(dl "Arch pkg" "bsnfs-linux-arm64-${V}.pkg.tar.zst") | $(dl tar.gz "bsnfs-linux-arm64-${V}.tar.gz") |

- **Windows:** the installer can set bsnfs up as a service and opens its admin page.
- **Linux:** the packages install a systemd service (\`bsnfs\`, not started until you add a folder to \`/etc/bsnfs/bsnfs.conf\`). The Linux binaries are static and run on any distribution.
- **macOS:** unpack, then run \`./bsnfs ~/folder\`, or \`sudo ./bsnfs service install ~/folder\` to run it at boot.
EOF
} > "${OUT:-/dev/stdout}"
