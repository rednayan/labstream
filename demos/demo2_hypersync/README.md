# Demo 2: Hyperscanning Phase-Lock (`liblsl` nearest-sample vs `labstream SyncGroup`)

Live oscilloscope showcase demonstrating how `labstream` v0.2.0 eliminates the
worst-case sub-sample phase shift between two asynchronous 250 SPS ADCs
(**72.0° at 100 Hz**, **43.2° at 60 Hz**, **36.0° at 50 Hz**) down to
**< 0.2°** in real time using `SyncGroup` (Blackman-windowed sinc
fractional-delay resampler, Step 18) + device-delay calibration (Step 15) +
drift clock (Step 17).

## Quick Run (1 machine — instant)

```bash
cargo run --release --manifest-path demos/demo2_hypersync/Cargo.toml
```

Open **http://127.0.0.1:8781** in Chrome or Firefox.

## 2-Machine Run (Windows + Linux)

**Machine A (Windows — publishes the two asynchronous ADCs):**
```powershell
cargo run --release --manifest-path demos/demo2_hypersync/Cargo.toml -- --mode sender
```

**Machine B (Linux — runs naive pairing vs `labstream SyncGroup` viewer):**
```bash
cargo run --release --manifest-path demos/demo2_hypersync/Cargo.toml -- --mode viewer --peer <WINDOWS_IP>
```

Open **http://127.0.0.1:8781** to watch both traces live, switch between
**50 Hz / 60 Hz / 100 Hz**, toggle **Pause**, and click **Export PNG** or
**Export SVG** for slides/README.
