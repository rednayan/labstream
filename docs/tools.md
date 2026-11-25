# Command-line tools: `labstream top` and `labstream play` (roadmap step 12)

**Status:** complete (`crates/labstream-cli`). `docs/roadmap.md`, step 12,
gives the success criterion.

## The purpose

Developers and lab operators need two small tools in the library:

1. **`labstream top`.** Discovers active LSL streams on the network and shows
   each stream in a terminal with its measured rate, transport path, latency,
   lost count, drop count, resend count, recoveries, and gap count. It reads
   those values from the step 11 health API (`Inlet::health`) and updates one
   time each second.
2. **`labstream play`.** Reads an XDF 1.0 recording and publishes its streams
   as live LSL outlets with the exact sample bits and timestamp intervals of
   the file. A developer can test Phasic or another consumer with no hardware.

## `labstream top`

```sh
labstream top [--interval SECONDS] [--count N] [--session ID] [--no-clear] [--json]
```

| Option | Behavior |
|---|---|
| `--interval SECONDS` | Update period in seconds (default `1.0`). |
| `--count N` | Stop after `N` updates (default runs until interrupted). |
| `--session ID` | Override the session query (default uses the active configuration). |
| `--no-clear` | Print each table below the previous one with no terminal clear sequence. |
| `--json` | Write one JSON object for each update tick with exact `InletHealth` fields. |

Each tick drains pending samples on every open inlet to measure the live rate
in Hz, reads `Inlet::health`, and writes the updated table or JSON line.

## `labstream play`

```sh
labstream play <FILE.xdf> [--speed FACTOR] [--loop] [--wait] [--stream NAME] [--name-prefix PREFIX] [--linger SECONDS]
```

| Option | Behavior |
|---|---|
| `<FILE.xdf>` | Path to an uncompressed XDF 1.0 file. |
| `--speed FACTOR` | Wall-clock pacing multiplier (default `1.0`). |
| `--loop` | Replay continuously, advancing timestamps monotonically across loops. |
| `--wait` | Wait until at least one consumer connects to each outlet before playback starts. |
| `--stream NAME` | Replay only streams whose name matches `NAME`. Repeat or separate with commas. |
| `--name-prefix PREFIX` | Prepend `PREFIX` to each published stream name. |
| `--linger SECONDS` | Keep outlets open after the last sample so consumers pull the tail (default `0.3`). |

### XDF 1.0 decoding and timing

`crates/labstream-cli/src/xdf.rs` parses XDF 1.0 chunks with no external
dependency:

- **Tag 1 (`FileHeader`)**: file version XML.
- **Tag 2 (`StreamHeader`)**: 4-byte stream ID and full `<info>` XML (parsed
  with `StreamInfo::from_fullinfo_xml`, preserving `<desc>` channel metadata).
- **Tag 3 (`Samples`)**: 4-byte stream ID, varlen sample count, per-sample
  timestamp byte width (`0` deduced from `nominal_srate` or `8` IEEE-754 LE
  `f64`), and raw little-endian channel values (`float32`, `double64`,
  `int32`, `int16`, `int8`, `int64`, and varlen `string`).
- **Tag 4 (`ClockOffset`)**: 4-byte stream ID, collection time, and clock
  offset. The first clock offset of each stream aligns multiple streams onto
  one timeline without distorting inter-sample intervals.
- **Tag 5 (`Boundary`)** and **Tag 6 (`StreamFooter`)**: skipped cleanly.

For each stream `s` with first sample timestamp `t0`, first clock offset
`off`, and global minimum aligned time `t_min`, sample `i` at file timestamp
`t_i` is published with timestamp:

```text
published_ts = play_origin + ((t0 + off) - t_min) + (t_i - t0)
```

For any two samples `i` and `j` of the same stream, `published_ts(j) -
published_ts(i)` equals `t_j - t_i`, and every channel value keeps the exact
bits read from the XDF file.

### Platform support

Both `labstream top` and `labstream play` use standard Rust threads, I/O, and
`labstream-net`. Linux, Windows, and macOS get the exact same behavior.

## Measured results

- `cargo test -p labstream-cli`: all 10 unit and live tests pass in 1.5 s,
  covering all seven sample formats (bit-exact `f32`/`f64`/integers/strings
  with NUL bytes), deduced XDF timestamps, real LabRecorder recording
  `conformance/captures/verify.xdf` (6 streams, 50-sample live replay with
  bit-exact values and intervals within `1e-8` s), and `TopMonitor` matching
  `Inlet::health` at a 1 s update cadence.
- Live smoke test (`labstream play conformance/captures/verify.xdf --stream
  Verify-rust` observed by `labstream top --count 3 --no-clear`): `top`
  discovered `Verify-rust` (EEG, 8 ch) and reported live rate (`98.0 Hz`),
  path (`UDP`), latency (`0.1 ms`), and zero loss, drop, resend, recovery,
  and gap counts.
- Two machines (Linux and Windows over Wi-Fi):
  - Windows `cargo test -p labstream-net --test health_live` (4/4 pass in
    1.26 s) and `cargo test -p labstream-cli` (10/10 pass in 1.55 s).
  - Linux `labstream play conformance/captures/verify.xdf --stream
    Verify-rust,Verify-Markers-rust --loop` watched by Windows
    `labstream.exe top` with no `--peer` flag: discovered both streams over
    Wi-Fi, reported `1.0 Hz` (`Verify-Markers-rust`) and `98-100 Hz`
    (`Verify-rust`) on `UDP` with `2-3 ms` latency and zero loss, drops,
    resends, recoveries, or gaps.

## `labstream relay`

```sh
labstream relay [--name NAME] [--type TYPE] [--query PRED] [--name-suffix TEXT] [--session ID] [--out-session ID] [--disk-mb MIB] [--disk-dir DIR] [--ram-mb MIB] [--local-only] [--ws-port PORT] [--ws-bind ADDR] [--ws-allow IP] [--ws-marker-stream NAME] [--duration SECONDS]
```

`labstream relay` (roadmap step 4) attaches to existing LSL outlets (such as
`liblsl` device drivers on the same machine) and republishes them through
`labstream` outlets carrying `seq/1`, `resume/1`, `dgram/1`, and optional
disk store-and-forward (`--disk-mb`). Each relayed stream keeps the full
`<desc>` tree, sample bits, and raw timestamps of the source stream, adds a
`<labstream_relay>` tag to `<desc>`, and marks `source_id` with `-relay` so
inlets and `labstream_streaminfo_is_relayed` can tell relayed streams from
source streams.

With `--ws-port PORT` (roadmap step 13), `labstream relay` also starts the
opt-in RFC 6455 WebSocket bridge (`labstream::ws::WsBridge`) bound to
`127.0.0.1` by default. Browser clients decode binary LSL frames with
`labstream-wasm` and publish markers onto `--ws-marker-stream`.

## `labstream calibrate`

```sh
labstream calibrate --target NAME --reference NAME [--channel INDEX] [--threshold VALUE] [--pulses N] [--timeout SECONDS] [--json]
```

`labstream calibrate` (roadmap step 15) measures fixed device delay between a
target sensor stream and a reference trigger or marker stream with sub-sample
edge interpolation. The reported offset goes into
`<desc><synchronization><offset_mean>` through `StreamInfo::set_device_delay`.

## Conformance plan

1. **XDF parser unit tests.** Round-trip all seven LSL sample formats (exact
   bit equality including negative zero, NaN, int boundaries, and strings with
   NUL bytes), deduced timestamps (`ts_bytes = 0`), clock offset alignment, and
   parsing the real LabRecorder recording `conformance/captures/verify.xdf`.
2. **Live `play` test.** Replay an XDF file with multiple formats (including
   float32 EEG and string markers) and confirm that a live LSL inlet receives
   the exact bits and exact inter-sample timestamp intervals.
3. **Live `top` test.** Open a live outlet, run `TopMonitor` (and the
   `labstream top --count 2 --json` CLI), and confirm that `top` reports the
   same values as `Inlet::health` and updates once per second.
