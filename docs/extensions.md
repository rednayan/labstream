# Proposal: feature negotiation between two labstream peers

**Status:** proposal. The header and the `seq` feature are implemented on
the branch `feat/seq`. The other features are not implemented. SPEC.md gets
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
   pattern sample. Each connection thus starts with an absolute number.
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
   The first record of the new connection then shows the gap.
6. A recovery to a new outlet (a new `uid`) starts again at rule 1, because
   the new outlet has a new counter.

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
   passes for liblsl and for labstream `main`.
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

- The final name of the header.
- The API that reports the features of the current connection.
