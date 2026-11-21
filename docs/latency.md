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

## What this benchmark does not measure

- **Two machines.** All results come from the loopback interface.
- **A real wireless link.** `netem` gives loss and delay with a fixed
  rule. A Wi-Fi link changes its rate, and loses packets in bursts. Do not
  add a `tc netem` rule to the `lo` interface of a shared machine, because
  the rule delays every program on the machine. `lossylink.py` uses its own
  namespaces instead.
- **The application.** Phasic and other programs pull in their own loop. Their
  poll interval adds to these numbers.
- **The first offset and the recovery.** The live tests in
  `crates/labstream-net/tests/` measure these.
