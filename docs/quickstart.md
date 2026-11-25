# Quickstart

This page shows how to install and run `labstream` from Rust, C, C++, Python,
the command line, and a web browser.

## Rust

Add the high-level crate to your project:

```sh
cargo add labstream
```

### Publish a stream

```rust
use labstream::{Format, Outlet, StreamInfo};

fn main() -> Result<(), labstream::Error> {
    let info = StreamInfo::builder("MyAmp", "EEG", Format::Float32)
        .rate(250.0)
        .source_id("amp-0001")
        .channel_labels(["Fp1", "Fp2", "C3", "C4"])
        .build()?;

    let outlet = Outlet::new(info)?;
    outlet.push(&[1.0f32, 2.0, 3.0, 4.0])?;
    outlet.push_metadata("imp.Fp1", "4.2 kOhm");
    Ok(())
}
```

### Read a stream with selection and health

```rust
use labstream::{Buffer, Chunk, Inlet, Post, Query};
use std::time::Duration;

fn main() -> Result<(), labstream::Error> {
    let info = labstream::resolve_first(&Query::stream_type("EEG"), Duration::from_secs(5))?
        .expect("an EEG stream");

    let mut inlet = Inlet::builder(&info)
        .buffer(Buffer::Seconds(8.0))
        .postprocess(Post::ALL | Post::DRIFT | Post::DEVICE_DELAY)
        .channels([0, 1])      // select/1: request only Fp1, Fp2 from outlet
        .decimate(2)           // select/1: anti-aliased 2x FIR decimation
        .open(Duration::from_secs(5))?;

    let mut chunk = Chunk::<f32>::new(inlet.info().channel_count(), 1024);
    inlet.pull_chunk(&mut chunk, Duration::from_millis(50))?;

    let h = inlet.health();
    println!("connected={} on_udp={} on_shm={} lost={:?}", h.connected, h.on_udp, h.on_shm, h.lost);
    Ok(())
}
```

### Align multiple asynchronous streams (`SyncGroup`)

```rust
use labstream::{Inlet, SyncConfig, SyncGroup};
use std::time::Duration;

// Given opened `labstream_net::Inlet` instances at different rates (for example 500 Hz and 120 Hz):
let cfg = SyncConfig::new(250.0); // common 250 Hz output grid
let mut group = SyncGroup::new(vec![inlet_a, inlet_b], cfg)?;
let mut out = Vec::new();
group.pull(&mut out, 64, Duration::from_millis(50))?;
for sample in &out {
    if sample.gap_mask == 0 {
        println!("t={:.6} values={:?}", sample.timestamp, sample.values);
    }
}
```

## C, C++, and Python (`liblsl` drop-in)

Download the prebuilt shared library for your platform from
[GitHub Releases](https://github.com/rednayan/labstream/releases):

| Platform | Release asset | Expected runtime filename |
|---|---|---|
| Linux x86-64 | `liblsl-x86_64-linux.so` | `liblsl.so` |
| macOS Apple Silicon | `liblsl-aarch64-macos.dylib` | `liblsl.dylib` |
| Windows x86-64 | `lsl-x86_64-windows.dll` | `lsl.dll` |

Or build from source:

```sh
cargo build -p labstream-capi --release
```

### Use with `pylsl`

Point `PYLSL_LIB` to the shared library:

```sh
export PYLSL_LIB=/path/to/liblsl.so
python3 -c "import pylsl; print(pylsl.library_version())"
```

## Command-line tools

Build the `labstream` binary:

```sh
cargo build -p labstream-cli --release
```

Common commands (full flags in [Command-Line Tools](tools.md)):

```sh
labstream doctor --peer 192.168.1.50          # diagnose multicast, broadcast, UDP, and TCP
labstream top                                 # live terminal monitor of active LSL streams
labstream play recording.xdf --loop           # replay an XDF 1.0 file as live LSL streams
labstream relay --disk-mb 512                 # wrap legacy liblsl outlets with resume/1 + disk spool
labstream calibrate --target EEG --reference Trig  # measure fixed hardware delay
```

## Browser viewer (WebSocket + WebAssembly)

Start the opt-in WebSocket bridge on loopback:

```sh
labstream relay --ws-port 8765
```

Build the 27 KB WebAssembly wire decoder:

```sh
cargo build --release --target wasm32-unknown-unknown -p labstream-wasm
```

Open `web/index.html` or connect from JavaScript through `web/labstream.js`.

## Enable transport encryption (`enc/1`)

Transport encryption is off by default so plain `liblsl` peers connect with zero
configuration. To enable mandatory encryption with a pre-shared key, add these
lines to `lsl_api.cfg`:

```ini
[labstream]
Features = {seq, resume, dgram, enc}
RequireEncryption = 1
EncryptionKey = your-lab-shared-secret
```

Read [labstream Extensions](extensions.md) for the X25519, HKDF-SHA256, and
ChaCha20-Poly1305 handshake rules.
