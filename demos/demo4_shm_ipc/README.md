# Demo 4: Local Microsecond IPC (`shm/1` vs TCP Loopback)

This demo races two separate OS processes against the same 64-channel `Float32` closed-loop outlet on one machine:

- **Standard LSL (TCP loopback):** receives every sample through the kernel TCP stack.
- **`labstream shm/1` (shared-memory ring):** receives every sample through a lock-free shared-memory ring buffer.

Both child processes measure true one-way IPC latency (`receive_clock - push_clock`) against the shared monotonic clock (`CLOCK_MONOTONIC`).

## Quick start

```bash
cd labstream
cargo run --release --manifest-path demos/demo4_shm_ipc/Cargo.toml
```

Open `http://127.0.0.1:8783/` in a browser.
