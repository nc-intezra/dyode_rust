# Changelog

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
