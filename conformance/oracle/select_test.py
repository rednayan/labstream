#!/usr/bin/env python3
"""End-to-end conformance test for Step 6 (`select/1`, Criterion 4).

Runs a 4000 Hz x 64-channel `Float32` `labstream` outlet (~8.5 Mbit/s raw) in
one Linux network namespace, connects a `labstream` inlet with `decimate = 16`
(250 Hz preview, ~0.53 Mbit/s) from a second network namespace across a `veth`
pair rate-limited to `1mbit` with `tc netem`, and verifies:

1. `labstream_samples_lost(inlet) == 0` and `dropped == 0` over 1 Mbit/s.
2. `lsl_get_fullinfo` reports `nominal_srate == 250.0` and `channel_count == 64`.
3. Every decimated sample arrives in order with finite values matching the
   20 Hz passband tone and strong attenuation of the 300 Hz stopband tone.
"""

import ctypes
import json
import math
import os
import subprocess
import sys
import tempfile
import time

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
LABSTREAM_LIB = os.path.join(ROOT, "target", "release", "liblsl.so")


def run_outlet(stream_name: str, seconds: float, ready_file: str, go_file: str):
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
    lib.lsl_push_chunk_ftp.restype = ctypes.c_int
    lib.lsl_push_chunk_ftp.argtypes = [
        ctypes.c_void_p, ctypes.POINTER(ctypes.c_float),
        ctypes.c_uint64, ctypes.c_double, ctypes.c_int,
    ]
    lib.lsl_destroy_outlet.argtypes = [ctypes.c_void_p]
    lib.lsl_destroy_streaminfo.argtypes = [ctypes.c_void_p]

    channels = 64
    rate = 4000.0
    chunk = 32  # 8 ms chunks at 4000 Hz
    info = lib.lsl_create_streaminfo(
        stream_name.encode(), b"EEG", channels, rate, 1, b"select-4k-64"
    )
    outlet = lib.lsl_create_outlet(info, 0, 360)
    assert outlet, "failed to create labstream outlet"
    with open(ready_file, "w") as f:
        f.write("READY\n")
    assert lib.lsl_wait_for_consumers(outlet, 15.0), "inlet did not connect"
    go_deadline = time.monotonic() + 15.0
    while not os.path.exists(go_file) and time.monotonic() < go_deadline:
        time.sleep(0.02)

    total_samples = int(rate * seconds)
    buf = (ctypes.c_float * (chunk * channels))()
    t0 = time.monotonic()
    sent = 0
    while sent < total_samples:
        n = min(chunk, total_samples - sent)
        for k in range(n):
            idx = sent + k
            ts = 1000.0 + idx / rate
            # Channel 0: 20 Hz passband tone; Channel 1: 300 Hz stopband tone.
            v0 = math.sin(2.0 * math.pi * 20.0 * ts)
            v1 = math.sin(2.0 * math.pi * 300.0 * ts)
            base = k * channels
            buf[base] = v0
            buf[base + 1] = v1
            for c in range(2, channels):
                buf[base + c] = float(c) * 0.25
        end_ts = 1000.0 + (sent + n - 1) / rate
        lib.lsl_push_chunk_ftp(outlet, buf, n * channels, end_ts, 1)
        sent += n
        target = t0 + sent / rate
        rem = target - time.monotonic()
        if rem > 0.0005:
            time.sleep(rem)

    time.sleep(0.5)
    lib.lsl_destroy_outlet(outlet)
    lib.lsl_destroy_streaminfo(info)


def run_inlet(stream_name: str, expected_min: int, peer_ip: str, cfg_path: str, go_file: str):
    with open(cfg_path, "w") as f:
        f.write(f"[lab]\nKnownPeers = {{{peer_ip}}}\n")
    os.environ["LSLAPICFG"] = cfg_path

    lib = ctypes.CDLL(LABSTREAM_LIB)
    lib.lsl_resolve_byprop.restype = ctypes.c_int
    lib.lsl_resolve_byprop.argtypes = [
        ctypes.POINTER(ctypes.c_void_p), ctypes.c_uint32,
        ctypes.c_char_p, ctypes.c_char_p, ctypes.c_int, ctypes.c_double,
    ]
    lib.labstream_create_inlet_selected.restype = ctypes.c_void_p
    lib.labstream_create_inlet_selected.argtypes = [
        ctypes.c_void_p, ctypes.c_int, ctypes.c_int, ctypes.c_int,
        ctypes.POINTER(ctypes.c_uint32), ctypes.c_uint32, ctypes.c_uint32,
    ]
    lib.lsl_open_stream.argtypes = [ctypes.c_void_p, ctypes.c_double, ctypes.POINTER(ctypes.c_int)]
    lib.lsl_get_fullinfo.restype = ctypes.c_void_p
    lib.lsl_get_fullinfo.argtypes = [ctypes.c_void_p, ctypes.c_double, ctypes.POINTER(ctypes.c_int)]
    lib.lsl_get_channel_count.restype = ctypes.c_int
    lib.lsl_get_channel_count.argtypes = [ctypes.c_void_p]
    lib.lsl_get_nominal_srate.restype = ctypes.c_double
    lib.lsl_get_nominal_srate.argtypes = [ctypes.c_void_p]
    lib.labstream_samples_lost.restype = ctypes.c_int64
    lib.labstream_samples_lost.argtypes = [ctypes.c_void_p]
    lib.lsl_pull_chunk_f.restype = ctypes.c_uint64
    lib.lsl_pull_chunk_f.argtypes = [
        ctypes.c_void_p, ctypes.POINTER(ctypes.c_float), ctypes.POINTER(ctypes.c_double),
        ctypes.c_uint64, ctypes.c_uint64, ctypes.c_double, ctypes.POINTER(ctypes.c_int),
    ]
    lib.lsl_destroy_inlet.argtypes = [ctypes.c_void_p]
    lib.lsl_destroy_streaminfo.argtypes = [ctypes.c_void_p]

    found = (ctypes.c_void_p * 1)()
    n = lib.lsl_resolve_byprop(found, 1, b"name", stream_name.encode(), 1, 15.0)
    assert n == 1, f"failed to resolve {stream_name}"

    # All 64 channels, decimate by 16 (4000 Hz -> 250 Hz preview).
    inlet = lib.labstream_create_inlet_selected(found[0], 60, 0, 0, None, 0, 16)
    assert inlet, "failed to create selected inlet"
    ec = ctypes.c_int(0)
    lib.lsl_open_stream(inlet, 15.0, ctypes.byref(ec))
    assert ec.value == 0, f"open_stream failed: {ec.value}"

    full = lib.lsl_get_fullinfo(inlet, 5.0, ctypes.byref(ec))
    ch_count = lib.lsl_get_channel_count(full)
    srate = lib.lsl_get_nominal_srate(full)
    lib.lsl_destroy_streaminfo(full)
    lib.lsl_destroy_streaminfo(found[0])

    with open(go_file, "w") as f:
        f.write("GO\n")

    max_rows = 256
    data_buf = (ctypes.c_float * (max_rows * 64))()
    ts_buf = (ctypes.c_double * max_rows)()
    received = 0
    pb_peak = 0.0
    sb_peak = 0.0
    prev_ts = None
    non_monotonic = 0

    deadline = time.monotonic() + 15.0
    while received < expected_min and time.monotonic() < deadline:
        got_elems = lib.lsl_pull_chunk_f(
            inlet, data_buf, ts_buf, max_rows * 64, max_rows, 0.2, ctypes.byref(ec)
        )
        rows = got_elems // 64
        if rows == 0:
            continue
        for r in range(rows):
            ts = ts_buf[r]
            if prev_ts is not None and ts <= prev_ts:
                non_monotonic += 1
            prev_ts = ts
            if received >= 10:
                pb_peak = max(pb_peak, abs(data_buf[r * 64]))
                sb_peak = max(sb_peak, abs(data_buf[r * 64 + 1]))
            received += 1

    lost = lib.labstream_samples_lost(inlet)
    lib.lsl_destroy_inlet(inlet)

    result = {
        "received": received,
        "expected_min": expected_min,
        "lost": lost,
        "channel_count": ch_count,
        "nominal_srate": srate,
        "pb_peak_20hz": round(pb_peak, 4),
        "sb_peak_300hz": round(sb_peak, 5),
        "non_monotonic": non_monotonic,
    }
    print(json.dumps(result))
    sys.stdout.flush()


def run_netns_test():
    if not os.path.exists(LABSTREAM_LIB):
        sys.exit(f"missing release library: {LABSTREAM_LIB}")

    script = os.path.abspath(__file__)
    stream = f"Select4k64_{os.getpid()}"
    seconds = 4.0
    # 4.0 s * 250 Hz = 1000 decimated samples (minus ~2 warmup samples at filter edge).
    expected_min = 990

    with tempfile.TemporaryDirectory(prefix="labstream-select-test-") as tmp:
        ready = os.path.join(tmp, "ready")
        go = os.path.join(tmp, "go")
        cfg = os.path.join(tmp, "inlet.cfg")
        out_json = os.path.join(tmp, "result.json")

        bash = f"""
set -euo pipefail
unshare -n sleep 45 &
NS_PID=$!
trap 'kill $NS_PID 2>/dev/null || true' EXIT
sleep 0.2

ip link set lo up
ip link add v_out type veth peer name v_in
ip link set v_in netns $NS_PID

ip link set v_out up
ip addr add 10.78.0.1/24 dev v_out

nsenter -t $NS_PID -n ip link set lo up
nsenter -t $NS_PID -n ip link set v_in up
nsenter -t $NS_PID -n ip addr add 10.78.0.2/24 dev v_in

# Limit the link to 1 Mbit/s (raw 4000 Hz x 64 ch is ~8.5 Mbit/s; 250 Hz preview is ~0.53 Mbit/s).
tc qdisc replace dev v_out root netem rate 1mbit
nsenter -t $NS_PID -n tc qdisc replace dev v_in root netem rate 1mbit

python3 {script} --role outlet --name {stream} --seconds {seconds} --ready {ready} --go {go} &
PID_OUT=$!
for _ in $(seq 1 50); do [ -f {ready} ] && break; sleep 0.1; done

nsenter -t $NS_PID -n python3 {script} --role inlet --name {stream} --expected {expected_min} --peer 10.78.0.1 --cfg {cfg} --go {go} > {out_json} &
PID_IN=$!

wait $PID_IN
wait $PID_OUT || true
"""
        proc = subprocess.run(
            ["unshare", "-Urn", "bash", "-c", bash],
            capture_output=True,
            text=True,
            timeout=45,
        )
        if proc.returncode != 0:
            print("STDOUT:", proc.stdout)
            print("STDERR:", proc.stderr)
            sys.exit(proc.returncode)

        with open(out_json) as f:
            res = json.loads(f.read().strip())
        print("Step 6 1 Mbit/s 4000Hz->250Hz x 64ch result:", json.dumps(res, indent=2))
        assert res["lost"] == 0, f"expected 0 lost, got {res['lost']}"
        assert res["received"] >= expected_min, f"expected >= {expected_min}, got {res['received']}"
        assert res["channel_count"] == 64
        assert abs(res["nominal_srate"] - 250.0) < 1e-6
        assert res["pb_peak_20hz"] > 0.95, f"20 Hz passband attenuated: {res['pb_peak_20hz']}"
        assert res["sb_peak_300hz"] < 0.02, f"300 Hz stopband not attenuated: {res['sb_peak_300hz']}"
        assert res["non_monotonic"] == 0
        print("PASS: 250 Hz preview of 4000 Hz x 64 ch over 1 Mbit/s arrived with 0 loss!")


if __name__ == "__main__":
    if "--role" in sys.argv:
        role = sys.argv[sys.argv.index("--role") + 1]
        if role == "outlet":
            run_outlet(
                sys.argv[sys.argv.index("--name") + 1],
                float(sys.argv[sys.argv.index("--seconds") + 1]),
                sys.argv[sys.argv.index("--ready") + 1],
                sys.argv[sys.argv.index("--go") + 1],
            )
        elif role == "inlet":
            run_inlet(
                sys.argv[sys.argv.index("--name") + 1],
                int(sys.argv[sys.argv.index("--expected") + 1]),
                sys.argv[sys.argv.index("--peer") + 1],
                sys.argv[sys.argv.index("--cfg") + 1],
                sys.argv[sys.argv.index("--go") + 1],
            )
    else:
        run_netns_test()
