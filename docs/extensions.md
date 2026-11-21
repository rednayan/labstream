# Proposal: feature negotiation between two labstream peers

**Status:** proposal. The header, the `seq` feature, and the `resume`
feature are implemented on `main`. The `dgram` section is a spec with no
code. The other features are not implemented.
`docs/roadmap.md` gives the order of the features. SPEC.md gets a section
for a feature only after the feature is accepted.

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

The answer holds only tokens whose name and version come from the request.
For each name, it holds at most one version. A feature can define a
parameter for its answer token. The `resume` section gives an example.

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
   the library has, except a feature that its section names as off by
   default (`dgram`). An inlet offers only the listed features, and an outlet
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

**Status:** implemented on `main`
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

**Status:** implemented on `main`. The token is `resume/1`. It is on by
default: `labstream.Features` is `{seq, resume}`. `docs/roadmap.md` gives
the success criterion.

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
4. An application can start a new labstream outlet with an old `uid`. A
   relay or a replay of a recording can do this. The new outlet has a new
   epoch, so negotiation rule 8 detects the new run.

#### The rules for the negotiation

1. `resume/1` is on only when `seq/1` is also on for the same connection.
   The numbers of `seq` identify the samples to send again.
2. Each outlet makes an epoch when it opens: 64 random bits from the
   random source of the operating system (`random_bytes` in
   `crates/labstream-net/src/lib.rs`). The epoch identifies one run of the
   outlet. A new outlet with an old `uid` gets a new epoch.
3. The parameter of `resume/1` has two parts with no separator: the epoch
   in exactly 16 hexadecimal digits, then a sample number in 1 to 16
   hexadecimal digits. Example: `resume/1=9c3e51a0d2b7f4e81f4a0` gives the
   epoch `9c3e51a0d2b7f4e8` and the number `1f4a0`. A parameter thus has
   17 to 32 digits, and the number always fits in 64 bits.
4. On the first connection, the inlet offers `resume/1` with no parameter.
5. On a recovery to the same `uid`, the inlet offers
   `resume/1=<epoch><R>`. The epoch comes from the last answer. `R` is the
   next number that the `seq` count expects. Example:
   `Labstream-Features: seq/1 resume/1=9c3e51a0d2b7f4e81f4a0`.
6. If the inlet has no epoch, or the `seq` count has no expected number, it
   offers `resume/1` with no parameter. A recovery to a new `uid` also
   offers no parameter (`seq` count rule 6).
7. The outlet answers `resume/1=<epoch><N>`. The epoch is its own epoch.
   `N` is the number of the first live sample of the connection. The
   answer always holds both parts, also for an offer with no parameter.
8. If the inlet knows an epoch, and the answer gives a different epoch,
   the outlet is a new run. The inlet then resets the `seq` count (`seq`
   count rule 6), the timestamp deducer, and the clock offset, as for a
   new `uid`. If the inlet knows no epoch, it cannot see a new run, and it
   keeps its state, as with `seq/1` only. The inlet keeps the epoch of
   each answer for the next recovery.
9. The outlet ignores a `resume/1` token with a parameter of fewer than 17
   or more than 32 digits. If the offer holds more than one `resume/1`
   token, the outlet ignores all of them. The connection then has no
   `resume`, and `seq` can stay on.
10. If the answer holds `resume/1` with no parameter, or with a parameter
    that rule 3 does not allow, the inlet sets `resume` off for that
    connection.
11. A blocking outlet (`transp_sync_blocking`) has no queue and no history.
    It does not answer `resume/1`.
12. Today `answer()` in `crates/labstream-proto/src/features.rs` ignores
    each offered token with a parameter, and the answer copies the offered
    token. For `resume`, the outlet accepts a parameter, and the answer
    holds a different parameter. `agreed()` already compares only the name
    and the version. The other features keep the current rules.

#### The history

1. The outlet keeps one history for the whole outlet, not one history for
   each connection. The history holds the last samples, with their `seq`
   numbers, in the order of the pushes.
2. The history also holds a sample that the outlet pushes with no consumer.
   The dropout is the time with no consumer.
3. The outlet starts the history when the first consumer agrees to
   `resume/1`. Before that point, the history costs nothing. An outlet that
   serves only liblsl inlets never keeps a history. The history then stays
   until the outlet closes.
4. Two limits set the size of the history. The outlet removes the oldest
   sample when it reaches a limit:
   - `labstream.ResumeSeconds`: the time of samples at the nominal rate.
     The default is 30 s. A stream with an irregular rate uses the memory
     limit only. The value 0 turns the history off, and the outlet does not
     answer `resume/1`.
   - `labstream.ResumeMemory`: the memory in MiB. The default is 64 MiB.
5. The memory of one sample is 16 bytes (the timestamp and the number),
   plus 32 bytes for each channel (the size of a `Value`), plus the bytes
   of each string value. Thus a marker stream with long strings reaches
   the memory limit before the time limit.
6. Each outlet has its own history. A process with 10 outlets that each
   reach the memory limit holds 640 MiB. An application with many outlets
   must set a lower `labstream.ResumeMemory`.
7. The default of 30 s covers a cut of 10 s. On Linux, the keepalive of
   labstream ends a half-open connection after 6 s
   (`crates/labstream-net/src/keepalive.rs:29`). The inlet then tries to
   connect again until the link works. In the lossy-link benchmark of
   `docs/roadmap.md`, step 0, a cut of 10 s gave a gap of about 11.8 s. The
   first default of 10 s lost the oldest samples of that gap, so the user
   set the default to 30 s on 2026-11-21.
8. The liblsl watchdog waits 15 s by default (`src/api_config.cpp:303`). A
   dropout that only the watchdog detects is longer than the default
   history. The `seq` count then shows the samples that the history did not
   hold.
9. At 4000 Hz x 64 channels, one second of history takes about 8 MB, so
   64 MiB holds about 8 s. At this load, the memory limit acts before the
   time limit. At 512 Hz x 8 channels, 30 s takes about 4.2 MB, and the
   time limit acts first. The flat sample store of `docs/roadmap.md` (the
   flat typed API step) decreases the cost of each sample by about 8
   times.

#### The resend

The outlet reads the epoch and the number `R` from the token. Let `O` be
the oldest number in the history, and `N` the number that the next push
gets.

1. The outlet registers the new consumer queue before it writes the answer
   headers, because the answer holds `N`. The registration is one step
   under the lock of the fanout. The queue gets each live sample from `N`.
   liblsl registers the queue later (`src/tcp_server.cpp:746`). A peer
   cannot see this difference.
2. If the epoch is the epoch of the outlet, and `O <= R < N`, the
   connection sends the history samples from `R` to `N - 1`. Then it sends
   the samples of the queue. If `R = N`, there is nothing to send again.
3. If `R < O`, the resend starts at `O`. The first `seq` record gives `O`.
   The inlet counts `O - R` lost samples, by `seq` count rule 2.
4. If `R > N`, or the epoch is different, or the offer has no parameter,
   or the history is empty, the outlet sends no history. The first `seq`
   record gives `N`.
5. The history holds numbers below `N`, and the queue holds numbers from
   `N`. Thus no number repeats, and no number below `R` goes to the inlet.
6. The connection reads the history in blocks of at most 256 samples. It
   takes the lock of the history once for each block. A push thus waits
   for at most the copy of one block, not for the whole resend. A copy of
   one sample at 64 channels took about 1.9 us in a measurement on the
   branch `perf/quick-wins`. Thus one block takes about 0.5 ms.
7. The history can remove samples during a resend, because of its limits.
   If the next number is no longer in the history, the connection
   continues at the oldest number. The `seq` record before that sample
   shows the gap.
8. The queue keeps its size from `Max-Buffer-Length`. The live samples
   wait in the queue during the resend. If the queue is full, it drops the
   oldest samples, as in SPEC.md 8.7. The `seq` record before the next
   sample shows this drop as a gap.
9. A resent sample keeps its original timestamp. The next section gives
   the rules.

The framing does not change. The `seq` record and the sample encoding of
SPEC.md 7 carry the resend.

#### The timestamps of a resend

An outlet sends a chunk with one timestamp. Only the first sample of the
chunk carries timestamp bytes. Each later sample carries the deduced tag
(SPEC.md 9.2, `chunk_timestamps` in `crates/labstream-net/src/outlet.rs`).
The inlet calculates a deduced timestamp from the sample before it
(SPEC.md 8.1). A resend can start in the middle of a chunk. The inlet then
has no correct sample before the first resent sample.

1. The history holds a full timestamp for each sample. It holds no deduced
   tag.
2. The fanout calculates the full timestamp under its lock, in the order of
   the `seq` numbers. It uses `TimestampDeducer` in
   `crates/labstream-wire/src/lib.rs`. The inlet uses the same code, so the
   two results are equal bit for bit.
3. Each push path goes through the fanout: `Outlet::push`,
   `Outlet::push_many`, and the chunk functions of the C API, which make
   their own deduced tags (`crates/labstream-capi/src/lib.rs:1301`).
4. One deducer serves the whole outlet. It starts when the outlet opens, as
   the `seq` counter does. It also runs before the history starts (history
   rule 3). Otherwise a history that starts in the middle of a chunk holds
   wrong timestamps. The cost is one addition for each sample. A blocking
   outlet has no history and needs no deducer.
5. When `resume/1` is on, the connection writes the full timestamp (tag 2
   of SPEC.md 7.1) for the first sample of the connection, and for each
   sample whose number is not one more than the number before it. A drop
   in the queue, or a sample that leaves the history during a resend,
   gives such a gap. Each resent sample also carries its full timestamp.
   The change from the history to the queue is not a gap when no number
   is missing, because the deduced timestamp is then correct.
6. Each other sample keeps the tag of its push. A connection with no gap
   carries the same bytes as with `seq/1` only.
7. Thus a deduced timestamp always follows the sample with the number
   before it. A connection with `seq/1` only can start in the middle of a
   chunk, and then gets wrong deduced timestamps until the next chunk
   (SPEC.md 8.4). With `resume/1`, this fault does not occur.

#### The clock after a recovery

1. A resend occurs only on a recovery to the same `uid` with the same
   epoch. On this path, the inlet keeps its clock offset, its
   post-processor, and its timestamp deducer
   (`crates/labstream-net/src/inlet.rs:756-771`).
2. The outlet is the same process, with the same clock. Thus the resent
   samples get the same clock offset as the live samples.
3. With the dejitter option, the resent samples close the gap in the
   count of samples. The count of the dejitter filter then stays correct
   (`samples_since_t0_++` in `src/time_postprocessor.cpp:119`).
4. A different epoch resets the clock offset (negotiation rule 8). The
   resent samples of a new run do not exist, so no resent sample gets the
   offset of a different process.

#### What the inlet reports

- `Inlet::lost()` counts only the samples that the history did not hold.
- `Inlet::resent()` in `labstream`, and `Inlet::samples_resent()` in
  `labstream-net`, give the count of samples that came from a history since
  the inlet opened. A sample came from the history when its number is
  below the `N` of the answer.
- The value is `None` when the current connection does not use `resume`.
- The C API gets two functions, one for the lost count and one for the
  resent count. Each returns a signed 64-bit value. The value -1 means
  `None`. The names follow the names of the Rust API.

#### The conformance plan

1. **liblsl outlet with a parameter.** Extend `oracle/headerprobe.py` to
   send `seq/1 resume/1=9c3e51a0d2b7f4e81f4a0`. Make sure that the answer
   and the data bytes are the same as for a request with no header.
2. **liblsl inlet.** Make sure that a labstream outlet keeps no history when
   only liblsl inlets connect. A unit test reads the size of the history.
3. **Released labstream 0.1.2.** Run it as a peer in both directions. Each
   run must give plain LSL.
4. **The `seq` build of #12.** Run it as a peer in both directions. It
   answers `seq/1` and does not answer `resume/1`. Make sure that the inlet
   counts the loss of a dropout and gets no resend.
5. **The configuration.** `labstream.Features = {resume}` without `seq`
   gives no resume, on the inlet and on the outlet.
   `labstream.ResumeSeconds = 0` gives no answer `resume/1`.
6. **The syntax.** Unit tests send a parameter of 16 and of 33 digits, an
   offer with two `resume/1` tokens, and an answer with no parameter. Each
   case must give a connection with `seq` and no `resume`.
7. **The resend rules.** Write a unit test for each case of the resend: `R`
   in the history, `R = N`, `R` below the history, `R` above `N`, a
   different epoch, and an empty history. One more test sends a resend
   that is longer than `Max-Buffer-Length`. One more test removes history
   samples during a resend.
8. **The timestamps.** A unit test starts a resend in the middle of a
   chunk. Each resent sample must carry tag 2, and each timestamp must be
   equal bit for bit to the timestamp that an inlet with no dropout
   calculates.
9. **A live dropout.** The outlet ends each consumer connection, and pushes
   for 3 s. The inlet must get each sample one time, in order, and
   `Inlet::lost()` must give 0. Run at 512 Hz x 8 channels and at
   4000 Hz x 64 channels. The test must fail with `resume` off.
   The outlet pushes chunks of more than one sample. A second inlet with no
   dropout must get the same values and the same timestamps, bit for bit,
   with all post-processing options on. Run the same test on a marker
   stream with string values and an irregular rate.
10. **A reused `uid`.** Close an outlet, and open a new outlet with the
    same `uid`. The inlet must get no resend, must reset its `seq` count,
    and must count no false loss.
11. **A dropout longer than the history.** Set `labstream.ResumeSeconds`
    to 1. The loss count must be exact.
12. **The push delay.** At 4000 Hz x 64 channels, measure the longest push
    during a resend of 8 s. It must stay below 2 ms.
13. **The interop matrix.** `oracle/interop.py` must give the same result
    as before, with `resume` on.
14. **Latency and CPU.** Run `latency.py` with `resume` on against the
    `seq` build. The history adds one copy for each push.

#### The status of the conformance plan

The status on 2026-11-20, at the merge of `feat/resume` into `main`. The
live tests are in `crates/labstream-net/tests/`, in the files that start
with `resume_`.

1. **Done.** `oracle/headerprobe.py` passes for liblsl, labstream 0.1.2,
   the `seq` build, and the `resume` build. The `resume` build answers
   `resume/1` with its own epoch and `seq/1`. Run the probe without
   `LD_LIBRARY_PATH`. Otherwise each probe program loads liblsl.
2. **Done.** `a_plain_request_starts_no_history`: a request with no header,
   which is the request of liblsl, and an offer of `seq/1` only, start no
   history. An offer of `resume/1` starts it.
3. **Done.** `oracle/interop.py` with 0.1.2 as the second peer: 28 of 28
   cells, and 24 of 24 cells with chunks of 16 and in the blocking mode.
   `oracle/resumeprobe.py` against a 0.1.2 outlet: the inlet loses the
   dropout, and both counts are unknown.
4. **Done.** The same interop runs with the `seq` build pass. Against a
   `seq` outlet, `oracle/resumeprobe.py` shows 1832 missing samples and a
   loss count of 1832. The resend count is unknown.
5. **Done.** `resume_config_no_seq.rs` and `resume_config_no_history.rs`.
   Each runs as its own process, because the library reads its
   configuration one time.
6. **Done.** The unit tests in `features.rs`.
7. **Done, with one change.** The resend does not go through the consumer
   queue, so `Max-Buffer-Length` does not limit it. The live samples wait
   in the queue during the resend. A unit test drops samples from that
   queue, and one more test removes history samples during a resend.
8. **Done.** The unit tests in `outlet.rs` compare each timestamp bit for
   bit.
9. **Done, with one change.** Three live tests cut the connection for 3 s:
   512 Hz x 8 channels, 4000 Hz x 64 channels, and a marker stream. Each
   sample arrives once, in order, with the raw timestamp of a second inlet,
   bit for bit. With `labstream.Features = {seq}`, these tests fail. With
   post-processing on, two inlets do not agree bit for bit, because each
   inlet measures its own clock offset. The test with post-processing thus
   checks the time axis of one inlet: it never goes back, and no step
   across the dropout is more than two sample periods.
10. **Done.** `a_new_outlet_with_an_old_uid_gives_no_resend_and_no_false_loss`.
    The new outlet has pushed 300 samples, and the inlet expects 100. The
    inlet gets only the live samples, with no false loss.
11. **Done.** `resume_short_history.rs`, with `ResumeSeconds = 1`: a 3 s
    dropout at 512 Hz gave 1448 missing samples, a loss count of 1448, and
    512 resent samples.
12. **Done, as a measurement.** `the_longest_push_during_a_long_resend_stays_short`
    is an ignored test. Run it with `cargo test --release -p labstream-net
    --test resume_live -- --ignored`. A resend of about 7.6 s at
    4000 Hz x 64 channels gave a longest push of 0.60, 0.62, and 0.81 ms in
    three runs. The limit is 2 ms. A resend of 8 s does not fit in the
    default memory limit at this load, so the cut is 7 s.
13. **Done.** Against liblsl: 28 of 28 cells, 24 of 24 with chunks, and 24
    of 24 in the blocking mode.
14. **Done.** The first build added about 80 µs to the median at
    4000 Hz x 64 channels, chunk 32. The fix gives the queues each sample
    before the history copy. The latency is now in the range of the `seq`
    build. The outlet CPU is about 1.5 points higher. `docs/latency.md`
    gives the runs.

#### Open questions

- The final names of the Rust API and the C API for the resend count.
- An API that marks each resent sample. Version 1 gives a count only. The
  health API step of `docs/roadmap.md` can add gap positions and resend
  positions.
- A request for the history on the first connection. A new viewer can then
  show the last seconds at once. Version 1 sends a history only after a
  recovery.

### `dgram`: samples over UDP

**Status:** spec approved by the user on 2026-11-21. Implemented on the
branch `feat/dgram`, not merged. Off by default. The token is `dgram/1`.
`docs/roadmap.md`, step 5, gives the decision for plain UDP (decision 12).

**Change of 2026-11-21.** The first version missed two targets. At 30 %
and 50 % loss, the resends went on TCP, and TCP at that loss waits
seconds. On a clean link of 1 Mbit/s, the fixed copies made the p99 worse
than TCP. The user asked for a fix of both. The resend of a small gap now
goes on UDP (the feedback datagram), and the count of copies follows the
measured loss. `dgram/1` is not released, and no liblsl peer negotiates
it, so the token stays `dgram/1` and the wire format changes in place.

#### The purpose

TCP gives each byte in order. When the network loses one segment, each
later sample waits until TCP sends the segment again. On a link with 20 ms
delay and 1 % loss, the p99 latency of both libraries is about 70 ms, and
the longest wait is up to 118 ms (`docs/latency.md`, section "A slow,
lossy, or cut link"). Wi-Fi loses packets in this way.

`dgram` sends the samples in UDP datagrams. A lost datagram stops only its
own samples. The next datagram carries copies of them, so the inlet repairs
most losses after one datagram. The TCP connection stays open, and carries
everything else.

#### What liblsl does

These facts come from `conformance/liblsl/` at `e651023c`.

1. The data channel of liblsl is TCP only. The UDP sockets of liblsl carry
   discovery and the time probes, not samples (`src/udp_server.cpp`).
2. A liblsl peer ignores the `Labstream-Features` header (the section "What
   liblsl does with an extra header" above). Thus a liblsl peer never uses
   `dgram`.

#### The scope

1. `dgram` changes only the path of the samples from the outlet to the
   inlet. The headers, the test pattern, the `seq` records, and the end of
   the stream stay on TCP.
2. `dgram` needs `seq/1` and `resume/1` on the same connection. The `seq`
   numbers put the samples in order. The `resume` history gives a sample
   again when each copy of it is lost.
3. `dgram` does not cover a change of address. If the address of a peer
   changes, the TCP connection ends, and `resume/1` fills the gap.
   `docs/roadmap.md` has connection migration as an open question.
4. `dgram` does not encrypt. Step 14 of `docs/roadmap.md` covers
   encryption.
5. `dgram` is off by default in version 1:
   `labstream.Features` is `{seq, resume}`. It goes on by default only
   after a test with two machines on Wi-Fi (conformance item 13).

#### The rules for the negotiation

1. The inlet offers `dgram/1` with no parameter.
2. The outlet answers `dgram/1` only if all of these are true. Otherwise it
   leaves `dgram/1` out.
   - The answer also holds `seq/1` and `resume/1`.
   - The outlet is not a blocking outlet.
   - The format is a string, or one sample takes at most 400 bytes. A
     datagram then holds at least 3 samples, so the copies can work. A
     stream with larger samples gets no gain from `dgram`, and stays on
     TCP.
3. The outlet opens one UDP socket for each connection. The socket uses
   the address family of the TCP connection. Its port is in the range of
   `ports.BasePort` and `ports.PortRange`, as the data port is, so that a
   firewall rule for LSL covers it.
4. The outlet answers `dgram/1=<port><cookie>`. The port is the UDP port of
   the outlet, in exactly 4 hexadecimal digits. The cookie is 64 random
   bits, in exactly 16 hexadecimal digits. Example:
   `dgram/1=c35b9f2e81d04a7713c6`.
5. The inlet ignores an answer `dgram/1` with no parameter, or with a
   parameter of other than 20 digits. `dgram` is then off.
6. The outlet ignores an offer `dgram/1` with a parameter.

#### The hello

The outlet sends no datagram before it knows the address of the inlet. The
inlet sends the first datagram, because a firewall or a NAT on the side of
the inlet then lets the answers through.

1. After the test pattern, the inlet sends a hello datagram to the UDP port
   of the outlet, at the address of the TCP connection:

   ```
   u8  type     1 (hello)
   u64 cookie   little-endian, from the answer
   ```

2. The inlet sends the hello again each 100 ms, until the first data
   datagram arrives, or for 1 s.
3. The outlet accepts the first hello with the correct cookie from the
   address of the TCP peer. The source address and port of that hello
   become the target of the data datagrams. The outlet ignores each other
   hello.
4. Until the outlet accepts a hello, the samples go on TCP, as with `seq`
   only. If no correct hello arrives in 1 s, the connection stays on TCP.
   The inlet then also stops its hellos.

#### The change of path

1. When the outlet accepts the hello, it writes a path record on TCP:

   ```
   u8  tag      0xFD
   u8  path     1 (UDP) or 0 (TCP)
   u64 number   little-endian, the number of the first sample on that path
   ```

2. Each sample below that number goes on the old path. Each sample from
   that number goes on the new path, except a sample that does not fit in
   one datagram. Such a sample goes on TCP, with its `seq` record.
3. The tag `0xFD` is not a sample tag, as `0xFE` is not. The record does
   not count toward `Max-Chunk-Length`.
4. A path record with `path = 0` gives the change back to TCP. The fallback
   section gives the cases.

#### The data datagram

```
u8  type     2 (data)
u64 cookie   little-endian, from the answer
u64 first    little-endian, the number of the first sample in the datagram
u16 count    little-endian, the count of samples in the datagram
u32 serial   little-endian, the number of the datagram
...          the samples, in the encoding of SPEC.md 7, one after another
```

1. The samples of one datagram have consecutive numbers, from `first`.
   Each data datagram of a connection gets the next `serial`, from 0: a
   datagram with new samples, a repeat datagram, and a resend datagram.
   The inlet measures the loss of datagrams from the gaps in `serial`.
2. Each sample carries its full timestamp (tag 2). A datagram can arrive
   after a later one, or not at all, so the inlet cannot deduce a
   timestamp from the sample before it. The history already holds the
   full timestamps (`resume` timestamp rule 1).
3. The byte order of the samples is the byte order of the TCP connection.
4. A datagram has at most 1200 bytes, the header included. This size
   passes most links with no fragmentation of IP.
5. The inlet ignores a datagram with a wrong cookie, or from an address
   that is not the address of the outlet.

#### The idle datagram

A loss shows at the inlet only when a later datagram arrives. On a marker
stream, the next marker can come minutes later. An idle datagram gives the
inlet the next number when no sample follows:

```
u8  type     3 (idle)
u64 cookie   little-endian, from the answer
u64 next     little-endian, the number that the next sample will get
```

1. After the last data datagram, the outlet sends the repeat datagram of
   the copies rule 3 at 5 ms, or an idle datagram if it has no copy to
   send. It then sends idle datagrams at 25 ms, 100 ms, and 250 ms. A lost
   last datagram thus shows at the inlet soon, also at heavy loss. While
   no sample follows, the outlet then sends an idle datagram each 1 s.
   These datagrams also keep the state of a NAT or a firewall open.
2. When `next` is above the last number that the inlet has, the inlet has
   a gap, and the resend rules apply.

#### The copies

1. The outlet first puts the new samples of a push into datagrams.
2. Then it fills the free space of each datagram with copies of the samples
   just before the new samples, newest first, so that the samples stay
   consecutive. Each sample goes into at most `1 + C` datagrams: once as
   new, and as a copy in at most `C` later datagrams. `C` is the count of
   copies of rule 6.
3. If the outlet sends no data datagram for 5 ms after a data datagram, it
   sends one repeat datagram: copies of the last samples, with no new
   sample. A push of a chunk, and the last marker before a quiet time,
   thus get their copies after 5 ms, not at the next push.
4. Example: at 512 Hz x 8 channels of `float32`, a sample takes 41 bytes.
   With one sample for each push, the next datagram comes about 2 ms later
   and carries the copies. With chunks of 16, the repeat datagram carries
   them after 5 ms.
5. At 4000 Hz x 64 channels with chunks of 32, a sample takes 265 bytes,
   and 4 samples fill a datagram. The new samples fill most datagrams, and
   the repeat datagram holds only the last 4 samples. A lost datagram then
   often needs a resend.
6. **The count of copies.** Copies cost bytes. On a link of 1 Mbit/s with
   no loss, 2 copies made each datagram of a 512 Hz x 8 channel stream
   about 60 % longer than the TCP segment, and the p99 went from 1.1 ms
   to 2.0 ms. Thus `C` follows the loss of datagrams that the inlet
   measures (the feedback datagram):

   | measured loss | `C` |
   |---|---|
   | 0 | 0 |
   | up to 2 % | 2 |
   | up to 10 % | 3 |
   | up to 25 % | 4 |
   | up to 40 % | 6 |
   | more than 40 % | 8 |

   - `C` starts at 0.
   - A higher `C` applies at once.
   - A lower `C` applies only after each report for 5 s gave it, and after
     the outlet sent 1000 datagrams.
   - With `C = 0`, the repeat datagram has no copy, and the outlet sends
     the idle datagram of the idle rule 1 in its place.

   With `C` copies, a raw loss `p` leaves about `p` to the power `C + 1`
   of the samples for a resend: 0.02 % at 30 % with `C = 6`, and 0.2 % at
   50 % with `C = 8`.

#### The resend

1. The inlet keeps the number that it expects next, `E`. It gives the
   application each sample in the order of the numbers, as with TCP.
2. A sample above `E` waits in a buffer. A copy of a sample that the inlet
   already has goes away.
3. A gap is a run of missing numbers below the highest number that the
   inlet knows of. A gap of at most 64 samples is a small gap. A larger
   gap comes from a cut, not from a lost datagram.
4. **A small gap goes on UDP.** If a small gap stays for 2 datagram
   periods, or for 10 ms if that is longer, the inlet puts it in a
   feedback datagram (the next section). Each small gap goes in, not only
   the gap at `E`, so the gaps get their resends in parallel. The inlet
   sends the gap again each `max(10 ms, 1.5 x R)` until the gap closes.
   `R` is the round trip of a resend: the time from the first request of
   a gap to the arrival of a sample of it. `R` starts at 50 ms.
5. The outlet sends the samples of a small gap from the `resume` history,
   in data datagrams on UDP, with no copies. If `C` is 3 or more, it sends
   each such datagram twice. It sends at most 256 samples for one
   feedback datagram, and it does not send a sample again within 10 ms.
   A sample that the history no longer holds gets no resend.
6. The inlet gives up a small gap when it stood at `E` for `max(1 s, 4 x
   R)`. The time counts from when the gap reached `E`, so a gap behind a
   TCP resend first waits for that resend. At 30 % loss in each direction,
   a request and its answer both arrive in about half of the tries. 1 s
   holds about 15 tries, and leaves less than 1 gap in 10000 unfilled. On
   a slow link, a TCP resend after a cut can fill the queue of the link
   for seconds. The first rule, 1 s from when the gap first showed, then
   gave up a sample in 2 of 3 runs of the cut of 10 s at 1 Mbit/s. The
   inlet gives up a small gap only while datagrams still arrive (within
   500 ms). After 1.5 s with no datagram, the link is in a cut, and the
   wait starts again when datagrams return. Without this rule, the inlet
   gave up a gap at the start of a cut before TCP closed, and `resume/1`
   asked past that sample.
7. **A large gap goes on TCP.** If a large gap at `E` stays for the same
   wait, the inlet asks for it on TCP:

   ```
   u8  tag      0xFC
   u64 from     little-endian, the first missing number
   u32 count    little-endian, the count of missing samples
   ```

8. The outlet sends the samples of a TCP request from the `resume` history
   on TCP, with their `seq` records, as for a resend after a recovery.
9. If the history no longer holds a sample, the outlet sends the next one
   that it holds. The inlet counts the samples between as lost.
10. The inlet gives up a large gap after 500 ms with no answer and no other
    data on TCP. Data on TCP shows that an answer can still be on the way,
    on a slow link. It counts the gap as lost, and gives the samples after
    it. A sample of the gap that arrives later goes away.
11. A gap that an open TCP request covers gets no new request on TCP or on
    UDP. Only a part beyond the request gets one. At the start of a
    connection with a `resume` resend, the numbers of the resend count as
    requested. Without this rule, a reconnect on a slow link sent each part
    of the resend about 14 times, and the backlog ended the connection
    again.

The first version sent each resend on TCP, one gap at a time. At 30 % loss
in each direction, TCP waits seconds for each lost segment, and the later
gaps waited behind the first one. The runs lost about 3000 of 6144
samples.

#### The feedback datagram

The inlet sends it to the UDP port of the outlet, from the socket of its
hellos:

```
u8  type     4 (feedback)
u64 cookie   little-endian, from the answer
u32 loss     little-endian, the measured loss of datagrams, in parts per
             million, or 0xFFFFFFFF if the inlet has no measure yet
u8  gaps     the count of gaps that follow, at most 64
...          each gap: u64 from, then u32 count, little-endian
```

1. The inlet measures the loss of datagrams from `serial`. It counts the
   datagrams that arrived and the range of their `serial` values in
   buckets of 100 ms, and adds the last 10 buckets. A gap in `serial`
   between two buckets does not count, so the time of a cut is no loss.
2. The inlet sends a feedback datagram each 100 ms while datagrams arrive,
   and also each time that a small gap is due for a request (resend rule
   4).
3. The outlet accepts a feedback datagram only with the correct cookie and
   from the address of the accepted hello. It ignores each other one.
4. The outlet sets `C` from `loss` (the copies, rule 6).

#### The count on two paths

The `seq` rules count one ordered byte stream. With `dgram`, samples
arrive on two paths, and a TCP resend can carry a number below the numbers
that UDP already gave.

1. The inlet merges the two paths by number, before the count. The `seq`
   count rules then apply to the merged stream: each sample that the inlet
   gives to the application moves `E` by one.
2. A `seq` record on TCP gives the number of the next sample on TCP only.
   It does not move `E`, and it counts no loss by itself.
3. A loss counts only in three cases: resend rules 6, 9, and 10.
   `Inlet::lost()` is thus the count of the numbers that the inlet never
   gave.

#### The fallback to TCP

A cut stops both paths, and a burst of Wi-Fi loss stops UDP for some
milliseconds. Neither is a reason to leave UDP. A fallback is only for a
UDP path that stays bad while data still arrives, or that stops while TCP
works.

1. **Heavy loss.** The inlet measures the loss after the copies and the
   resends: the samples that it gave up, in each window of 1 s. A window
   counts only if at least 10 datagrams arrived in it. If 3 windows in a
   row show more than 5 %, the inlet writes a path request on TCP. The
   first version counted the samples that it asked for. At 50 % loss that
   count is about 12 %, so the connection changed to TCP, and TCP at 50 %
   loss lost about 5350 of 6144 samples. The path request:

   ```
   u8  tag      0xFB
   u8  path     0 (TCP)
   u64 from     little-endian, the number that the inlet expects next
   ```

   The outlet then writes a path record `path = 0` with the number `from`,
   and sends the samples from `from` on TCP, from the history and then
   live.
2. **A dead UDP path.** On the UDP path, the outlet sends a datagram at
   least each 1 s (the idle rule). If no datagram arrives for 3 s while
   TCP still works, the inlet writes the same path request.
3. **An error of the socket.** If the outlet cannot send a datagram, it
   writes a path record `path = 0` itself.
4. **The way back.** 30 s after a fallback, the outlet tries UDP again: it
   writes a path record `path = 1` and sends on UDP. The address of the
   inlet stays the address of the accepted hello. If the path is still
   bad, rule 1 or 2 gives the next fallback.

A raw loss of 30 % or 50 % thus stays on UDP: the copies and the resends
on UDP leave almost no sample to give up. TCP at such a loss is worse than
UDP, so a fallback would only add loss. Rule 1 is for a UDP path that is
much worse than TCP, for example a UDP rate limit in a firewall.

#### Keepalive on UDP

labstream ends a quiet TCP connection after about 5 s with no answer to its
keepalive probes, and after 6 s with data that the peer does not
acknowledge (`crates/labstream-net/src/keepalive.rs`). On UDP, the TCP
connection is quiet, so the probes run all the time. At 50 % loss in each
direction, a probe and its answer both arrive in a quarter of the tries,
and 3 failed probes in a row are frequent. Without the rules below, the
TCP connection broke in 2 of 3 runs at 50 % loss, and the `resume` resend
of the reconnect stalled on TCP: about 1100 samples lost in each.

1. While the samples go on UDP, each side uses long timers on its TCP
   socket: the first probe after 30 s, a probe each 5 s, 6 probes, and an
   acknowledgement timeout of 60 s.
2. The datagrams show that the peer is alive. The inlet gets a datagram at
   least each 1 s (the idle rule), and the outlet gets a feedback datagram
   each 100 ms.
3. If a side gets no datagram of its peer for 3 s, or if the path changes
   to TCP, it uses the short timers again.

#### What the inlet reports

- `Inlet::lost()` counts the samples that neither a copy nor a resend gave.
- `Inlet::resent()` counts the samples of a `resume` resend, as before. It
  does not count a copy or a resend of `dgram`.
- The API that shows the path of the connection (UDP or TCP) is an open
  question.

#### The conformance plan

1. **liblsl and older peers.** `oracle/headerprobe.py` with `dgram/1` in
   the offer: liblsl, 0.1.2, the `seq` build, and the `resume` build answer
   no `dgram/1`, and nothing else changes.
2. **The negotiation.** Unit tests: `dgram/1` without `seq/1` or
   `resume/1`, a parameter of a wrong length, an offer with a parameter, a
   blocking outlet, and a stream with samples of more than 400 bytes. Each
   gives a connection with no `dgram`. A live test: the UDP port is in the
   range of `ports.BasePort` and `ports.PortRange`.
3. **The encoding.** Unit tests: a data datagram reads back, a cut datagram
   is an error, and a datagram never has more than 1200 bytes.
4. **The copies.** Unit tests: each sample goes into at most `1 + C`
   datagrams, the samples of each datagram are consecutive, and one lost
   datagram of a 512 Hz x 8 channel stream costs no sample when `C` is 2.
   The count of copies follows the table of the copies rule 6: it goes up
   at once, and down only after 5 s and 1000 datagrams.
4a. **The repair at heavy loss.** A unit test simulates the packer, the
    reorder buffer, and the feedback with a seeded random loss in each
    direction and a delay of 20 ms. At 30 % loss no sample is lost. At
    50 % loss at most 0.1 % is lost. With the resend on TCP rules of the
    first version, the same simulation loses samples at 30 %.
4b. **The watchdog.** A live test with `tuning.WatchdogTimeThreshold`
    shorter than the run: a connection on UDP, with no byte on TCP, keeps
    its samples on UDP past the watchdog time.
5. **The order and the count at the inlet.** Unit tests with each pattern
   of loss, repeat, and reorder on UDP, mixed with resends on TCP: the
   inlet gives each sample once, in order, and `Inlet::lost()` counts only
   the numbers that it never gave. A gap past the time limit counts as
   lost, and a late sample of that gap goes away. An idle datagram after a
   lost last datagram starts a resend.
6. **The hello.** Live tests: a wrong cookie gets no data. A hello from a
   different address gets no data. With no hello, the connection stays on
   TCP after 1 s, and loses no sample.
7. **The change of path.** A live test: no sample is lost, and none
   arrives twice, at the change to UDP and at the change back to TCP.
8. **Large samples.** A live test with string samples of 2000 bytes, mixed
   with small ones. The large ones go on TCP, and each sample arrives
   once, in order.
9. **Heavy loss and the fallback.** In the lossy-link benchmark:
    - a loss of 30 % in each direction keeps the connection on UDP, with
      no lost sample,
    - a loss of 50 % in each direction keeps the connection on UDP, and
      loses much less than TCP: at most 0.1 % of the samples,
    - the same two rules from the start of the run, before the
      connection opens: the result of the connect is reported, for
      `dgram` and for TCP,
    - after a fallback, the connection tries UDP again after 30 s,
    - a UDP path that stops while TCP works (a rule that drops only UDP)
      changes the connection to TCP within 4 s, with no lost sample.
10. **The success criterion.** `oracle/lossylink.py` with `dgram/1` on.
    The script needs options for the load and for a marker stream first.
    - At 512 Hz x 8 channels, chunk 1, with 20 ms delay and 1 % loss: the
      p99 is less than 35 ms, and no sample is lost.
    - The same with chunks of 16, and on a marker stream with 200 markers
      each second: the p99 is less than 35 ms, and no sample is lost.
    - At 4000 Hz x 64 channels, chunk 32, with 20 ms delay and 1 % loss:
      the p99 is no worse than with TCP, and no sample is lost.
    - The rules `clean`, `delay`, and `rate` give a p99 no worse than TCP,
      outside the range of the runs.
    - The cut of 3 s loses no sample, and the connection is still on UDP
      after the cut.
    - The cut of 10 s loses no sample, and the longest wait is no longer
      than with TCP (about 11.9 s).
11. **The interop matrix.** `oracle/interop.py` gives the same result as
    before, with `dgram` on.
12. **Latency and CPU on loopback.** `oracle/latency.py` with `dgram` on
    against the build before it. The CPU of each side must not grow by
    more than 25 %.
13. **Two machines on Wi-Fi.** A manual test with an outlet and an inlet on
    two machines, one on Wi-Fi, for 10 minutes. This test must pass before
    `dgram` goes on by default.
14. **Windows and macOS.** The tests of items 6 to 8 pass in CI on each
    platform.

#### The status of the conformance plan

The status on 2026-11-21, on the branch `feat/dgram`. `docs/latency.md`,
section "`dgram/1` over a slow, lossy, or cut link", gives the runs.

1. **Done.** `oracle/headerprobe.py` offers `dgram/1`. liblsl, 0.1.2, the
   `seq` build, and the `dgram` build with its default configuration
   answer no `dgram/1`, and nothing else changes.
2. **Done.** The unit tests in `features.rs`, and `dgram_outlet_live.rs`
   for the port range.
3. **Done.** The unit tests in `dgram.rs`.
4. **Done.** The unit tests of `Packer` and `CopyControl` in `dgram.rs`.
4a. **Done.** The simulation unit tests in `dgram.rs` cover 1 %, 30 %, and
    50 % loss.
4b. **Done.** `crates/labstream-net/tests/dgram_watchdog.rs`.
5. **Done.** The unit tests of `Reorder` in `dgram.rs`, with loss, repeats,
   reorder on two paths, and cuts.
6. **Partly done.** `dgram_outlet_live.rs`: a wrong cookie keeps the samples
   on TCP. A hello from a different address is not tested.
7. **Done.** `dgram_outlet_live.rs` and `dgram_live.rs`: the change to UDP,
   a path request back to TCP, and a cut of 3 s, with each sample once.
8. **Done.** Both live files: string samples of 2000 bytes go on TCP, in
   order.
9. **Mostly met.** A UDP path that stops changes to TCP after about 2.9 s
   with no lost sample. `udpback` returns to UDP after 30 s with no lost
   sample. At 30 % loss after 2 s, `dgram` loses no sample. At 50 % loss
   after 2 s, a traced 3-run batch lost `0, 0, 0` samples. One earlier
   3-run batch lost `6110, 0, 1` (one run stalled after 34 samples). From
   the start, 30 % loss gave `0, 0, 759` lost on `dgram` against
   `no connect, 4129, 5939` on TCP. At 50 % from the start, discovery or
   handshake failed in most runs for every library.
10. **Met.** With 20 ms delay and 1 % loss, the p99 is 22.0 ms at chunk 1,
    28.9 ms at chunk 16, and 20.9 ms for markers, with no lost sample. At
    4000 Hz x 64 channels, `dgram` loses none and TCP loses over 12000. At
    1 Mbit/s, `dgram` matches TCP (1.2 ms against 1.1 ms). The cuts of 3 s,
    10 s, and 10 s at 1 Mbit/s lose no sample, and `cut3` stays on UDP.
11. **Done.** `oracle/interop.py` with `dgram` on passes 28 of 28 plain
    cells (plus 4 self-tests), 24 of 24 numeric chunk cells, and 24 of 24
    numeric sync cells.
12. **Partly met.** `oracle/latency.py` on loopback gives lower p50 on
    every load, and equal or lower CPU on single-sample loads. Chunked
    loads exceed +25 % CPU on the inlet (0.8 % against 0.6 % at 512 Hz x 8,
    chunk 16, and 3.7 % against 2.3 % at 4000 Hz x 64, chunk 32). Outlet
    CPU at 4000 Hz x 64, chunk 32 is 6.6 % against 5.2 % (+27 %), because
    one chunk splits into 8 datagrams.
13. **Not done.** Two machines on Wi-Fi. `dgram` stays off by default.
14. **Not done.** Windows and macOS, until CI works again.

#### Open questions

- The API that shows the path of the connection, and the count of copies
  and resends that the inlet used.
- The size of 1200 bytes. A link with a larger MTU can take more samples in
  each datagram.
- A rate limit of the outlet. Version 1 sends at the rate of the
  application. The fallback rule 1 is the only reaction to loss.

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

Each item gives its status on `main`.

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
