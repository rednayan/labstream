# Proposal: feature negotiation between two labstream peers

**Status:** proposal. The header and the `seq` feature are implemented on
the branch `feat/seq`. The `resume` section is a spec with no code. The
other features are not implemented. `docs/roadmap.md` gives the order of
the features. SPEC.md gets
a section for a feature only after the feature is accepted.

## The purpose

Some improvements need a change to the data channel. Examples are sequence
numbers that make sample loss visible, and a shared-memory transport for two
processes on one machine. liblsl does not know these changes.

This page defines how a labstream inlet and a labstream outlet agree to use
such a feature. The agreement must leave every other pair unchanged:

| Outlet | Inlet | Result |
|---|---|---|
| liblsl | liblsl | plain LSL, as today |
| liblsl | labstream | plain LSL |
| labstream | liblsl | plain LSL |
| labstream 0.1.2 or earlier | labstream | plain LSL |
| labstream | labstream | plain LSL, plus each feature that both ends accept |

## What liblsl does with an extra header

These facts come from `conformance/liblsl/` at `e651023c`.

1. The feed server reads each request header line, and acts only on the keys
   that it knows. An unknown key has no effect
   (`src/tcp_server.cpp:605-633`).
2. The feed client reads each answer header line the same way, and acts only
   on the keys that it knows (`src/data_receiver.cpp:215-253`).
3. Both sides cut a header line at the first `;`, and convert the line to
   lowercase before they compare the key (`src/tcp_server.cpp:610-613`,
   `src/data_receiver.cpp:220-223`).
4. Both sides read a header line into a buffer of 16384 bytes
   (`src/tcp_server.cpp:604`, `src/data_receiver.cpp:193`).
5. The labstream parser cuts and converts the same way
   (`crates/labstream-proto/src/header.rs:103-107`).

As a result, a liblsl outlet ignores an extra request header, and a liblsl
inlet ignores an extra answer header. Item 3 limits the syntax of the header.
A value must not contain `;`, and a value must not depend on letter case.

## The header

### The request

A labstream inlet adds one header line to its feed request:

```
Labstream-Features: seq/1 shm/1
```

The name of the header is `Labstream-Features`. This name is final. A later
version of labstream must not change it, because peers of different versions
must agree on it.

The value is a list of tokens with one space between two tokens. Each token
has this form:

```
token   = name "/" version [ "=" param ]
name    = 1*( a-z / 0-9 / "-" )
version = 1*( 0-9 )
param   = 1*( 0-9 / a-f )
```

A parameter is hexadecimal, because the header line loses its letter case.
The whole line must stay below 16384 bytes. An inlet can offer more than one
version of a feature, for example `seq/1 seq/2`.

### The answer

A labstream outlet that reads the request header adds one line to its answer:

```
Labstream-Features: seq/1
```

The answer holds only tokens from the request. For each name, it holds at
most one version.

### The rules

1. An inlet sends the request header only when it supports one or more
   features.
2. An outlet sends the answer header only when the request held one.
3. An outlet ignores a token that it does not know, or that does not match
   the syntax.
4. An inlet ignores an answer token that it did not offer.
5. A feature is on only when the inlet offered it and the outlet answered it.
6. If the answer has no `Labstream-Features` line, no feature is on. A liblsl
   outlet and an older labstream outlet give this result.
7. The configuration key `labstream.Features` lists the features that this
   process can use, for example `{seq}`. The default lists every feature that
   the library has. An inlet offers only the listed features, and an outlet
   answers only the listed features. The value `{}` turns every feature off.
   liblsl does not read this key.

## Where the framing changes

After the answer headers, the outlet sends two test pattern samples
(SPEC.md 6). These two samples always use the plain framing of SPEC.md 7.
The inlet compares them exactly as today.

A feature can change the framing only from the first byte after the second
test pattern sample. Thus the test pattern stays a check of the codec, and a
fault in a feature cannot hide a fault in the codec.

## The scope of an agreement

An agreement holds for one connection. A recovery opens a new connection with
a new request, so the set of features can change. An outlet that restarts as
a liblsl program gives an example.

The inlet must report the features of the current connection, and must show
when the set changes. This works the same way as the clock reset after a
recovery. The API names are an open question.

## The first features

Each feature below gets its own section and its own conformance checks
before any code. The text here gives only the requirements that affect the
negotiation.

### `seq`: sequence numbers

**Status:** implemented on the branch `feat/seq`
(`crates/labstream-proto/src/seq.rs`). The token is `seq/1`.

#### The purpose

LSL has no sign of a lost sample (SPEC.md 8.7). An application can only
guess a loss from a gap in the timestamps. `seq` gives each sample a number,
so the inlet can count the samples that it did not receive.

#### The numbers

1. The outlet gives each pushed sample the next number of one counter. The
   counter starts at 0 when the outlet opens.
2. One counter serves the whole outlet, not one counter for each connection.
   A sample that the outlet pushes with no consumer also takes a number.
3. The two real causes of loss are a full consumer queue in the outlet
   (SPEC.md 8.7) and the gap between a lost connection and the next one. A
   counter for each connection starts again after a recovery, so it shows
   neither cause. That is the reason for rule 2.

#### The record

When `seq/1` is on, a numbering record can stand between two samples. It
has 9 bytes:

```
u8  tag      0xFE
u64 number   little-endian, the number of the next sample
```

1. The tag values 1 and 2 start a sample (`src/sample.h:19-20`, SPEC.md
   7.1). The tag `0xFE` is not a sample tag, so a reader can tell a record
   from a sample by its first byte.
2. The number is always little-endian. The `Byte-Order` of the connection
   does not apply to it.
3. On each connection, the first record comes directly after the second test
   pattern sample. The outlet writes it at once, before any sample, with the
   number that the next sample will get. Each connection thus starts with an
   absolute number, and a gap before the connection shows at once, also on
   a stream that sends no more samples.
4. After that, the outlet writes a record only before a sample whose number
   is not one more than the number of the sample before it. A connection
   with no loss carries one record and no more.
5. A record is not a sample. It does not count toward `Max-Chunk-Length`.
6. A record that is cut at the end of a read is not an error. The reader
   waits for the rest of the bytes, as for a cut sample.

#### The count

The inlet keeps the number that it expects next.

1. The first record after the inlet opens sets the expected number, and
   counts no loss. The inlet has no number from before that point.
2. A later record with a number above the expected number adds the
   difference to the count of lost samples.
3. A later record with a number below the expected number counts no loss.
   The inlet adds one to a separate count of numbers that went back, and
   takes the new number. The arithmetic must not underflow.
4. Each sample adds one to the expected number.
5. A recovery to the same outlet (the same `uid`) keeps the expected number.
   The first record of the new connection then shows the gap. Live test:
   `a_gap_shows_at_once_on_a_quiet_stream`.
6. A recovery to a new outlet (a new `uid`) starts again at rule 1, because
   the new outlet has a new counter. Live test:
   `a_new_outlet_starts_a_new_count`.

#### What the inlet reports

- `Inlet::samples_lost()` in `labstream-net` and `Inlet::lost()` in
  `labstream` give the count of lost samples since the inlet opened.
- The value is `None` when the current connection does not use `seq`. A
  liblsl outlet gives this result. `None` means "unknown". It does not mean
  "no loss".
- Version 1 gives a count only. It does not give the position of each gap.
  An application that must place a gap still uses the timestamps.
- The inlet queue also drops the oldest sample when the application reads
  too slowly. This drop happens after the count, so `seq` does not see it.
  `Inlet::samples_dropped()` in `labstream-net` and `Inlet::dropped()` in
  `labstream` report it, as before. The total loss is the sum
  of the two values.

### `resume`: send the samples of a dropout again

**Status:** proposal, not implemented. The token is `resume/1`.
`docs/roadmap.md` gives the success criterion.

#### The purpose

A connection can end while the outlet stays alive. Examples are a cable
pull, a Wi-Fi dropout, or a reset of a switch. The inlet then connects again
to the same outlet. Each sample that the outlet pushed between the two
connections is lost. `seq` counts this loss. `resume` prevents it.

#### What liblsl does

These facts come from `conformance/liblsl/` at `e651023c`.

1. The outlet makes a new consumer queue for each feed connection, after
   the headers (`src/tcp_server.cpp:746`).
2. A new consumer queue is empty. Its constructor registers it with the send
   buffer (`src/consumer_queue.cpp:10-20`).
3. A push goes only to the queues that are registered at that time
   (`src/send_buffer.cpp:20-23`).
4. The destructor of a consumer queue removes it from the send buffer
   (`src/consumer_queue.cpp:22-24`).

Thus no queue holds a sample that the outlet pushes between the end of one
connection and the start of the next. liblsl has no history for each outlet.

#### The scope

1. `resume` covers a lost connection to a live outlet. The outlet has the
   same `uid` before and after the dropout.
2. `resume` does not cover a restart of the outlet. A liblsl outlet gets a
   new `uid` when it starts (`src/tcp_server.cpp:329`). A labstream outlet
   makes a new `uid` when the description has none
   (`crates/labstream-net/src/outlet.rs:142-143`).
3. A feed request with a wrong `uid` gets `404 Not found`
   (`src/tcp_server.cpp:585-588`). Thus a resume request cannot reach a
   different outlet.
4. An application can start a new labstream outlet with an old `uid`. The
   new outlet then has a new counter and an empty history. Resend rule 3
   covers this case.

#### The rules for the negotiation

1. `resume/1` is on only when `seq/1` is also on for the same connection.
   The numbers of `seq` identify the samples to send again.
2. On the first connection, the inlet offers `resume/1` with no parameter.
3. On a recovery to the same `uid`, the inlet offers `resume/1=<number>`.
   The number is the next number that the `seq` count expects, in
   hexadecimal, as the header syntax requires. Example:
   `Labstream-Features: seq/1 resume/1=1f4a0`.
4. On a recovery to a new `uid`, the inlet offers `resume/1` with no
   parameter. The `seq` count starts again (`seq` count rule 6).
5. The outlet answers `resume/1` with no parameter. The first `seq` record
   of the connection gives the number of the first sample that the outlet
   sends.
6. A blocking outlet (`transp_sync_blocking`) has no queue and no history.
   It does not answer `resume/1`.
7. Today `answer()` in `crates/labstream-proto/src/features.rs` ignores each
   offered token with a parameter. For `resume`, the outlet must accept a
   parameter. The other features keep the current rule.

#### The history

1. The outlet keeps one history for the whole outlet, not one history for
   each connection. The history holds the last samples, with their `seq`
   numbers, in the order of the pushes.
2. The history also holds a sample that the outlet pushes with no consumer.
   The dropout is the time with no consumer.
3. The outlet starts the history when the first consumer agrees to
   `resume/1`. Before that point, the history costs nothing. An outlet that
   serves only liblsl inlets never keeps a history.
4. Two limits set the size of the history. The outlet removes the oldest
   sample when it reaches a limit:
   - `labstream.ResumeSeconds`: the time of samples at the nominal rate.
     The default is 10 s. A stream with an irregular rate uses the memory
     limit only.
   - `labstream.ResumeMemory`: the memory in MiB. The default is 64 MiB.
5. The default of 10 s covers a half-open connection. On Linux, the
   keepalive of labstream ends such a connection after 6 s
   (`crates/labstream-net/src/keepalive.rs:29`). The resolve of a recovery
   then takes up to 2 s (`find_again` in `crates/labstream-net/src/inlet.rs`).
6. The liblsl watchdog waits 15 s by default (`src/api_config.cpp:303`). A
   dropout that only the watchdog detects is longer than the default
   history. The `seq` count then shows the samples that the history did not
   hold.
7. A `Value` takes 32 bytes today. At 4000 Hz x 64 channels, one second of
   history takes about 8 MB, so 64 MiB holds about 8 s. The flat sample
   store of `docs/roadmap.md` (the flat typed API step) decreases this cost
   by about 8 times.

#### The resend

The outlet reads the requested number `R` from the token. Let `O` be the
oldest number in the history, and `N` the number that the next push gets.

1. If `O <= R <= N`, the new consumer queue starts with the history samples
   from `R` to `N - 1`. Then the live samples follow.
2. If `R < O`, the queue starts at `O`. The first `seq` record gives `O`.
   The inlet counts `O - R` lost samples, by `seq` count rule 2.
3. If `R > N`, or if the history is empty, the outlet sends no history. The
   first `seq` record gives `N`. The inlet then applies `seq` count rule 2
   or rule 3.
4. The copy of the history into the queue and the registration of the queue
   are one step under the lock of the fanout. Thus no number is missing and
   no number repeats between the history and the live samples.
5. The outlet never sends a number below `R`. Thus the inlet never gets a
   sample twice.
6. The queue keeps its size from `Max-Buffer-Length`. If the resend is
   longer than the queue, the queue drops the oldest samples, as in SPEC.md
   8.7. The `seq` record before the next sample shows this drop as a gap.
7. A resent sample keeps its original timestamp. The next section gives
   the rules.

#### The timestamps of a resend

An outlet sends a chunk with one timestamp. Only the first sample of the
chunk carries timestamp bytes. Each later sample carries the deduced tag
(SPEC.md 9.2, `chunk_timestamps` in `crates/labstream-net/src/outlet.rs`).
The inlet calculates a deduced timestamp from the sample before it
(SPEC.md 8.1). A resend can start in the middle of a chunk. The inlet then
has no correct sample before the first resent sample.

1. The history holds a full timestamp for each sample. It holds no deduced
   tag.
2. The outlet calculates the full timestamp at the push, in the order of
   the `seq` numbers. It uses `TimestampDeducer` in
   `crates/labstream-wire/src/lib.rs`. The inlet uses the same code, so the
   two results are equal bit for bit.
3. One deducer serves the whole outlet. It starts when the outlet opens, as
   the `seq` counter does. It also runs before the history starts (history
   rule 3). Otherwise a history that starts in the middle of a chunk holds
   wrong timestamps. The cost is one addition for each sample. A blocking
   outlet has no history and needs no deducer.
4. Each resent sample carries its full timestamp on the wire (tag 2 of
   SPEC.md 7.1). A resent sample thus has 8 more bytes than a deduced
   sample.
5. The live samples after the resend keep their tags. The first live sample
   can carry a deduced tag. The inlet calculates its timestamp from the last
   resent sample, which is correct.
6. The live path does not change. A connection with no resend carries the
   same bytes as with `seq/1` only.
7. A connection that starts in the middle of a chunk gets wrong deduced
   timestamps until the next chunk starts. liblsl has the same behavior
   (SPEC.md 8.4). A resend does not have this fault.

The framing does not change. The `seq` record and the sample encoding of
SPEC.md 7 carry the resend.

#### What the inlet reports

- `Inlet::lost()` counts only the samples that the history did not hold.
- `Inlet::resent()` in `labstream`, and `Inlet::samples_resent()` in
  `labstream-net`, give the count of samples that came from a history since
  the inlet opened. These names are a proposal.
- The value is `None` when the current connection does not use `resume`.

#### The conformance plan

1. **liblsl outlet with a parameter.** Extend `oracle/headerprobe.py` to
   send `seq/1 resume/1=1f4a0`. Make sure that the answer and the data bytes
   are the same as for a request with no header.
2. **liblsl inlet.** Make sure that a labstream outlet keeps no history when
   only liblsl inlets connect. A unit test reads the size of the history.
3. **Released labstream 0.1.2.** Run it as a peer in both directions. Each
   run must give plain LSL.
4. **The `seq` build of #12.** Run it as a peer in both directions. It
   answers `seq/1` and does not answer `resume/1`. Make sure that the inlet
   counts the loss of a dropout and gets no resend.
5. **The configuration.** `labstream.Features = {resume}` without `seq`
   gives no resume, on the inlet and on the outlet.
6. **The resend rules.** Write a unit test for each case of the resend: `R`
   in the history, `R` below it, `R` above `N`, and an empty history. One
   more test sends a resend that is longer than `Max-Buffer-Length`.
   One more test starts a resend in the middle of a chunk. Each resent
   sample must carry tag 2, and each timestamp must be equal bit for bit to
   the timestamp that an inlet with no dropout calculates.
7. **A live dropout.** The outlet ends each consumer connection, and pushes
   for 3 s. The inlet must get each sample one time, in order, and
   `Inlet::lost()` must give 0. Run at 512 Hz x 8 channels and at
   4000 Hz x 64 channels. The test must fail with `resume` off.
   The outlet pushes chunks of more than one sample. A second inlet with no
   dropout must get the same values and the same timestamps, bit for bit.
   Run the same test on a marker stream with string values and an irregular
   rate.
8. **A dropout longer than the history.** Set `labstream.ResumeSeconds` to
   1. The loss count must be exact.
9. **The interop matrix.** `oracle/interop.py` must give the same result as
   before, with `resume` on.
10. **Latency and CPU.** Run `latency.py` with `resume` on against the `seq`
    build. The history adds one copy for each push.

#### Open questions

- The final names of the API for the resend count.
- How the application sees that a group of samples is a resend. These
  samples arrive late, and a viewer must put them in the past.
- A request for the history on the first connection. A new viewer can then
  show the last seconds at once. Version 1 sends a history only after a
  recovery. A token such as `resume/1=0` cannot give this request, because 0
  is a valid number.
- The memory count for string values. A string has no fixed size.
- The clock offset after a recovery. The inlet resets the clock state after
  a recovery. The resent samples arrive before the first new offset. The
  correct offset for these samples needs a test.

### `shm`: shared memory on one machine

The hostname does not prove that the two processes share a machine. Two
machines can have the same name, and a container can have the name of its
host. Thus `shm` needs one more step after the headers:

1. The inlet offers `shm/1`.
2. The outlet makes a memory segment. It writes a random value into the
   segment. It answers `shm/1=<segment id>`.
3. The inlet opens the segment and reads the random value.
4. The inlet writes one line on the TCP connection: `shm-ok <value>` or
   `shm-no`.
5. If the value matches, the outlet sends the samples through the segment.
   Otherwise, or after 1 s with no line, the outlet sends the samples on TCP.

The outlet does not send data before step 5. The TCP connection stays open
in both cases. It carries the end of the stream and the recovery.

The segment must allow access only by the user that owns the outlet. The
wake-up method and the code for each platform are not part of this page.

### A clock model with drift

A fit of offset and drift is a choice of the inlet alone. The outlet answers
the same time probes as today. Thus this feature needs no negotiation, and
it does not use this header.

## Alternatives that this proposal rejects

**A higher `Data-Protocol-Version`, for example 120.** liblsl uses the lower
of the two versions (`src/tcp_server.cpp:641`). Thus a liblsl peer falls back
correctly. But the version numbers belong to liblsl. A future liblsl release
can use the same number for a different format, and the two ends then agree
on different things. A version is also one scale, and the features are
independent.

**A new request line, for example `LSL:labstream-feed`.** A liblsl server
sends no answer to a request line that it does not know
(`src/tcp_server.cpp:502-531`). The inlet then needs a timeout and a second
connection for each liblsl outlet. That adds a delay to each start and to
each recovery.

**A field in the stream description.** The description goes to every
consumer, and LabRecorder writes it into each XDF file. A field there changes
the stored data, and it does not describe one connection.

## The conformance plan

Each item gives its status on the branch `feat/seq`.

1. **liblsl outlet, raw request.** Send a feed request with the header to the
   oracle outlet. Make sure that the answer and the data bytes are the same
   as for the request without the header. **Done:** `oracle/headerprobe.py`
   passes for liblsl and for labstream `main`. It also reads the first data
   byte: a sample tag (1 or 2) for each outlet, and the record tag `0xFE`
   only when the answer holds `seq/1`.
2. **liblsl inlet.** Make sure that a labstream outlet sends no
   `Labstream-Features` line, and no record, when the request has none.
   **Done:** `crates/labstream-net/tests/seq_live.rs`,
   `a_request_without_the_header_gets_plain_samples`. The interop matrix
   also runs liblsl inlets against the labstream outlet.
3. **Released labstream 0.1.2.** Run it as a peer in both directions. It
   knows nothing of the header, so each run must give plain LSL. **Done:**
   below `crates/`, `main` differs from the tag `v0.1.2` only in one comment.
   `oracle/headerprobe.py` on a `main` outlet shows no answer line. Item 2
   covers a 0.1.2 inlet, which sends no header.
4. **Two labstream peers.** Run each feature on and off, and each mix of
   offer and answer. **Done:** the unit tests in `feed.rs` and
   `features.rs` cover each mix. `oracle/interop.py` and its self test pass
   with `seq` on, and again with `labstream.Features = {}` in a
   configuration file. `oracle/headerprobe.py` with the feature off gives an
   empty answer and no other change.
5. **Syntax.** Send tokens with uppercase letters, with `;`, and with a bad
   form. Make sure that the outlet ignores each bad token, and keeps the
   good ones. **Done:** the unit tests in `features.rs`.
6. **The full interop matrix.** `oracle/interop.py` must give the same result
   as before, with the header on. **Done:** each mode gives the same count
   as on `main`.

## Open questions

- The API that reports the features of the current connection.
