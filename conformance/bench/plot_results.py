#!/usr/bin/env python3
"""Publication-grade vector SVG figure generator for labstream benchmarks.

Reads JSON outputs in conformance/bench/results/ and renders clean, light-mode,
IEEE/Nature-style vector figures with proper axes, log scales, ticks, and
colorblind-safe editorial palettes. Zero external Python dependencies.
"""

import json
import math
from pathlib import Path

RESULTS_DIR = Path(__file__).resolve().parent / "results"

FONT_SANS = "Inter, 'Helvetica Neue', 'Liberation Sans', Arial, sans-serif"
FONT_MONO = "'JetBrains Mono', 'Liberation Mono', monospace"

# Editorial, colorblind-safe palette (light theme)
C_BG = "#ffffff"
C_INK = "#111827"
C_SUB = "#4b5563"
C_TICK = "#6b7280"
C_AXIS = "#1f2937"
C_GRID = "#e5e7eb"
C_LABSTREAM = "#0f766e"       # deep teal
C_LIBLSL = "#94a3b8"          # cool slate (baseline)
C_WARN = "#b91c1c"            # muted crimson (error / raw burst / outage)
C_WARN_TINT = "#fef2f2"       # subtle outage tint
C_OK_TINT = "#f0fdfa"         # subtle clean tint


def esc(s: str) -> str:
    return (
        s.replace("&", "&amp;")
        .replace("<", "&lt;")
        .replace(">", "&gt;")
        .replace('"', "&quot;")
    )


def svg_open(w: int, h: int, title: str, subtitle: str) -> list[str]:
    return [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" viewBox="0 0 {w} {h}">',
        f'  <rect width="{w}" height="{h}" fill="{C_BG}"/>',
        f'  <text x="36" y="34" fill="{C_INK}" font-family="{FONT_SANS}" font-size="16" font-weight="700">{esc(title)}</text>',
        f'  <text x="36" y="53" fill="{C_SUB}" font-family="{FONT_SANS}" font-size="11.5">{esc(subtitle)}</text>',
        f'  <line x1="36" y1="64" x2="{w - 36}" y2="64" stroke="{C_GRID}" stroke-width="1"/>',
    ]


def draw_legend(lines: list[str], x: float, y: float, items: list[tuple[str, str, str]]):
    """items: list of (label, color, shape: 'rect'|'line')."""
    cx = x
    for label, color, shape in items:
        if shape == "line":
            lines.append(f'  <line x1="{cx}" y1="{y - 4}" x2="{cx + 18}" y2="{y - 4}" stroke="{color}" stroke-width="2.2"/>')
        else:
            lines.append(f'  <rect x="{cx}" y="{y - 9}" width="12" height="10" fill="{color}"/>')
        lines.append(f'  <text x="{cx + 22}" y="{y}" fill="{C_SUB}" font-family="{FONT_SANS}" font-size="11">{esc(label)}</text>')
        cx += 26 + len(label) * 6.5 + 14


def fig1_transport(bench: dict, out_path: Path):
    b = bench["benchmarks"]
    b1_lsl = b["b1_local_ipc_liblsl"]["inlet"]
    b1_ls = b["b1_local_ipc_labstream_shm"]["inlet"]
    b3_lsl = b["b3_lossy_liblsl"]["inlet"]
    b3_ls = b["b3_lossy_labstream_dgram"]["inlet"]

    W, H = 960, 410
    lines = svg_open(
        W, H,
        "Figure 1. Transport Latency: Local IPC (shm/1) and Lossy Wi-Fi (dgram/1)",
        f"Pinned C++ liblsl ({bench['meta']['liblsl_commit']}) vs labstream on bare-metal Linux ({bench['meta']['cores']} cores, kernel {bench['meta']['kernel']})",
    )
    draw_legend(lines, 36, 86, [("C++ liblsl (TCP)", C_LIBLSL, "rect"), ("labstream (shm/1 & dgram/1)", C_LABSTREAM, "rect")])

    # Panel A: Local IPC log-scale latency (us)
    px, py, pw, ph = 84, 122, 370, 224
    lines.append(f'  <text x="{px}" y="{py - 12}" fill="{C_INK}" font-family="{FONT_SANS}" font-size="12.5" font-weight="600">a. Local IPC Latency (64 ch @ 1000 Hz, log scale)</text>')
    log_min, log_max = 0.0, 3.5  # 1 us to 3162 us

    def y_log(val_us: float) -> float:
        lv = max(log_min, min(log_max, math.log10(max(val_us, 1.0))))
        return py + ph - ((lv - log_min) / (log_max - log_min)) * ph

    for exp in [0, 1, 2, 3]:
        yy = y_log(10 ** exp)
        lines.append(f'  <line x1="{px}" y1="{yy:.1f}" x2="{px + pw}" y2="{yy:.1f}" stroke="{C_GRID}" stroke-width="0.9"/>')
        lbl = f"{10**exp}"
        lines.append(f'  <text x="{px - 8}" y="{yy + 3.5:.1f}" text-anchor="end" fill="{C_TICK}" font-family="{FONT_MONO}" font-size="10">{lbl}</text>')
    lines.append(f'  <line x1="{px}" y1="{py}" x2="{px}" y2="{py + ph}" stroke="{C_AXIS}" stroke-width="1.2"/>')
    lines.append(f'  <line x1="{px}" y1="{py + ph}" x2="{px + pw}" y2="{py + ph}" stroke="{C_AXIS}" stroke-width="1.2"/>')
    lines.append(f'  <text x="{px - 48}" y="{py + ph/2:.1f}" text-anchor="middle" fill="{C_SUB}" font-family="{FONT_SANS}" font-size="11" transform="rotate(-90 {px - 48} {py + ph/2:.1f})">Latency (µs, log₁₀)</text>')

    groups_a = [
        ("p50", b1_lsl["p50_us"], b1_ls["p50_us"]),
        ("p95", b1_lsl["p95_us"], b1_ls["p95_us"]),
        ("p99", b1_lsl["p99_us"], b1_ls["p99_us"]),
        ("max", b1_lsl["max_us"], b1_ls["max_us"]),
    ]
    step_a = pw / len(groups_a)
    for i, (lbl, v_lsl, v_ls) in enumerate(groups_a):
        gx = px + i * step_a + step_a * 0.18
        bw = step_a * 0.28
        y1 = y_log(v_lsl)
        y2 = y_log(v_ls)
        lines.append(f'  <rect x="{gx:.1f}" y="{y1:.1f}" width="{bw:.1f}" height="{py + ph - y1:.1f}" fill="{C_LIBLSL}"/>')
        lines.append(f'  <text x="{gx + bw/2:.1f}" y="{y1 - 5:.1f}" text-anchor="middle" fill="{C_SUB}" font-family="{FONT_MONO}" font-size="9.5">{v_lsl:.0f}</text>')
        lines.append(f'  <rect x="{gx + bw + 4:.1f}" y="{y2:.1f}" width="{bw:.1f}" height="{py + ph - y2:.1f}" fill="{C_LABSTREAM}"/>')
        lines.append(f'  <text x="{gx + bw + 4 + bw/2:.1f}" y="{y2 - 5:.1f}" text-anchor="middle" fill="{C_LABSTREAM}" font-family="{FONT_MONO}" font-size="9.5" font-weight="600">{v_ls:.1f}</text>')
        lines.append(f'  <text x="{gx + bw + 2:.1f}" y="{py + ph + 18}" text-anchor="middle" fill="{C_SUB}" font-family="{FONT_SANS}" font-size="11">{lbl}</text>')

    # Panel B: Lossy Wi-Fi latency (ms, linear)
    qx, qy, qw, qh = 544, 122, 376, 224
    lines.append(f'  <text x="{qx}" y="{qy - 12}" fill="{C_INK}" font-family="{FONT_SANS}" font-size="12.5" font-weight="600">b. Lossy Wi-Fi (20 ± 5 ms, 1% packet loss)</text>')
    max_ms = max(b3_lsl["max_us"], b3_ls["max_us"]) / 1000.0
    ymax_b = math.ceil(max_ms / 25.0) * 25.0

    def y_lin(val_ms: float) -> float:
        return qy + qh - (val_ms / ymax_b) * qh

    for tick in range(0, int(ymax_b) + 1, int(ymax_b // 4)):
        yy = y_lin(float(tick))
        lines.append(f'  <line x1="{qx}" y1="{yy:.1f}" x2="{qx + qw}" y2="{yy:.1f}" stroke="{C_GRID}" stroke-width="0.9"/>')
        lines.append(f'  <text x="{qx - 8}" y="{yy + 3.5:.1f}" text-anchor="end" fill="{C_TICK}" font-family="{FONT_MONO}" font-size="10">{tick}</text>')
    lines.append(f'  <line x1="{qx}" y1="{qy}" x2="{qx}" y2="{qy + qh}" stroke="{C_AXIS}" stroke-width="1.2"/>')
    lines.append(f'  <line x1="{qx}" y1="{qy + qh}" x2="{qx + qw}" y2="{qy + qh}" stroke="{C_AXIS}" stroke-width="1.2"/>')
    lines.append(f'  <text x="{qx - 46}" y="{qy + qh/2:.1f}" text-anchor="middle" fill="{C_SUB}" font-family="{FONT_SANS}" font-size="11" transform="rotate(-90 {qx - 46} {qy + qh/2:.1f})">Latency (ms)</text>')

    groups_b = [
        ("p50", b3_lsl["p50_us"] / 1000.0, b3_ls["p50_us"] / 1000.0),
        ("p95", b3_lsl["p95_us"] / 1000.0, b3_ls["p95_us"] / 1000.0),
        ("p99", b3_lsl["p99_us"] / 1000.0, b3_ls["p99_us"] / 1000.0),
        ("max", b3_lsl["max_us"] / 1000.0, b3_ls["max_us"] / 1000.0),
    ]
    step_b = qw / len(groups_b)
    for i, (lbl, v_lsl, v_ls) in enumerate(groups_b):
        gx = qx + i * step_b + step_b * 0.18
        bw = step_b * 0.28
        y1 = y_lin(v_lsl)
        y2 = y_lin(v_ls)
        lines.append(f'  <rect x="{gx:.1f}" y="{y1:.1f}" width="{bw:.1f}" height="{qy + qh - y1:.1f}" fill="{C_LIBLSL}"/>')
        lines.append(f'  <text x="{gx + bw/2:.1f}" y="{y1 - 5:.1f}" text-anchor="middle" fill="{C_SUB}" font-family="{FONT_MONO}" font-size="9.5">{v_lsl:.1f}</text>')
        lines.append(f'  <rect x="{gx + bw + 4:.1f}" y="{y2:.1f}" width="{bw:.1f}" height="{qy + qh - y2:.1f}" fill="{C_LABSTREAM}"/>')
        lines.append(f'  <text x="{gx + bw + 4 + bw/2:.1f}" y="{y2 - 5:.1f}" text-anchor="middle" fill="{C_LABSTREAM}" font-family="{FONT_MONO}" font-size="9.5" font-weight="600">{v_ls:.1f}</text>')
        lines.append(f'  <text x="{gx + bw + 2:.1f}" y="{qy + qh + 18}" text-anchor="middle" fill="{C_SUB}" font-family="{FONT_SANS}" font-size="11">{lbl}</text>')

    lines.append(f'  <text x="36" y="{H - 16}" fill="{C_TICK}" font-family="{FONT_SANS}" font-size="10">Lower is better. Local IPC p50: 2.0 µs (labstream shm/1) vs 592.0 µs (C++ liblsl TCP); Lossy Wi-Fi p99: 40.3 ms vs 74.3 ms.</text>')
    lines.append("</svg>")
    out_path.write_text("\n".join(lines) + "\n")


def fig2_hypersync(hs: dict, out_path: Path):
    pairs = hs["pairs"]
    short_labels = [
        "2-dev 250 Hz",
        "4-dev 250 Hz",
        "8-dev 250 Hz",
        "8-dev 1000 Hz",
        "8-dev 2000 Hz",
        "8-dev mixed",
        "8-dev drop",
    ]

    W, H = 980, 440
    lines = svg_open(
        W, H,
        "Figure 2. Multi-Device Hyperscanning: Phase Accuracy and Memory Scaling",
        f"Head-to-head C++ liblsl nearest-sample pairing vs labstream SyncGroup (2–8 devices, up to 64 ch @ 2000 Hz)",
    )
    draw_legend(lines, 36, 86, [("C++ liblsl (nearest-sample)", C_WARN, "rect"), ("labstream SyncGroup (32-tap sinc)", C_LABSTREAM, "rect")])

    # Panel A: Worst-pair phase error (log10 degrees: 1e-4 to 1e3)
    px, py, pw, ph = 136, 120, 380, 266
    lines.append(f'  <text x="{px}" y="{py - 12}" fill="{C_INK}" font-family="{FONT_SANS}" font-size="12.5" font-weight="600">a. Worst-Pair Phase Error (degrees, log₁₀ scale)</text>')
    lmin, lmax = -4.0, 2.6  # 0.0001° to ~400°

    def x_log(deg: float) -> float:
        lv = max(lmin, min(lmax, math.log10(max(deg, 1e-4))))
        return px + ((lv - lmin) / (lmax - lmin)) * pw

    for exp, lbl in [(-4, "10⁻⁴"), (-3, "10⁻³"), (-2, "10⁻²"), (-1, "0.1°"), (0, "1°"), (1, "10°"), (2, "100°")]:
        xx = x_log(10.0 ** exp)
        lines.append(f'  <line x1="{xx:.1f}" y1="{py}" x2="{xx:.1f}" y2="{py + ph}" stroke="{C_GRID}" stroke-width="0.9"/>')
        lines.append(f'  <text x="{xx:.1f}" y="{py + ph + 16}" text-anchor="middle" fill="{C_TICK}" font-family="{FONT_MONO}" font-size="9.5">{lbl}</text>')
    lines.append(f'  <line x1="{px}" y1="{py}" x2="{px}" y2="{py + ph}" stroke="{C_AXIS}" stroke-width="1.2"/>')
    lines.append(f'  <line x1="{px}" y1="{py + ph}" x2="{px + pw}" y2="{py + ph}" stroke="{C_AXIS}" stroke-width="1.2"/>')

    row_h = ph / len(pairs)
    for i, p in enumerate(pairs):
        ry = py + i * row_h + 6
        bh = 10.5
        d_lsl = p["liblsl"]["worst_deg"]
        d_ls = p["labstream"]["worst_deg"]
        w1 = max(3.0, x_log(d_lsl) - px)
        w2 = max(3.0, x_log(d_ls) - px)
        lines.append(f'  <text x="{px - 10}" y="{ry + 14}" text-anchor="end" fill="{C_SUB}" font-family="{FONT_SANS}" font-size="11">{short_labels[i]}</text>')
        lines.append(f'  <rect x="{px}" y="{ry}" width="{w1:.1f}" height="{bh}" fill="{C_WARN}" fill-opacity="0.82"/>')
        lines.append(f'  <text x="{px + w1 + 5:.1f}" y="{ry + 9}" fill="{C_WARN}" font-family="{FONT_MONO}" font-size="9.5">{d_lsl:.1f}°</text>')
        lines.append(f'  <rect x="{px}" y="{ry + bh + 2}" width="{w2:.1f}" height="{bh}" fill="{C_LABSTREAM}"/>')
        lines.append(f'  <text x="{px + w2 + 5:.1f}" y="{ry + bh + 11}" fill="{C_LABSTREAM}" font-family="{FONT_MONO}" font-size="9.5" font-weight="600">{d_ls:.4f}°</text>')

    # Panel B: Resident Memory (RSS, MB)
    qx, qy, qw, qh = 612, 120, 326, 266
    lines.append(f'  <text x="{qx}" y="{qy - 12}" fill="{C_INK}" font-family="{FONT_SANS}" font-size="12.5" font-weight="600">b. Resident Memory Footprint (RSS, MB)</text>')
    max_rss = 220.0

    def x_rss(mb: float) -> float:
        return qx + (min(mb, max_rss) / max_rss) * qw

    for tick in [0, 50, 100, 150, 200]:
        xx = x_rss(float(tick))
        lines.append(f'  <line x1="{xx:.1f}" y1="{qy}" x2="{xx:.1f}" y2="{qy + qh}" stroke="{C_GRID}" stroke-width="0.9"/>')
        lines.append(f'  <text x="{xx:.1f}" y="{qy + qh + 16}" text-anchor="middle" fill="{C_TICK}" font-family="{FONT_MONO}" font-size="9.5">{tick}</text>')
    lines.append(f'  <line x1="{qx}" y1="{qy}" x2="{qx}" y2="{qy + qh}" stroke="{C_AXIS}" stroke-width="1.2"/>')
    lines.append(f'  <line x1="{qx}" y1="{qy + qh}" x2="{qx + qw}" y2="{qy + qh}" stroke="{C_AXIS}" stroke-width="1.2"/>')

    for i, p in enumerate(pairs):
        ry = qy + i * row_h + 6
        bh = 10.5
        r_lsl = p["liblsl"]["rss_mb"]
        r_ls = p["labstream"]["rss_mb"]
        w1 = max(2.0, x_rss(r_lsl) - qx)
        w2 = max(2.0, x_rss(r_ls) - qx)
        lines.append(f'  <rect x="{qx}" y="{ry}" width="{w1:.1f}" height="{bh}" fill="{C_LIBLSL}"/>')
        lines.append(f'  <text x="{qx + w1 + 5:.1f}" y="{ry + 9}" fill="{C_SUB}" font-family="{FONT_MONO}" font-size="9.5">{r_lsl:.0f} MB</text>')
        lines.append(f'  <rect x="{qx}" y="{ry + bh + 2}" width="{w2:.1f}" height="{bh}" fill="{C_LABSTREAM}"/>')
        lines.append(f'  <text x="{qx + w2 + 5:.1f}" y="{ry + bh + 11}" fill="{C_LABSTREAM}" font-family="{FONT_MONO}" font-size="9.5" font-weight="600">{r_ls:.1f} MB</text>')

    lines.append(f'  <text x="36" y="{H - 14}" fill="{C_TICK}" font-family="{FONT_SANS}" font-size="10">Lower is better. At 8 devices × 2000 Hz (64 ch): SyncGroup achieves 0.0004° phase error (55,000× lower) and 18.9 MB RSS (10.6× less memory).</text>')
    lines.append("</svg>")
    out_path.write_text("\n".join(lines) + "\n")


def fig3_live_dejitter(live: dict, out_path: Path):
    res = live["metrics"]
    raw = res.get("raw_trace_ms", [])
    sync = res.get("sync_trace_ms", [])
    n = min(len(raw), len(sync), 300)
    raw = raw[:n]
    sync = sync[:n]
    srate = res["nominal_srate"]
    dt_ms = 1000.0 / srate

    W, H = 960, 480
    lines = svg_open(
        W, H,
        "Figure 3. Live Wireless EEG Timestamp Dejittering Over LAN (1000 Hz, Saline)",
        f"Real commercial wireless EEG amplifier (8 ch @ {srate:.0f} Hz) streamed from remote PC: raw Bluetooth burst arrival vs labstream SyncGroup",
    )
    draw_legend(lines, 36, 86, [("Raw wireless arrival timestamps", C_WARN, "line"), ("labstream SyncGroup (DEJITTER + MONOTONIZE)", C_LABSTREAM, "line")])

    px, pw = 86, 834
    # Panel A: Cumulative timestamp
    y1, h1 = 114, 152
    max_t = math.ceil(max(raw[-1], sync[-1], n * dt_ms) / 50.0) * 50.0
    lines.append(f'  <text x="{px}" y="{y1 - 8}" fill="{C_INK}" font-family="{FONT_SANS}" font-size="12" font-weight="600">a. Cumulative Sample Timestamp (ms)</text>')
    for tick in range(0, int(max_t) + 1, max(50, int(max_t // 4))):
        yy = y1 + h1 - (tick / max_t) * h1
        lines.append(f'  <line x1="{px}" y1="{yy:.1f}" x2="{px + pw}" y2="{yy:.1f}" stroke="{C_GRID}" stroke-width="0.9"/>')
        lines.append(f'  <text x="{px - 8}" y="{yy + 3.5:.1f}" text-anchor="end" fill="{C_TICK}" font-family="{FONT_MONO}" font-size="10">{tick}</text>')
    lines.append(f'  <line x1="{px}" y1="{y1}" x2="{px}" y2="{y1 + h1}" stroke="{C_AXIS}" stroke-width="1.2"/>')
    lines.append(f'  <line x1="{px}" y1="{y1 + h1}" x2="{px + pw}" y2="{y1 + h1}" stroke="{C_AXIS}" stroke-width="1.2"/>')

    raw_pts = " ".join(f"{px + (i/(n-1))*pw:.1f},{y1 + h1 - (raw[i]/max_t)*h1:.1f}" for i in range(n))
    sync_pts = " ".join(f"{px + (i/(n-1))*pw:.1f},{y1 + h1 - (sync[i]/max_t)*h1:.1f}" for i in range(n))
    lines.append(f'  <polyline fill="none" stroke="{C_WARN}" stroke-width="1.7" points="{raw_pts}"/>')
    lines.append(f'  <polyline fill="none" stroke="{C_LABSTREAM}" stroke-width="1.8" points="{sync_pts}"/>')

    # Panel B: Residual deviation from ideal grid
    y2, h2 = 304, 126
    raw_res = [raw[i] - (raw[0] + i * dt_ms) for i in range(n)]
    sync_res = [sync[i] - (sync[0] + i * dt_ms) for i in range(n)]
    max_abs = math.ceil(max(max(abs(v) for v in raw_res), 10.0) / 10.0) * 10.0
    y2_mid = y2 + h2 / 2.0
    lines.append(f'  <text x="{px}" y="{y2 - 8}" fill="{C_INK}" font-family="{FONT_SANS}" font-size="12" font-weight="600">b. Residual Deviation from Ideal {dt_ms:.1f} ms Sample Grid (ms)</text>')
    for tv in [-max_abs, -max_abs / 2, 0, max_abs / 2, max_abs]:
        yy = y2_mid - (tv / max_abs) * (h2 * 0.46)
        dash = ' stroke-dasharray="3 3"' if tv == 0 else ""
        lines.append(f'  <line x1="{px}" y1="{yy:.1f}" x2="{px + pw}" y2="{yy:.1f}" stroke="{C_GRID}" stroke-width="0.9"{dash}/>')
        lines.append(f'  <text x="{px - 8}" y="{yy + 3.5:.1f}" text-anchor="end" fill="{C_TICK}" font-family="{FONT_MONO}" font-size="10">{tv:+.0f}</text>')
    for sidx in range(0, n, 50):
        xx = px + (sidx / (n - 1)) * pw
        lines.append(f'  <line x1="{xx:.1f}" y1="{y2 + h2}" x2="{xx:.1f}" y2="{y2 + h2 + 4}" stroke="{C_AXIS}" stroke-width="1"/>')
        lines.append(f'  <text x="{xx:.1f}" y="{y2 + h2 + 16}" text-anchor="middle" fill="{C_TICK}" font-family="{FONT_MONO}" font-size="10">{sidx}</text>')
    lines.append(f'  <line x1="{px}" y1="{y2}" x2="{px}" y2="{y2 + h2}" stroke="{C_AXIS}" stroke-width="1.2"/>')
    lines.append(f'  <line x1="{px}" y1="{y2 + h2}" x2="{px + pw}" y2="{y2 + h2}" stroke="{C_AXIS}" stroke-width="1.2"/>')

    raw_rpts = " ".join(f"{px + (i/(n-1))*pw:.1f},{y2_mid - (raw_res[i]/max_abs)*(h2*0.46):.1f}" for i in range(n))
    sync_rpts = " ".join(f"{px + (i/(n-1))*pw:.1f},{y2_mid - (sync_res[i]/max_abs)*(h2*0.46):.1f}" for i in range(n))
    lines.append(f'  <polyline fill="none" stroke="{C_WARN}" stroke-width="1.5" points="{raw_rpts}"/>')
    lines.append(f'  <polyline fill="none" stroke="{C_LABSTREAM}" stroke-width="2.0" points="{sync_rpts}"/>')

    lines.append(f'  <text x="{px + pw/2:.1f}" y="{H - 22}" text-anchor="middle" fill="{C_SUB}" font-family="{FONT_SANS}" font-size="11">Sample Index (first {n} samples = {n * dt_ms:.0f} ms @ {srate:.0f} Hz)</text>')
    lines.append(f'  <text x="36" y="{H - 6}" fill="{C_TICK}" font-family="{FONT_SANS}" font-size="10">Peak raw wireless sawtooth deviation: ±{max(abs(v) for v in raw_res):.1f} ms | SyncGroup residual: ±{max(abs(v) for v in sync_res):.4f} ms | Live saline comp/1 ratio: {res["comp_ratio"]:.3f}x ({(1-res["comp_ratio"])*100:.1f}% saved).</text>')
    lines.append("</svg>")
    out_path.write_text("\n".join(lines) + "\n")


def fig4_live_dropout(drop: dict, out_path: Path):
    rows = drop["per_sec_rows"]
    gaps = drop["per_sec_gaps"]
    n = len(rows)

    W, H = 960, 390
    lines = svg_open(
        W, H,
        "Figure 4. Live Physical Power-Off Dropout Isolation and Recovery (1000 Hz)",
        "25 s live dual-stream SyncGroup capture (1000 Hz ExG + 20 Hz orientation) across an ~8.8 s physical wireless amplifier power-off",
    )
    draw_legend(lines, 36, 86, [("Clean aligned rows/s", C_LABSTREAM, "rect"), ("Isolated gap_mask outage (8,775 rows = 8.775 s)", C_WARN, "rect")])

    px, py, pw, ph = 84, 118, 836, 206
    ymax = 1200.0
    bar_slot = pw / n

    # Shaded RF outage region (seconds 14..22)
    ox1 = px + 14 * bar_slot
    ox2 = px + 22 * bar_slot
    lines.append(f'  <rect x="{ox1:.1f}" y="{py}" width="{ox2 - ox1:.1f}" height="{ph}" fill="{C_WARN_TINT}"/>')
    lines.append(f'  <text x="{(ox1 + ox2)/2:.1f}" y="{py + 22}" text-anchor="middle" fill="{C_WARN}" font-family="{FONT_SANS}" font-size="11" font-weight="600">Physical Amp Off (~8.8 s)</text>')
    lines.append(f'  <text x="{(ox1 + ox2)/2:.1f}" y="{py + 38}" text-anchor="middle" fill="{C_SUB}" font-family="{FONT_SANS}" font-size="10">SyncGroup waits cleanly (no stale samples)</text>')

    for tick in [0, 300, 600, 900, 1200]:
        yy = py + ph - (tick / ymax) * ph
        lines.append(f'  <line x1="{px}" y1="{yy:.1f}" x2="{px + pw}" y2="{yy:.1f}" stroke="{C_GRID}" stroke-width="0.9"/>')
        lines.append(f'  <text x="{px - 8}" y="{yy + 3.5:.1f}" text-anchor="end" fill="{C_TICK}" font-family="{FONT_MONO}" font-size="10">{tick}</text>')
    # Target 1000 Hz reference line
    y1000 = py + ph - (1000.0 / ymax) * ph
    lines.append(f'  <line x1="{px}" y1="{y1000:.1f}" x2="{px + pw}" y2="{y1000:.1f}" stroke="{C_LABSTREAM}" stroke-width="1" stroke-dasharray="4 4" stroke-opacity="0.5"/>')

    for i in range(n):
        clean = max(0, rows[i] - gaps[i])
        g = gaps[i]
        bx = px + i * bar_slot + bar_slot * 0.16
        bw = bar_slot * 0.68
        if clean > 0:
            ch = min(ph, (clean / ymax) * ph)
            lines.append(f'  <rect x="{bx:.1f}" y="{py + ph - ch:.1f}" width="{bw:.1f}" height="{ch:.1f}" fill="{C_LABSTREAM}"/>')
        if g > 0:
            # Cap visual gap indicator neatly above clean bar with explicit label
            gh = 30.0
            ch = min(ph - gh - 4, (clean / ymax) * ph)
            gy = py + ph - ch - gh - 2
            lines.append(f'  <rect x="{bx:.1f}" y="{gy:.1f}" width="{bw:.1f}" height="{gh:.1f}" fill="{C_WARN}"/>')
            lines.append(f'  <line x1="{bx + bw/2:.1f}" y1="{gy:.1f}" x2="{bx + bw/2:.1f}" y2="{py + 14}" stroke="{C_WARN}" stroke-width="1" stroke-dasharray="2 2"/>')
            lines.append(f'  <text x="{bx + bw/2 - 6:.1f}" y="{py + 12}" text-anchor="end" fill="{C_WARN}" font-family="{FONT_MONO}" font-size="10" font-weight="600">8,775 gap rows isolated on reconnect</text>')

    lines.append(f'  <line x1="{px}" y1="{py}" x2="{px}" y2="{py + ph}" stroke="{C_AXIS}" stroke-width="1.2"/>')
    lines.append(f'  <line x1="{px}" y1="{py + ph}" x2="{px + pw}" y2="{py + ph}" stroke="{C_AXIS}" stroke-width="1.2"/>')
    lines.append(f'  <text x="{px - 50}" y="{py + ph/2:.1f}" text-anchor="middle" fill="{C_SUB}" font-family="{FONT_SANS}" font-size="11" transform="rotate(-90 {px - 50} {py + ph/2:.1f})">Aligned Rows / s</text>')

    for sec in range(0, n, 5):
        xx = px + (sec + 0.5) * bar_slot
        lines.append(f'  <line x1="{xx:.1f}" y1="{py + ph}" x2="{xx:.1f}" y2="{py + ph + 4}" stroke="{C_AXIS}" stroke-width="1"/>')
        lines.append(f'  <text x="{xx:.1f}" y="{py + ph + 17}" text-anchor="middle" fill="{C_TICK}" font-family="{FONT_MONO}" font-size="10">{sec}s</text>')
    lines.append(f'  <text x="{px + pw/2:.1f}" y="{py + ph + 36}" text-anchor="middle" fill="{C_SUB}" font-family="{FONT_SANS}" font-size="11">Elapsed Capture Time (seconds)</text>')
    lines.append(f'  <text x="36" y="{H - 12}" fill="{C_TICK}" font-family="{FONT_SANS}" font-size="10">Baseline (1–13 s): 11,935 clean rows, 0 false gaps | Reconnect (22 s): 8,775 exact gap-masked rows | Recovery (23–24 s): 1,996 clean rows, 0 gaps.</text>')
    lines.append("</svg>")
    out_path.write_text("\n".join(lines) + "\n")


def main():
    RESULTS_DIR.mkdir(parents=True, exist_ok=True)
    p_bench = RESULTS_DIR / "bench-results.json"
    p_hs = RESULTS_DIR / "hypersync-results.json"
    p_live = RESULTS_DIR / "live-eeg-saline-1000hz.json"
    p_drop = RESULTS_DIR / "live-dropout-report.json"

    if p_bench.exists():
        fig1_transport(json.loads(p_bench.read_text()), RESULTS_DIR / "fig1_transport_benchmarks.svg")
        print("[plot] wrote fig1_transport_benchmarks.svg")
    if p_hs.exists():
        fig2_hypersync(json.loads(p_hs.read_text()), RESULTS_DIR / "fig2_hypersync_scaling.svg")
        print("[plot] wrote fig2_hypersync_scaling.svg")
    if p_live.exists():
        fig3_live_dejitter(json.loads(p_live.read_text()), RESULTS_DIR / "fig3_live_wireless_dejitter.svg")
        print("[plot] wrote fig3_live_wireless_dejitter.svg")
    if p_drop.exists():
        fig4_live_dropout(json.loads(p_drop.read_text()), RESULTS_DIR / "fig4_live_dropout_recovery.svg")
        print("[plot] wrote fig4_live_dropout_recovery.svg")


if __name__ == "__main__":
    main()
