#!/usr/bin/env bash
# DYODE installer (Rust + lidi).
#
#   sudo ./install.sh --side in            input (sending) box
#   sudo ./install.sh --side out           output (receiving) box
#   sudo ./install.sh --side out --variant v2   DYODE v2 (serial link)
#   sudo ./install.sh --offline --side out      from the offline bundle only
#
# Where things come from:
#   --offline          only the files in this folder: the offline bundle
#                      (bin/<cpu>/dyode, packaging/debs/).  Nothing is
#                      fetched.  The whole bundle is checked against
#                      SHA256SUMS before anything is changed.
#   --online           apt for OS packages; cargo may build the binary
#                      (neither given: asked on an interactive terminal,
#                      else offline when this folder is a bundle)
#   --with LIST        also install file-sharing services for the received
#                      files: ssh, nfs, smb (comma-separated).  Packages only:
#                      configure the shares yourself (never share *.incoming).
#   --skip-os-packages install no OS packages (nftables, iproute2, services)
#
# Options:
#   --binary PATH      use this dyode binary (default: the bundle's
#                      bin/<cpu>/dyode, else target/release/dyode, built with
#                      cargo if missing and --online)
#   --prefix DIR       where the binary goes (default /usr/local/bin)
#   --etc DIR          where config.yaml and dyode.key live (default /etc/dyode)
#   --no-firewall      do not install the diode-port nftables rules (out side)
#   --no-tuning        do not install the kernel settings
#   --firewall-only    only (re)apply the firewall rules from config.yaml
#   --tuning-only      only (re)apply the kernel settings
#   --no-wizard        do not run the guided setup (dyode setup), which
#                      otherwise runs on every interactive install; an
#                      existing config.yaml is then kept, or one is made
#                      from config.example.yaml
#   --wizard           run it, and say so when the terminal cannot
#   --import-config F  start the guided setup from this config.yaml (e.g. the
#                      other box's)
#   --no-enable        write the systemd units but do not enable/start them
#   --dry-run          say what would be done, change nothing
#
# DYODE is one self-contained binary: there is no virtualenv, no Python
# wheel and no udpcast to install.  For an air-gapped box, carry the
# offline bundle (dyode-<version>-offline.tar.gz, from the GitHub release)
# across: it holds the binaries for amd64 and arm64, the OS packages for
# every supported system, and the vendored source.  See docs/OFFLINE.md.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
MODE="" WITH="" SKIP_OS=0
SIDE="" VARIANT="v1" BINARY="" PREFIX="/usr/local/bin" ETC="${DYODE_ETC_DIR:-/etc/dyode}"
NO_FIREWALL=0 NO_TUNING=0 FIREWALL_ONLY=0 TUNING_ONLY=0 NO_ENABLE=0 DRY_RUN=0
WIZARD=auto IMPORT_CFG=""
SYS_ETC="${DYODE_ETC:-/etc}"

say()  { printf '%s\n' "$*"; }
step() { printf '\n==> %s\n' "$*"; }
warn() { printf 'WARNING: %s\n' "$*" >&2; }
die()  { printf 'ERROR: %s\n' "$*" >&2; exit 1; }
run()  { if [ "$DRY_RUN" -eq 1 ]; then say "   would run: $*"; else "$@"; fi; }

while [ $# -gt 0 ]; do
  case "$1" in
    --side) SIDE="${2:-}"; shift 2 ;;
    --variant) VARIANT="${2:-}"; shift 2 ;;
    --binary) BINARY="${2:-}"; shift 2 ;;
    --prefix) PREFIX="${2:-}"; shift 2 ;;
    --etc) ETC="${2:-}"; shift 2 ;;
    --no-firewall) NO_FIREWALL=1; shift ;;
    --no-tuning) NO_TUNING=1; shift ;;
    --firewall-only) FIREWALL_ONLY=1; shift ;;
    --tuning-only) TUNING_ONLY=1; shift ;;
    --no-enable) NO_ENABLE=1; shift ;;
    --wizard) WIZARD=yes; shift ;;
    --no-wizard) WIZARD=no; shift ;;
    --import-config) IMPORT_CFG="${2:-}"; WIZARD=yes; shift 2 ;;
    --dry-run) DRY_RUN=1; shift ;;
    --offline) MODE=offline; shift ;;
    --online) MODE=online; shift ;;
    --with) WITH="${2:-}"; shift 2 ;;
    --skip-os-packages) SKIP_OS=1; shift ;;
    -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'; exit 0 ;;
    *) die "unknown option $1 (see --help)" ;;
  esac
done
case "$SIDE" in in|out) ;; *) die "choose --side in or --side out" ;; esac
case "$VARIANT" in v1|v2) ;; *) die "--variant must be v1 or v2" ;; esac
WITH="${WITH//,/ }"
for svc in $WITH; do
  case "$svc" in ssh|nfs|smb) ;; *) die "--with: unknown service $svc (ssh, nfs, smb)" ;; esac
done
if [ "$DRY_RUN" -eq 0 ] && [ "$(id -u)" -ne 0 ] && [ -z "${DYODE_ETC:-}" ]; then
  die "run as root (sudo), or use --dry-run"
fi

# Write SRC to DEST only if it changed; keep a backup of a hand-edited copy.
# Prints "unchanged", "" (new) or the backup path.
install_file() {
  local dest="$1" src="$2" mode="${3:-0644}" backup=""
  if [ -f "$dest" ] && cmp -s "$dest" "$src"; then echo unchanged; return 0; fi
  mkdir -p "$(dirname "$dest")"
  if [ -f "$dest" ]; then backup="$dest.backup-$(date +%Y%m%d-%H%M%S)"; cp -p "$dest" "$backup"; fi
  install -m "$mode" "$src" "$dest"
  echo "$backup"
}

# ------------------------------------------------------------- the host
OS_RELEASE="${DYODE_OS_RELEASE:-/etc/os-release}"   # override: tests
OS_ID="" OS_VER=""
if [ -r "$OS_RELEASE" ]; then
  OS_ID="$(. "$OS_RELEASE" && echo "${ID:-}")"
  OS_VER="$(. "$OS_RELEASE" && echo "${VERSION_ID:-}")"
fi
ARCH="${DYODE_ARCH:-$(dpkg --print-architecture 2>/dev/null || true)}"   # override: tests
if [ -z "$ARCH" ]; then
  case "$(uname -m)" in x86_64) ARCH=amd64 ;; aarch64|arm64) ARCH=arm64 ;; *) ARCH="$(uname -m)" ;; esac
fi
DEB_DIR="$ROOT/packaging/debs/$OS_ID-$OS_VER/$ARCH"     # e.g. ubuntu-24.04/amd64
BUNDLE_BIN="$ROOT/bin/$ARCH/dyode"
SUPPORTED="Ubuntu 24.04 and 26.04, Debian 12 and 13 / Raspberry Pi OS bookworm and trixie (64-bit); amd64 and arm64"
is_bundle() { [ -f "$ROOT/SHA256SUMS" ] && [ -d "$ROOT/bin" ]; }
debs_present() { [ -n "$OS_ID" ] && ls "$DEB_DIR"/*.deb >/dev/null 2>&1; }

interactive() { [ -t 0 ] && [ -t 1 ]; }

# Offline or online, asked before anything is installed: in a dialog box
# by the dyode binary when one is at hand (the bundle's, a local build),
# else as a plain question.
ask_mode() {
  local def=online bin="" c ans rc=0
  is_bundle && def=offline
  for c in "$BINARY" "$BUNDLE_BIN" "$ROOT/target/release/dyode" "$PREFIX/dyode"; do
    if [ -n "$c" ] && [ -x "$c" ] && "$c" version >/dev/null 2>&1; then bin="$c"; break; fi
  done
  if [ -n "$bin" ]; then
    ans="$(mktemp)"
    # shellcheck disable=SC2046
    "$bin" setup --ask-install-mode "$ans" --default "$def" $(is_bundle && echo --bundle) || rc=$?
    MODE="$(cat "$ans" 2>/dev/null || true)"; rm -f "$ans"
    [ "$rc" -eq 3 ] && die "installation cancelled; nothing was changed"
    case "$MODE" in offline|online) return 0 ;; esac
  fi
  local hint="this folder is not an offline bundle"
  is_bundle && hint="this folder is an offline bundle"
  while :; do
    printf 'Install offline (only this folder) or online (apt, cargo)?  [%s] (%s): ' "$def" "$hint"
    read -r ans || die "no answer; nothing was changed"
    case "${ans:-$def}" in
      offline|off|o|1) MODE=offline; return 0 ;;
      online|on|n|2) MODE=online; return 0 ;;
      q|quit) die "installation cancelled; nothing was changed" ;;
    esac
  done
}

if [ -z "$MODE" ]; then
  if interactive && [ "$FIREWALL_ONLY" -eq 0 ] && [ "$TUNING_ONLY" -eq 0 ]; then
    ask_mode
  elif is_bundle; then MODE=offline
  else MODE=online
  fi
fi

# -------------------------------------------------------- OS packages
# Keep in step with CORE_PKGS / SERVICE_PKGS in tools/build_debs.sh.
service_pkgs() {
  case "$1" in
    ssh) echo "openssh-server openssh-sftp-server openssh-client" ;;
    nfs) echo "nfs-kernel-server nfs-common" ;;
    smb) echo "samba smbclient cifs-utils" ;;
  esac
}
pkg_installed() { dpkg-query -W -f='${Status}' "$1" 2>/dev/null | grep -q "install ok installed"; }
PKGS=()
if [ "$SKIP_OS" -eq 0 ] && [ "$FIREWALL_ONLY" -eq 0 ] && [ "$TUNING_ONLY" -eq 0 ]; then
  if [ "$SIDE" = out ] && [ "$VARIANT" = v1 ] && [ "$NO_FIREWALL" -eq 0 ] && ! command -v nft >/dev/null 2>&1; then
    PKGS+=(nftables)
  fi
  if [ "$SIDE" = in ] && [ "$VARIANT" = v1 ] && ! command -v ip >/dev/null 2>&1; then
    PKGS+=(iproute2)
  fi
  # openssl: 'dyode api-cert' and the guided setup make the API's TLS certificate with it.
  if [ "$VARIANT" = v1 ] && ! command -v openssl >/dev/null 2>&1; then
    PKGS+=(openssl)
  fi
  for svc in $WITH; do
    for pkg in $(service_pkgs "$svc"); do pkg_installed "$pkg" || PKGS+=("$pkg"); done
  done
  # Test hook: extra packages, to exercise the offline apt path.
  for pkg in ${DYODE_EXTRA_PKGS:-}; do PKGS+=("$pkg"); done
fi

say "DYODE installer: $MODE install, ${OS_ID:-unknown} ${OS_VER:-?} ($ARCH), side $SIDE ($VARIANT)"
if is_bundle; then say "  bundle:      $(sed -n '1p' "$ROOT/BUNDLE.txt" 2>/dev/null || echo "offline bundle")"; fi
if [ "${#PKGS[@]}" -gt 0 ]; then
  say "  OS packages: ${PKGS[*]} (from $([ "$MODE" = offline ] && echo "packaging/debs/$OS_ID-$OS_VER/$ARCH" || echo apt))"
else
  say "  OS packages: nothing needed"
fi

# Catch files damaged on their way across the air gap, before any change.
check_bundle() {
  if ! is_bundle && [ "${#PKGS[@]}" -eq 0 ] && { [ -n "$BINARY" ] || [ -x "$ROOT/target/release/dyode" ]; }; then
    say "  (not a bundle, but nothing needs fetching: offline install from a local build)"
    return 0
  fi
  is_bundle || die "--offline: this folder is not an offline bundle (no SHA256SUMS and bin/).  Download dyode-<version>-offline.tar.gz from the release, or build one with tools/build_bundle.sh"
  step "checking the offline bundle"
  (cd "$ROOT" && sha256sum --quiet -c SHA256SUMS) ||
    die "files in the bundle do not match SHA256SUMS; copy the bundle again"
  # Every binary and package must be listed: an unlisted one would be used unchecked.
  local unlisted
  unlisted="$(cd "$ROOT" && comm -23 \
      <(find bin packaging/debs -type f 2>/dev/null | sort) \
      <(sed 's/^[0-9a-f]*  \*\{0,1\}//' SHA256SUMS | sort))"
  [ -z "$unlisted" ] || die "file(s) in the bundle not listed in SHA256SUMS, so they cannot be checked: $(echo $unlisted | head -c 300)"
  say "   $(wc -l < "$ROOT/SHA256SUMS" | tr -d ' ') files intact"
  if [ "${#PKGS[@]}" -gt 0 ]; then
    [ "$OS_ID" != raspbian ] || die "32-bit Raspberry Pi OS is not covered by the bundle; use the 64-bit edition, or install ${PKGS[*]} by hand and rerun with --skip-os-packages"
    debs_present || die "no bundled OS packages for ${OS_ID:-this system} ${OS_VER} $ARCH (bundled for: $SUPPORTED).  Install ${PKGS[*]} by hand, then rerun with --skip-os-packages"
    (cd "$DEB_DIR" && sha256sum --quiet -c SHA256SUMS) || die "packages in ${DEB_DIR#"$ROOT"/} do not match their SHA256SUMS"
    say "   $(ls "$DEB_DIR"/*.deb | wc -l | tr -d ' ') OS packages for $OS_ID $OS_VER $ARCH"
  fi
}

# Offline, apt is pointed at a private repository built from the bundled
# .deb files and nothing else: apt resolves dependencies itself, installs
# only what is missing, never downgrades a package the host has newer, and
# cannot reach the network.
install_offline_debs() {
  local tmp; tmp="$(mktemp -d)"
  # shellcheck disable=SC2064
  trap "rm -rf '$tmp'" RETURN
  mkdir -p "$tmp/repo" "$tmp/lists/partial" "$tmp/parts"
  cp "$DEB_DIR"/*.deb "$tmp/repo/"
  chmod 755 "$tmp" "$tmp/repo"; chmod 644 "$tmp/repo"/*.deb
  # The Packages index is made from the .deb files themselves, so it can
  # never disagree with them; only dpkg-deb is needed.
  for deb in "$tmp/repo"/*.deb; do
    dpkg-deb -f "$deb"
    printf 'Filename: ./%s\nSize: %s\nSHA256: %s\n\n' "$(basename "$deb")" \
      "$(stat -c %s "$deb")" "$(sha256sum "$deb" | cut -d' ' -f1)"
  done > "$tmp/repo/Packages"
  echo "deb [trusted=yes] file:$tmp/repo ./" > "$tmp/local.list"
  local opts=(-o "Dir::Etc::SourceList=$tmp/local.list"
              -o "Dir::Etc::SourceParts=$tmp/parts"
              -o "Dir::State::Lists=$tmp/lists"
              -o "Dir::Cache::pkgcache=" -o "Dir::Cache::srcpkgcache="
              -o "APT::Sandbox::User=root" -o "Acquire::Languages=none")
  apt-get "${opts[@]}" -qq update
  DEBIAN_FRONTEND=noninteractive apt-get "${opts[@]}" install -y --no-install-recommends "${PKGS[@]}" ||
    die "the bundled packages for $OS_ID $OS_VER $ARCH cannot satisfy ${PKGS[*]} on this host (see apt above).  Rebuild them with tools/build_debs.sh, or install them by hand and rerun with --skip-os-packages"
}

install_os_packages() {
  [ "${#PKGS[@]}" -gt 0 ] || return 0
  step "OS packages: ${PKGS[*]}"
  if [ "$DRY_RUN" -eq 1 ]; then
    say "   would install from $([ "$MODE" = offline ] && echo "${DEB_DIR#"$ROOT"/} via a private local apt repository" || echo apt)"
  elif [ "$MODE" = offline ]; then
    install_offline_debs
  else
    apt-get update -qq
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends "${PKGS[@]}"
  fi
}

DYODE=""
find_binary() {
  if [ -n "$BINARY" ]; then DYODE="$BINARY"
  elif is_bundle; then
    [ -x "$BUNDLE_BIN" ] || die "the bundle has no binary for $ARCH (has: $(ls "$ROOT/bin" | tr '\n' ' '))"
    DYODE="$BUNDLE_BIN"
  elif [ -x "$ROOT/target/release/dyode" ]; then DYODE="$ROOT/target/release/dyode"
  elif [ "$MODE" = online ] && command -v cargo >/dev/null 2>&1; then
    step "building dyode (cargo build --release)"
    run cargo build --release --manifest-path "$ROOT/Cargo.toml"
    DYODE="$ROOT/target/release/dyode"
  elif [ -x "$PREFIX/dyode" ]; then DYODE="$PREFIX/dyode"
  else die "no dyode binary: use the offline bundle, build it with 'cargo build --release', or pass --binary"
  fi
  [ "$DRY_RUN" -eq 1 ] || [ -x "$DYODE" ] || die "$DYODE is not executable"
  "$DYODE" version >/dev/null 2>&1 || [ "$DRY_RUN" -eq 1 ] ||
    die "$DYODE does not run on this machine ($ARCH): wrong CPU, or a damaged copy"
}

# ---------------------------------------------------------------- firewall
firewall_wanted() { [ "$NO_FIREWALL" -eq 0 ] && [ "$SIDE" = out ] && [ "$VARIANT" = v1 ]; }

apply_firewall() {
  firewall_wanted || return 0
  step "diode-port firewall"
  local cfg="$ETC/config.yaml" rules iface
  if [ ! -f "$cfg" ]; then
    say "   no $cfg yet: once it is in place, run: sudo ./install.sh --side out --firewall-only"
    return 0
  fi
  rules="$(mktemp)"
  if ! "$DYODE" firewall-rules -c "$cfg" > "$rules" 2>"$rules.err"; then
    warn "could not build firewall rules from $cfg: $(cat "$rules.err")"; rm -f "$rules" "$rules.err"; return 0
  fi
  rm -f "$rules.err"
  iface="$("$DYODE" firewall-rules -c "$cfg" --interface)"
  if [ "$DRY_RUN" -eq 1 ]; then
    say "   would write $ETC/firewall.nft and enable dyode-firewall.service:"
    sed -n 's/^\t*\(.*udp dport.*\) counter drop.*/     \1 -> drop/p' "$rules"; rm -f "$rules"; return 0
  fi
  command -v nft >/dev/null 2>&1 || { warn "nft not found: install nftables, then rerun with --firewall-only"; rm -f "$rules"; return 0; }
  nft -c -f "$rules" >/dev/null 2>&1 || { warn "nft rejected the generated rules; nothing changed"; rm -f "$rules"; return 0; }
  say "   $ETC/firewall.nft: $(install_file "$ETC/firewall.nft" "$rules" 0644 | sed 's/^$/written/')"
  rm -f "$rules"
  local unit; unit="$(mktemp)"
  cat > "$unit" <<EOF
# Managed by DYODE install.sh (rewritten on each run; --no-firewall to skip).
[Unit]
Description=DYODE firewall rules for the diode ports
DefaultDependencies=no
After=nftables.service
Before=network-pre.target dyode-out.service
Wants=network-pre.target

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/usr/sbin/nft -f $ETC/firewall.nft
ExecStop=/usr/sbin/nft delete table inet dyode

[Install]
WantedBy=multi-user.target
EOF
  install_file "$SYS_ETC/systemd/system/dyode-firewall.service" "$unit" >/dev/null
  printf '# Managed by DYODE install.sh: drop packets on the diode NIC whose source\n# would not be routed back through it.\nnet.ipv4.conf.%s.rp_filter = 1\n' "$iface" > "$unit"
  install_file "$SYS_ETC/sysctl.d/92-dyode-firewall.conf" "$unit" >/dev/null
  rm -f "$unit"
  nft -f "$ETC/firewall.nft" && say "   loaded: $(sed -n 's/^# Diode ports \(.*\)$/\1/p' "$ETC/firewall.nft")" ||
    warn "nft could not load $ETC/firewall.nft; it will be loaded at the next boot"
  sysctl -q -p "$SYS_ETC/sysctl.d/92-dyode-firewall.conf" >/dev/null 2>&1 || true
  if command -v systemctl >/dev/null 2>&1; then
    systemctl daemon-reload >/dev/null 2>&1 || true
    systemctl enable dyode-firewall.service >/dev/null 2>&1 || warn "could not enable dyode-firewall.service"
  fi
}

# ------------------------------------------------------------------ tuning
# lidi sends each RaptorQ block as one burst and asks for socket buffers of
# one block's worth of packets (~30 MB at the defaults); the kernel caps
# them at rmem_max/wmem_max, which default to ~208 KB.  On the receiving
# box, writeback is also made continuous so large files do not stall the
# receiver (CAVEATS.md, "Disk speed").
tuning_content() {
  local kb dirty bg
  if [ "$SIDE" = out ]; then
    kb="$(awk '/^MemTotal:/{print $2; exit}' "${DYODE_MEMINFO:-/proc/meminfo}" 2>/dev/null || true)"
    case "$kb" in ''|*[!0-9]*) kb=$((4096 * 1024)) ;; esac
    dirty=$((kb * 1024 / 5)); [ "$dirty" -gt $((2048 << 20)) ] && dirty=$((2048 << 20)); [ "$dirty" -lt $((128 << 20)) ] && dirty=$((128 << 20))
    bg=$((dirty / 4)); [ "$bg" -gt $((64 << 20)) ] && bg=$((64 << 20))
    cat <<EOF
# Managed by DYODE install.sh (rewritten on each run; --no-tuning to skip).
# Receiving box: room for lidi's bursts, and continuous disk writeback.
net.core.rmem_max = 67108864
net.core.rmem_default = 33554432
net.core.netdev_max_backlog = 10000
vm.dirty_background_bytes = $bg
vm.dirty_bytes = $dirty
EOF
  else
    cat <<'EOF'
# Managed by DYODE install.sh (rewritten on each run; --no-tuning to skip).
# Sending box: room for lidi's bursts.
net.core.wmem_max = 67108864
net.core.wmem_default = 33554432
EOF
  fi
}

apply_tuning() {
  [ "$NO_TUNING" -eq 0 ] && [ "$VARIANT" = v1 ] || return 0
  step "kernel settings"
  local f="$SYS_ETC/sysctl.d/90-dyode.conf" new
  if [ "$DRY_RUN" -eq 1 ]; then say "   would write $f:"; tuning_content | sed -n 's/^\([a-z].*\)/     \1/p'; return 0; fi
  new="$(mktemp)"; tuning_content > "$new"
  say "   $f: $(install_file "$f" "$new" | sed 's/^$/written/')"; rm -f "$new"
  sysctl -q -p "$f" >/dev/null 2>&1 || warn "sysctl could not load $f; it applies at the next boot"
}

# ------------------------------------------------------------------- main
# Offline: check the whole bundle BEFORE changing anything, so a damaged or
# incomplete copy stops the install instead of leaving it half done.
[ "$MODE" = offline ] && check_bundle
find_binary
install_os_packages
if [ "$FIREWALL_ONLY" -eq 1 ]; then apply_firewall; exit 0; fi
if [ "$TUNING_ONLY" -eq 1 ]; then apply_tuning; exit 0; fi

step "binary"
run install -D -m 0755 "$DYODE" "$PREFIX/dyode"
say "   $PREFIX/dyode ($("$DYODE" version 2>/dev/null || echo '?'))"

step "configuration in $ETC"
run mkdir -p "$ETC"
run_wizard() {
  [ "$WIZARD" = no ] && return 1
  if ! interactive; then
    [ "$WIZARD" = yes ] && warn "--wizard needs an interactive terminal; skipping the guided setup"
    return 1
  fi
  local args=(setup --side "$SIDE" --variant "$VARIANT" --etc "$ETC")
  [ -n "$IMPORT_CFG" ] && args+=(--import-config "$IMPORT_CFG")
  [ "$DRY_RUN" -eq 1 ] && args+=(--dry-run)
  # The installer writes the systemd unit itself, from what setup wrote.
  if DYODE_INSTALLER=1 "$DYODE" "${args[@]}"; then
    return 0
  fi
  warn "guided setup left before writing; continuing without it"
  return 1
}
if run_wizard; then
  if [ "$DRY_RUN" -eq 1 ]; then say "   guided setup done (dry run: nothing written)"; else say "   $ETC/config.yaml written by the guided setup"; fi
elif [ -f "$ETC/config.yaml" ]; then
  say "   keeping $ETC/config.yaml"
else
  run install -m 0640 "$ROOT/config.example.yaml" "$ETC/config.yaml"
  say "   wrote $ETC/config.yaml from config.example.yaml: EDIT IT (addresses, MACs, modules),"
  say "   then copy the SAME file to the other box.  'dyode interfaces' lists NICs and MACs,"
  say "   or run the guided setup:  sudo dyode setup --side $SIDE"
fi
if [ "$VARIANT" = v1 ] && [ ! -f "$ETC/dyode.key" ]; then
  say "   no $ETC/dyode.key yet.  On ONE box:  sudo dyode new-key -c $ETC/config.yaml"
  say "   then copy it to the other box beside config.yaml (chmod 600)."
fi

step "systemd"
case "$VARIANT/$SIDE" in v1/in) CMD=in ;; v1/out) CMD=out ;; v2/in) CMD=serial-in ;; v2/out) CMD=serial-out ;; esac
UNIT="$(mktemp)"
SERIAL_FLAG=""; [ "$VARIANT" = v2 ] && SERIAL_FLAG="--serial"
CFG_FOR_UNIT="$ETC/config.yaml"
if [ ! -f "$CFG_FOR_UNIT" ]; then
  CFG_FOR_UNIT="$ROOT/config.example.yaml"
  [ "$VARIANT" = v1 ] && warn "the unit waits for the example config's NIC; run 'sudo dyode setup' or re-run this installer once config.yaml is right"
fi
# shellcheck disable=SC2086
"$DYODE" systemd-unit --side "$SIDE" $SERIAL_FLAG --workdir "$ETC" --binary "$PREFIX/dyode" -c "$CFG_FOR_UNIT" | sed '1d' > "$UNIT"
if [ "$DRY_RUN" -eq 1 ]; then
  say "   would write $SYS_ETC/systemd/system/dyode-$SIDE.service ($CMD)"
else
  say "   dyode-$SIDE.service: $(install_file "$SYS_ETC/systemd/system/dyode-$SIDE.service" "$UNIT" | sed 's/^$/written/')"
  install_file "$SYS_ETC/logrotate.d/dyode-transfer" "$ROOT/packaging/logrotate/dyode-transfer" >/dev/null
  sed "s#/usr/local/bin/dyode#$PREFIX/dyode#" "$ROOT/packaging/systemd/dyode-log-archive.service" > "$UNIT.a"
  install_file "$SYS_ETC/systemd/system/dyode-log-archive.service" "$UNIT.a" >/dev/null
  install_file "$SYS_ETC/systemd/system/dyode-log-archive.timer" "$ROOT/packaging/systemd/dyode-log-archive.timer" >/dev/null
  rm -f "$UNIT.a"
  say "   logrotate and weekly log archiving installed"
fi
rm -f "$UNIT"

apply_tuning
apply_firewall

if [ "$DRY_RUN" -eq 0 ] && command -v systemctl >/dev/null 2>&1; then
  systemctl daemon-reload || true
  systemctl enable --now dyode-log-archive.timer >/dev/null 2>&1 || true
  if [ "$NO_ENABLE" -eq 0 ] && [ -f "$ETC/config.yaml" ] && { [ "$VARIANT" = v2 ] || [ -f "$ETC/dyode.key" ]; } &&
     "$PREFIX/dyode" check -c "$ETC/config.yaml" $( [ "$VARIANT" = v2 ] && echo --serial ) >/dev/null 2>&1; then
    systemctl enable --now "dyode-$SIDE.service" && say "   dyode-$SIDE.service enabled and started"
  else
    say "
Next: edit $ETC/config.yaml, put the shared key in place, check with
    sudo dyode check -c $ETC/config.yaml --side $SIDE
then start DYODE:
    sudo systemctl enable --now dyode-$SIDE.service && journalctl -u dyode-$SIDE -f"
    [ "$SIDE" = out ] && [ "$VARIANT" = v1 ] && say "and apply the firewall:  sudo ./install.sh --side out --firewall-only"
  fi
fi
exit 0
