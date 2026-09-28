#!/usr/bin/env python3
"""Record what liblsl does under stress.

The interop matrix in `interop.py` runs one narrow path: twelve samples, one
consumer, post-processing off, and no buffer ever fills. Three subsystems of
liblsl therefore have no conformance evidence at all.

This tool records the behavior of those three, against the reference and
nothing else. Each recording becomes the specification that a rewrite has to
match. Until a recording exists, an implementation of `consumer_queue` or
`inlet_connection` would be written blind.

Every sample carries its own index in channel 0, so a drop is visible by
reading the received list. A gap in the indices names exactly which samples
went missing and where.

Scenarios:
  backpressure   a fast writer and a slow reader with a small buffer
  multiconsumer  one outlet feeding several inlets at different speeds
  recovery       an outlet that stops and starts again with a new identifier
  chunking       the effect of Max-Chunk-Length on the bytes that arrive
"""

import argparse
import json
import pathlib
import re
import socket
import struct
import subprocess
import sys
import time

BASE_PORT = 16572


def bits(x):
    return struct.unpack("<Q", struct.pack("<d", x))[0]


def write_indexed_samples(path, count, channels):
    """Write samples whose first channel holds the sample index.

    A received list then names the samples that arrived, so a drop shows up as
    a gap in the numbers instead of as a count that does not add up.
    """
    with open(path, "w") as f:
        for i in range(count):
            ts = 1000.0 + i * 0.001
            toks = [f"{struct.unpack('<I', struct.pack('<f', float(i)))[0]:x}"]
            for k in range(1, channels):
                toks.append(f"{struct.unpack('<I', struct.pack('<f', float(k)))[0]:x}")
            f.write(f"{bits(ts):x} " + " ".join(toks) + "\n")


def read_indices(path):
    """Read the sample indices out of a received file."""
    out = []
    try:
        with open(path) as f:
            for line in f:
                parts = line.split()
                if len(parts) < 2:
                    continue
                raw = int(parts[1], 16)
                out.append(int(struct.unpack("<f", struct.pack("<I", raw))[0]))
    except FileNotFoundError:
        pass
    return out


def wait_for(proc, marker, timeout, what):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        line = proc.stderr.readline()
        if not line:
            raise RuntimeError(f"{what}: the peer stopped before {marker}")
        line = line.strip()
        if line.startswith("ERROR"):
            raise RuntimeError(f"{what}: {line}")
        if line.startswith(marker):
            return line
    raise RuntimeError(f"{what}: timed out waiting for {marker}")


def stop(*procs):
    for p in procs:
        if p is None:
            continue
        p.terminate()
        try:
            p.wait(timeout=5)
        except subprocess.TimeoutExpired:
            p.kill()


def describe(indices, sent):
    """Summarize a received list against what went out."""
    if not indices:
        return {"received": 0, "note": "nothing arrived"}
    gaps = []
    for a, b in zip(indices, indices[1:]):
        if b != a + 1:
            gaps.append({"after": a, "next": b, "missing": b - a - 1})
    return {
        "received": len(indices),
        "sent": sent,
        "first": indices[0],
        "last": indices[-1],
        "in_order": all(b > a for a, b in zip(indices, indices[1:])),
        "gap_count": len(gaps),
        "missing_total": sum(g["missing"] for g in gaps),
        "gaps": gaps[:8],
    }


# ---------------------------------------------------------------------------
# backpressure
# ---------------------------------------------------------------------------

def scenario_backpressure(peer, work, buflen=1, count=600, channels=2):
    """A fast writer and a slow reader with a small buffer.

    The question this answers: when the buffer fills, which samples survive?
    A ring that drops the oldest keeps the newest, so the reader sees a late
    range with a gap at the front. A design that blocks the writer instead
    would show every sample and a long run time.

    The stream carries no rate. `Max-Buffer-Length` is a count of seconds when
    a rate exists, so a rate would make the buffer far too large to overflow.
    With no rate the conversion multiplies by 100, so a request of 1 gives a
    buffer of 100 samples (`src/stream_info_impl.cpp:254-270`).
    """
    sent = pathlib.Path(work) / "bp_sent.txt"
    got = pathlib.Path(work) / "bp_got.txt"
    write_indexed_samples(sent, count, channels)
    name = "BpTest"

    out = subprocess.Popen(
        [peer, "outlet", "--name", name, "--format", "float32", "--channels", str(channels),
         "--srate", "0", "--input", str(sent), "--push-delay-ms", "0"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    inp = None
    try:
        wait_for(out, "READY", 20, "outlet")
        inp = subprocess.Popen(
            [peer, "inlet", "--name", name, "--format", "float32", "--channels", str(channels),
             "--count", str(count), "--output", str(got),
             "--buflen", str(buflen), "--pull-delay-ms", "20"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        wait_for(inp, "READY", 25, "inlet")
        out.stdin.write("GO\n")
        out.stdin.flush()
        started = time.monotonic()
        wait_for(out, "PUSHED", 60, "outlet")
        push_seconds = time.monotonic() - started
        # Give the reader time to drain whatever survived.
        time.sleep(3.0)
    finally:
        stop(inp, out)

    idx = read_indices(got)
    r = describe(idx, count)
    r["buflen_requested"] = buflen
    r["buffer_samples_expected"] = buflen * 100  # no rate, so the factor is 100
    r["push_seconds"] = round(push_seconds, 3)
    # A writer that never waited for the reader finishes far faster than the
    # reader can consume. That is the signature of a policy that drops.
    r["writer_blocked"] = push_seconds > count * 0.02 * 0.5
    return r


# ---------------------------------------------------------------------------
# several consumers
# ---------------------------------------------------------------------------

def scenario_multiconsumer(peer, work, count=400, channels=2):
    """One outlet feeding a fast reader and a slow reader.

    The question: does a slow consumer hold back a fast one? liblsl gives each
    consumer its own queue, so the fast reader must see every sample even while
    the slow reader loses some.
    """
    sent = pathlib.Path(work) / "mc_sent.txt"
    fast = pathlib.Path(work) / "mc_fast.txt"
    slow = pathlib.Path(work) / "mc_slow.txt"
    write_indexed_samples(sent, count, channels)
    name = "McTest"

    out = subprocess.Popen(
        [peer, "outlet", "--name", name, "--format", "float32", "--channels", str(channels),
         "--srate", "0", "--input", str(sent), "--push-delay-ms", "2"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    a = b = None
    try:
        wait_for(out, "READY", 20, "outlet")
        a = subprocess.Popen(
            [peer, "inlet", "--name", name, "--format", "float32", "--channels", str(channels),
             "--count", str(count), "--output", str(fast), "--buflen", "3600"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        wait_for(a, "READY", 25, "fast inlet")
        b = subprocess.Popen(
            [peer, "inlet", "--name", name, "--format", "float32", "--channels", str(channels),
             "--count", str(count), "--output", str(slow), "--buflen", "1",
             "--pull-delay-ms", "40"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        wait_for(b, "READY", 25, "slow inlet")

        out.stdin.write("GO\n")
        out.stdin.flush()
        wait_for(out, "PUSHED", 60, "outlet")
        time.sleep(4.0)
    finally:
        stop(a, b, out)

    return {
        "fast": describe(read_indices(fast), count),
        "slow": describe(read_indices(slow), count),
    }


# ---------------------------------------------------------------------------
# recovery
# ---------------------------------------------------------------------------

def scenario_recovery(peer, work, count=300, channels=2):
    """An outlet that stops and starts again with a new identifier.

    The question: does an inlet with recovery enabled reconnect on its own, and
    does it keep delivering samples from the new instance?

    The stream keeps its name and its source identifier across the restart. A
    stream with no source identifier cannot be recovered at all, and liblsl
    warns about that.
    """
    sent = pathlib.Path(work) / "rc_sent.txt"
    got = pathlib.Path(work) / "rc_got.txt"
    write_indexed_samples(sent, count, channels)
    name = "RcTest"
    args_out = [peer, "outlet", "--name", name, "--format", "float32",
                "--channels", str(channels), "--srate", "200", "--input", str(sent),
                "--source-id", "rc_fixed_source", "--push-delay-ms", "10"]

    out = subprocess.Popen(args_out, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                           stderr=subprocess.PIPE, text=True)
    inp = None
    uid_first = uid_second = None
    try:
        uid_first = wait_for(out, "READY", 20, "outlet").split()[-1]
        inp = subprocess.Popen(
            [peer, "inlet", "--name", name, "--format", "float32", "--channels", str(channels),
             "--count", str(count), "--output", str(got), "--buflen", "3600",
             "--recover", "1", "--timeout", "40"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        wait_for(inp, "READY", 25, "inlet")

        out.stdin.write("GO\n")
        out.stdin.flush()
        time.sleep(1.2)
        before = len(read_indices(got))

        # Stop the source. The inlet now has nothing to read.
        stop(out)
        time.sleep(1.5)
        during = len(read_indices(got))

        # Start it again. The name and the source identifier match, and the
        # instance identifier is new.
        out = subprocess.Popen(args_out, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                               stderr=subprocess.PIPE, text=True)
        uid_second = wait_for(out, "READY", 20, "outlet 2").split()[-1]
        out.stdin.write("GO\n")
        out.stdin.flush()
        # Recovery uses a watchdog, so allow it time to notice and reconnect.
        time.sleep(18.0)
        after = len(read_indices(got))
    finally:
        stop(inp, out)

    # The inlet's own account of what happened, for a run that did not recover.
    try:
        inlet_err = inp.stderr.read() if inp is not None else ""
    except Exception:
        inlet_err = ""
    idx = read_indices(got)
    return {
        "inlet_stderr": inlet_err[-2000:],
        "uid_before": uid_first,
        "uid_after": uid_second,
        "uid_changed": uid_first != uid_second,
        "received_before_stop": before,
        "received_after_stop": during,
        "received_after_restart": after,
        "recovered": after > during,
        "summary": describe(idx, count),
    }


# ---------------------------------------------------------------------------
# chunking
# ---------------------------------------------------------------------------

def resolve_raw(name, timeout=6.0):
    rx = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    rx.bind(("", 0))
    port = rx.getsockname()[1]
    msg = f"LSL:shortinfo\r\nname='{name}'\r\n{port} chprobe\r\n".encode()
    tx = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    tx.setsockopt(socket.SOL_SOCKET, socket.SO_BROADCAST, 1)
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        for p in range(BASE_PORT, BASE_PORT + 4):
            for a in ("127.0.0.1", "255.255.255.255"):
                try:
                    tx.sendto(msg, (a, p))
                except OSError:
                    pass
        rx.settimeout(0.4)
        try:
            data, _ = rx.recvfrom(65536)
        except (socket.timeout, TimeoutError):
            continue
        head, sep, body = data.partition(b"\r\n")
        if not sep or head.decode(errors="replace").strip() != "chprobe":
            continue
        text = body.decode(errors="replace")
        return {
            "uid": re.search(r"<uid>(.*?)</uid>", text).group(1),
            "port": int(re.search(r"<v4data_port>(\d+)</v4data_port>", text).group(1)),
        }
    return None


def scenario_chunking(peer, work, chunklen, count=120, channels=2):
    """The effect of Max-Chunk-Length on the bytes that arrive.

    A chunk is not a wire unit. SPEC.md section 9 says so. The observable is
    the size of each socket write, so this scenario reads the raw stream and
    records how many bytes arrive per read.

    The writer must not set the push-through flag. That flag forces a write on
    every sample, and it overrides the chunk length entirely
    (`src/tcp_server.cpp:782`). A first run with the flag set showed the same
    read sizes for every chunk length, which is the flag winning.
    """
    sent = pathlib.Path(work) / f"ck{chunklen}_sent.txt"
    write_indexed_samples(sent, count, channels)
    name = f"CkTest{chunklen}"

    out = subprocess.Popen(
        [peer, "outlet", "--name", name, "--format", "float32", "--channels", str(channels),
         "--srate", "1000", "--input", str(sent), "--push-delay-ms", "0",
         "--pushthrough", "0"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    try:
        wait_for(out, "READY", 20, "outlet")
        found = resolve_raw(name)
        if not found:
            raise RuntimeError("could not resolve")

        s = socket.create_connection(("127.0.0.1", found["port"]), timeout=5)
        req = (
            f"LSL:streamfeed/110 {found['uid']}\r\n"
            "Native-Byte-Order: 1234\r\nEndian-Performance: 1000000\r\n"
            "Has-IEEE754-Floats: 1\r\nSupports-Subnormals: 1\r\nValue-Size: 4\r\n"
            "Data-Protocol-Version: 110\r\nMax-Buffer-Length: 3600\r\n"
            f"Max-Chunk-Length: {chunklen}\r\n"
            "Hostname: chunkrig\r\nSource-Id: chunkrig\r\nSession-Id: default\r\n\r\n"
        ).encode()
        s.sendall(req)
        out.stdin.write("GO\n")
        out.stdin.flush()

        s.settimeout(0.5)
        reads = []
        total = b""
        end = time.monotonic() + 6.0
        while time.monotonic() < end:
            try:
                chunk = s.recv(65536)
            except (socket.timeout, TimeoutError):
                continue
            if not chunk:
                break
            reads.append(len(chunk))
            total += chunk
        s.close()
    finally:
        stop(out)

    head, _, body = total.partition(b"\r\n\r\n")
    sample_bytes = 1 + 8 + 4 * channels
    return {
        "chunklen_requested": chunklen,
        "sample_bytes": sample_bytes,
        "body_bytes": len(body),
        "whole_samples": len(body) // sample_bytes,
        "read_count": len(reads),
        "read_sizes_first": reads[:12],
        "mean_read": round(sum(reads) / len(reads), 1) if reads else 0,
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--peer", default=".build/peer")
    ap.add_argument("--out", default="artifacts/behavior-liblsl.json")
    ap.add_argument("--only", default=None)
    args = ap.parse_args()

    work = pathlib.Path("artifacts/behavior-work")
    work.mkdir(parents=True, exist_ok=True)
    results = {"oracle_commit": "e651023ca67996a05a028fd88a28603297120294"}

    todo = args.only.split(",") if args.only else [
        "backpressure", "multiconsumer", "recovery", "chunking"]

    if "backpressure" in todo:
        print("=== backpressure: a fast writer, a slow reader, a buffer of 20 ===")
        r = scenario_backpressure(args.peer, work)
        results["backpressure"] = r
        print(f"  sent {r.get('sent')}, received {r['received']}, "
              f"first {r.get('first')}, last {r.get('last')}")
        print(f"  gaps {r.get('gap_count')}, missing {r.get('missing_total')}, "
              f"writer blocked: {r.get('writer_blocked')}")

    if "multiconsumer" in todo:
        print("\n=== several consumers: one fast, one slow ===")
        r = scenario_multiconsumer(args.peer, work)
        results["multiconsumer"] = r
        print(f"  fast: received {r['fast']['received']}, gaps {r['fast'].get('gap_count')}")
        print(f"  slow: received {r['slow']['received']}, gaps {r['slow'].get('gap_count')}")

    if "recovery" in todo:
        print("\n=== recovery: the outlet stops and starts with a new identifier ===")
        r = scenario_recovery(args.peer, work)
        results["recovery"] = r
        print(f"  uid changed: {r['uid_changed']}")
        print(f"  received before {r['received_before_stop']}, "
              f"while down {r['received_after_stop']}, after restart {r['received_after_restart']}")
        print(f"  recovered: {r['recovered']}")

    if "chunking" in todo:
        print("\n=== chunking: the size of each socket read ===")
        results["chunking"] = []
        for ck in (0, 1, 16):
            r = scenario_chunking(args.peer, work, ck)
            results["chunking"].append(r)
            print(f"  Max-Chunk-Length {ck:3}: {r['whole_samples']:4} samples in "
                  f"{r['read_count']:4} reads, mean {r['mean_read']} bytes")

    pathlib.Path(args.out).parent.mkdir(parents=True, exist_ok=True)
    pathlib.Path(args.out).write_text(json.dumps(results, indent=1))
    print(f"\nwrote {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
