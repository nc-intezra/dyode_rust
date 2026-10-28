# DYODE (Rust + lidi)

A low-cost, DIY data diode for ICS and other one-way push needs: a fork of
[DYODE](https://github.com/nc-intezra/dyode_python3) rewritten in Rust,
with [lidi](https://github.com/ANSSI-FR/lidi) -- ANSSI's unidirectional
transfer software, with RaptorQ forward error correction -- as the diode
transport.

```
 OT network            input box                         output box              IT network
 PLC, files,  ──▶  dyode in ── lidi-send ══ UDP ══▶ lidi-receive ── dyode out ──▶ SCADA, shares,
 syslog            (signs every transfer)    one way    (verifies every transfer)     SIEM, browsers
```

What moves through the diode, one way:

| Module   | Input box                                    | Output box                                        |
|----------|----------------------------------------------|---------------------------------------------------|
| `folder` | watches a folder, sends each settled file    | verifies it, places it in `out` (never via a link) |
| `modbus` | polls a PLC over Modbus/TCP                  | serves the values from a **read-only** Modbus server |
| `logs`   | syslog UDP/TCP and `tail -F` of files        | writes files and/or forwards to a SIEM            |
| `screen` | sends the newest screenshot (JPEG)           | serves it as an MJPEG page                        |

DYODE v2 ("light", an optocoupler on a serial line) is supported too, for
Modbus: `dyode serial-in` / `dyode serial-out`.

## How lidi is used

lidi is linked in as a library (crates `lidi-send`, `lidi-receive`,
`lidi-protocol`, pinned to v3.0.0), so a box runs one program, `dyode`,
configured by one file.  Each DYODE module is a lidi *endpoint*; every
Modbus poll, log batch, screen frame and file is one lidi transfer, wrapped
in a signed DYODE envelope (HMAC-SHA256 with a key shared by both boxes, plus
replay protection).  lidi's RaptorQ coding replaces udpcast and DYODE's own
UDP framing.  See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Quick start

Air-gapped box?  Carry `dyode-<version>-offline.tar.gz` from the release
and run `sudo ./install.sh --offline --side in|out` from it: see
[docs/OFFLINE.md](docs/OFFLINE.md).  With internet access:

```bash
cargo build --release                       # -> target/release/dyode
sudo ./install.sh --side in                 # on the input box
sudo ./install.sh --side out --import-config /media/usb/config.yaml   # output box
```

On a terminal, `install.sh` first asks whether to install offline or
online, then runs the guided setup (`dyode setup`): dialog boxes for the
diode network card, addresses, lidi ports, modules (each with an Advanced
screen) and the shared key.  It writes `/etc/dyode/config.yaml`, checked
by DYODE's own loader, and the systemd unit is generated from it.  Set up
one box, then import its config.yaml on the other.  `dyode setup` can be
run again at any time; `--no-wizard` skips it, and `dyode setup --plain`
asks plain questions instead of drawing dialogs.  Then:

```bash
sudo dyode check -c /etc/dyode/config.yaml --side in   # also compares the installed unit
sudo systemctl enable --now dyode-in        # dyode-out on the output box
sudo ./install.sh --side out --firewall-only   # output box, once config.yaml is final
```

Everything is in [config.example.yaml](config.example.yaml).  An existing
DYODE (Python) `config.yaml` and `dyode.key` work unchanged; see
[docs/MIGRATION.md](docs/MIGRATION.md) for what changed and why.

Both boxes must run the same DYODE version: the envelope is new, and the
lidi parameters must match.

## Commands

| Command | |
|---|---|
| `dyode in` / `dyode out` | run the input / output side (v1, lidi) |
| `dyode serial-in` / `serial-out` | run DYODE v2 (Modbus over serial) |
| `dyode setup` | guided setup in dialog boxes (or `--plain` questions) |
| `dyode status [--json]` | this box's status (and the input box's, on the output box) |
| `dyode api-token add\|list\|revoke` | tokens for this box's local API ([docs/API.md](docs/API.md)) |
| `dyode api-cert` | a self-signed TLS certificate for the local API |
| `dyode new-key` | create the shared message key |
| `dyode check` | validate config.yaml, the key and the installed unit; throughput hints |
| `dyode lidi-config` | print the lidi parameters derived from config.yaml |
| `dyode interfaces` | list NICs with MAC, state and IPv4 |
| `dyode systemd-unit --side in\|out` | print the systemd unit |
| `dyode firewall-rules` | print the output box's nftables rules |
| `dyode logs --archive` / `--stats` | weekly log archive / transfer statistics |

## Logs

Human log on stderr (journald) and in `/var/log/dyode-transfer/dyode.log`;
one JSON record per line in `transfer.jsonl` (same schema as the Python
version: `file_sent`, `file_stored`, `file_missing`, `batch_received`,
`logs_gap`, ...).  Rotation is logrotate's
([packaging/logrotate](packaging/logrotate/dyode-transfer)); a weekly timer
packs rotated files into one tarball (`dyode logs --archive`).

## Status

Tested here: unit tests (`cargo test`, 40 tests), an in-process loopback
test through real lidi engines, and both sides end to end over loopback
with a simulated lossy link -- see [CAVEATS.md](CAVEATS.md) before
deploying, in particular on rate limiting.  Not yet run on diode hardware.

## License

GPLv3, like DYODE (see [LICENSE](LICENSE)).  lidi is LGPL-3.0, by ANSSI.
