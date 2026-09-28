#!/usr/bin/env python3
"""Watch how much memory each library holds while it runs.

A recording says nothing about this. Every sample can be right while the
process that sent it grows without limit, and the failure only arrives hours
later, in the middle of a session.

Three publishers run side by side, one for each library, and one consumer reads
from the Rust outlet. The resident size of every process is sampled, and the
first reading is held against the last.

**liblsl is the control.** An allocator holds memory it has stopped using, so a
number that rises is only meaningful next to the number that liblsl produces
under the same load.

    oracle/memsoak.py --minutes 60
"""

import argparse
import pathlib
import subprocess
import sys
import time

ROOT = pathlib.Path(__file__).resolve().parent.parent


def rss_kb(pid):
    """The resident size of one process, in kilobytes."""
    try:
        for line in open(f"/proc/{pid}/status"):
            if line.startswith("VmRSS:"):
                return int(line.split()[1])
    except OSError:
        return None
    return None


def slope_per_hour(samples):
    """A straight line through the readings, in kilobytes for each hour."""
    n = len(samples)
    if n < 3:
        return 0.0
    mean_t = sum(t for t, _ in samples) / n
    mean_v = sum(v for _, v in samples) / n
    top = sum((t - mean_t) * (v - mean_v) for t, v in samples)
    bottom = sum((t - mean_t) ** 2 for t, _ in samples)
    if bottom == 0:
        return 0.0
    return top / bottom * 3600.0


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--minutes", type=float, default=60.0)
    ap.add_argument("--every", type=float, default=30.0, help="seconds between readings")
    ap.add_argument("--rate", type=float, default=100.0)
    ap.add_argument("--channels", type=int, default=8)
    ap.add_argument("--out", default="artifacts/memsoak.csv")
    ap.add_argument("--analyse", default=None,
                    help="read a file that a run wrote and give the verdict, without running "
                         "anything. A run that was stopped early is still worth reading.")
    args = ap.parse_args()

    if args.analyse:
        history = read_csv(args.analyse)
        longest = max((s[-1][0] for s in history.values() if s), default=0.0)
        print(f"=== {args.analyse}, {longest / 60:.1f} minutes of readings")
        return report(history, longest / 60.0, args.analyse)

    rust_pub = ROOT.parent / "target" / "release" / "examples" / "publish"
    capi_pub = ROOT / ".build" / "publish_capi"
    oracle_pub = ROOT / ".build" / "publish_oracle"
    peer = ROOT.parent / "target" / "release" / "lsl-peer"
    for path in (rust_pub, capi_pub, oracle_pub, peer):
        if not path.exists():
            print(f"missing {path}")
            return 1

    common = ["--channels", str(args.channels), "--rate", str(args.rate), "--name", "MemSoak"]
    procs = {}
    started = []
    try:
        for tag, binary in (("oracle", oracle_pub), ("capi", capi_pub), ("rust", rust_pub)):
            p = subprocess.Popen([str(binary), *common, "--tag", tag],
                                 stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            procs[f"{tag} publisher"] = p
            started.append(p)
        time.sleep(4)

        # One consumer, so the outlet of this library has real work to do.
        reader = subprocess.Popen(
            [str(peer), "inlet", "--name", "MemSoak-rust", "--format", "float32",
             "--channels", str(args.channels), "--count", "100000000",
             "--output", "/dev/null", "--timeout", "30"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        procs["rust consumer"] = reader
        started.append(reader)
        time.sleep(4)

        for name, p in procs.items():
            if p.poll() is not None:
                print(f"{name} stopped before the run began")
                return 1

        history = {name: [] for name in procs}
        out = pathlib.Path(args.out)
        out.parent.mkdir(parents=True, exist_ok=True)
        f = out.open("w")
        f.write("seconds," + ",".join(procs) + "\n")

        print(f"=== reading the resident size every {args.every:.0f} s "
              f"for {args.minutes:.0f} minutes")
        begin = time.time()
        end = begin + args.minutes * 60.0
        while time.time() < end:
            now = time.time() - begin
            row = [f"{now:.0f}"]
            for name, p in procs.items():
                kb = rss_kb(p.pid)
                if kb is None:
                    print(f"  {name} stopped after {now / 60:.1f} minutes")
                    row.append("")
                    continue
                history[name].append((now, kb))
                row.append(str(kb))
            f.write(",".join(row) + "\n")
            f.flush()
            if int(now) % 600 < args.every:
                parts = " ".join(
                    f"{n.split()[0]}={history[n][-1][1] / 1024:.1f}MB"
                    for n in procs if history[n])
                print(f"  {now / 60:5.1f} min  {parts}", flush=True)
            time.sleep(args.every)
        f.close()
    finally:
        for p in started:
            p.terminate()
        for p in started:
            try:
                p.wait(timeout=5)
            except subprocess.TimeoutExpired:
                p.kill()

    return report(history, args.minutes, args.out)


def report(history, minutes, out_path):
    """Hold the readings against the control and give a verdict."""
    print()
    # A process reaches its working size in the first minutes. A line drawn
    # through that rise is a measure of the warm up and not of a leak, so the
    # early readings are left out of the line.
    warmup = max(2, len(next(iter(history.values()), [])) // 10)
    print(f"=== what each process held  (the first {warmup} readings are left out of the line)")
    print(f"  {'process':16}{'first':>10}{'last':>10}{'most':>10}{'growth':>10}{'per hour':>12}")
    control = None
    rows = {}
    for name, samples in history.items():
        if len(samples) < warmup + 3:
            print(f"  {name:16} too few readings")
            continue
        first, last = samples[0][1], samples[-1][1]
        most = max(v for _, v in samples)
        steady = samples[warmup:]
        rate = slope_per_hour(steady)
        rows[name] = (rate, last - steady[0][1])
        if name.startswith("oracle"):
            control = rate
        print(f"  {name:16}{first / 1024:9.1f}M{last / 1024:9.1f}M{most / 1024:9.1f}M"
              f"{(last - first) / 1024:9.2f}M{rate / 1024:11.2f}M")

    print()
    if control is None:
        print("no control was recorded, so nothing can be read from these numbers")
        return 1
    print(f"  liblsl, the control, changed by {control / 1024:.2f} MB in an hour.")
    if minutes < 10:
        print(f"  A run of {minutes:.0f} minutes is too short to name a leak. The numbers above")
        print("  are a warm up, and a line drawn through them says little.")
        return 0

    bad = []
    for name, (rate, grew) in rows.items():
        if name.startswith("oracle"):
            continue
        # Two tests have to agree. A line that rises far past the control, and
        # a real rise in the steady part of the run. One alone is noise.
        if rate > max(abs(control) * 3.0, 2048.0) and grew > 5 * 1024:
            bad.append((name, rate, grew))
    if bad:
        for name, rate, grew in bad:
            print(f"  {name} rose {grew / 1024:.1f} MB after the warm up, "
                  f"which is {rate / 1024:.2f} MB in an hour.")
        return 1
    print("  Nothing rose past the control after the warm up.")
    print(f"  The readings are in {out_path}.")
    return 0


def read_csv(path):
    """Rebuild the readings from a file that a run wrote.

    A run writes each reading as it takes it, so a run that was stopped early
    still holds every reading it managed to take.
    """
    lines = [l.strip() for l in open(path) if l.strip()]
    if len(lines) < 2:
        raise SystemExit(f"{path} holds no readings")
    names = lines[0].split(",")[1:]
    history = {n: [] for n in names}
    for line in lines[1:]:
        parts = line.split(",")
        when = float(parts[0])
        for name, value in zip(names, parts[1:]):
            if value:
                history[name].append((when, int(value)))
    return history


if __name__ == "__main__":
    sys.exit(main())
