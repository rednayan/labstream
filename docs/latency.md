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

## What this benchmark does not measure

- **Two machines.** All results come from the loopback interface.
- **A slow or lossy network.** Do not add a `tc netem` rule to the `lo`
  interface of a shared machine. The rule delays every program on the machine.
  Use a network namespace instead. Make two namespaces with a `veth` pair,
  add the `netem` rule inside one of them, and run the outlet and the inlet in
  different namespaces. Discovery then needs the namespace addresses in
  `lab.KnownPeers` of the configuration. This procedure was not run yet.
- **The application.** Phasic and other programs pull in their own loop. Their
  poll interval adds to these numbers.
- **The first offset and the recovery.** The live tests in
  `crates/labstream-net/tests/` measure these.
