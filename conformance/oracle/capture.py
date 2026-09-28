#!/usr/bin/env python3
"""Capture rig for the LSL conformance suite.

This tool speaks the LSL protocol directly and records the exact bytes that a
real liblsl outlet sends back. It never links liblsl. That is deliberate. A
capture tool that used liblsl would inherit any defect that liblsl has.

There is no LSL protocol specification. Every message format here was read from
the liblsl source at the revision named in oracle/PINS. Each format carries a
citation to the file and line that defines it.

Commands:
  discover   Send a shortinfo query. Record every reply.
  timeprobe  Send a time probe to one outlet. Record the reply.
  feed       Open a data feed. Record the handshake and the first samples.
"""

import argparse
import json
import pathlib
import socket
import struct
import sys
import time

# Defaults from src/api_config.cpp:170-202.
BASE_PORT = 16572
PORT_RANGE = 32
LINK_ADDRESSES = ["255.255.255.255", "224.0.0.1", "224.0.0.183"]
SITE_ADDRESSES = ["239.255.172.215"]

# Sample encoding tags, from src/sample.h:19-20.
TAG_DEDUCED_TIMESTAMP = 1
TAG_TRANSMITTED_TIMESTAMP = 2


def now():
    """Monotonic time, to match the clock that liblsl uses (src/common.cpp:20)."""
    return time.monotonic()


class Recorder:
    """Collects one capture as an ordered list of events.

    Every event carries a direction and a monotonic offset. The raw bytes are
    stored as hex, so the record stays readable and stays exact.
    """

    def __init__(self, name):
        self.name = name
        self.t0 = now()
        self.events = []

    def add(self, direction, peer, data, note=""):
        self.events.append({
            "t": round(now() - self.t0, 6),
            "dir": direction,          # "tx" or "rx"
            "peer": peer,
            "len": len(data),
            "hex": data.hex(),
            "text": _printable(data),
            "note": note,
        })

    def save(self, outdir):
        outdir = pathlib.Path(outdir)
        outdir.mkdir(parents=True, exist_ok=True)
        path = outdir / f"{self.name}.json"
        path.write_text(json.dumps({
            "capture": self.name,
            "oracle_commit": "e651023ca67996a05a028fd88a28603297120294",
            "events": self.events,
        }, indent=2))
        return path


def _printable(data):
    """Return a readable form of the bytes, for a human reading the record."""
    try:
        s = data.decode("utf-8")
    except UnicodeDecodeError:
        return None
    return s if all(c == "\n" or c == "\r" or c == "\t" or " " <= c <= "~" for c in s) else None


# --------------------------------------------------------------------------
# discovery
# --------------------------------------------------------------------------

def cmd_discover(args):
    """Send a shortinfo query and record every reply.

    Query format, from src/resolve_attempt_udp.cpp:52-57:
        LSL:shortinfo\\r\\n
        <query>\\r\\n
        <return_port> <query_id>\\r\\n

    The query_id is opaque. liblsl builds it with std::hash, whose value differs
    between standard libraries. The responder echoes it. We only compare it with
    what we sent, and we never derive it.
    """
    rec = Recorder(args.name)

    recv = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    recv.bind(("", 0))
    recv.settimeout(args.timeout)
    return_port = recv.getsockname()[1]

    query_id = args.query_id
    msg = f"LSL:shortinfo\r\n{args.query}\r\n{return_port} {query_id}\r\n".encode()

    send = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    send.setsockopt(socket.SOL_SOCKET, socket.SO_BROADCAST, 1)
    send.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_TTL, 1)

    targets = []
    for addr in LINK_ADDRESSES + SITE_ADDRESSES + list(args.extra_host):
        for port in range(BASE_PORT, BASE_PORT + args.port_span):
            targets.append((addr, port))

    print(f"query   : {args.query!r}")
    print(f"reply to: port {return_port}")
    print(f"targets : {len(targets)} address and port pairs")

    for target in targets:
        try:
            send.sendto(msg, target)
            rec.add("tx", f"{target[0]}:{target[1]}", msg, "shortinfo query")
        except OSError as err:
            # A host without a route to a multicast group is normal. Record it
            # and continue, because another target can still answer.
            rec.add("tx", f"{target[0]}:{target[1]}", b"", f"send failed: {err}")

    deadline = now() + args.timeout
    replies = 0
    while now() < deadline:
        recv.settimeout(max(0.05, deadline - now()))
        try:
            data, peer = recv.recvfrom(65536)
        except (socket.timeout, TimeoutError):
            break
        note = ""
        head, sep, _ = data.partition(b"\r\n")
        if sep and head.decode(errors="replace").strip() == query_id:
            note = "query_id matches"
            replies += 1
        else:
            note = "query_id does NOT match, reply ignored by a real inlet"
        rec.add("rx", f"{peer[0]}:{peer[1]}", data, note)

    path = rec.save(args.out)
    print(f"replies : {replies} matching")
    print(f"saved   : {path}")
    return 0 if replies or args.allow_empty else 1


# --------------------------------------------------------------------------
# time probe
# --------------------------------------------------------------------------

def cmd_timeprobe(args):
    """Send a time probe and record the reply.

    Request, from src/time_receiver.cpp:136:
        LSL:timedata\\r\\n<wave_id> <t0>\\r\\n
    Reply, from src/udp_server.cpp process_timedata_request:
        ' ' <wave_id> <t0> <t1> <t2>
    """
    rec = Recorder(args.name)
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.settimeout(args.timeout)

    for i in range(args.count):
        t0 = now()
        msg = f"LSL:timedata\r\n{args.wave_id} {t0!r}\r\n".encode()
        sock.sendto(msg, (args.host, args.port))
        rec.add("tx", f"{args.host}:{args.port}", msg, f"probe {i}")
        try:
            data, peer = sock.recvfrom(65536)
        except (socket.timeout, TimeoutError):
            rec.add("rx", "", b"", f"probe {i} timed out")
            continue
        t3 = now()
        parts = data.decode(errors="replace").split()
        note = f"probe {i}"
        if len(parts) >= 4:
            wave, r0, t1, t2 = parts[0], float(parts[1]), float(parts[2]), float(parts[3])
            # src/time_receiver.cpp:170-178
            rtt = (t3 - r0) - (t2 - t1)
            offset = ((t1 - r0) + (t2 - t3)) / 2
            note = f"probe {i} wave={wave} rtt={rtt:.6f} offset={offset:.6f}"
        rec.add("rx", f"{peer[0]}:{peer[1]}", data, note)
        time.sleep(args.interval)

    path = rec.save(args.out)
    print(f"saved   : {path}")
    return 0


# --------------------------------------------------------------------------
# data feed
# --------------------------------------------------------------------------

def cmd_feed(args):
    """Open a data feed and record the handshake plus the first bytes.

    Request line, from src/data_receiver.cpp:170-190. The header block ends with
    a blank line. The reply carries UID, Suppress-Subnormals, and
    Data-Protocol-Version (src/tcp_server.cpp:674-677).
    """
    rec = Recorder(args.name)
    sock = socket.create_connection((args.host, args.port), timeout=args.timeout)

    req = (
        f"LSL:streamfeed/{args.version} {args.uid}\r\n"
        "Native-Byte-Order: 1234\r\n"
        "Endian-Performance: 0\r\n"
        "Has-IEEE754-Floats: 1\r\n"
        "Supports-Subnormals: 1\r\n"
        f"Value-Size: {args.value_size}\r\n"
        f"Data-Protocol-Version: {args.version}\r\n"
        f"Max-Buffer-Length: {args.max_buflen}\r\n"
        f"Max-Chunk-Length: {args.max_chunklen}\r\n"
        "Hostname: capture-rig\r\n"
        "Source-Id: capture-rig\r\n"
        "Session-Id: default\r\n"
        "\r\n"
    ).encode()

    sock.sendall(req)
    rec.add("tx", f"{args.host}:{args.port}", req, "streamfeed request")

    sock.settimeout(args.timeout)
    total = b""
    deadline = now() + args.duration
    while now() < deadline and len(total) < args.max_bytes:
        try:
            chunk = sock.recv(65536)
        except (socket.timeout, TimeoutError):
            break
        if not chunk:
            break
        total += chunk
        rec.add("rx", f"{args.host}:{args.port}", chunk, "feed bytes")

    sock.close()
    path = rec.save(args.out)
    print(f"received: {len(total)} bytes")
    print(f"saved   : {path}")
    return 0


def main():
    p = argparse.ArgumentParser(description=__doc__,
                                formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--out", default="captures", help="output directory")
    sub = p.add_subparsers(dest="cmd", required=True)

    d = sub.add_parser("discover", help="send a shortinfo query")
    d.add_argument("--query", default="", help="XPath query, empty matches every stream")
    d.add_argument("--query-id", default="captureprobe0001")
    d.add_argument("--timeout", type=float, default=2.0)
    d.add_argument("--port-span", type=int, default=PORT_RANGE)
    d.add_argument("--extra-host", action="append", default=[])
    d.add_argument("--name", default="discover")
    d.add_argument("--allow-empty", action="store_true")
    d.set_defaults(func=cmd_discover)

    t = sub.add_parser("timeprobe", help="send time probes")
    t.add_argument("--host", required=True)
    t.add_argument("--port", type=int, required=True)
    t.add_argument("--wave-id", type=int, default=12345)
    t.add_argument("--count", type=int, default=5)
    t.add_argument("--interval", type=float, default=0.1)
    t.add_argument("--timeout", type=float, default=2.0)
    t.add_argument("--name", default="timeprobe")
    t.set_defaults(func=cmd_timeprobe)

    f = sub.add_parser("feed", help="open a data feed")
    f.add_argument("--host", required=True)
    f.add_argument("--port", type=int, required=True)
    f.add_argument("--uid", required=True)
    f.add_argument("--version", type=int, default=110)
    f.add_argument("--value-size", type=int, default=4)
    f.add_argument("--max-buflen", type=int, default=360)
    f.add_argument("--max-chunklen", type=int, default=0)
    f.add_argument("--duration", type=float, default=2.0)
    f.add_argument("--max-bytes", type=int, default=65536)
    f.add_argument("--timeout", type=float, default=2.0)
    f.add_argument("--name", default="feed")
    f.set_defaults(func=cmd_feed)

    args = p.parse_args()
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
