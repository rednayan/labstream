# Health API (roadmap step 11)

**Status:** library, C API, tests, and latency check complete.
`docs/roadmap.md`, step 11, gives the success criterion. Phasic UI integration
lives in the Phasic repository.

## The purpose

An LSL application cannot see whether a live stream is healthy. A viewer shows
samples, and silent drops, late resends, high latency, or a full outlet queue
stay hidden. This step adds:

1. **Inlet health (`Inlet::health`).** One call gives latency, lost samples,
   dropped samples, resent samples, clock uncertainty, recoveries, connection
   state, transport path (`on_udp`), buffer fill, buffer capacity, and gap
   count.
2. **Gap timestamps (`Inlet::gaps` and `Inlet::take_gaps`).** Each gap carries
   its start timestamp, end timestamp, sample count, and kind (`Lost` on the
   outlet or network, or `Dropped` in the inlet queue).
3. **Outlet health (`Outlet::health`).** One call gives the fill, capacity,
   and drops of each connected consumer queue, the resume history length, and
   the pushed sample count.
4. **C API (`labstream-capi`).** Exports `labstream_inlet_health`,
   `labstream_inlet_gaps`, `labstream_inlet_take_gaps`, and
   `labstream_outlet_consumers` with the same values.

## Design

### Inlet latency

When the inlet reader thread pushes a batch of samples into the inlet queue, it
stores the timestamp of the last sample and the local clock at arrival in two
relaxed atomics. `Inlet::health` computes latency only when the caller asks for
it:

```text
latency = max(0.0, arrival_local - (sample_timestamp + clock_offset))
```

The value is `None` before the first sample arrives or before the first clock
offset measurement completes. `Inlet::pull` and `Inlet::pull_chunk` do no extra
work.

### Gap positions

Two sources produce gaps:

1. **Lost samples (`GapKind::Lost`).** When `seq/1` or `dgram/1` advances the
   lost count between a delivered sample at `start` and the next delivered
   sample at `end`, the inlet records `Gap { start, end, count, kind: Lost }`.
   When a recovery attaches to a new outlet run (new epoch or new `uid`), the
   cursor resets so timestamps from two different clocks do not pair.
2. **Dropped samples (`GapKind::Dropped`).** When the inlet queue overflows
   between a pulled sample (or the first dropped sample) at `start` and the
   next surviving sample at `end`, the queue records a coalesced
   `Gap { start, end, count, kind: Dropped }`.

Each gap ring holds at most 1024 entries, dropping the oldest gap when full so
long streams use bounded memory. When `Post::CLOCK_SYNC` is enabled on the
inlet and a clock offset is available, `Inlet::gaps` and `Inlet::take_gaps`
shift `start` and `end` by the clock offset so the markers align with the
pulled timestamps.

### Outlet health

`Fanout` assigns each connected consumer a monotonic `id` and records its peer
address. `Outlet::health` reads `len`, `capacity`, and `dropped` from each
consumer queue. A blocking outlet (`Outlet::new_blocking`) holds no queue, so
each consumer reports `fill = 0`, `capacity = 0`, and `dropped = 0`.

### C API layout

All four C functions live in `labstream-capi`:

| Function | Behavior |
|---|---|
| `labstream_inlet_health(inlet, out)` | Fills `labstream_inlet_health`. Unknown floats are `NaN`, and unknown counts (`lost`, `resent`) are `-1`. |
| `labstream_inlet_gaps(inlet, out, max_gaps)` | Copies up to `max_gaps` `labstream_gap` entries and returns the count written, or returns the total gap count when `out` is null. |
| `labstream_inlet_take_gaps(inlet, out, max_gaps)` | Drains up to `max_gaps` gaps and returns the count written. |
| `labstream_outlet_consumers(outlet, out, max_consumers)` | Copies up to `max_consumers` `labstream_consumer_health` entries and returns the count written, or returns the consumer count when `out` is null. |

### Platform support

All health and gap code uses standard Rust atomics, mutexes, and `clock()`.
Linux, Windows, and macOS get the exact same behavior and C ABI.

## Measured latency (`conformance/oracle/latency.py`)

Measured on Linux loopback (median of 3 runs, 5 s each, 0 lost across all
48 runs):

| Load | `liblsl` oracle p50 / p99 | `labstream` (step 11) p50 / p99 | `labstream` CPU (out / in) |
|---|---|---|---|
| 512 Hz x 8, chunk 1 | 89 / 169 us | 73 / 143 us | 3.5 % / 2.1 % |
| 512 Hz x 8, chunk 16 | 206 / 596 us | 133 / 303 us | 1.2 % / 0.5 % |
| 4000 Hz x 64, chunk 1 | 57 / 90 us | 45 / 75 us | 15.8 % / 10.6 % |
| 4000 Hz x 64, chunk 32 | 166 / 546 us | 173 / 426 us | 4.3 % / 2.9 % |

## Conformance plan

1. **Unit tests.** `GapTracker` (single gap, coalesced gaps, ring cap, cursor
   reset, clock shift), `SampleQueue` drop gaps, `Fanout::health` with multiple
   consumers and drops.
2. **Live tests (`crates/labstream-net/tests/health_live.rs` and
   `crates/labstream/tests/round_trip.rs`).** Test every field of
   `InletHealth`, `Gap` for both outlet drops (`Lost`) and inlet drops
   (`Dropped`), `OutletHealth` with fast and slow consumers, and `recoveries`
   after a reconnect.
3. **C API tests (`crates/labstream-capi/src/lib.rs`).** Test all four C
   functions against null handles, closed inlets, live streams, inlet drops,
   and outlet consumers.
4. **Latency check (`conformance/oracle/latency.py`).** Confirm no latency
   regression on loopback.
