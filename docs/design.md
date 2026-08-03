# Why this crate is shaped this way

This page records the reason for each choice in the API. Each one comes from a
port of a real application, not from taste.

## Why a new crate

`lsl-net` is exact to the C++ library. That is what makes the conformance claim
true, and it must stay that way. An API that is pleasant to call and an API that
is exact to a C++ header are not the same API.

This proposal puts the second one in a crate named `lsl`, above `lsl-net`. The
protocol crates do not change. A program that needs a protocol detail can still
call them.

```
        lsl          the API a Rust program calls
       /   \
 lsl-capi   lsl-net  the protocol, exact to sccn/liblsl
             |
   lsl-proto, lsl-wire, lsl-time
```

## The evidence

Each item below cost time in one real port. The g-signals viewer moved from the
C++ liblsl to `lsl-net` on 3 August 2026. The port is 200 lines. About 50 of
those lines exist only because the API has no answer for them.

| Lines | What the consumer wrote | The API answer |
|---|---|---|
| 19 | `channel_decls`, a walk of the `<channels>` tree | `StreamInfo::channels()` |
| 13 | `pull_chunk`, a drain loop | `Inlet::pull_chunk()` |
| 11 | `value_f32`, an 8-arm match | `Scalar`, `as_f64` |
| 7 | `marker_text` | `as_text` |

## What each change corrects

### 1. One import, not two

`Inlet::pull` gives a `lsl_wire::Sample`. A program that reads samples therefore
declares two dependencies for one library. The `lsl` crate re-exports what a
program needs.

### 2. A chunk read

This is the largest item. `lsl-net` gives one sample for each call. Each call
takes the queue lock, reads the clock, and allocates a `Vec<Value>`. A stream of
8 channels at 1000 Hz therefore costs 1000 locks and 1000 allocations each
second, and every consumer writes the same loop to hide it.

`Chunk<T>` holds the values end to end and the caller keeps it. One call takes
one lock and allocates nothing.

```rust
let mut chunk = Chunk::<f32>::new(8, 4096);
loop {
    chunk.clear();
    inlet.pull_chunk(&mut chunk, Duration::from_millis(20))?;
    for value in chunk.channel(0) { ... }      // one channel, no transpose
}
```

`Chunk::channel` also removes the transpose loop. A viewer holds one buffer for
each channel, and the samples arrive one after another. Every consumer writes
that loop today.

### 3. A buffer size that names its unit

liblsl gives one `int32` that means seconds for a regular stream and samples for
an irregular one (`include/lsl_cpp.h:914`). `lsl-net` gives one `i32` that always
means samples. A program that moves between them compiles and then drops data.

```rust
.buffer(Buffer::Seconds(8.0))     // not the number 8
```

### 4. Three resolve calls, not a minimum count

`lsl_net::resolve` takes `minimum: usize`. There is no value that means "collect
for the whole window". The port passed `usize::MAX`.

```rust
resolve_all(&query, timeout)        // the whole window, for a browser
resolve_first(&query, timeout)      // stop at the first answer
resolve_at_least(&query, 3, timeout)
```

### 5. Post-processing flags a caller can name

`Inlet::set_postprocessing` takes a `u32`. The constants live in `lsl-time`,
which the caller does not depend on. A caller therefore cannot name a legal
value.

```rust
.postprocess(Post::CLOCK_SYNC | Post::DEJITTER)
```

### 6. A query that escapes its values

A predicate built with `format!` breaks on a value that holds an apostrophe. The
`Query` type escapes every value, and it gives `concat()` when the value holds
both quote characters.

```rust
Query::source_id("Bob's device")    // name="Bob's device"
```

### 7. A description that cannot disagree with itself

`StreamInfo::new` does not take the source identifier, so a caller writes the
field afterward. Nothing makes the channel count agree with the description tree.

```rust
StreamInfo::builder("MyDevice", "EEG", Format::Float32)
    .rate(250.0)
    .source_id("device-0001")
    .channel_labels(["Fp1", "Fp2", "C3", "C4"])   // this sets the count
    .build()?
```

### 8. Channels a program can read

Every LSL consumer walks the `<channels>` tree. `StreamInfo::channels()` does it
once, and it always gives one entry for each channel.

It reads `<label>` first and `<name>` second. LSL documents `<label>`. Many
sources write `<name>`, and explorepy is one of them. A reader of `<label>` alone
gets no channel names from a Mentalab Explore device. This is measured, not
supposed.

### 9. An error type that separates lost from timeout

liblsl has `lost_error` and `timeout_error`. `std::io::Result` has neither, so a
program cannot tell "the source is gone" from "the socket failed". A viewer needs
that difference: one case reconnects, the other stops the session.

### 10. A push that takes a slice

`Outlet::push` takes a `&Sample`, so each push allocates a `Vec<Value>`. A push
of `&[f32]` allocates nothing and checks the channel count. liblsl reads past the
end of a short buffer instead (`include/lsl_cpp.h:1094`).

## The block calls, measured

`SampleQueue::push_many` and `SampleQueue::pop_many` in `lsl-net` do the work.
`examples/bench.rs` measures them against the one-sample calls. Each variant runs
alone, five times, and the fastest run counts.

| 8 channels, 100000 samples | one sample at a time | block | gain |
|---|---|---|---|
| write | 39.0 ms | 16.6 ms | 2.4x |
| read | 5.4 ms | 3.3 ms | 1.7x |

At 64 channels the write gain falls to 1.2x and the read gain holds at 1.6x.

One case gives no gain. When a writer thread and a reader thread both run as fast
as the machine allows, the block read is 0.87x at 64 channels. `pop_many` holds
the lock while it copies the samples out, and that stalls the writer. The
benchmark writes about 1000 times faster than any real stream, so the difference
is far below one microsecond for each sample at a true rate. A queue that swapped
its buffer under the lock and copied outside it would remove even that.

One allocation for each sample remains. The decoder builds a `Vec<Value>` as it
reads the wire. This crate reuses its own buffers, so nothing allocates on the
side of the caller. A decode straight into a typed buffer is the next step, and
it is a change inside `lsl-net`.

## What this proposal does not change

- No protocol crate changes. The conformance measurements stay valid.
- `lsl-capi` does not change. A C program sees the same 165 symbols.
- `lsl-net` keeps every call it has now. This crate is above it, not in front of
  it.

## Open questions

1. **The crate name.** `lsl` is the name a user expects. The name is taken on
   crates.io by the binding crate of the LSL project.
2. **A typed pull converts.** `pull::<f32>` on an `Int32` stream converts, the way
   liblsl converts (`include/lsl_cpp.h:1053-1077`). An error instead is more
   strict, and it breaks a program that moves from liblsl.
3. **`Watcher`.** A browser wants a background resolver. `ContinuousResolver`
   exists in `lsl-net`. The wrapper is not written.
