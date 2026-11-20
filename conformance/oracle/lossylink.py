#!/usr/bin/env python3
"""Measure the push-to-pull latency over a slow, lossy, or cut link.

`docs/roadmap.md`, step 0. The script needs no root:

1. It starts itself again in a new user namespace and network namespace
   (`unshare -Urn`). There it has the rights of root for its own
   namespaces only. The interfaces of the host do not change.
2. It makes a second network namespace inside the first one, and joins the
   two with a `veth` pair: 10.77.0.1 for the outlet, 10.77.0.2 for the
   inlet.
3. It puts one `netem` rule on each end of the pair, so each direction gets
   the same rule.
4. It runs `oracle/latency.c` as the outlet in the first namespace, and as
   the inlet in the second one. Discovery uses `lab.KnownPeers`.
5. A cut changes both rules to a loss of 100 %, then back. No packet
   arrives, and no socket closes: a half-open connection.

Usage:
    oracle/lossylink.py
    oracle/lossylink.py --lib liblsl=.build/install/lib --lib labstream=DIR
    oracle/lossylink.py --rules clean,delay --repeat 3

Run it without `LD_LIBRARY_PATH`. Each program finds its library through
its `RUNPATH`, and `LD_LIBRARY_PATH` takes precedence over it.
"""

import argparse
import json
import os
import statistics
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import latency  # noqa: E402  (the build and the output parser of latency.py)

ROOT = Path(__file__).resolve().parent.parent
OUTLET_ADDR = "10.77.0.1"
INLET_ADDR = "10.77.0.2"
INSIDE = "LABSTREAM_LOSSYLINK_INSIDE"

# name: (netem arguments, run seconds, cut start, cut seconds)
RULES = {
    "clean": ("", 10.0, None, None),
    "delay": ("delay 20ms", 10.0, None, None),
    "loss": ("delay 20ms loss 1%", 10.0, None, None),
    "rate": ("rate 1mbit", 10.0, None, None),
    "cut3": ("", 15.0, 5.0, 3.0),
    "cut10": ("", 25.0, 5.0, 10.0),
}


def spread(values: list) -> str:
    """The median, with the range in brackets, to 0.1 ms."""
    if len(values) == 1:
        return f"{values[0]:.1f}"
    return f"{statistics.median(values):.1f} ({min(values):.1f}-{max(values):.1f})"


def sh(*cmd: str) -> None:
    subprocess.run(list(cmd), check=True)


class Link:
    """The second namespace, the veth pair, and the rules on both ends."""

    def __init__(self):
        sh("ip", "link", "set", "lo", "up")
        self.holder = subprocess.Popen(["unshare", "-n", "sleep", "infinity"])
        time.sleep(0.3)
        self.pid = str(self.holder.pid)
        sh("ip", "link", "add", "va", "type", "veth", "peer", "name", "vb")
        sh("ip", "link", "set", "vb", "netns", self.pid)
        sh("ip", "addr", "add", f"{OUTLET_ADDR}/24", "dev", "va")
        sh("ip", "link", "set", "va", "up")
        self.inside("ip", "link", "set", "lo", "up")
        self.inside("ip", "addr", "add", f"{INLET_ADDR}/24", "dev", "vb")
        self.inside("ip", "link", "set", "vb", "up")
        self.set_rule("")

    def inside(self, *cmd: str) -> None:
        sh("nsenter", "-t", self.pid, "-n", *cmd)

    def prefix(self) -> list:
        """The command prefix that runs a program in the inlet namespace."""
        return ["nsenter", "-t", self.pid, "-n"]

    def set_rule(self, netem: str) -> None:
        # A `netem` rule with no arguments still passes each packet.
        args = netem.split() if netem else ["delay", "0ms"]
        sh("tc", "qdisc", "replace", "dev", "va", "root", "netem", *args)
        self.inside("tc", "qdisc", "replace", "dev", "vb", "root", "netem", *args)

    def close(self) -> None:
        self.holder.kill()
        self.holder.wait()


def run_pair(link: Link, out_bin: Path, in_bin: Path, name: str, rule: str) -> tuple:
    netem, seconds, cut_at, cut_for = RULES[rule]
    link.set_rule(netem)
    common = ["--name", name, "--rate", "512", "--channels", "8", "--seconds", str(seconds)]
    out_err, in_err = [], []
    outlet = subprocess.Popen([str(out_bin), "outlet", *common, "--chunk", "1"],
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    try:
        latency.wait_ready(outlet, "outlet", out_err)
        inlet = subprocess.Popen([*link.prefix(), str(in_bin), "inlet", *common,
                                  "--timeout", "60"],
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        try:
            latency.wait_ready(inlet, "inlet", in_err)
            if cut_at is not None:
                def cut():
                    time.sleep(cut_at)
                    link.set_rule("loss 100%")
                    time.sleep(cut_for)
                    link.set_rule(netem)
                threading.Thread(target=cut, daemon=True).start()
            in_out = inlet.stdout.read().decode()
            inlet.wait(timeout=seconds + 90)
        finally:
            if inlet.poll() is None:
                inlet.kill()
        out_out = outlet.stdout.read().decode()
        outlet.wait(timeout=60)
    finally:
        if outlet.poll() is None:
            outlet.kill()
    try:
        return latency.last_json(out_out), latency.last_json(in_out)
    except RuntimeError:
        raise RuntimeError(f"{rule}: no result. outlet: {out_err[-5:]} inlet: {in_err[-5:]}")


def inside(args) -> int:
    named = []
    for item in args.lib:
        name, _, path = item.partition("=")
        named.append((name, Path(path).resolve()))
    rules = args.rules.split(",")
    for r in rules:
        if r not in RULES:
            print(f"unknown rule {r}", file=sys.stderr)
            return 2

    # Discovery between the namespaces goes to the two addresses. liblsl and
    # labstream both read this file.
    work = Path(tempfile.mkdtemp(prefix="lossylink-"))
    cfg = work / "lsl_api.cfg"
    text = f"[lab]\nKnownPeers = {{{OUTLET_ADDR}, {INLET_ADDR}}}\n"
    # `--set labstream.ResumeSeconds=30` adds one key in its section.
    for item in args.set:
        key, _, value = item.partition("=")
        section, _, name = key.partition(".")
        text += f"[{section}]\n{name} = {value}\n"
    cfg.write_text(text)
    os.environ["LSLAPICFG"] = str(cfg)
    os.environ.pop("LD_LIBRARY_PATH", None)

    bins = {}
    for name, path in named:
        bins[name] = work / f"latency_{name}"
        latency.build(ROOT / "oracle" / "latency.c", path, bins[name])

    link = Link()
    runs = []
    counter = 0
    try:
        for rule in rules:
            for k in range(args.repeat):
                # Alternate the order of the libraries from run to run.
                order = named if k % 2 == 0 else list(reversed(named))
                for name, _ in order:
                    counter += 1
                    stream = f"Lossy{os.getpid()}_{counter}"
                    o, i = run_pair(link, bins[name], bins[name], stream, rule)
                    runs.append({"library": name, "rule": rule, "outlet_run": o,
                                 "inlet_run": i})
                    print(f"  {rule:<6} {name:<10} p50 {i['p50_us'] / 1000:.1f} ms, "
                          f"p99 {i['p99_us'] / 1000:.1f} ms, max {i['max_us'] / 1000:.1f} ms, "
                          f"lost {i['lost']}, repeated {i['repeated']}", file=sys.stderr)
    finally:
        link.close()

    print("\n| rule | library | p50 ms | p99 ms | max ms | lost | repeated |")
    print("|---|---|---|---|---|---|---|")
    for rule in rules:
        for name, _ in named:
            ins = [r["inlet_run"] for r in runs if r["rule"] == rule and r["library"] == name]
            ms = lambda key: spread([x[key] / 1000 for x in ins])  # noqa: E731
            print(f"| {rule} | {name} | {ms('p50_us')} | {ms('p99_us')} | "
                  f"{max(x['max_us'] for x in ins) / 1000:.1f} | "
                  f"{', '.join(str(x['lost']) for x in ins)} | "
                  f"{sum(x['repeated'] for x in ins)} |")
    print(f"\nEach cell: the median of {args.repeat} runs, with the range in brackets. "
          "The lost column gives each run.")

    ok = True
    if "clean" in rules and "delay" in rules:
        print("\ncontrol: the delay rule must add 20 ms to the median, within 2 ms")
        for name, _ in named:
            med = lambda rule: statistics.median(  # noqa: E731
                r["inlet_run"]["p50_us"] for r in runs
                if r["rule"] == rule and r["library"] == name) / 1000
            shift = med("delay") - med("clean")
            good = abs(shift - 20.0) <= 2.0
            ok = ok and good
            print(f"  {name}: {shift:.1f} ms ({'pass' if good else 'FAIL'})")
    if args.json:
        Path(args.json).write_text(json.dumps(runs, indent=1))
    return 0 if ok else 1


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--lib", action="append", default=None, metavar="NAME=DIR")
    ap.add_argument("--rules", default=",".join(RULES))
    ap.add_argument("--repeat", type=int, default=3)
    ap.add_argument("--json", default=None)
    ap.add_argument("--set", action="append", default=[], metavar="SECTION.KEY=VALUE",
                    help="add one key to the configuration file of both processes")
    args = ap.parse_args()
    if args.lib is None:
        args.lib = [f"liblsl={ROOT / '.build' / 'install' / 'lib'}",
                    f"labstream={ROOT / '.build' / 'rustlib'}"]
    # Resolve the paths before the namespace changes nothing but the network.
    args.lib = [f"{n}={Path(p).resolve()}" for n, _, p in
                (item.partition("=") for item in args.lib)]
    if os.environ.get(INSIDE):
        return inside(args)
    env = dict(os.environ, **{INSIDE: "1"})
    env.pop("LD_LIBRARY_PATH", None)
    cmd = ["unshare", "-Urn", sys.executable, str(Path(__file__).resolve()),
           "--rules", args.rules, "--repeat", str(args.repeat)]
    for item in args.lib:
        cmd += ["--lib", item]
    if args.json:
        cmd += ["--json", str(Path(args.json).resolve())]
    for item in args.set:
        cmd += ["--set", item]
    return subprocess.run(cmd, env=env).returncode


if __name__ == "__main__":
    sys.exit(main())
