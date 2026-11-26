#!/usr/bin/env python3
"""Reproducible bare-metal head-to-head benchmark suite: C++ liblsl vs labstream.

Runs with zero Docker overhead using native Linux `unshare -Urn` + `veth` + `tc netem`
and `taskset` CPU core pinning. Outputs:
  - conformance/bench/results/bench-results.json
  - conformance/bench/results/bench-summary.md
  - conformance/bench/results/bench-latency.svg
  - conformance/bench/results/bench-cpu.svg
"""

import argparse
import json
import os
import platform
import shutil
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
WORKER_SRC = BENCH_DIR / "worker.c"
WORKER_LIBLSL = BUILD_DIR / "worker_liblsl"
WORKER_LABSTREAM = BUILD_DIR / "worker_labstream"


def system_info() -> dict:
    cpu = "unknown"
    try:
        for line in Path("/proc/cpuinfo").read_text().splitlines():
            if line.startswith("model name"):
                cpu = line.split(":", 1)[1].strip()
                break
    except OSError:
        pass
    gov = "unknown"
    p_gov = Path("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor")
    if p_gov.exists():
        gov = p_gov.read_text().strip()
    git_sha = subprocess.check_output(
        ["git", "-C", str(ROOT), "rev-parse", "--short", "HEAD"], text=True
    ).strip()
    pins = (ROOT / "conformance" / "oracle" / "PINS").read_text()
    liblsl_rev = next(
        (l.split("=", 1)[1].strip()[:8] for l in pins.splitlines() if l.startswith("LIBLSL_COMMIT=")),
        "unknown",
    )
    return {
        "cpu": cpu,
        "cores": os.cpu_count(),
        "governor": gov,
        "kernel": platform.release(),
        "git_sha": git_sha,
        "liblsl_commit": liblsl_rev,
        "timestamp_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
    }


def build_workers():
    BUILD_DIR.mkdir(parents=True, exist_ok=True)
    for out_bin, lib_dir in [
        (WORKER_LIBLSL, ORACLE_LIB_DIR),
        (WORKER_LABSTREAM, LABSTREAM_LIB_DIR),
    ]:
        if not (lib_dir / "liblsl.so").exists():
            sys.exit(f"missing {lib_dir / 'liblsl.so'} — build release artifacts first")
        cmd = [
            "gcc", "-O2", "-Wall", "-Wextra",
            str(WORKER_SRC),
            f"-I{ORACLE_INC}",
            f"-L{lib_dir}",
            f"-Wl,-rpath,{lib_dir}",
            "-llsl", "-lm", "-ldl",
            "-o", str(out_bin),
        ]
        subprocess.run(cmd, check=True)


def write_cfg(text: str) -> str:
    fd, path = tempfile.mkstemp(prefix="lsl_bench_", suffix=".cfg")
    with os.fdopen(fd, "w") as f:
        f.write(text)
    return path


def pin_cmd(core: int, args: list[str]) -> list[str]:
    if shutil.which("taskset") and (os.cpu_count() or 1) >= 2:
        return ["taskset", "-c", str(core)] + args
    return args


def run_local_pair(
    worker: Path,
    name: str,
    ch: int,
    srate: float,
    chunk: int,
    secs: float,
    cfg_text: str,
) -> dict:
    cfg = write_cfg(cfg_text)
    env = os.environ.copy()
    env.pop("LD_LIBRARY_PATH", None)
    env["LSLAPICFG"] = cfg
    try:
        p_out = subprocess.Popen(
            pin_cmd(0, [str(worker), "outlet", name, str(ch), str(srate), str(chunk), str(secs + 0.5)]),
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=env,
        )
        time.sleep(0.25)
        p_in = subprocess.Popen(
            pin_cmd(1, [str(worker), "inlet", name, str(ch), str(secs), "0.8"]),
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=env,
        )
        in_out, in_err = p_in.communicate(timeout=secs + 20)
        out_out, out_err = p_out.communicate(timeout=20)
        if p_in.returncode != 0 or p_out.returncode != 0:
            raise RuntimeError(f"bench failed: out={out_err} in={in_err}")
        return {
            "outlet": json.loads(out_out.strip().splitlines()[-1]),
            "inlet": json.loads(in_out.strip().splitlines()[-1]),
        }
    finally:
        os.unlink(cfg)


def run_netns_pair(
    worker: Path,
    name: str,
    ch: int,
    srate: float,
    chunk: int,
    secs: float,
    cfg_extra: str,
    netem_rule: str = "",
    cut_sec: float = 0.0,
) -> dict:
    """Run outlet and inlet in two separate Linux network namespaces over a veth pair."""
    cfg_out = write_cfg(cfg_extra)
    cfg_in = write_cfg("[lab]\nKnownPeers = {10.88.0.1}\n\n" + cfg_extra)
    result_file = tempfile.mktemp(prefix="lsl_netns_res_", suffix=".json")

    helper = f"""
import json, os, subprocess, sys, time

def sh(c): subprocess.run(c, shell=True, check=True)

sh("ip link set lo up")
holder = subprocess.Popen(["unshare", "-n", "sleep", "infinity"])
time.sleep(0.25)
pid = str(holder.pid)
try:
    sh("ip link add va type veth peer name vb")
    sh(f"ip link set vb netns {{pid}}")
    sh("ip addr add 10.88.0.1/24 dev va && ip link set va up")
    sh(f"nsenter -t {{pid}} -n ip link set lo up")
    sh(f"nsenter -t {{pid}} -n ip addr add 10.88.0.2/24 dev vb")
    sh(f"nsenter -t {{pid}} -n ip link set vb up")

    netem = {netem_rule!r}
    if netem:
        sh(f"tc qdisc replace dev va root netem {{netem}}")
        sh(f"nsenter -t {{pid}} -n tc qdisc replace dev vb root netem {{netem}}")

    env_out = os.environ.copy()
    env_out.pop("LD_LIBRARY_PATH", None)
    env_out["LSLAPICFG"] = {cfg_out!r}

    env_in = os.environ.copy()
    env_in.pop("LD_LIBRARY_PATH", None)
    env_in["LSLAPICFG"] = {cfg_in!r}

    p_out = subprocess.Popen(
        ["taskset", "-c", "0", {str(worker)!r}, "outlet", {name!r}, {str(ch)!r}, {str(srate)!r}, {str(chunk)!r}, {str(secs + 1.0)!r}],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=env_out,
    )
    time.sleep(0.35)
    p_in = subprocess.Popen(
        ["nsenter", "-t", pid, "-n", "taskset", "-c", "1", {str(worker)!r}, "inlet", {name!r}, {str(ch)!r}, {str(secs)!r}, "0.8"],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=env_in,
    )

    cut_sec = {cut_sec!r}
    if cut_sec > 0.0:
        time.sleep(2.5)
        sh("tc qdisc replace dev va root netem loss 100%")
        sh(f"nsenter -t {{pid}} -n tc qdisc replace dev vb root netem loss 100%")
        time.sleep(cut_sec)
        sh("tc qdisc del dev va root 2>/dev/null || true")
        sh(f"nsenter -t {{pid}} -n tc qdisc del dev vb root 2>/dev/null || true")

    in_out, in_err = p_in.communicate(timeout={secs + 25})
    out_out, out_err = p_out.communicate(timeout=25)
    if p_in.returncode != 0 or p_out.returncode != 0:
        sys.stderr.write(f"netns bench failed: out={{out_err}} in={{in_err}}\\n")
        sys.exit(1)

    res = {{
        "outlet": json.loads(out_out.strip().splitlines()[-1]),
        "inlet": json.loads(in_out.strip().splitlines()[-1]),
    }}
    with open({result_file!r}, "w") as f:
        json.dump(res, f)
finally:
    holder.kill()
    holder.wait()
"""
    try:
        subprocess.run(
            ["unshare", "-Urn", "python3", "-c", helper],
            check=True,
        )
        return json.loads(Path(result_file).read_text())
    finally:
        for p in (cfg_out, cfg_in, result_file):
            if os.path.exists(p):
                os.unlink(p)


def write_bar_svg(path: Path, title: str, subtitle: str, rows: list[tuple[str, float, str, str]], unit: str):
    W = 920
    row_h = 54
    H = 96 + len(rows) * row_h + 24
    max_v = max([r[1] for r in rows] + [1.0])
    bar_x = 340
    bar_w = 450

    lines = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}">',
        f'<rect width="{W}" height="{H}" rx="12" fill="#0b0f17"/>',
        f'<text x="24" y="36" fill="#e6edf7" font-family="system-ui,sans-serif" font-size="19" font-weight="700">{title}</text>',
        f'<text x="24" y="58" fill="#8ea0b8" font-family="monospace" font-size="12">{subtitle}</text>',
    ]
    for i, (label, val, color, note) in enumerate(rows):
        y = 88 + i * row_h
        w = max(6.0, (val / max_v) * bar_w)
        lines.append(f'<text x="24" y="{y + 20}" fill="#cbd5e1" font-family="system-ui,sans-serif" font-size="13" font-weight="600">{label}</text>')
        lines.append(f'<rect x="{bar_x}" y="{y + 4}" width="{bar_w}" height="24" rx="5" fill="#131a26"/>')
        lines.append(f'<rect x="{bar_x}" y="{y + 4}" width="{w:.1f}" height="24" rx="5" fill="{color}"/>')
        lines.append(f'<text x="{bar_x + bar_w + 12}" y="{y + 21}" fill="#e6edf7" font-family="monospace" font-size="13" font-weight="700">{val:.1f} {unit}</text>')
        if note:
            lines.append(f'<text x="24" y="{y + 38}" fill="#64748b" font-family="monospace" font-size="11">{note}</text>')
    lines.append("</svg>")
    path.write_text("\n".join(lines) + "\n")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--secs", type=float, default=6.0, help="seconds per benchmark run (default 6)")
    args = ap.parse_args()
    secs = args.secs

    RESULTS_DIR.mkdir(parents=True, exist_ok=True)
    meta = system_info()
    print(f"[bench] building C workers (-O2) on {meta['cpu']} ({meta['cores']} cores, gov={meta['governor']})...")
    build_workers()

    results = {"meta": meta, "benchmarks": {}}

    print("[bench] B1: Local IPC latency (64 ch @ 1000 Hz, chunk 1) — liblsl TCP vs labstream shm/1")
    results["benchmarks"]["b1_local_ipc_liblsl"] = run_local_pair(
        WORKER_LIBLSL, "B1_LSL", 64, 1000.0, 1, secs, ""
    )
    results["benchmarks"]["b1_local_ipc_labstream_shm"] = run_local_pair(
        WORKER_LABSTREAM, "B1_SHM", 64, 1000.0, 1, secs,
        "[labstream]\nFeatures = {seq, resume, shm}\n"
    )

    print("[bench] B2: High-throughput local IPC (64 ch @ 4000 Hz, chunk 32) — CPU & RSS")
    results["benchmarks"]["b2_highrate_liblsl"] = run_local_pair(
        WORKER_LIBLSL, "B2_LSL", 64, 4000.0, 32, secs, ""
    )
    results["benchmarks"]["b2_highrate_labstream_shm"] = run_local_pair(
        WORKER_LABSTREAM, "B2_SHM", 64, 4000.0, 32, secs,
        "[labstream]\nFeatures = {seq, resume, shm}\n"
    )

    print("[bench] B3: Lossy Wi-Fi (8 ch @ 512 Hz, chunk 16, tc netem 20ms±5ms 1% loss)")
    results["benchmarks"]["b3_lossy_liblsl"] = run_netns_pair(
        WORKER_LIBLSL, "B3_LSL", 8, 512.0, 16, secs + 2.0, "",
        netem_rule="delay 20ms 5ms loss 1%",
    )
    results["benchmarks"]["b3_lossy_labstream_dgram"] = run_netns_pair(
        WORKER_LABSTREAM, "B3_DGR", 8, 512.0, 16, secs + 2.0,
        "[labstream]\nFeatures = {seq, resume, dgram}\n",
        netem_rule="delay 20ms 5ms loss 1%",
    )

    print("[bench] B4: Wire compression (comp/1) & encryption (enc/1) on 32 ch @ 500 Hz, chunk 16")
    results["benchmarks"]["b4_plain_tcp"] = run_local_pair(
        WORKER_LABSTREAM, "B4_PLN", 32, 500.0, 16, secs,
        "[labstream]\nFeatures = {seq, resume}\n"
    )
    results["benchmarks"]["b4_comp_tcp"] = run_local_pair(
        WORKER_LABSTREAM, "B4_CMP", 32, 500.0, 16, secs,
        "[labstream]\nFeatures = {seq, resume, comp}\n"
    )
    results["benchmarks"]["b4_enc_tcp"] = run_local_pair(
        WORKER_LABSTREAM, "B4_ENC", 32, 500.0, 16, secs,
        "[labstream]\nFeatures = {seq, resume, enc}\nRequireEncryption = 1\nEncryptionKey = bench-key\n"
    )

    print("[bench] B5: 3-second hard Wi-Fi blackout (8 ch @ 512 Hz, tc netem loss 100% for 3.0 s)")
    results["benchmarks"]["b5_cut_liblsl"] = run_netns_pair(
        WORKER_LIBLSL, "B5_LSL", 8, 512.0, 16, 10.0, "", cut_sec=3.0,
    )
    results["benchmarks"]["b5_cut_labstream"] = run_netns_pair(
        WORKER_LABSTREAM, "B5_LS", 8, 512.0, 16, 10.0,
        "[labstream]\nFeatures = {seq, resume, dgram}\n", cut_sec=3.0,
    )

    # Save JSON
    json_path = RESULTS_DIR / "bench-results.json"
    json_path.write_text(json.dumps(results, indent=2) + "\n")

    # Build Markdown summary
    b = results["benchmarks"]
    def cpu_tot(k): return b[k]["outlet"]["cpu_pct"] + b[k]["inlet"]["cpu_pct"]

    md = [
        f"# labstream Benchmark Report (`{meta['git_sha']}` vs `liblsl {meta['liblsl_commit']}`)",
        "",
        f"- **CPU:** {meta['cpu']} ({meta['cores']} cores, governor `{meta['governor']}`)",
        f"- **Kernel:** Linux `{meta['kernel']}` | **Date (UTC):** `{meta['timestamp_utc']}`",
        "",
        "## Head-to-Head Results",
        "",
        "| Benchmark | Library / Mode | p50 (µs) | p99 (µs) | Total CPU (%) | Inlet RSS (MB) | Notes |",
        "|---|---|---:|---:|---:|---:|---|",
        f"| B1: Local IPC (64ch @ 1000 Hz, ch=1) | C++ `liblsl` (TCP) | {b['b1_local_ipc_liblsl']['inlet']['p50_us']:.1f} | {b['b1_local_ipc_liblsl']['inlet']['p99_us']:.1f} | {cpu_tot('b1_local_ipc_liblsl'):.1f}% | {b['b1_local_ipc_liblsl']['inlet']['rss_mb']:.1f} | baseline |",
        f"| B1: Local IPC (64ch @ 1000 Hz, ch=1) | **`labstream shm/1`** | **{b['b1_local_ipc_labstream_shm']['inlet']['p50_us']:.1f}** | **{b['b1_local_ipc_labstream_shm']['inlet']['p99_us']:.1f}** | **{cpu_tot('b1_local_ipc_labstream_shm'):.1f}%** | {b['b1_local_ipc_labstream_shm']['inlet']['rss_mb']:.1f} | `on_shm={b['b1_local_ipc_labstream_shm']['inlet']['on_shm']}` |",
        f"| B2: High-Rate (64ch @ 4000 Hz, ch=32) | C++ `liblsl` (TCP) | {b['b2_highrate_liblsl']['inlet']['p50_us']:.1f} | {b['b2_highrate_liblsl']['inlet']['p99_us']:.1f} | {cpu_tot('b2_highrate_liblsl'):.1f}% | {b['b2_highrate_liblsl']['inlet']['rss_mb']:.1f} | baseline |",
        f"| B2: High-Rate (64ch @ 4000 Hz, ch=32) | **`labstream shm/1`** | **{b['b2_highrate_labstream_shm']['inlet']['p50_us']:.1f}** | **{b['b2_highrate_labstream_shm']['inlet']['p99_us']:.1f}** | **{cpu_tot('b2_highrate_labstream_shm'):.1f}%** | {b['b2_highrate_labstream_shm']['inlet']['rss_mb']:.1f} | `on_shm={b['b2_highrate_labstream_shm']['inlet']['on_shm']}` |",
        f"| B3: Lossy Wi-Fi (20ms±5ms, 1% loss) | C++ `liblsl` (TCP) | {b['b3_lossy_liblsl']['inlet']['p50_us']/1000:.1f} ms | {b['b3_lossy_liblsl']['inlet']['p99_us']/1000:.1f} ms | {cpu_tot('b3_lossy_liblsl'):.1f}% | {b['b3_lossy_liblsl']['inlet']['rss_mb']:.1f} | TCP HOL blocking |",
        f"| B3: Lossy Wi-Fi (20ms±5ms, 1% loss) | **`labstream dgram/1`** | **{b['b3_lossy_labstream_dgram']['inlet']['p50_us']/1000:.1f} ms** | **{b['b3_lossy_labstream_dgram']['inlet']['p99_us']/1000:.1f} ms** | {cpu_tot('b3_lossy_labstream_dgram'):.1f}% | {b['b3_lossy_labstream_dgram']['inlet']['rss_mb']:.1f} | `lost={b['b3_lossy_labstream_dgram']['inlet']['lost']}` |",
        f"| B4: 32ch @ 500 Hz (ch=16) | `labstream` plain TCP | {b['b4_plain_tcp']['inlet']['p50_us']:.1f} | {b['b4_plain_tcp']['inlet']['p99_us']:.1f} | {cpu_tot('b4_plain_tcp'):.1f}% | {b['b4_plain_tcp']['inlet']['rss_mb']:.1f} | ratio `1.00` |",
        f"| B4: 32ch @ 500 Hz (ch=16) | **`labstream comp/1`** | {b['b4_comp_tcp']['inlet']['p50_us']:.1f} | {b['b4_comp_tcp']['inlet']['p99_us']:.1f} | {cpu_tot('b4_comp_tcp'):.1f}% | {b['b4_comp_tcp']['inlet']['rss_mb']:.1f} | ratio **`{b['b4_comp_tcp']['inlet']['comp_ratio']:.3f}`** |",
        f"| B4: 32ch @ 500 Hz (ch=16) | **`labstream enc/1`** | {b['b4_enc_tcp']['inlet']['p50_us']:.1f} | {b['b4_enc_tcp']['inlet']['p99_us']:.1f} | {cpu_tot('b4_enc_tcp'):.1f}% | {b['b4_enc_tcp']['inlet']['rss_mb']:.1f} | `encrypted={b['b4_enc_tcp']['inlet']['encrypted']}` |",
        f"| B5: 3 s Hard Cut (8ch @ 512 Hz) | C++ `liblsl` | — | — | — | — | rx `{b['b5_cut_liblsl']['inlet']['received']}` / `{b['b5_cut_liblsl']['outlet']['pushed']}` |",
        f"| B5: 3 s Hard Cut (8ch @ 512 Hz) | **`labstream resume/1`** | — | — | — | — | rx `{b['b5_cut_labstream']['inlet']['received']}` / `{b['b5_cut_labstream']['outlet']['pushed']}`, `lost={b['b5_cut_labstream']['inlet']['lost']}`, `resent={b['b5_cut_labstream']['inlet']['resent']}` |",
        "",
    ]
    md_text = "\n".join(md)
    (RESULTS_DIR / "bench-summary.md").write_text(md_text)

    # Generate pure-Python SVG charts
    write_bar_svg(
        RESULTS_DIR / "bench-latency.svg",
        "Local IPC & Lossy Wi-Fi Latency: C++ liblsl vs labstream",
        f"CPU: {meta['cpu']} | Linux {meta['kernel']} | commit {meta['git_sha']}",
        [
            ("B1 Local IPC p50 — C++ liblsl (TCP)", b["b1_local_ipc_liblsl"]["inlet"]["p50_us"], "#f87171", "64 ch @ 1000 Hz, chunk 1"),
            ("B1 Local IPC p50 — labstream shm/1", b["b1_local_ipc_labstream_shm"]["inlet"]["p50_us"], "#34d399", "lock-free shared-memory ring"),
            ("B1 Local IPC p99 — C++ liblsl (TCP)", b["b1_local_ipc_liblsl"]["inlet"]["p99_us"], "#fb923c", "tail latency"),
            ("B1 Local IPC p99 — labstream shm/1", b["b1_local_ipc_labstream_shm"]["inlet"]["p99_us"], "#10b981", "tail latency"),
        ],
        "µs",
    )
    write_bar_svg(
        RESULTS_DIR / "bench-cpu.svg",
        "Combined Outlet + Inlet CPU (% of 1 Core): C++ liblsl vs labstream",
        f"CPU: {meta['cpu']} | Linux {meta['kernel']} | commit {meta['git_sha']}",
        [
            ("B2 4000 Hz x 64 ch — C++ liblsl", cpu_tot("b2_highrate_liblsl"), "#f87171", "chunk 32, local TCP"),
            ("B2 4000 Hz x 64 ch — labstream shm/1", cpu_tot("b2_highrate_labstream_shm"), "#34d399", "chunk 32, shared memory"),
            ("B4 500 Hz x 32 ch — labstream plain", cpu_tot("b4_plain_tcp"), "#38bdf8", "TCP baseline"),
            ("B4 500 Hz x 32 ch — labstream comp/1", cpu_tot("b4_comp_tcp"), "#a78bfa", f"ratio {b['b4_comp_tcp']['inlet']['comp_ratio']:.3f}"),
            ("B4 500 Hz x 32 ch — labstream enc/1", cpu_tot("b4_enc_tcp"), "#fbbf24", "X25519 + ChaCha20-Poly1305"),
        ],
        "% CPU",
    )

    print("\n" + md_text)
    print(f"[bench] artifacts written to {RESULTS_DIR}/")


if __name__ == "__main__":
    main()
