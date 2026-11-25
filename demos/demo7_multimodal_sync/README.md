# Demo 7: Multi-Modal Heterogeneous-Rate Fusion (`SyncGroup` Step 18)

This demo publishes three independent LSL streams at three different sampling rates:

- **`Demo7_EEG` at `500 Hz`** (downsampled by 2x onto the common grid)
- **`Demo7_ECG` at `250 Hz`** (with a 1.7 ms fractional sample offset)
- **`Demo7_GSR` at `120 Hz`** (upsampled by non-integer ratio 2.083x onto the common grid)

`labstream::SyncGroup` resamples all three streams in real time onto one unified **`250 Hz`** monotonic time grid using its 32-tap Blackman-windowed sinc kernel.

Clicking **Drop EEG**, **Drop ECG**, or **Drop GSR** injects an 0.8-second sensor dropout. `SyncGroup` flags the missing interval explicitly in `gap_mask` (`0b001`, `0b010`, `0b100`) with `NaN` values instead of fabricating interpolated samples, while the other two modalities stay aligned.

## Quick start

```bash
cd labstream
cargo run --release --manifest-path demos/demo7_multimodal_sync/Cargo.toml
```

Open `http://127.0.0.1:8786/` in a browser.
