#!/usr/bin/env python3
"""End-to-end container fallback test for Step 9 (`shm/1`, Criterion 3).

Runs a `labstream` outlet in one mount namespace and a `labstream` inlet in a
second mount namespace with an isolated `/dev/shm` (`unshare -Urm` +
`mount -t tmpfs tmpfs /dev/shm`) sharing loopback (`127.0.0.1`), proving that
even when both peers share the same hostname and loopback IP, an isolated
container `/dev/shm` fails the `shm/1` token probe, replies `shm-no`, and
streams cleanly over TCP with zero errors and zero lost samples.
"""

import ctypes
import json
import os
import subprocess
import sys
import tempfile
import time

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
LABSTREAM_LIB = os.path.join(ROOT, "target", "release", "liblsl.so")


def run_outlet(stream_name: str, count: int, ready_file: str, go_file: str):
    lib = ctypes.CDLL(LABSTREAM_LIB)
    lib.lsl_create_streaminfo.restype = ctypes.c_void_p
    lib.lsl_create_streaminfo.argtypes = [
        ctypes.c_char_p, ctypes.c_char_p, ctypes.c_int, ctypes.c_double,
        ctypes.c_int, ctypes.c_char_p,
    ]
    lib.lsl_create_outlet.restype = ctypes.c_void_p
    lib.lsl_create_outlet.argtypes = [ctypes.c_void_p, ctypes.c_int, ctypes.c_int]
    lib.lsl_wait_for_consumers.restype = ctypes.c_int
    lib.lsl_wait_for_consumers.argtypes = [ctypes.c_void_p, ctypes.c_double]
    lib.lsl_push_sample_ft.restype = ctypes.c_int
    lib.lsl_push_sample_ft.argtypes = [
        ctypes.c_void_p, ctypes.POINTER(ctypes.c_float), ctypes.c_double,
    ]
    lib.lsl_destroy_outlet.argtypes = [ctypes.c_void_p]
    lib.lsl_destroy_streaminfo.argtypes = [ctypes.c_void_p]

    info = lib.lsl_create_streaminfo(
        stream_name.encode(), b"EEG", 8, 250.0, 1, b"shm-container-src"
    )
    outlet = lib.lsl_create_outlet(info, 0, 360)
    assert outlet, "failed to create outlet"
    with open(ready_file, "w") as f:
        f.write("READY\n")
    assert lib.lsl_wait_for_consumers(outlet, 15.0), "inlet did not connect"
    deadline = time.monotonic() + 10.0
    while not os.path.exists(go_file) and time.monotonic() < deadline:
        time.sleep(0.02)

    buf = (ctypes.c_float * 8)()
    for k in range(count):
        buf[0] = float(k)
        for c in range(1, 8):
            buf[c] = float(c) * 1.5
        lib.lsl_push_sample_ft(outlet, buf, 500.0 + k / 250.0)
        if k % 25 == 0:
            time.sleep(0.002)

    time.sleep(0.5)
    lib.lsl_destroy_outlet(outlet)
    lib.lsl_destroy_streaminfo(info)


def run_inlet(stream_name: str, count: int, go_file: str):
    lib = ctypes.CDLL(LABSTREAM_LIB)
    lib.lsl_resolve_byprop.restype = ctypes.c_int
    lib.lsl_resolve_byprop.argtypes = [
        ctypes.POINTER(ctypes.c_void_p), ctypes.c_uint32,
        ctypes.c_char_p, ctypes.c_char_p, ctypes.c_int, ctypes.c_double,
    ]
    lib.lsl_create_inlet.restype = ctypes.c_void_p
    lib.lsl_create_inlet.argtypes = [ctypes.c_void_p, ctypes.c_int, ctypes.c_int, ctypes.c_int]
    lib.lsl_open_stream.argtypes = [ctypes.c_void_p, ctypes.c_double, ctypes.POINTER(ctypes.c_int)]
    lib.labstream_inlet_on_shm.restype = ctypes.c_int
    lib.labstream_inlet_on_shm.argtypes = [ctypes.c_void_p]
    lib.labstream_samples_lost.restype = ctypes.c_int64
    lib.labstream_samples_lost.argtypes = [ctypes.c_void_p]
    lib.lsl_pull_sample_f.restype = ctypes.c_double
    lib.lsl_pull_sample_f.argtypes = [
        ctypes.c_void_p, ctypes.POINTER(ctypes.c_float), ctypes.c_int,
        ctypes.c_double, ctypes.POINTER(ctypes.c_int),
    ]
    lib.lsl_destroy_inlet.argtypes = [ctypes.c_void_p]
    lib.lsl_destroy_streaminfo.argtypes = [ctypes.c_void_p]

    found = (ctypes.c_void_p * 1)()
    n = lib.lsl_resolve_byprop(found, 1, b"name", stream_name.encode(), 1, 15.0)
    assert n == 1, f"failed to resolve {stream_name}"

    inlet = lib.lsl_create_inlet(found[0], 360, 0, 0)
    ec = ctypes.c_int(0)
    lib.lsl_open_stream(inlet, 10.0, ctypes.byref(ec))
    assert ec.value == 0, f"open_stream failed: {ec.value}"
    on_shm = lib.labstream_inlet_on_shm(inlet)
    with open(go_file, "w") as f:
        f.write("GO\n")

    buf = (ctypes.c_float * 8)()
    received = 0
    bad = 0
    deadline = time.monotonic() + 10.0
    while received < count and time.monotonic() < deadline:
        ts = lib.lsl_pull_sample_f(inlet, buf, 8, 1.0, ctypes.byref(ec))
        if ts == 0.0:
            continue
        if int(round(buf[0])) != received or abs(ts - (500.0 + received / 250.0)) > 1e-9:
            bad += 1
        received += 1

    lost = lib.labstream_samples_lost(inlet)
    lib.lsl_destroy_inlet(inlet)
    lib.lsl_destroy_streaminfo(found[0])

    print(json.dumps({
        "on_shm": on_shm,
        "received": received,
        "expected": count,
        "bad": bad,
        "lost": lost,
    }))
    sys.stdout.flush()


def main():
    if not os.path.exists(LABSTREAM_LIB):
        sys.exit(f"missing release library: {LABSTREAM_LIB}")
    script = os.path.abspath(__file__)
    stream = f"ShmContainer_{os.getpid()}"
    count = 200

    with tempfile.TemporaryDirectory(prefix="labstream-shm-container-") as tmp:
        ready = os.path.join(tmp, "ready")
        go = os.path.join(tmp, "go")
        out_json = os.path.join(tmp, "result.json")

        # Outer user+mount+net namespace runs the outlet on loopback.
        # Inner mount namespace (`unshare -m`) mounts a fresh private `/dev/shm`
        # (simulating a container) while sharing loopback with the outlet.
        bash = f"""
set -euo pipefail
ip link set lo up
python3 {script} --role outlet --name {stream} --count {count} --ready {ready} --go {go} &
PID_OUT=$!
for _ in $(seq 1 50); do [ -f {ready} ] && break; sleep 0.05; done

unshare -m bash -c '
  mount -t tmpfs tmpfs /dev/shm
  python3 {script} --role inlet --name {stream} --count {count} --go {go}
' > {out_json}

wait $PID_OUT
"""
        proc = subprocess.run(
            ["unshare", "-Urnm", "bash", "-c", bash],
            capture_output=True,
            text=True,
            timeout=30,
        )
        if proc.returncode != 0:
            print("STDOUT:", proc.stdout)
            print("STDERR:", proc.stderr)
            sys.exit(proc.returncode)

        with open(out_json) as f:
            res = json.loads(f.read().strip())
        print("Step 9 isolated /dev/shm container fallback result:", json.dumps(res, indent=2))
        assert res["on_shm"] == 0, f"expected TCP fallback (on_shm=0), got {res}"
        assert res["received"] == count, f"expected {count}, got {res['received']}"
        assert res["bad"] == 0, f"corrupted samples: {res['bad']}"
        assert res["lost"] == 0, f"lost samples: {res['lost']}"
        print("PASS: container with isolated /dev/shm fell back cleanly to TCP with 0 errors and 0 loss!")


if __name__ == "__main__":
    if "--role" in sys.argv:
        role = sys.argv[sys.argv.index("--role") + 1]
        if role == "outlet":
            run_outlet(
                sys.argv[sys.argv.index("--name") + 1],
                int(sys.argv[sys.argv.index("--count") + 1]),
                sys.argv[sys.argv.index("--ready") + 1],
                sys.argv[sys.argv.index("--go") + 1],
            )
        elif role == "inlet":
            run_inlet(
                sys.argv[sys.argv.index("--name") + 1],
                int(sys.argv[sys.argv.index("--count") + 1]),
                sys.argv[sys.argv.index("--go") + 1],
            )
    else:
        main()
