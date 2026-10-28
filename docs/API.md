# The local API

From 2.1.0 each DYODE box can serve its own status, read-only, to
Prometheus, Zabbix or anything that speaks HTTPS.  This is phase 1 of the
API plan; sending data from OT to IT through the input box's API comes in
2.2.0.

The diode stays the only link between the boxes:

- each box's API listens on that box's **management** network card, never on
  the diode card (DYODE refuses to start otherwise);
- the output box reports the input box from the **status message** the
  input box sends through the diode every `lidi.heartbeat` seconds, signed
  like everything else.  Nothing on the IT side ever reaches the input box.

The full description is [api/openapi.yaml](api/openapi.yaml), also served by
every box at `/v1/openapi.json`.

## Endpoints

| Path | Token | What |
|---|---|---|
| `GET /v1/health` | none | up, side, version, uptime |
| `GET /v1/status` | read | this box; on the output box, the input box too, with the age of its last status, and warnings |
| `GET /metrics` | read | the same as Prometheus text |
| `GET /v1/check` | read | `dyode check` for this box, as JSON |
| `GET /v1/openapi.json` | none | the API description |

Every answer closes the connection.  `HEAD` works on every path.

## Setting it up

The guided setup (`dyode setup`, run by install.sh) asks for it on its
"Local API" screen: it writes the `api:` section, makes a certificate and a
first token, and shows the token once at the end.  By hand:

```bash
# on the box, as root
sudo dyode api-cert --name dyode-out --ip 192.168.50.4     # /etc/dyode/api.crt, api.key
sudo dyode api-token add prometheus --scope read           # prints the token once
```

```yaml
# config.yaml (the same file on both boxes; each box uses its own side)
api:
  in:
    listen: 10.20.0.5:8443          # input box, OT management NIC
    tls: {cert: api.crt, key: api.key}
  out:
    listen: 192.168.50.4:8443       # output box, IT NIC
    tls: {cert: api.crt, key: api.key}
    # tokens: api-tokens            # default: beside config.yaml
    # max_clients: 32               # simultaneous connections
```

Restart DYODE (`systemctl restart dyode-out`) and check:

```bash
curl --cacert api.crt https://192.168.50.4:8443/v1/health
curl --cacert api.crt -H "Authorization: Bearer $TOKEN" https://192.168.50.4:8443/v1/status
sudo dyode check -c /etc/dyode/config.yaml --side out      # lists the API and its tokens
```

Rules the loader enforces:

- `listen` names one address: not `0.0.0.0`, and not the box's diode address.
  At start, an address that belongs to the diode NIC is refused as well.
- `tls` is required.  `tls: off` is allowed only on `127.0.0.1`, behind your
  own reverse proxy.
- The output box's nftables rules (install.sh) also drop the API port on the
  diode NIC.

## Tokens

`dyode api-token add NAME --scope read | list | revoke NAME`.  Tokens are
stored as SHA-256 hashes in `api-tokens` beside config.yaml (mode 600) and
are sent as `Authorization: Bearer <token>`.  The file is per box: it is not
part of config.yaml and is never copied to the other box, so an IT token is
worthless on the OT box.  Refused requests become `api_refused` records in
transfer.jsonl and count in `dyode_counter_total{counter="api_refused"}`.
Client certificates come in a later phase.

## Certificates

`dyode api-cert` makes a self-signed P-256 certificate with openssl (an OS
package install.sh installs), valid 825 days, and prints its SHA-256
fingerprint.  Give clients the certificate itself (`--cacert`, Prometheus
`tls_config.ca_file`, Zabbix "SSL certificate" settings).  A certificate
from your own CA works the same way: point `tls.cert` at the chain and
`tls.key` at the key.

## Metrics

Every series has `side="in"` or `side="out"`.  On the output box both sides
appear; on the input box only `in`.

| Metric | Type | Meaning |
|---|---|---|
| `dyode_info{version,transport_hash,config}` | gauge | 1 per box known |
| `dyode_status_age_seconds` | gauge | seconds since the box's status was taken (the input box's, as seen from the output box) |
| `dyode_status_received` | gauge | 0 until the output box has heard from the input box |
| `dyode_uptime_seconds` | gauge | since DYODE started |
| `dyode_config_match` | gauge | 1 when both boxes run the same transport configuration |
| `dyode_module_up{module,type}` | gauge | 1 running, 0 restarting |
| `dyode_module_restarts_total{module}` | counter | module restarts |
| `dyode_events_total{module,event}` | counter | transfer.jsonl events: `file_sent`, `file_stored`, `file_rejected`, `file_missing`, `logs_gap`, ... |
| `dyode_bytes_total{module,direction}` | counter | file bytes sent (in) or stored (out) |
| `dyode_last_activity_timestamp_seconds{module}` | gauge | time of the module's last event |
| `dyode_free_bytes{module,dir}` | gauge | free space under the module's folder |
| `dyode_counter_total{counter}` | counter | `lidi_transfers_aborted`, `api_refused`, `api_busy`, ... |
| `dyode_static_arp_ok` | gauge | the input box's static ARP entry is in place |

Counters start at zero when DYODE starts; use `rate()` / `increase()`.

The alert that matters most: the input box's status stops arriving, which
means the input box, its NIC, the diode or the output box's receiver has
failed.

```yaml
- alert: DyodeInputSilent
  expr: dyode_status_age_seconds{side="in"} > 30 or dyode_status_received{side="in"} == 0
  for: 1m
```

Ready-made rules, a Grafana dashboard and a Zabbix 7 template are in
[nc-intezra/dyode_monitoring](https://github.com/nc-intezra/dyode_monitoring).

## `dyode status`

Each box also writes its status (and on the output box the input box's) to
`/run/dyode/status-<side>.json` every heartbeat.  `dyode status` prints it,
`dyode status --json` gives it to scripts.  No API needed.

## Upgrading

Both boxes need 2.1.0: the status channel uses one more lidi endpoint (the
one after the last module), which a 2.0.x output box does not know.  With
the default `lidi.max_clients`, DYODE adds a slot for it; an explicit
`max_clients` should be raised by one (`dyode check` says so).
