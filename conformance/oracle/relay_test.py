#!/usr/bin/env python3
"""End-to-end conformance test for Step 4 (`labstream relay`).

Runs a real pinned C++ `liblsl` oracle outlet alongside `labstream relay` in
one Linux network namespace, connects a `labstream` inlet from a second network
namespace over a `veth` pair, applies a 3-second 100% packet-loss cut with
`tc netem` on the link after the relay, and verifies:

1. `labstream_samples_lost(inlet) == 0` after the cut (Criterion 1).
2. Exact sample values, exact timestamps, and preserved `<desc>` XML
   (Criterion 2).
3. `labstream_streaminfo_is_relayed(info) == 1` and distinct name/source_id
   (Criterion 3).
"""

import ctypes
import json
import os
import subprocess
import sys
import tempfile
import time

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
ORACLE_LIB = os.path.join(ROOT, "conformance", ".build", "install", "lib", "liblsl.so.2")
LABSTREAM_LIB = os.path.join(ROOT, "target", "release", "liblsl.so")
LABSTREAM_BIN = os.path.join(ROOT, "target", "release", "labstream")


def run_oracle_outlet(stream_name: str, seconds: float, ready_file: str, go_file: str):
    lib = ctypes.CDLL(ORACLE_LIB)
    lib.lsl_create_streaminfo.restype = ctypes.c_void_p
    lib.lsl_create_streaminfo.argtypes = [
        ctypes.c_char_p, ctypes.c_char_p, ctypes.c_int, ctypes.c_double,
        ctypes.c_int, ctypes.c_char_p,
    ]
    lib.lsl_get_desc.restype = ctypes.c_void_p
    lib.lsl_get_desc.argtypes = [ctypes.c_void_p]
    lib.lsl_append_child.restype = ctypes.c_void_p
    lib.lsl_append_child.argtypes = [ctypes.c_void_p, ctypes.c_char_p]
    lib.lsl_append_child_value.restype = ctypes.c_void_p
    lib.lsl_append_child_value.argtypes = [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_char_p]
    lib.lsl_create_outlet.restype = ctypes.c_void_p
    lib.lsl_create_outlet.argtypes = [ctypes.c_void_p, ctypes.c_int, ctypes.c_int]
    lib.lsl_wait_for_consumers.restype = ctypes.c_int
    lib.lsl_wait_for_consumers.argtypes = [ctypes.c_void_p, ctypes.c_double]
    lib.lsl_push_sample_ft.restype = ctypes.c_int
    lib.lsl_push_sample_ft.argtypes = [ctypes.c_void_p, ctypes.POINTER(ctypes.c_float), ctypes.c_double]
    lib.lsl_destroy_outlet.argtypes = [ctypes.c_void_p]
    lib.lsl_destroy_streaminfo.argtypes = [ctypes.c_void_p]

    channels = 8
    rate = 250.0
    info = lib.lsl_create_streaminfo(
        stream_name.encode(), b"EEG", channels, rate, 1, b"oracle-eeg-01"
    )
    desc = lib.lsl_get_desc(info)
    chs = lib.lsl_append_child(desc, b"channels")
    for c in range(channels):
        ch = lib.lsl_append_child(chs, b"channel")
        lib.lsl_append_child_value(ch, b"label", f"Ch{c}".encode())
        lib.lsl_append_child_value(ch, b"unit", b"microvolts")

    outlet = lib.lsl_create_outlet(info, 0, 360)
    assert outlet, "failed to create liblsl oracle outlet"
    with open(ready_file, "w") as f:
        f.write("READY\n")
    assert lib.lsl_wait_for_consumers(outlet, 15.0), "relay did not connect to oracle outlet"
    # Wait until the downstream labstream inlet is attached to the relay too.
    go_deadline = time.monotonic() + 15.0
    while not os.path.exists(go_file) and time.monotonic() < go_deadline:
        time.sleep(0.05)
    time.sleep(0.2)

    total = int(rate * seconds)
    buf = (ctypes.c_float * channels)()
    t0 = time.monotonic()
    for i in range(total):
        target = t0 + i / rate
        rem = target - time.monotonic()
        if rem > 0.001:
            time.sleep(rem)
        buf[0] = float(i)
        for c in range(1, channels):
            buf[c] = float(c) * 0.5
        ts = 5000.0 + i / rate
        lib.lsl_push_sample_ft(outlet, buf, ts)

    time.sleep(1.5)
    lib.lsl_destroy_outlet(outlet)
    lib.lsl_destroy_streaminfo(info)


def run_labstream_inlet(relay_name: str, expected: int, peer_ip: str, cfg_path: str, go_file: str):
    with open(cfg_path, "w") as f:
        f.write(f"[lab]\nKnownPeers = {{{peer_ip}}}\n")
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
    lib.lsl_get_fullinfo.restype = ctypes.c_void_p
    lib.lsl_get_fullinfo.argtypes = [ctypes.c_void_p, ctypes.c_double, ctypes.POINTER(ctypes.c_int)]
    lib.lsl_get_xml.restype = ctypes.c_void_p
    lib.lsl_get_xml.argtypes = [ctypes.c_void_p]
    lib.lsl_destroy_string.argtypes = [ctypes.c_void_p]
    lib.labstream_streaminfo_is_relayed.restype = ctypes.c_int
    lib.labstream_streaminfo_is_relayed.argtypes = [ctypes.c_void_p]
    lib.labstream_samples_lost.restype = ctypes.c_int64
    lib.labstream_samples_lost.argtypes = [ctypes.c_void_p]
    lib.labstream_samples_resent.restype = ctypes.c_int64
    lib.labstream_samples_resent.argtypes = [ctypes.c_void_p]
    lib.lsl_pull_sample_f.restype = ctypes.c_double
    lib.lsl_pull_sample_f.argtypes = [
        ctypes.c_void_p, ctypes.POINTER(ctypes.c_float), ctypes.c_int,
        ctypes.c_double, ctypes.POINTER(ctypes.c_int),
    ]
    lib.lsl_destroy_inlet.argtypes = [ctypes.c_void_p]
    lib.lsl_destroy_streaminfo.argtypes = [ctypes.c_void_p]

    found = (ctypes.c_void_p * 1)()
    n = lib.lsl_resolve_byprop(found, 1, b"name", relay_name.encode(), 1, 15.0)
    assert n == 1, f"failed to resolve relayed stream {relay_name}"
    is_relayed_short = lib.labstream_streaminfo_is_relayed(found[0])

    inlet = lib.lsl_create_inlet(found[0], 360, 0, 1)
    ec = ctypes.c_int(0)
    lib.lsl_open_stream(inlet, 15.0, ctypes.byref(ec))
    assert ec.value == 0, f"open_stream failed: {ec.value}"
    with open(go_file, "w") as f:
        f.write("GO\n")

    full = lib.lsl_get_fullinfo(inlet, 5.0, ctypes.byref(ec))
    is_relayed_full = lib.labstream_streaminfo_is_relayed(full)
    xml_ptr = lib.lsl_get_xml(full)
    xml = ctypes.string_at(xml_ptr).decode("utf-8", errors="replace")
    lib.lsl_destroy_string(xml_ptr)
    lib.lsl_destroy_streaminfo(full)
    lib.lsl_destroy_streaminfo(found[0])

    channels = 8
    rate = 250.0
    buf = (ctypes.c_float * channels)()
    received = 0
    bad_val = 0
    bad_ts = 0
    seen = set()
    deadline = time.monotonic() + 25.0
    while received < expected and time.monotonic() < deadline:
        ts = lib.lsl_pull_sample_f(inlet, buf, channels, 0.5, ctypes.byref(ec))
        if ts == 0.0:
            continue
        idx = int(round(buf[0]))
        seen.add(idx)
        for c in range(1, channels):
            if buf[c] != float(c) * 0.5:
                bad_val += 1
        want_ts = 5000.0 + idx / rate
        if abs(ts - want_ts) > 1e-9:
            bad_ts += 1
        received += 1
        if idx == expected - 1:
            break

    lost = lib.labstream_samples_lost(inlet)
    resent = lib.labstream_samples_resent(inlet)
    lib.lsl_destroy_inlet(inlet)

    result = {
        "received": received,
        "unique": len(seen),
        "expected": expected,
        "bad_val": bad_val,
        "bad_ts": bad_ts,
        "lost": lost,
        "resent": resent,
        "is_relayed_short": is_relayed_short,
        "is_relayed_full": is_relayed_full,
        "has_channels_desc": "<label>Ch7</label>" in xml and "<unit>microvolts</unit>" in xml,
        "has_relay_tag": "<labstream_relay>" in xml,
    }
    print(json.dumps(result))
    sys.stdout.flush()


def run_netns_test():
    for path in [ORACLE_LIB, LABSTREAM_LIB, LABSTREAM_BIN]:
        if not os.path.exists(path):
            sys.exit(f"missing required binary/library: {path}")

    script = os.path.abspath(__file__)
    stream = f"OracleRelaySrc_{os.getpid()}"
    relay_name = f"{stream}-relay"
    seconds = 16.0
    expected = int(250 * seconds)

    with tempfile.TemporaryDirectory(prefix="labstream-relay-test-") as tmp:
        ready = os.path.join(tmp, "ready")
        go = os.path.join(tmp, "go")
        cfg = os.path.join(tmp, "inlet.cfg")
        out_json = os.path.join(tmp, "result.json")

        # Run inside an unprivileged user+net namespace (`unshare -Urn`).
        # Current netns holds `liblsl` oracle + `labstream relay` (10.77.0.1);
        # child netns (via `unshare -n`) holds `labstream` inlet (10.77.0.2).
        # At t=3.0 s, cut the veth link with `tc netem loss 100%` for 8.0 s
        # (long enough to exceed TCP keepalive and trigger `resume/1` resend!).
        bash = f"""
set -euo pipefail
unshare -n sleep 60 &
NS_PID=$!
trap 'kill $NS_PID 2>/dev/null || true' EXIT
sleep 0.2

ip link set lo up
ip link add v_src type veth peer name v_in
ip link set v_in netns $NS_PID

ip link set v_src up
ip addr add 10.77.0.1/24 dev v_src

nsenter -t $NS_PID -n ip link set lo up
nsenter -t $NS_PID -n ip link set v_in up
nsenter -t $NS_PID -n ip addr add 10.77.0.2/24 dev v_in

# Start pinned C++ liblsl oracle outlet in source netns
python3 {script} --role oracle --name {stream} --seconds {seconds} --ready {ready} --go {go} &
PID_OUT=$!
for _ in $(seq 1 50); do [ -f {ready} ] && break; sleep 0.1; done

# Start labstream relay in source netns alongside the liblsl oracle
{LABSTREAM_BIN} relay --name {stream} --disk-mb 16 --duration 25 >/dev/null 2>&1 &
PID_RELAY=$!
sleep 0.8

# Start labstream inlet in child netns across the veth link
nsenter -t $NS_PID -n python3 {script} --role inlet --name {relay_name} --expected {expected} --peer 10.77.0.1 --cfg {cfg} --go {go} > {out_json} &
PID_IN=$!
for _ in $(seq 1 100); do [ -f {go} ] && break; sleep 0.1; done

# Cut the link AFTER the relay for 8.0 seconds (exceeds keepalive, forces resume/1)
sleep 3.0
tc qdisc replace dev v_src root netem loss 100%
nsenter -t $NS_PID -n tc qdisc replace dev v_in root netem loss 100%
sleep 8.0
tc qdisc del dev v_src root
nsenter -t $NS_PID -n tc qdisc del dev v_in root

wait $PID_IN
wait $PID_OUT || true
kill $PID_RELAY 2>/dev/null || true
wait $PID_RELAY 2>/dev/null || true
"""
        proc = subprocess.run(
            ["unshare", "-Urn", "bash", "-c", bash],
            capture_output=True,
            text=True,
            timeout=60,
        )
        if proc.returncode != 0:
            print("STDOUT:", proc.stdout)
            print("STDERR:", proc.stderr)
            sys.exit(proc.returncode)

        with open(out_json) as f:
            res = json.loads(f.read().strip())
        print("Step 4 oracle->relay->cut->inlet result:", json.dumps(res, indent=2))
        assert res["lost"] == 0, f"expected 0 lost, got {res['lost']}"
        assert res["unique"] == expected, f"expected {expected} unique, got {res['unique']}"
        assert res["bad_val"] == 0, f"value mismatch count: {res['bad_val']}"
        assert res["bad_ts"] == 0, f"timestamp mismatch count: {res['bad_ts']}"
        assert res["is_relayed_short"] == 1
        assert res["is_relayed_full"] == 1
        assert res["has_channels_desc"]
        assert res["has_relay_tag"]
        print("PASS: liblsl oracle -> labstream relay -> 8s netem cut -> labstream inlet: 0 lost!")


if __name__ == "__main__":
    if "--role" in sys.argv:
        idx = sys.argv.index("--role")
        role = sys.argv[idx + 1]
        if role == "oracle":
            name = sys.argv[sys.argv.index("--name") + 1]
            secs = float(sys.argv[sys.argv.index("--seconds") + 1])
            ready = sys.argv[sys.argv.index("--ready") + 1]
            go = sys.argv[sys.argv.index("--go") + 1]
            run_oracle_outlet(name, secs, ready, go)
        elif role == "inlet":
            name = sys.argv[sys.argv.index("--name") + 1]
            exp = int(sys.argv[sys.argv.index("--expected") + 1])
            peer = sys.argv[sys.argv.index("--peer") + 1]
            cfg = sys.argv[sys.argv.index("--cfg") + 1]
            go = sys.argv[sys.argv.index("--go") + 1]
            run_labstream_inlet(name, exp, peer, cfg, go)
    else:
        run_netns_test()
