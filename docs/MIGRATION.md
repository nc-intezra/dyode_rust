# Migrating from DYODE (Python) 1.5.x

The Rust version replaces both the Python code and udpcast.  Upgrade **both
boxes together**: a Rust box and a Python box cannot talk (different wire
format), and the lidi parameters must match.

## What stays the same

* **`config.yaml`** -- the same file loads unchanged.  Same keys, defaults
  and validation messages.  `dyode check` lists anything it ignores.
* **`dyode.key`** -- same format (hex, beside config.yaml, mode 0600); keep it.
* **Folders** -- `in`, `out`, `staging` (`<out>.incoming`), `.dyode_sent`,
  `keep_sent_hours`, `settle`, `batch_timeout`.
* **Logs** -- `/var/log/dyode-transfer/dyode.log` and `transfer.jsonl`, same
  event names and fields (`file_sent`, `file_stored`, `file_rejected`,
  `file_missing`, `manifest_received`, `batch_received`, `logs_*`), same
  logrotate file and weekly archive (`dyode logs --archive` replaces
  `dyode_logs.py --archive`).
* **Security behaviour from 1.5.0** -- signed traffic with replay refusal,
  nothing placed through a symlink (D-01), the file sent is the file checked
  (D-04), bounded receive buffers (D-05), read-only Modbus mirror with
  exceptions 01/02 (D-03), nftables diode-port firewall (D-02), screen viewer
  on the IT side and capped at `max_streams`.
* **systemd** -- `dyode-in.service` / `dyode-out.service`, bound to the diode
  NIC, waiting for its address, `Restart=always`.

## What changed

| Python 1.5 | Rust |
|---|---|
| `python3 dyode_in.py` / `dyode_out.py` (venv, pymodbus, PyYAML) | `dyode in` / `dyode out`, one binary |
| udpcast (`udp-sender`/`udp-receiver`) for files | lidi, RaptorQ, embedded |
| DYO1 datagrams + DYA1 seal for messages | lidi transfers + DYL1 envelope |
| one UDP port per module (`port:`) | all modules share `lidi.ports`; `port:` is ignored |
| manifest sent before each batch | each file carries its own signed name/size/hash; a summary follows the batch |
| one file at a time, `file_gap` between files | concurrent transfers (`lidi.max_clients`), no gap |
| `fec: 8x16/64` per folder module | `lidi.repair` (percent), for everything |
| `bitrate: 8` shared by folder modules | `lidi.max_mbps` (default 100) and per-module `bitrate` |
| one process per module | one supervised thread per module |
| `dyode_setup.py` (interactive wizard) | `dyode interfaces`, `dyode check`, `dyode systemd-unit`, `install.sh` |
| `python3 dyode_setup.py --new-key` | `dyode new-key` |

Ignored keys (accepted, reported by `dyode check`): `fec`, `autostart`,
`start_timeout`, `file_gap`, and module `port`.

## New: the `lidi:` section

All optional; see config.example.yaml.  `ports`, `mtu`, `block`, `repair`,
`heartbeat` and `max_clients` must be identical on both boxes, which they
are when config.yaml is.

## The firewall

The nftables rules (`install.sh --side out --firewall-only`, or
`dyode firewall-rules`) now cover `lidi.ports` instead of the per-module
ports.  **Re-apply them after upgrading**, or the old rules protect ports
nothing listens on any more.

## Steps

1. Stop both services.
2. On each box: `cargo build --release` (or copy the binary), then
   `sudo ./install.sh --side in|out`.  The installer keeps an existing
   `/etc/dyode/config.yaml`; to keep using the Python folder, pass
   `--etc /opt/dyode/DYODE_v1_full`.
3. Optionally add a `lidi:` section (the same on both boxes).
4. `sudo dyode check -c /etc/dyode/config.yaml --side in` (and `out`).
5. Output box: `sudo ./install.sh --side out --firewall-only`.
6. Start both: `systemctl enable --now dyode-in` / `dyode-out`.

## Not ported

* The curses setup wizard.  Its non-interactive parts are commands:
  `dyode interfaces` (NICs and MACs), `dyode systemd-unit`, `dyode check`.
* The NFS/SMB share *configuration* (`/etc/exports`, `smb.conf`).
  `install.sh --with ssh,nfs,smb` installs the servers, online or offline;
  configure the shares of the `out` folders yourself, and never share the
  `*.incoming` staging folder.

The offline install bundle is back in 2.0.1: see docs/OFFLINE.md.
