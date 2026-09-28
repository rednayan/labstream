# The conformance workbench

This directory measures the crates of this repository against the C++ library.

There is no written specification of the LSL protocol. The C++ implementation
is the specification. Every claim in this repository is therefore either read
in `liblsl/` and cited as `file:line`, or measured against a pinned build.

**Oracle:** `sccn/liblsl` at `e651023c`, a git submodule at `liblsl/`.

`../docs/conformance.md` gives the result. This page gives the tools.

## What works

An unchanged application that was written for liblsl runs on this library.
pylsl, LabRecorder, and a third-party recorder each do so.

| Measure | Result |
|---|---|
| C symbols exported | 165 of 165, none a stub |
| Example programs of liblsl that link and agree | 21 of 21 |
| Interop cells against the oracle | 28 default, 16 blocking, 12 IPv6, and more |
| XPath queries answered as the oracle answers them | 42 of 42 |
| Longest recording | 7.81 hours, 8.4 million samples, nothing lost |

Protocol 1.00 is refused on purpose. It carries every sample in a Boost
archive, and no liblsl of the last decade asks for it.

## Layout

| Path | What it holds |
|---|---|
| `peer/` | `lsl-peer`, the program that the interop harness drives |
| `oracle/` | every tool that measures this library against the C++ one |
| `captures/`, `artifacts/` | what those tools recorded |
| `liblsl/` | the pinned C++ library, a git submodule |
| `CONFORMANCE-PLAN.md` | the plan, and a log of each piece of work |
| `liblsl-architecture.html` | a study of the C++ library, from before the Rust crates |

The crates under test are not here. They are in `../crates/`, and this
directory tests those crates and no copy of them. `lsl-peer` is a member of the
same Cargo workspace, and it sets `publish = false`.

## Get the C++ library

A clone of this repository does not fetch the submodule. Fetch it once, from
the repository root:

```sh
git submodule update --init conformance/liblsl
```

The build of the oracle writes to `.build/` in this directory. Git ignores
that directory. `oracle/README.md` gives the build.

## Build the library under test

The Cargo workspace is at the repository root. Its build output is at
`../target/`. Run these commands from this directory:

```sh
cargo build --release -p lsl-peer -p labstream-capi
mkdir -p .build/rustlib
cp ../target/release/liblsl.so .build/rustlib/liblsl.so
```

An application finds `liblsl.so` the way it finds any other shared library.
pylsl reads the `PYLSL_LIB` environment variable. A C or C++ program links
against it with no change to its source.

## Measure it yourself

Each tool below compares this library against the pinned C++ build. Every one
of them can fail, and each one was made to fail on purpose at least once. Run
them from this directory:

```sh
oracle/interop.py --iut ../target/release/lsl-peer   # the live matrix
oracle/interop.py --selftest                         # the harness must catch a corrupt stream
oracle/xpath.py                                      # 42 queries, against either library
oracle/examples.py                                   # the example programs of liblsl, unchanged
oracle/abitree.sh                                    # the description tree, call by call
oracle/configcheck.sh                                # the configuration file
oracle/refuse100.py                                  # protocol 1.00 must be refused, not misread
oracle/pylsl_check.py --lib .build/rustlib/liblsl.so
oracle/memsoak.py --minutes 15                       # what each process holds
```

`oracle/interop.py` takes `--sync`, `--desc`, `--chunk N`, and `--zero-stamps`
for the modes that a plain run does not reach.

`oracle/makevectors.py` and `oracle/recordtranscripts.py` write golden data.
By default they write it into the tests of `labstream-wire` and
`labstream-proto`, below `../crates/`.

### Across two machines

```sh
oracle/labrecorder.sh oracle --name LabTest    # real liblsl, the control
oracle/labrecorder.sh capi   --name LabTest    # this library, through the C ABI
oracle/labrecorder.sh rust   --name LabTest    # this library, through labstream-net
```

Add `--ipv6` to publish over IPv6 alone. Each mode tags its streams, so all
three run at once and one recording holds all of them. Read the recording
with:

```sh
oracle/checkxdf.py --selftest captures/verify.xdf
```

**Read the control first.** If the other machine cannot see the liblsl stream
either, the network blocks the test. Then nothing else in the run has a
meaning.

## The one lesson

Nearly every defect found in this project was silent. Four examples: a
timestamp of zero, an address that was always the loopback, a flag that was
accepted and dropped, and a multicast port that only the first outlet held.
Each produced output that looked right. Each passed every test that existed.

A test written from the source found none of them. Disagreement with the
oracle found them, or real software on a real network found them.

Three times the failure was in the measuring tool and not in the library. A
control that is known to pass is therefore part of every tool here. A tool that
never failed is not trusted.

## License

The `liblsl/` submodule keeps its own license. Everything else here follows the
license of the repository, in `../LICENSE`.
