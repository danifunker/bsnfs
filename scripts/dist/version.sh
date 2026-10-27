#!/usr/bin/env bash
# Print the version a distribution build is stamped with.
#
#   version.sh timestamp [REF]        YYYY.MM.DD.HHMMSS, the UTC commit time of
#                                     REF (default HEAD): what every push to
#                                     main is released as
#   version.sh nightly [YYYY-MM-DD]   nightly.YYYY.MM.DD (UTC today by default)
#   version.sh release vX.Y.Z         X.Y.Z  (accepts the tag or the bare version)
#   version.sh dev                    dev.<short commit>
#
# The same string goes into `bsnfs --version`, the admin page, the installers
# and every file name. deb/rpm get a leading "0~" when the version does not
# start with a digit (see pkg_version in lib.sh).
set -euo pipefail
cd "$(dirname "$0")/../.."

usage() { sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//'; exit "${1:-0}"; }

channel="${1:-}"
[ -n "$channel" ] || usage 2
shift

case "$channel" in
    timestamp)
        # The commit's time, not the build's: re-running a build reproduces
        # the same version. Digits and dots only, so deb, rpm, pacman and
        # Inno Setup all accept it as is.
        TZ=UTC git show -s --format=%cd --date=format-local:%Y.%m.%d.%H%M%S "${1:-HEAD}"
        ;;
    nightly)
        if [ $# -gt 0 ]; then
            d="$1"
            [[ "$d" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}$ ]] || { echo "bad date '$d' (want YYYY-MM-DD)" >&2; exit 2; }
            echo "nightly.${d//-/.}"
        else
            date -u +nightly.%Y.%m.%d
        fi
        ;;
    release)
        [ $# -gt 0 ] || { echo "release needs a tag (vX.Y.Z)" >&2; exit 2; }
        v="${1#refs/tags/}"; v="${v#v}"
        [[ "$v" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "'$1' is not a vX.Y.Z release tag" >&2; exit 2; }
        echo "$v"
        ;;
    dev)
        sha=$(git rev-parse --short=8 HEAD 2>/dev/null || echo unknown)
        echo "dev.${sha}"
        ;;
    -h|--help) usage ;;
    *) usage 2 ;;
esac
