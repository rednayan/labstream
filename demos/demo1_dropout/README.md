# Demo 1: "Pull the Wi-Fi Cable" (`liblsl` vs `labstream`)

Two-machine live showcase comparing genuine C++ `liblsl` (`e651023c`) against
`labstream` v0.2.0 (`seq/1 + resume/1 + dgram/1`) over the exact same Wi-Fi or
Ethernet link.

## Machine A — Sender (Windows laptop)

1. Clone this repository on your Windows machine and open a terminal inside it.
2. Start the sender:

```powershell
cargo run --release --manifest-path demos/demo1_dropout/Cargo.toml --bin demo1-sender
```

3. Note the IPv4 address printed by `demo1-sender` (for example `192.168.1.42`).
   If Windows Firewall prompts, allow access on Private networks.

## Machine B — Viewer (Linux PC)

1. Start the split-screen viewer (pass `--peer <WINDOWS_IP>` so discovery works
   even if Wi-Fi blocks multicast):

```bash
./demos/demo1_dropout/run_viewer.sh --peer 192.168.1.42
```

2. Open **http://127.0.0.1:8780** in Chrome or Firefox.

## Run the Demo

1. Wait until both **liblsl (top)** and **labstream (bottom)** show **LIVE**
   and smooth waveforms + green `STIM_xxxx ✓` CRC32-verified markers.
2. On **Machine A (Windows)**, turn Wi-Fi **OFF** (or unplug the Ethernet cable)
   for **5 to 10 seconds**.
3. **While Machine A is completely offline**, press **Enter** (or type a word
   like `OFFLINE_PROOF` and press **Enter**) in the Windows terminal to inject
   an unpredictable sharp EEG spike and custom marker.
4. Turn Wi-Fi back **ON** (or plug the cable back in) and watch the screen:
   - **liblsl (top):** freezes / permanently loses **~2,500–5,000 samples**,
     misses every marker, and never sees your offline event.
   - **labstream (bottom):** reconnects, triggers `resume/1`, backfills
     **100 % of samples in gold/amber**, verifies every 1-second block with
     **`CRC32: 100% BIT-EXACT` (`✓`)**, and displays your exact **`★ EVENT`**
     marker + sharp EEG spike right inside the gold recovered window!
5. Click **Export PNG** or **Export SVG** in the top bar to save a
   presentation-ready comparison chart with all live counter and CRC32 badges.
