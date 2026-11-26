#!/usr/bin/env python3
"""Multi-device hyperscanning benchmark suite (2, 4, and 8 simultaneous devices).

Measures:
  - H1: Device scaling sweep (2, 4, 8 devices x 8 ch @ 250 SPS, 40 Hz & 100 Hz)
  - H2: 8-device heterogeneous-rate fusion (4x 500 Hz + 2x 250 Hz + 2x 120 Hz -> 250 Hz)
  - H3: 8-device single-participant 1.0 s dropout (gap_mask isolation + zero stall)

Outputs to conformance/bench/results/:
  - hypersync-results.json
  - hypersync-summary.md
  - hypersync-scaling.svg
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
WORKER_SRC = BENCH_DIR / "hypersync_worker.c"
WORKER_BIN = BUILD_DIR / "hypersync_worker"


def build_worker():
    BUILD_DIR.mkdir(parents=True, exist_ok=True)
    if not (LABSTREAM_LIB_DIR / "liblsl.so").exists():
        sys.exit("missing target/release/liblsl.so — run: cargo build --release -p labstream-capi")
    subprocess.run(
        [
            "gcc", "-O2", "-Wall", "-Wextra",
            str(WORKER_SRC),
            f"-I{ORACLE_INC}",
            f"-L{LABSTREAM_LIB_DIR}",
            f"-Wl,-rpath,{LABSTREAM_LIB_DIR}",
            "-llsl", "-lm", "-lpthread",
            "-o", str(WORKER_BIN),
        ],
        check=True,
    )


def run_case(n_devs: int, ch: int, mode: str, stim_hz: float, secs: float) -> dict:
    fd, cfg = tempfile.mkstemp(prefix="hs_bench_", suffix=".cfg")
    with os.fdopen(fd, "w") as f:
        f.write("[labstream]\nFeatures = {seq, resume, shm}\n")
    env = os.environ.copy()
    env.pop("LD_LIBRARY_PATH", None)
    env["LSLAPICFG"] = cfg
    try:
        out = subprocess.check_output(
            [str(WORKER_BIN), str(n_devs), str(ch), mode, str(stim_hz), str(secs)],
            text=True, env=env, timeout=secs + 30,
        )
        return json.loads(out.strip().splitlines()[-1])
    finally:
        os.unlink(cfg)


def write_svg(path: Path, cases: list[dict], git_sha: str):
    W = 940
    row_h = 56
    H = 104 + len(cases) * row_h + 24
    bar_x = 340
    bar_w = 430
    max_cpu = max([c["cpu_pct"] for c in cases] + [5.0])

    lines = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}">',
        f'<rect width="{W}" height="{H}" rx="12" fill="#0b0f17"/>',
        f'<text x="24" y="36" fill="#e6edf7" font-family="system-ui,sans-serif" font-size="19" font-weight="700">labstream SyncGroup Hyperscanning Scaling (2 to 8 Devices)</text>',
        f'<text x="24" y="58" fill="#8ea0b8" font-family="monospace" font-size="12">32-tap Blackman-sinc multi-stream alignment | commit {git_sha}</text>',
    ]
    for i, c in enumerate(cases):
        y = 92 + i * row_h
        lbl = f"{c['devices']} devices ({c['total_channels']} ch, {c['mode']}, {c['stim_hz']:.0f} Hz)"
        w = max(8.0, (c["cpu_pct"] / max_cpu) * bar_w)
        note = f"phase: {c['synced_worst_deg']:.3f}° vs {c['naive_worst_deg']:.1f}° naive | RMS: {c['synced_worst_rms_pct']:.3f}% | RSS: {c['rss_mb']:.1f} MB"
        lines.append(f'<text x="24" y="{y + 19}" fill="#cbd5e1" font-family="system-ui,sans-serif" font-size="13" font-weight="600">{lbl}</text>')
        lines.append(f'<rect x="{bar_x}" y="{y + 4}" width="{bar_w}" height="22" rx="5" fill="#131a26"/>')
        lines.append(f'<rect x="{bar_x}" y="{y + 4}" width="{w:.1f}" height="22" rx="5" fill="#34d399"/>')
        lines.append(f'<text x="{bar_x + bar_w + 12}" y="{y + 20}" fill="#e6edf7" font-family="monospace" font-size="13" font-weight="700">{c["cpu_pct"]:.1f}% CPU</text>')
        lines.append(f'<text x="24" y="{y + 38}" fill="#64748b" font-family="monospace" font-size="11">{note}</text>')
    lines.append("</svg>")
    path.write_text("\n".join(lines) + "\n")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--secs", type=float, default=5.0, help="seconds per scenario (default 5)")
    args = ap.parse_args()

    RESULTS_DIR.mkdir(parents=True, exist_ok=True)
    build_worker()
    git_sha = subprocess.check_output(
        ["git", "-C", str(ROOT), "rev-parse", "--short", "HEAD"], text=True
    ).strip()

    scenarios = [
        ("H1a: 2-device hyperscanning (16 ch, 100 Hz)", 2, 8, "uniform", 100.0, args.secs),
        ("H1b: 4-device hyperscanning (32 ch, 100 Hz)", 4, 8, "uniform", 100.0, args.secs),
        ("H1c: 8-device hyperscanning (64 ch, 100 Hz)", 8, 8, "uniform", 100.0, args.secs),
        ("H2:  8-device heterogeneous rates (500/250/120 Hz -> 250 Hz, 40 Hz)", 8, 4, "mixed", 40.0, args.secs),
        ("H3:  8-device w/ 1.0 s headset #4 dropout (gap_mask)", 8, 8, "dropout", 40.0, max(args.secs, 5.0)),
    ]

    rows = []
    for title, n_devs, ch, mode, stim_hz, secs in scenarios:
        print(f"[hypersync] {title}...")
        res = run_case(n_devs, ch, mode, stim_hz, secs)
        res["title"] = title
        rows.append(res)

    payload = {
        "git_sha": git_sha,
        "kernel": platform.release(),
        "cores": os.cpu_count(),
        "timestamp_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "scenarios": rows,
    }
    (RESULTS_DIR / "hypersync-results.json").write_text(json.dumps(payload, indent=2) + "\n")

    md = [
        f"# labstream Multi-Device Hyperscanning Benchmark (`{git_sha}`)",
        "",
        "| Scenario | Devices (Total Ch) | Naive Worst Phase | SyncGroup Worst Phase | SyncGroup RMS | CPU (%) | RSS (MB) | Gap Isolation |",
        "|---|---:|---:|---:|---:|---:|---:|---|",
    ]
    for r in rows:
        gap_note = (
            f"dev#4=`{r['drop_dev3_gaps']}`, healthy=`{r['healthy_dev_gaps']}`"
            if r["mode"] == "dropout"
            else f"`{r['healthy_dev_gaps']}` gaps"
        )
        md.append(
            f"| {r['title']} | {r['devices']} ({r['total_channels']} ch) | {r['naive_worst_deg']:.1f}° | **{r['synced_worst_deg']:.4f}°** | **{r['synced_worst_rms_pct']:.3f}%** | {r['cpu_pct']:.1f}% | {r['rss_mb']:.1f} | {gap_note} |"
        )
    md_text = "\n".join(md) + "\n"
    (RESULTS_DIR / "hypersync-summary.md").write_text(md_text)
    write_svg(RESULTS_DIR / "hypersync-scaling.svg", rows, git_sha)
    print("\n" + md_text)
    print(f"[hypersync] artifacts written to {RESULTS_DIR}/")


if __name__ == "__main__":
    main()
