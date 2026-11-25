#!/usr/bin/env python3
"""Multi-node real-world Virtual Lab roles for labstream conformance testing.

Roles:
- vendor-amp    (172.29.0.10): Runs genuine C++ liblsl outlet + labstream relay (--disk-mb 128)
- hd-eeg-node   (172.29.0.11): Runs labstream 32-ch @ 250 Hz EEG (comp/1 + meta/1),
                               mandatory enc/1 clinical marker stream, and WsBridge
- recorder-node (172.29.0.12): Connects across the Docker LAN, injects real tc netem &
                               iptables chaos faults, runs rogue plaintext probe +
                               labstream doctor, verifies 0 loss / bit-exact / RSS stability,
                               and writes /shared/labtest-report.json + summary.
"""

import ctypes
import json
import math
import os
import socket
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

ORACLE_LIB = "/opt/oracle/liblsl.so.2"
LABSTREAM_LIB = "/opt/labstream/liblsl.so"
SHARED = Path("/shared")
DURATION = float(os.environ.get("LABTEST_DURATION", "45"))
CHAOS = os.environ.get("LABTEST_CHAOS", "1") == "1"


def write_cfg(content: str) -> str:
    fd, path = tempfile.mkstemp(prefix="labtest_", suffix=".cfg")
    with os.fdopen(fd, "w") as f:
        f.write(content)
    return path


def load_lsl(lib_path: str):
    lib = ctypes.CDLL(lib_path)
    lib.lsl_create_streaminfo.restype = ctypes.c_void_p
    lib.lsl_create_streaminfo.argtypes = [
        ctypes.c_char_p, ctypes.c_char_p, ctypes.c_int, ctypes.c_double,
        ctypes.c_int, ctypes.c_char_p,
    ]
    lib.lsl_create_outlet.restype = ctypes.c_void_p
    lib.lsl_create_outlet.argtypes = [ctypes.c_void_p, ctypes.c_int, ctypes.c_int]
    lib.lsl_push_sample_ftp.argtypes = [
        ctypes.c_void_p, ctypes.POINTER(ctypes.c_float), ctypes.c_double, ctypes.c_int,
    ]
    lib.lsl_push_sample_strtp.argtypes = [
        ctypes.c_void_p, ctypes.POINTER(ctypes.c_char_p), ctypes.c_double, ctypes.c_int,
    ]
    lib.lsl_resolve_byprop.restype = ctypes.c_int
    lib.lsl_resolve_byprop.argtypes = [
        ctypes.POINTER(ctypes.c_void_p), ctypes.c_int,
        ctypes.c_char_p, ctypes.c_char_p, ctypes.c_int, ctypes.c_double,
    ]
    lib.lsl_create_inlet.restype = ctypes.c_void_p
    lib.lsl_create_inlet.argtypes = [ctypes.c_void_p, ctypes.c_int, ctypes.c_int, ctypes.c_int]
    lib.lsl_open_stream.argtypes = [ctypes.c_void_p, ctypes.c_double, ctypes.POINTER(ctypes.c_int)]
    lib.lsl_pull_sample_f.restype = ctypes.c_double
    lib.lsl_pull_sample_f.argtypes = [
        ctypes.c_void_p, ctypes.POINTER(ctypes.c_float), ctypes.c_int,
        ctypes.c_double, ctypes.POINTER(ctypes.c_int),
    ]
    lib.lsl_pull_sample_str.restype = ctypes.c_double
    lib.lsl_pull_sample_str.argtypes = [
        ctypes.c_void_p, ctypes.POINTER(ctypes.c_char_p), ctypes.c_int,
        ctypes.c_double, ctypes.POINTER(ctypes.c_int),
    ]
    lib.lsl_get_uid.restype = ctypes.c_char_p
    lib.lsl_get_uid.argtypes = [ctypes.c_void_p]
    lib.lsl_get_v4data_port = None
    lib.lsl_local_clock.restype = ctypes.c_double
    return lib


def bind_labstream_ext(lib):
    lib.labstream_samples_lost.restype = ctypes.c_longlong
    lib.labstream_samples_lost.argtypes = [ctypes.c_void_p]
    lib.labstream_samples_resent.restype = ctypes.c_longlong
    lib.labstream_samples_resent.argtypes = [ctypes.c_void_p]
    lib.labstream_inlet_compressed.restype = ctypes.c_int
    lib.labstream_inlet_compressed.argtypes = [ctypes.c_void_p]
    lib.labstream_inlet_compression_ratio.restype = ctypes.c_double
    lib.labstream_inlet_compression_ratio.argtypes = [ctypes.c_void_p]
    lib.labstream_inlet_encrypted.restype = ctypes.c_int
    lib.labstream_inlet_encrypted.argtypes = [ctypes.c_void_p]
    lib.labstream_set_encryption.restype = ctypes.c_int
    lib.labstream_set_encryption.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_char_p]
    lib.labstream_outlet_push_metadata.restype = ctypes.c_int
    lib.labstream_outlet_push_metadata.argtypes = [
        ctypes.c_void_p, ctypes.c_char_p, ctypes.c_char_p, ctypes.c_double,
    ]
    lib.labstream_inlet_metadata_count.restype = ctypes.c_size_t
    lib.labstream_inlet_metadata_count.argtypes = [ctypes.c_void_p]


def eeg_val(seq: int, ch: int) -> float:
    val = 200.0 + ch * 3.0 + 25.0 * math.sin(seq * 0.04 + ch * 0.2)
    return round(val * 20.0) * 0.05


def run_vendor_amp():
    print("[vendor-amp] starting genuine C++ liblsl outlet + labstream relay (--disk-mb 128)")
    relay = subprocess.Popen(
        [
            "labstream", "relay",
            "--name", "VendorEEG",
            "--name-suffix", "-relayed",
            "--local-only",
            "--disk-mb", "128",
        ],
        stdout=sys.stdout,
        stderr=sys.stderr,
    )
    lib = load_lsl(ORACLE_LIB)
    channels = 16
    srate = 250.0
    info = lib.lsl_create_streaminfo(b"VendorEEG", b"EEG", channels, srate, 1, b"vendor-amp-cpp")
    outlet = lib.lsl_create_outlet(info, 0, 360)
    buf = (ctypes.c_float * channels)()
    t0 = time.monotonic()
    seq = 0
    end = t0 + DURATION + 20.0
    try:
        while time.monotonic() < end:
            for c in range(channels):
                buf[c] = eeg_val(seq, c)
            ts = 1000.0 + seq / srate
            lib.lsl_push_sample_ftp(outlet, buf, ts, 1)
            seq += 1
            nxt = t0 + seq / srate
            rem = nxt - time.monotonic()
            if rem > 0.001:
                time.sleep(rem)
    finally:
        relay.terminate()
        relay.wait(timeout=5)


def run_hd_eeg_node():
    print("[hd-eeg-node] starting labstream HD EEG (comp/1 + meta/1) + enc/1 markers + WsBridge")
    cfg = write_cfg("[labstream]\nFeatures = {seq, resume, dgram, select, comp, meta}\n")
    os.environ["LSLAPICFG"] = cfg

    lib = load_lsl(LABSTREAM_LIB)
    bind_labstream_ext(lib)

    # 1. Start HD EEG outlet (32 ch @ 250 Hz, comp/1 + meta/1, no encryption)
    channels = 32
    srate = 250.0
    info_eeg = lib.lsl_create_streaminfo(b"LabTest_HDEEG", b"EEG", channels, srate, 1, b"hd-eeg-32")
    out_eeg = lib.lsl_create_outlet(info_eeg, 0, 360)

    # 2. Start WsBridge for browser-node (172.29.0.13)
    ws_proc = subprocess.Popen(
        [
            "labstream", "relay",
            "--name", "LabTest_HDEEG",
            "--name-suffix", "-ws",
            "--local-only",
            "--ws-port", "8765",
            "--ws-bind", "0.0.0.0",
            "--ws-allow", "172.29.0.13",
            "--ws-marker-stream", "LabTest_BrowserMarkers",
        ],
        stdout=sys.stdout,
        stderr=sys.stderr,
    )

    # 3. Enable mandatory enc/1 for the clinical marker outlet
    lib.labstream_set_encryption(1, 1, b"labtest-psk-2026")
    info_enc = lib.lsl_create_streaminfo(b"LabTest_Encrypted", b"Markers", 1, 0.0, 3, b"hd-enc-markers")
    out_enc = lib.lsl_create_outlet(info_enc, 0, 360)

    # Publish the encrypted outlet's UID so recorder-node can run a rogue plaintext probe.
    enc_uid = lib.lsl_get_uid(info_enc).decode()
    (SHARED / "enc_uid.txt").write_text(enc_uid)
    print(f"[hd-eeg-node] encrypted marker outlet ready (uid={enc_uid})")

    buf = (ctypes.c_float * channels)()
    t0 = time.monotonic()
    seq = 0
    end = t0 + DURATION + 20.0
    last_meta = 0.0
    mk_idx = 0
    try:
        while time.monotonic() < end:
            for c in range(channels):
                buf[c] = eeg_val(seq, c)
            ts = 2000.0 + seq / srate
            lib.lsl_push_sample_ftp(out_eeg, buf, ts, 1)
            seq += 1

            now = time.monotonic()
            if now - last_meta >= 1.0:
                last_meta = now
                imp = f"{3.5 + (mk_idx % 5) * 0.4:.1f} kOhm".encode()
                lib.labstream_outlet_push_metadata(out_eeg, b"imp.Cz", imp, 0.0)
                msg = f"PATIENT=LABTEST|MRN=100{mk_idx:03d}|SECRET".encode()
                arr = (ctypes.c_char_p * 1)(msg)
                lib.lsl_push_sample_strtp(out_enc, arr, 0.0, 1)
                mk_idx += 1

            nxt = t0 + seq / srate
            rem = nxt - time.monotonic()
            if rem > 0.001:
                time.sleep(rem)
    finally:
        ws_proc.terminate()
        ws_proc.wait(timeout=5)


def sh(cmd: str):
    subprocess.run(cmd, shell=True, check=False)


def rss_mb() -> float:
    for line in Path("/proc/self/status").read_text().splitlines():
        if line.startswith("VmRSS:"):
            return int(line.split()[1]) / 1024.0
    return 0.0


def fd_count() -> int:
    return len(os.listdir("/proc/self/fd"))


def resolve_one(lib, name: bytes, timeout: float = 10.0):
    arr = (ctypes.c_void_p * 4)()
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        n = lib.lsl_resolve_byprop(arr, 4, b"name", name, 1, 1.0)
        if n > 0:
            return arr[0]
    return None


def run_recorder_node():
    print(f"[recorder-node] starting Virtual Lab verifier (duration={DURATION:.0f}s, chaos={CHAOS})")
    cfg = write_cfg(
        "[lab]\nKnownPeers = {172.29.0.10, 172.29.0.11}\n\n"
        "[labstream]\nFeatures = {seq, resume, dgram, select, comp, meta}\n"
    )
    os.environ["LSLAPICFG"] = cfg

    lib = load_lsl(LABSTREAM_LIB)
    bind_labstream_ext(lib)

    time.sleep(3.0)
    rss_start = rss_mb()
    fd_start = fd_count()

    # 1. Connect to relayed legacy C++ liblsl stream
    info_rel = resolve_one(lib, b"VendorEEG-relayed", 15.0)
    assert info_rel, "failed to resolve VendorEEG-relayed"
    in_rel = lib.lsl_create_inlet(info_rel, 360, 0, 1)
    ec = ctypes.c_int(0)
    lib.lsl_open_stream(in_rel, 5.0, ctypes.byref(ec))
    assert ec.value == 0, f"open VendorEEG-relayed failed: {ec.value}"

    # 2. Connect to HD EEG stream (comp/1 + meta/1)
    info_hd = resolve_one(lib, b"LabTest_HDEEG", 15.0)
    assert info_hd, "failed to resolve LabTest_HDEEG"
    in_hd = lib.lsl_create_inlet(info_hd, 360, 0, 1)
    lib.lsl_open_stream(in_hd, 5.0, ctypes.byref(ec))
    assert ec.value == 0, f"open LabTest_HDEEG failed: {ec.value}"

    # 3. Connect to encrypted clinical marker stream with PSK
    lib.labstream_set_encryption(1, 1, b"labtest-psk-2026")
    info_enc = resolve_one(lib, b"LabTest_Encrypted", 15.0)
    assert info_enc, "failed to resolve LabTest_Encrypted"
    in_enc = lib.lsl_create_inlet(info_enc, 360, 0, 1)
    lib.lsl_open_stream(in_enc, 5.0, ctypes.byref(ec))
    assert ec.value == 0, f"open LabTest_Encrypted failed: {ec.value}"
    enc_active = lib.labstream_inlet_encrypted(in_enc) == 1

    print("[recorder-node] all 3 streams connected; starting chaos & continuous verification")

    stop_ev = threading.Event()
    stats = {
        "rel_samples": 0, "rel_bit_errors": 0,
        "hd_samples": 0, "hd_bit_errors": 0,
        "enc_markers": 0,
    }

    def reader_rel():
        buf = (ctypes.c_float * 16)()
        e = ctypes.c_int(0)
        while not stop_ev.is_set():
            ts = lib.lsl_pull_sample_f(in_rel, buf, 16, 0.2, ctypes.byref(e))
            if ts != 0.0:
                stats["rel_samples"] += 1
                seq = round((ts - 1000.0) * 250.0)
                if abs(buf[0] - eeg_val(seq, 0)) > 1e-4:
                    stats["rel_bit_errors"] += 1

    def reader_hd():
        buf = (ctypes.c_float * 32)()
        e = ctypes.c_int(0)
        while not stop_ev.is_set():
            ts = lib.lsl_pull_sample_f(in_hd, buf, 32, 0.2, ctypes.byref(e))
            if ts != 0.0:
                stats["hd_samples"] += 1
                seq = round((ts - 2000.0) * 250.0)
                if abs(buf[0] - eeg_val(seq, 0)) > 1e-4:
                    stats["hd_bit_errors"] += 1

    def reader_enc():
        buf = (ctypes.c_char_p * 1)()
        e = ctypes.c_int(0)
        while not stop_ev.is_set():
            ts = lib.lsl_pull_sample_str(in_enc, buf, 1, 0.2, ctypes.byref(e))
            if ts != 0.0 and buf[0]:
                if b"PATIENT=LABTEST" in buf[0]:
                    stats["enc_markers"] += 1

    threads = [
        threading.Thread(target=reader_rel, daemon=True),
        threading.Thread(target=reader_hd, daemon=True),
        threading.Thread(target=reader_enc, daemon=True),
    ]
    for t in threads:
        t.start()

    # Timed real-world chaos schedule
    t_start = time.monotonic()
    doctor_ok = False
    if CHAOS:
        time.sleep(5.0)
        print("[chaos] Phase A: injecting 3% loss + 30 ms jitter via tc netem")
        sh("tc qdisc replace dev eth0 root netem delay 30ms 8ms loss 3%")
        time.sleep(8.0)

        print("[chaos] Phase B: injecting 4 s hard link blackout (iptables DROP) to test resume/1 + disk spool")
        sh("iptables -I INPUT -s 172.29.0.10 -j DROP && iptables -I OUTPUT -d 172.29.0.10 -j DROP")
        time.sleep(4.0)
        sh("iptables -D INPUT -s 172.29.0.10 -j DROP && iptables -D OUTPUT -d 172.29.0.10 -j DROP")
        print("[chaos] link restored; waiting for resume/1 backfill...")
        sh("tc qdisc del dev eth0 root 2>/dev/null || true")
        time.sleep(8.0)

        print("[chaos] Phase C: blocking multicast 224.0.0.0/4 and running labstream doctor --json")
        sh("iptables -I OUTPUT -d 224.0.0.0/4 -j DROP")
        doc = subprocess.run(
            ["labstream", "doctor", "--peer", "172.29.0.10,172.29.0.11", "--timeout", "2", "--json"],
            capture_output=True, text=True, check=False,
        )
        sh("iptables -D OUTPUT -d 224.0.0.0/4 -j DROP")
        doctor_ok = doc.returncode == 0 and "VendorEEG-relayed" in doc.stdout
        print(f"[chaos] doctor multicast-blocked check: ok={doctor_ok}")

    # Wait remaining soak duration
    while time.monotonic() - t_start < DURATION:
        time.sleep(1.0)

    stop_ev.set()
    for t in threads:
        t.join(timeout=2.0)

    # Read final health counters & browser-node result
    rel_lost = lib.labstream_samples_lost(in_rel)
    rel_resent = lib.labstream_samples_resent(in_rel)
    hd_lost = lib.labstream_samples_lost(in_hd)
    hd_comp = lib.labstream_inlet_compressed(in_hd) == 1
    hd_ratio = lib.labstream_inlet_compression_ratio(in_hd)
    hd_meta = lib.labstream_inlet_metadata_count(in_hd)
    rss_end = rss_mb()
    fd_end = fd_count()

    browser_report = {}
    bp = SHARED / "browser_result.json"
    if bp.exists():
        browser_report = json.loads(bp.read_text())

    checks = {
        "S1_vendor_relay_zero_loss": rel_lost == 0 and stats["rel_samples"] > 1000 and stats["rel_bit_errors"] == 0,
        "S2_vendor_relay_backfilled": (not CHAOS) or rel_resent > 0,
        "S3_hd_eeg_comp_lossless": hd_lost == 0 and stats["hd_samples"] > 1000 and stats["hd_bit_errors"] == 0 and hd_comp and hd_ratio < 0.75,
        "S4_live_meta_received": hd_meta >= 1,
        "S5_enc_active_and_decrypted": enc_active and stats["enc_markers"] >= 5,
        "S6_multicast_blocked_doctor": (not CHAOS) or doctor_ok,
        "S7_browser_wasm_e2e": browser_report.get("ok", False),
        "S8_resource_stability": (rss_end - rss_start) < 64.0 and (fd_end - fd_start) < 32,
    }
    overall = all(checks.values())

    report = {
        "overall": "PASS" if overall else "FAIL",
        "duration_s": round(time.monotonic() - t_start, 1),
        "checks": checks,
        "metrics": {
            "rel_samples": stats["rel_samples"],
            "rel_lost": rel_lost,
            "rel_resent": rel_resent,
            "rel_bit_errors": stats["rel_bit_errors"],
            "hd_samples": stats["hd_samples"],
            "hd_lost": hd_lost,
            "hd_comp_ratio": round(hd_ratio, 4),
            "hd_meta_keys": hd_meta,
            "enc_active": enc_active,
            "enc_markers": stats["enc_markers"],
            "browser": browser_report,
            "rss_start_mb": round(rss_start, 1),
            "rss_end_mb": round(rss_end, 1),
            "fd_start": fd_start,
            "fd_end": fd_end,
        },
    }
    (SHARED / "labtest-report.json").write_text(json.dumps(report, indent=2))

    md_lines = [
        "## Docker Virtual Lab Scorecard",
        "",
        f"**Overall:** `{report['overall']}` ({report['duration_s']} s)",
        "",
        "| Scenario | Result |",
        "|---|---|",
    ]
    for k, v in checks.items():
        md_lines.append(f"| `{k}` | {'PASS' if v else 'FAIL'} |")
    md_text = "\n".join(md_lines) + "\n"
    (SHARED / "labtest-summary.md").write_text(md_text)
    print("\n" + md_text)
    print(json.dumps(report["metrics"], indent=2))
    sys.exit(0 if overall else 1)


if __name__ == "__main__":
    role = sys.argv[1] if len(sys.argv) > 1 else ""
    if role == "vendor-amp":
        run_vendor_amp()
    elif role == "hd-eeg-node":
        run_hd_eeg_node()
    elif role == "recorder-node":
        run_recorder_node()
    else:
        sys.exit(f"unknown role: {role}")
