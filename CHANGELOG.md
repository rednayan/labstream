# Changelog

This project follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `docs/repository.md`. It gives the layout of the repository, how the
  workspace joins the crates, the daily commands, CI, and the dependency
  updates.
- `docs/releasing.md`. It gives each step of a release to crates.io and to
  GitHub, a test of the pipeline with no publish, the recovery for each
  failure, a publish by hand, and a checklist.

### Changed

- `docs/versioning.md` gives only what a version number means. Its release
  steps are now in `docs/releasing.md`.
- `release.yml` publishes only the crates that crates.io does not hold at the
  version. A release that stops partway now completes when a person reruns the
  failed jobs. Before, the workflow refused a partial state, and a person had
  to publish the rest by hand. `docs/releasing.md` gives the recovery.

## [0.1.2] - 2026-09-29

This release changes no source code of the library. It changes one
dependency, `socket2`. It is the first release from the repository
`rednayan/labstream`, and the first that publishes all six crates together. `labstream` goes from 0.1.0 to 0.1.2, and it has no 0.1.1.

### Fixed

- Four live tests pushed a sample before the outlet registered the inlet. The
  outlet sent that sample to no consumer, and the test failed in some runs.
  `no_stage_leaves_a_timestamp_alone` failed on Windows and on Linux, and
  `a_deduced_timestamp_is_rebuilt_from_the_rate` failed on Windows.
  `clock_sync_moves_a_timestamp_by_the_measured_offset` and
  `a_marker_stream_goes_out_and_comes_back_as_text` held the same fault. Each
  test now waits for the consumer. The library did not change.
- `both_outlets_hold_the_multicast_port` is ignored on macOS. It fails there,
  and `docs/conformance.md` gives the measurement that decides the correction.
  Linux and Windows still run it.

### Added

- `.github/workflows/release.yml`. A push of a tag that starts with `v` checks
  the tag against the manifest and the changelog, runs the tests, publishes to
  crates.io, builds `labstream-capi` for Linux, macOS, and Windows, and writes
  the GitHub release with the three libraries attached.
- `publish = false` in `labstream-capi`. That crate builds a `cdylib` alone,
  which a Rust program cannot link, so no command can send it to crates.io.

### Changed

- `labstream-net` takes `socket2` 0.6 in place of 0.5. `labstream-net` uses it
  for one socket option, `reuse_address`, on the port of the multicast query.
  The call did not change. `socket2` 0.6 needs Rust 1.70, and the minimum of
  this workspace stays 1.75.
- `docs/versioning.md` gives the workflow, the one setting that crates.io needs
  for it, and the way to make a person approve each publish.
- `README.md` names `labstream` on crates.io. The page said that the API crate
  was not published.
- One repository, `rednayan/labstream`, now holds every crate. It replaces
  `rednayan/labstream-core` and `rednayan/labstream-rs`, and it keeps the
  history of both. The `repository` field of each crate names it.
- `labstream` is a member of the workspace. It takes `labstream-net` and
  `labstream-wire` by path and by version, so a change to both builds and
  tests in one checkout. The published manifest still names a version on
  crates.io.
- `labstream` takes the version of the workspace. One release now publishes
  six crates, and `release.yml` publishes `labstream` with the others.
  `crates/labstream/CHANGELOG.md` holds the entry for `labstream` 0.1.0.
- The conformance workbench is in `conformance/`. It held a copy of each
  protocol crate from before the rename, and that copy was out of date. It now
  tests the crates below `crates/`. `lsl-peer` is the one crate that it keeps,
  and that crate sets `publish = false`.
- The C++ library is a git submodule at `conformance/liblsl/`, at the same
  commit, `e651023c`.

## [0.1.1] - 2026-08-04

The first release on crates.io. `labstream-core` is the crate to name, and it
carries the other four.

### Added

- `labstream-core`. This crate holds no code. It names `labstream-wire`,
  `labstream-proto`, `labstream-time`, and `labstream-net`, so a program takes
  one dependency and reaches every part through `labstream_core::wire`,
  `::proto`, `::time`, and `::net`. It does not name `labstream-capi`, which
  builds a shared library for a C program.
- The `net` feature of `labstream-core`, on by default. A program that opens no
  socket can turn it off and keep the three crates that hold no input and no
  output.

### Fixed

- `an_outlet_reports_a_port_for_each_protocol` asserted that the IPv4 data port
  and the IPv6 data port differ. That assertion was wrong, and it failed on
  macOS and on Windows. A measurement of liblsl on Windows read 16572 for both
  fields, so one number for both families is what liblsl gives. The library
  already matched. The assertion has gone, and the test keeps the check that
  each family reports a port.

### Changed

- `docs/conformance.md` records the Windows measurement of the two port
  fields. Windows now carries one measured field. macOS carries none.
- Each crate takes its dependency on another crate here from
  `[workspace.dependencies]`, which holds a version as well as a path.
  `cargo publish` refuses a path with no version.
- `docs/versioning.md` gives the publish order, and it says why
  `labstream-capi` is not in that order. That crate builds a `cdylib` alone,
  which a Rust program cannot link.

## [0.1.0] - 2026-08-03

The first release. The library speaks protocol 1.10 of the Lab Streaming
Layer.

### Added

- `labstream-wire`: the sample codec for protocol 1.10. This crate holds no input
  and no output.
- `labstream-proto`: the handshake, the discovery messages, and the time sync. This
  crate holds no input and no output.
- `labstream-time`: the timestamp filter. The result matches the C++ filter bit for
  bit.
- `labstream-net`: sockets, outlet, inlet, resolver, configuration file, and XPath
  queries.
- `labstream-capi`: the C ABI. It exports 165 symbols and builds as `liblsl.so`.
- `SPEC.md`: the protocol document. Every claim cites the C++ source.
- `docs/conformance.md`: the conformance result and the method.
- `docs/versioning.md`: what the version number covers, and how to release.

### Platforms

- Linux, macOS, and Windows each build the workspace.
- `clock()` reads the performance counter on Windows. That is the clock that
  MSVC gives `steady_clock`, and liblsl reads `steady_clock`
  (`src/common.cpp:20`). `counter_seconds` holds the arithmetic, and every
  platform runs the five tests of it.
- `.gitattributes` holds one line ending for the golden data. Git on Windows
  changed a line ending on checkout, and six tests that compare against
  recorded output of liblsl failed there.
- `libc` is a dependency of Unix alone.

### Verified

- 165 of 165 C symbols export, and none is a stub.
- 21 of 21 example programs of liblsl link and agree.
- 42 of 42 XPath queries return what the C++ library returns.
- One recording of 7.81 hours held 8.4 million samples with no loss.
- 231 tests pass on Linux.

### Known limitations

- Only Linux carries a measurement against liblsl. macOS and Windows run the
  tests of this repository and nothing more.
- macOS: a second outlet does not hold the multicast port. Another machine sees
  one of two streams from one program, and not both. `docs/conformance.md`
  gives the measurement that decides the correction.
- Windows: one live test passes in one run and fails in the next. It waits five
  seconds for a sample over loopback.

### Refused

- Protocol 1.00. That version carries every sample in a Boost archive. An
  inlet that asks for 1.00 gets a refusal, not a wrong read.

[Unreleased]: https://github.com/rednayan/labstream/compare/v0.1.2...HEAD
[0.1.2]: https://github.com/rednayan/labstream/releases/tag/v0.1.2
[0.1.1]: https://github.com/rednayan/labstream/releases/tag/v0.1.1
[0.1.0]: https://github.com/rednayan/labstream/releases/tag/v0.1.0
