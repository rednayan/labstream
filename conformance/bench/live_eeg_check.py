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


def write_svg(path: Path, report: dict):
    res = report["metrics"]
    W, H = 960, 520
    c_dt = res["chunk_dt_ms"]
    s_err = res["step_err_ms"]
    comp_pct = res["comp_ratio"] * 100.0
    saved_pct = max(0.0, 100.0 - comp_pct)
    max_ms = max(c_dt["max"], s_err["max"], 10.0)

    def bar_w(val: float, max_val: float, span: float) -> float:
        return max(4.0, min(span, (val / max_val) * span))

    lines = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}">',
        f'<rect width="{W}" height="{H}" rx="12" fill="#0b0f17"/>',
        f'<text x="24" y="36" fill="#e6edf7" font-family="system-ui,sans-serif" font-size="19" font-weight="700">Live Wireless EEG Hardware Validation (Remote PC over LAN)</text>',
        f'<text x="24" y="57" fill="#8ea0b8" font-family="monospace" font-size="12">Primary: {res["primary_type"]} ({res["channels"]} ch, {res["format"]} @ {res["nominal_srate"]:.0f} Hz) | {res["streams_discovered"]} streams discovered | {report["timestamp_utc"]}</text>',
    ]

    # Top 4 KPI cards
    cards = [
        ("Effective Rate", f'{res["effective_srate"]:.2f} Hz', f'nominal {res["nominal_srate"]:.0f} Hz ({res["finite_pct"]:.0f}% finite)', "#38bdf8"),
        ("comp/1 Wire Ratio", f'{res["comp_ratio"]:.3f}x', f'{saved_pct:.1f}% bandwidth saved', "#34d399"),
        ("SyncGroup Gaps", f'{res["sync_hw_gaps"]} / {res["sync_rows"]}', f'{res["sync_streams"]} live streams aligned', "#34d399" if res["sync_hw_gaps"] == 0 else "#f87171"),
        ("Client Footprint", f'{res["sync_cpu_pct"]:.2f}% CPU', f'{res["rss_mb"]:.1f} MB RSS ({res["samples_received"]} samples)', "#a78bfa"),
    ]
    for idx, (title, big, sub, color) in enumerate(cards):
        cx = 24 + idx * 232
        lines.append(f'<rect x="{cx}" y="74" width="220" height="82" rx="8" fill="#131a26" stroke="#1e293b"/>')
        lines.append(f'<text x="{cx + 14}" y="96" fill="#94a3b8" font-family="system-ui,sans-serif" font-size="11" font-weight="600">{title}</text>')
        lines.append(f'<text x="{cx + 14}" y="126" fill="{color}" font-family="monospace" font-size="22" font-weight="700">{big}</text>')
        lines.append(f'<text x="{cx + 14}" y="146" fill="#64748b" font-family="monospace" font-size="10">{sub}</text>')

    # Panel A: Wireless arrival & step jitter bars
    lines.append('<rect x="24" y="174" width="448" height="220" rx="8" fill="#131a26" stroke="#1e293b"/>')
    lines.append('<text x="40" y="200" fill="#e2e8f0" font-family="system-ui,sans-serif" font-size="14" font-weight="700">Wireless Packet Arrival &amp; Raw Step Jitter (ms)</text>')
    jitter_rows = [
        ("Chunk dt p50", c_dt["p50"], "#38bdf8"),
        ("Chunk dt p95", c_dt["p95"], "#fbbf24"),
        ("Chunk dt p99", c_dt["p99"], "#f97316"),
        ("Chunk dt max", c_dt["max"], "#f87171"),
        ("Step err p50", s_err["p50"], "#38bdf8"),
        ("Step err p99", s_err["p99"], "#f97316"),
        ("Step err max", s_err["max"], "#f87171"),
    ]
    for i, (lbl, val, col) in enumerate(jitter_rows):
        ry = 220 + i * 23
        bw = bar_w(val, max_ms, 210.0)
        lines.append(f'<text x="40" y="{ry + 12}" fill="#cbd5e1" font-family="monospace" font-size="11">{lbl}</text>')
        lines.append(f'<rect x="148" y="{ry}" width="210" height="14" rx="3" fill="#0b0f17"/>')
        lines.append(f'<rect x="148" y="{ry}" width="{bw:.1f}" height="14" rx="3" fill="{col}"/>')
        lines.append(f'<text x="366" y="{ry + 11}" fill="#e2e8f0" font-family="monospace" font-size="11">{val:.2f} ms</text>')

    # Panel B: comp/1 bandwidth comparison
    lines.append('<rect x="488" y="174" width="448" height="104" rx="8" fill="#131a26" stroke="#1e293b"/>')
    lines.append('<text x="504" y="200" fill="#e2e8f0" font-family="system-ui,sans-serif" font-size="14" font-weight="700">Live Wire Bandwidth: Raw TCP vs comp/1</text>')
    lines.append('<text x="504" y="226" fill="#94a3b8" font-family="monospace" font-size="11">Raw TCP</text>')
    lines.append('<rect x="590" y="214" width="260" height="16" rx="4" fill="#64748b"/>')
    lines.append('<text x="858" y="226" fill="#cbd5e1" font-family="monospace" font-size="11">100.0%</text>')
    cw = bar_w(comp_pct, 100.0, 260.0)
    lines.append('<text x="504" y="254" fill="#6ee7b7" font-family="monospace" font-size="11">comp/1</text>')
    lines.append('<rect x="590" y="242" width="260" height="16" rx="4" fill="#0b0f17"/>')
    lines.append(f'<rect x="590" y="242" width="{cw:.1f}" height="16" rx="4" fill="#34d399"/>')
    lines.append(f'<text x="858" y="254" fill="#6ee7b7" font-family="monospace" font-size="11">{comp_pct:.1f}%</text>')

    # Panel C: SyncGroup health summary
    lines.append('<rect x="488" y="290" width="448" height="104" rx="8" fill="#131a26" stroke="#1e293b"/>')
    lines.append('<text x="504" y="316" fill="#e2e8f0" font-family="system-ui,sans-serif" font-size="14" font-weight="700">Live SyncGroup Dual-Stream Alignment</text>')
    good_rows = max(0, res["sync_rows"] - res["sync_hw_gaps"])
    good_pct = (100.0 * good_rows / res["sync_rows"]) if res["sync_rows"] > 0 else 0.0
    gw = bar_w(good_pct, 100.0, 320.0)
    lines.append('<rect x="504" y="332" width="320" height="20" rx="4" fill="#7f1d1d"/>')
    lines.append(f'<rect x="504" y="332" width="{gw:.1f}" height="20" rx="4" fill="#34d399"/>')
    lines.append(f'<text x="834" y="346" fill="#6ee7b7" font-family="monospace" font-size="12" font-weight="700">{good_pct:.1f}% clean</text>')
    lines.append(f'<text x="504" y="374" fill="#94a3b8" font-family="monospace" font-size="11">Aligned rows: {res["sync_rows"]} | Hardware gaps: {res["sync_hw_gaps"]} | Dejitter: ON</text>')

    # Footer note
    lines.append('<rect x="24" y="410" width="912" height="86" rx="8" fill="#131a26" stroke="#1e293b"/>')
    lines.append('<text x="40" y="436" fill="#e2e8f0" font-family="system-ui,sans-serif" font-size="13" font-weight="700">Capture Summary (Vendor-Agnostic)</text>')
    lines.append(f'<text x="40" y="458" fill="#94a3b8" font-family="monospace" font-size="11">Duration: {res["duration_sec"]:.2f} s | Chunks: {res["chunks_received"]} | Raw burst TS jumps: {res["timestamp_gaps"]} | Inlet CPU: {res["inlet_cpu_pct"]:.2f}%</text>')
    lines.append(f'<text x="40" y="478" fill="#94a3b8" font-family="monospace" font-size="11">Signal range: [{res["signal_min"]:.1f}, {res["signal_max"]:.1f}] | RMS: {res["signal_rms"]:.1f} | Kernel: {report["kernel"]}</text>')

    lines.append("</svg>")
    path.write_text("\n".join(lines) + "\n")


def write_staircase_svg(path: Path, report: dict):
    res = report["metrics"]
    raw = res.get("raw_trace_ms", [])
    sync = res.get("sync_trace_ms", [])
    if len(raw) < 20 or len(sync) < 20:
        return
    n = min(len(raw), len(sync), 300)
    raw = raw[:n]
    sync = sync[:n]
    srate = res["nominal_srate"]
    dt_ms = 1000.0 / srate

    W, H = 960, 480
    px, pw = 76, 844

    # Top panel: Sample index vs Timestamp (ms)
    y1_top, y1_h = 84, 170
    max_t = max(raw[-1], sync[-1], n * dt_ms, 1.0)
    raw_pts = " ".join(
        f"{px + (i / (n - 1)) * pw:.1f},{y1_top + y1_h - (raw[i] / max_t) * y1_h:.1f}"
        for i in range(n)
    )
    sync_pts = " ".join(
        f"{px + (i / (n - 1)) * pw:.1f},{y1_top + y1_h - (sync[i] / max_t) * y1_h:.1f}"
        for i in range(n)
    )

    # Bottom panel: Timestamp residual vs ideal linear grid (ms)
    y2_top, y2_h = 304, 130
    raw_res = [raw[i] - (raw[0] + i * dt_ms) for i in range(n)]
    sync_res = [sync[i] - (sync[0] + i * dt_ms) for i in range(n)]
    max_abs = max(max(abs(v) for v in raw_res), max(abs(v) for v in sync_res), 5.0)
    y2_mid = y2_top + y2_h / 2.0
    raw_rpts = " ".join(
        f"{px + (i / (n - 1)) * pw:.1f},{y2_mid - (raw_res[i] / max_abs) * (y2_h * 0.45):.1f}"
        for i in range(n)
    )
    sync_rpts = " ".join(
        f"{px + (i / (n - 1)) * pw:.1f},{y2_mid - (sync_res[i] / max_abs) * (y2_h * 0.45):.1f}"
        for i in range(n)
    )

    lines = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}">',
        f'<rect width="{W}" height="{H}" rx="12" fill="#0b0f17"/>',
        f'<text x="24" y="34" fill="#e6edf7" font-family="system-ui,sans-serif" font-size="18" font-weight="700">Live Wireless EEG Timestamps: Raw Burst Staircase vs SyncGroup Dejitter</text>',
        f'<text x="24" y="55" fill="#8ea0b8" font-family="monospace" font-size="12">First {n} samples ({n * dt_ms:.0f} ms @ {srate:.0f} Hz) | Red = raw wireless arrival timestamps, Green = labstream SyncGroup</text>',
        # Top plot box
        f'<rect x="{px}" y="{y1_top}" width="{pw}" height="{y1_h}" rx="6" fill="#131a26" stroke="#1e293b"/>',
        f'<text x="{px + 10}" y="{y1_top + 18}" fill="#cbd5e1" font-family="system-ui,sans-serif" font-size="12" font-weight="600">Cumulative Sample Timestamp (ms) vs Sample Index</text>',
        f'<text x="16" y="{y1_top + 14}" fill="#64748b" font-family="monospace" font-size="10">{max_t:.0f} ms</text>',
        f'<text x="30" y="{y1_top + y1_h}" fill="#64748b" font-family="monospace" font-size="10">0 ms</text>',
        f'<polyline fill="none" stroke="#f87171" stroke-width="2" points="{raw_pts}"/>',
        f'<polyline fill="none" stroke="#34d399" stroke-width="2" points="{sync_pts}"/>',
        # Bottom plot box
        f'<rect x="{px}" y="{y2_top}" width="{pw}" height="{y2_h}" rx="6" fill="#131a26" stroke="#1e293b"/>',
        f'<line x1="{px}" y1="{y2_mid:.1f}" x2="{px + pw}" y2="{y2_mid:.1f}" stroke="#334155" stroke-dasharray="4 4"/>',
        f'<text x="{px + 10}" y="{y2_top + 18}" fill="#cbd5e1" font-family="system-ui,sans-serif" font-size="12" font-weight="600">Residual Deviation from Ideal {dt_ms:.1f} ms Grid (ms)</text>',
        f'<text x="14" y="{y2_top + 16}" fill="#64748b" font-family="monospace" font-size="10">+{max_abs:.0f} ms</text>',
        f'<text x="30" y="{y2_mid + 4:.1f}" fill="#64748b" font-family="monospace" font-size="10">0 ms</text>',
        f'<text x="14" y="{y2_top + y2_h - 4}" fill="#64748b" font-family="monospace" font-size="10">-{max_abs:.0f} ms</text>',
        f'<polyline fill="none" stroke="#f87171" stroke-width="1.8" points="{raw_rpts}"/>',
        f'<polyline fill="none" stroke="#34d399" stroke-width="2" points="{sync_rpts}"/>',
        f'<text x="{px}" y="{H - 16}" fill="#94a3b8" font-family="monospace" font-size="11">Raw sawtooth peak deviation: +/-{max(abs(v) for v in raw_res):.1f} ms | SyncGroup residual: +/-{max(abs(v) for v in sync_res):.4f} ms</text>',
        "</svg>",
    ]
    path.write_text("\n".join(lines) + "\n")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--type", default="EEG", help="LSL stream type to match (default EEG, or ALL)")
    ap.add_argument("--secs", type=float, default=15.0, help="capture duration in seconds (default 15)")
    ap.add_argument("--peer", default="", help="optional LAN IP of the remote streaming PC")
    ap.add_argument("--from-json", default="", help="render SVG dashboard from an existing JSON report")
    args = ap.parse_args()

    RESULTS_DIR.mkdir(parents=True, exist_ok=True)
    if args.from_json:
        report = json.loads(Path(args.from_json).read_text())
        out_svg = RESULTS_DIR / "live-eeg-dashboard.svg"
        write_svg(out_svg, report)
        write_staircase_svg(RESULTS_DIR / "live-eeg-staircase.svg", report)
        print(f"[live-eeg] wrote {out_svg}")
        return

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
    out_svg = RESULTS_DIR / "live-eeg-dashboard.svg"
    write_svg(out_svg, report)
    write_staircase_svg(RESULTS_DIR / "live-eeg-staircase.svg", report)

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
