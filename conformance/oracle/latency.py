#!/usr/bin/env python3
"""Measure the push-to-pull latency of each library, and of each mixed pair.

The script builds `oracle/latency.c` once for each library, runs an outlet
process and an inlet process for each cell, and writes one table. Each cell
runs more than once, because one run of a timing test proves little.

The control row is liblsl to liblsl. With `--control`, the script also runs
that pair with a known delay in the timestamps, and the table must show the
same delay. If it does not, the instrument is wrong, not the library.

Usage:
    oracle/latency.py
    oracle/latency.py --lib oracle=.build/install/lib --lib main=/path/to/dir
    oracle/latency.py --loads 512x8x1,4000x64x32 --seconds 10 --repeat 3
    oracle/latency.py --control

A load `RATExCHANNELSxCHUNK` pushes CHUNK samples in each push call. The
first `--lib` is the reference. The script runs each library against itself,
and the reference against each other library, in both directions.
"""

import argparse
import json
import os
import statistics
import subprocess
import sys
import tempfile
import threading
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

DEFAULT_LOADS = "512x8x1,512x8x16,4000x64x1,4000x64x32"


def build(source: Path, lib_dir: Path, out: Path) -> None:
    cmd = [
        "cc", "-O2", "-I", str(ROOT / ".build" / "install" / "include"), str(source),
        "-o", str(out), "-lm", "-ldl", "-L", str(lib_dir), "-llsl",
        f"-Wl,-rpath,{lib_dir}",
    ]
    subprocess.run(cmd, check=True)


def wait_ready(proc: subprocess.Popen, what: str, lines: list) -> None:
    """Read stderr until the READY line. Keep the rest for an error report."""
    for raw in proc.stderr:
        line = raw.decode(errors="replace").rstrip()
        lines.append(line)
        if line == "READY":
            # Drain the rest in the background, so the pipe never fills.
            threading.Thread(target=lambda: [lines.append(r.decode(errors="replace").rstrip())
                                             for r in proc.stderr], daemon=True).start()
            return
    raise RuntimeError(f"{what}: the peer stopped before READY: {lines[-5:]}")


def last_json(text: str) -> dict:
    for line in reversed(text.splitlines()):
        line = line.strip()
        if line.startswith("{"):
            return json.loads(line)
    raise RuntimeError("no JSON line in the output")


def run_pair(out_bin: Path, in_bin: Path, name: str, rate: float, channels: int, chunk: int,
             seconds: float, offset_us: float) -> tuple:
    common = ["--name", name, "--rate", str(rate), "--channels", str(channels),
              "--seconds", str(seconds)]
    out_err, in_err = [], []
    outlet = subprocess.Popen([str(out_bin), "outlet", *common, "--chunk", str(chunk),
                               "--offset-us", str(offset_us)],
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    try:
        wait_ready(outlet, "outlet", out_err)
        inlet = subprocess.Popen([str(in_bin), "inlet", *common],
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        try:
            wait_ready(inlet, "inlet", in_err)
            in_out = inlet.stdout.read().decode()
            inlet.wait(timeout=seconds + 30)
        finally:
            if inlet.poll() is None:
                inlet.kill()
        out_out = outlet.stdout.read().decode()
        outlet.wait(timeout=30)
    finally:
        if outlet.poll() is None:
            outlet.kill()
    return last_json(out_out), last_json(in_out)


def spread(values: list) -> str:
    if len(values) == 1:
        return f"{values[0]:.0f}"
    return f"{statistics.median(values):.0f} ({min(values):.0f}-{max(values):.0f})"


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--lib", action="append", default=None, metavar="NAME=DIR",
                    help="a library directory that holds liblsl.so (repeat it)")
    ap.add_argument("--loads", default=DEFAULT_LOADS)
    ap.add_argument("--seconds", type=float, default=10.0)
    ap.add_argument("--repeat", type=int, default=3)
    ap.add_argument("--no-mixed", action="store_true",
                    help="run each library only against itself")
    ap.add_argument("--control", action="store_true",
                    help="also measure a known delay of 1000 us with the reference pair")
    ap.add_argument("--json", default=None, help="write every run to this file")
    args = ap.parse_args()

    libs = args.lib or [f"oracle={ROOT / '.build' / 'install' / 'lib'}",
                        f"rust={ROOT / '.build' / 'rustlib'}"]
    named = []
    for item in libs:
        name, _, path = item.partition("=")
        named.append((name, Path(path).resolve()))

    work = Path(tempfile.mkdtemp(prefix="latency-"))
    bins = {}
    for name, path in named:
        bins[name] = work / f"latency_{name}"
        build(ROOT / "oracle" / "latency.c", path, bins[name])

    ref = named[0][0]
    pairs = [(n, n) for n, _ in named]
    if not args.no_mixed:
        for n, _ in named[1:]:
            pairs += [(ref, n), (n, ref)]

    loads = []
    for spec in args.loads.split(","):
        rate, channels, chunk = spec.split("x")
        loads.append((float(rate), int(channels), int(chunk)))

    runs = []
    counter = 0

    def one(out_name, in_name, rate, channels, chunk, offset_us=0.0):
        nonlocal counter
        counter += 1
        stream = f"Lat{os.getpid()}_{counter}"
        o, i = run_pair(bins[out_name], bins[in_name], stream, rate, channels, chunk,
                        args.seconds, offset_us)
        row = {"outlet": out_name, "inlet": in_name, "rate": rate, "channels": channels,
               "chunk": chunk, "offset_us": offset_us, "outlet_run": o, "inlet_run": i}
        runs.append(row)
        print(f"  {out_name:>10} -> {in_name:<10} {rate:g} Hz x {channels} ch, chunk {chunk}: "
              f"p50 {i['p50_us']:.0f} us, p99 {i['p99_us']:.0f} us, lost {i['lost']}",
              file=sys.stderr)
        return row

    for rate, channels, chunk in loads:
        for out_name, in_name in pairs:
            for _ in range(args.repeat):
                one(out_name, in_name, rate, channels, chunk)

    control_ok = None
    if args.control:
        base = [one(ref, ref, 512.0, 8, 1)["inlet_run"]["p50_us"] for _ in range(args.repeat)]
        late = [one(ref, ref, 512.0, 8, 1, 1000.0)["inlet_run"]["p50_us"]
                for _ in range(args.repeat)]
        shift = statistics.median(late) - statistics.median(base)
        control_ok = abs(shift - 1000.0) < 100.0
        print(f"\ncontrol: a delay of 1000 us shows as {shift:.0f} us "
              f"({'pass' if control_ok else 'FAIL'})")

    print("\n| outlet -> inlet | load | p50 us | p99 us | p99.9 us | max us | lost | "
          "CPU outlet % | CPU inlet % |")
    print("|---|---|---|---|---|---|---|---|---|")
    for rate, channels, chunk in loads:
        for out_name, in_name in pairs:
            cell = [r for r in runs if r["outlet"] == out_name and r["inlet"] == in_name
                    and r["rate"] == rate and r["channels"] == channels
                    and r["chunk"] == chunk and r["offset_us"] == 0.0]
            if not cell:
                continue
            ins = [r["inlet_run"] for r in cell]
            outs = [r["outlet_run"] for r in cell]
            print(f"| {out_name} -> {in_name} | {rate:g} Hz x {channels}, chunk {chunk} | "
                  f"{spread([x['p50_us'] for x in ins])} | {spread([x['p99_us'] for x in ins])} | "
                  f"{spread([x['p999_us'] for x in ins])} | {max(x['max_us'] for x in ins):.0f} | "
                  f"{sum(x['lost'] + x['repeated'] + x['alien'] for x in ins)} | "
                  f"{statistics.median(x['cpu_pct'] for x in outs):.1f} | "
                  f"{statistics.median(x['cpu_pct'] for x in ins):.1f} |")
    print(f"\nEach cell: median of {args.repeat} runs, with the range in brackets. "
          f"Each run: {args.seconds:g} s. The first second is not measured.")
    print("Libraries:")
    for name, path in named:
        seen = {r["inlet_run"]["library"] for r in runs if r["inlet"] == name}
        print(f"  {name}: {path} (loaded: {', '.join(sorted(seen))})")

    if args.json:
        Path(args.json).write_text(json.dumps(runs, indent=1))
    if control_ok is False:
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
