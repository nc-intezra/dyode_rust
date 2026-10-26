#!/usr/bin/env bash
# Build the release binary for this machine's CPU, portable across every
# supported system: compiled inside Debian 12 (rust:1-bookworm), so it needs
# glibc 2.36 or newer -- Debian 12 / Raspberry Pi OS bookworm is the oldest
# supported system, and Ubuntu 24.04/26.04 and Debian 13 are newer -- and
# nothing beyond libc and libgcc_s.  Built with mimalloc.
#
#   tools/build_binary.sh             needs Docker or Podman
#   OFFLINE=1 tools/build_binary.sh   from a vendored source tree, no network
#
# Why not a static musl binary: lidi 3.0.0 does not build for musl targets
# (lidi-receive passes libc::MSG_WAITFORONE, an i32, where musl's recvmmsg
# takes a u32).  Dropping lidi's recvmmsg mode would cost receive throughput,
# so the binary links glibc dynamically instead, against the oldest one.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IMAGE="${IMAGE:-rust:1-bookworm}"
MAX_GLIBC="2.36"
ENGINE="${ENGINE:-$(command -v docker || command -v podman || true)}"
[ -n "$ENGINE" ] || { echo "needs docker or podman" >&2; exit 1; }
args="--release --locked --features mimalloc"
[ "${OFFLINE:-0}" = 1 ] && args="$args --offline"
net=(); [ "${OFFLINE:-0}" = 1 ] && net=(--network none)
"$ENGINE" run --rm "${net[@]}" -v "$ROOT:/src" -w /src \
  -e CARGO_TARGET_DIR=/src/target/portable -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" \
  "$IMAGE" bash -euc "cargo build $args; chown -R \$HOST_UID:\$HOST_GID /src/target/portable"
BIN="$ROOT/target/portable/release/dyode"
# Refuse a binary that would not run on the oldest supported system.
need="$(objdump -T "$BIN" | grep -o 'GLIBC_[0-9.]*' | sed 's/GLIBC_//' | sort -V | tail -1)"
if [ "$(printf '%s\n%s\n' "$need" "$MAX_GLIBC" | sort -V | tail -1)" != "$MAX_GLIBC" ]; then
  echo "$BIN needs glibc $need, newer than $MAX_GLIBC" >&2; exit 1
fi
echo "needs glibc >= $need: $("$BIN" version)"
echo "$BIN"
