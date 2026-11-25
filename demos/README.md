# Interactive demos for labstream v0.2.0

Each demo is a standalone Cargo package outside the root workspace. Each demo
opens a local browser dashboard with live metrics and PNG export.

| # | Directory | Feature showcase | URL |
|---|---|---|---|
| 1 | `demo1_dropout` | Wi-Fi dropout recovery (`resume/1` + `dgram/1` + CRC32) against C++ `liblsl` | `http://127.0.0.1:8780/` |
| 2 | `demo2_hypersync` | Hyperscanning phase-lock (`SyncGroup` 0.014 deg vs 72-144 deg naive) | `http://127.0.0.1:8781/` |
| 3 | `demo3_bandwidth` | Bandwidth squeeze (`comp/1` lossless + `select/1` FIR decimation on 128 ch @ 1000 Hz) | `http://127.0.0.1:8782/` |
| 4 | `demo4_shm_ipc` | Cross-process local IPC (`shm/1` shared-memory ring vs TCP loopback) | `http://127.0.0.1:8783/` |
| 5 | `demo5_browser_meta` | Browser WebAssembly decoder (`labstream-wasm` + `WsBridge`) and live `meta/1` impedance | `http://127.0.0.1:8784/` |
| 6 | `demo6_encryption` | Hospital Wi-Fi security (`enc/1` ChaCha20-Poly1305 live TCP wire sniffer) | `http://127.0.0.1:8785/` |
| 7 | `demo7_multimodal_sync` | Multi-modal fusion (`EEG 500 Hz` + `ECG 250 Hz` + `GSR 120 Hz` -> `250 Hz` + `gap_mask`) | `http://127.0.0.1:8786/` |

## Quick run commands

Run from the repository root (`labstream/`):

```sh
# Demo 1 (two machines: see demos/demo1_dropout/README.md)
./demos/demo1_dropout/run_viewer.sh --peer <SENDER_IP>

# Demo 2 (1-machine instant run)
cargo run --release --manifest-path demos/demo2_hypersync/Cargo.toml

# Demo 3 (1-machine instant run)
cargo run --release --manifest-path demos/demo3_bandwidth/Cargo.toml

# Demo 4 (cross-process local IPC)
cargo run --release --manifest-path demos/demo4_shm_ipc/Cargo.toml

# Demo 5 (browser WASM + live meta/1)
cargo run --release --manifest-path demos/demo5_browser_meta/Cargo.toml

# Demo 6 (live TCP wire sniffer)
cargo run --release --manifest-path demos/demo6_encryption/Cargo.toml

# Demo 7 (multi-modal SyncGroup fusion)
cargo run --release --manifest-path demos/demo7_multimodal_sync/Cargo.toml
```
