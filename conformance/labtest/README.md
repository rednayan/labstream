# Virtual Lab Multi-Node Testbed (`conformance/labtest/`)

This directory runs a four-container Docker Compose virtual LAN (`172.29.0.0/24`)
with isolated kernel network stacks, separate `/dev/shm` namespaces, and real
`tc netem` + `iptables` fault injection.

## Nodes

| Container | IP | Role |
|---|---|---|
| `vendor-amp` | `172.29.0.10` | Genuine C++ `liblsl` (16 ch @ 250 Hz) wrapped by `labstream relay --disk-mb 128` |
| `hd-eeg-node` | `172.29.0.11` | `labstream` 32 ch @ 250 Hz EEG (`comp/1` + `meta/1`), mandatory `enc/1` markers, and `WsBridge` |
| `recorder-node` | `172.29.0.12` | Connects across the virtual LAN, injects `tc netem` loss/jitter, 4 s hard link blackout, and multicast block, and verifies 0 loss, bit-exact values, `comp/1`, `meta/1`, `enc/1`, and RSS/FD stability |
| `browser-node` | `172.29.0.13` | Node.js + 27 KB `labstream-wasm` decoding live binary WebSocket frames over the LAN and sending browser markers back to LSL |

## Prerequisites

Build the release binaries and WASM module from `labstream/`:

```sh
cargo build --release -p labstream-capi -p labstream-cli
cargo build --release --target wasm32-unknown-unknown -p labstream-wasm
```

## Run

```sh
# Quick CI profile (~35 s chaos run)
./conformance/labtest/run_labtest.sh --profile quick

# Custom duration (for example 4-hour overnight soak)
./conformance/labtest/run_labtest.sh --minutes 240
```
