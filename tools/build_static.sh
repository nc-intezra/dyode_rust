#!/usr/bin/env bash
# Build the static release binary for this machine's CPU:
# target/<arch>-unknown-linux-musl/release/dyode, linked statically against
# musl with mimalloc, so the one file runs on every supported distribution
# whatever its glibc.
#
#   tools/build_static.sh            needs: rustup, musl-tools (musl-gcc)
#   OFFLINE=1 tools/build_static.sh  from a vendored source tree
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
case "$(uname -m)" in
  x86_64) T=x86_64-unknown-linux-musl ;;
  aarch64|arm64) T=aarch64-unknown-linux-musl ;;
  *) echo "unsupported CPU $(uname -m)" >&2; exit 1 ;;
esac
command -v musl-gcc >/dev/null || { echo "needs musl-gcc (apt install musl-tools)" >&2; exit 1; }
rustup target list --installed 2>/dev/null | grep -qx "$T" || rustup target add "$T"
args=(--release --target "$T" --features mimalloc --manifest-path "$ROOT/Cargo.toml")
[ "${OFFLINE:-0}" = 1 ] && args+=(--offline --locked)
# mimalloc's C code is compiled with musl-gcc for this target.
CC_x86_64_unknown_linux_musl=musl-gcc CC_aarch64_unknown_linux_musl=musl-gcc \
  cargo build "${args[@]}"
BIN="$ROOT/target/$T/release/dyode"
if ldd "$BIN" 2>&1 | grep -q "=>"; then
  echo "$BIN is dynamically linked" >&2; exit 1
fi
"$BIN" version
echo "$BIN"
