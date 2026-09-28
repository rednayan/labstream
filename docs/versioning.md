# Versioning

This repository follows [Semantic Versioning 2.0.0](https://semver.org/spec/v2.0.0.html).

This page gives what the version number covers, what it does not cover, and
which number a change needs. `docs/releasing.md` gives how to make a release.

## Three numbers that a reader can confuse

Three versions appear in this project. Each one changes for its own reason.

| Version | What it names | Who sets it |
|---|---|---|
| 0.1.2 | the crates in this repository | this project |
| 1.10 | the LSL wire protocol | the C++ library at `sccn/liblsl` |
| 1.75 | the minimum Rust compiler | this project |

A change to the crate version says nothing about the protocol version. This
library speaks protocol 1.10, and it will speak protocol 1.10 at version 1.0
and after it. `SPEC.md` gives the protocol. `README.md` explains why the
library refuses protocol 1.00.

## What the version covers

The version covers the public Rust API of these seven crates:

- `labstream`
- `labstream-core`
- `labstream-wire`
- `labstream-proto`
- `labstream-time`
- `labstream-net`
- `labstream-capi`

`labstream-core` holds no code of its own. It names the four crates that carry
the library. A change to what it names is a change to its API, and the table
below gives the rule for one.

A public item is an item that `cargo doc --workspace --no-deps` shows. If a
change removes such an item, or changes its signature, that change is
breaking.

## What the version does not cover

**The C ABI.** `labstream-capi` builds `liblsl.so` and exports 165 C symbols.
liblsl fixes every one of those names and signatures. A change to them will
break an installed pylsl or LabRecorder, so this project will not make one.
The C surface is therefore stable at every version, and the number above it
does not describe it.

**The exact bits of a timestamp.** `labstream-time` targets bit-exact
agreement with the C++ filter. That target is a measurement and not a promise
across every platform. The crate documentation gives the reason.

**A private item, a test, or a document.** A change to any of them is a patch.

## The rule for 0.x

The library is before version 1.0. Under semantic versioning a major version of zero
gives no stability promise. This project reads that rule as follows:

| Change | Before 1.0 | At 1.0 and after |
|---|---|---|
| breaking API change | minor, `0.1.0` to `0.2.0` | major |
| new API, nothing breaks | patch, `0.1.0` to `0.1.1` | minor |
| defect correction | patch | patch |

Read the `README.md` status table before you depend on an API. It states which
parts are measured.

## The crates move together

Every crate reads `version.workspace = true`. One release therefore gives every
crate the same number. A change in `labstream-net` alone still raises the
version of `labstream-wire` and of `labstream`.

`labstream` joined this rule at 0.1.2. Before that it had a repository and a
version of its own, and its one release was 0.1.0. Thus `labstream` has no
version 0.1.1. Semantic versioning permits the skip of a number.

`lsl-peer` below `conformance/` is not part of the rule. It is a tool of this
repository. It keeps version 0.0.0 and sets `publish = false`.

This costs a reader nothing, and it removes a matrix of crate pairs that no
person measured. If two crates from different releases must work together, no
test in this repository covers that pair.

## The minimum Rust version

The workspace sets `rust-version = "1.75"`. A job in `.github/workflows/ci.yml`
builds against that exact compiler on every pull request.

An increase of the minimum is a minor change before 1.0, and a minor change
after 1.0. The `CHANGELOG.md` entry must name the new minimum. Do not raise the
minimum for convenience. Raise it for a feature that the library needs.

## A protocol change is not a version rule

A defect in a protocol rule can produce a wrong recording. The version number
cannot express that risk. `CHANGELOG.md` must therefore name what a defect
produced, so a reader can decide whether the defect touched their data.

`CONTRIBUTING.md` explains when a change needs a conformance run.

## How to make a release

`docs/releasing.md` gives every step: the preparation, a test of the pipeline
with no publish, the tag, the publish to crates.io, the GitHub release, and the
recovery for each failure.
