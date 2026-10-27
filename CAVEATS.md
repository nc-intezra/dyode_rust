# Caveats

Known limitations, and what to do about them.

## Pace the sender: lidi sends in bursts

lidi encodes a RaptorQ block (220 KB by default, ~150 packets at MTU 1500)
and sends all its packets at once, at line rate.  A receiver, switch or
media converter that cannot absorb the burst drops packets.  A block that
loses more packets than its repair packets can replace is lost.  **Losing a
block aborts every transfer in flight**, so a large file is lost along with
the Modbus poll that happened to share that block's time window.

That is why `lidi.max_mbps` defaults to **100 Mbit/s**.  In testing over a
simulated link, unpaced sending at ~700 Mbit/s lost 1 file in 3 at only
0.5% random loss (the losses were buffer overruns in bursts).  Paced at
200 Mbit/s, the same link delivered every file at 0.5% and at 2% random loss,
with zero lost blocks.

To go faster:

1. `sudo ./install.sh --side in|out --tuning-only` (socket buffers of 64 MB:
   lidi asks for one block's worth of packets, ~30 MB, and warns
   `UDP socket recv buffer may be too small` otherwise).
2. Raise `lidi.max_mbps` step by step while watching the output box for
   `lidi: lost block` and `file_rejected ... incomplete`.
3. On multi-core boxes, add `lidi.ports` (one encode/decode thread each);
   `dyode check` suggests a list for the box's CPU count.
4. Raise `lidi.repair` on a lossy link (each point costs ~1% of bandwidth).
5. ANSSI's own guidance on tuning lidi, including `tc` rate limiting, is in
   lidi's documentation (`doc/tweaking.rst`, `doc/tc_ratelimiter.rst`).

## Nothing can be re-requested

Unchanged from every DYODE version: there is no return path.  A file lidi
cannot rebuild is reported on the output box (`file_rejected`, then
`file_missing` in the batch summary) and must be re-sent by copying it back
from `.dyode_sent` on the input box.  If the transfer's very first block is
lost, the output box never learns the file existed.  The batch summary
names it, unless the summary is lost too.  Compare `file_sent` (input) with
`file_stored` (output) for certainty.

## Disk speed on the output box

Received data is written as it arrives.  If the disk cannot keep up, lidi's
per-transfer queue fills (`lidi.client_queue_size` blocks, ~225 MB by
default) and lidi drops the transfer, rather than run out of memory.  Keep
`max_mbps` below the sustained write speed of the output disk (measure with
`dd if=/dev/zero of=<out>/test bs=1M count=4096 oflag=direct`).  The
installer's writeback settings (`vm.dirty_*`) keep writes continuous.

## The firewall matters more with lidi

Every DYODE envelope is signed, so forged *content* is refused (tested: a
second sender with another key had every file, update, frame and log batch
refused).  lidi's own packets are not authenticated, though.  Forged lidi
packets with a new session id make the receiver abort the transfers in
flight, and a flood costs CPU.  The nftables rules installed on the output
box (`install.sh --side out --firewall-only`) drop diode-port traffic that
does not come from the input box on the diode NIC.  Re-apply them whenever
`lidi.ports` or the addresses change.

## Ordering

Several transfers are in flight at once, so they can complete out of order.
Modbus updates carry a sequence number and an older one never overwrites a
newer one; log batches are re-ordered by sequence.  Files are independent.

## Tested on diode hardware

2.0.2 has run on a real diode (reported 2026-10-27): a single 20 GB file
crossed intact at **800.2 Mbit/s** (the `mbps` of its `batch_sent` event),
noticeably faster than DYODE (Python) with udpcast, with this `lidi:`
section on both boxes:

```yaml
lidi:
  ports: [5000, 5001, 5002, 5003]
  max_mbps: 800
```

The cap, not the link, set that speed: `max_mbps` paces the file's bytes,
and the bucket's 50 ms starting burst accounts for the extra 0.2 Mbit/s over
a ~200 s transfer.  On the wire, RaptorQ repair packets (`repair` 5% plus
lidi's 2%) and packet headers come on top, so the link carried roughly
860 Mbit/s.

That is one data point for one link: raise `max_mbps` on your own hardware
only as described above.  Not yet measured on hardware: many small files
with `parallel_files` above 1, and where the link starts losing blocks above
800 Mbit/s.  The field fixes recorded in the Python version's CAVEATS
(writeback stalls on LUKS disks, long transfers and batch timeouts) are
addressed by design here (continuous writes, activity-based batch timeout);
the 20 GB transfer exercised the long-transfer case.
