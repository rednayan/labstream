#!/usr/bin/env python3
"""The live interop harness.

The harness runs one peer as an outlet and one peer as an inlet, then compares
the samples that arrived with the samples that went out. It knows nothing about
either implementation. A peer is any program that accepts this command line:

    peer outlet --name N --format F --channels C --srate S --input FILE
    peer inlet  --name N --format F --channels C --count K --output FILE

That interface is the whole contract. A Rust peer drops in with no change to
the harness.

The matrix has four cells. Only the two mixed cells test interoperation. The
two matching cells catch a defect in the harness itself.

    +----------------+------------------+------------------+
    |                | oracle inlet     | peer inlet       |
    +----------------+------------------+------------------+
    | oracle outlet  | control          | real test        |
    | peer outlet    | real test        | self-consistency |
    +----------------+------------------+------------------+

While the peer under test is the null one, every cell is liblsl against
liblsl. Every cell must pass. A failure means the harness holds the defect.

The harness must also be able to fail. `--selftest` corrupts a stream on
purpose and reports whether the comparison noticed. A harness that never fails
proves nothing.
"""

import argparse
import json
import pathlib
import struct
import subprocess
import sys
import tempfile
import time

FORMATS = {
    "float32": dict(width=4, kind="float"),
    "double64": dict(width=8, kind="float"),
    "int32": dict(width=4, kind="int"),
    "int16": dict(width=2, kind="int"),
    "int8": dict(width=1, kind="int"),
    "int64": dict(width=8, kind="int"),
    "string": dict(width=0, kind="string"),
}


def bits_of_double(x):
    return struct.unpack("<Q", struct.pack("<d", x))[0]


def expected_chunk_stamps(rows, chunk, srate):
    """The timestamps that a chunk push produces. SPEC.md 9.2 and 8.1.

    The value passed for a chunk names its **last** sample. The sender counts
    backward by `(count - 1) / rate` to date the first, writes only that one on
    the wire, and the reader adds one period at a time for the rest.

    This repeats the same arithmetic in the same order, so the result holds the
    exact numbers that both implementations produce. A different order would
    differ in the last bits and the comparison would fail for no real reason.
    """
    out = []
    for start in range(0, len(rows), chunk):
        block = rows[start:start + chunk]
        first = block[-1][0] - (len(block) - 1) / srate
        t = first
        out.append(t)
        for _ in block[1:]:
            t = t + 1.0 / srate
            out.append(t)
    return out


def make_samples(fmt, channels, count, srate=None):
    """Build a fixed sample list. Every value is exact and reproducible.

    In chunk mode the timestamps have to sit on the grid that the rate
    describes, because the reader rebuilds them by adding one period at a time.
    A grid of any other spacing would make the rebuilt values differ from the
    sent ones for a reason that is not a defect.
    """
    kind = FORMATS[fmt]["kind"]
    step = 0.25 if srate is None else 1.0 / srate
    rows = []
    for i in range(count):
        # A timestamp that is never 0.0, because a pushed 0.0 means "now"
        # (SPEC.md 8.3), and never -1.0, because that means "deduce this".
        ts = 1000.0 + i * step
        toks = []
        for k in range(channels):
            n = i * channels + k
            if kind == "float":
                if fmt == "float32":
                    v = struct.unpack("<f", struct.pack("<i", (n * 2654435761) % 2147483647))[0]
                    if v != v or v in (float("inf"), float("-inf")):
                        v = float(n)
                    toks.append(f"{struct.unpack('<I', struct.pack('<f', v))[0]:x}")
                else:
                    v = float(n) * 1.5 - 3.25
                    toks.append(f"{struct.unpack('<Q', struct.pack('<d', v))[0]:x}")
            elif kind == "int":
                w = FORMATS[fmt]["width"]
                lo = -(1 << (w * 8 - 1))
                hi = (1 << (w * 8 - 1)) - 1
                v = lo if n % 7 == 0 else (hi if n % 7 == 1 else (n % 97) - 48)
                toks.append(f"{v & ((1 << 64) - 1):x}")
            else:
                s = b"" if n % 5 == 0 else (b"val" + str(n).encode() + (b"\x00pad" if n % 3 == 0 else b""))
                # An empty value hex-encodes to nothing, and a whitespace split
                # then loses the field. A sentinel keeps the column count right.
                toks.append(s.hex() or "-")
        rows.append((ts, toks))
    return rows


def write_samples(path, rows):
    with open(path, "w") as f:
        for ts, toks in rows:
            f.write(f"{bits_of_double(ts):x} " + " ".join(toks) + "\n")


def read_samples(path):
    rows = []
    with open(path) as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            parts = line.split()
            rows.append((parts[0], parts[1:]))
    return rows


def normalize(rows):
    """Bring a written list and a read list into the same shape."""
    out = []
    for ts, toks in rows:
        t = f"{bits_of_double(ts):x}" if isinstance(ts, float) else ts.lstrip("0") or "0"
        out.append((
            t.lstrip("0") or "0",
            ["" if tok == "-" else (tok.lstrip("0") or "0") for tok in toks],
        ))
    return out


def wait_for(proc, marker, timeout, what):
    """Read the standard error of a peer until a marker arrives."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        line = proc.stderr.readline()
        if not line:
            raise RuntimeError(f"{what}: the peer stopped before {marker}")
        line = line.strip()
        if line.startswith("ERROR"):
            raise RuntimeError(f"{what}: {line}")
        if line.startswith(marker):
            return line
    raise RuntimeError(f"{what}: timed out waiting for {marker}")


def compare(want, have, tolerance):
    """Compare two sample lists.

    With no post-processing a timestamp is the one the sender wrote, so the
    comparison is exact. With a stage enabled the inlet corrects the value by
    an offset that it measured, and that offset differs between runs. The
    timestamp then needs a bound and the values stay exact.
    """
    problems = []
    if len(have) != len(want):
        problems.append(f"count: sent {len(want)}, received {len(have)}")
    for i, (a, b) in enumerate(zip(want, have)):
        if tolerance is None:
            if a[0] != b[0]:
                problems.append(f"sample {i}: timestamp {a[0]} became {b[0]}")
                break
        else:
            ta = struct.unpack("<d", struct.pack("<Q", int(a[0], 16)))[0]
            tb = struct.unpack("<d", struct.pack("<Q", int(b[0], 16)))[0]
            if abs(ta - tb) > tolerance:
                problems.append(
                    f"sample {i}: timestamp moved {tb - ta:+.9f} s, past the "
                    f"bound of {tolerance} s")
                break
        if a[1] != b[1]:
            problems.append(f"sample {i}: values {a[1]} became {b[1]}")
            break
    return problems


def expected_desc(channels):
    """The description tree that both peers publish.

    The text is built here rather than read from a peer, so the comparison has
    an outside reference. Two peers that agree with each other and with nothing
    else would pass a comparison that only put them side by side.

    The layout follows the oracle, measured in `captures/desc/`. One tab marks
    each level, and a tag whose only child is text stays on one line.

    `blank` shows a difference that only a live run finds. The sender builds it
    with an empty text value, and writes `<blank></blank>`. The reader drops a
    text node that holds nothing, so the tree that arrives holds `<blank />`
    and matches `hollow`, which never had a child at all. The two forms are
    apart in the sender and together in the receiver.

    The first run of this cell expected `<blank></blank>` and every cell
    failed, with real liblsl on both sides. The control cell is what named the
    expectation as the defect.
    """
    out = ["\t<desc>", "\t\t<channels>"]
    for i in range(channels):
        out.append("\t\t\t<channel>")
        out.append(f"\t\t\t\t<label>Ch{i}</label>")
        out.append("\t\t\t\t<unit>microvolts</unit>")
        out.append("\t\t\t\t<type>EEG</type>")
        out.append("\t\t\t</channel>")
    out.append("\t\t</channels>")
    out.append("\t\t<manufacturer>Acme &amp; Co &lt;test&gt;</manufacturer>")
    out.append("\t\t<blank />")
    out.append("\t\t<hollow />")
    out.append("\t</desc>")
    return "\n".join(out) + "\n"


def run_cell(outlet_bin, inlet_bin, fmt, channels, count, workdir, name, corrupt=None,
             postproc=0, tolerance=None, chunk=0, srate=100.0, desc=False,
             zero_stamps=False, sync=False):
    """Run one cell of the matrix and return the comparison result."""
    rows = make_samples(fmt, channels, count, srate if chunk else None)
    sent = pathlib.Path(workdir) / f"{name}_sent.txt"
    got = pathlib.Path(workdir) / f"{name}_got.txt"
    write_samples(sent, rows)

    outlet_cmd = [outlet_bin, "outlet", "--name", name, "--format", fmt,
                  "--channels", str(channels), "--srate", str(srate), "--input", str(sent)]
    if chunk:
        outlet_cmd += ["--chunk", str(chunk)]
    if zero_stamps:
        outlet_cmd += ["--zero-stamps", "1"]
    if sync:
        outlet_cmd += ["--sync", "1"]
    desc_file = pathlib.Path(workdir) / f"{name}_desc.txt"
    if desc:
        outlet_cmd += ["--desc", "1"]
    out_proc = subprocess.Popen(
        outlet_cmd,
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    in_proc = None
    try:
        wait_for(out_proc, "READY", 20, "outlet")

        inlet_cmd = [inlet_bin, "inlet", "--name", name, "--format", fmt,
                     "--channels", str(channels), "--count", str(count),
                     "--output", str(got)]
        if postproc:
            inlet_cmd += ["--postproc", str(postproc)]
        if desc:
            inlet_cmd += ["--desc", "1", "--desc-output", str(desc_file)]
        in_proc = subprocess.Popen(
            inlet_cmd,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        wait_for(in_proc, "READY", 40, "inlet")

        out_proc.stdin.write("GO\n")
        out_proc.stdin.flush()
        wait_for(out_proc, "PUSHED", 40, "outlet")
        wait_for(in_proc, "DONE", 40, "inlet")
    finally:
        for p in (in_proc, out_proc):
            if p is not None:
                p.terminate()
                try:
                    p.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    p.kill()

    problems = []
    if desc:
        wanted = expected_desc(channels)
        if not desc_file.exists():
            problems.append("the inlet wrote no description tree")
        else:
            arrived = desc_file.read_text()
            if corrupt == "drop_desc_line":
                arrived = "\n".join(arrived.split("\n")[:-2]) + "\n"
            if arrived != wanted:
                problems.append(desc_difference(wanted, arrived))

    # A pushed timestamp of zero means the current clock, so the value that
    # arrives is not the value that went out. SPEC.md 8.3. The comparison then
    # tests the rule instead of the numbers: every timestamp must be a real
    # clock reading, and the readings must rise.
    if zero_stamps:
        arrived = read_samples(got)
        problems += check_clock_stamps(arrived, len(rows), srate)
        want = [(None, r[1]) for r in normalize(rows)]
        have = [(None, r[1]) for r in normalize(arrived)]
        return problems + compare(want, have, tolerance)

    want = normalize(rows)
    have = normalize(read_samples(got))

    # A chunk carries one timestamp and the reader rebuilds the rest, so the
    # expected list comes from the rule rather than from the file. SPEC.md 9.2.
    if chunk:
        stamps = expected_chunk_stamps(rows, chunk, srate)
        want = [(f"{bits_of_double(t):x}".lstrip("0") or "0", w[1])
                for t, w in zip(stamps, want)]

    # The self test corrupts what arrived, so a working harness reports a
    # difference. A harness that still passes here proves nothing at all.
    if corrupt == "drop_last" and have:
        have = have[:-1]
    elif corrupt == "flip_value" and have:
        ts, toks = have[0]
        toks = list(toks)
        toks[0] = "deadbeef" if toks[0] != "deadbeef" else "1"
        have[0] = (ts, toks)
    elif corrupt == "shift_timestamp" and have:
        ts, toks = have[0]
        have[0] = (f"{bits_of_double(999.0):x}".lstrip("0"), toks)
    elif corrupt == "reorder" and len(have) > 1:
        have[0], have[1] = have[1], have[0]

    return problems + compare(want, have, tolerance)


def check_clock_stamps(rows, expected_count, srate):
    """Test the timestamps that a push of zero produces.

    liblsl replaces a pushed zero with the current clock inside the outlet
    (`src/stream_outlet_impl.cpp:170`). A consumer reads a timestamp of zero as
    "no sample", so an outlet that sends one delivers values that no
    application can use.

    Nothing here compares against the sent file, because the sender chose the
    values. The rule is what gets tested.
    """
    out = []
    stamps = [struct.unpack("<d", struct.pack("<Q", int(r[0], 16)))[0] for r in rows]
    if not stamps:
        return ["no sample arrived"]
    zeros = sum(1 for t in stamps if t == 0.0)
    if zeros:
        out.append(f"{zeros} of {len(stamps)} timestamps are zero, which reads as no sample")
        return out
    # A clock reading is the age of the machine in seconds, so it is far above
    # the small numbers that a test file holds.
    if stamps[0] < 1.0:
        out.append(f"the first timestamp is {stamps[0]}, which is not a clock reading")
    for k in range(1, len(stamps)):
        if stamps[k] <= stamps[k - 1]:
            out.append(f"timestamp {k} does not rise: {stamps[k - 1]} then {stamps[k]}")
            break
    return out


def desc_difference(want, have):
    """Name the first line that differs, so a failure is readable."""
    a, b = want.split("\n"), have.split("\n")
    for i in range(max(len(a), len(b))):
        x = a[i] if i < len(a) else "(nothing)"
        y = b[i] if i < len(b) else "(nothing)"
        if x != y:
            return f"the description tree differs at line {i + 1}: want {x!r}, got {y!r}"
    return "the description tree differs"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--oracle", default=".build/peer",
                    help="the peer that wraps real liblsl")
    ap.add_argument("--iut", default=None,
                    help="the implementation under test. Defaults to the oracle peer, "
                         "which makes every cell liblsl against liblsl.")
    ap.add_argument("--formats", default="float32,double64,int32,int16,int8,int64,string")
    ap.add_argument("--channels", type=int, default=4)
    ap.add_argument("--count", type=int, default=12)
    ap.add_argument("--selftest", action="store_true",
                    help="corrupt each stream on purpose and report whether the "
                         "comparison noticed")
    ap.add_argument("--json", default=None)
    ap.add_argument("--postproc", type=int, default=0,
                    help="post-processing flags for the inlet. 1 is clock sync, "
                         "2 is jitter removal, 4 is the monotonic clamp.")
    ap.add_argument("--tolerance", type=float, default=0.05,
                    help="how far a timestamp can move once a stage runs, in seconds")
    ap.add_argument("--chunk", type=int, default=0,
                    help="push in chunks of this many samples. A chunk carries one "
                         "timestamp for its last sample, so this exercises the rebuilt "
                         "timestamps that a single push never produces.")
    ap.add_argument("--srate", type=float, default=100.0)
    ap.add_argument("--sync", action="store_true",
                    help="open the outlet in the blocking, zero-copy mode. The wire is the "
                         "same, so every cell must still pass.")
    ap.add_argument("--zero-stamps", action="store_true",
                    help="push every sample with a timestamp of zero, which means the "
                         "current clock. The comparison then tests that rule.")
    ap.add_argument("--desc", action="store_true",
                    help="publish a channel tree and compare the tree that the inlet "
                         "read back. The tree never travels with a discovery answer, "
                         "so this exercises the data port on its own.")
    args = ap.parse_args()

    iut = args.iut or args.oracle
    null_run = args.iut is None
    cells = [
        ("oracle_out__oracle_in", args.oracle, args.oracle, "control"),
        ("oracle_out__iut_in", args.oracle, iut, "real test"),
        ("iut_out__oracle_in", iut, args.oracle, "real test"),
        ("iut_out__iut_in", iut, iut, "self-consistency"),
    ]

    results = []
    failed = 0
    with tempfile.TemporaryDirectory() as work:
        if args.selftest:
            print("=== harness self test: a corrupt stream must be reported ===")
            missed = 0
            corruptions = ["drop_last", "flip_value", "shift_timestamp", "reorder"]
            if args.desc:
                corruptions.append("drop_desc_line")
            for i, corrupt in enumerate(corruptions):
                problems = run_cell(args.oracle, args.oracle, "float32", 2, 6,
                                    work, f"self{i}", corrupt=corrupt,
                                    desc=args.desc)
                ok = bool(problems)
                if not ok:
                    missed += 1
                print(f"  {corrupt:18} {'reported' if ok else 'MISSED'}"
                      f"   {problems[0] if problems else ''}")
                results.append({"cell": f"selftest_{corrupt}", "detected": ok})
            print()
            if missed:
                print(f"the harness missed {missed} corrupt stream(s). It cannot judge "
                      f"an implementation.")
                return 1

        mode = "null peer, liblsl on both sides" if null_run else "IUT under test"
        if args.postproc:
            mode += f", post-processing {args.postproc}, timestamp bound {args.tolerance}s"
        if args.chunk:
            mode += f", chunks of {args.chunk}"
        if args.desc:
            mode += ", with a description tree"
        if args.sync:
            mode += ", the outlet in blocking mode"
        if args.zero_stamps:
            mode += ", timestamps pushed as zero"
        print(f"=== interop matrix ({mode}) ===")
        for fmt in args.formats.split(","):
            for cell_name, out_bin, in_bin, kind in cells:
                name = f"IO{abs(hash((fmt, cell_name))) % 100000}"
                try:
                    problems = run_cell(out_bin, in_bin, fmt, args.channels,
                                        args.count, work, name,
                                        postproc=args.postproc,
                                        tolerance=args.tolerance if args.postproc else None,
                                        chunk=args.chunk, srate=args.srate,
                                        desc=args.desc,
                                        zero_stamps=args.zero_stamps,
                                        sync=args.sync)
                except Exception as err:
                    problems = [f"the run failed: {err}"]
                ok = not problems
                if not ok:
                    failed += 1
                results.append({
                    "format": fmt, "cell": cell_name, "kind": kind,
                    "pass": ok, "problems": problems,
                })
                mark = "pass" if ok else "FAIL"
                print(f"  {fmt:9} {cell_name:24} {kind:17} {mark}"
                      f"{'  ' + problems[0] if problems else ''}")

    total = len([r for r in results if "format" in r])
    print(f"\n{total - failed} of {total} cells pass")
    if args.json:
        pathlib.Path(args.json).write_text(json.dumps(results, indent=1))
        print(f"wrote {args.json}")

    if null_run and failed:
        print("\nEvery cell used real liblsl on both sides, so the harness holds "
              "the defect and not the implementation.")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
