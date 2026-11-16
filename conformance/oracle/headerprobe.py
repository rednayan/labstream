#!/usr/bin/env python3
"""Make sure that an outlet ignores the `Labstream-Features` request header.

`docs/extensions.md` proposes this header. The proposal depends on one fact:
an outlet that does not know the header gives the same answer with it and
without it. This probe measures that fact on a real outlet.

The probe starts `oracle/publish.c`, linked to one library. It opens two feed
connections by hand. One request has the extra header, and one does not.
The answer headers and the two test pattern samples must be the same, byte
for byte.

Usage:
    oracle/headerprobe.py                      # liblsl and the Rust library
    oracle/headerprobe.py --lib oracle=.build/install/lib
"""

import argparse
import os
import re
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CHANNELS = 8

# The request that liblsl sends (`src/data_receiver.cpp:170-190`), with fixed
# values.
BASE_HEADERS = [
    "Native-Byte-Order: 1234",
    "Endian-Performance: 0",
    "Has-IEEE754-Floats: 1",
    "Supports-Subnormals: 1",
    "Value-Size: 4",
    "Data-Protocol-Version: 110",
    "Max-Buffer-Length: 360",
    "Max-Chunk-Length: 0",
    "Hostname: probe",
    "Source-Id: ",
    "Session-Id: default",
]
EXTRA = "Labstream-Features: seq/1 shm/1=0a1b2c"


def build(lib_dir: Path, out: Path) -> None:
    subprocess.run(["cc", "-I", str(ROOT / ".build" / "install" / "include"),
                    str(ROOT / "oracle" / "publish.c"), "-o", str(out), "-lm",
                    "-L", str(lib_dir), "-llsl", f"-Wl,-rpath,{lib_dir}"], check=True)


def find_stream(lib_dir: Path, name: str) -> tuple:
    """Resolve the stream with pylsl and give its uid and IPv4 data port."""
    code = (
        "import pylsl,sys\n"
        f"r=pylsl.resolve_byprop('name','{name}',1,10)\n"
        "print(r[0].as_xml() if r else '')\n"
    )
    lib = next(p for p in sorted(lib_dir.glob("liblsl.so*")))
    env = dict(os.environ, PYLSL_LIB=str(lib))
    xml = subprocess.run([sys.executable, "-c", code], env=env, capture_output=True,
                         text=True, timeout=30).stdout
    uid = re.search(r"<uid>([^<]*)</uid>", xml)
    port = re.search(r"<v4data_port>(\d+)</v4data_port>", xml)
    if not uid or not port:
        raise RuntimeError("the stream was not found")
    return uid.group(1), int(port.group(1))


def read_exact(sock: socket.socket, n: int) -> bytes:
    out = b""
    while len(out) < n:
        part = sock.recv(n - len(out))
        if not part:
            raise RuntimeError(f"the connection closed after {len(out)} bytes")
        out += part
    return out


def feed(port: int, uid: str, extra: bool) -> tuple:
    """Send one feed request. Give the answer headers and the test pattern."""
    lines = [f"LSL:streamfeed/110 {uid}", *BASE_HEADERS]
    if extra:
        lines.append(EXTRA)
    request = "\r\n".join(lines) + "\r\n\r\n"
    with socket.create_connection(("127.0.0.1", port), timeout=5) as s:
        s.sendall(request.encode())
        head = b""
        while b"\r\n\r\n" not in head:
            part = s.recv(1)
            if not part:
                raise RuntimeError("the connection closed in the headers")
            head += part
        # Each test pattern sample: a tag, a timestamp, and the values.
        # SPEC.md 6.4.
        pattern = read_exact(s, 2 * (1 + 8 + 4 * CHANNELS))
    return head.decode(errors="replace"), pattern


def probe(name: str, lib_dir: Path, work: Path) -> bool:
    binary = work / f"publish_{name}"
    build(lib_dir, binary)
    stream = f"HeaderProbe{os.getpid()}{name}"
    pub = subprocess.Popen([str(binary), "--name", stream, "--channels", str(CHANNELS),
                            "--rate", "100", "--seconds", "20"],
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        time.sleep(1.0)
        uid, port = find_stream(lib_dir, stream)
        plain_head, plain_pattern = feed(port, uid, extra=False)
        with_head, with_pattern = feed(port, uid, extra=True)
    finally:
        pub.kill()
        pub.wait()
    same = plain_head == with_head and plain_pattern == with_pattern
    leaked = "labstream-features" in with_head.lower()
    ok = same and not leaked
    print(f"  {name:<8} {'pass' if ok else 'FAIL'}  answer headers "
          f"{'same' if plain_head == with_head else 'DIFFERENT'}, test pattern "
          f"{'same' if plain_pattern == with_pattern else 'DIFFERENT'}, "
          f"{'a Labstream-Features answer' if leaked else 'no Labstream-Features answer'}")
    if not ok:
        print("    without:", plain_head.replace("\r\n", " | "))
        print("    with:   ", with_head.replace("\r\n", " | "))
    return ok


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--lib", action="append", default=None, metavar="NAME=DIR")
    args = ap.parse_args()
    libs = args.lib or [f"oracle={ROOT / '.build' / 'install' / 'lib'}",
                        f"rust={ROOT / '.build' / 'rustlib'}"]
    work = Path(tempfile.mkdtemp(prefix="headerprobe-"))
    results = []
    for item in libs:
        name, _, path = item.partition("=")
        results.append(probe(name, Path(path).resolve(), work))
    print()
    if all(results):
        print("each outlet ignores the Labstream-Features header")
        return 0
    print("an outlet reacts to the Labstream-Features header")
    return 1


if __name__ == "__main__":
    sys.exit(main())
