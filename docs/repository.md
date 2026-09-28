# The repository

This page gives how the repository is organized, why it is organized that way,
and how to work in it every day.

`docs/releasing.md` gives how to make a release. `docs/versioning.md` gives
what a version number means. `CONTRIBUTING.md` gives the rules for a change.

## One repository for everything

One git repository, `rednayan/labstream`, holds every crate, the conformance
workbench, and the documents. One Cargo workspace holds every crate.

Until September 2026, three repositories held this work:

| Old repository | What it held | Where it is now |
|---|---|---|
| `rednayan/labstream-core` | the protocol crates | `crates/labstream-*` |
| `rednayan/labstream-rs` | the `labstream` API crate | `crates/labstream` |
| `labstream-conformance`, never published | the workbench | `conformance/` |

The two published repositories are archived on GitHub. Their history is part of
this repository. The history of `labstream-rs` came in through `git subtree`,
and its release tag is `labstream-v0.1.0` here.

One repository has these results:

- A change to a protocol crate and to `labstream` builds and tests in one
  checkout, with one command.
- The workbench tests the crates that go to crates.io. It holds no copy of
  them.
- One tag, one workflow, and one changelog make a release of every crate.

## The layout

```
labstream/
├── Cargo.toml            the workspace: members, shared fields, shared dependencies
├── Cargo.lock            the exact dependency versions, for this workspace
├── crates/               every crate, one directory for each crate
│   ├── labstream/            the API that a Rust program calls
│   ├── labstream-core/       one dependency that names the four crates below
│   ├── labstream-wire/       the sample codec
│   ├── labstream-proto/      the handshake, discovery, and time sync
│   ├── labstream-time/       the timestamp filter
│   ├── labstream-net/        sockets, outlet, inlet, resolver
│   └── labstream-capi/       the C ABI, built as liblsl.so
├── conformance/          the tools that measure the crates against liblsl
│   ├── peer/                 lsl-peer, the program that the interop tool drives
│   ├── oracle/               the measurement tools and the Docker build of liblsl
│   ├── artifacts/            recorded results, as JSON and CSV
│   ├── captures/             recorded traffic and recordings
│   └── liblsl/               the pinned C++ library, a git submodule
├── docs/                 the documents in the table at the end of this page
├── .github/
│   ├── workflows/ci.yml      the tests, on every push and pull request
│   ├── workflows/release.yml the release, on the push of a tag
│   └── dependabot.yml        the weekly dependency updates
├── SPEC.md               the protocol, with a citation for every claim
├── README.md             the front page
├── CHANGELOG.md          what changed in each version
├── CONTRIBUTING.md       the rules for a change
├── CLAUDE.md             the instructions for Claude Code in this repository
└── LICENSE               MIT
```

## The crates

| Crate | What it holds | Needs | On crates.io |
|---|---|---|---|
| `labstream` | the API: block reads, the channel list, the query builder, one error type | `labstream-net`, `labstream-wire` | yes |
| `labstream-core` | no code. It names the four crates below, so a program takes one dependency | `-wire`, `-proto`, `-time`, and `-net` as an option | yes |
| `labstream-wire` | the sample codec. No input and no output | nothing | yes |
| `labstream-proto` | the handshake, discovery, and time sync. No input and no output | `-wire` | yes |
| `labstream-time` | the timestamp filter, bit exact to liblsl. No input and no output | nothing | yes |
| `labstream-net` | sockets, outlet, inlet, resolver, configuration, XPath | `-wire`, `-proto`, `-time` | yes |
| `labstream-capi` | the C ABI. It builds `liblsl.so`, `liblsl.dylib`, or `lsl.dll` | `-wire`, `-net`, `-time` | no |
| `lsl-peer` | a program that the interop tool drives. It is at `conformance/peer/` | `-wire`, `-net` | no |

This list gives which crate needs which:

```
labstream        needs  labstream-net, labstream-wire
labstream-core   needs  labstream-wire, labstream-proto, labstream-time, labstream-net (an option)
labstream-capi   needs  labstream-net, labstream-time, labstream-wire
labstream-net    needs  labstream-proto, labstream-time, labstream-wire
labstream-proto  needs  labstream-wire
labstream-time   needs  nothing
labstream-wire   needs  nothing
```

The order of a publish comes from this list. A crate goes to crates.io after
each crate that it needs.

Two crates are not on crates.io. Each one sets `publish = false`, so no command
can send it there:

- `labstream-capi` builds a `cdylib` alone, which a Rust program cannot link.
  The GitHub release carries the built library for Linux, macOS, and Windows.
  `docs/releasing.md` gives the reason in full.
- `lsl-peer` is a tool of the workbench and not a library.

## How the workspace joins the crates

The root `Cargo.toml` holds three sections that every crate uses.

**`[workspace.package]`** holds the fields that every crate shares: `version`,
`edition`, `rust-version`, `license`, `repository`, `authors`, `keywords`, and
`categories`. A crate takes a field with a line such as
`version.workspace = true`. One release therefore gives every crate the same
version.

**`[workspace.dependencies]`** holds each crate of this repository that another
crate needs. Each entry has a path and a version:

```toml
labstream-net = { path = "crates/labstream-net", version = "0.1.2" }
```

A crate takes the entry with `labstream-net.workspace = true`. The two halves
of the entry have two jobs:

| Half | Who reads it | What it gives |
|---|---|---|
| `path` | Cargo, in this workspace | the source in this checkout. A change builds at once |
| `version` | crates.io, in the published crate | the release that a reader downloads |

`cargo publish` removes the path from the published manifest and keeps the
version. A reader of `labstream` on crates.io therefore gets `labstream-net`
from crates.io. The version in each entry must equal the workspace version.
`docs/releasing.md` gives that step.

**`[profile.test]`** sets `opt-level = 1` for tests. The golden tests read
several thousand recorded vectors, and at `opt-level = 0` they are slow.

`lsl-peer` does not take the shared version. It keeps `0.0.0`, because it is
never released.

## Daily work

### Get the repository

```sh
git clone git@github.com:rednayan/labstream.git
cd labstream
```

That clone builds and tests every crate. It does not fetch the C++ library,
because only the workbench needs it. To fetch it too:

```sh
git submodule update --init conformance/liblsl
```

### Build and test

Run these commands from the repository root:

```sh
cargo build --workspace
cargo test --workspace
cargo fmt --all
cargo clippy --workspace --all-targets
cargo doc --workspace --no-deps --open
```

The tests need no network hardware, no C++ toolchain, and no submodule. The
tests of `labstream-net` and `labstream` bind loopback sockets.

To test one crate, name it:

```sh
cargo test -p labstream-net
cargo test -p labstream --test round_trip
```

The build output of every crate is in `target/` at the root. The workbench
tools read their binaries from there.

### Change a protocol crate and the API together

Edit both crates, and run `cargo test --workspace`. The workspace builds
`labstream` against the source of `labstream-net` in the checkout. No `[patch]`
section and no second checkout is necessary.

### Build the C library

```sh
cargo build -p labstream-capi --release
```

The result is `target/release/liblsl.so` on Linux, `liblsl.dylib` on macOS, and
`lsl.dll` on Windows. A C program links against it with no change to its
source. For pylsl, set `PYLSL_LIB` to the path of the file.

### Run the workbench

The workbench needs Docker, a C++ toolchain, Python, and a network. The crates
need none of them. `conformance/README.md` gives each tool, and
`conformance/oracle/README.md` gives the Docker build of liblsl.

Run the tools from `conformance/`. The tools find the workspace one directory
above.

A change to a protocol rule needs a conformance run. `CONTRIBUTING.md` gives
the rule.

## Files that git does not keep

`.gitignore` names them. A new clone does not have them.

| Path | What it holds | How to make it again |
|---|---|---|
| `target/` | the Cargo build output | any `cargo build` |
| `conformance/.build/` | the oracle build: liblsl, its examples, and probes | `conformance/oracle/README.md` |
| `conformance/captures/soak.xdf` | the long soak recording, 339 MB | `conformance/oracle/labrecorder.sh` and `memsoak.py` |
| `__pycache__/` | Python bytecode | any run of a Python tool |
| `.claude/settings.local.json` | the Claude Code permissions of one person | Claude Code writes it |

## Continuous integration

`.github/workflows/ci.yml` runs on every push to `main` and on every pull
request.

| Job | What it does |
|---|---|
| Test on ubuntu-latest, macos-latest, windows-latest | `cargo build` and `cargo test` of the workspace |
| Minimum supported Rust version | `cargo build` of the workspace with Rust 1.75.0 |
| Format and clippy | `cargo fmt --check`, and `cargo clippy` with warnings as errors |
| Documentation | `cargo doc` with warnings as errors |

CI does not fetch the submodule, and it does not run the workbench.

One test is ignored on macOS alone: `both_outlets_hold_the_multicast_port`.
`docs/conformance.md` gives the open question and the measurement that
answers it.

### Live tests

A live test opens a real outlet and a real inlet over loopback. Such a test
must wait for the connection before it pushes a sample:

```rust
assert!(outlet.wait_for_consumers(Duration::from_secs(5)));
outlet.push(&sample);
```

An outlet sends a sample only to a consumer that is already connected.
`Inlet::open` can return before the outlet registers the consumer. A sample
pushed in that gap is lost, and the test fails in some runs and not in others.
Four tests held that fault until v0.1.2.

## Dependency updates

`.github/dependabot.yml` asks for updates once each week, for the GitHub
Actions and for the Cargo dependencies. Each update is a pull request, and CI
runs on it.

- A Cargo update that changes `socket2` or `libc` touches the socket layer.
  After such a merge, a run of `conformance/oracle/labrecorder.sh` across two
  machines is the only test of multicast discovery between machines.
- Dependabot does not update `dtolnay/rust-toolchain` in the job for the
  minimum Rust version. That version is a rule, and `docs/versioning.md` gives
  it.
- If a pull request of Dependabot shows an old failure, comment
  `@dependabot rebase` on it. Dependabot then rebases it on `main`, and CI runs
  again.

## Add a crate

1. Make the directory below `crates/`, with a `Cargo.toml` and `src/`.
2. Add the path to `members` in the root `Cargo.toml`.
3. Take the shared fields: `version.workspace = true`, and the others.
4. If another crate needs it, add an entry to `[workspace.dependencies]` with
   a path and the workspace version.
5. If it goes to crates.io, add its name to the list in the publish step of
   `.github/workflows/release.yml`. If it does not, set `publish = false`.
6. Add it to the tables in this page and in `README.md`.

A new crate needs one more step before the workflow can publish it.
crates.io accepts a trusted publisher only for a crate that exists. The first
release of a new crate therefore goes by hand, with a token.
`docs/releasing.md` gives that case.

## The documents

| Document | What it holds |
|---|---|
| `README.md` | the front page: status, crates, examples |
| `docs/repository.md` | this page |
| `docs/releasing.md` | how to release to crates.io and to GitHub, and how to recover |
| `docs/versioning.md` | what a version number means, and the rule for each change |
| `docs/conformance.md` | what was measured against liblsl, how, and the result |
| `SPEC.md` | the protocol, with a citation for every claim |
| `CONTRIBUTING.md` | the rules for a change, and the style of the documents |
| `CHANGELOG.md` | what changed in each version |
| `crates/labstream/README.md` | the page of `labstream` on crates.io |
| `crates/labstream/docs/design.md` | why the API is a separate crate |
| `conformance/README.md` | the tools of the workbench |
| `conformance/CONFORMANCE-PLAN.md` | the log of the conformance work |
