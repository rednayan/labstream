#!/usr/bin/env python3
"""Measure how much XPath a resolver really needs.

Stream matching in liblsl runs a full XPath 1.0 query through pugixml
(`src/stream_info_impl.cpp:214`). That is risk R1 of the conformance plan, and
it has been the largest unknown in the rewrite since the first week.

The plan says to decide with data. This tool supplies the data. It sends a
battery of queries to a real outlet and records which ones match, so the
question "how much XPath must a resolver support" gets an answer with numbers
instead of an opinion.

A query that liblsl cannot parse throws inside pugixml. liblsl catches it, logs
a warning, and returns no match (`src/stream_info_impl.cpp:239-241`). A silent
absence of an answer therefore means one of two things, and this tool cannot
tell them apart from outside: the query parsed and did not match, or it failed
to parse. Both look the same on the wire, which is itself worth knowing.

**Stop every other stream before a run.** A query goes to the whole machine,
and this tool records whether *any* stream answered. A run with other streams
alive reported `channel_count<4` and `not(name='XpTest')` as matches, which
was correct: a marker stream of one channel was live and answered both. The
expected values below hold only when the test stream is the only one there.
"""

import argparse
import json
import pathlib
import socket
import subprocess
import sys
import time

BASE_PORT = 16572
MULTICAST_PORT = 16571

# The stream that every query runs against.
STREAM = {
    "name": "XpTest",
    "type": "EEG",
    "channel_count": 8,
    "srate": 100,
    "source_id": "xp_src",
    "session_id": "default",
}

# Each entry: group, query, what the query is for, and whether a match is
# expected from reading the stream description.
BATTERY = [
    # --- the subset that liblsl itself builds -------------------------------
    ("used", "", "an empty query matches every stream", True),
    ("used", "session_id='default'", "the session test that every query carries", True),
    ("used", "session_id='default' and name='XpTest'", "the shape resolve_stream builds", True),
    ("used", "session_id='default' and name='Other'", "the same shape, no match", False),
    ("used", "name='XpTest'", "a bare equality", True),
    ("used", "type='EEG'", "a different field", True),
    ("used", "source_id='xp_src'", "the source identifier", True),

    # --- boolean structure --------------------------------------------------
    ("boolean", "name='XpTest' or name='Nothing'", "disjunction", True),
    ("boolean", "name='Nothing' or name='AlsoNothing'", "disjunction, no match", False),
    ("boolean", "not(name='Nothing')", "negation", True),
    ("boolean", "not(name='XpTest')", "negation, no match", False),
    ("boolean", "name='XpTest' and type='EEG' and source_id='xp_src'", "three terms", True),
    ("boolean", "(name='XpTest' or name='X') and type='EEG'", "parentheses", True),
    ("boolean", "true()", "a boolean literal", True),
    ("boolean", "false()", "a boolean literal, no match", False),

    # --- string functions ---------------------------------------------------
    ("string", "contains(name,'XpT')", "contains", True),
    ("string", "contains(name,'zzz')", "contains, no match", False),
    ("string", "starts-with(name,'Xp')", "starts-with", True),
    ("string", "starts-with(name,'zz')", "starts-with, no match", False),
    ("string", "string-length(name)>3", "string-length with a comparison", True),
    ("string", "substring(name,1,2)='Xp'", "substring", True),
    ("string", "translate(name,'X','x')='xpTest'", "translate", True),
    ("string", "concat(name,type)='XpTestEEG'", "concat", True),
    ("string", "normalize-space(name)='XpTest'", "normalize-space", True),

    # --- numbers ------------------------------------------------------------
    ("number", "channel_count=8", "numeric equality", True),
    ("number", "channel_count>4", "greater than", True),
    ("number", "channel_count<4", "less than, no match", False),
    ("number", "channel_count>=8", "greater or equal", True),
    ("number", "nominal_srate=100", "a real number", True),
    ("number", "number(channel_count)+1=9", "arithmetic", True),
    ("number", "floor(nominal_srate div 3)=33", "division and floor", True),

    # --- paths --------------------------------------------------------------
    ("path", "info/name='XpTest'", "a path below the root", False),
    ("path", "//name='XpTest'", "a descendant path", True),
    ("path", "name", "a bare node test, which is true when the node exists", True),
    ("path", "missing_field", "a node that does not exist", False),
    ("path", "count(//name)=1", "count over a path", True),
    ("path", "//v4data_port>0", "a numeric field deeper in the document", True),

    # --- malformed ----------------------------------------------------------
    ("malformed", "in'va'lid", "unbalanced quotes", False),
    ("malformed", "name=", "an incomplete comparison", False),
    ("malformed", "and and and", "keywords only", False),
    ("malformed", "name='XpTest'))", "unbalanced parentheses", False),
    ("malformed", "!!!", "not a query at all", False),
]


def probe(query, port_span=4, timeout=1.2):
    """Send one query and report whether an answer came back."""
    rx = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    rx.bind(("", 0))
    rx.settimeout(timeout)
    port = rx.getsockname()[1]
    token = "xpprobe"
    msg = f"LSL:shortinfo\r\n{query}\r\n{port} {token}\r\n".encode()

    tx = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    tx.setsockopt(socket.SOL_SOCKET, socket.SO_BROADCAST, 1)
    for p in list(range(BASE_PORT, BASE_PORT + port_span)) + [MULTICAST_PORT]:
        for addr in ("127.0.0.1", "255.255.255.255"):
            try:
                tx.sendto(msg, (addr, p))
            except OSError:
                pass
    try:
        data, _ = rx.recvfrom(65536)
    except (socket.timeout, TimeoutError):
        return False
    head, sep, _ = data.partition(b"\r\n")
    return bool(sep) and head.decode(errors="replace").strip() == token


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--peer", default=".build/peer")
    ap.add_argument("--out", default="artifacts/xpath-liblsl.json")
    args = ap.parse_args()

    work = pathlib.Path("artifacts/behavior-work")
    work.mkdir(parents=True, exist_ok=True)
    sample_file = work / "xp_sent.txt"
    sample_file.write_text("4093880000000000 " + " ".join(["0"] * STREAM["channel_count"]) + "\n")

    out = subprocess.Popen(
        [args.peer, "outlet", "--name", STREAM["name"], "--format", "float32",
         "--channels", str(STREAM["channel_count"]), "--srate", str(STREAM["srate"]),
         "--input", str(sample_file), "--source-id", STREAM["source_id"],
         "--type", STREAM["type"]],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)

    results = []
    try:
        # Wait for the outlet to come up.
        while True:
            line = out.stderr.readline()
            if not line:
                raise RuntimeError("the outlet stopped early")
            if line.startswith("READY"):
                break
        time.sleep(0.5)

        print(f"{'group':10} {'match':>6} {'want':>6}   query")
        for group, query, why, expect in BATTERY:
            got = probe(query)
            results.append({
                "group": group, "query": query, "purpose": why,
                "matched": got, "expected": expect, "as_expected": got == expect,
            })
            mark = "" if got == expect else "   <-- differs from the reading"
            print(f"{group:10} {str(got):>6} {str(expect):>6}   {query!r}{mark}")
    finally:
        out.terminate()
        try:
            out.wait(timeout=5)
        except subprocess.TimeoutExpired:
            out.kill()

    # Summarize by group: this is the number that scopes the work.
    groups = {}
    for r in results:
        g = groups.setdefault(r["group"], {"total": 0, "matched": 0, "surprises": 0})
        g["total"] += 1
        g["matched"] += 1 if r["matched"] else 0
        g["surprises"] += 0 if r["as_expected"] else 1

    print("\nby group:")
    for g, v in groups.items():
        print(f"  {g:10} {v['total']:2} queries, {v['matched']:2} matched, "
              f"{v['surprises']} differed from the reading")

    pathlib.Path(args.out).parent.mkdir(parents=True, exist_ok=True)
    pathlib.Path(args.out).write_text(json.dumps(
        {"oracle_commit": "e651023ca67996a05a028fd88a28603297120294",
         "stream": STREAM, "results": results, "groups": groups}, indent=1))
    print(f"\nwrote {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
