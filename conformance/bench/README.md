# Reproducible Bare-Metal Benchmark Suite (`conformance/bench/`)

This suite measures pinned C++ `sccn/liblsl` (`e651023c`) and `labstream` head
to head on bare-metal Linux with zero Docker overhead.

It compiles `worker.c` (`-O2`) twice, once against `conformance/.build/install/lib/liblsl.so`
and once against `target/release/liblsl.so`, pins the outlet and inlet to
separate CPU cores with `taskset`, and runs network scenarios inside isolated
Linux network namespaces (`unshare -Urn` + `veth` + `tc netem`).

## Benchmarks

- **B1 (Local IPC latency):** 64 ch @ 1000 Hz, chunk 1 — C++ `liblsl` TCP vs `labstream shm/1`
- **B2 (High-rate CPU & RSS):** 64 ch @ 4000 Hz, chunk 32 — C++ `liblsl` vs `labstream shm/1`
- **B3 (Lossy Wi-Fi):** 8 ch @ 512 Hz, chunk 16, `tc netem delay 20ms 5ms loss 1%` — C++ `liblsl` vs `labstream dgram/1`
- **B4 (`comp/1` & `enc/1` overhead):** 32 ch @ 500 Hz, chunk 16 — plain TCP vs `comp/1` vs `enc/1`
- **B5 (3 s hard blackout):** 8 ch @ 512 Hz, `tc netem loss 100%` for 3.0 s — C++ `liblsl` vs `labstream resume/1`
- **H1–H3 (Multi-device hyperscanning, `hypersync_bench.py`):** 2, 4, and 8 simultaneous asynchronous devices (up to 64 ch), 8-device heterogeneous rates (`500 / 250 / 120 Hz -> 250 Hz`), and 8-device single-headset 1.0 s dropout (`gap_mask` isolation)

## Run

Build `labstream-capi` in release mode first:

```sh
cd labstream
cargo build --release -p labstream-capi
python3 conformance/bench/run_bench.py
python3 conformance/bench/hypersync_bench.py
```

Outputs are written to `conformance/bench/results/`:

- `bench-results.json` — machine metadata and all metrics
- `bench-summary.md` — Markdown comparison table
- `bench-latency.svg` and `bench-cpu.svg` — zero-dependency SVG charts
