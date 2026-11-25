# Demo 5: Browser-Native WASM + Live `meta/1` Electrode Impedance

This demo runs an 8-channel, 250 Hz `Float32` EEG outlet together with the built-in RFC 6455 WebSocket bridge (`labstream::ws::WsBridge`) and the 27 KB `labstream-wasm` WebAssembly decoder.

What it demonstrates:

- **Zero-install browser decoding:** binary LSL wire frames travel over WebSocket and decode in the browser with `labstream-wasm`.
- **Live `meta/1` metadata (`0xFC`):** electrode impedance (`Fp1`..`O2` in kOhm) and battery level update live with zero stream reconnects. Clicking **Loosen C3** raises C3 impedance to `48 kOhm` via `meta/1` and injects 50 Hz mains hum onto C3; clicking **Apply Gel** restores normal contact.
- **Bidirectional Browser -> LSL markers:** clicking a marker button sends a WebSocket marker to `WsBridge`, which publishes it onto a real LSL `BrowserMarkers` outlet verified by a native Rust `Inlet`.

## Quick start

If `labstream_wasm.wasm` is not built yet:

```bash
cd labstream
cargo build --release --target wasm32-unknown-unknown -p labstream-wasm
```

Then run Demo 5:

```bash
cd labstream
cargo run --release --manifest-path demos/demo5_browser_meta/Cargo.toml
```

Open `http://127.0.0.1:8784/` in a browser.
