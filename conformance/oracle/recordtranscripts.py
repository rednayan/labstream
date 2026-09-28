#!/usr/bin/env python3
"""Record handshake transcripts from the reference oracle.

Each case sends one crafted request to a real liblsl outlet and records the
answer. The replay test then drives the same request through the Rust
negotiation and compares the two answers.

The comparison runs after canonicalizing, because an answer carries a UID that
changes on every run. Every canonical placeholder carries a predicate, so an
erased field still gets an assertion. See `crates/lsl-proto/src/canon.rs`.

`Endian-Performance` is a measured benchmark result, and no test can compare
it. The cases therefore use only the two unambiguous extremes: a value of 0,
where the outlet always wins the speed contest, and a very large value, where
it always loses. A value in between depends on the machine.
"""

import argparse
import json
import pathlib
import re
import socket
import subprocess
import sys
import time

BASE_PORT = 16572
VALUE_SIZE = {"float32": 4, "double64": 8, "string": 0,
              "int32": 4, "int16": 2, "int8": 1, "int64": 8}

SLOW = 0            # the client is slower, so the outlet converts
FAST = 10 ** 12     # the client is faster, so the outlet leaves the order alone


def resolve(name, timeout=6.0):
    rx = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    rx.bind(("", 0))
    port = rx.getsockname()[1]
    msg = f"LSL:shortinfo\r\nname='{name}'\r\n{port} tprobe\r\n".encode()
    tx = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    tx.setsockopt(socket.SOL_SOCKET, socket.SO_BROADCAST, 1)
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        for p in range(BASE_PORT, BASE_PORT + 4):
            for addr in ("127.0.0.1", "255.255.255.255"):
                try:
                    tx.sendto(msg, (addr, p))
                except OSError:
                    pass
        rx.settimeout(0.4)
        try:
            data, _ = rx.recvfrom(65536)
        except (socket.timeout, TimeoutError):
            continue
        head, sep, body = data.partition(b"\r\n")
        if not sep or head.decode(errors="replace").strip() != "tprobe":
            continue
        text = body.decode(errors="replace")
        got = re.search(r"<name>(.*?)</name>", text)
        if not got or got.group(1) != name:
            continue
        return {
            "uid": re.search(r"<uid>(.*?)</uid>", text).group(1),
            "port": int(re.search(r"<v4data_port>(\d+)</v4data_port>", text).group(1)),
            "xml": text,
        }
    return None


def build_request(uid, fmt, *, version=110, order=1234, perf=SLOW,
                  ieee=1, subnormals=1, value_size=None, protocol_version=None,
                  data_protocol_version=110, extra_lines=(), chunk=0, buflen=360):
    """Build a request line and a header block."""
    vs = VALUE_SIZE[fmt] if value_size is None else value_size
    lines = [
        f"LSL:streamfeed/{version} {uid}",
        f"Native-Byte-Order: {order}",
        f"Endian-Performance: {perf}",
        f"Has-IEEE754-Floats: {ieee}",
        f"Supports-Subnormals: {subnormals}",
        f"Value-Size: {vs}",
        f"Data-Protocol-Version: {data_protocol_version}",
        f"Max-Buffer-Length: {buflen}",
        f"Max-Chunk-Length: {chunk}",
        "Hostname: transcriptrig",
        "Source-Id: transcriptrig",
        "Session-Id: default",
    ]
    if protocol_version is not None:
        lines.insert(6, f"Protocol-Version: {protocol_version}")
    lines.extend(extra_lines)
    return ("\r\n".join(lines) + "\r\n\r\n").encode()


def exchange(host, port, request, read_bytes=4096, timeout=4.0):
    """Send a request and read until the header block ends."""
    s = socket.create_connection((host, port), timeout=timeout)
    s.sendall(request)
    s.settimeout(timeout)
    buf = b""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline and len(buf) < read_bytes:
        try:
            chunk = s.recv(4096)
        except (socket.timeout, TimeoutError):
            break
        if not chunk:
            break
        buf += chunk
        if b"\r\n\r\n" in buf:
            break
    s.close()
    head, sep, _ = buf.partition(b"\r\n\r\n")
    # A refusal carries a status line and no header block, so it has no blank
    # line. Keep whatever arrived.
    return (head + b"\r\n\r\n").decode(errors="replace") if sep else buf.decode(errors="replace")


CASES = [
    # name,                     format,     kwargs
    ("plain_110",               "float32",  {}),
    ("plain_double",            "double64", {}),
    ("plain_string",            "string",   {}),
    ("plain_int16",             "int16",    {}),
    ("swap_slow_client",        "float32",  dict(order=4321, perf=SLOW)),
    ("noswap_fast_client",      "float32",  dict(order=4321, perf=FAST)),
    ("swap_double_slow",        "double64", dict(order=4321, perf=SLOW)),
    ("onebyte_never_swaps",     "int8",     dict(order=4321, perf=SLOW)),
    ("downgrade_value_size",    "float32",  dict(value_size=8)),
    ("downgrade_no_ieee754",    "float32",  dict(ieee=0)),
    ("suppress_subnormals",     "float32",  dict(subnormals=0)),
    ("subnormals_int_format",   "int32",    dict(subnormals=0)),
    ("live_protocol_version",   "float32",  dict(protocol_version=100)),
    ("dead_header_only",        "float32",  dict(data_protocol_version=100)),
    ("version_too_new",         "float32",  dict(version=200)),
    ("header_with_comment",     "float32",  dict(extra_lines=["Max-Chunk-Length: 7 ; a comment"])),
    ("header_odd_casing",       "float32",  dict(extra_lines=["mAx-ChUnK-LeNgTh: 5"])),
    ("header_without_colon",    "float32",  dict(extra_lines=["this line holds no colon"])),
    ("unknown_header",          "float32",  dict(extra_lines=["X-Not-A-Real-Header: 1"])),
    ("unknown_byte_order",      "float32",  dict(order=2134, perf=SLOW)),
]

# These cases need a UID that names no stream.
BAD_UID_CASES = [("wrong_uid", "float32", {}), ("empty_uid", "float32", {})]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--gen", default=".build/genvectors")
    ap.add_argument("--out", default="crates/lsl-proto/tests/transcripts")
    args = ap.parse_args()
    outdir = pathlib.Path(args.out)
    outdir.mkdir(parents=True, exist_ok=True)

    # One outlet per format keeps the run short.
    formats = sorted({f for _, f, _ in CASES} | {f for _, f, _ in BAD_UID_CASES})
    written, failed = 0, []

    for fmt in formats:
        name = f"Tr{fmt}"
        proc = subprocess.Popen([args.gen, fmt, "4", name],
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            while True:
                line = proc.stderr.readline()
                if not line:
                    raise RuntimeError("the generator stopped early")
                if "OUTLET_READY" in line:
                    break
            found = resolve(name)
            if not found:
                raise RuntimeError(f"could not resolve {name}")

            for case_name, case_fmt, kw in CASES:
                if case_fmt != fmt:
                    continue
                req = build_request(found["uid"], fmt, **kw)
                ans = exchange("127.0.0.1", found["port"], req)
                rec = {
                    "case": case_name,
                    "oracle_commit": "e651023ca67996a05a028fd88a28603297120294",
                    "format": fmt,
                    "channels": 4,
                    "stream_uid": found["uid"],
                    "request": req.decode(errors="replace"),
                    "answer": ans,
                }
                (outdir / f"{case_name}.json").write_text(json.dumps(rec, indent=1))
                first = ans.split("\r\n")[0]
                print(f"  {case_name:26} {first}")
                written += 1

            for case_name, case_fmt, kw in BAD_UID_CASES:
                if case_fmt != fmt:
                    continue
                uid = "" if case_name == "empty_uid" else "00000000-0000-0000-0000-000000000000"
                req = build_request(uid, fmt, **kw)
                ans = exchange("127.0.0.1", found["port"], req)
                rec = {
                    "case": case_name,
                    "oracle_commit": "e651023ca67996a05a028fd88a28603297120294",
                    "format": fmt,
                    "channels": 4,
                    "stream_uid": found["uid"],
                    "request": req.decode(errors="replace"),
                    "answer": ans,
                }
                (outdir / f"{case_name}.json").write_text(json.dumps(rec, indent=1))
                print(f"  {case_name:26} {ans.split(chr(13))[0].strip()}")
                written += 1

        except Exception as err:
            failed.append(f"{fmt}: {err}")
            print(f"  FAILED {fmt}: {err}")
        finally:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()

    print(f"\nwrote {written} transcripts to {outdir}")
    if failed:
        for f in failed:
            print("  ", f)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
