#!/usr/bin/env python3
"""Cut the connection of a labstream inlet, against outlets of each version.

`docs/extensions.md`, section `resume`, conformance items 3, 4, and 9. The
inlet is the library under test. It reads through a TCP proxy that cuts each
connection and refuses new ones for some seconds. The outlet is
`oracle/publish.c`, linked to one library for each run:

- liblsl and labstream 0.1.2 do not number their samples. The inlet loses
  the samples of the dropout, and both counts are unknown (-1).
- labstream with `seq/1` only numbers its samples. The inlet loses the
  samples of the dropout, and the loss count is exact. The resend count is
  unknown.
- labstream with `resume/1` sends the samples of the dropout again. The inlet
  gets each sample once, the loss count is 0, and the resend count covers
  the dropout.

The last channel of each sample holds its index, so the script can see each
missing sample.

Usage:
    oracle/resumeprobe.py --inlet DIR --outlet NAME=KIND=DIR [--outlet ...]

KIND is `none`, `seq`, or `resume`: what the outlet library supports.
"""

import argparse
import ctypes
import os
import re
import socket
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CHANNELS = 4
RATE = 500.0
CUT_SECONDS = 3.0


def build(lib_dir: Path, out: Path) -> None:
    subprocess.run(["cc", "-I", str(ROOT / ".build" / "install" / "include"),
                    str(ROOT / "oracle" / "publish.c"), "-o", str(out), "-lm",
                    "-L", str(lib_dir), "-llsl", f"-Wl,-rpath,{lib_dir}"], check=True)


class Proxy:
    """Forward each connection to the outlet. `cut` ends them all."""

    def __init__(self, target: int):
        self.target = target
        self.listener = socket.socket()
        self.listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.listener.bind(("127.0.0.1", 0))
        self.listener.listen(8)
        self.port = self.listener.getsockname()[1]
        self.paused = False
        self.open = []
        self.lock = threading.Lock()
        threading.Thread(target=self.accept, daemon=True).start()

    def accept(self):
        while True:
            client, _ = self.listener.accept()
            if self.paused:
                client.close()
                continue
            try:
                server = socket.create_connection(("127.0.0.1", self.target), timeout=5)
            except OSError:
                client.close()
                continue
            server.settimeout(None)
            with self.lock:
                self.open += [client, server]
            for a, b in ((client, server), (server, client)):
                threading.Thread(target=self.pump, args=(a, b), daemon=True).start()

    @staticmethod
    def pump(a: socket.socket, b: socket.socket):
        try:
            while True:
                data = a.recv(65536)
                if not data:
                    break
                b.sendall(data)
        except OSError:
            pass
        for s in (a, b):
            try:
                s.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass

    def cut_for(self, seconds: float):
        self.paused = True
        with self.lock:
            for s in self.open:
                try:
                    s.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass
            self.open = []
        time.sleep(seconds)
        self.paused = False


def load(lib_dir: Path) -> ctypes.CDLL:
    lib = ctypes.CDLL(str(lib_dir / "liblsl.so"))
    vp, c, d, i = ctypes.c_void_p, ctypes.c_char_p, ctypes.c_double, ctypes.c_int
    lib.lsl_resolve_byprop.argtypes = [ctypes.POINTER(vp), i, c, c, i, d]
    lib.lsl_get_xml.argtypes = [vp]
    lib.lsl_get_xml.restype = ctypes.c_void_p
    lib.lsl_streaminfo_from_xml.argtypes = [c]
    lib.lsl_streaminfo_from_xml.restype = vp
    lib.lsl_create_inlet.argtypes = [vp, i, i, i]
    lib.lsl_create_inlet.restype = vp
    lib.lsl_open_stream.argtypes = [vp, d, ctypes.POINTER(ctypes.c_int32)]
    lib.lsl_pull_sample_f.argtypes = [vp, ctypes.POINTER(ctypes.c_float), i, d,
                                      ctypes.POINTER(ctypes.c_int32)]
    lib.lsl_pull_sample_f.restype = d
    lib.labstream_samples_lost.argtypes = [vp]
    lib.labstream_samples_lost.restype = ctypes.c_int64
    lib.labstream_samples_resent.argtypes = [vp]
    lib.labstream_samples_resent.restype = ctypes.c_int64
    lib.lsl_destroy_inlet.argtypes = [vp]
    return lib


def run(lib, name: str, kind: str, outlet_dir: Path, work: Path) -> bool:
    binary = work / f"publish_{name}"
    build(outlet_dir, binary)
    stream = f"ResumeProbe{os.getpid()}{name}"
    pub = subprocess.Popen([str(binary), "--name", stream, "--channels", str(CHANNELS),
                            "--rate", str(RATE), "--seconds", "60"],
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        time.sleep(1.0)
        found = (ctypes.c_void_p * 1)()
        if lib.lsl_resolve_byprop(found, 1, b"name", stream.encode(), 1, 10.0) < 1:
            print(f"  {name:<8} FAIL  the stream was not found")
            return False
        xml = ctypes.string_at(lib.lsl_get_xml(found[0])).decode()
        port = int(re.search(r"<v4data_port>(\d+)</v4data_port>", xml).group(1))
        proxy = Proxy(port)
        xml = re.sub(r"<v4data_port>\d+</v4data_port>",
                     f"<v4data_port>{proxy.port}</v4data_port>", xml)
        xml = re.sub(r"<v4address>[^<]*</v4address>|<v4address\s*/>",
                     "<v4address>127.0.0.1</v4address>", xml)
        info = lib.lsl_streaminfo_from_xml(xml.encode())
        inlet = lib.lsl_create_inlet(info, 360, 0, 1)
        ec = ctypes.c_int32(0)
        lib.lsl_open_stream(inlet, 5.0, ctypes.byref(ec))
        if ec.value != 0:
            print(f"  {name:<8} FAIL  the inlet did not open: {ec.value}")
            return False

        got = []
        stop = threading.Event()

        def reader():
            buf = (ctypes.c_float * CHANNELS)()
            err = ctypes.c_int32(0)
            while not stop.is_set():
                t = lib.lsl_pull_sample_f(inlet, buf, CHANNELS, 0.1, ctypes.byref(err))
                if t != 0.0:
                    got.append(int(buf[CHANNELS - 1]))

        thread = threading.Thread(target=reader, daemon=True)
        thread.start()
        time.sleep(1.0)
        proxy.cut_for(CUT_SECONDS)
        time.sleep(4.0)
        stop.set()
        thread.join()
        lost = lib.labstream_samples_lost(inlet)
        resent = lib.labstream_samples_resent(inlet)
        lib.lsl_destroy_inlet(inlet)
    finally:
        pub.kill()
        pub.wait()

    first, last = got[0], got[-1]
    in_order = all(b > a for a, b in zip(got, got[1:]))
    missing = (last - first + 1) - len(set(got))
    twice = len(got) - len(set(got))
    dropout = int(RATE * (CUT_SECONDS - 1.0))
    if kind == "resume":
        ok = missing == 0 and twice == 0 and in_order and lost == 0 and resent >= dropout
    elif kind == "seq":
        ok = missing >= dropout and twice == 0 and lost == missing and resent == -1
    else:
        ok = missing >= dropout and twice == 0 and lost == -1 and resent == -1
    print(f"  {name:<8} {'pass' if ok else 'FAIL'}  {kind:<6} received {len(got)}, "
          f"missing {missing}, twice {twice}, in order {in_order}, "
          f"lost {lost}, resent {resent}")
    return ok


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--inlet", required=True, help="the directory of the inlet library")
    ap.add_argument("--outlet", action="append", required=True, metavar="NAME=KIND=DIR")
    args = ap.parse_args()
    lib = load(Path(args.inlet).resolve())
    work = Path(tempfile.mkdtemp(prefix="resumeprobe-"))
    results = []
    for item in args.outlet:
        name, kind, path = item.split("=", 2)
        results.append(run(lib, name, kind, Path(path).resolve(), work))
    print()
    if all(results):
        print("each outlet gave the result of its version")
        return 0
    print("an outlet did not give the result of its version")
    return 1


if __name__ == "__main__":
    sys.exit(main())
