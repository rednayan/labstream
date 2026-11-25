# Frequently Asked Questions

## Does `labstream` work with existing `liblsl` hardware drivers and LabRecorder?

Yes. `labstream` implements LSL protocol 1.10 and exports all 165 C symbols of
`liblsl`. A `labstream` inlet reads from any `liblsl` outlet, and a `liblsl`
inlet (such as LabRecorder or `pylsl`) reads from any `labstream` outlet.

When an existing vendor driver ships with `liblsl` and cannot be modified, run
`labstream relay` on the acquisition machine to wrap its local outlets with
`seq/1`, `resume/1`, `dgram/1`, and optional disk store-and-forward.

## Do `labstream` extensions break plain `liblsl` peers?

No. Every extension (`seq/1`, `resume/1`, `dgram/1`, `shm/1`, `comp/1`,
`select/1`, `meta/1`, `enc/1`) negotiates through `Labstream-Features` headers
during the TCP handshake. A `liblsl` peer ignores unknown headers and receives
plain LSL 1.10 frames.

## Why does `labstream` refuse protocol 1.00?

Protocol 1.00 encodes samples in Boost serialization archives. Current `liblsl`
releases use protocol 1.10. `labstream` returns a clear refusal to a 1.00
request instead of misreading bytes.

## Is transport encryption (`enc/1`) enabled by default?

No. `enc/1` is off by default so unconfigured `liblsl` and `labstream` peers
interoperate with zero setup. Enable it with `labstream.Features` (`enc`),
`labstream.RequireEncryption = 1`, or `labstream::enc::set_runtime_policy`.

## What happens to `shm/1` inside Docker or containers with isolated `/dev/shm`?

During the `shm/1` handshake, the inlet opens the shared-memory segment and
returns a random token over TCP. If the segment is not visible (for example,
across container namespaces), the inlet does not return the token and both
sides fall back to TCP or `dgram/1` with zero sample loss.

## How does `SyncGroup` handle missing samples from one sensor?

`SyncGroup` never stalls healthy streams permanently and never invents fake
interpolated samples across a dropout. When one stream has a gap, `SyncGroup`
writes `NaN` for that stream's channels at affected grid timestamps and sets
bit `i` in `AlignedSample::gap_mask`.

## How is `labstream` verified against `sccn/liblsl`?

The `conformance/` workbench builds `sccn/liblsl` at pinned commit `e651023c`
and runs byte-level golden tests, 21 official C/C++ examples, 42 XPath oracle
queries, lossy-network namespace tests, and a 7.81-hour soak recording. Read
[Conformance Results](conformance.md) for the full record.

## What license does `labstream` use?

MIT. Read `LICENSE` in the repository root.
