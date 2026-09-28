# labstream

The Lab Streaming Layer, for Rust programs.

This crate is the API that a Rust program calls. It holds no protocol code. The
protocol is in the crates of
[`labstream-core`](https://crates.io/crates/labstream-core), and those crates
stay exact to the C++ library at `sccn/liblsl`. All of them are in one
repository, [`rednayan/labstream`](https://github.com/rednayan/labstream).

```
       labstream            this crate: the API a Rust program calls
        /      \
labstream-capi  labstream-net    the protocol, exact to sccn/liblsl
                     |
   labstream-proto, labstream-wire, labstream-time
```

Two APIs are necessary because one API cannot do both jobs. `labstream-net` answers to
the C++ source, line by line, and that is what makes the conformance claim true.
This crate answers to the person who writes the program.

## Status

The API can change before version 1.0. This crate takes the version of the
workspace, so its number is the number of the protocol crates that it calls.

## Add the library to a program

```sh
cargo add labstream
```

## Read a stream

```rust
use labstream::{Buffer, Chunk, Inlet, Post, Query};
use std::time::Duration;

let info = labstream::resolve_first(&Query::stream_type("EEG"), Duration::from_secs(5))?
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
use labstream::{Channel, Format, Outlet, StreamInfo};

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

Each item comes from a port of a real application. `docs/design.md` in this
crate gives the measurements and the reason for each choice.

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

Run these commands from the repository root:

```sh
cargo test -p labstream          # the tests bind loopback sockets
cargo run -p labstream --release --example bench
```

The tests need no hardware. They publish their own stream and read it back.

## Conventions

`CONTRIBUTING.md` at the repository root gives the rules for a change. They
apply to this crate as to every other crate of the workspace.

## License

MIT. Read `LICENSE`.
