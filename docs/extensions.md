# Proposal: feature negotiation between two labstream peers

**Status:** proposal. Nothing on this page is implemented. The wire format does
not change until a feature section below is accepted and SPEC.md gets a
section for it.

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
7. A configuration key can remove a feature from the offer and from the
   answer. The name of the key is an open question.

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

The outlet gives each sample a number when the application pushes it. One
counter serves the whole outlet, not one counter for each connection.

This rule is necessary. The two real causes of loss are a full consumer
queue (SPEC.md 8.7) and the gap between a lost connection and the next one.
A counter for each connection starts again after a recovery, so it shows
neither cause.

The inlet reports the number of samples that it did not receive. The counter
also shows a repeated sample and a sample out of order.

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

1. **liblsl outlet, raw request.** Send a feed request with the header to the
   oracle outlet. Make sure that the answer and the data bytes are the same
   as for the request without the header. `oracle/headerprobe.py` does this
   now. It passes for liblsl and for labstream `main`.
2. **liblsl inlet.** Make sure that a labstream outlet sends no
   `Labstream-Features` line when the request has none.
3. **Released labstream 0.1.2.** Run it as a peer in both directions. It
   knows nothing of the header, so each run must give plain LSL.
4. **Two labstream peers.** Run each feature on and off, and each mix of
   offer and answer.
5. **Syntax.** Send tokens with uppercase letters, with `;`, and with a bad
   form. Make sure that the outlet ignores each bad token, and keeps the
   good ones.
6. **The full interop matrix.** `oracle/interop.py` must give the same result
   as before, with the header on.

## Open questions

- The final name of the header.
- The name of the configuration key from rule 7.
- The API that reports the features of the current connection.
- The exact bytes of the `seq` framing. This belongs to the `seq` section.
