# `transport: air` -- one-way broadcast over radio

New in 2.2.0.  `air` carries DYODE's modules over a one-way, lossy,
broadcast bearer such as a raw-injection Wi-Fi link (wfb-ng), to any number
of receivers that never transmit.  It replaces lidi as the transport; the
modules, the DYL1 envelope (signature, replay window) and the output
handlers are the same.

**Status: tested in software only** (unit tests, loopback tests with a
lossy relay, and the real `dyode in`/`dyode out` binaries over loopback).
It has not yet run over a radio.

## When to use it -- and when not

`air` does not make a radio one-way.  The one-way guarantee still comes from
the wired DYODE diode: put the radio hop *outside* the trust boundary and
treat it as an untrusted, lossy bearer.

```
 protected OT ─▶ dyode in ══ wired diode ══▶ dyode out ─┐  radio relay box
                (lidi)                       (lidi)     │  (second config)
                                                        ▼
                                               dyode in (transport: air)
                                                        │ UDP 127.0.0.1:5601
                                                   wfb-ng udp_direct_tx
                                                        ≈≈≈ radio ≈≈≈
                       receivers:  wfb-ng udp_direct_rx ─▶ dyode out (air) ─▶ IT
```

- **One known receiver?**  You don't need `air`.  Forward the relay's
  delivered files over an ordinary two-way bridge (TCP, rsync).  It's
  simpler and uses less airtime.
- **Use `air` for** many receivers, receivers that join late or must stay
  silent, or links too poor or asymmetric for two-way use.  In simulation,
  broadcast beat per-receiver resend from about 5 receivers on a bad
  channel.

The relay box runs two DYODE instances with two config files: `dyode out`
(lidi, from the wired diode, writing to a folder) and `dyode in`
(`transport: air`, reading that folder).  Give each its own systemd unit
(`dyode systemd-unit --side ... -c FILE`).

## How it works

On the input box, each object is one DYL1 envelope: a file, a Modbus
snapshot, a log batch, a screen frame or a status message.  These are the
same bytes lidi would carry.  For each object:

1. **Chunks.**  The object is cut into chunks of `air.chunk` bytes (1 MiB).
   Each chunk is sealed with ChaCha20-Poly1305 and becomes one RaptorQ
   source block of `air.symbol`-byte symbols (about 820 symbols per chunk).
   Chunk 0 is the sealed **manifest**: module, length, chunk count,
   SHA-256 of the whole object, creation time and expiry.
2. **Carousel.**  The object is sent pass after pass:
   - Pass 1 sends the source symbols plus `air.repair`% repair symbols.
   - Every later pass sends K·(1+r) **new** repair symbols per chunk, so a
     receiver that missed pass 1 entirely can still finish from any one
     later pass.
   - Chunks go out in interleaving windows of at least `air.interleave`
     chunks, one symbol of each in turn, so a burst of loss costs each chunk
     only a few symbols.  No window is shorter than the depth: a short tail
     window would be wiped out by one fade.
   - At least 8 manifest symbols are spread through every pass (more on big
     objects: one per 128 packets), so a late joiner still learns what it
     is collecting.
   - Passes of one object are at least `air.pass_gap` seconds apart.
     Objects take turns window by window, so a small object's passes land in
     different fades.
3. **Packets.**  Each symbol travels in one DYB1 UDP datagram with
   54 bytes of overhead:
   - header: magic, version, flags, key id, endpoint, boot, object id,
     chunk index, chunk length, RaptorQ payload id;
   - the symbol;
   - a 16-byte HMAC-SHA256 tag.

   Everything is paced to `air.max_mbps`.

On each output box:

1. Packets are checked (format, key id, tag) before they cost a decode.
2. **Boot rule.**  Only the newest sender boot is accepted.  The boot id is
   the sender's start time in its high bits, so it sorts.  An older boot is
   refused; a newer one replaces it.  A boot claiming to start more than
   `air.clock_skew` in the future is refused.
3. **Done table.**  Object ids already delivered or refused in this boot
   are ignored.  The boot and the table are saved in
   `air.state/air-state.json`, so a restarted receiver delivers nothing
   twice and accepts no recording of an older boot.
4. Each chunk is decoded and opened as soon as enough symbols arrive, even
   before the manifest.  Chunks of multi-chunk objects go to
   `air.state/parts/<id>.part`, with a bitmap beside it, so **a restarted
   receiver resumes** where it was.
5. When every chunk is in and the SHA-256 matches the manifest, the object
   goes to the module's envelope parser.  That is the same parser and
   module handler lidi feeds, so the DYL1 signature and replay checks run
   as usual.

## Configuration

Both boxes read the same `config.yaml`:

```yaml
transport: air            # default: lidi
air:
  to: 127.0.0.1:5601      # input box: wfb-ng udp_direct_tx's input
  listen: 127.0.0.1:5600  # output box: where wfb-ng udp_direct_rx delivers
  max_mbps: 8             # whole packets; set below the radio's rate
  # symbol: 1280          # bytes per packet payload (64-3991); same on both
  # chunk: 1048576        # bytes per chunk; same on both boxes
  # repair: 20            # % repair symbols in pass 1
  # interleave: 16        # chunks interleaved against bursts
  # passes: 2             # for files and logs; 3 on bad channels / late joiners
  # pass_gap: 5           # seconds between passes of one object
  # max_age: 172800       # seconds an object stays valid
  # clock_skew: 300       # seconds of clock difference tolerated
  # spool: /var/lib/dyode/air-spool   # input box
  # spool_max_mb: 2048
  # state: /var/lib/dyode/air         # output box
  # max_objects: 64
  # max_buffer_mb: 256
  # max_partial_mb: 65536
  # orphan_timeout: 600
```

Relative `spool` and `state` paths are relative to config.yaml.  Every key
is listed in `config.example.yaml`.

**Per module**, `carousel` sets how long an object stays on air:

| `carousel:`        | Meaning                                              | Default for       |
|--------------------|------------------------------------------------------|-------------------|
| `{passes: N}`      | send N passes, then forget it                        | folder, logs (`air.passes`) |
| `{until: SECONDS}` | keep sending for that long (at least `air.passes`)   | --                |
| `latest`           | keep sending the newest object until a newer replaces it | modbus, screen, status |

**A key is required.**  `auth.required: false` does not apply to `air`:
every packet is signed and every chunk encrypted with keys derived from
`dyode.key`.

`dyode check` prints the rate files will really move at.
`dyode lidi-config` prints all the air parameters.

### Rate: what files really move at

The radio's rate is not the file rate.  With symbol T, repair r and P passes:

    file rate ≈ max_mbps × T / (T + 82) / (1 + r) / P

82 bytes is the DYB1 overhead plus UDP/IP headers.  At wfb-ng MCS1
(about 10.4 Mbit/s of symbols), 20% repair and 2 passes, that is about
4.3 Mbit/s: roughly 47 GB a day, or 10 hours for a 20 GB file.  Size
expectations on this.

## wfb-ng

Turn wfb-ng's FEC off (`fec_k = 1, fec_n = 1`; `air` does its own FEC) and
use one-way UDP streams.  These profiles are untested:

```ini
# /etc/wifibroadcast.cfg on the input (relay) box
[dyode_tx]
peer = 'listen://0.0.0.0:5601'
stream_tx = 0x01
fec_k = 1
fec_n = 1
# plus the radio settings: channel, mcs_index = 1, bandwidth = 20, stbc, ldpc

# on each receiver
[dyode_rx]
peer = 'connect://127.0.0.1:5600'
stream_rx = 0x01
```

Keep `air.symbol + 54` under wfb-ng's 4045-byte payload limit (the config
check enforces it).  Before trusting files to the link, measure it with
`dyode probe` (docs/PROBE.md), and set `repair`, `interleave` and `passes`
from the measured loss and fades.

## Security

What `air` gives you:

- **Outsiders can't inject.**  Packets not signed with the key are dropped
  before decoding.
- **Content stays private** from anyone without the key.
- **Integrity.**  Every chunk is authenticated, and the whole object is
  checked against its sealed manifest's SHA-256.  The envelope inside is
  verified again by the module.
- **No replays across restarts.**  The done table and the sortable-boot
  rule are saved to disk.  Expiry adds a clock-based limit.

What it doesn't:

- **Every receiver holds the key**, so any receiver can sign packets.  A
  compromised receiver can forge objects for the others, or inject junk
  symbols.  Junk costs time, not integrity: a poisoned chunk fails its seal
  and is collected again.  Signatures that receivers can check but not make
  (Ed25519 manifests), or sealing on the protected side, are future work.
- **One sender per key.**  Two input boxes sharing a key would take turns
  being "the newest boot".
- **Clocks matter.**  Keep both boxes on NTP or GPS time within
  `air.clock_skew`.
- The radio is broadcast: anyone in range can record it, and jam it.

## Crashes and restarts

- **Input box restart:** a new boot.  Objects still on the old carousel are
  not resent (files already handed over are retired; see `keep_sent_hours`
  for re-sending one).  Receivers drop what they were collecting from the
  old boot.
- **Output box restart:** partial multi-chunk objects resume from `.part`
  files.  Single-chunk objects (most messages) are collected again from
  later passes.
- An object that completes in the instant before a crash, while it is
  being handed to its module, can be lost.  As with lidi, a diode gives no
  proof of delivery.

## Limits and counters

Limits on the output box:

- `max_objects` objects at once; the one idle longest is dropped first.
- `max_buffer_mb` of symbols in decoders.
- `max_partial_mb` of `.part` files.  An object bigger than this is
  refused.
- An object without a manifest is forgotten after `orphan_timeout`
  seconds of silence.

Counters (in `dyode status --json`, `/v1/status` and `/metrics` as
`dyode_counter_total{counter=...}`):

| Counter | Meaning |
|---------|---------|
| `air_packets_sent` | input box |
| `air_objects_queued`, `air_objects_sent` | input box |
| `air_packets_received`, `air_packets_duplicate` | output box |
| `air_packets_bad_tag`, `air_packets_other_key`, `air_packets_malformed` | dropped before decoding |
| `air_packets_old_boot`, `air_boot_in_future`, `air_sender_boots` | boot rule |
| `air_chunks_bad_seal` | decoded chunk failed authentication (poisoned) |
| `air_objects_received` | complete and handed to the module |
| `air_objects_expired`, `air_objects_orphaned`, `air_objects_evicted`, `air_objects_superseded` | dropped (superseded: an older value of a `latest` module) |
| `air_objects_bad_manifest`, `air_objects_bad_hash`, `air_objects_too_big` | refused |
| `air_buffer_full`, `air_partial_full`, `air_send_errors` | limits hit |

## Differences from the design sketch

- **Cipher.**  The sketch named XChaCha20-Poly1305 with the boot in the
  nonce.  2.2.0 uses ChaCha20-Poly1305 under a per-boot key,
  HKDF(k_enc, salt = boot), with nonce = object id ‖ chunk index.  The
  guarantee is the same (no nonce reuse under a key), and it needs no new
  crypto dependency.
- **Content-addressed resume across sender restarts** is not in 2.2.0.
  Resume works across receiver restarts.
- The setup wizard does not ask about `air`: set it in config.yaml.
  The wizard keeps these keys when it imports a file.
- The systemd unit of an air box does not wait for a diode NIC
  (`BindsTo=`), since there isn't one.
- `dyode firewall-rules` for air keeps the air port to loopback when
  `air.listen` is a loopback address.
- A folder module's `bitrate` is ignored with `air`: `air.max_mbps` paces
  everything.

## Tests

- `cargo test`: codec and packet tags, sealing, manifest, schedule rules
  (balanced windows, fresh repair per pass, manifest spacing), done-table
  ranges, config.
- `tests/air.rs`, over loopback through a relay:
  - files and Modbus over a clean link, with another key's packets ignored;
  - four files over a Gilbert-Elliott channel with periodic fades (about
    17% loss);
  - a receiver crash mid-object that resumes from its `.part` file, under
    an insider's poisoned symbols;
  - a full replay of the recorded traffic after a restart, which delivers
    nothing;
  - a new sender boot, after which the old boot's recording is refused.
- Real binaries (release build) over loopback, on a 2-vCPU x86 sandbox
  with `max_mbps: 2000`: a 200 MB file arrived identical in 5.5 s, about
  290 Mbit/s of file.  The input box used about 40% of a CPU, the output
  box about 20%.  That is far above any radio rate here.  A Raspberry Pi
  has not been measured.
