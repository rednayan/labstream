# Changelog

This project follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

This page holds the history of `labstream` before it joined the workspace.
From the next release, `labstream` takes the version of the workspace, and
`CHANGELOG.md` at the repository root holds every entry.

## [0.1.0] - 2026-08-04

The first release. This crate is the API that a Rust program calls. It holds
no protocol code.

### Added

- `Inlet`, with a builder that takes a buffer length and a post-processing set.
- `Outlet`, with a builder for the stream description.
- `Chunk<T>`, a buffer of one block of samples. `channel` gives one channel,
  and `iter` gives one sample at a time with its timestamp.
- `Query`, a builder for the XPath query that a resolver takes.
- `StreamInfo` and `Channel`, which give the description of a stream.
- `Scalar`, the trait that names the formats a sample can hold.
- `Error`, one error type for the crate.
- `docs/design.md`, which gives why this crate is separate from
  `labstream-core`.
- `examples/bench.rs`, which measures the block calls.

### Depends on

- `labstream-net` 0.1.1 and `labstream-wire` 0.1.1, from crates.io. Those
  crates hold the protocol, and they stay exact to `sccn/liblsl`.

[0.1.0]: https://github.com/rednayan/labstream/releases/tag/labstream-v0.1.0
