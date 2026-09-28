#!/usr/bin/env python3
"""Build golden vectors for the LSL sample codec.

For each format and channel count this driver:
  1. starts the generator, which owns a real liblsl outlet,
  2. resolves the stream and opens a data feed,
  3. records the exact bytes that liblsl writes,
  4. reads the value record that the generator printed.

The value record is ground truth from the producer side. A test that compares
bytes alone passes when a decoder and an encoder hold the same error. The value
record closes that hole.

The driver asks for a byte order. A request that claims the opposite order and
a slow conversion speed makes the server swap every value, so the big-endian
vectors also come from the oracle and not from a rewrite of the codec.
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

# From SPEC.md 7.2.
FORMAT_ID = {
    "float32": 1, "double64": 2, "string": 3,
    "int32": 4, "int16": 5, "int8": 6, "int64": 7,
}
VALUE_SIZE = {
    "float32": 4, "double64": 8, "string": 0,
    "int32": 4, "int16": 2, "int8": 1, "int64": 8,
}


def resolve(name, timeout=5.0):
    """Send a shortinfo query and return the description of one stream."""
    rx = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    rx.bind(("", 0))
    port = rx.getsockname()[1]
    query = f"name='{name}'"
    msg = f"LSL:shortinfo\r\n{query}\r\n{port} vectorprobe\r\n".encode()

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
        if not sep or head.decode(errors="replace").strip() != "vectorprobe":
            continue
        text = body.decode(errors="replace")
        got = re.search(r"<name>(.*?)</name>", text)
        if not got or got.group(1) != name:
            continue
        return {
            "uid": re.search(r"<uid>(.*?)</uid>", text).group(1),
            "port": int(re.search(r"<v4data_port>(\d+)</v4data_port>", text).group(1)),
        }
    return None


def open_feed(host, port, uid, fmt, order, duration):
    """Open a data feed and return the answer headers and the sample bytes."""
    # Claiming the opposite order with a conversion speed of 0 makes the server
    # swap. SPEC.md 5.2 lists the four conditions.
    native = 1234
    claim = 1234 if order == "little" else 4321
    perf = 0 if order == "big" else 1000000

    s = socket.create_connection((host, port), timeout=5)
    req = (
        f"LSL:streamfeed/110 {uid}\r\n"
        f"Native-Byte-Order: {claim}\r\n"
        f"Endian-Performance: {perf}\r\n"
        "Has-IEEE754-Floats: 1\r\n"
        "Supports-Subnormals: 1\r\n"
        f"Value-Size: {VALUE_SIZE[fmt]}\r\n"
        "Data-Protocol-Version: 110\r\n"
        "Max-Buffer-Length: 3600\r\n"
        "Max-Chunk-Length: 0\r\n"
        "Hostname: vectorrig\r\nSource-Id: vectorrig\r\nSession-Id: default\r\n"
        "\r\n"
    ).encode()
    s.sendall(req)

    buf = b""
    deadline = time.monotonic() + duration
    s.settimeout(0.5)
    while time.monotonic() < deadline:
        try:
            chunk = s.recv(65536)
        except (socket.timeout, TimeoutError):
            continue
        if not chunk:
            break
        buf += chunk
    s.close()

    head, sep, body = buf.partition(b"\r\n\r\n")
    if not sep:
        raise RuntimeError("the answer held no header block")
    headers = {}
    lines = head.decode(errors="replace").split("\r\n")
    status = lines[0]
    for line in lines[1:]:
        if ":" in line:
            k, _, v = line.partition(":")
            headers[k.strip().lower()] = v.strip()
    return status, headers, body, native


def build(fmt, channels, order, genbin, outdir, index):
    name = f"Vec{index}{fmt}{channels}{order}"
    proc = subprocess.Popen(
        [genbin, fmt, str(channels), name],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
    )
    try:
        # Wait for the outlet to come up.
        while True:
            line = proc.stderr.readline()
            if not line:
                raise RuntimeError("the generator stopped before it was ready")
            if "OUTLET_READY" in line:
                break

        found = resolve(name)
        if not found:
            raise RuntimeError(f"could not resolve {name}")

        status, headers, body, native = open_feed(
            "127.0.0.1", found["port"], found["uid"], fmt, order, duration=6.0
        )

        pushed = json.loads(proc.stdout.read())
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()

    got_order = int(headers.get("byte-order", native))
    vector = {
        "name": name,
        "oracle_commit": "e651023ca67996a05a028fd88a28603297120294",
        "format": fmt,
        "format_id": FORMAT_ID[fmt],
        "channels": channels,
        "requested_order": order,
        "byte_order": got_order,
        "suppress_subnormals": headers.get("suppress-subnormals") == "1",
        "data_protocol_version": int(headers.get("data-protocol-version", 0)),
        "status": status,
        "bytes": body.hex(),
        "byte_len": len(body),
        "pushed": pushed["pushed"],
    }
    path = pathlib.Path(outdir) / f"{name}.json"
    path.write_text(json.dumps(vector, indent=1))
    swapped = "swapped" if got_order != native else "native"
    print(f"  {name:28} {len(body):7} bytes  order={got_order} ({swapped})  "
          f"pushed={len(pushed['pushed'])}")
    return vector


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--gen", default=".build/genvectors")
    ap.add_argument("--out", default=str(pathlib.Path(__file__).resolve().parents[2] / "crates/labstream-wire/tests/vectors"))
    ap.add_argument("--formats", default="float32,double64,string,int8,int16,int32,int64")
    ap.add_argument("--channels", default="1,2,8")
    ap.add_argument("--orders", default="little,big")
    args = ap.parse_args()

    pathlib.Path(args.out).mkdir(parents=True, exist_ok=True)
    formats = args.formats.split(",")
    channels = [int(c) for c in args.channels.split(",")]
    orders = args.orders.split(",")

    total, failed, i = 0, [], 0
    for fmt in formats:
        for ch in channels:
            for order in orders:
                i += 1
                try:
                    build(fmt, ch, order, args.gen, args.out, i)
                    total += 1
                except Exception as err:
                    failed.append(f"{fmt} x{ch} {order}: {err}")
                    print(f"  FAILED {fmt} x{ch} {order}: {err}")

    print(f"\nwrote {total} vector files to {args.out}")
    if failed:
        print(f"{len(failed)} failed:")
        for f in failed:
            print("  ", f)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
