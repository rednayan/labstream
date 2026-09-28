# liblsl-rust

A Lab Streaming Layer core in Rust, and the conformance suite that proves it
speaks the same protocol as the C++ library.

There is no written specification of the LSL protocol. The C++ implementation
is the specification. Every claim in this repository is therefore either read
in `liblsl/` and cited as `file:line`, or measured against a pinned build.

**Oracle:** `sccn/liblsl` at `e651023c`, vendored in `liblsl/`.

## What works

An unchanged application that was written for liblsl runs on this library.
pylsl, LabRecorder, and a third-party recorder have each done so.

| Measure | Result |
|---|---|
| C symbols exported | 165 of 165, none a stub |
| Example programs of liblsl that link and agree | 21 of 21 |
| Interop cells against the oracle | 28 default, 16 blocking, 12 IPv6, and more |
| XPath queries answered as the oracle answers them | 42 of 42 |
| Longest recording | 7.81 hours, 8.4 million samples, nothing lost |
| Workspace tests | 218 |

Protocol 1.00 is refused on purpose. It carries every sample in a Boost
archive, and no liblsl of the last decade asks for it.

## Layout

| Path | What it holds |
|---|---|
| `crates/lsl-wire` | the sample codec. No input and no output |
| `crates/lsl-proto` | the handshake, discovery, and time sync. No sockets |
| `crates/lsl-time` | the post-processing filter, bit exact |
| `crates/lsl-net` | sockets, outlet, inlet, resolver, configuration, XPath |
| `crates/lsl-capi` | the C ABI, built as `liblsl.so` |
| `crates/lsl-peer` | the program that the interop harness drives |
| `oracle/` | every tool that measures this library against the C++ one |
| `captures/`, `artifacts/` | what those tools recorded |
| `SPEC.md` | the protocol, with a citation for every claim |
| `CONFORMANCE-PLAN.md` | the plan, and a log of each piece of work |

## Build

```sh
cargo build --release            # every crate
cargo test --workspace           # 218 tests
```

The C ABI builds as a drop-in library:

```sh
cargo build -p lsl-capi --release
cp target/release/liblsl.so .build/rustlib/liblsl.so
```

An application finds it the way it finds any other. pylsl reads the
`PYLSL_LIB` environment variable. A C or C++ program links against it with no
change to its source.

## Measure it yourself

Each tool below compares this library against the pinned C++ build. Every one
of them can fail, and each has been made to fail on purpose at least once.

```sh
oracle/interop.py --iut target/release/lsl-peer   # the live matrix
oracle/interop.py --selftest                      # the harness must catch a corrupt stream
oracle/xpath.py                                   # 42 queries, against either library
oracle/examples.py                                # the example programs of liblsl, unchanged
oracle/abitree.sh                                 # the description tree, call by call
oracle/configcheck.sh                             # the configuration file
oracle/refuse100.py                               # protocol 1.00 must be refused, not misread
oracle/pylsl_check.py --lib .build/rustlib/liblsl.so
oracle/memsoak.py --minutes 15                    # what each process holds
```

`oracle/interop.py` takes `--sync`, `--desc`, `--chunk N`, and `--zero-stamps`
for the modes that a plain run does not reach.

### Across two machines

```sh
oracle/labrecorder.sh oracle --name LabTest    # real liblsl, the control
oracle/labrecorder.sh capi   --name LabTest    # this library, through the C ABI
oracle/labrecorder.sh rust   --name LabTest    # this library, through lsl-net
```

Add `--ipv6` to publish over IPv6 alone. Each mode tags its streams, so all
three run at once and one recording holds all of them. Read the recording
with:

```sh
oracle/checkxdf.py --selftest captures/verify.xdf
```

**Read the control first.** If the other machine cannot see the liblsl stream
either, the network blocks the test and nothing else in the run means
anything.

## The one lesson

Nearly every defect found in this project was silent. Four examples: a
timestamp of zero, an address that was always the loopback, a flag that was
accepted and dropped, and a multicast port that only the first outlet held.
Each produced output that looked right. Each passed every test that existed.

None of them was found by a test written from the source. They were found by
disagreement with the oracle, or by real software on a real network.

Three times the failing result was the measuring tool rather than the library.
A control that is known to pass is therefore part of every tool here, and a
tool that has never been made to fail is not trusted.

## License

The vendored `liblsl/` keeps its own license. Everything else here follows the
license named in `Cargo.toml`.
