# Demo 3: Bandwidth Squeeze (`comp/1` + `select/1`)

This demo compares live wire throughput on a 128-channel, 1000 Hz `Float32` EEG stream (`4.17 Mbit/s` raw):

- **Standard LSL (uncompressed):** transmits all 128 channels at 1000 Hz (`4.17 Mbit/s`).
- **`labstream comp/1` (lossless):** transmits all 128 channels at 1000 Hz with bit-exact wire compression and verifies every float bit against the reference generator (`0` bit errors).
- **`labstream comp/1 + select/1` (outlet-side subset):** selects a channel subset (`128`, `32`, `8`, or `4` channels) and decimates with a Hamming-windowed sinc FIR filter (`1000`, `500`, `250`, or `125 Hz`) on the outlet before wire transmission.

## Quick start (1 machine)

```bash
cd labstream
cargo run --release --manifest-path demos/demo3_bandwidth/Cargo.toml
```

Open `http://127.0.0.1:8782/` in a browser.

## Two-machine run

Machine A (sender):

```bash
cargo run --release --manifest-path demos/demo3_bandwidth/Cargo.toml -- --mode sender
```

Machine B (viewer):

```bash
cargo run --release --manifest-path demos/demo3_bandwidth/Cargo.toml -- --mode viewer --peer <MACHINE_A_IP>
```

Open `http://127.0.0.1:8782/` on Machine B.
