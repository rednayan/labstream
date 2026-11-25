# labstream

`labstream` is a Rust implementation of the Lab Streaming Layer (LSL) protocol.
It implements LSL protocol version 1.10 and exports all 165 C ABI symbols of
`sccn/liblsl`. An unchanged C, C++, or Python application built for `liblsl`
runs on `labstream` with no source change.

When two `labstream` peers connect, they negotiate optional protocol extensions
inside the standard LSL handshake. A `liblsl` peer ignores those headers and
receives plain LSL 1.10.

## Measured status (v0.2.0)

Every conformance claim below comes from the oracle workbench in `conformance/`
against a pinned build of `sccn/liblsl` at commit `e651023c`. Read
[Conformance Results](conformance.md) for the test method and artifacts.

| Measure | Result |
|---|---|
| Exported C ABI symbols | 165 of 165, zero stubs |
| Official `liblsl` C/C++ examples that link and agree | 21 of 21 |
| XPath 1.0 queries that match the C++ oracle | 42 of 42 |
| Long-duration soak recording | 7.81 hours, 8.4 million samples, 0 lost |
| Workspace unit and integration tests | 508 tests, 7 doctests |

## Architecture

The workspace separates pure protocol logic from OS I/O. Only `labstream-net`
opens sockets, files, or shared memory.

```text
  Rust application           C / C++ / Python app          Browser
         |                            |                       |
    [labstream]               [labstream-capi]         [labstream-wasm]
         |                     (liblsl.so/.dll)               |
         +-------------+--------------+                  (WebSocket)
                       |                                      |
                [labstream-net] <-----------------------------+
               /       |       \
  [labstream-wire] [labstream-proto] [labstream-time]
   (zero I/O)       (zero I/O)        (zero I/O)
```

| Crate | Purpose | Published on crates.io |
|---|---|---|
| `labstream` | Ergonomic Rust API: typed chunks, builders, `SyncGroup`, health | Yes |
| `labstream-core` | Re-export facade over `-wire`, `-proto`, `-time`, and `-net` | Yes |
| `labstream-wire` | Binary sample codec and test pattern (no I/O) | Yes |
| `labstream-proto` | Handshake, discovery, time sync, extensions (no I/O) | Yes |
| `labstream-time` | Bit-exact post-processor, drift model, sinc aligner (no I/O) | Yes |
| `labstream-net` | Sockets, outlet, inlet, resolver, `shm/1`, `enc/1`, `ws`, `doctor` | Yes |
| `labstream-capi` | C ABI shared library (`liblsl.so`, `liblsl.dylib`, `lsl.dll`) | GitHub Releases |
| `labstream-cli` | `labstream` CLI (`doctor`, `top`, `play`, `relay`, `calibrate`) | Source / Binary |
| `labstream-wasm` | `wasm32-unknown-unknown` wire decoder (27 KB) | Source / WASM |

## Negotiated extensions

All extensions are opt-in or auto-negotiated in the LSL handshake. Read
[labstream Extensions](extensions.md) for the exact wire records and state
machines.

| Extension | Wire mechanism | Purpose |
|---|---|---|
| `seq/1` | Record tags `0xFD`, `0xFE` | Monotonic per-sample sequence numbers and exact gap detection |
| `resume/1` | History RAM + disk spool | Zero-loss sample and marker backfill after network cuts |
| `dgram/1` | UDP datagrams + NACK | Low-jitter delivery with adaptive redundancy on lossy Wi-Fi |
| `shm/1` | Lock-free SPSC seqlock ring | Same-machine cross-process IPC (`1 us` p50 at 4000 Hz x 64 ch) |
| `comp/1` | Delta + ZigZag + Golomb-Rice | Bit-exact lossless wire compression for numeric streams |
| `select/1` | Projection + Hamming-sinc FIR | Outlet-side channel subset and anti-aliased decimation |
| `meta/1` | In-band record `0xFC` | Live key-value metadata updates with zero reconnect |
| `enc/1` | X25519 + HKDF-SHA256 + ChaCha20 | Opt-in authenticated transport encryption (off by default) |
| `SyncGroup` | 32-tap Blackman-sinc aligner | Real-time multi-stream resampling onto one monotonic grid |
| `ws` + WASM | RFC 6455 bridge + 27 KB WASM | Zero-install browser viewers and browser-to-LSL markers |

## Where to go next

- Start with [Quickstart](quickstart.md) to add `labstream` to Rust, C, C++, or Python.
- Read [Command-Line Tools](tools.md) to use `doctor`, `top`, `play`, `relay`, and `calibrate`.
- Read [LSL 1.10 Specification](_ref/spec.md) and [labstream Extensions](extensions.md) for wire details.
- Run the seven live comparisons in [Interactive Demos](demos.md).
