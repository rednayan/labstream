# LSL Protocol Conformance Suite — Plan

> **A note on names.** This page is a log, and it keeps the names of the day
> that each entry was written. The crates had the names `lsl-wire`,
> `lsl-proto`, `lsl-time`, `lsl-net`, and `lsl-capi`, below `crates/` of the
> workbench. They now have the names `labstream-wire`, `labstream-proto`,
> `labstream-time`, `labstream-net`, and `labstream-capi`, below `crates/` at
> the repository root. `lsl-peer` keeps its name, and it is at
> `conformance/peer/`.

**Status:** draft v1
**Oracle:** `sccn/liblsl` @ `e651023c` (`v1.17.7-18-ge651023c`), vendored at `./liblsl`
**Goal:** an executable, implementation-independent definition of "speaks LSL correctly."

---

## 0. Why this first

There is no LSL protocol specification. The C++ implementation *is* the specification.
Every claim of compatibility made by any other implementation is therefore unfalsifiable —
including `eugenehp/rlsl`'s "full protocol 1.00 + 1.10 compatibility, 162/162 C ABI symbols."

That is the actual gap in this ecosystem. Not the absence of a Rust core — one exists, it
took a night to generate. What's missing is any way to know whether it, or ours, is correct.

Two consequences shape everything below:

1. **The suite has standalone value.** It tests `liblsl` against itself, catches regressions
   upstream, and validates *any* implementation in any language. It is useful before a single
   line of the Rust core exists, and it is the only artifact here that `sccn` would plausibly
   adopt.
2. **The suite must not assume our implementation.** If it is written as "tests for our Rust
   crate," it is worthless as an oracle. It targets a wire protocol and a C ABI, and the
   implementation under test (IUT) is a pluggable adapter.

---

## 1. Established ground truth

Extracted by reading the oracle source. Citations are to `./liblsl`.

### 1.1 Discovery — UDP

Query, sent to multicast/broadcast/unicast targets (`src/resolve_attempt_udp.cpp:52-57`):

```
LSL:shortinfo\r\n
<query>\r\n
<return_port> <query_id>\r\n
```

Response, sent to `<return_port>` at the sender's address (`src/udp_server.cpp:process_shortinfo_request`):

```
<query_id>\r\n<shortinfo_xml>
```

- `query_id` is `std::to_string(std::hash<std::string>(query))` — **implementation-defined**.
  libstdc++, libc++, and MSVC produce different values. It is an opaque round-trip token:
  the responder echoes it, the requester matches it against what it sent. **A conformant
  implementation must never derive or validate it, only echo.** This is a conformance
  assertion, not an incidental detail.
- Query matching is `pugi::xpath_query(query).evaluate_boolean(doc.first_child())`
  (`src/stream_info_impl.cpp:214`) — **full XPath 1.0**, not a restricted subset. See §5, R1.

### 1.2 Time synchronization — UDP

Request (`src/time_receiver.cpp:136`): `LSL:timedata\r\n<wave_id> <t0>\r\n`
Response (`src/udp_server.cpp:process_timedata_request`): `' ' <wave_id> <t0> <t1> <t2>`
(leading space, `precision(16)`).

Estimation (`src/time_receiver.cpp:170-178`), NTP-style:
```
rtt    = (t3 - t0) - (t2 - t1)
offset = ((t1 - t0) + (t2 - t3)) / 2
```
Over a burst of probes, the estimate with the **lowest RTT** wins; `timeoffset_ = -best_offset`.

### 1.3 Data transport — TCP

Methods (`src/tcp_server.cpp:502-526`):

| Request line | Meaning |
|---|---|
| `LSL:shortinfo` | resolve response over TCP |
| `LSL:fullinfo` | full XML stream info |
| `LSL:streamfeed` | data feed, protocol 1.00 |
| `LSL:streamfeed/<ver> <uid>` | data feed, protocol ≥ 1.10 |

**1.10 client request headers** (`src/data_receiver.cpp:170-190`): `Native-Byte-Order`,
`Endian-Performance`, `Has-IEEE754-Floats`, `Supports-Subnormals`, `Value-Size`,
`Data-Protocol-Version`, `Max-Buffer-Length`, `Max-Chunk-Length`, `Hostname`, `Source-Id`,
`Session-Id`, terminated by a blank line.

**1.10 server response headers** (`src/tcp_server.cpp:674-677`): `UID`, `Suppress-Subnormals`,
`Data-Protocol-Version`, and a byte-order field.

**Negotiation** (`src/tcp_server.cpp:576-662`):
`data_protocol_version_ = min(cfg_proto_version, client_protocol_version)`, with explicit
downgrade-to-100 paths. `LSL_PROTOCOL_VERSION = 110` (`src/common.h:46`).
Major-version mismatch (`request/100 > cfg/100`) triggers a redirect/refusal.
UID mismatch ⇒ client throws `lost_error` and reconnects.

### 1.4 Sample encoding

**≥ 1.10** (`src/sample.cpp:189-265`) — raw, explicit byte order:
```
u8 tag:  1 = TAG_DEDUCED_TIMESTAMP   (no timestamp bytes follow)
         2 = TAG_TRANSMITTED_TIMESTAMP (f64 timestamp follows)
then N channel values, byte-swapped iff reverse_byte_order and width > 1
```
Strings carry a length prefix whose **width varies** (u8/u16/u32/u64 —
`src/sample.cpp:208-254`). The exact width-selection rule is not yet characterized and is a
Phase 1 deliverable, not an assumption.

**1.00** (`src/sample.cpp:330-350`) — `eos::portable_archive`, a Boost.Serialization
portable binary archive with its own header, versioning, and float encoding. This is a
materially harder reimplementation target than 1.10. See §4.

### 1.5 Timestamp post-processing

`src/time_postprocessor.cpp`. Three composable flags:

- `proc_clocksync` — add last measured offset; requery every 50 samples **and** at most
  twice per second (`next_query_time_ = lsl_clock() + 0.5`).
- `proc_dejitter` — recursive least squares fit of `t ≈ w0 + w1·n`, forgetting factor
  `lam = 2^(-1/(srate · halftime))`, baseline-subtracted by an integer `t0` for numerical
  accuracy. **Deterministic given an input timestamp sequence** — therefore golden-testable.
- `proc_monotonize` — clamp to running max.

Order is fixed: clocksync → dejitter → monotonize.

### 1.6 Existing test assets (reusable)

`./liblsl/testing`, 3,174 lines, Catch2. Directly relevant:
`int/serialization_v100.cpp` (already exercises denormals, ±inf, NaN, NUL-embedded strings),
`common/bytecmp.cpp`, `ext/discovery.cpp`, `ext/time.cpp`, `ext/sync_outlet.cpp`,
`int/sync_endian.cpp`, `int/postproc.cpp`.

**Do not rebuild what exists.** Phase 1 starts by mining these for edge cases already known
to matter to the maintainers.

---

## 2. The oracle problem

A conformance suite needs a source of truth that is not the thing being tested. Ours is a
real `liblsl` process. Three ways to consult it, in decreasing determinism and increasing fidelity:

**Tier A — Golden vectors (offline, byte-exact).**
Fixtures generated once from the pinned oracle, committed to the repo. No network, no
`liblsl` build required to *run* the suite. Fast, hermetic, CI-friendly anywhere. This is
the backbone.

**Tier B — Transcript equivalence (offline, canonicalized).**
Recorded full sessions. Compared structurally after canonicalization (§3), because handshakes
contain nondeterministic fields.

**Tier C — Live interop (online, differential).**
IUT and oracle as real processes over loopback. The 2×2 matrix:

| | oracle inlet | IUT inlet |
|---|---|---|
| **oracle outlet** | control (must pass trivially) | **real test** |
| **IUT outlet** | **real test** | self-consistency (weakest — can be self-consistently wrong) |

Only the off-diagonal cells prove anything. The diagonal cells are there to catch harness bugs
and to detect the failure mode where an implementation is internally coherent but wrong.

Tier A catches *encoding* errors. Tier C catches *behavioral* errors — timing, state machine,
reconnection. Both are required; neither subsumes the other.

---

## 3. The central engineering problem: determinism

This is the hard part and the reason naive "just diff the bytes" plans fail.

Nondeterministic across runs: wall-clock timestamps (f64), stream UIDs (random), ephemeral
ports, hostnames, session IDs, `std::hash` query IDs, `Endian-Performance` (a measured
benchmark), interface enumeration order, and multicast delivery order.

**Strategy: split the conformance surface by determinism class, and never mix them in one
assertion.**

| Class | Example | Assertion style |
|---|---|---|
| **Exact** | sample codec given fixed input + byte order | byte-for-byte equality |
| **Canonical** | handshake headers | rewrite variable fields to typed placeholders, then exact-compare |
| **Structural** | discovery response XML | field-wise, order-insensitive where the protocol allows |
| **Numerical** | clock offset convergence | bounded error / statistical envelope |
| **Temporal** | reconnect backoff, requery cadence | interval bounds, not point values |

The canonicalizer is a real component with a real spec, not a regex pass. Each variable field
gets a declared type and a validity predicate — e.g. `UID → <UID:hex16>` asserts *"16 hex
chars"*, `<TS:f64 in [t_start, t_end]>` asserts a bound. **A canonicalizer that erases a field
without asserting anything about it is a silent hole in coverage**, so every placeholder must
carry a predicate, and the suite reports placeholder-without-predicate as a lint failure.

The `Endian-Performance` header is the clearest case: its value is a benchmark result and can
never be compared, but its *presence, position, and numeric-parseability* must be.

---

## 4. Scoping decision: protocol 1.00

`LSL_PROTOCOL_VERSION = 110`. 1.00 is reached only via downgrade or old peers, and it requires
reimplementing `eos::portable_archive` — a Boost.Serialization format with a nontrivial header
and float encoding.

**Decision: the suite covers 1.00; the Rust implementation does not, initially.**

The two are separable and that separation is the point:

- The suite ships 1.00 golden vectors **and** tests that an implementation which declines 1.00
  *declines it correctly* — i.e. negotiates down, refuses cleanly, and does not corrupt the
  connection. Correct refusal is a conformance property.
- This bounds the rewrite's risk without leaving a hole in the oracle. If we later implement
  1.00, the target already exists.
- Anyone else's implementation gets measured against 1.00 whether or not we support it — which
  is exactly what an implementation-independent suite should do.

---

## 5. Phased plan

Each phase has a falsifiable exit criterion. No phase begins before its predecessor's exit
criterion is met.

### Phase 0 — Reproducible oracle (est. small)

- Pin `liblsl` at `e651023c`. Containerized build (Docker or nix) so vectors are regenerable
  byte-identically by anyone, on any host.
- Build and run the existing 3,174-line suite; record the baseline. **If upstream tests do not
  pass on our machine, everything downstream is built on sand** — resolve before proceeding.
- Build the capture rig: a UDP/TCP recording proxy that sits between two `liblsl` processes and
  logs both directions with monotonic timestamps and direction tags.

**Exit:** `docker run … regen-vectors` twice produces bit-identical output; upstream suite green.

**Status: complete (2026-08-02).** See `oracle/BASELINE.md`. Three findings changed this plan:

1. **`lsl_test_internal` is not hermetic.** Four files in `testing/int/` open sockets, and one
   test carries the Catch2 tag `[!mayfail]`, so a real container-network failure still exits 0.
   The hermetic gate is that binary with `[network]` excluded — 886 assertions, 21 cases. Any
   later reference to "the internal group" as the gate means `test-gate`, never `test-int`.
2. **Two protocol facts were missing from the source read** — the feed response opens with a
   status line `LSL/110 200 OK`, and the byte-order header is named `Byte-Order`. Both are
   Phase 1 inputs. This is the evidence for capturing before writing the spec, not after.
3. **The capture rig is a protocol client, not a proxy.** A proxy would need to rewrite the
   ports advertised in the discovery reply. A direct client is simpler and produces the same
   bytes. `oracle/capture.py` links nothing from liblsl, deliberately.

### Phase 1 — Specification extraction

Write `SPEC.md`: every message, every field, every state transition, every negotiation branch,
each with a source citation into the pinned oracle.

Must resolve the open questions this plan deliberately left open:
- exact string length-prefix width rule (§1.4)
- full `Value-Size` / `Suppress-Subnormals` / byte-order negotiation truth table
- chunking and `Max-Chunk-Length` boundary semantics (upstream #163, #161 suggest this is subtle)
- `DEDUCED_TIMESTAMP` deduction rule on the receiving side
- error/teardown paths: what a conformant peer does on malformed input, UID mismatch, redirect

Mine `./liblsl/testing` for edge cases the maintainers already consider load-bearing.

**Exit:** every claim in `SPEC.md` carries a `file:line` citation; a reviewer can trace each one
back to source without reading the whole codebase. Zero "TODO/unknown" markers remain in
normative sections. This document is independently publishable and is the first thing worth
showing `sccn`.

**Status: complete (2026-08-02).** `SPEC.md`, 83 citations, 54 normative / 22 observed / 6
captured claims. Open items are quarantined in §13, outside every normative section. Findings:

1. **The feed opens with two test-pattern samples**, and the client compares them against
   locally built copies (`tcp_server.cpp:694-705`, `data_receiver.cpp:276-297`). The protocol
   carries a built-in codec self-test. This changes M2: a Rust outlet that a real liblsl inlet
   accepts has a *proven* codec for that format, at handshake time, with a named error on
   failure. It also means an implementation that skips those two samples desynchronizes the
   entire stream.
2. **The server reads a request header that no client sends.** The client writes
   `Data-Protocol-Version` (`data_receiver.cpp:183`); the server matches `protocol-version`
   (`tcp_server.cpp:628`). Confirmed on the wire against the pinned oracle. A conformant server
   must read `Protocol-Version`; the response header keeps the `Data-` prefix.
3. **The string length prefix is a width byte plus the length.** The encoder emits widths 1, 4,
   and 8 only. The decoder also accepts 2 (`sample.cpp:198-214`, `sample.cpp:251`). Encode and
   decode are deliberately asymmetric — a vector set must cover both directions separately.
4. **A chunk is not a wire unit.** No header, no length, no delimiter; it only decides when the
   server calls one socket write (`tcp_server.cpp:782`). "Chunk boundary semantics" was the
   wrong question — a reader cannot observe a boundary at all.
5. Three status codes exist, not one: 200, 404, and 505 (`tcp_server.cpp:673`, `:586`, `:578`).

### Phase 2 — Codec conformance (Tier A, exact)

Golden vectors over the cross-product:

- channel formats × {little, big endian} × {deduced, transmitted timestamp}
- edge values: `±0.0`, denormal min, type max, `±inf`, quiet NaN, empty string, NUL-embedded
  string, string at each length-prefix width boundary (the ±1 cases around each width
  transition are where implementations break)
- chunk boundaries: 0, 1, `Max-Chunk-Length ± 1`

Fixtures as `(description, input, expected_bytes)` in a language-neutral format so a
non-Rust IUT can consume them.

**Exit:** ≥ 200 vectors; oracle round-trips all of them; a deliberately mutated codec is caught
by ≥ 1 vector for every mutation in a hand-written mutation list (this is the test-the-tests step
— a suite that passes everything proves nothing).

**Status: complete (2026-08-02).** `crates/lsl-wire`, 22 tests green, clippy clean.
42 vector files / **576 golden samples** / 40,140 oracle bytes, all 7 formats, 15 with
oracle-swapped byte order. 14 of 15 mutants caught; the 15th is provably unreachable. Notes:

1. **Big-endian vectors come from the oracle, not from us.** A request claiming
   `Native-Byte-Order: 4321` with `Endian-Performance: 0` makes the server lose the speed
   contest and swap every value (SPEC §5.2). So the swapped path is oracle-derived rather than
   produced by re-running our own encoder with a flag flipped.
2. **Byte round-trip alone is not sufficient**, and this is not hypothetical. A decoder and an
   encoder that hold the same error still reproduce the input bytes. The generator therefore
   emits an independent record of every value it pushed, as bit patterns, and the test compares
   against that record. The first failure found was caught by exactly this check.
3. **`push_sample(data, 0.0)` means "use the current clock"** (`stream_outlet_impl.h:258`). A
   vector using `0.0` as a fixed timestamp was silently non-reproducible. Now SPEC §8.3.
4. **One mutation class is unreachable through the oracle.** Negotiation condition 3 requires a
   value size above 1, so liblsl never swaps a one-byte format — no capture can ever hold a
   swapped `int8` stream. The mutation harness marks it `Blocked` with that reason, asserts that
   no vector catches it (a vector that did would falsify the reason), and covers the property
   with a direct test instead. Coverage boundaries get recorded, not deleted.

### Phase 3 — Handshake & discovery conformance (Tier B, canonical)

Canonicalizer + transcript corpus covering: the full version-negotiation matrix (including
every downgrade path), byte-order negotiation in both directions, UID mismatch, redirect,
`shortinfo`/`fullinfo`, and a representative XPath query battery for `matches_query`.

**Exit:** every transcript replays deterministically; every canonicalizer placeholder carries a
predicate (lint clean); the XPath battery covers the query forms actually used by
LabRecorder and pylsl in the wild, not invented ones.

**Status: complete for the feed handshake (2026-08-02), XPath deferred.** `crates/lsl-proto`,
84 workspace tests green, clippy clean. 22 transcripts recorded from the oracle; all replay
with a byte-identical answer after canonicalizing. 12 of 12 negotiation mutants caught. Notes:

1. **The canonicalizer lint is enforced, not aspirational.** Every rule carries a predicate and
   a reason, and `lint()` fails a rule that asserts nothing without explaining why. The lint runs
   as its own test *before* any comparison uses the rules — risk R3 closed.
2. **`Endian-Performance` is handled by construction, not by comparison.** The corpus uses only
   the two unambiguous extremes (0 and 10¹²), where the outcome does not depend on the machine.
   Any positive server value reproduces both. A middle value would make the transcript
   machine-dependent, so none is recorded.
3. **The corpus is minimal, and that is a fragility.** Nine of twelve mutants are caught by
   exactly one transcript. Deleting any single transcript silently removes a branch from
   coverage. The mutation test is what makes that visible; keep it in CI or the corpus rots.
4. **`can_convert_endian` is now read** (`src/util/endian.hpp:23-30`) — SPEC open question 3
   closed, and the rule is in SPEC §5.2.
5. **XPath is deferred, deliberately.** It is risk R1 and the largest unknown in the rewrite.
   The plan says to decide with data — the queries real applications send — and that data does
   not exist yet. Nothing in M3 needed it, and inventing a battery now would answer the wrong
   question. It gates the resolver, not the feed.

### Phase 4 — Live interop matrix (Tier C)

The 2×2 matrix over loopback with a null Rust IUT first (a thin `liblsl` FFI shim), which
validates the *harness* before it is used to judge anything. Then swap in the real IUT.

**Exit:** all four cells pass with byte-identical received sample streams; harness proven by
the shim before judging any native implementation.

**Status: complete (2026-08-02).** `oracle/peer.cpp` (the null IUT) and `oracle/interop.py`
(the harness). **28 of 28 cells pass** — 7 formats × 4 matrix positions. Notes:

1. **The harness is implementation-independent by construction.** A peer is any program
   accepting `peer outlet|inlet --name --format --channels …`. That CLI is the entire contract,
   so the harness links neither liblsl nor our Rust code. A Rust peer drops in with zero harness
   changes — which is also what stops the suite from quietly becoming "tests for our
   implementation" (requirement 2 of §0).
2. **The harness proves it can fail before it judges anything.** `--selftest` corrupts a stream
   four ways (drop a sample, flip a value, shift a timestamp, reorder two) and requires the
   comparison to report every one. A harness that never fails is indistinguishable from one that
   never checks. It runs *first*, and aborts the run rather than printing a green matrix.
3. **M4 earned its place on the first run.** All 4 string cells failed with real liblsl on both
   sides — by construction a harness defect, and the harness said so in those words. Cause: an
   empty string value hex-encodes to the empty string, so the field vanished under a whitespace
   split and the column count silently dropped by one. **Had this surfaced in M5 it would have
   read as a bug in the Rust implementation.** Fixed with a `-` sentinel on both sides.
4. **Post-processing must be off on the inlet.** `proc_none` is the default, but the peer sets it
   explicitly — every stage rewrites the timestamp the comparison depends on.
5. **Known limit:** with the null IUT every cell is liblsl against liblsl, so the two mixed cells
   carry no more information than the control. They become real tests only in M5, when a native
   peer fills the IUT slot. The value delivered here is a *proven* harness, not interop evidence.

### M5 — Native peer, live interop (this is the plan's Phase 4 with a real IUT)

**Status: complete (2026-08-02).** `crates/lsl-net` + `crates/lsl-peer`. **28 of 28 cells pass
with a native Rust implementation** — all 7 formats, and crucially the 14 mixed cells, which
are now genuine interop tests. A real liblsl inlet resolves, connects to, and reads exact
samples from a Rust outlet, and the reverse. Workspace: 94 tests, clippy clean. Findings:

1. **The dedicated multicast port was missing from SPEC.** `ports.MulticastPort = 16571`
   (`src/api_config.cpp:169`) sits **outside** the 16572–16603 unicast range. A resolver sends a
   multicast/broadcast query to 16571 and a unicast query to the range; an outlet binds both
   (`src/stream_outlet_impl.cpp:90-101`). Our outlet listened only on the range, so a real
   liblsl inlet never saw it. Now SPEC §1.1, normative.
2. **A multicast join must name each interface.** Joining with the unspecified address lets the
   kernel choose, and it can choose loopback — which carries no multicast at all. liblsl's own
   log says so: `netif 'lo' (status: 1, multicast: 0, broadcast: 0)`. On a single machine that
   makes the outlet invisible even though everything else is correct. Also now SPEC §1.1.
3. **`std::fs::read("/dev/urandom")` hangs forever** — that file has no end. A UID generator
   built on it blocks at startup and consumes memory without bound. Use `read_exact`.
4. **The XPath subset is real and small.** `resolver_impl::build_query`
   (`src/resolver_impl.cpp:66-73`) emits only `session_id='X' and prop='Y'`, so that shape was
   built first and everything else returned `false`. **Section 6l closed this.** The battery of
   `oracle/xpath.py` turned the risk into a list, and `crates/lsl-net/src/xpath.rs` now answers
   all 42 queries exactly as liblsl answers them.
5. **What was missing at that point:** protocol 1.00, IPv6, `LSL:fullinfo`, the time correction
   on the inlet, the drop-oldest policy, and the description tree. **Every one of them is now
   built or refused on purpose.** Sections 6b to 6m carry the work, and section 8 carries the
   state at the end.

### Phase 5 — Time sync & post-processing

Dejitter and monotonize are deterministic given an input sequence — golden-test them directly
against `int/postproc.cpp`'s expectations. Clock offset estimation needs an injected clock to
be testable at all; assert convergence envelopes, not point values.

**Exit:** dejitter RLS matches oracle output within f64 tolerance over ≥ 10⁴-sample sequences
including gaps and resets; offset estimator converges within bound under simulated asymmetric
delay.

**Status: complete (2026-08-02).** `crates/lsl-time` + estimator envelope tests in `lsl-proto`.
**96,008 samples across 10 sequences, bit-exact — 100.0000%, not "within tolerance".** Workspace
119 tests, clippy clean. Findings:

1. **The float discipline from tab B worked, exactly as specified.** Same operation order, no
   `mul_add`, integer baseline kept. Every returned value *and* the full internal state (`w0`,
   `w1`, `P00`, `P01`, `P11`, `lam`, counters) match the oracle to the bit. The plan hedged this
   as "a target, not a promise"; on x86-64 with GCC 13.3 vs rustc 1.91 it holds outright.
2. **The generator links the oracle's own object files.** `oracle/gentime.cpp` builds against
   `.build/oracle/CMakeFiles/lslobj.dir/*.o`, so the numbers come from the exact binary M0
   pinned — not from a recompile that might differ.
3. **The mutation test found a defect in the *assertion*, not the data.** With a 1e-12 tolerance,
   `reassociated_gain` (reordering one sum in the gain denominator) read as uncaught. Direct
   measurement showed the two orderings differ on **10,402 of 96,008 evaluations**, by ~1 ulp —
   the sequences discriminated it perfectly and the comparison threw the signal away. Both the
   golden and mutation tests now assert bit-exact, and the tolerance is gone. **A tolerance is a
   place where a suite silently stops testing.**
4. **Two mutants exposed real gaps in the sequence set**, and the fix was new sequences, not a
   weaker bar: `baseline_not_truncated` needed a fractional start timestamp (every sequence began
   at exactly 5000.0), and `slope_starts_at_zero` needed a sequence short enough that convergence
   had not yet erased the initial slope. Added `fractional_start`, `short`, `tiny`, plus
   `highrate` and `lowrate` for magnitude spread. 10 of 10 mutants now caught.
5. **The estimator gets an envelope, not a golden value** — its inputs are three clock readings a
   real network decides. The bound is derived from the rule itself: the lowest-RTT probe wins, so
   the kept estimate's error is at most half its round-trip time. Measured 0.09 ms error against
   a 2.17 ms bound over 200 probes on a path with asymmetric jitter. The test also asserts the
   bound stays *useful* (< 20 ms), so a vacuous bound cannot pass.
6. **A trap worth recording:** `is_initialized()` tests `t0_ != 0`, and `t0_` is the first
   timestamp truncated to an unsigned integer. A stream whose first timestamp is below 1.0 reads
   as uninitialized forever. Covered by a unit test.

### Phase 5c - The XPath measurement (risk R1)

**Status: complete (2026-08-02).** `oracle/xpath.py`, 42 queries, all 42 behaving as read from
the source. `artifacts/xpath-liblsl.json`.

1. **Full XPath 1.0 is genuinely supported** - `contains`, `starts-with`, `substring`,
   `translate`, `concat`, `normalize-space`, `string-length`, arithmetic, `div`, `floor`,
   `count()`, `//`, `not()`, parentheses, `true()`/`false()`. Nothing was rejected.
2. **The evaluation context is the `<info>` element, not the document.**
   `stream_info_impl.cpp:214` passes `doc.first_child()`. So `info/name='X'` matches **nothing**
   and `//name='X'` matches. An implementation that evaluates against the document root inverts
   both answers, and nothing else in the protocol would reveal the error.
3. **A malformed query is indistinguishable from a non-matching one.** pugixml throws, liblsl
   catches and returns false. Five malformed queries produced silence, exactly like a valid
   query that did not match. Fail-closed is therefore not just our choice - it is the oracle's.
4. **The risk is smaller than it looked, and now has a size.** liblsl's own API only ever emits
   a conjunction of equalities. Full XPath is reachable only through `resolve_stream(pred)` with
   a hand-written predicate. That is a real feature to build, but it no longer blocks anything.

### Phase 5b - Behavior under stress (pulled forward from Phase 6)

The interop matrix runs one narrow path: 12 samples, one consumer, post-processing off, no
buffer ever fills. Three subsystems therefore had **zero** conformance evidence -
`consumer_queue`, `inlet_connection`, `api_config` - and two of them rank #2 and #3 in the
architecture study. A rewrite of either would have been written blind.

`oracle/behavior.py` records liblsl's behavior in four scenarios, against the reference only.
Each recording becomes the specification the Rust implementation has to match.

**Status: complete (2026-08-02).** Findings:

1. **`Max-Buffer-Length` carries samples on the wire; the API argument does not.** The
   conversion happens at the C boundary (`lsl_inlet_c.cpp:20`, rule at
   `stream_info_impl.cpp:254-270`): with a rate the argument is **seconds**, with no rate it is
   multiplied by 100, and the `transp_bufsize_samples` flag makes it literal. At 1000 Hz a
   request of 20 builds 20,000 samples. **The first run of this test failed for exactly that
   reason** - a "20 sample" buffer never overflowed. Now SPEC 8.6.
2. **Drop-oldest, with numbers.** A 100-sample ring, 600 samples pushed fast at a 50 Hz reader:
   received sample 0, a gap of 499, then 500-599. The writer never blocked, and nothing reported
   the loss - no error, no dropped count. A gap is detectable only if the sender puts an index
   in the data. Now SPEC 8.7.
3. **The push-through flag overrides the chunk length.** A first chunking run showed identical
   read sizes for every `Max-Chunk-Length`, because the writer set push-through on every sample
   (`tcp_server.cpp:782`). The flag wins. The scenario now clears it.
4. **Two harness defects, both mine, both found by the reference.** The peer read the stream
   identifier from its local description instead of from the outlet, so it always logged an
   empty value. And a "slow" consumer with a rate had a 5000-sample buffer, so it never fell
   behind. Both would have read as implementation defects in Phase 1.

### Phase 6 — Adversarial

- `cargo-fuzz` on the codec and header parsers, seeded from the Phase 2/3 corpus.
- Fault injection: packet loss, reorder, duplication, truncation mid-handshake, interface
  up/down mid-stream, network switch.

Phase 6 targets the actual upstream bug cluster — 13 open discovery/network issues, plus
crash reports #199 (continuous_resolver crash on network switch) and #158. **Reproducing an
open upstream issue in this harness is the single strongest credibility signal available**,
and it is worth more to adoption than any amount of green CI.

---

## 6. Risk register

| # | Risk | Impact | Mitigation |
|---|---|---|---|
| **R1** | Query matching is full XPath 1.0 via pugixml. No Rust crate matches pugixml's dialect and edge-case behavior. | High — blocks a *general* resolver | **Closed (2026-08-03).** `oracle/xpath.py` measured the size of the problem first: 42 queries, every XPath 1.0 group answered by the oracle. That battery then became the specification. `crates/lsl-net/src/xpath.rs` reads it, and the same battery run against our outlet answers **42 of 42 queries identically to liblsl**, malformed ones included. No dependency was added. |
| **R2** | `eos::portable_archive` (1.00) is deep. | Medium | Scoped out of the rewrite (§4), still covered by the suite. |
| **R3** | Canonicalization hides a real difference. | High — silent false pass | Every placeholder carries a predicate; lint enforces it (§3). |
| **R4** | Suite over-fits the oracle's incidental behavior, so a *correct* alternative implementation fails. | Medium | Separate **normative** from **observed** assertions in `SPEC.md`. `std::hash` query IDs are the canonical example: observed, never normative. |
| **R5** | Nondeterminism makes CI flaky; team learns to ignore red. | High — kills the project slowly | Tiers A/B are hermetic and gate merges. Tier C runs separately and is allowed to be noisy. |
| **R6** | Oracle upstream drifts. | Low | Pinned commit; re-pinning is a deliberate, reviewed change with vector regeneration. |

---

## 6b. Phase 1 of the rewrite

The suite reached its ceiling as a pre-implementation activity. Phase 1 builds the pieces that
the recordings in Phase 5b now specify.

### Step 1 - Wire the clock into the inlet

**Status: complete (2026-08-02).** `crates/lsl-net/src/clock.rs`, plus post-processing in
`Inlet::pull`. 130 workspace tests, clippy clean.

The pieces existed after M6 and nothing joined them. An inlet returned raw sender timestamps,
so the library did not yet do the one thing LSL exists for.

1. **The clock had the wrong origin, and it was invisible until a live run.** `lsl_local_clock`
   is `steady_clock::now().time_since_epoch()` (`src/common.cpp:20`) - on Linux that is
   `CLOCK_MONOTONIC`, whose origin is boot. Ours measured from the start of the process. Every
   timestamp was therefore wrong by the age of the machine: **the first live measurement
   reported an offset of 31,426 seconds. After the fix the same measurement reports 15
   microseconds.**

   This matters because `proc_none` is the *default*. An application that stamps samples with
   the local clock, read by a consumer on the same machine that applies no correction, gets
   timestamps wrong by hours with nothing reporting it. Now SPEC 3.0.

2. **`std::time::Instant` cannot supply the value and neither can `/proc/uptime`.** `Instant`
   uses the right clock and keeps its origin private. `/proc/uptime` counts a *different* clock:
   it includes time spent suspended, and read 66,567 on a machine where `CLOCK_MONOTONIC` read
   31,492. The system call is the only agreeing source, so `lsl-net` relaxed
   `forbid(unsafe_code)` to `deny` with one documented `allow` around `clock_gettime`. That is
   the second crate permitted unsafe, and the reason is recorded at the call site.

3. **A measurement error of mine nearly became a bug report.** Two inlets run back to back see
   different samples, so comparing their first timestamps showed a 4.5 s "offset" that was
   really 18 samples of elapsed stream. The trace settled it: the burst had already measured
   -1.58e-5 s correctly. A comparison between two runs is not a comparison between two clocks.

4. **The interop matrix now runs with the stages on.** A corrected timestamp is not
   bit-comparable, because the offset differs per run, so the harness gained a timestamp bound
   while values stay exact. That is the determinism-class split from section 3 applied to a new
   surface.

### Step 2 - The bounded queue and backpressure

**Status: complete (2026-08-02).** `crates/lsl-net/src/queue.rs`, wired into both sides.

The outlet used an unbounded `mpsc` channel and the inlet decoded inside `pull`. Neither could
drop a sample, so under sustained overload the first grew without limit and the second left the
socket unread. `consumer_queue` was the subsystem at 10% built and 0% covered.

**The recorded behavior now reproduces exactly.** Same scenario, same numbers:

| Measure | liblsl | Rust |
|---|---|---|
| received | 101 | 101 |
| first / last | 0 / 599 | 0 / 599 |
| gaps / missing | 1 / 499 | 1 / 499 |
| writer blocked | no | no |

The multi-consumer scenario matches too, including the gap count: fast reader 400 samples and 0
gaps, slow reader 121 samples across 16 gaps, on both implementations.

1. **The inlet needed a background reader.** Decoding inside `pull` leaves the socket unread
   while the application works, so a loss lands in the *outlet's* queue instead of the inlet's.
   liblsl runs the same thread (`src/data_receiver.h`). This was a restructure, not an addition.
2. **The unit rule from SPEC 8.6 became code, and the test caught its absence at once.** The
   first run against the Rust peer received 2 samples where liblsl received 101, because
   `--buflen 1` was taken literally as one sample. liblsl converts it to 100 at the C boundary.
   `StreamInfo::buffer_samples` now implements `calc_transport_buf_samples`, and the C ABI in
   Phase 2 will call it. Recording the rule in SPEC was not enough; only running the scenario
   against both implementations found the gap.
3. **A recording is a far stronger specification than prose.** "Drops the oldest" would have
   been satisfied by a dozen wrong implementations. "101 received, first 0, last 599, one gap of
   499, writer not blocked" is satisfied by one.

### Step 3 - The watchdog and recovery

**Status: complete (2026-08-02).** `read_and_recover` in `crates/lsl-net/src/inlet.rs`, plus
`StreamInfo::recovery_query`. 149 workspace tests, clippy clean.

`inlet_connection` was the last subsystem at 5% built and 0% covered. An inlet that lost its
source stayed dead, which makes the library unusable in a room where equipment gets restarted.

**The recorded behavior reproduces.**

| Measure | liblsl | Rust |
|---|---|---|
| identifier changed | yes | yes |
| received before the stop | 98 | 100 |
| received while down | 98 | 100 |
| received after the restart | 300 | 300 |
| recovered | yes | yes |

The two counts before the stop differ because they measure a wall-clock moment during a live
push, not a protocol value.

1. **The recovery query is not the resolve query.** `src/inlet_connection.cpp:154-173` matches on
   channel count, name, type, source identifier, and channel format - and **deliberately omits
   the nominal rate**, with a comment explaining that a rate written as text and read back is
   often a different number, which would make the stream unfindable. It also omits the session
   identifier, unlike `resolver_impl::build_query`.
2. **The result must be unique.** liblsl accepts a recovery only when exactly one stream matches
   (`src/inlet_connection.cpp:188-192`), because a source identifier that is not unique would
   otherwise attach the inlet to the wrong stream. A result still carrying the old identifier
   means the source never left.
3. **One thread owns the connection for its whole life.** Reading and reconnecting in the same
   loop avoids handing a socket between threads. The watchdog is not a separate timer: a source
   that stopped without closing looks exactly like a quiet one, and the 15 second threshold
   (`src/api_config.cpp:302-303`) is what separates them.
4. **Two of my own defects, both silent.** The `--recover` flag never reached the inlet because
   `cargo fmt` had reflowed the line a patch was matching on, so the edit applied to nothing and
   reported success. And a manual test killed a subshell rather than the peer, so the first
   outlet delivered everything and no recovery was needed - it looked like a pass. The harness
   found both because it terminates the process it started and compares against a recording.

## 6c. Phase 2 - The C ABI

**Status: the core path works (2026-08-02).** `crates/lsl-capi`, built as `liblsl.so`.
112 symbols exported, clippy clean.

**An unmodified pylsl imports and runs against it.** No patch, no shim - pylsl reads the
`PYLSL_LIB` environment variable, so the library is swapped by pointing at a path.

| Test | Result |
|---|---|
| `import pylsl` against our library | works |
| pylsl round trip: outlet, resolve, inlet, 5 samples | exact timestamps and values |
| **real liblsl inlet reading pylsl-on-our-library** | 10 samples, exact |
| **pylsl-on-our-library reading a real liblsl outlet** | 6 samples, exact |

The last two are the ones that matter. A Python application that has only ever used liblsl now
talks to a Rust implementation, and to a real liblsl on the other side, without knowing.

1. **Every symbol must exist before any call happens.** pylsl sets an argument list for all 85
   symbols it uses during `import`, so one missing name stops the import outright. Coverage is
   therefore all-or-nothing at load time, and the first useful milestone is "the import
   succeeds", not "a function works".
2. **pylsl bundles liblsl 1.17.7** - the exact revision M0 pinned. The ABI under test is the ABI
   that was studied.
3. **The buffer length conversion lives here, and now it is exercised by a real caller.**
   `lsl_create_inlet_ex` calls `StreamInfo::buffer_samples`, which is where SPEC 8.6 says liblsl
   puts it. Phase 1 built the rule; this is the first code path that a Python application
   reaches it through.
4. **The chunk calls are implemented, and they exposed a hole that nothing else could.**
   `push_chunk` with one timestamp treats that value as the time of the **last** sample and
   counts backward by `(count - 1) / rate` (`src/stream_outlet_impl.h:247-274`, now SPEC 9.2).
   Only the first sample of a chunk carries a timestamp; the rest carry the deduced tag.

   That is what found the gap. `TimestampDeducer` was written in M2 and tested against golden
   sequences, and **nothing in the reader ever called it**. Every earlier test pushed samples one
   at a time, and a single push always carries a timestamp, so the reconstruction never ran. The
   first chunk returned five samples timestamped -1.0. A real liblsl inlet now reads chunks from
   this library and rebuilds the same values, and a regression test covers the path.
5. **59 of the 171 header symbols are still absent**, and a smaller group is present as a stub
   that returns an error code. The named gaps are the background resolver, the description tree
   beyond one level, `pull_sample_buf`, and the configuration file.

## 6d. The harness gains a chunk mode

**Status: complete (2026-08-02).** `oracle/interop.py --chunk N`, plus `--chunk` on both peers.
**24 of 24 cells across the six numeric formats.**

The harness pushed one sample at a time, and a single push always carries an explicit
timestamp. The reconstruction of a deduced timestamp therefore never ran in any cell, which is
why the missing `TimestampDeducer` call survived M2 through Phase 2 with every test green.

1. **The expectation is computed, not copied.** A chunk carries one timestamp for its last
   sample. The sender counts backward by `(count - 1) / rate`, writes only that one value, and
   the reader adds one period at a time for the rest. `expected_chunk_stamps` repeats that same
   arithmetic in the same order.
2. **The test is not vacuous, and the number says so.** Of 20 samples, **6 rebuilt timestamps
   differ bit for bit from the sent ones**, because accumulating `+0.01` five times is not the
   same double as multiplying by five. Both implementations reproduce the accumulated value
   exactly. The cell therefore checks the operation order and not only the values.
3. **The sample grid has to match the rate.** The harness generates timestamps spaced
   `1 / srate` in chunk mode. Any other spacing would make the rebuilt values differ from the
   sent ones for a reason that is not a defect, and the cell would fail on arithmetic instead of
   on behavior.
4. **The self test still reports a corrupt stream in chunk mode**, so the comparison is known to
   be able to fail here too.

**Known gap:** the chunk cells cover the six numeric formats. A string chunk is not covered,
because the peer builds a flat buffer of fixed-width values and a string has no fixed width.

## 6e. The description tree

**Status: complete (2026-08-02).** `crates/lsl-net/src/desc.rs`, `LSL:fullinfo` on both sides,
the tree calls of the C ABI, and a harness cell. 180 workspace tests, clippy clean.

Channel labels, units, and device names live in the description tree. Without it a stream is a
nameless block of numbers, and no recorder can label a channel. No code reached this part of the
core before now.

| Measure | Result |
|---|---|
| Byte-exact document against the oracle | 7 of 7 captures |
| Tree calls of the C ABI, real liblsl beside ours | 120 lines, no difference |
| Interop cells with a tree | 16 of 16 |
| pylsl checks, each library | 5 of 5, same output |

1. **The tree does not travel with a discovery answer.** `to_shortinfo_message` builds a fresh
   document with an empty `<desc />` (`src/stream_info_impl.cpp:160`). The tree travels only on
   the data port, in answer to `LSL:fullinfo`, and the close of the connection ends the answer
   (`src/tcp_server.cpp:508`, `src/info_receiver.cpp:60`). An inlet that reads only the
   discovery answer sees no labels at all.
2. **The layout rules are measured, not derived.** `oracle/descxml.cpp` builds trees through
   real liblsl and writes the document. `oracle/descread.cpp` feeds documents back in and writes
   what comes out. The two files fixed every rule in SPEC 12.3 and 12.4. Four questions each got
   one measured answer:

   - which characters are escaped in text
   - which characters are escaped in an attribute
   - when a tag stays on one line
   - what a read drops
3. **The number format was wrong and no test saw it.** The writer used a fixed count of
   decimals. That count matched a rate of 100 and no other rate. liblsl writes 16 significant
   digits and keeps every trailing zero (`src/util/cast.cpp:9`), so a rate of 0 wrote as
   `0.0000000000000` where liblsl writes `0.000000000000000`. Every reader accepts both, so
   every earlier test passed. The byte-exact comparison against a capture named the defect.
4. **The harness named my own expectation as the defect, with liblsl on both sides.** The first
   run failed in all four cells, and the control cell failed with the rest. A sender writes
   `<blank></blank>` for an empty value and `<hollow />` for a tag with no child. A reader drops
   a text node that holds nothing, so both arrive as `<hollow />`. The expectation described the
   sender, and the cell measured the receiver. **This is the work that the control cell does.**
   A control cell that fails says the harness is wrong, not the implementation.
5. **A carriage return does not survive the wire.** `captures/desc/desc_reparse.hex` measures
   it. `one\ntwo\rthree\tfour` returns as `one\ntwo\nthree\tfour`. That is liblsl behavior,
   and this implementation reproduces it.
6. **Three calls of the C ABI do not do what the header suggests.** `lsl_append_child_value`
   returns the parent and not the new tag. `lsl_value` returns nothing for a tag, because the
   text is a child node. `lsl_set_value` fails on a tag. `oracle/desctree.c` measures all three.
   A mutation that returned the child instead of the parent failed the comparison at once.
7. **The work found two more differences.** A stream description that no outlet published
   carries an empty session identifier, not `default` (`src/tcp_server.cpp:328`). A pull on an
   inlet that was never opened must start the reader and report "no sample" after its timeout,
   not a lost stream (`src/data_receiver.cpp:88`). `oracle/pylsl_check.py` found both. It runs
   the same five checks against each library and compares the output.

**Known gap:** an application can walk up from `<desc>` to a field of `<info>` and write there.
That write does not change the description, because the struct fields stay the authority for
`<info>`. Only a walk up from `<desc>` reaches those fields, and nothing writes through it.

## 6f. The whole C ABI, and the example programs of liblsl

**Status: complete (2026-08-03).** 165 of 165 symbols, the background resolver, and the
configuration file. 196 workspace tests, clippy clean.

Before this step **no C or C++ application was able to load this library**. pylsl worked,
because pylsl calls only the names that carry a timestamp and a pushthrough flag. `lsl_cpp.h` inlines
`push_sample(const float *)` to `lsl_push_sample_f`, which did not exist. The gap was invisible
to every test in the suite, because every test used pylsl or the peer.

| Measure | Result |
|---|---|
| Exported symbols | **165 of 165** |
| Example programs that link, both libraries | **21 of 21** |
| Example programs that run alone and write the same text | 2 of 2, byte for byte |
| Sender and receiver pairs, all four library combinations | **32 of 32**, 16 of them mixed |
| Configurations that both libraries read the same way | 7 of 7 |
| Tree calls of the C ABI | 120 lines, no difference |

### The acceptance test is upstream's own code

`oracle/examples.py` compiles the 21 programs in `liblsl/examples/` twice, once against each
library, and runs them. **Nothing in this project changed a line of them.** The harness asks
three questions, in order of strength:

   1. Does every program link?
   2. Does a program that runs alone write the same text?
   3. Does a sender of one library reach a receiver of the other?

`HandleMetaData.cpp` is the strongest single case. It builds an outlet with a channel tree,
resolves it, opens an inlet, asks for the whole description, and walks the tree with
`next_sibling`. That is the whole of section 6e, tested by a program written years before this
project, and the 83 lines of output are identical.

1. **The first run failed, and the difference was real.** liblsl printed `<v4address />` where
   this library printed `<v4address></v4address>`. A program builds the second form, because
   `write_xml` appends an empty text node. A reader drops that node, so the same field writes
   back as the first form. `inlet.info()` returns a description that came from a reader, so it
   must use the first form. SPEC 12.4 now carries the rule, and `StreamInfo` carries one flag
   that says where the description came from.
2. **A second difference stays open.** liblsl binds an IPv6 port and reports its number. This
   library binds none and reports zero. The harness masks the field and names the reason. IPv6
   is the largest piece of the protocol that this implementation does not speak.
3. **The work found a memory hazard on the way.** `lsl_get_xml` handed back a Rust `CString`, and
   liblsl hands back a `malloc` buffer that `lsl_destroy_string` frees with `free`
   (`src/common.cpp:53`). A C application is allowed to call `free` on the result, and that call
   corrupts the heap. Every string that this library hands out now uses `malloc`.
4. **A pull no longer needs an open.** `src/data_receiver.cpp:88` starts the reader on the first
   pull, so `lsl_open_stream` is optional. This library reported a lost stream instead.
   `oracle/pylsl_check.py` found it by running the same five checks against both libraries.
5. **`lsl_last_error` always returns an empty string**, on both. `src/common.cpp:57` declares a
   buffer of zeros inside the function and nothing ever writes to it. An implementation with a
   real message does not match.
6. **`lsl_library_version` returns 117, not 11700.** `src/common.h:49`. The value is the major
   number times 100 plus the minor number.

### The background resolver

`GetAllStreams.cpp` needs it, and it is what an application uses to show a live list of
streams. The list grows when a stream answers and shrinks when one stops
(`src/resolver_impl.cpp:151-168`). Two details matter. An expired entry is removed when the
application asks, not by the thread. And nothing on the wire says that a stream went away, so
only silence removes one. A live test publishes a stream, waits for it, stops it, and waits for
the list to empty.

### The configuration file

`api_config` was the largest untouched subsystem, and every value in it changes the wire. The
reader is 60 lines and its rules are unusual:

1. **A number sign does not start a comment.** Only a semicolon does
   (`src/util/inireader.cpp:13`). A file
   that starts with `#` holds a line with no `=`, the reader throws, and **every default
   applies**. A site can set `BasePort` and never learn that the value did nothing.
2. **Only the text `1` means yes** (`src/util/cast.hpp:10`). `ForceDefaultTimestamps = true`
   means no.
3. **A repeated key throws the whole file away.**

`oracle/configcheck.sh` runs one probe program against both libraries with seven files and
compares. All three rules above are measured, not read.

**Known gap:** the probe found one defect in itself first. The push arrived before the consumer
was registered, so liblsl dropped the sample. Six of seven cases then failed with liblsl on both
sides. The correction was `lsl_wait_for_consumers` in the probe, not a change to the library.

## 6g. A two machine test, and the defect it found

**Status: the defect is corrected (2026-08-03).** 199 workspace tests, clippy clean.

`oracle/labrecorder.sh` publishes a stream for a recorder on a second machine. It offers three
publishers, and the third exists only for attribution:

| Mode | What it uses |
|---|---|
| `oracle` | `liblsl/examples/SendData.cpp` against real liblsl. **The control.** |
| `capi` | the same program against this library |
| `rust` | `crates/lsl-net/examples/publish.rs`, no C ABI on the path |

The control passed and `capi` passed. **`rust` failed:** the stream appeared in the recorder and
in a plotting application, the connection opened, and no data ever plotted.

### What was wrong

**An outlet built through `lsl-net` sent a timestamp of zero on every sample.**

A pushed timestamp of zero means the current clock (SPEC 8.3). liblsl applies that rule inside
the outlet (`src/stream_outlet_impl.cpp:170`). This implementation applied it at the C boundary
only, in `push_typed`. Every caller that used the Rust API directly therefore sent a zero.

A consumer reads a timestamp of zero as **"no sample"**. `lsl_pull_sample_f` returns 0.0, and
pylsl turns that into `(None, None)`. So the channel values arrived correct and in order, and
no recorder and no plot was able to use one of them.

### Why the whole suite missed it

Every earlier test supplied a timestamp:

- the interop peer reads each timestamp out of a file, and no file holds a zero
- the golden vectors carry fixed values, and a generator that pushed 0.0 was corrected in M1 for
  a different reason
- pylsl and the example programs reach the outlet through the C ABI, which already applied the
  rule

**No test ever pushed a zero through `Outlet::push`.** The rule was implemented once, tested
through the path that did not need it, and absent from the path that did.

### What found it, and what now covers it

A person opened a recorder on another machine and saw an empty plot. The measurement that named
the defect was three lines of output: the values were right, the sample counter rose, and the
timestamp read `0.0`.

Two guards now cover the rule. `crates/lsl-net/tests/zero_timestamp.rs` tests one sample, a
chunk, and a value that must travel unchanged. That guard is the weaker of the two, because it
tests this implementation against itself.

The stronger guard is a cell of the harness. `--zero-stamps` makes the outlet push a zero, and
the comparison then tests the rule instead of the numbers: no timestamp may be zero, the first
must look like a clock reading, and the readings must rise. **16 of 16 cells pass, and the cell
was proved able to fail**: with the correction removed, the two cells that use the Rust outlet
report `8 of 8 timestamps are zero, which reads as no sample`, while the cell with the oracle
outlet still passes. The failure names the layer on its own.

`crates/lsl-net/src/outlet.rs` carries the rule where liblsl carries it.

### The three publishers now send one signal

The first version of the test used `SendData.cpp` for the two C modes, and that program sends
`rand()` noise. Two plots of it never agree, so a difference between the modes proved nothing.

`oracle/publish.c` replaces it. It sends the signal that
`crates/lsl-net/examples/publish.rs` sends: channel `k` carries a sine wave of `k + 1` Hz, the
last channel carries the sample number, and every value comes from that number rather than from
a clock or a random source. The description tree and the source identifier match as well.

`oracle/samesignal.sh` records from each publisher and compares. **199 samples present in all
three recordings, every channel value the same bit for bit.** A plot of one mode can therefore
be held against a plot of another, and a difference is a defect.

### A marker stream, and a second defect

Both publishers now send a marker stream beside the signal: one channel of text, no rate, type
`Markers`. One marker leaves at each whole second, which is where the sine of channel one crosses
zero. **A plot then shows each marker on a crossing.** An alignment error is visible there
without any measurement. The measured skew between a marker and its sample is 8 to 16
microseconds, under one part in five hundred of a sample period at 100 Hz.

Each mode tags its streams with its own name, so the three run at the same time and a recorder
shows which library sent which stream:

```
LabTest-oracle   LabTest-Markers-oracle
LabTest-capi     LabTest-Markers-capi
LabTest-rust     LabTest-Markers-rust
```

Six streams on one network, from three libraries, side by side in one recording. A plot then
holds the three against each other with no restart between them.

The marker stream needs a second outlet in one process, and that found a second defect.

**Only the first outlet held the port that carries a multicast query.** liblsl sets
`reuse_address` before it binds (`src/udp_server.cpp:60`). This implementation did not. The
second outlet still answered a unicast query, and a unicast query walks the whole port range, so
**every test on one machine passed**. Another machine sends a multicast query and finds the
signal without its markers.

`crates/lsl-net/tests/two_outlets.rs` now tests that both outlets hold the port and that a
resolver finds both streams. SPEC 1.1b carries the rule.

One error of method is worth recording as well. The first run of the marker test failed for the
C ABI, while the same program passed against liblsl. The cause was a stale `liblsl.so`. The
correction sat in `lsl-net`, and only `lsl-net` was rebuilt. **A comparison against the oracle is
worth nothing when the artifact under test is old.**

The timestamps are left out of that comparison on purpose. Each run starts at its own moment, so
they must differ. The `--zero-stamps` cell tests the timestamp rule instead.

1. **A test that compares only values passes a stream that no application can use.** The
   interop harness compares timestamps as well. It still missed this defect, because it never
   supplies the input that starts the rule.
2. **One rule, one place.** The rule was in `lsl-capi` and not in `lsl-net`. A library with two
   public entry points needs its rules below both of them.
3. **The three publishers earned their cost.** Two passes and one failure named the layer in one
   run. A single publisher shows one failure and no cause.

## 6h. The recording is the acceptance test

**Status: passed (2026-08-03).** `oracle/checkxdf.py`, `captures/verify.xdf`.

The three publishers tag their streams, so LabRecorder records all six into one file. That file
is the acceptance artifact: a real recorder, on a second machine, wrote it from three libraries at
once.

**The oracle streams inside the file are the control.** Real liblsl produced them and a real
recorder stored them. A fault in the reader therefore shows up in the control first, inside the
same file and the same session.

| Check | Result |
|---|---|
| Chunks parsed | 655, ending at byte 777,345 of 777,345 |
| Samples lost, any stream | **none.** The counter steps by 1 for every sample of all three |
| Channel labels, units, type, rate | the same in all three |
| Channel values | **4,219 samples compared, every one the same as the control, bit for bit** |
| Timestamps | mean gap 10.096 to 10.099 ms against a 10 ms period, none that fall |
| Markers | 61 in each, correct shape, every one on a zero crossing to 4 decimal places |

1. **The reader was written against the file, not against a description of the format.** Two
   measurements make it trustworthy. The parse ends exactly at the end of the file with no byte
   left over in any chunk, and the values it produces are the values that the publishers were
   told to send. A wrong reader fails both.
2. **`--selftest` damages the recording on purpose.** It removes a sample, changes a value,
   changes a label, makes a timestamp fall, and moves a marker off its crossing. All five are
   reported. A check that only ever passes proves nothing.
3. **The self test found a hole in itself first.** The first version changed a value at index 10
   of one stream, and the report stayed silent. The cause was correct behavior: the comparison
   covers the sample numbers that **every** stream holds, and index 10 of that stream sat outside
   that window. The damage now lands inside the window, and the report names the limit of the
   comparison in its own output.

### The long run

A second recording ran for **7.81 hours**. The reader was rewritten to walk the file and keep
counters rather than samples, so the size of a recording no longer decides whether the check can
run. A 355 MB file with 289,702 chunks reads in 39 seconds.

The comparison changed as well, and it grew stronger. Every value comes from the sample number,
so the expected value of any sample is computed from the counter channel rather than taken from
another stream:

    channel k  =  float32(100 * sin(2 * pi * (k + 1) * n / rate))
    last       =  float32(n)

A comparison between two streams covers only the samples that both hold. This covers every
sample of every stream. **The control proves the rule**: real liblsl wrote the oracle stream, so
a wrong formula fails there first.

| Measure | oracle | capi | rust |
|---|---|---|---|
| Samples | 2,809,974 | 2,810,333 | 2,810,312 |
| Hours | 7.81 | 7.81 | 7.81 |
| **Samples lost** | **0** | **0** | **0** |
| **Values that break the rule** | **0** | **0** | **0** |
| Timestamps that fall | 0 | 0 | 0 |
| Mean gap between timestamps | 10.1040 ms | 10.1027 ms | 10.1028 ms |
| Longest gap | 12.73 ms | 11.79 ms | 11.76 ms |
| Markers | 28,100 | 28,104 | 28,104 |
| Markers off a zero crossing | 0 | 0 | 0 |

**8.4 million samples across three libraries, and not one of them differs.** The longest gap
between two timestamps was 12.73 ms against a period of 10 ms, and the worst of the three
belongs to liblsl.

**What the recording does not answer.** It holds no measure of memory. A publisher that grew
without limit for eight hours and never failed is weak evidence and not a measurement. A soak
that watches the size of each process is a separate test.

One number needs an explanation. The capi run matched 28,103 of its 28,104 markers to a sample.
The last marker names a second whose sample the recorder never stored, because the recording
stopped between the two. Nothing is missing from the stream.

**What the short recording does not cover.** The value comparison there held one stream against
another, so it spanned only the window that all three shared, 4,219 of about 6,092 samples. The
rule above removed that limit.

## 6i. IPv6, and the address that was never carried

**Status: complete (2026-08-03).** 209 workspace tests, clippy clean.

### The larger defect came first

`StreamInfo` held no address at all, and the inlet named `127.0.0.1` in three places: the
handshake, the request for the whole description, and the clock probe. **An inlet of this
implementation was able to reach an outlet on its own machine and nowhere else.**

Nothing found it, and nothing was going to. Every test in the project runs on one machine. Every
live test across the network used our outlet with the inlet of liblsl, so the reverse direction
had never once been tried.

An outlet leaves the address empty, and the resolver of the peer fills it in from the sender of
the answer (`src/resolve_attempt_udp.cpp:135-140`). `StreamInfo` now carries `v4address` and
`v6address`, the resolver stamps them, and every connection uses them.

**The guard is a negative test.** A test that names a real address of this machine passes even
when the address is ignored, because the loopback reaches the same outlet.
`crates/lsl-net/tests/address.rs` therefore also names `192.0.2.1`. RFC 5737 keeps that range
for documentation, and no machine holds it. An inlet that falls back to the loopback opens the stream, and the test
fails. With the correction removed, it does.

### IPv6

| Piece | What it does |
|---|---|
| `ports.IPv6` | `disabled` leaves IPv4 alone, `forced` leaves IPv6 alone |
| Multicast groups | each scope adds `FF02:`, `FF05:`, `FF08:`, or `FF0E:` before the suffix |
| Outlet | one TCP acceptor and one unicast UDP server for each family, each with its own port |
| Multicast responder | one for each family, both on the multicast port |
| Resolver | one leg for each family, sending only to the groups of that family |
| Inlet | takes IPv4 when the description holds one, IPv6 when it does not |

**Measured against real liblsl with `IPv6 = force` on both sides: 12 of 12 interop cells pass**,
for float32, int32, and string, in both directions.

1. **The two families hold different ports.** Each stack binds its own port out of the same
   range, so a stream reports `v4data_port` 16572 and `v6data_port` 16573. A reader that assumes
   one port for both cannot connect.
2. **A link-local address needs its interface number.** Nothing in liblsl writes that down. The
   first run of the IPv6 interop failed with `Invalid argument` on every cell that used our
   inlet. `Ipv6Addr::to_string` in Rust drops the `%` and the number. The text that asio writes
   keeps them. An address of the form `fe80::1` names no machine on its own. This is
   measured behavior, not a rule read in the source, and SPEC 1.1 now records it.
3. **The IPv6 multicast socket must refuse IPv4.** A socket that accepts both takes the
   multicast port from the IPv4 responder, and one of the two families then goes unanswered.
4. **A join names an index, not an address.** The list comes from `/proc/net/if_inet6`, and
   index zero is tried as well, because that is what lets the operating system choose.

### Two machines, IPv6 alone

`oracle/labrecorder.sh --ipv6` writes a configuration that leaves IPv6 alone and points the
library at it. The publishers then bind **no IPv4 socket**, so a consumer has no other way in.
**Nothing changes on the second machine**, which removes the usual doubt about whether a
setting there took effect.

`captures/ipv6.xdf` holds the result. Every stream in the file reports `v4data_port` of 0:

```
IPv6Test-oracle          v4data=0  v6data=16572
IPv6Test-capi            v4data=0  v6data=16574
IPv6Test-rust            v4data=0  v6data=16576
IPv6Test-Markers-oracle  v4data=0  v6data=16573
IPv6Test-Markers-capi    v4data=0  v6data=16575
IPv6Test-Markers-rust    v4data=0  v6data=16577
```

A recorder on a second machine found all six and stored them. Discovery and the data port
therefore both worked over IPv6. Nothing else was on offer.

| Check | Result |
|---|---|
| Samples | 7,084 to 7,088 in each stream |
| Samples lost | **none** |
| Values that break the rule | **none** |
| Timestamps that fall | 0 |
| Mean gap | 10.0908 to 10.0965 ms against 10 ms |
| Markers | 71 each, none off a zero crossing, none with the wrong word |

**The port numbers are the evidence.** A recording that shows only samples does not separate
IPv6 from IPv4. The file records what each outlet published, and every one of them offered IPv6
and nothing else.

## 6j. Protocol 1.00 stays out, and the refusal is now measured

**Status: complete (2026-08-03).** `oracle/refuse100.py`.

Protocol 1.00 is not a different protocol. It is the same exchange with a shorter handshake and
a different encoding. The request line carries no version and no UID. The client then sends one line with the buffer
length and the chunk length, and the server answers with no status line at all
(`src/data_receiver.cpp:256-258`, `src/tcp_server.cpp:679-689`). Every sample then travels
through `eos::portable_archive`, a Boost archive vendored inside liblsl at
`src/portable_archive/`, 872 lines of headers pinned to archive version 9.

`LSL_PROTOCOL_VERSION = 110` was already in the first commit of the current repository, dated
2015-05-05, and has never changed. Any liblsl of the last decade speaks 1.10. **The decision is
to leave 1.00 out.**

### A decision still has to behave

The plan already claimed that correct refusal is a conformance property. **Nothing measured it**,
and when something finally did, both directions were wrong.

1. **Our inlet decoded an archive as 1.10.** The client tested for a version that is too new and
   never for one that is too old. A liblsl outlet set to 1.00 answers a 1.10 request with
   `Data-Protocol-Version: 100`, and the feed then failed with `BadTag(127)`, which names the
   wrong thing entirely. The client now stops on the version and says so.
2. **Our outlet answered 505, which means something else.** liblsl sends 505 only when the
   **major** version differs (`src/tcp_server.cpp:576`). A request for 1.00 against a 1.10 server
   is one that liblsl serves. There is no code in this protocol for "I cannot write that
   version", so the outlet now closes the connection with nothing written. A peer then reads the
   end of the stream, which every client already handles.

| Pair | Result |
|---|---|
| liblsl on both sides, both at 1.00 | the feed works, so the configuration took effect |
| our inlet, a liblsl outlet at 1.00 | stops in 0.5 s, naming the version |
| a liblsl inlet at 1.00, our outlet | the outlet closes, and the peer stops on its own timeout |

**The cost, stated plainly:** a peer configured with `tuning.UseProtocolVersion = 100` cannot use
this library. It waits for its own timeout and then reports one.

### The coverage figure was wrong

`sync_serialization.h` was counted as protocol 1.00. Reading it, it is byte swapping for sync
mode and has nothing to do with 1.00. The real 1.00 surface, `src/portable_archive/`, was never
counted at all. Two honest numbers replace the single wrong one:

| Denominator | Lines | Built |
|---|---|---|
| What this project chose to build | 8,888 | **86.8%** |
| The whole of liblsl, 1.00 and sync mode included | 9,854 | **78.3%** |

## 6k. The blocking mode

**Status: complete (2026-08-03).** 212 workspace tests, clippy clean.

`transp_sync_blocking` reads like a second protocol and is not one. Three facts settle it. The
socket moves to the blocking writer **after** the status line, the headers, and the test pattern
(`src/tcp_server.cpp:733`). The layout of a sample does not change
(`src/sync_serialization.h`). The back-dating of a chunk happens before the two modes divide
(`src/stream_outlet_impl.h:259-261`). **A consumer cannot tell the two modes apart.**

That was measured before anything was built. `oracle/peer.cpp` gained `--sync`, and a matrix with
liblsl on both sides passed every cell. Only then was the mode worth implementing. The work was small: the flag,
the refusal of strings, and a writer that holds the socket instead of a queue.

| Measure | Result |
|---|---|
| Interop with the blocking outlet, against liblsl | **16 of 16 cells**, four formats, both directions |
| A stream of strings | refused, with the reason in the message |
| Consumers of a blocking outlet | counted through the writer, not the queue |
| Samples lost to a slow reader | none |

1. **The flag was silently ignored.** `lsl_create_outlet_ex` took `_flags` and dropped it. An
   application that asked for the blocking mode got the queue instead, and nothing said so. The
   wire was right, so no test of the wire finds it.
2. **A test can measure the wrong side.** The first version of the loss test asked for a buffer
   of one sample and received three of two hundred. Every one of them was lost in the **inlet**,
   because the number that an inlet sends as `Max-Buffer-Length` sizes its own queue as well.
   The two sides cannot be told apart through the public calls, and the test now says so instead
   of pretending to measure what it cannot.

## 6l. The query language

**Status: complete (2026-08-03).** `crates/lsl-net/src/xpath.rs`, 218 workspace tests, clippy
clean.

Risk R1 stood open from the first week. Query matching is XPath 1.0 through pugixml
(`src/stream_info_impl.cpp:214`). The fear was that an implementation needs a whole engine.

**The battery answered that before any code was written.** `oracle/xpath.py` sends 42 queries to
a real outlet and records which ones match. That measurement turned an unbounded risk into a
list, and the list is short: boolean operators, six comparisons, seven string functions, four
number forms, and three shapes of path. A description holds elements and text and nothing else,
so attributes, predicates in brackets, axes, variables, and the union operator never appear.

### The result

| Measure | Before | After |
|---|---|---|
| Queries answered as liblsl answers them | 21 of 42 | **42 of 42** |
| Queries where we differ | 21 | **0** |
| Dependencies added | | none |

The reader is a lexer, a parser of eight levels, and an evaluator, in one file. It follows XPath
1.0 where the battery reaches. A comparison against a set of nodes is true when **any** node
satisfies it. A relational operator always compares numbers. A query that cannot be read matches
nothing.

1. **The failure that mattered was silent.** A query beyond equality returned false, and on the
   wire that is the same as "this stream does not match". liblsl answers the same way for a
   query it cannot parse (`src/stream_info_impl.cpp:239-241`). **A comparison of two libraries
   therefore shows nothing**, unless the test knows what the query means. The battery knows.
2. **Two old tests asserted the limitation.** They named queries that "must not match" and they
   passed for months. They were not wrong when they were written, and they were the last thing
   standing between the old behavior and the new one. They now assert the oracle's answer
   instead.
3. **A path starts at `<info>`, not above it.** `info/name='XpTest'` matches nothing, because it
   looks for an `<info>` inside `<info>`. That is measured. A reading of the standard alone
   gets it wrong.

## 6m. What the processes hold

**Status: measured (2026-08-03).** `oracle/memsoak.py`, `artifacts/memsoak-15min.csv`.

A recording says nothing about memory. Every sample can be right while the process that sent it
grows without limit, and the failure arrives hours later, in the middle of a session. The soak of
section 6h proved that no sample was lost. **It says nothing about what the publishers held
while they did it.** This was the one claim on the list with no evidence either way.

Three publishers ran side by side, one for each library, with a consumer reading from the Rust
outlet. The resident size of each process was read every 30 seconds for 15.5 minutes.

| Process | First | Last | Distinct values in the whole run |
|---|---|---|---|
| liblsl publisher | 6.27 MB | 6.27 MB | **one** |
| our publisher, C ABI | 4.22 MB | 4.22 MB | **one** |
| our publisher, `lsl-net` | 3.74 MB | 3.86 MB | three, all inside the first two minutes |
| our consumer | 2.95 MB | 3.07 MB | three, all inside the first two minutes |

The two liblsl-shaped processes never moved at all. The two Rust processes rose by 120 kB during
the first ninety seconds and then held one value for the remaining fourteen minutes. That is a
process reaching its working size, not a leak.

**What the run rules out.** Each stream pushed about 93,000 samples. A leak of 100 bytes for
each sample shows as 9.3 MB, and one of 10 bytes as 0.93 MB. Neither appeared. A leak that grows
with the number of samples is therefore ruled out above roughly ten bytes a sample.

**What it does not rule out.** A leak slower than that. Two paths were never touched either. The
map of a background resolver is pruned only when an application asks for the results
(`src/resolver_impl.cpp:151-168`), and a recovery builds state each time a source restarts.

### The threshold was the defect

A two minute run of the same tool reported **a leak in both Rust processes**, at 3.25 MB an hour.
There was none. A single step of 100 kB during the warm up, drawn across two minutes, reaches
that number. The control read exactly zero, because liblsl had already settled.

Three corrections followed. The first tenth of the readings is left out of the line. A verdict
now needs two signals to agree, a slope far above the control **and** five megabytes of real
growth after the warm up. A run under ten minutes refuses to give a verdict and says why.

**Without those corrections the report is a false accusation** against the implementation, from
a tool that measured nothing but a warm up.

## 7. What the suite told us

The suite was built first so that it could scope the rewrite. It did, and the answers are below.
Each one was an open question when this plan was written.

- **XPath (R1) was the largest single unknown.** The battery measured it: 42 queries, every
  group of XPath 1.0 answered by the oracle. That turned an unbounded risk into a list, and the
  list was short enough to read in one file. Section 6l.
- **The 1.10 codec surface is 42 golden vectors and 576 samples**, bit-exact. Nothing about it
  was guesswork.
- **Most defects are silent.** This is the finding that outlasts the others. A pushed timestamp
  of zero, a hardcoded loopback address, a flag that was accepted and dropped, a multicast port
  that only the first outlet held: each produced correct-looking output and passed every test
  that existed. **None of them was found by a test written from the source.** They were found by
  disagreement with the oracle, or by real software on a real network.
- **A measuring tool is as likely to be wrong as the thing it measures.** Three times a failing
  result turned out to be the instrument: a config probe that raced its own consumer, an XPath
  battery reading streams left over from an earlier run, and a memory soak that mistook a warm
  up for a leak. Each would have been a false accusation if reported without a control.

The rewrite is scoped honestly now, because none of the above is a guess.

---

## 8. Where this ended

**In scope: 8,888 lines of liblsl, about 88 percent built.** Out of scope by choice: protocol
1.00 and the sync-mode byte swap, 966 lines.

| Evidence | Measure |
|---|---|
| Workspace tests | 218 |
| C symbols exported | 165 of 165, none a stub |
| Golden vectors, transcripts, time sequences | 42, 22, 10 with 96,008 samples |
| Interop cells | 28 default, 16 blocking, 12 IPv6, plus chunk, description, and timestamp cells |
| XPath queries answered as liblsl answers them | 42 of 42 |
| Example programs of liblsl that link and agree | 21 of 21 |
| Longest recording | 7.81 hours, 8.4 million samples, nothing lost |
| Memory after warm up | flat, below liblsl |
| Third-party applications | pylsl, LabRecorder, and Phasic, none of them changed |

**What is not done:** protocol 1.00, a second machine pair, and any claim about a network other
than the one measured here.
