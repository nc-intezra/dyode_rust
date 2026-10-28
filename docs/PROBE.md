# Measuring a link: `dyode probe`

`dyode probe` measures a one-way link before you trust it with video or
files.  iperf3 cannot do this: it needs a TCP control channel, and nothing
comes back through a diode or a receive-only radio.

* **`dyode probe send`** sends numbered, timestamped UDP packets at a fixed
  rate.  Each stream has its own thread and its own UDP port (stream *i* uses
  port + *i*), so several streams spread over CPU cores and NIC receive
  queues.
* **`dyode probe recv`** listens on the same ports, with one thread per port.
  It prints a line every second while traffic flows, then a report for each
  stream.
* **`dyode probe analyze`** replays a `recv --log` file through the same
  analysis.  Use it on field logs, or to try other `--fec-sim` settings
  without sending again.

It is part of the normal `dyode` binary, so the offline bundle already puts it
on both boxes.

## Quick start (through the diode)

On the **output** box, start the receiver first:

```sh
dyode probe recv --listen 0.0.0.0:7000 --streams 4 --json probe.json --log probe.csv
```

On the **input** box, send to the output box's diode address:

```sh
dyode probe send --to 192.168.100.2:7000 --streams 4 --mbps 400 --size 1400 --duration 60
```

* **Rates.** `--mbps` and `--pps` give the total across all streams.
  `--mbps` counts UDP payload bytes, the same way `lidi.max_mbps` does.
* **Packet size.** `--size` defaults to 1316 bytes (7 MPEG-TS packets).
* **Ports.** Use ports that lidi does not use.  DYODE's nftables rules only
  restrict the lidi ports (`lidi.ports`), so probe ports such as 7000 and up
  pass.
* **ARP.** The input box needs the output box's MAC pinned, because nothing
  can answer ARP through a diode.  `dyode in` pins it.  When DYODE is not
  running, pin it yourself:
  `ip neigh replace OUT_IP lladdr OUT_MAC dev IFACE nud permanent`.
* **Stopping.** The receiver stops on its own after every stream's end
  marker, or after `--idle` seconds of silence (default 10).  Ctrl-C stops it
  early and still prints the report.

## Reading the report

```text
stream 0  (run d8d194651ecfe0e9)
  packets   expected 30000, received 28914, lost 1086 (3.6200%); duplicates 0, reordered 0, late 0  [end marker seen]
  rate      6.169 Mbit/s received over 15.0 s; 400-byte packets sent at 2000.0 pkt/s
  bursts    271 bursts, mean 4.01 pkt, longest 27 pkt (13.5 ms)
            length:count  1:74 2:42 3:44 4:24 5:30 6:14 7:10 8:10 10:4 11:3 12:4 13:4 ... up to 27
            duration      <1ms:96 1-2ms:75 2-5ms:78 5-10ms:21 10-20ms:1
  gilbert   p 0.9373%  r 24.9540%   bench replay: tc qdisc add dev IFACE root netem loss gemodel 0.9373% 24.9540%
  delay     min 0.009 ms, median 0.074, p99 0.213, max 6.707; variation p99 0.204 ms; jitter 0.051 ms
  fec sim   T     50 ms r 0.25  (n 100 k 80): 4 of 300 blocks failed (1.3333%), 0 at risk, unrecovered 0.34667%
  fec sim   T    200 ms r 0.25  (n 400 k 320): 0 of 75 blocks failed (0.0000%), 0 at risk, unrecovered 0.00000%
```

That run went through a relay that dropped packets with a Gilbert model set
to p = 1% and r = 25%.  The probe measured p = 0.94% and r = 24.95%.

| Line | Meaning |
|---|---|
| `packets` | Loss is judged per sequence number.  `reordered` packets arrived after a later one but were still in time.  `late` packets arrived after their place in the window had been judged lost (raise `--reorder-ms` if this is not zero).  Without the end marker, packets lost at the very end cannot be counted. |
| `bursts` | Runs of consecutive lost packets: a histogram of lengths in packets, and of durations.  A duration is measured between the packets received on either side of the gap. |
| `gilbert` | Two-state loss model fitted to the run.  p = P(next packet lost \| this one arrived); r = P(next packet arrives \| this one lost).  The mean burst is 1/r, and the long-run loss is p/(p+r).  The `netem` text reproduces the pattern on a bench link. |
| `delay` | Receive time minus send time.  Absolute values need synchronised clocks (PTP or GPS); `variation` and `jitter` (RFC 3550) do not. |
| `fec sim` | What RaptorQ protecting every T of traffic with repair ratio r would have left.  Each group has n = T × packet rate packets, of which k = n/(1+r) are source.  A group fails with fewer than k received; with k or k+1 it is "at risk" (RaptorQ wants k+2 for ~99.99%).  `unrecovered` counts source packets lost in failed groups: the code is systematic, so packets that did arrive are still delivered. |

**The FEC simulation is only meaningful at the right packet rate.**  Send the
probe at the packet rate the protected stream will use: media plus repair,
for example `--mbps 32` for an 8 Mbit/s camera at r = 3.  Then each probe
packet stands in for one packet of that stream.

Defaults are `--fec-sim 100ms:0.25,200ms:1,200ms:3,500ms:1`, the settings in
the streaming design note.  `--fec-sim none` turns the simulation off.

## Options

| `probe send` | Default | |
|---|---|---|
| `--to HOST:PORT` | required | stream 0's destination; stream *i* goes to port + *i* |
| `--streams N` | 1 | parallel streams, one thread and port each |
| `--mbps X` or `--pps N` | 10 Mbit/s | total rate across streams (payload bytes) |
| `--size BYTES` | 1316 | UDP payload, 40 to 65507 |
| `--duration S` or `--count N` | 10 s | `--count` is packets per stream |
| `--burst N` | 1 | send N packets back to back per tick (same average rate): tests how the link takes bursts |
| `--bind IP` | any | source address |
| `--sndbuf BYTES` | system | socket send buffer |
| `--json FILE` | | sender report as JSON |

| `probe recv` | Default | |
|---|---|---|
| `--listen IP:PORT` | 0.0.0.0:7000 | stream 0's port; stream *i* on port + *i* |
| `--streams N` | 1 | |
| `--idle S` | 10 | stop after this much silence (counted from the first packet) |
| `--duration S` | none | stop after this long regardless |
| `--report S` | 1 | seconds between live lines; 0 for none |
| `--reorder-ms MS` | 500 | how late a packet may arrive and still count |
| `--fec-sim T:R,...` | see above | `200ms:1`, `0.5s:3`, `200:0.25`; `none` to disable |
| `--rcvbuf BYTES` | 32 MiB | receive buffer per port; the report warns if the kernel gave less (raise `net.core.rmem_max`, or run as root) |
| `--keep` | off | keep listening after a run ends, with one report per run (field soaks) |
| `--log FILE` | | one CSV line per packet, for `probe analyze` |
| `--json FILE` | | every report as JSON, plus per-port counters (damaged and foreign datagrams, buffer size) |
| `--quiet` | off | no live lines or per-run text |

`probe analyze LOG [--reorder-ms MS] [--fec-sim ...] [--json FILE]` replays a
log in arrival order.

The log format, one line per packet, is:
`stream,run,seq,send_ns,recv_ns,len,interval_us,kind`.  `kind` is `d` for
data and `e` for an end marker.  The log grows by about 80 bytes per packet,
which is about 5.5 MB/s at 70,000 packets per second, so use it for field
runs at radio rates rather than for hours at full diode speed.

## Per hardware class

* **Wired diode.** Run at 25, 50, 75 and 90% of the planned rate with
  `--streams` equal to the lidi port count.  Then run the same commands while
  a large lidi file transfer is under way, to see whether video and files can
  share the link.
* **Raw WiFi (WFB-ng).** Point `--to` at WFB-ng's UDP input on the
  transmitting host, and `--listen` at its UDP output on the receiving host.
  Keep `--size` at or under WFB-ng's 3,993-byte limit.  Run once with
  WFB-ng's own FEC at its lowest setting, so you see the raw channel, and
  once with its default FEC.
* **SDR (DVB-S2).** The probe needs an IP path, so it applies once the
  datagrams are carried as IP over DVB-S2 (MPE, ULE or GSE).  Before that,
  measure loss from MPEG-TS continuity counters.
* **LoRa.** Keep `--size` within the radio's payload (255 bytes or less,
  minus the bridge's own header) and the rate within airtime: `--pps 2
  --size 200` is already about 3.2 kbit/s.  Use spans of seconds for the
  simulation (`--fec-sim 5s:1,30s:1`) and a larger `--reorder-ms` if the
  bridge queues packets.

Every probe packet carries a CRC-32.  A datagram that arrives damaged (a radio
bridge that passes bit errors) is counted per port as `corrupt` and treated
as lost.

## Notes

* **The sender spins.** For precise pacing it busy-waits for the last
  ~50 µs before each packet, so expect one busy core per stream at high
  rates.  If `worst lateness` is above a few milliseconds, the sender could
  not keep up: use more `--streams`.
* **Probe packets are not authenticated.**  This is a measurement tool for
  trial links; never point it at a production DYODE port.
* **Tested here:** unit tests, a loopback run of 4 streams at 2 Gbit/s total
  with no loss, and a Gilbert-model relay whose p, r and loss the probe
  recovered.  Not yet run on diode or radio hardware.
