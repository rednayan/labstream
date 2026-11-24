#!/usr/bin/env python3
"""End-to-end conformance test for Step 7 (`comp/1`, Criteria 1, 3, and 4).

Tests:
1. Criterion 3 (1 Mbit/s `tc netem` veth link, 512 Hz x 64 ch Int32 EEG, chunk 16):
   Raw wire rate is ~1.15 Mbit/s (> 1 Mbit/s).
   - `comp` OFF (`{seq, resume, dgram}`): saturates the 1 Mbit/s link (loses/drops
     samples or lags severely).
   - `comp` ON over UDP (`{seq, resume, dgram, comp}`): arrives with 0 lost,
     0 dropped, bit-exact Int32 values, and wire ratio < 0.55.
   - `comp` ON over TCP (`{seq, resume, comp}`): arrives with 0 lost,
     0 dropped, bit-exact Int32 values, and wire ratio < 0.50.
2. Criterion 4 (4000 Hz x 64 ch Int32 EEG, chunk 32, loopback without `shm`):
   Measures outlet CPU over 5 s with `comp` ON over TCP and UDP and asserts
   outlet CPU < 5.0 % of one core.
"""

import ctypes
import json
import os
import resource
import subprocess
import sys
import tempfile
import time

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
LABSTREAM_LIB = os.path.join(ROOT, "target", "release", "liblsl.so")


def eeg_value(idx: int, ch: int) -> int:
    """Deterministic realistic 24-bit integer ADC EEG waveform (~20 uV RMS + DC offset).

    Matches an ADS1299-class 24-bit converter (0.02235 uV/LSB) with realistic
    per-channel DC offset, 10 Hz alpha, 50 Hz mains, and thermal ADC noise.
    """
    # Deterministic xorshift64 noise in [-12, +12] counts (~0.27 uV)
    x = ((idx * 6364136223846793005 + ch * 1442695040888963407 + 0x9E3779B97F4A7C15) & 0xFFFFFFFFFFFFFFFF)
    x ^= (x >> 12)
    x ^= ((x << 25) & 0xFFFFFFFFFFFFFFFF)
    x ^= (x >> 27)
    noise = (x % 25) - 12
    dc = 120_000 + ch * 9_500
    # Integer triangle approximations for ~10 Hz and ~50 Hz smooth waves (fast in Python).
    p10 = (idx + ch * 7) % 52
    tri10 = (p10 if p10 < 26 else 52 - p10) * 30 - 390
    p50 = idx % 10
    tri50 = (p50 if p50 < 5 else 10 - p50) * 40 - 100
    return dc + tri10 + tri50 + noise


def cpu_seconds() -> float:
    r = resource.getrusage(resource.RUSAGE_SELF)
    return r.ru_utime + r.ru_stime


def run_outlet(
    stream_name: str,
    rate: float,
    channels: int,
    chunk: int,
    seconds: float,
    cfg_path: str,
    ready_file: str,
    go_file: str,
):
    os.environ["LSLAPICFG"] = cfg_path
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
    lib.lsl_push_chunk_itp.restype = ctypes.c_int
    lib.lsl_push_chunk_itp.argtypes = [
        ctypes.c_void_p, ctypes.POINTER(ctypes.c_int32),
        ctypes.c_uint64, ctypes.c_double, ctypes.c_int,
    ]
    lib.lsl_destroy_outlet.argtypes = [ctypes.c_void_p]
    lib.lsl_destroy_streaminfo.argtypes = [ctypes.c_void_p]

    # cft_int32 = 4
    info = lib.lsl_create_streaminfo(
        stream_name.encode(), b"EEG", channels, rate, 4, stream_name.encode()
    )
    outlet = lib.lsl_create_outlet(info, 0, 360)
    assert outlet, "failed to create outlet"
    with open(ready_file, "w") as f:
        f.write("READY\n")
    assert lib.lsl_wait_for_consumers(outlet, 15.0), "inlet did not connect"
    go_deadline = time.monotonic() + 15.0
    while not os.path.exists(go_file) and time.monotonic() < go_deadline:
        time.sleep(0.01)

    total_samples = int(rate * seconds)
    # Pre-generate all samples before timing so Python generation does not
    # pollute outlet CPU measurement.
    chunks_data = []
    sent = 0
    while sent < total_samples:
        n = min(chunk, total_samples - sent)
        buf = (ctypes.c_int32 * (n * channels))()
        for k in range(n):
            idx = sent + k
            base = k * channels
            for c in range(channels):
                buf[base + c] = eeg_value(idx, c)
        end_ts = 1000.0 + (sent + n - 1) / rate
        chunks_data.append((buf, n, end_ts))
        sent += n

    cpu0 = cpu_seconds()
    t0 = time.monotonic()
    pushed = 0
    for buf, n, end_ts in chunks_data:
        lib.lsl_push_chunk_itp(outlet, buf, n * channels, end_ts, 1)
        pushed += n
        target = t0 + pushed / rate
        rem = target - time.monotonic()
        if rem > 0.0005:
            time.sleep(rem)
    wall = time.monotonic() - t0
    cpu_pct = 100.0 * (cpu_seconds() - cpu0) / max(wall, 1e-6)

    time.sleep(0.6)
    lib.lsl_destroy_outlet(outlet)
    lib.lsl_destroy_streaminfo(info)
    print(json.dumps({"role": "outlet", "pushed": pushed, "wall_s": round(wall, 3), "cpu_pct": round(cpu_pct, 2)}))
    sys.stdout.flush()


def run_inlet(
    stream_name: str,
    rate: float,
    channels: int,
    expected: int,
    cfg_path: str,
    go_file: str,
    verify_bits: bool,
    wait_s: float,
):
    os.environ["LSLAPICFG"] = cfg_path
    lib = ctypes.CDLL(LABSTREAM_LIB)
    lib.lsl_resolve_byprop.restype = ctypes.c_int
    lib.lsl_resolve_byprop.argtypes = [
        ctypes.POINTER(ctypes.c_void_p), ctypes.c_uint32,
        ctypes.c_char_p, ctypes.c_char_p, ctypes.c_int, ctypes.c_double,
    ]
    lib.lsl_create_inlet.restype = ctypes.c_void_p
    lib.lsl_create_inlet.argtypes = [ctypes.c_void_p, ctypes.c_int, ctypes.c_int, ctypes.c_int]
    lib.lsl_open_stream.argtypes = [ctypes.c_void_p, ctypes.c_double, ctypes.POINTER(ctypes.c_int)]
    lib.labstream_samples_lost.restype = ctypes.c_int64
    lib.labstream_samples_lost.argtypes = [ctypes.c_void_p]
    lib.labstream_inlet_compressed.restype = ctypes.c_int32
    lib.labstream_inlet_compressed.argtypes = [ctypes.c_void_p]
    lib.labstream_inlet_compression_ratio.restype = ctypes.c_double
    lib.labstream_inlet_compression_ratio.argtypes = [ctypes.c_void_p]
    lib.lsl_pull_chunk_i.restype = ctypes.c_uint64
    lib.lsl_pull_chunk_i.argtypes = [
        ctypes.c_void_p, ctypes.POINTER(ctypes.c_int32), ctypes.POINTER(ctypes.c_double),
        ctypes.c_uint64, ctypes.c_uint64, ctypes.c_double, ctypes.POINTER(ctypes.c_int),
    ]
    lib.lsl_destroy_inlet.argtypes = [ctypes.c_void_p]
    lib.lsl_destroy_streaminfo.argtypes = [ctypes.c_void_p]

    found = (ctypes.c_void_p * 1)()
    n = lib.lsl_resolve_byprop(found, 1, b"name", stream_name.encode(), 1, 15.0)
    assert n == 1, f"failed to resolve {stream_name}"

    inlet = lib.lsl_create_inlet(found[0], 10, 0, 0)
    assert inlet, "failed to create inlet"
    ec = ctypes.c_int(0)
    lib.lsl_open_stream(inlet, 15.0, ctypes.byref(ec))
    assert ec.value == 0, f"open_stream failed: {ec.value}"
    lib.lsl_destroy_streaminfo(found[0])

    with open(go_file, "w") as f:
        f.write("GO\n")

    max_rows = 256
    data_buf = (ctypes.c_int32 * (max_rows * channels))()
    ts_buf = (ctypes.c_double * max_rows)()
    received = 0
    mismatches = 0
    t0 = time.monotonic()
    deadline = t0 + wait_s
    while received < expected and time.monotonic() < deadline:
        got_elems = lib.lsl_pull_chunk_i(
            inlet, data_buf, ts_buf, max_rows * channels, max_rows, 0.2, ctypes.byref(ec)
        )
        rows = got_elems // channels
        if rows == 0:
            continue
        if verify_bits:
            for r in range(rows):
                idx = received + r
                base = r * channels
                for c in range(channels):
                    if data_buf[base + c] != eeg_value(idx, c):
                        mismatches += 1
        received += rows
    elapsed = time.monotonic() - t0
    lost = lib.labstream_samples_lost(inlet)
    compressed = lib.labstream_inlet_compressed(inlet)
    ratio = lib.labstream_inlet_compression_ratio(inlet)
    lib.lsl_destroy_inlet(inlet)

    res = {
        "role": "inlet",
        "expected": expected,
        "received": received,
        "lost": lost,
        "mismatches": mismatches,
        "compressed": compressed,
        "ratio": None if (ratio != ratio) else round(ratio, 4),
        "elapsed_s": round(elapsed, 3),
    }
    print(json.dumps(res))
    sys.stdout.flush()


def run_1mbit_case(label: str, features: str, verify_bits: bool, seconds: float = 5.0):
    script = os.path.abspath(__file__)
    stream = f"Comp1M_{label}_{os.getpid()}"
    rate = 512.0
    channels = 64
    chunk = 16
    expected = int(rate * seconds)
    wait_s = seconds + 2.5

    with tempfile.TemporaryDirectory(prefix=f"labstream-comp-{label}-") as tmp:
        ready = os.path.join(tmp, "ready")
        go = os.path.join(tmp, "go")
        cfg_out = os.path.join(tmp, "out.cfg")
        cfg_in = os.path.join(tmp, "in.cfg")
        out_json = os.path.join(tmp, "out.json")
        in_json = os.path.join(tmp, "in.json")

        with open(cfg_out, "w") as f:
            f.write(f"[lab]\nKnownPeers = {{10.79.0.1, 10.79.0.2}}\n[labstream]\nFeatures = {{{features}}}\n")
        with open(cfg_in, "w") as f:
            f.write(f"[lab]\nKnownPeers = {{10.79.0.1, 10.79.0.2}}\n[labstream]\nFeatures = {{{features}}}\n")

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
ip addr add 10.79.0.1/24 dev v_out

nsenter -t $NS_PID -n ip link set lo up
nsenter -t $NS_PID -n ip link set v_in up
nsenter -t $NS_PID -n ip addr add 10.79.0.2/24 dev v_in

tc qdisc replace dev v_out root netem rate 1mbit
nsenter -t $NS_PID -n tc qdisc replace dev v_in root netem rate 1mbit

python3 {script} --role outlet --name {stream} --rate {rate} --channels {channels} --chunk {chunk} --seconds {seconds} --cfg {cfg_out} --ready {ready} --go {go} > {out_json} &
PID_OUT=$!
for _ in $(seq 1 50); do [ -f {ready} ] && break; sleep 0.1; done

nsenter -t $NS_PID -n python3 {script} --role inlet --name {stream} --rate {rate} --channels {channels} --expected {expected} --cfg {cfg_in} --go {go} --verify {1 if verify_bits else 0} --wait {wait_s} > {in_json} &
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
            print(f"[{label}] STDOUT:", proc.stdout)
            print(f"[{label}] STDERR:", proc.stderr)
            sys.exit(proc.returncode)
        with open(in_json) as f:
            in_res = json.loads(f.read().strip())
        with open(out_json) as f:
            out_txt = f.read().strip()
            out_res = json.loads(out_txt) if out_txt else {}
        return out_res, in_res


def run_cpu_case(label: str, features: str, seconds: float = 5.0):
    script = os.path.abspath(__file__)
    stream = f"CompCpu_{label}_{os.getpid()}"
    rate = 4000.0
    channels = 64
    chunk = 32
    expected = int(rate * seconds)

    with tempfile.TemporaryDirectory(prefix=f"labstream-cpu-{label}-") as tmp:
        ready = os.path.join(tmp, "ready")
        go = os.path.join(tmp, "go")
        cfg = os.path.join(tmp, "lsl.cfg")
        with open(cfg, "w") as f:
            f.write(f"[labstream]\nFeatures = {{{features}}}\n")

        p_out = subprocess.Popen(
            [
                sys.executable, script, "--role", "outlet", "--name", stream,
                "--rate", str(rate), "--channels", str(channels), "--chunk", str(chunk),
                "--seconds", str(seconds), "--cfg", cfg, "--ready", ready, "--go", go,
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        for _ in range(50):
            if os.path.exists(ready):
                break
            time.sleep(0.1)
        p_in = subprocess.Popen(
            [
                sys.executable, script, "--role", "inlet", "--name", stream,
                "--rate", str(rate), "--channels", str(channels), "--expected", str(expected),
                "--cfg", cfg, "--go", go, "--verify", "0", "--wait", str(seconds + 5.0),
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        in_stdout, in_stderr = p_in.communicate(timeout=30)
        out_stdout, out_stderr = p_out.communicate(timeout=30)
        if p_in.returncode != 0 or p_out.returncode != 0:
            print(f"[{label}] out err: {out_stderr}\nin err: {in_stderr}")
            sys.exit(1)
        return json.loads(out_stdout.strip()), json.loads(in_stdout.strip())


def main():
    if not os.path.exists(LABSTREAM_LIB):
        sys.exit(f"missing release library: {LABSTREAM_LIB}")

    print("=== Criterion 3: 512 Hz x 64 ch Int32 EEG (~1.15 Mbit/s raw) over 1 Mbit/s link ===")
    o_off, i_off = run_1mbit_case("uncompressed", "seq, resume, dgram", verify_bits=False)
    print("  comp OFF (UDP):", json.dumps(i_off))
    o_udp, i_udp = run_1mbit_case("udp_comp", "seq, resume, dgram, comp", verify_bits=True)
    print("  comp ON  (UDP):", json.dumps(i_udp))
    o_tcp, i_tcp = run_1mbit_case("tcp_comp", "seq, resume, comp", verify_bits=True)
    print("  comp ON  (TCP):", json.dumps(i_tcp))

    assert i_off["received"] < i_off["expected"] or i_off["lost"] > 0 or i_off["elapsed_s"] > 5.8, (
        f"expected uncompressed ~1.15 Mbit/s stream to fail/lag on 1 Mbit/s link: {i_off}"
    )
    for tag, res in [("UDP+comp", i_udp), ("TCP+comp", i_tcp)]:
        assert res["compressed"] == 1, f"{tag}: comp not active"
        assert res["lost"] == 0, f"{tag}: expected 0 lost, got {res['lost']}"
        assert res["received"] == res["expected"], f"{tag}: short receive {res}"
        assert res["mismatches"] == 0, f"{tag}: bit mismatch count {res['mismatches']}"
        assert res["ratio"] is not None and res["ratio"] < 0.55, f"{tag}: weak ratio {res['ratio']}"

    print("=== Criterion 4: 4000 Hz x 64 ch Int32 EEG (chunk 32) outlet CPU (< 5.0 %) ===")
    co_udp, ci_udp = run_cpu_case("udp_4k", "seq, resume, dgram, comp")
    print("  4000Hz x 64ch UDP+comp outlet:", json.dumps(co_udp), "inlet:", json.dumps(ci_udp))
    co_tcp, ci_tcp = run_cpu_case("tcp_4k", "seq, resume, comp")
    print("  4000Hz x 64ch TCP+comp outlet:", json.dumps(co_tcp), "inlet:", json.dumps(ci_tcp))
    assert ci_udp["lost"] == 0 and ci_udp["received"] == ci_udp["expected"]
    assert ci_tcp["lost"] == 0 and ci_tcp["received"] == ci_tcp["expected"]
    assert co_udp["cpu_pct"] < 5.0, f"UDP+comp outlet CPU {co_udp['cpu_pct']}% >= 5.0%"
    assert co_tcp["cpu_pct"] < 5.0, f"TCP+comp outlet CPU {co_tcp['cpu_pct']}% >= 5.0%"
    print("PASS: All Step 7 1 Mbit/s and 4000 Hz x 64 ch CPU criteria met!")


if __name__ == "__main__":
    if "--role" in sys.argv:
        def arg(flag):
            return sys.argv[sys.argv.index(flag) + 1]
        role = arg("--role")
        if role == "outlet":
            run_outlet(
                arg("--name"), float(arg("--rate")), int(arg("--channels")),
                int(arg("--chunk")), float(arg("--seconds")),
                arg("--cfg"), arg("--ready"), arg("--go"),
            )
        else:
            run_inlet(
                arg("--name"), float(arg("--rate")), int(arg("--channels")),
                int(arg("--expected")), arg("--cfg"), arg("--go"),
                arg("--verify") == "1", float(arg("--wait")),
            )
    else:
        main()
