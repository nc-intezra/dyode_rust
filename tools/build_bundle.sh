#!/usr/bin/env bash
# Assemble the offline install bundle: everything an air-gapped DYODE box
# needs, in one folder and one tarball.
#
#   tools/build_bundle.sh --bin amd64=PATH --bin arm64=PATH [--source TARBALL] [--out DIR]
#
# Layout of dist/dyode-<version>/ (and dist/dyode-<version>-offline.tar.gz):
#
#   install.sh, config.example.yaml, README.md, CAVEATS.md, ...   as in the repo
#   bin/<arch>/dyode                 binaries (tools/build_binary.sh)
#   packaging/debs/<os>-<ver>/<arch>/  OS packages (tools/build_debs.sh), each
#                                    folder with its own SHA256SUMS
#   source/dyode-<version>-src.tar.gz  the source with every dependency
#                                    vendored and Cargo.lock, to rebuild
#                                    without a network (cargo build --offline)
#   BUNDLE.txt                       what this is, built from which commit
#   SHA256SUMS                       every other file in the bundle
#
# install.sh --offline checks SHA256SUMS -- and that no file is unlisted --
# before it changes anything.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="$ROOT/dist"
SOURCE=""
declare -A BINS=()

while [ $# -gt 0 ]; do
  case "$1" in
    --bin) BINS["${2%%=*}"]="${2#*=}"; shift 2 ;;
    --source) SOURCE="$2"; shift 2 ;;
    --out) OUT="$2"; shift 2 ;;
    -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $1 (try --help)" >&2; exit 2 ;;
  esac
done
[ "${#BINS[@]}" -gt 0 ] || { echo "give at least one --bin ARCH=PATH" >&2; exit 2; }

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)"
NAME="dyode-$VERSION"
DEST="$OUT/$NAME"
rm -rf "$DEST"; mkdir -p "$DEST"

# The repository files an install uses.
for f in install.sh config.example.yaml README.md CAVEATS.md CHANGELOG.md LICENSE; do
  cp -p "$ROOT/$f" "$DEST/"
done
cp -rp "$ROOT/docs" "$DEST/"
mkdir -p "$DEST/packaging" "$DEST/tools"
cp -rp "$ROOT/packaging/logrotate" "$ROOT/packaging/systemd" "$DEST/packaging/"
cp -p "$ROOT/tools/"*.sh "$DEST/tools/"

for arch in "${!BINS[@]}"; do
  case "$arch" in amd64|arm64) ;; *) echo "unknown arch $arch (amd64, arm64)" >&2; exit 2 ;; esac
  src="${BINS[$arch]}"
  [ -x "$src" ] || { echo "$src is not an executable" >&2; exit 1; }
  install -D -m 0755 "$src" "$DEST/bin/$arch/dyode"
done

ndeb=0
if [ -d "$ROOT/packaging/debs" ]; then
  for d in "$ROOT"/packaging/debs/*/*/; do
    [ -f "$d/SHA256SUMS" ] || { echo "no SHA256SUMS in $d" >&2; exit 1; }
    (cd "$d" && sha256sum --quiet -c SHA256SUMS) || { echo "bad checksums in $d" >&2; exit 1; }
    rel="${d#"$ROOT"/}"
    mkdir -p "$DEST/$rel"
    cp -p "$d"/*.deb "$d/SHA256SUMS" "$d/MANIFEST.txt" "$DEST/$rel" 2>/dev/null || cp -p "$d"/*.deb "$d/SHA256SUMS" "$DEST/$rel"
    ndeb=$((ndeb + $(ls "$d"/*.deb | wc -l)))
  done
fi

if [ -n "$SOURCE" ]; then
  install -D -m 0644 "$SOURCE" "$DEST/source/$(basename "$SOURCE")"
fi

commit="$(git -C "$ROOT" rev-parse HEAD 2>/dev/null || echo unknown)"
{
  echo "DYODE $VERSION offline install bundle"
  echo "built:   $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "commit:  $commit"
  echo
  echo "binaries:"
  for arch in $(printf '%s\n' "${!BINS[@]}" | sort); do
    echo "  bin/$arch/dyode  $("$DEST/bin/$arch/dyode" version 2>/dev/null || echo '(other CPU: not run here)')"
  done
  echo "OS packages: $ndeb .deb file(s) for:"
  for d in "$DEST"/packaging/debs/*/*/; do [ -d "$d" ] && d="${d%/}" && echo "  ${d#"$DEST"/packaging/debs/}"; done
  [ -n "$SOURCE" ] && echo "source:  source/$(basename "$SOURCE") (vendored; cargo build --release --offline --locked)"
  echo
  echo "Install:  sudo ./install.sh --offline --side in|out"
} > "$DEST/BUNDLE.txt"

# Every file, relative paths, sorted; SHA256SUMS itself excluded.
(cd "$DEST" && find . -type f ! -path ./SHA256SUMS | sed 's#^\./##' | sort |
  xargs -d '\n' sha256sum > SHA256SUMS)

tar -C "$OUT" -czf "$OUT/$NAME-offline.tar.gz" "$NAME"
(cd "$OUT" && sha256sum "$NAME-offline.tar.gz" > "$NAME-offline.tar.gz.sha256")
echo "bundle: $OUT/$NAME-offline.tar.gz ($(du -h "$OUT/$NAME-offline.tar.gz" | cut -f1), $ndeb .deb, ${#BINS[@]} binaries)"
