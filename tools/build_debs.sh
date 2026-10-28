#!/usr/bin/env bash
# Fetch the OS packages DYODE may need on a box that has no internet, into
# packaging/debs/<os>-<release>/<arch>/.  Run on a machine WITH internet
# access and Docker (or Podman).  `install.sh --offline` installs from them.
#
#   tools/build_debs.sh
#   TARGETS="ubuntu-24.04/amd64 debian-13/arm64" tools/build_debs.sh
#
# DYODE itself is one self-contained binary and needs no OS packages beyond:
#   nftables   the output box's diode-port firewall (install.sh)
#   iproute2   'ip neigh', for the input box's static ARP entry
#   openssl    the local API's TLS certificate (dyode api-cert)
# plus the optional services for serving received files that
# `install.sh --with ssh,nfs,smb` installs.
#
# Targets (folder/CPU):
#   ubuntu-24.04/amd64  ubuntu-24.04/arm64      Ubuntu 24.04 LTS
#   ubuntu-26.04/amd64  ubuntu-26.04/arm64      Ubuntu 26.04 LTS
#   debian-12/amd64     debian-12/arm64         Debian 12, Raspberry Pi OS bookworm (64-bit)
#   debian-13/amd64     debian-13/arm64         Debian 13, Raspberry Pi OS trixie (64-bit)
#
# 64-bit Raspberry Pi OS takes these packages unchanged from Debian's arm64
# archive and reports itself as Debian in /etc/os-release, so the debian-*
# folders serve it.  32-bit Raspberry Pi OS (armhf) is not covered.
#
# Each target is fetched inside a clean container of that release with
# apt's --download-only, so dependencies are resolved by apt itself rather
# than guessed.  That container is smaller than a real install, so the set
# is a superset of what a given host needs; install.sh lets apt pick only
# the missing ones and never downgrades anything.
#
# arm64 on an x86 machine needs QEMU emulation:
#   docker run --privileged --rm tonistiigi/binfmt --install arm64
# or run on an arm64 machine (the release workflow does).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_BASE="${OUT_BASE:-$ROOT/packaging/debs}"
TARGETS="${TARGETS:-ubuntu-24.04/amd64 ubuntu-24.04/arm64 ubuntu-26.04/amd64 ubuntu-26.04/arm64 debian-12/amd64 debian-12/arm64 debian-13/amd64 debian-13/arm64}"
# Keep in step with core_pkgs()/service_pkgs() in install.sh.
CORE_PKGS="nftables iproute2 openssl"
SERVICE_PKGS="openssh-server openssh-sftp-server openssh-client nfs-common nfs-kernel-server samba cifs-utils smbclient"
PKGS="$CORE_PKGS $SERVICE_PKGS"

case "${1:-}" in
  -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'; exit 0 ;;
  "") ;;
  *) echo "unknown option: $1 (try --help)" >&2; exit 2 ;;
esac

ENGINE="${ENGINE:-$(command -v docker || command -v podman || true)}"
[ -n "$ENGINE" ] || { echo "needs docker or podman" >&2; exit 1; }

fail=0
for target in $TARGETS; do
  dir="${target%/*}" arch="${target#*/}"
  image="${dir%%-*}:${dir#*-}"          # ubuntu-24.04 -> ubuntu:24.04
  out="$OUT_BASE/$dir/$arch"
  echo "== $dir $arch ($image): $PKGS"
  rm -rf "$out"; mkdir -p "$out"
  if ! "$ENGINE" run --rm --platform "linux/$arch" \
        -v "$out:/out" -e PKGS="$PKGS" \
        -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" \
        "$image" bash -c '
      set -e
      export DEBIAN_FRONTEND=noninteractive
      mkdir -p /out/partial          # apt refuses to download without it
      apt-get update -qq
      apt-get install -y -qq --no-install-recommends --download-only \
        -o Dir::Cache::archives=/out $PKGS
      rm -rf /out/partial /out/lock
      cd /out
      sha256sum -- *.deb > SHA256SUMS
      chown -R "$HOST_UID:$HOST_GID" /out
    '; then
    echo "   FAILED (for arm64 on x86, see the QEMU note in --help)" >&2
    fail=1
    continue
  fi
  {
    echo "$dir $arch ($image)"
    echo "built:     $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "requested: $PKGS"
    echo
    (cd "$out" && ls -1 -- *.deb)
  } > "$out/MANIFEST.txt"
  echo "   $(ls "$out"/*.deb | wc -l) packages, $(du -sh "$out" | cut -f1)"
done
[ "$fail" -eq 0 ] || { echo "some targets failed" >&2; exit 1; }
