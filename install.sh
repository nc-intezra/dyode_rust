#!/usr/bin/env bash
# DYODE installer (Rust + lidi).
#
#   sudo ./install.sh --side in            input (sending) box
#   sudo ./install.sh --side out           output (receiving) box
#   sudo ./install.sh --side out --variant v2   DYODE v2 (serial link)
#
# Options:
#   --binary PATH      use this dyode binary (default: target/release/dyode,
#                      built with cargo if missing)
#   --prefix DIR       where the binary goes (default /usr/local/bin)
#   --etc DIR          where config.yaml and dyode.key live (default /etc/dyode)
#   --no-firewall      do not install the diode-port nftables rules (out side)
#   --no-tuning        do not install the kernel settings
#   --firewall-only    only (re)apply the firewall rules from config.yaml
#   --tuning-only      only (re)apply the kernel settings
#   --no-enable        write the systemd units but do not enable/start them
#   --dry-run          say what would be done, change nothing
#
# DYODE is one self-contained binary: there is no virtualenv, no Python
# wheel and no udpcast to install.  For an air-gapped box, build it once
# (cargo build --release) on a machine with the same CPU architecture and
# copy target/release/dyode with this folder.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
SIDE="" VARIANT="v1" BINARY="" PREFIX="/usr/local/bin" ETC="${DYODE_ETC_DIR:-/etc/dyode}"
NO_FIREWALL=0 NO_TUNING=0 FIREWALL_ONLY=0 TUNING_ONLY=0 NO_ENABLE=0 DRY_RUN=0
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
    --dry-run) DRY_RUN=1; shift ;;
    -h|--help) sed -n '2,25p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) die "unknown option $1 (see --help)" ;;
  esac
done
case "$SIDE" in in|out) ;; *) die "choose --side in or --side out" ;; esac
case "$VARIANT" in v1|v2) ;; *) die "--variant must be v1 or v2" ;; esac
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

DYODE=""
find_binary() {
  if [ -n "$BINARY" ]; then DYODE="$BINARY"
  elif [ -x "$ROOT/target/release/dyode" ]; then DYODE="$ROOT/target/release/dyode"
  elif command -v cargo >/dev/null 2>&1; then
    step "building dyode (cargo build --release)"
    run cargo build --release --manifest-path "$ROOT/Cargo.toml"
    DYODE="$ROOT/target/release/dyode"
  elif [ -x "$PREFIX/dyode" ]; then DYODE="$PREFIX/dyode"
  else die "no dyode binary: build it with 'cargo build --release' or pass --binary"
  fi
  [ "$DRY_RUN" -eq 1 ] || [ -x "$DYODE" ] || die "$DYODE is not executable"
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
find_binary
if [ "$FIREWALL_ONLY" -eq 1 ]; then apply_firewall; exit 0; fi
if [ "$TUNING_ONLY" -eq 1 ]; then apply_tuning; exit 0; fi

step "binary"
run install -D -m 0755 "$DYODE" "$PREFIX/dyode"
say "   $PREFIX/dyode ($("$DYODE" version 2>/dev/null || echo '?'))"

step "configuration in $ETC"
run mkdir -p "$ETC"
if [ -f "$ETC/config.yaml" ]; then
  say "   keeping $ETC/config.yaml"
else
  run install -m 0640 "$ROOT/config.example.yaml" "$ETC/config.yaml"
  say "   wrote $ETC/config.yaml from config.example.yaml: EDIT IT (addresses, MACs, modules),"
  say "   then copy the SAME file to the other box.  'dyode interfaces' lists NICs and MACs."
fi
if [ "$VARIANT" = v1 ] && [ ! -f "$ETC/dyode.key" ]; then
  say "   no $ETC/dyode.key yet.  On ONE box:  sudo dyode new-key -c $ETC/config.yaml"
  say "   then copy it to the other box beside config.yaml (chmod 600)."
fi

step "systemd"
case "$VARIANT/$SIDE" in v1/in) CMD=in ;; v1/out) CMD=out ;; v2/in) CMD=serial-in ;; v2/out) CMD=serial-out ;; esac
UNIT="$(mktemp)"
SERIAL_FLAG=""; [ "$VARIANT" = v2 ] && SERIAL_FLAG="--serial"
CFG_FOR_UNIT="$ETC/config.yaml"; [ -f "$CFG_FOR_UNIT" ] || CFG_FOR_UNIT="$ROOT/config.example.yaml"
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
     "$PREFIX/dyode" check -c "$ETC/config.yaml" >/dev/null 2>&1; then
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
