#!/usr/bin/env python3
"""Run the example programs of liblsl against both libraries.

These programs come from `liblsl/examples/`. Upstream wrote them, and nothing
in this project changed a line of them. They are compiled twice, once against
real liblsl and once against the Rust library, and then run.

Three questions, in order of strength:

1. **Does every program link?** `lsl_cpp.h` calls the plain names such as
   `lsl_push_sample_f`, so a library that exports only some of them cannot
   load. This is a yes or no answer for the whole set.
2. **Does a program that runs on its own produce the same output?** Some
   examples build an outlet and an inlet in one process, so the output is the
   same on every run once the identifiers and the ports are masked.
3. **Does a sender of one library reach a receiver of the other?** The pair
   runs in all four combinations. The two mixed pairs test the wire, and the
   two matching pairs show that a failure belongs to the wire and not to the
   harness.

Usage:
    oracle/examples.py
    oracle/examples.py --only HandleMetaData
"""

import argparse
import pathlib
import re
import shutil
import subprocess
import sys
import tempfile
import time

ROOT = pathlib.Path(__file__).resolve().parent.parent
EXAMPLES = ROOT / "liblsl" / "examples"
INCLUDE = ROOT / ".build" / "install" / "include"

# A benchmark measures the machine, not the library.
SKIP = {"BenchmarkSyncVsAsync.cpp"}

# Programs that build an outlet and an inlet in one process. The output is
# compared byte for byte after the mask below.
SELF_CONTAINED = [
    ("HandleMetaData", ["MetaSelfCpp"], 30),
    ("HandleMetaDataC", [], 30),
]

# A sender and a receiver, with the arguments that make them meet. The receiver
# stops on its own or after the time limit.
#
# `expect` is a piece of text that has to appear in the output. Without it the
# check only asks whether the receiver wrote anything, and a program that
# writes a banner before it resolves would pass while finding nothing.
PAIRS = [
    ("SendDataSimple", ["ExSimple"], "ReceiveDataSimple", ["ExSimple"], 6, None),
    ("SendData", ["ExEEG", "ExType", "8", "100"], "ReceiveData", ["name", "ExEEG", "10"], 10, None),
    ("SendDataC", ["ExC"], "ReceiveDataC", [], 8, None),
    ("SendStringMarkers", ["ExMarkers"], "ReceiveStringMarkers", [], 8, None),
    ("SendDataInChunks", ["ExAudio", "Audio", "1000", "2"], "ReceiveDataInChunks", ["ExAudio"], 8,
     None),
    # These three read the description rather than the data.
    ("SendDataSimple", ["ExAll"], "GetAllStreams", [], 10, "Found ExAll"),
    ("SendDataSimple", ["ExFull"], "GetFullinfo", ["name", "ExFull"], 10,
     "<name>ExFull</name>"),
    ("SendDataSimple", ["ExTime"], "GetTimeCorrection", ["name", "ExTime"], 10,
     "<name>ExTime</name>"),
]

# Everything that changes between two runs of the same program.
MASKS = [
    (re.compile(r"<uid>[^<]*</uid>"), "<uid>M</uid>"),
    (re.compile(r"<created_at>[^<]*</created_at>"), "<created_at>M</created_at>"),
    (re.compile(r"<hostname>[^<]*</hostname>"), "<hostname>M</hostname>"),
    (re.compile(r"<v4address>[^<]*</v4address>"), "<v4address>M</v4address>"),
    (re.compile(r"<v6address>[^<]*</v6address>"), "<v6address>M</v6address>"),
    (re.compile(r"<v4address />"), "<v4address>M</v4address>"),
    (re.compile(r"<v6address />"), "<v6address>M</v6address>"),
    (re.compile(r"<v4data_port>[^<]*</v4data_port>"), "<v4data_port>M</v4data_port>"),
    (re.compile(r"<v4service_port>[^<]*</v4service_port>"), "<v4service_port>M</v4service_port>"),
    # liblsl binds an IPv6 port and reports its number. This implementation
    # binds none and reports zero. That is a real difference in what the two
    # libraries do, and it is recorded in CONFORMANCE-PLAN.md rather than
    # hidden. Every capture and every live test so far used IPv4.
    (re.compile(r"<v6data_port>[^<]*</v6data_port>"), "<v6data_port>M</v6data_port>"),
    (re.compile(r"<v6service_port>[^<]*</v6service_port>"), "<v6service_port>M</v6service_port>"),
    (re.compile(r"<session_id>[^<]*</session_id>"), "<session_id>M</session_id>"),
]


def mask(text):
    for pattern, replacement in MASKS:
        text = pattern.sub(replacement, text)
    return text


def build(source, libdir, outdir):
    """Compile one example against one library."""
    out = outdir / source.stem
    cc = ["cc"] if source.suffix == ".c" else ["c++", "-std=c++17"]
    cmd = cc + [
        "-I", str(INCLUDE), str(source), "-o", str(out),
        "-L", str(libdir), "-llsl", "-Wl,-rpath," + str(libdir),
    ]
    proc = subprocess.run(cmd, capture_output=True, text=True)
    return (out if proc.returncode == 0 else None), proc.stderr.strip()


def run_for(cmd, seconds):
    """Run a program and return what it wrote, whatever way it stops."""
    proc = subprocess.Popen(
        cmd, stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    try:
        return proc.communicate(timeout=seconds)
    except subprocess.TimeoutExpired:
        proc.terminate()
        try:
            return proc.communicate(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
            return proc.communicate()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--oracle", default=str(ROOT / ".build" / "install" / "lib"))
    ap.add_argument("--rust", default=str(ROOT / ".build" / "rustlib"))
    ap.add_argument("--only", default=None, help="run one program or one pair by name")
    args = ap.parse_args()

    oracle_dir, rust_dir = pathlib.Path(args.oracle), pathlib.Path(args.rust)
    sources = sorted(p for p in EXAMPLES.iterdir()
                     if p.suffix in (".c", ".cpp") and p.name not in SKIP)

    work = pathlib.Path(tempfile.mkdtemp(prefix="lsl-examples-"))
    failed = 0
    try:
        # --- 1. does everything link ---
        print(f"=== link: {len(sources)} example programs, both libraries")
        built = {"oracle": {}, "rust": {}}
        for source in sources:
            for tag, libdir in (("oracle", oracle_dir), ("rust", rust_dir)):
                outdir = work / tag
                outdir.mkdir(parents=True, exist_ok=True)
                path, err = build(source, libdir, outdir)
                if path is None:
                    failed += 1
                    first = (err.splitlines() or ["no output"])[-1]
                    print(f"  {source.name:26} {tag:7} FAIL   {first[:90]}")
                else:
                    built[tag][source.stem] = path
        print(f"  {len(built['rust'])} of {len(sources)} link against the Rust library")
        print(f"  {len(built['oracle'])} of {len(sources)} link against liblsl")
        print()

        # --- 2. programs that run on their own ---
        print("=== output: a program that runs on its own must write the same text")
        for name, argv, limit in SELF_CONTAINED:
            if args.only and args.only != name:
                continue
            if name not in built["oracle"] or name not in built["rust"]:
                print(f"  {name:26} SKIP   it did not build")
                failed += 1
                continue
            a, _ = run_for([str(built["oracle"][name])] + argv, limit)
            time.sleep(0.5)
            b, _ = run_for([str(built["rust"][name])] + argv, limit)
            a, b = mask(a), mask(b)
            if a == b and a.strip():
                print(f"  {name:26} pass   {len(a.splitlines())} lines, identical")
            else:
                failed += 1
                print(f"  {name:26} FAIL   {'no output' if not a.strip() else 'differs'}")
                for i, (x, y) in enumerate(zip(a.splitlines(), b.splitlines()), 1):
                    if x != y:
                        print(f"    line {i}\n      liblsl {x!r}\n      rust   {y!r}")
                        break
        print()

        # --- 3. a sender of one library and a receiver of the other ---
        print("=== wire: a sender of one library must reach a receiver of the other")
        for sender, sargv, receiver, rargv, limit, expect in PAIRS:
            if args.only and args.only not in (sender, receiver):
                continue
            missing = [n for n in (sender, receiver)
                       if n not in built["oracle"] or n not in built["rust"]]
            if missing:
                print(f"  {sender} + {receiver:24} SKIP   {missing} did not build")
                failed += 1
                continue
            for stag in ("oracle", "rust"):
                for rtag in ("oracle", "rust"):
                    out_proc = subprocess.Popen(
                        [str(built[stag][sender])] + sargv, stdin=subprocess.DEVNULL,
                        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                    try:
                        time.sleep(1.0)
                        text, err = run_for([str(built[rtag][receiver])] + rargv, limit)
                    finally:
                        out_proc.terminate()
                        try:
                            out_proc.wait(timeout=5)
                        except subprocess.TimeoutExpired:
                            out_proc.kill()
                    lines = [l for l in text.splitlines() if l.strip()]
                    label = f"{sender}[{stag}] -> {receiver}[{rtag}]"
                    kind = "mixed" if stag != rtag else "control"
                    ok = bool(lines) and (expect is None or expect in text)
                    if ok:
                        note = f"{len(lines)} lines"
                        if expect:
                            note += f", found {expect!r}"
                        print(f"  {label:52} {kind:8} pass   {note}")
                    else:
                        failed += 1
                        if lines and expect:
                            why = f"{len(lines)} lines, but no {expect!r}"
                        else:
                            why = (err.strip().splitlines() or ["no output"])[-1]
                        print(f"  {label:52} {kind:8} FAIL   {why[:70]}")
                    time.sleep(0.5)
    finally:
        shutil.rmtree(work, ignore_errors=True)

    print()
    if failed:
        print(f"{failed} check(s) failed")
        return 1
    print("every example links, runs, and agrees")
    return 0


if __name__ == "__main__":
    sys.exit(main())
