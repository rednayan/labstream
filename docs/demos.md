# Interactive Demos

The `demos/` directory contains seven standalone Cargo packages. Each package
runs outside the root workspace, starts live LSL streams, and serves a local
browser dashboard with PNG export.

| # | Directory | Feature demonstrated | Verified result | Port |
|---|---|---|---|---|
| 1 | `demos/demo1_dropout` | `resume/1` + `dgram/1` + CRC32 vs C++ `liblsl` over Wi-Fi cut | `0` lost samples, bit-exact CRC32, offline spike recovered | `8780` |
| 2 | `demos/demo2_hypersync` | `SyncGroup` phase-lock (100 Hz @ 250 SPS, 2.0 ms offset) | `0.014 deg` error vs `72-144 deg` naive nearest-sample | `8781` |
| 3 | `demos/demo3_bandwidth` | `comp/1` lossless + `select/1` FIR on 128 ch @ 1000 Hz | `4.17 -> 2.27 Mbit/s` (`comp/1`, `0` bit errors), `0.063 Mbit/s` (`8 ch @ 250 Hz`) | `8782` |
| 4 | `demos/demo4_shm_ipc` | Cross-process `shm/1` ring vs TCP loopback (64 ch @ 1000 Hz) | `8.8 us` p50 vs `78.3 us` TCP (`8.9x` faster) | `8783` |
| 5 | `demos/demo5_browser_meta` | 27 KB `labstream-wasm` + `WsBridge` + live `meta/1` impedance | Live 10-20 impedance updates with zero reconnect, Browser -> LSL markers | `8784` |
| 6 | `demos/demo6_encryption` | `enc/1` ChaCha20-Poly1305 live TCP wire sniffer | `0` plaintext leaks, `7.3+ bits/B` entropy, `403 Encryption required` on probe | `8785` |
| 7 | `demos/demo7_multimodal_sync` | `SyncGroup` fusion (`500` + `250` + `120 Hz` -> `250 Hz`) | Exact `gap_mask` (`0b001`/`0b010`/`0b100`) on 0.8 s sensor dropout | `8786` |

## Run commands

Run each command from the repository root (`labstream/`).

### Demo 1: Wi-Fi dropout recovery against C++ `liblsl`

Machine A (sender, Windows or Linux):

```sh
cargo run --release --manifest-path demos/demo1_dropout/Cargo.toml --bin demo1-sender
```

Machine B (viewer with genuine C++ `liblsl` oracle sidecar):

```sh
./demos/demo1_dropout/run_viewer.sh --peer <MACHINE_A_IP>
```

Open `http://127.0.0.1:8780/`. Press Enter on the sender during an offline cut
to inject an `OFFLINE_PROOF` EEG spike and marker.

### Demo 2: Hyperscanning phase-lock

```sh
cargo run --release --manifest-path demos/demo2_hypersync/Cargo.toml
```

Open `http://127.0.0.1:8781/`.

### Demo 3: Bandwidth squeeze (`comp/1` + `select/1`)

```sh
cargo run --release --manifest-path demos/demo3_bandwidth/Cargo.toml
```

Open `http://127.0.0.1:8782/`.

### Demo 4: Local microsecond IPC (`shm/1` vs TCP)

```sh
cargo run --release --manifest-path demos/demo4_shm_ipc/Cargo.toml
```

Open `http://127.0.0.1:8783/`.

### Demo 5: Browser WASM + live `meta/1` impedance

```sh
cargo build --release --target wasm32-unknown-unknown -p labstream-wasm
cargo run --release --manifest-path demos/demo5_browser_meta/Cargo.toml
```

Open `http://127.0.0.1:8784/`.

### Demo 6: Hospital Wi-Fi security (`enc/1` wire sniffer)

```sh
cargo run --release --manifest-path demos/demo6_encryption/Cargo.toml
```

Open `http://127.0.0.1:8785/`.

### Demo 7: Multi-modal `SyncGroup` fusion + `gap_mask`

```sh
cargo run --release --manifest-path demos/demo7_multimodal_sync/Cargo.toml
```

Open `http://127.0.0.1:8786/`.
