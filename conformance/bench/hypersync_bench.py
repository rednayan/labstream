#!/usr/bin/env python3
"""Head-to-head multi-device hyperscanning benchmark: C++ liblsl vs labstream SyncGroup.

Tests 2, 4, and 8 simultaneous asynchronous devices at 250 Hz, 1000 Hz, and 2000 Hz,
heterogeneous multi-modal rates (1000/500/250 Hz -> 500 Hz), and 8-device headset dropout.
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
ORACLE_LIB_DIR = ROOT / "conformance" / ".build" / "install" / "lib"
LABSTREAM_LIB_DIR = ROOT / "target" / "release"
WORKER_SRC = BENCH_DIR / "hypersync_worker.c"
BIN_LIBLSL = BUILD_DIR / "hypersync_liblsl"
BIN_LABSTREAM = BUILD_DIR / "hypersync_labstream"


def build_workers():
    BUILD_DIR.mkdir(parents=True, exist_ok=True)
    for out_bin, lib_dir in [(BIN_LIBLSL, ORACLE_LIB_DIR), (BIN_LABSTREAM, LABSTREAM_LIB_DIR)]:
        if not (lib_dir / "liblsl.so").exists():
            sys.exit(f"missing {lib_dir / 'liblsl.so'}")
        subprocess.run(
            [
                "gcc", "-O2", "-Wall", "-Wextra",
                str(WORKER_SRC),
                f"-I{ORACLE_INC}",
                f"-L{lib_dir}",
                f"-Wl,-rpath,{lib_dir}",
                "-llsl", "-lm", "-ldl", "-lpthread",
                "-o", str(out_bin),
            ],
            check=True,
        )


def run_one(binary: Path, engine: str, n_devs: int, ch: int, srate: float, mode: str, stim_hz: float, secs: float) -> dict:
    fd, cfg = tempfile.mkstemp(prefix="hs_bench_", suffix=".cfg")
    with os.fdopen(fd, "w") as f:
        f.write("[ports]\nPortRange = 128\n\n[lab]\nKnownPeers = {127.0.0.1}\n")
        if engine == "sync":
            f.write("\n[labstream]\nFeatures = {seq, resume, shm}\n")
    env = os.environ.copy()
    env.pop("LD_LIBRARY_PATH", None)
    env["LSLAPICFG"] = cfg
    try:
        out = subprocess.check_output(
            [str(binary), engine, str(n_devs), str(ch), str(srate), mode, str(stim_hz), str(secs)],
            text=True, env=env, timeout=secs + 40,
        )
        return json.loads(out.strip().splitlines()[-1])
    finally:
        os.unlink(cfg)


def write_svg(path: Path, pairs: list[dict], git_sha: str):
    W = 960
    row_h = 68
    H = 104 + len(pairs) * row_h + 24
    bar_x = 340
    bar_w = 450
    max_deg = max([max(p["liblsl"]["worst_deg"], p["labstream"]["worst_deg"]) for p in pairs] + [10.0])

    lines = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}">',
        f'<rect width="{W}" height="{H}" rx="12" fill="#0b0f17"/>',
        f'<text x="24" y="36" fill="#e6edf7" font-family="system-ui,sans-serif" font-size="19" font-weight="700">Hyperscanning Worst-Pair Phase Error: C++ liblsl vs labstream SyncGroup</text>',
        f'<text x="24" y="58" fill="#8ea0b8" font-family="monospace" font-size="12">2 to 8 devices | 250 Hz to 2000 Hz | Red = C++ liblsl nearest-sample, Green = labstream SyncGroup | {git_sha}</text>',
    ]
    for i, p in enumerate(pairs):
        y = 90 + i * row_h
        lsl = p["liblsl"]
        ls = p["labstream"]
        w_lsl = max(6.0, (lsl["worst_deg"] / max_deg) * bar_w)
        w_ls = max(4.0, (ls["worst_deg"] / max_deg) * bar_w)
        lines.append(f'<text x="24" y="{y + 18}" fill="#cbd5e1" font-family="system-ui,sans-serif" font-size="13" font-weight="600">{p["label"]}</text>')
        lines.append(f'<rect x="{bar_x}" y="{y + 4}" width="{bar_w}" height="18" rx="4" fill="#131a26"/>')
        lines.append(f'<rect x="{bar_x}" y="{y + 4}" width="{w_lsl:.1f}" height="18" rx="4" fill="#f87171"/>')
        lines.append(f'<text x="{bar_x + bar_w + 10}" y="{y + 17}" fill="#fca5a5" font-family="monospace" font-size="12" font-weight="700">{lsl["worst_deg"]:.1f}° (CPU {lsl["cpu_pct"]:.1f}%)</text>')
        lines.append(f'<rect x="{bar_x}" y="{y + 26}" width="{bar_w}" height="18" rx="4" fill="#131a26"/>')
        lines.append(f'<rect x="{bar_x}" y="{y + 26}" width="{w_ls:.1f}" height="18" rx="4" fill="#34d399"/>')
        lines.append(f'<text x="{bar_x + bar_w + 10}" y="{y + 39}" fill="#6ee7b7" font-family="monospace" font-size="12" font-weight="700">{ls["worst_deg"]:.3f}° (CPU {ls["cpu_pct"]:.1f}%)</text>')
        lines.append(f'<text x="24" y="{y + 40}" fill="#64748b" font-family="monospace" font-size="11">{ls["devices"]} devs, {ls["total_channels"]} ch @ {ls["base_srate"]:.0f} Hz | RSS: {lsl["rss_mb"]:.0f} vs {ls["rss_mb"]:.0f} MB</text>')
    lines.append("</svg>")
    path.write_text("\n".join(lines) + "\n")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--secs", type=float, default=4.0, help="seconds per run (default 4)")
    args = ap.parse_args()

    RESULTS_DIR.mkdir(parents=True, exist_ok=True)
    build_workers()
    git_sha = subprocess.check_output(
        ["git", "-C", str(ROOT), "rev-parse", "--short", "HEAD"], text=True
    ).strip()

    # (label, n_devs, ch_per_dev, base_srate, mode, stim_hz, secs)
    scenarios = [
        ("H1a: 2-dev @ 250 Hz (16 ch)", 2, 8, 250.0, "uniform", 100.0, args.secs),
        ("H1b: 4-dev @ 250 Hz (32 ch)", 4, 8, 250.0, "uniform", 100.0, args.secs),
        ("H1c: 8-dev @ 250 Hz (64 ch)", 8, 8, 250.0, "uniform", 100.0, args.secs),
        ("H2:  8-dev @ 1000 Hz (64 ch, high rate)", 8, 8, 1000.0, "uniform", 200.0, args.secs),
        ("H3:  8-dev @ 2000 Hz (64 ch, ultra-high)", 8, 8, 2000.0, "uniform", 400.0, args.secs),
        ("H4:  8-dev mixed 1000/500/250 -> 500 Hz", 8, 4, 500.0, "mixed", 80.0, args.secs),
        ("H5:  8-dev @ 500 Hz + 1.0 s dev#4 drop", 8, 8, 500.0, "dropout", 80.0, max(args.secs, 4.5)),
    ]

    pairs = []
    for label, n_devs, ch, srate, mode, stim_hz, secs in scenarios:
        print(f"[hypersync] {label} — running C++ liblsl...")
        lsl_res = run_one(BIN_LIBLSL, "naive", n_devs, ch, srate, mode, stim_hz, secs)
        print(f"[hypersync] {label} — running labstream SyncGroup...")
        ls_res = run_one(BIN_LABSTREAM, "sync", n_devs, ch, srate, mode, stim_hz, secs)
        pairs.append({"label": label, "liblsl": lsl_res, "labstream": ls_res})

    payload = {
        "git_sha": git_sha,
        "kernel": platform.release(),
        "cores": os.cpu_count(),
        "timestamp_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "pairs": pairs,
    }
    (RESULTS_DIR / "hypersync-results.json").write_text(json.dumps(payload, indent=2) + "\n")

    md = [
        f"# Head-to-Head Hyperscanning Benchmark: C++ `liblsl` vs `labstream SyncGroup` (`{git_sha}`)",
        "",
        "| Scenario | C++ `liblsl` Phase Err | `SyncGroup` Phase Err | `liblsl` CPU / RSS | `SyncGroup` CPU / RSS | Notes |",
        "|---|---:|---:|---:|---:|---|",
    ]
    for p in pairs:
        lsl = p["liblsl"]
        ls = p["labstream"]
        note = (
            f"dev#4 gaps=`{ls['drop_dev3_gaps']}`, healthy=`{ls['healthy_dev_gaps']}` (`liblsl` unflagged)"
            if ls["mode"] == "dropout"
            else f"`SyncGroup` RMS `{ls['worst_rms_pct']:.3f}%` vs `liblsl` `{lsl['worst_rms_pct']:.1f}%`"
        )
        md.append(
            f"| {p['label']} | {lsl['worst_deg']:.1f}° | **{ls['worst_deg']:.4f}°** | {lsl['cpu_pct']:.1f}% / {lsl['rss_mb']:.1f} MB | **{ls['cpu_pct']:.1f}% / {ls['rss_mb']:.1f} MB** | {note} |"
        )
    md_text = "\n".join(md) + "\n"
    (RESULTS_DIR / "hypersync-summary.md").write_text(md_text)
    write_svg(RESULTS_DIR / "hypersync-scaling.svg", pairs, git_sha)
    print("\n" + md_text)
    print(f"[hypersync] artifacts written to {RESULTS_DIR}/")


if __name__ == "__main__":
    main()
