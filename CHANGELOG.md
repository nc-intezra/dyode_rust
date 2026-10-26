# Changelog

## [2.0.1] - unreleased

### Added

- **Offline install bundle**, as DYODE (Python) 1.5 had:
  `dyode-<version>-offline.tar.gz` on every release, holding the binaries
  for amd64 and arm64 (built in Debian 12, so they run on every supported
  system: glibc 2.36 or newer; with mimalloc), the OS packages
  (nftables, iproute2, optional SSH/NFS/SMB servers) for Ubuntu 24.04/26.04
  and Debian / Raspberry Pi OS 12/13 on both CPUs, and the source with every
  dependency vendored.  See docs/OFFLINE.md.
- `install.sh --offline` / `--online` / `--with ssh,nfs,smb` /
  `--skip-os-packages`.  Offline, the whole bundle is checked against
  SHA256SUMS (unlisted files refused) before anything changes, and the
  packages go in through a private, file-only apt repository.
- Release workflow: builds the binaries from the vendored source with
  `--offline`, in a container without network, fetches the packages in clean containers, and installs the
  bundle in a `--network none` container of every supported system before
  publishing.
- `mimalloc` cargo feature; `dyode version` names the allocator.

### Fixed

- `install.sh` no longer asks a DYODE v2 box for a message key before
  enabling its service.

## [2.0.0] - 2026-10-26

First Rust release: a port of DYODE (Python) 1.5.0, with ANSSI's lidi as
the diode transport.  Upgrade both boxes together; see docs/MIGRATION.md.

### Changed

- **Transport: lidi 3.0.0 (RaptorQ), embedded as a library**, replacing
  udpcast for files and DYODE's DYO1 datagrams for messages.  New `lidi:`
  section in config.yaml; all modules share `lidi.ports`.
- **One signed envelope per transfer (DYL1)**, replacing the DYA1 seal and
  the folder manifests: header and body SHA-256 under HMAC-SHA256 with the
  existing `dyode.key`, same replay rules.  A module tag refuses traffic from
  a box with a different config.yaml.
- **Folder transfers** run concurrently, need no `file_gap`/`autostart`, and
  are followed by a signed batch summary that names missing files.
- **Modbus** uses a built-in client and read-only server (FC1-4, device
  identification) instead of pymodbus; older updates never overwrite newer
  ones.
- **One binary**, `dyode`, with subcommands replacing `dyode_in.py`,
  `dyode_out.py`, `dyode_logs.py`, and parts of `dyode_setup.py`; modules run as
  supervised threads.
- **Rate limiting**: `lidi.max_mbps` (default 100) and per-module `bitrate`.
- **Installer** rewritten for the binary: systemd units, logrotate, archive
  timer, receive/send kernel tuning, nftables rules for `lidi.ports`.

### Unchanged

config.yaml (old files load; udpcast-only keys are reported as ignored),
dyode.key, folder layout, `transfer.jsonl` schema and event names, the 1.5.0
security behaviour, DYODE v2 serial framing.
