# The reference oracle

This directory builds one exact revision of liblsl. That build is the reference
for every conformance vector in this project.

There is no LSL protocol specification. The C++ implementation is the
specification. This image makes that implementation reproducible, so a vector
that changes means a real change and not a different machine.

## Files

| File | Purpose |
|---|---|
| `PINS` | Every pinned version. One place, exact values. |
| `Dockerfile` | The reproducible build. |
| `entrypoint.sh` | Commands that the image accepts. |

## Build the image

Run this from the repository root, not from this directory:

```
docker build -f oracle/Dockerfile -t lsl-oracle .
```

The build fails if the `liblsl/` tree is not the revision named in `PINS`. A
wrong revision produces wrong vectors, and a wrong vector is worse than no
vector.

## Run the tests

```
docker run --rm lsl-oracle info
docker run --rm lsl-oracle test-gate
docker run --rm lsl-oracle test-ext
```

## The test groups

The upstream suite splits into two binaries. The split is close to the tier
model in `CONFORMANCE-PLAN.md`, but it does not match it exactly.

| Command | What it runs | Uses sockets | Treat a failure as |
|---|---|---|---|
| `test-gate` | Internal binary, `[network]` excluded | No | A real defect |
| `test-int` | The whole internal binary | Some tests do | Read the output first |
| `test-ext` | Exported binary | Yes | Possible environment cause |

Use `test-gate` as the merge gate. Do not use `test-int`.

The name `lsl_test_internal` suggests a hermetic group. It is not one. Four
files in `testing/int/` open sockets: `network.cpp`, `tcpserver.cpp`,
`sync_endian.cpp`, and `bench_timesync.cpp`.

One of those tests carries the Catch2 tag `[!mayfail]`. Inside a container it
cannot join a multicast group, it fails, and the binary still returns 0. A
tolerated failure hides a real environment difference behind a green result.

## Baseline result

`BASELINE.md` holds the full record, both build environments, and every finding.

| Command | Container result |
|---|---|
| `test-gate` | 886 assertions in 21 test cases pass |
| `test-int` | 974 assertions, 1 tolerated failure, exit 0 |
| `test-ext` | 2155 assertions, 1 failure from the container network |

## Reproducibility

Proved. Three independent builds produce identical artifacts. Run
`oracle/verify-reproducible.sh` to confirm this again. That script builds twice
with no cache, then compares the hash of every artifact.

The base image is pinned by digest. That digest pins the compiler, CMake, and
Catch2 together. pugixml is pinned by commit, because pugixml evaluates the
XPath query that decides whether a stream matches.

## One open item

The host build and the container build use different compilers. The container
uses GCC 13.3.0, and the host uses GCC 11.4.0. Take every golden vector from the
container, and never from the host.
