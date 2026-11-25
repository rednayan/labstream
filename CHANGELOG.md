# Changelog

This project follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0] - 2026-11-25

This release completes the full labstream feature roadmap (steps 0 to 18) while
keeping wire compatibility with liblsl. Highlights include dropout-free `resume/1`
and disk store-and-forward, UDP datagrams (`dgram/1`), shared memory (`shm/1`),
lossless wire compression (`comp/1`), channel and rate selection (`select/1`),
live metadata (`meta/1`), opt-in transport encryption (`enc/1`), opt-in drift
clock and device-delay calibration, browser reach (WebSocket and WebAssembly),
CLI tools (`doctor`, `top`, `play`, `relay`, `calibrate`), and real-time
multi-stream sync (`SyncGroup`).

### Added

- Real-time multi-stream sync and resampler (`docs/roadmap.md`, step 18):
  `labstream_time::sync::StreamAligner` and `labstream_net::SyncGroup`
  (re-exported as `labstream::SyncGroup`) align `N` asynchronous numeric
  inlets onto one strictly monotonic common time grid using Blackman-windowed
  sinc fractional-delay interpolation with bounded lookahead and bounded
  per-stream buffers, automatic opt-in `CLOCKSYNC | DRIFT | DEVICE_DELAY`, and
  explicit per-stream gap bitmasks (`AlignedSample::gap_mask`). Adds C
  functions `labstream_sync_create`, `labstream_sync_total_channels`,
  `labstream_sync_pull`, and `labstream_sync_destroy`.
- Opt-in negotiated transport encryption via `enc/1` (`docs/roadmap.md`, step
  14, `docs/extensions.md`, section `enc`): off by default, enabled via
  `labstream.Features` (`enc`), `labstream.RequireEncryption = 1`, or
  `labstream::enc::set_runtime_policy` / `labstream_set_encryption`. Uses the
  reviewed `ring` crate (gated behind Cargo feature `encryption`) for
  ephemeral X25519 key exchange (RFC 7748), HKDF-SHA256 (RFC 5869) with
  optional pre-shared key (`labstream.EncryptionKey`), and length-prefixed
  ChaCha20-Poly1305 AEAD framing (RFC 8439). Adds `Inlet::is_encrypted`,
  `InletHealth::encrypted`, `labstream_inlet_encrypted`, and
  `labstream_set_encryption`, verified by `crates/labstream-net/tests/enc_live.rs`.
- Browser reach via opt-in WebSocket bridge and WebAssembly decoder
  (`docs/roadmap.md`, step 13): `labstream_net::ws::WsBridge` (re-exported in
  `labstream::ws`) and `labstream relay` (`--ws-port`, `--ws-bind`,
  `--ws-allow`, `--ws-marker-stream`) expose a zero-dependency RFC 6455
  WebSocket endpoint (off by default, loopback-only unless configured) that
  forwards exact LSL wire samples in binary frames and publishes browser
  markers onto live LSL marker outlets. `crates/labstream-wasm` compiles
  `labstream-wire` to `wasm32-unknown-unknown` with `web/labstream.js` and
  `web/index.html`, and `labstream-capi` adds `labstream_ws_bridge_start`,
  `labstream_ws_bridge_port`, and `labstream_ws_bridge_stop`.
- In-band live metadata via `meta/1` (`docs/roadmap.md`, step 16,
  `docs/extensions.md`, section `meta`): `labstream_proto::meta` adds the
  `0xFC` record (`seq`, `timestamp`, `key`, `value`) on the TCP feed channel
  across TCP, `dgram/1`, and `shm/1` transports while leaving liblsl inlets
  unchanged. Adds `Outlet::{push_metadata, push_metadata_at, metadata}`,
  `Inlet::{has_live_metadata, pull_metadata, metadata}`, and C functions
  `labstream_outlet_push_metadata`, `labstream_inlet_has_live_metadata`,
  `labstream_inlet_metadata_count`, and `labstream_inlet_pull_metadata`.
- Opt-in linear offset-and-drift clock model (`docs/roadmap.md`, step 17):
  `labstream_time::DriftModel` and `DriftFit` fit a 64-burst RTT-weighted line
  with Huber outlier down-weighting from each time-sync burst midpoint.
  `flags::DRIFT` (`16`, `Post::DRIFT`, `LABSTREAM_POST_DRIFT`) removes the 2 s
  burst step while default `flags::CLOCKSYNC` stays bit-exact with liblsl.
  Adds `Inlet::clock_drift`, `Inlet::smooth_time_correction`,
  `InletHealth::clock_drift`, `labstream_inlet_clock_drift`, and
  `labstream_inlet_smooth_time_correction`.
- Device-delay calibration (`docs/roadmap.md`, step 15):
  `labstream_net::calibrate` (re-exported in `labstream::calibrate`) and
  `labstream calibrate` (`crates/labstream-cli`) measure fixed device delay
  against a reference marker or trigger stream via sub-sample edge
  interpolation. `StreamInfo::set_device_delay` and `StreamInfo::device_delay`
  store and read `<desc><synchronization><offset_mean>` (XDF-retained). Opt-in
  `flags::DEVICE_DELAY` (`32`, `Post::DEVICE_DELAY`,
  `LABSTREAM_POST_DEVICE_DELAY`) subtracts the delay on the inlet, with C
  functions `labstream_streaminfo_set_device_delay`,
  `labstream_streaminfo_device_delay`, `labstream_inlet_set_device_delay`, and
  `labstream_inlet_device_delay`.
- Lossless wire compression via `comp/1` (`docs/roadmap.md`, step 7,
  `docs/extensions.md`, section `comp`): `labstream_proto::comp` (`CompState`,
  `DgramCompEncoder`, `decode_dgram_payload`) applies bit-exact channel deltas
  (with an order-preserving IEEE-754 bijection for floats), ZigZag folding, and
  adaptive LSB-first Golomb-Rice codes over TCP (`0xF9`/`0xFA` with
  zero-overhead plain fallback) and self-contained UDP `DATA_COMP` (type `5`)
  datagrams. Adds `Inlet::is_compressed` and `Inlet::compression_ratio`
  (`labstream-net` and `labstream`), `InletHealth::{compressed,
  compression_ratio}`, `labstream_inlet_compressed` and
  `labstream_inlet_compression_ratio` (`labstream-capi`), live tests
  (`crates/labstream-net/tests/comp_live.rs`), `comp-report` EEG benchmark, and
  the 1 Mbit/s netns and 4000 Hz CPU test (`conformance/oracle/comp_test.py`).
- Same-machine shared-memory transport via `shm/1` (`docs/roadmap.md`, step 9,
  `docs/extensions.md`, section `shm`): `ShmParam` negotiation in
  `labstream-proto`, owner-only lock-free seqlock SPSC ring in
  `labstream-net` (`shm.rs`, Linux `shm_open`/`SYS_futex` with predictive
  pre-wake, macOS POSIX shm, Windows `CreateFileMappingW`/`CreateEventW`),
  zero-hop `Fanout` writes and zero-allocation `Outlet::push_numeric_raw`,
  `Outlet::push_chunk_numeric_raw`, and `Inlet::pull_numeric_into` paths,
  `Inlet::on_shm` (`labstream-net`, `labstream`), `InletHealth::on_shm` (shown
  as `SHM` in `labstream top`), and `labstream_inlet_on_shm` in
  `labstream-capi`. At 4000 Hz x 64 channels, chunk 1, `oracle/latency.py`
  measures 1 us p50 latency and 15.0 % total CPU (down from ~38 % baseline and
  51.5 % liblsl). Verified with `crates/labstream-net/tests/shm_live.rs` and
  `conformance/oracle/shm_container_test.py` (isolated `/dev/shm` TCP fallback).
- Channel and rate selection via `select/1` (`docs/roadmap.md`, step 6,
  `docs/extensions.md`, section `select`): `SelectParam`, `fir_taps`,
  `project_packed`, and `StreamSelector` in `labstream-proto` (bit-exact
  channel projection and named Hamming-windowed sinc low-pass FIR decimator),
  `StreamInfo::apply_selection`, `Inlet::open_selected`, and
  `Inlet::selection` in `labstream-net`, `InletBuilder::{channels, decimate,
  rate}` in `labstream`, and `labstream_create_inlet_selected` in
  `labstream-capi`. Verified with unit tests, live tests in
  `crates/labstream-net/tests/select_live.rs`, and the 1 Mbit/s netns test
  `conformance/oracle/select_test.py` (4000 Hz x 64 ch to 250 Hz with 0 lost).
- Local-to-network LSL stream relay (`docs/roadmap.md`, step 4):
  `labstream_net::relay::{StreamRelay, RelayOptions, is_relayed,
  build_relayed_info}` (re-exported in `labstream::relay`), the
  `labstream relay` command in `labstream-cli` (with `--disk-mb` for
  store-and-forward), and `labstream_streaminfo_is_relayed` in
  `labstream-capi`. Verified with live tests in
  `crates/labstream-net/tests/relay_live.rs`, CLI tests in
  `crates/labstream-cli/src/relay.rs`, and the real liblsl oracle netns
  cut test `conformance/oracle/relay_test.py` (0 lost across an 8 s cut).
- Disk-backed store-and-forward for `resume/1` (`docs/roadmap.md`, step 3).
  When `labstream.ResumeDisk` (in MiB, default 0) is above zero, `History`
  spills oldest samples from RAM into bounded immutable segment files on
  disk (optional directory `labstream.ResumeDiskPath`, default system
  temporary directory) while keeping RAM within `labstream.ResumeMemory`.
  `Fanout::read_history_packed` reads disk segments outside the `Fanout`
  lock so resends do not delay live consumers. The outlet removes all spool
  files and its temporary directory on drop.
- `Outlet::new_with_limits`, `Outlet::history_ram_bytes`, and
  `Outlet::history_disk_bytes` in `labstream-net` (plus RAM and disk byte
  getters on `labstream::Outlet`), and C functions
  `labstream_outlet_history_ram_bytes` and
  `labstream_outlet_history_disk_bytes` in `labstream-capi`, with live tests
  in `crates/labstream-net/tests/store_and_forward_live.rs`.
- Flat typed chunk methods `push_chunk_{f32,f64,i32,i16,i8,i64}` on
  `Outlet` and `pull_chunk_{f32,f64,i32,i16,i8,i64}` on `Inlet` in both
  `labstream` and `labstream-net` (`docs/roadmap.md`, step 8).
- `PackedSample` and `PackedValues` in `labstream-wire`, with
  `Codec::encode_packed`, `Codec::encode_packed_at`, and
  `Codec::decode_packed`. Numeric samples share one `Arc<[u8]>` payload
  across consumer queues and the `resume` history with zero payload copies,
  and cut `resume` history memory by 7.6x for 64-channel `f32` streams.
- Fast `PackedSample` paths across `SampleQueue`, `Fanout`, `History`,
  `dgram_out`, `dgram_in`, and the numeric C ABI push and pull calls in
  `labstream-capi`, with live tests in
  `crates/labstream-net/tests/flat_shared_live.rs`.
- The `seq/1` feature between two labstream peers (`docs/extensions.md`). A
  labstream inlet offers it in the request header `Labstream-Features`, and
  a labstream outlet answers it. liblsl ignores the header on both sides.
  The outlet numbers its samples, and the inlet counts the samples that
  never arrived: from a full outlet queue, and from a lost connection.
- `Inlet::lost` in `labstream` and `Inlet::samples_lost` in `labstream-net`.
  The value is `None` when the connection does not use `seq/1`.
- The configuration key `labstream.Features`. The value `{}` turns every
  labstream feature off.
- `labstream_proto::features` and `labstream_proto::seq`, with
  `FeedParams::to_wire_with`, `FeedClient::start_with`,
  `FeedClient::features` and `Accepted::to_wire_with`.
- `SampleQueue::with_first_number`, `SampleQueue::pop_many_numbered` and
  `Fanout::next_number`.
- `docs/latency.md` and `conformance/oracle/latency.py`, with
  `conformance/oracle/latency.c`. They measure the time from a push to a pull,
  and the CPU of each side, for liblsl and labstream.
- `docs/extensions.md`, a proposal for a feature header between two labstream
  peers, with the `seq` and `resume` features.
- `conformance/oracle/headerprobe.py`. It makes sure that the header changes
  nothing else on an outlet, also with a `resume/1` parameter.
- `docs/repository.md`. It gives the layout of the repository, how the
  workspace joins the crates, the daily commands, CI, and the dependency
  updates.
- `docs/releasing.md`. It gives each step of a release to crates.io and to
  GitHub, a test of the pipeline with no publish, the recovery for each
  failure, a publish by hand, and a checklist.
- `Outlet::push_samples`, which sends a block with one timestamp for each
  sample.
- `SampleQueue::push_owned`, which moves a block into a queue with no copy.
- `ClockState::reset_offset`, `ClockState::retarget`, `ClockState::is_running`,
  and `clock::FIRST_OFFSET_TIMEOUT`.
- `clock::FIRST_PROBE_INTERVAL` and `clock::FIRST_RETRY`.
- The `resume/1` feature between two labstream peers (`docs/extensions.md`,
  section `resume`). After a lost connection, the outlet sends the samples
  of the dropout again from a history, with their original timestamps.
  A 3 s cut at 512 Hz x 8 channels, at 4000 Hz x 64 channels, or on a
  marker stream loses no sample. The feature needs `seq/1`, and a liblsl
  peer or an older labstream peer gives plain LSL.
- The configuration keys `labstream.ResumeSeconds` (default 30 s, 0 turns
  the history off) and `labstream.ResumeMemory` (default 64 MiB). The
  default of `labstream.Features` is now `{seq, resume}`.
- `Inlet::resent` in `labstream`, `Inlet::samples_resent` in
  `labstream-net`, and `Outlet::history_len` in `labstream-net`.
- Two C functions that liblsl does not have: `labstream_samples_lost` and
  `labstream_samples_resent`. Each returns -1 when the count is unknown.
- `labstream_proto::features::{ResumeParam, ResumeRequest, offer_resume,
  resume_request, resume_answer}`, `labstream_net::history`,
  `Fanout::with_rate`, `Fanout::start_history`, `Fanout::read_history`,
  `Fanout::add_resumable`, `SampleQueue::with_deducer` and
  `SampleQueue::pop_many_full`.
- `docs/roadmap.md`, the order of the labstream-only features.
- `conformance/oracle/resumeprobe.py`. It cuts the connection of an inlet
  against outlets of each version.
- The `dgram/1` feature, on by default (`docs/extensions.md`, section
  `dgram`). The default of `labstream.Features` is now
  `{seq, resume, dgram}`. Between two labstream peers, the samples go in
  UDP datagrams with copies. The count of copies follows the loss that the
  inlet measures, from 0 on a clean link to 8 at heavy loss. The inlet asks
  for small gaps in feedback datagrams on UDP, and the outlet sends them
  again on UDP. The TCP connection carries the resends of large gaps and
  the path records. With 20 ms delay and 1 % loss, the p99 latency at
  512 Hz drops from about 71 ms to about 22 ms, with no lost sample.
  `Inlet::on_udp` shows the path.
- `labstream_proto::dgram`: the datagrams and records, `Packer`,
  `Reorder`, `CopyControl`, and `LossMeter`. `SampleQueue::wake`, and
  `seq::Counter::resync`.
- `conformance/oracle/lossylink.py`. It measures the latency over a slow,
  lossy, or cut link between two network namespaces, with no root.
  `oracle/latency.c` has new options `--timeout` and `--irregular`. The
  runner takes loads, markers, a configuration for each library, and rules
  for heavy loss (also from the start of a run), a dead UDP path, and a UDP
  path that comes back. A run whose inlet cannot connect gives the result
  `no connect`.
- `labstream_net::netif` and cross-platform interface enumeration (`getifaddrs`
  on Linux and macOS, `GetIpAddrTable` and `GetIpNetTable` on Windows).
  Before, interface enumeration read `/proc/net/fib_trie` and returned no
  interface on Windows and macOS.
- Fallback discovery (`labstream.FallbackDiscovery = auto` by default,
  `docs/discovery.md`). When multicast wave 1 finds no stream, the resolver
  tries directed subnet broadcast, ARP neighbor unicast, a local `/24`
  unicast probe, and TCP `LSL:shortinfo` for `lab.KnownPeers`. All paths use
  standard LSL packets and find both `labstream` and `liblsl` outlets.
- `labstream_net::doctor` (re-exported as `labstream::doctor`), the
  `labstream-cli` crate (`labstream doctor [--peer IP[,IP]] [--timeout S]
  [--json]`), and the C function `labstream_doctor_json`.
- `conformance/oracle/doctor_net.py`. It blocks multicast, broadcast, and UDP
  in two network namespaces against both `labstream` and `liblsl` outlets.
- Stream health API (`docs/health.md`, roadmap step 11): `Inlet::health`,
  `Inlet::gaps`, `Inlet::take_gaps`, and `Outlet::health` in both
  `labstream-net` and `labstream`, with `InletHealth`, `OutletHealth`,
  `ConsumerHealth`, `Gap`, and `GapKind`.
- Four C functions in `labstream-capi`: `labstream_inlet_health`,
  `labstream_inlet_gaps`, `labstream_inlet_take_gaps`, and
  `labstream_outlet_consumers`.
- Loss-aware dejitter (roadmap step 2). On connections that negotiate `seq/1`,
  `Inlet::pull` and `Inlet::pull_chunk` advance `PostProcessor::skip_samples`
  at the exact position of each lost or dropped gap so the first smoothed
  timestamp after a gap stays within one sample interval. Plain LSL
  connections without `seq/1`, and streams with zero loss, keep bit-exact
  liblsl timestamps.
- `labstream top` and `labstream play` in `crates/labstream-cli`
  (`docs/tools.md`, roadmap step 12). `top` shows each live stream with its
  measured rate, path, latency, and health counts from `Inlet::health` once
  each second (text table or JSON). `play` reads an XDF 1.0 file and replays
  its streams as live outlets with exact sample bits and file timestamp
  intervals.

### Fixed

- When an outlet closes a `dgram/1` consumer session, it now shuts down the
  shared TCP socket so the request reader thread exits at once and the inlet
  receives TCP FIN without waiting for the 15 s watchdog.
- Unicast discovery queries now leave from the receive socket so the reply
  returns on the same port and passes stateful firewalls such as Windows
  Defender Firewall with no inbound rule.

- An inlet with `CLOCKSYNC` waits up to 5 s for the first clock offset, as
  liblsl does. Before, the first timestamps used an offset of 0, and the real
  offset arrived about 0.6 s later as a step. If no offset arrives in 5 s, the
  samples continue with no correction. liblsl raises a timeout error there.
- A recovering inlet connects again to the same outlet after its TCP
  connection closes. Before, the stream stopped with no error while the
  outlet still ran. After a restart of the outlet, the clock probes now go to
  the new service port, and the offset resets.
- A consumer connection writes the samples of one push call with one socket
  write. Before, each sample had its own write, and a partial chunk could
  wait 200 ms.
- The inlet reader moves its buffer once for each read, and takes the queue
  lock once for each read. Before, it did both once for each sample.
- A discovery query uses the multicast TTL of the configuration. Before, it
  always used 1.

### Changed

- `README.md` gives how to download the built C library from a GitHub release,
  for each of the three platforms. It no longer says that only the tests of
  `labstream-net` need a network, because the tests of `labstream` bind sockets
  too.
- `docs/versioning.md` gives only what a version number means. Its release
  steps are now in `docs/releasing.md`.
- Without a clock offset, an inlet sends its clock probes 4 ms apart, and
  the burst stops when each probe has its answer. The first offset on
  loopback now arrives in about 40 ms. Before, it took about 370 ms. A burst
  that finds no offset tries again after 250 ms. The later bursts keep the
  configured cadence.
- Each read in a clock probe slot waits only for the time that is left in the
  slot. Before, a fixed 40 ms read timeout ended a 64 ms slot early.
- Each data connection uses TCP keepalive: a first probe after 2 s without
  traffic, then one each second, and an end after 3 probes without an
  answer. On Linux, data without an acknowledgement for 6 s also ends the
  connection. A connection that died with no close now ends in about 5 s,
  and a recovering inlet connects again. Before, an inlet waited for the
  15 s watchdog, and kept the dead socket while the outlet was visible.
- On Unix, a read that gives `ETIMEDOUT` ends the connection. A read timeout
  gives `EAGAIN` there.
- `socket2` now uses its `all` feature.
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

[Unreleased]: https://github.com/rednayan/labstream/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/rednayan/labstream/releases/tag/v0.2.0
[0.1.2]: https://github.com/rednayan/labstream/releases/tag/v0.1.2
[0.1.1]: https://github.com/rednayan/labstream/releases/tag/v0.1.1
[0.1.0]: https://github.com/rednayan/labstream/releases/tag/v0.1.0
