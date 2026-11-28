#!/usr/bin/env python3
"""Vendor-agnostic live EEG hardware validation runner for labstream.

Connects labstream to an active external LSL stream, measures real interop,
arrival jitter, comp/1 compression ratio on physiological samples, and
SyncGroup alignment. No device names, vendor strings, or hostnames are logged.
"""

import argparse
import json
import os
import platform
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent
BENCH_DIR = Path(__file__).resolve().parent
BUILD_DIR = BENCH_DIR / ".build"
RESULTS_DIR = BENCH_DIR / "results"
ORACLE_INC = ROOT / "conformance" / ".build" / "install" / "include"
LABSTREAM_LIB_DIR = ROOT / "target" / "release"
WORKER_SRC = BENCH_DIR / "live_eeg_worker.c"
WORKER_BIN = BUILD_DIR / "live_eeg_worker"


def build_worker():
    BUILD_DIR.mkdir(parents=True, exist_ok=True)
    if not (LABSTREAM_LIB_DIR / "liblsl.so").exists():
        sys.exit(f"missing {LABSTREAM_LIB_DIR / 'liblsl.so'}; build labstream-capi --release first")
    subprocess.run(
        [
            "gcc", "-O2", "-Wall", "-Wextra",
            str(WORKER_SRC),
            f"-I{ORACLE_INC}",
            f"-L{LABSTREAM_LIB_DIR}",
            f"-Wl,-rpath,{LABSTREAM_LIB_DIR}",
            "-llsl", "-lm", "-ldl",
            "-o", str(WORKER_BIN),
        ],
        check=True,
    )


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--type", default="EEG", help="LSL stream type to match (default EEG, or ALL)")
    ap.add_argument("--secs", type=float, default=15.0, help="capture duration in seconds (default 15)")
    ap.add_argument("--peer", default="", help="optional LAN IP of the remote streaming PC")
    args = ap.parse_args()

    RESULTS_DIR.mkdir(parents=True, exist_ok=True)
    build_worker()

    peers = ["127.0.0.1"] + ([args.peer] if args.peer else [])
    peers_str = ", ".join(peers)
    fd, cfg = tempfile.mkstemp(prefix="live_eeg_", suffix=".cfg")
    with os.fdopen(fd, "w") as f:
        f.write(f"[lab]\nKnownPeers = {{{peers_str}}}\n\n[labstream]\nFeatures = {{seq, resume, comp}}\n")
    env = os.environ.copy()
    env.pop("LD_LIBRARY_PATH", None)
    env["LSLAPICFG"] = cfg

    print(f"[live-eeg] looking for active LSL stream (type={args.type}) for {args.secs:.0f}s...")
    try:
        out = subprocess.check_output(
            [str(WORKER_BIN), args.type, str(args.secs)],
            text=True, env=env, timeout=args.secs + 35,
        )
    finally:
        os.unlink(cfg)

    res = json.loads(out.strip().splitlines()[-1])
    report = {
        "kernel": platform.release(),
        "cores": os.cpu_count(),
        "timestamp_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "metrics": res,
    }
    out_json = RESULTS_DIR / "live-eeg-report.json"
    out_json.write_text(json.dumps(report, indent=2) + "\n")

    print("\n=== Anonymized Live Hardware Validation Report ===")
    print(f"  Streams discovered : {res['streams_discovered']} (primary type: {res['primary_type']}, {res['channels']} ch, {res['format']})")
    print(f"  Sampling rate      : nominal {res['nominal_srate']:.2f} Hz | effective {res['effective_srate']:.2f} Hz")
    print(f"  Samples / Chunks   : {res['samples_received']} samples across {res['chunks_received']} chunks ({res['finite_pct']:.2f}% finite)")
    print(f"  Drops / TS Gaps    : samples_lost={res['samples_lost']} | timestamp_gaps={res['timestamp_gaps']}")
    print(f"  Chunk arrival dt   : p50={res['chunk_dt_ms']['p50']:.2f} ms | p95={res['chunk_dt_ms']['p95']:.2f} ms | p99={res['chunk_dt_ms']['p99']:.2f} ms | max={res['chunk_dt_ms']['max']:.2f} ms")
    print(f"  Sample step error  : p50={res['step_err_ms']['p50']:.4f} ms | p99={res['step_err_ms']['p99']:.4f} ms | max={res['step_err_ms']['max']:.4f} ms")
    print(f"  Live comp/1 ratio  : active={bool(res['comp_active'])} | ratio={res['comp_ratio']:.4f} ({(1.0 - res['comp_ratio']) * 100.0:.1f}% wire reduction)")
    print(f"  SyncGroup check    : {res['sync_streams']} stream(s) (real_pair={bool(res['sync_real_pair'])}) | rows={res['sync_rows']} | hw_gaps={res['sync_hw_gaps']} | CPU={res['sync_cpu_pct']:.2f}% | RSS={res['rss_mb']:.1f} MB")
    print(f"  Saved report       : {out_json}")


if __name__ == "__main__":
    main()
