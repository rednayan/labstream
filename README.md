# lsl-rs

The Lab Streaming Layer, for Rust programs.

This crate is the API that a Rust program calls. It holds no protocol code. The
protocol lives in [`lsl-rustlang`](../lsl-rustlang), and those crates stay exact
to the C++ library at `sccn/liblsl`.

```
        lsl          this crate: the API a Rust program calls
       /   \
 lsl-capi   lsl-net  the protocol, exact to sccn/liblsl
             |
   lsl-proto, lsl-wire, lsl-time
```

Two APIs are necessary because one API cannot do both jobs. `lsl-net` answers to
the C++ source, line by line, and that is what makes the conformance claim true.
This crate answers to the person who writes the program.

## Status

Version 0.1.0. The API can change before version 1.0.

## Read a stream

```rust
use lsl::{Buffer, Chunk, Inlet, Post, Query};
use std::time::Duration;

let info = lsl::resolve_first(&Query::stream_type("EEG"), Duration::from_secs(5))?
    .expect("an EEG stream");

let mut inlet = Inlet::builder(&info)
    .buffer(Buffer::Seconds(8.0))
    .postprocess(Post::ALL)
    .open(Duration::from_secs(5))?;

for c in inlet.info().channels() {
    println!("{} in {}", c.label, c.unit);
}

let mut chunk = Chunk::<f32>::new(info.channel_count(), 4096);
loop {
    chunk.clear();
    inlet.pull_chunk(&mut chunk, Duration::from_millis(20))?;
    for value in chunk.channel(0) {
        println!("{value}");
    }
}
```

## Publish a stream

```rust
use lsl::{Channel, Format, Outlet, StreamInfo};

let info = StreamInfo::builder("MyDevice", "EEG", Format::Float32)
    .rate(250.0)
    .source_id("device-0001")
    .channels([
        Channel::new("Fp1").unit("microvolts").kind("EEG"),
        Channel::new("Fp2").unit("microvolts").kind("EEG"),
    ])
    .build()?;

let outlet = Outlet::new(info)?;
outlet.push(&[1.0f32, 2.0])?;
```

## What this crate adds

Each item comes from a port of a real application. `docs/design.md` gives the
measurements and the reason for each choice.

| Item | What it removes |
|---|---|
| `Chunk<T>` and `pull_chunk` | one lock and one allocation for each sample |
| `Chunk::channel` | the loop that turns samples into one buffer per channel |
| `StreamInfo::channels()` | the walk of the `<channels>` description tree |
| `StreamInfo::builder` | a channel count that disagrees with the channels |
| `Buffer::Seconds` | a buffer size whose unit the reader must guess |
| `Query` | a predicate that breaks on an apostrophe |
| `Post::CLOCK_SYNC` | a `u32` whose legal values live in another crate |
| `Error::Lost` | one error type for "the source is gone" and "the socket failed" |
| `Scalar`, `as_f64`, `as_text` | a match over seven value formats in each program |
| `Watcher` | a browser that pays a full timeout for each refresh |

## Build and test

```sh
cargo test          # the tests bind loopback sockets
cargo run --release --example bench
```

The tests need no hardware. They publish their own stream and read it back.

## Conventions

Documents and code comments follow ASD-STE100 Simplified Technical English, as
in `lsl-rustlang`. Before a pull request, run:

```sh
cargo fmt --all
cargo clippy --all-targets
cargo test
cargo doc --no-deps
```

Correct every warning.

## License

MIT.
