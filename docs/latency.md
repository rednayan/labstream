# Latency and CPU

This page gives how to measure the time from a push to a pull, and the
results of the first measurement. Run the measurement again before you claim
that a change makes the library faster.

## The instrument

`conformance/oracle/latency.c` is one C program for the C ABI. It runs as an
outlet or as an inlet. The runner `conformance/oracle/latency.py` links it
once to each library, then runs an outlet process and an inlet process for
each cell.

- **The clock.** The program reads its own `CLOCK_MONOTONIC`, not the clock
  of the library. The outlet gives the push time as the timestamp of each
  sample. The inlet does no post-processing, and records the pull time minus
  the timestamp.
- **The chunk.** All samples of one push call get the push time. The latency
  of a chunk is thus the latency of its last sample to leave.
- **The pull.** The inlet calls `lsl_pull_sample_f` with a timeout of 3 s. The
  call returns when a sample arrives, so no poll interval adds latency.
- **Loss.** Channel 0 carries the sample number. The inlet counts lost,
  repeated, and reordered samples.
- **The library.** Each process reports the file of the library that the
  loader picked. A stale `RUNPATH` once made a test use the wrong library.
- **CPU.** Each process reports its user and system time, as a percentage of
  one core, over the time that it sends or receives.
- **The schedule.** The outlet waits on an absolute schedule, and reports how
  late each push is. This lag is not part of the latency.

## The control

The control pair is liblsl to liblsl. With `--control`, the runner also runs
that pair with timestamps that are 1000 µs early. The table must show a delay
that is 1000 µs larger. On the run below, the delay showed as 966 µs.

## How to run it

Run these commands from `conformance/`. The oracle build must exist
(`conformance/oracle/README.md`).

```bash
oracle/latency.py --control
```

To compare branches, build the C library of each branch in its own worktree
and target directory. Then give each directory to the runner:

```bash
oracle/latency.py --lib oracle=.build/install/lib --lib main=DIR1 --lib fix=DIR2 --no-mixed --control
```

Do not copy a branch build into `.build/rustlib`. Other scripts read that
directory.

## The first results

Machine: AMD Ryzen 5 PRO 3500U, 8 threads, Linux 6.16, loopback, no other
load on purpose. Each cell is the median of 3 runs of 10 s, with the range of
the 3 runs in brackets. The first second of each run is not measured.

Libraries:

- `oracle`: liblsl at `e651023c`.
- `main`: labstream `a912cef`.
- `fix`: branch `fix/network-robustness` at `e01e998` (pull request #9).
- `quick`: branch `perf/quick-wins` at `d6880e4` (pull request #10).

| outlet -> inlet | load | p50 µs | p99 µs | p99.9 µs | max µs | lost | CPU outlet % | CPU inlet % |
|---|---|---|---|---|---|---|---|---|
| oracle -> oracle | 512 Hz x 8, chunk 1 | 102 (93-142) | 327 (201-501) | 419 (231-624) | 2150 | 0 | 5.7 | 3.4 |
| main -> main | 512 Hz x 8, chunk 1 | 96 (94-96) | 212 (204-217) | 465 (260-1808) | 2932 | 0 | 4.2 | 3.2 |
| fix -> fix | 512 Hz x 8, chunk 1 | 96 (94-97) | 204 (200-235) | 254 (246-1011) | 3218 | 0 | 4.2 | 3.2 |
| quick -> quick | 512 Hz x 8, chunk 1 | 94 (92-98) | 204 (194-239) | 419 (308-557) | 2382 | 0 | 4.1 | 3.2 |
| oracle -> oracle | 512 Hz x 8, chunk 16 | 246 (224-247) | 646 (599-648) | 742 (729-779) | 780 | 0 | 0.7 | 0.4 |
| main -> main | 512 Hz x 8, chunk 16 | 290 (287-297) | 667 (614-1054) | 1025 (703-2150) | 2151 | 0 | 2.0 | 1.5 |
| fix -> fix | 512 Hz x 8, chunk 16 | 220 (218-238) | 514 (464-514) | 617 (480-624) | 626 | 0 | 1.1 | 0.5 |
| quick -> quick | 512 Hz x 8, chunk 16 | 220 (214-236) | 531 (495-536) | 696 (676-754) | 756 | 0 | 1.1 | 0.5 |
| oracle -> oracle | 4000 Hz x 64, chunk 1 | 71 (70-72) | 116 (114-117) | 152 (152-164) | 2803 | 0 | 30.4 | 18.4 |
| main -> main | 4000 Hz x 64, chunk 1 | 73 (73-75) | 116 (114-119) | 154 (149-172) | 755 | 0 | 21.2 | 18.4 |
| fix -> fix | 4000 Hz x 64, chunk 1 | 75 (74-76) | 121 (116-165) | 168 (159-2487) | 5199 | 0 | 21.5 | 18.2 |
| quick -> quick | 4000 Hz x 64, chunk 1 | 73 (71-74) | 125 (105-145) | 224 (157-1681) | 3985 | 0 | 20.5 | 17.5 |
| oracle -> oracle | 4000 Hz x 64, chunk 32 | 318 (317-320) | 651 (646-667) | 768 (764-844) | 876 | 0 | 3.3 | 1.7 |
| main -> main | 4000 Hz x 64, chunk 32 | 382 (381-406) | 795 (756-852) | 1108 (892-1309) | 2916 | 0 | 9.3 | 10.8 |
| fix -> fix | 4000 Hz x 64, chunk 32 | 343 (310-355) | 664 (626-699) | 810 (796-1063) | 3097 | 0 | 3.1 | 2.2 |
| quick -> quick | 4000 Hz x 64, chunk 32 | 328 (322-339) | 658 (657-671) | 829 (797-832) | 892 | 0 | 3.2 | 2.2 |

### What the results show

- **A chunk push on `main` costs 3 to 5 times the CPU of liblsl.** At
  4000 Hz x 64 channels in chunks of 32, `main` uses 9.3 % and 10.8 %. The
  `fix` branch uses 3.1 % and 2.2 %, near liblsl. The `fix` branch writes each chunk
  with one write, and locks the inlet queue once for each read.
- **A chunk push on `main` is also slower.** The median is 382 µs on `main`,
  343 µs on `fix`, and 318 µs on liblsl.
- **A push of one sample gives the same latency in all four builds.** The
  median is 70 to 100 µs. At 4000 Hz x 64 channels, the labstream outlet uses
  about 21 % of one core, and the liblsl outlet uses 30 %.
- **No build lost a sample.**
- **The maximum is noise.** A single value of 2 to 5 ms comes from the
  scheduler of the operating system. The p99.9 column is a better measure of
  the tail.
- **The `quick` branch does not change the latency of the data path.** It
  changes the start of a connection and the end of a dead connection. This
  benchmark measures neither.

## The order of the runs

The order of the runs can move a result. In one run of `seq` against
`quick`, the `seq` rows ran last in each group. At 512 Hz x 8 channels,
`seq` then showed a median of 138 µs against 97 µs. With `seq` first, both
gave 91 µs, and the p99 values were 188 µs and 190 µs. A difference at a low
rate is thus not a result until it stays after a change of order.

On the branch `feat/seq`, the `seq/1` feature does not change the latency
or the CPU at 4000 Hz x 64 channels:

| outlet -> inlet | load | p50 µs | p99 µs | CPU outlet % | CPU inlet % |
|---|---|---|---|---|---|
| quick -> quick | 4000 Hz x 64, chunk 1 | 73 (72-73) | 103 (101-104) | 20.7 | 17.5 |
| seq -> seq | 4000 Hz x 64, chunk 1 | 73 (72-75) | 99 (97-113) | 20.5 | 17.7 |
| quick -> quick | 4000 Hz x 64, chunk 32 | 356 (348-366) | 711 (677-712) | 3.3 | 2.4 |
| seq -> seq | 4000 Hz x 64, chunk 32 | 350 (332-352) | 648 (609-654) | 3.2 | 2.3 |

## The cost of `resume/1`

On the branch `feat/resume`, the outlet keeps a history when an inlet agrees
to `resume/1`. Each push then copies each sample into the history. The
first build copied the samples before it gave them to the consumer queues.
At 4000 Hz x 64 channels, chunk 32, the median then grew from about 260 µs
to about 340 µs, in both orders of the runs.

The build that this page describes gives the samples to the queues first,
then copies them into the history, under the same lock. The rows below come
from two runs, one in each order. `seq` is `main` with `seq/1` only. `resume`
is the first build. `fixed` is the build with the queues first.

| outlet -> inlet | load | p50 µs | p99 µs | CPU outlet % | CPU inlet % |
|---|---|---|---|---|---|
| seq -> seq | 4000 Hz x 64, chunk 32 | 240 (216-268), 278 (278-293) | 580, 645 | 2.4, 2.8 | 1.9, 2.1 |
| resume -> resume | 4000 Hz x 64, chunk 32 | 326 (324-352), 357 (350-366) | 661, 707 | 3.4, 3.8 | 2.0, 2.0 |
| fixed -> fixed | 4000 Hz x 64, chunk 32 | 261 (249-281), 276 (274-294) | 609, 610 | 3.9, 4.3 | 1.9, 2.1 |
| seq -> seq | 4000 Hz x 64, chunk 1 | 76 (75-78), 73 (72-77) | 114, 111 | 21.4, 20.4 | 18.4, 17.6 |
| fixed -> fixed | 4000 Hz x 64, chunk 1 | 74 (70-76), 74 (70-76) | 109, 108 | 22.1, 21.1 | 17.4, 18.0 |

- **Latency.** With the queues first, the latency of `fixed` stays in the
  range of `seq`.
- **CPU.** The copy into the history still costs the outlet about 1.5
  points of one core at chunk 32, and about 0.7 points at chunk 1. One copy
  of a sample at 64 channels takes about 1.9 µs. The flat typed API with
  shared samples (`docs/roadmap.md`, step 8) removes this copy.
- **512 Hz x 8 channels.** No difference stayed after a change of order.
- **A peer without `resume/1`.** The outlet keeps no history. The rows of
  `seq` against the first build stayed in the range of `seq` against `seq`.

## A slow, lossy, or cut link

`conformance/oracle/lossylink.py` runs the same program over a `veth` pair
between two network namespaces, with one `netem` rule on each end. It needs
no root, and it does not change the interfaces of the host.
`docs/roadmap.md`, step 0, gives the method and the rules. Each rule applies
in both directions. The load is 512 Hz x 8 channels, one sample for each
push. Each cell is the median of 3 runs, with the range in brackets. The
runs alternate the order of the libraries.

**A correction.** The first runs of 2026-11-21 had a fault in the runner.
`tc qdisc replace` keeps each parameter that the new rule does not give, so
the rate limit of the rule `1 Mbit/s` stayed on the link for the two cuts
after it. The rows of the cuts in that version of this page thus came from
a link of 1 Mbit/s. Their medians of 0.5 s to 3.5 s, and the note "not
explained", came from that limit. The runner now starts each rule from an
empty root. The table below is the second run, with the corrected runner,
on the branch `feat/dgram`.

The libraries: liblsl, and labstream `main` with the first default history
of 10 s (`081f7fa`) and with the default of 30 s (`dfb6c39`):

| rule | library | p50 ms | p99 ms | max ms | lost in each run |
|---|---|---|---|---|---|
| clean | liblsl | 0.1 (0.1-0.1) | 0.4 (0.4-0.5) | 9.2 | 0, 0, 0 |
| clean | history 10 s | 0.1 (0.1-0.1) | 0.2 (0.2-0.2) | 3.2 | 0, 0, 0 |
| clean | history 30 s | 0.1 (0.1-0.1) | 0.4 (0.4-0.4) | 0.7 | 0, 0, 0 |
| delay 20 ms | liblsl | 20.1 (20.1-20.1) | 20.3 (20.2-20.3) | 20.7 | 0, 0, 0 |
| delay 20 ms | history 10 s | 20.1 (20.1-20.1) | 20.3 (20.3-20.3) | 23.7 | 0, 0, 0 |
| delay 20 ms | history 30 s | 20.1 (20.1-20.1) | 20.2 (20.2-20.3) | 23.2 | 0, 0, 0 |
| delay 20 ms, loss 1 % | liblsl | 22.2 (22.2-22.8) | 71.9 (67.2-84.3) | 118.3 | 0, 0, 0 |
| delay 20 ms, loss 1 % | history 10 s | 23.2 (22.6-23.4) | 75.6 (68.1-76.2) | 115.3 | 0, 0, 0 |
| delay 20 ms, loss 1 % | history 30 s | 22.4 (21.3-23.4) | 69.0 (68.1-70.0) | 91.5 | 0, 0, 0 |
| 1 Mbit/s | liblsl | 1.1 (1.1-1.1) | 1.3 (1.2-1.4) | 1.6 | 0, 0, 0 |
| 1 Mbit/s | history 10 s | 0.9 (0.9-0.9) | 1.0 (1.0-1.2) | 3.8 | 0, 0, 0 |
| 1 Mbit/s | history 30 s | 0.9 (0.9-0.9) | 1.2 (1.0-1.2) | 1.7 | 0, 0, 0 |
| cut of 3 s | liblsl | 0.1 (0.1-0.2) | 2990.2 (2975.9-3000.6) | 3141.2 | 0, 0, 0 |
| cut of 3 s | history 10 s | 0.1 (0.1-0.1) | 2997.9 (2993.8-3004.8) | 3143.6 | 0, 0, 0 |
| cut of 3 s | history 30 s | 0.1 (0.1-0.1) | 2974.4 (2966.4-3024.7) | 3165.1 | 0, 0, 0 |
| cut of 10 s | liblsl | 1419.3 (1395.5-1436.3) | 13047.5 (13042.1-13069.3) | 13309.5 | 0, 0, 0 |
| cut of 10 s | history 10 s | 0.2 (0.1-0.2) | 9776.9 (9770.3-9779.1) | 10000.3 | 921, 937, 454 |
| cut of 10 s | history 30 s | 0.3 (0.2-0.4) | 11592.9 (10666.3-11598.1) | 11838.1 | 0, 0, 0 |

- **The control.** The rule of 20 ms adds 20.0 ms to the median of each
  library.
- **Loss of 1 %.** TCP sends each lost packet again, so no sample is lost.
  Each lost packet holds back the samples after it. The p99 grows from
  20 ms to about 70 ms for each library.
- **1 Mbit/s.** The load needs about 0.2 Mbit/s, so it fits. The time to
  send each packet adds about 1 ms.
- **The cut of 3 s.** The keepalive limit is 6 s, so no library closes the
  connection. TCP sends the data again after the cut. No sample is lost,
  and the longest wait is about 3.1 s. `resume/1` does not act, because the
  connection does not end.
- **The cut of 10 s.** liblsl does not see the cut, because its watchdog
  waits 15 s. TCP waits longer between two attempts after each failure, so
  the data arrives about 13 s after the start of the cut, and no sample is
  lost. labstream closes the connection after about 6 s, and connects again
  after the end of the cut. A history of 10 s then loses 454 to 937 samples
  on a normal link too, so the corrected runs confirm decision 11 of
  `docs/roadmap.md`. A history of 30 s loses none, and the longest wait is
  about 11.8 s.

## `dgram/1` over a slow, lossy, or cut link

The same runner, with one build of the branch `feat/dgram` as two
libraries: `tcp` with the default configuration, and `udp` with
`labstream.Features = {seq, resume, dgram}`. These results are after the
change of 2026-11-21 (adaptive copies, UDP resends of small gaps, relaxed
keepalive on UDP, and no give-up during a cut). 3 runs for each cell
(2 runs for `UDP stops 5 s, then returns`):

| rule | load | tcp p99 ms | udp p99 ms | lost tcp | lost udp |
|---|---|---|---|---|---|
| clean | 512 Hz x 8, chunk 1 | 0.2 (0.2-0.3) | 0.2 (0.2-0.3) | 0, 0, 0 | 0, 0, 0 |
| delay 20 ms | 512 Hz x 8, chunk 1 | 20.3 (20.2-20.4) | 20.5 (20.3-20.5) | 0, 0, 0 | 0, 0, 0 |
| delay 20 ms, loss 1 % | 512 Hz x 8, chunk 1 | 71.4 (68.4-73.2) | 22.0 (20.5-22.0) | 0, 0, 0 | 0, 0, 0 |
| delay 20 ms, loss 1 % | 512 Hz x 8, chunk 16 | 91.5 (20.9-91.9) | 28.9 (25.3-29.2) | 0, 0, 0 | 0, 0, 0 |
| delay 20 ms, loss 1 % | markers, 200 each second | 70.2 (69.2-70.8) | 20.9 (20.7-25.0) | 0, 0, 0 | 0, 0, 0 |
| delay 20 ms, loss 1 % | 4000 Hz x 64, chunk 32 | 4486.2 (4323.9-5371.3) | 84.3 (71.6-84.7) | 12192, 16810, 12528 | 0, 0, 0 |
| 1 Mbit/s | 512 Hz x 8, chunk 1 | 1.1 (1.1-1.2) | 1.2 (1.1-1.3) | 0, 0, 0 | 0, 0, 0 |
| cut of 3 s | 512 Hz x 8, chunk 1 | 2979.5 (2979.1-2985.8) | 2930.8 (2928.6-2930.8) | 0, 0, 0 | 0, 0, 0 |
| cut of 10 s | 512 Hz x 8, chunk 1 | 11574.4 (11546.3-11593.3) | 11164.6 (11150.1-11173.2) | 0, 0, 0 | 0, 0, 0 |
| cut of 10 s at 1 Mbit/s | 512 Hz x 8, chunk 1 | 11697.4 (11696.7-11699.2) | 11287.7 (11271.1-11299.6) | 0, 0, 0 | 0, 0, 0 |
| UDP stops, TCP works | 512 Hz x 8, chunk 1 | 0.3 (0.3-0.3) | 2858.9 (2858.8-2859.4) | 0, 0, 0 | 0, 0, 0 |
| UDP stops 5 s, then returns | 512 Hz x 8, chunk 1 | not run | 2562.3 (2562.2-2562.3) | not run | 0, 0 |
| loss 30 % after 2 s | 512 Hz x 8, chunk 1 | 4989.2 (4352.3-9359.3) | 49.3 (32.0-131.6) | 4792, 4482, 4207 | 0, 0, 0 |
| loss 50 % after 2 s | 512 Hz x 8, chunk 1 | 121.8 (89.5-4480.9) | 281.0 (265.4-376.7) | 5366, 5064, 5367 | 0, 0, 0 |

- **Loss of 1 %.** The p99 of `dgram` stays below 29 ms for each load with
  small samples, with no lost sample.
- **4000 Hz x 64 channels.** TCP cannot carry 8 Mbit/s with 1 % loss and a
  round trip of 40 ms, so it loses over 12000 samples. `dgram` loses none.
- **1 Mbit/s.** On a clean link, `C` stays at 0, so `dgram` sends no copy
  and matches TCP (1.2 ms against 1.1 ms).
- **The cuts.** `dgram` loses no sample on `cut3`, `cut10`, and `cut10rate`,
  and waits a little less than TCP. `cut3` stays on UDP.
- **UDP stops.** The inlet asks for TCP after 3 s with no datagram, with no
  lost sample. With `udpback`, the outlet tries UDP again after 30 s, also
  with no lost sample.
- **Loss of 30 % and 50 % after 2 s.** TCP loses 4200 to 5367 samples.
  `dgram` stays on UDP, scales `C` up, and repairs small gaps on UDP. It
  lost no sample in 3 runs at 30 % and in 3 traced runs at 50 %. One earlier
  run at 50 % stalled after 34 samples (`6110, 0, 1` lost in that batch).
- **Heavy loss from the start.** At 30 % from the start, `dgram` lost
  `0, 0, 759` samples, TCP gave `no connect, 4129, 5939`, and liblsl lost
  `4225, 3846, 5507`. At 50 % from the start, discovery or handshake failed
  in most runs for all three libraries.

### `dgram/1` on loopback

`oracle/latency.py` compares `main` (`dfb6c39`) against `feat/dgram` with
`labstream.Features = {seq, resume, dgram}`, 3 runs of 10 s for each cell:

| outlet -> inlet | load | p50 µs | p99 µs | p99.9 µs | max µs | lost | CPU outlet % | CPU inlet % |
|---|---|---|---|---|---|---|---|---|
| main -> main | 512 Hz x 8, chunk 1 | 175 (169-186) | 401 (400-414) | 549 (536-615) | 2590 | 0 | 6.0 | 4.5 |
| dgram -> dgram | 512 Hz x 8, chunk 1 | 158 (150-159) | 385 (369-405) | 515 (494-547) | 3987 | 0 | 5.9 | 3.6 |
| main -> main | 512 Hz x 8, chunk 16 | 288 (262-290) | 571 (526-580) | 742 (621-763) | 765 | 0 | 1.6 | 0.6 |
| dgram -> dgram | 512 Hz x 8, chunk 16 | 223 (218-223) | 513 (475-662) | 829 (640-1368) | 1370 | 0 | 2.0 | 0.8 |
| main -> main | 4000 Hz x 64, chunk 1 | 65 (65-66) | 173 (172-214) | 1926 (847-2463) | 5550 | 0 | 21.1 | 16.5 |
| dgram -> dgram | 4000 Hz x 64, chunk 1 | 54 (53-54) | 130 (120-148) | 719 (460-868) | 4050 | 0 | 20.0 | 12.4 |
| main -> main | 4000 Hz x 64, chunk 32 | 343 (328-351) | 598 (576-610) | 1090 (1019-1143) | 3511 | 0 | 5.2 | 2.3 |
| dgram -> dgram | 4000 Hz x 64, chunk 32 | 255 (235-262) | 595 (572-603) | 1225 (1146-1315) | 8882 | 0 | 6.6 | 3.7 |

- **Latency.** `dgram` gives a lower p50 than TCP on every load.
- **CPU, single samples.** On both single-sample loads, `dgram` uses equal
  or lower CPU than TCP on both sides.
- **CPU, chunks.** At 512 Hz x 8, chunk 16, outlet CPU is +25 % and inlet
  CPU is 0.8 % against 0.6 %. At 4000 Hz x 64, chunk 32, outlet CPU is
  6.6 % against 5.2 % (+27 %) and inlet CPU is 3.7 % against 2.3 % (+61 %).
  One chunk of 32 samples takes 8 UDP datagrams instead of 1 TCP write.

### `dgram/1` between two machines on Wi-Fi

`conformance/peer/src/bin/wifitest.rs` ran twice between a Linux outlet and
a Windows inlet over Wi-Fi on 2026-11-21, with `lab.KnownPeers`. Each pass
ran 3 loads of 200 s each (10 minutes total for each mode). Two machines do
not share a monotonic clock, so the table gives the tail jitter above p50
(pass 1, then pass 2 on `main` with `dgram/1` on by default):

| mode | load | samples | lost | on_udp % | p99-p50 ms (run 1 / run 2) | p99.9-p50 ms (run 1 / run 2) | max-p50 ms (run 1 / run 2) |
|---|---|---|---|---|---|---|---|
| tcp | 512 Hz x 8, chunk 1 | 102400 | 0 / 0 | 0 | 20.0 / 30.0 | 39.6 / 79.9 | 164.6 / 161.3 |
| udp | 512 Hz x 8, chunk 1 | 102400 | 0 / 0 | 100 | 19.6 / 18.7 | 30.2 / 25.1 | 81.5 / 43.7 |
| tcp | 1000 Hz x 32, chunk 1 | 200000 | 0 / 0 | 0 | 6817.6 / 26.1 | 7060.0 / 58.5 | 7178.2 / 108.5 |
| udp | 1000 Hz x 32, chunk 1 | 200000 | 0 / 0 | 100 | 21.9 / 19.7 | 42.8 / 28.8 | 94.8 / 66.8 |
| tcp | 4000 Hz x 64, chunk 32 | 800000 | 0 / 0 | 0 | 128.0 / 75.8 | 176.7 / 211.7 | 285.7 / 257.6 |
| udp | 4000 Hz x 64, chunk 32 | 800000 | 0 / 0 | 100 | 19.3 / 19.1 | 38.4 / 61.5 | 85.0 / 146.0 |

- **Loss and order.** Every run gave 0 lost, 0 repeated, 0 reordered, and 0
  dropped samples across 2.2 million UDP samples, with 0 recoveries.
- **Consistency.** `dgram` held `p99-p50` between 18.7 ms and 21.9 ms on
  every load in both passes. TCP showed 26 ms to 128 ms on clean passes,
  and stalled for 7.2 s in pass 1 at 1000 Hz x 32 channels.
- **4000 Hz x 64 channels, chunk 32.** `dgram` cut `p99-p50` by 4x to 6.6x
  against TCP in both passes (19.3 ms and 19.1 ms against 128.0 ms and
  75.8 ms).
- **Discovery on Windows.** Multicast discovery did not find the Linux
  outlet, because `local_ipv4_addresses()` reads `/proc/net/fib_trie` on
  Linux only. Unicast with `lab.KnownPeers` needed an inbound allow rule in
  Windows Firewall for the UDP return port of the resolver.

## Step 8: flat typed API and shared samples

`oracle/latency.py` ran on 2026-11-24 with `oracle` (liblsl) and `flat`
(`feat/flat-shared`, `PackedSample` with `Arc<[u8]>` payloads across queues,
`resume` history, codecs, and C ABI), 3 runs of 5 s for each cell:

| outlet -> inlet | load | p50 us | p99 us | p99.9 us | max us | lost | CPU outlet % | CPU inlet % |
|---|---|---|---|---|---|---|---|---|
| oracle -> oracle | 512 Hz x 8, chunk 1 | 96 (91-97) | 191 (187-236) | 494 (488-512) | 992 | 0 | 5.2 | 3.1 |
| flat -> flat | 512 Hz x 8, chunk 1 | 90 (79-119) | 352 (309-428) | 435 (415-525) | 542 | 0 | 3.6 | 2.5 |
| oracle -> oracle | 512 Hz x 8, chunk 16 | 293 (283-316) | 581 (514-658) | 660 (626-688) | 688 | 0 | 0.8 | 0.4 |
| flat -> flat | 512 Hz x 8, chunk 16 | 230 (196-231) | 620 (601-620) | 637 (625-660) | 660 | 0 | 1.7 | 0.8 |
| oracle -> oracle | 4000 Hz x 64, chunk 1 | 70 (69-72) | 98 (94-100) | 125 (116-127) | 512 | 0 | 30.3 | 18.3 |
| flat -> flat | 4000 Hz x 64, chunk 1 | 50 (48-50) | 75 (75-76) | 100 (96-103) | 538 | 0 | 16.8 | 11.2 |
| oracle -> flat | 4000 Hz x 64, chunk 1 | 65 (65-66) | 90 (89-94) | 112 (106-117) | 286 | 0 | 29.1 | 15.9 |
| flat -> oracle | 4000 Hz x 64, chunk 1 | 67 (64-68) | 94 (93-100) | 132 (128-136) | 354 | 0 | 18.4 | 17.4 |
| oracle -> oracle | 4000 Hz x 64, chunk 32 | 294 (246-322) | 630 (597-674) | 682 (663-802) | 804 | 0 | 2.8 | 1.6 |
| flat -> flat | 4000 Hz x 64, chunk 32 | 141 (139-180) | 462 (276-489) | 1004 (641-1788) | 1799 | 0 | 3.4 | 2.2 |
| oracle -> flat | 4000 Hz x 64, chunk 32 | 148 (139-170) | 255 (242-272) | 337 (265-5633) | 5635 | 0 | 1.8 | 1.3 |
| flat -> oracle | 4000 Hz x 64, chunk 32 | 170 (164-173) | 496 (416-564) | 1393 (589-5092) | 5096 | 0 | 1.9 | 1.3 |

- **Latency.** At 4000 Hz x 64 channels, p50 drops to 50 us (chunk 1) and
  141 us (chunk 32), below both the earlier `dgram` build (54 us and 255 us)
  and liblsl (70 us and 294 us).
- **Inlet CPU.** At 4000 Hz x 64 channels, chunk 32, inlet CPU drops to
  2.2 % on UDP (from 3.7 %) and 1.3 % on TCP against liblsl (below liblsl at
  1.6 %). At chunk 1, inlet user CPU is 1.8 % (0.09 s in 5 s) while kernel
  `recvfrom` and condvar wakeups take 9.2 % (0.46 s sys), for 11.2 % total
  against 18.3 % for liblsl.
- **Fanout with 3 consumers.** `Fanout::push_many_packed` shares one
  `Arc<[u8]>` across all queues and the `resume` history with zero payload
  copies (about 3 ms for 20000 samples of 64 channels, under 0.1 % of one
  core). End-to-end outlet process CPU with 3 loopback UDP consumers is
  8.43 % against 3.63 % with 1 consumer at chunk 32 (2.32x) and 42.20 %
  against 17.48 % at chunk 1 (2.41x), because each consumer thread runs its
  own socket writes.
- **History memory.** A 64-channel `f32` sample takes 272 bytes in the
  `resume` history instead of 2064 bytes with `Vec<Value>` (7.6x reduction).

## Step 9: `shm/1` shared memory on one machine

`oracle/latency.py` ran on 2026-11-24 with `oracle` (liblsl) and `shm`
(`feat/shm`, owner-only lock-free seqlock SPSC ring with Linux `SYS_futex`
predictive pre-wake and zero-allocation numeric C ABI paths), 3 runs of 5 s
for each cell:

| outlet -> inlet | load | p50 us | p99 us | p99.9 us | max us | lost | CPU outlet % | CPU inlet % |
|---|---|---|---|---|---|---|---|---|
| oracle -> oracle | 512 Hz x 8, chunk 1 | 100 (98-106) | 287 (212-793) | 3478 (275-4419) | 6025 | 0 | 5.6 | 3.3 |
| shm -> shm | 512 Hz x 8, chunk 1 | 10 (4-10) | 42 (33-45) | 55 (55-56) | 127 | 0 | 1.6 | 1.3 |
| oracle -> oracle | 512 Hz x 8, chunk 16 | 188 (174-192) | 292 (289-297) | 320 (310-3024) | 3024 | 0 | 0.6 | 0.4 |
| shm -> shm | 512 Hz x 8, chunk 16 | 38 (34-43) | 69 (59-97) | 72 (60-222) | 223 | 0 | 0.8 | 0.2 |
| oracle -> oracle | 4000 Hz x 64, chunk 1 | 75 (75-76) | 116 (114-117) | 203 (181-209) | 960 | 0 | 32.2 | 19.3 |
| shm -> shm | 4000 Hz x 64, chunk 1 | 1 (1-1) | 23 (19-23) | 38 (35-54) | 728 | 0 | 6.4 | 8.6 |
| oracle -> oracle | 4000 Hz x 64, chunk 32 | 157 (157-166) | 300 (293-310) | 349 (320-363) | 365 | 0 | 2.1 | 1.3 |
| shm -> shm | 4000 Hz x 64, chunk 32 | 37 (34-38) | 73 (73-79) | 93 (81-97) | 111 | 0 | 1.4 | 0.6 |

- **Latency.** At 4000 Hz x 64 channels, chunk 1, `shm -> shm` reaches 1 us
  p50 across all 3 runs (75x below liblsl at 75 us and well below the 10 us
  criterion), 23 us p99, and 38 us p99.9. At chunk 32, p50 drops to 37 us
  (4.2x below liblsl at 157 us) with max 111 us.
- **CPU.** At 4000 Hz x 64 channels, chunk 1, outlet plus inlet CPU drops to
  15.0 % (6.4 % + 8.6 %), down 2.5x from the 38 % baseline and 3.4x below
  liblsl (51.5 %). At 512 Hz x 8, chunk 1, total CPU drops from 8.9 %
  (liblsl) to 2.9 %.
- **Container fallback.** `conformance/oracle/shm_container_test.py` runs an
  inlet in a separate mount namespace with an isolated `/dev/shm` over
  loopback: the inlet replies `shm-no`, `labstream_inlet_on_shm` returns 0,
  and all 200 samples arrive over TCP with 0 errors and 0 lost.

## What this benchmark does not measure

- **One-way latency between two machines.** Two machines do not share a
  monotonic clock, so `wifitest` measures tail jitter above p50, not
  absolute one-way latency.
- **The application.** Phasic and other programs pull in their own loop. Their
  poll interval adds to these numbers.
- **The first offset and the recovery.** The live tests in
  `crates/labstream-net/tests/` measure these.
