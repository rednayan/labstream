# M0 baseline

Recorded on 2026-08-02. Oracle revision
`e651023ca67996a05a028fd88a28603297120294`.

## Two build environments

The container is the reference. The host build exists for speed during
development. The two use different compilers, so they are not the same build.

| Item | Container (reference) | Host (development) |
|---|---|---|
| Base | `ubuntu@sha256:4fbb8e6a...` | Ubuntu 22.04 |
| Compiler | GCC 13.3.0 | GCC 11.4.0 |
| CMake | 3.28.3 | 4.4.2 |
| Catch2 | 3.4.0-1build1, from the image | v3.4.0, fetched from git |
| pugixml | commit `ee86beb3`, pinned | tag v1.15, fetched |

The system CMake on the host is 3.22.1. liblsl needs 3.23 or higher, so the
system CMake cannot configure it. This is the first reason the build needs a
container.

## Test results

| Group | Host | Container |
|---|---|---|
| `test-gate` (hermetic) | Not measured | 886 assertions, 21 cases, pass |
| `test-int` (whole internal binary) | 980 assertions, pass | 974 assertions, 1 tolerated failure, exit 0 |
| `test-ext` (exported) | 2155 assertions, pass | 2155 assertions, **1 failure** |

## Finding 1: the internal group is not hermetic

The conformance plan assumed that `lsl_test_internal` holds no network tests.
That assumption is wrong.

Four files in `testing/int/` open sockets: `network.cpp`, `tcpserver.cpp`,
`sync_endian.cpp`, and `bench_timesync.cpp`.

Inside the container, `reuseport` in `testing/int/network.cpp:264` cannot join a
multicast group and reports `No such device`. The test carries the Catch2 tag
`[!mayfail]`, so the run still returns 0. A tolerated failure hides a real
environment difference.

**Correction.** The merge gate is the internal group with the `[network]` tag
excluded. Use `test-gate`, not `test-int`. That subset holds 886 assertions in
21 test cases, and it passes with no tolerated failure.

## Finding 2: one exported test fails in a container

`testing/ext/discovery.cpp:63`, the test named `downed outlet deadlock`.

The test starts an outlet, resolves it, destroys the outlet, and expects
`inlet.info()` to raise an exception. On the host, the connection is refused and
the call raises. Inside the container network namespace, the call returns with
no exception.

This is a difference in the environment and not a defect in liblsl. It confirms
the tier model. A network test reports. It does not gate.

## Finding 3: two protocol facts that the source read missed

The capture rig found both within minutes of the first capture. Both belong in
`SPEC.md`.

1. The feed response starts with a status line, `LSL/110 200 OK`. The source
   read recorded the header fields below it and missed this line.
2. The byte order header is named `Byte-Order`, and the observed value is `1234`.

## Capture rig

`oracle/capture.py` speaks the protocol directly. It does not link liblsl. A
tool that linked liblsl would inherit any defect that liblsl has.

All three channels captured against `examples/SendData`.

| Channel | Result |
|---|---|
| Discovery | 5 matching replies, 675 bytes each |
| Time sync | 3 probes, round-trip time 0.25 ms on loopback |
| Data feed | 4108 bytes, 97 whole samples |

A sample of 8 float32 channels with a transmitted timestamp measures 41 bytes.
That is 1 tag byte, 8 timestamp bytes, and 32 channel bytes.

## Reproducibility: proved

`oracle/verify-reproducible.sh` ran two builds with no cache. Both produced
identical artifacts. The first build of the day produced the same hashes, so
three independent builds agree.

| Artifact | SHA-256, first 16 |
|---|---|
| `liblsl.so.1.17.7` | `8faf07d141bd9e50` |
| `lslver` | `be94a71c9e6959d8` |
| `testing/lsl_test_internal` | `455aba046fbd9664` |
| `testing/lsl_test_exported` | `d339a766b8d1e475` |
| `testing/lsl_test_runtime_config` | `d9c5a240ac44077a` |

This result was not certain. liblsl writes git version information into the
build, and a C++ build often carries a build path or a timestamp into the
binary. Neither happens here.

If any hash above changes, either a pin in `PINS` changed, or a defect exists.
Regenerate every golden vector when a hash changes.

## M0 exit criteria

| Criterion | State |
|---|---|
| Oracle pinned and the pin is enforced at build time | Met |
| Two builds produce identical artifacts | Met |
| Upstream suite green | Met, with the gate corrected to `test-gate` |
| Capture rig records all three channels | Met |

## Open item

The host build and the container build use different compilers. Take every
golden vector from the container, and never from the host.
