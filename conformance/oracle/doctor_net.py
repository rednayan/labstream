#!/usr/bin/env python3
"""Verify step 10 (`docs/roadmap.md`, `docs/discovery.md`) on a network that blocks multicast.

The script needs no root:
1. It runs itself inside a new user namespace and network namespace
   (`unshare -Urn`).
2. It creates a second network namespace and connects the two with a `veth`
   pair (`10.77.0.1/24` for the outlet, `10.77.0.2/24` for the inlet).
3. It installs `tc` filters to block multicast and broadcast, or all UDP.
4. It tests both a `labstream` outlet and a `liblsl` oracle outlet against:
   - `FallbackDiscovery = off` (must find 0 streams),
   - `FallbackDiscovery = auto` with multicast and `255.255.255.255` blocked
     (must find the stream in less than 2 s),
   - `FallbackDiscovery = auto` with multicast and all broadcast blocked
     (must find the stream via `/24` unicast in less than 2 s),
   - `labstream doctor --json` (must name multicast blocking as the cause and
     list the stream found by fallback),
   - all UDP blocked with `KnownPeers = {10.77.0.1}` (TCP `LSL:shortinfo`
     must find the stream in less than 2 s, and `labstream doctor --peer`
     must name UDP blocking while TCP works).
"""

import ctypes
import json
import os
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent
ORACLE_LIB = ROOT / "conformance" / ".build" / "install" / "lib" / "liblsl.so"
LABSTREAM_LIB = ROOT / "target" / "release" / "liblsl.so"
DOCTOR_BIN = ROOT / "target" / "release" / "labstream"

OUTLET_ADDR = "10.77.0.1"
INLET_ADDR = "10.77.0.2"
SUBNET_BCAST = "10.77.0.255"
INSIDE = "LABSTREAM_DOCTOR_NET_INSIDE"


def sh(*cmd: str) -> None:
    subprocess.run(list(cmd), check=True)


class Link:
    """Two network namespaces connected by a veth pair with tc packet filters."""

    def __init__(self) -> None:
        self.has_root: dict[str, bool] = {}
        sh("ip", "link", "set", "lo", "up")
        self.holder = subprocess.Popen(["unshare", "-n", "sleep", "infinity"])
        time.sleep(0.3)
        self.pid = str(self.holder.pid)
        sh("ip", "link", "add", "va", "type", "veth", "peer", "name", "vb")
        sh("ip", "link", "set", "vb", "netns", self.pid)
        sh("ip", "addr", "add", f"{OUTLET_ADDR}/24", "brd", SUBNET_BCAST, "dev", "va")
        sh("ip", "link", "set", "va", "up")
        self.inside("ip", "link", "set", "lo", "up")
        self.inside("ip", "addr", "add", f"{INLET_ADDR}/24", "brd", SUBNET_BCAST, "dev", "vb")
        self.inside("ip", "link", "set", "vb", "up")

    def inside(self, *cmd: str) -> None:
        sh("nsenter", "-t", self.pid, "-n", *cmd)

    def prefix(self) -> list[str]:
        return ["nsenter", "-t", self.pid, "-n"]

    def _reset_prio(self, run, dev: str) -> None:
        if self.has_root.get(dev):
            run("tc", "qdisc", "del", "dev", dev, "root")
        run("tc", "qdisc", "add", "dev", dev, "root", "handle", "1:", "prio")
        run(
            "tc", "qdisc", "add", "dev", dev, "parent", "1:3", "handle", "30:",
            "netem", "loss", "100%",
        )
        self.has_root[dev] = True

    def block_multicast(self, also_block_subnet_bcast: bool = False) -> None:
        """Drop IPv4 multicast, IPv6 multicast, and limited (and optional subnet) broadcast."""
        for run, dev in ((sh, "va"), (self.inside, "vb")):
            self._reset_prio(run, dev)
            # IPv4 multicast 224.0.0.0/4
            run(
                "tc", "filter", "add", "dev", dev, "parent", "1:0", "protocol", "ip",
                "prio", "1", "u32", "match", "ip", "dst", "224.0.0.0/4", "flowid", "1:3",
            )
            # IPv4 limited broadcast 255.255.255.255/32
            run(
                "tc", "filter", "add", "dev", dev, "parent", "1:0", "protocol", "ip",
                "prio", "2", "u32", "match", "ip", "dst", "255.255.255.255/32", "flowid", "1:3",
            )
            # IPv6 multicast ff00::/8
            run(
                "tc", "filter", "add", "dev", dev, "parent", "1:0", "protocol", "ipv6",
                "prio", "3", "u32", "match", "ip6", "dst", "ff00::/8", "flowid", "1:3",
            )
            if also_block_subnet_bcast:
                run(
                    "tc", "filter", "add", "dev", dev, "parent", "1:0", "protocol", "ip",
                    "prio", "4", "u32", "match", "ip", "dst", f"{SUBNET_BCAST}/32",
                    "flowid", "1:3",
                )

    def block_all_udp(self) -> None:
        """Drop all IPv4 and IPv6 UDP packets on both ends, leaving TCP open."""
        for run, dev in ((sh, "va"), (self.inside, "vb")):
            self._reset_prio(run, dev)
            run(
                "tc", "filter", "add", "dev", dev, "parent", "1:0", "protocol", "ip",
                "prio", "1", "u32", "match", "ip", "protocol", "17", "0xff", "flowid", "1:3",
            )
            run(
                "tc", "filter", "add", "dev", dev, "parent", "1:0", "protocol", "ipv6",
                "prio", "2", "u32", "match", "ip6", "protocol", "17", "0xff", "flowid", "1:3",
            )

    def flush_arp(self) -> None:
        """Clear neighbor entries so /24 unicast fallback cannot rely on warm ARP."""
        subprocess.run(["ip", "neigh", "flush", "dev", "va"], check=False)
        subprocess.run([*self.prefix(), "ip", "neigh", "flush", "dev", "vb"], check=False)

    def close(self) -> None:
        self.holder.kill()
        self.holder.wait()


def run_outlet_child(lib_path: str, name: str) -> None:
    """Subprocess entrypoint: open one outlet and stay alive until killed."""
    lib = ctypes.CDLL(lib_path)
    lib.lsl_create_streaminfo.argtypes = [
        ctypes.c_char_p, ctypes.c_char_p, ctypes.c_int32, ctypes.c_double,
        ctypes.c_int32, ctypes.c_char_p,
    ]
    lib.lsl_create_streaminfo.restype = ctypes.c_void_p
    lib.lsl_create_outlet.argtypes = [ctypes.c_void_p, ctypes.c_int32, ctypes.c_int32]
    lib.lsl_create_outlet.restype = ctypes.c_void_p

    info = lib.lsl_create_streaminfo(
        name.encode(), b"EEG", 4, 250.0, 1, f"src-{name}".encode()
    )
    outlet = lib.lsl_create_outlet(info, 0, 360)
    if not outlet:
        sys.stderr.write("failed to open outlet\n")
        sys.exit(1)
    print("READY", flush=True)
    while True:
        time.sleep(1.0)


def run_resolve_child(lib_path: str, name: str, timeout: float) -> None:
    """Subprocess entrypoint: resolve `name` and print JSON timing result."""
    lib = ctypes.CDLL(lib_path)
    lib.lsl_resolve_byprop.argtypes = [
        ctypes.POINTER(ctypes.c_void_p), ctypes.c_uint32, ctypes.c_char_p,
        ctypes.c_char_p, ctypes.c_int32, ctypes.c_double,
    ]
    lib.lsl_resolve_byprop.restype = ctypes.c_int32
    lib.lsl_get_name.argtypes = [ctypes.c_void_p]
    lib.lsl_get_name.restype = ctypes.c_char_p
    lib.lsl_destroy_streaminfo.argtypes = [ctypes.c_void_p]
    lib.lsl_destroy_streaminfo.restype = None

    buf = (ctypes.c_void_p * 8)()
    t0 = time.monotonic()
    n = lib.lsl_resolve_byprop(buf, 8, b"name", name.encode(), 1, timeout)
    dt = time.monotonic() - t0
    names = []
    for i in range(max(0, n)):
        raw = lib.lsl_get_name(buf[i])
        names.append(raw.decode() if raw else "")
        lib.lsl_destroy_streaminfo(buf[i])
    print(json.dumps({"count": n, "elapsed_s": round(dt, 4), "names": names}), flush=True)


def write_cfg(directory: Path, name: str, text: str) -> Path:
    path = directory / name
    path.write_text(text)
    return path


def start_outlet(lib_path: Path, name: str) -> subprocess.Popen:
    env = dict(os.environ)
    env.pop("LSLAPICFG", None)
    proc = subprocess.Popen(
        [sys.executable, __file__, "--role", "outlet", "--lib", str(lib_path), "--name", name],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        env=env,
    )
    line = proc.stdout.readline().strip() if proc.stdout else ""
    if line != "READY":
        err = proc.stderr.read() if proc.stderr else ""
        proc.kill()
        raise RuntimeError(f"outlet {name} ({lib_path}) failed to start: {line!r} {err}")
    # Wait briefly for the outlet responder threads to settle.
    time.sleep(0.15)
    return proc


def resolve_in_inlet_ns(link: Link, cfg: Path, name: str, timeout: float = 2.0) -> dict:
    env = dict(os.environ, LSLAPICFG=str(cfg))
    out = subprocess.check_output(
        [
            *link.prefix(),
            sys.executable, __file__,
            "--role", "resolve",
            "--lib", str(LABSTREAM_LIB),
            "--name", name,
            "--timeout", str(timeout),
        ],
        text=True,
        env=env,
    )
    return json.loads(out.strip())


def run_doctor_in_inlet_ns(link: Link, cfg: Path, extra_args: list[str]) -> tuple[int, dict]:
    env = dict(os.environ, LSLAPICFG=str(cfg))
    proc = subprocess.run(
        [*link.prefix(), str(DOCTOR_BIN), "doctor", "--json", *extra_args],
        capture_output=True,
        text=True,
        env=env,
    )
    if not proc.stdout.strip():
        raise RuntimeError(f"doctor produced no JSON (rc={proc.returncode}): {proc.stderr}")
    return proc.returncode, json.loads(proc.stdout)


def check_scenario(link: Link, tmp: Path, label: str, outlet_lib: Path) -> None:
    print(f"\n=== Outlet: {label} ({outlet_lib.name}) ===")
    name = f"DoctorNet_{label}"
    outlet = start_outlet(outlet_lib, name)
    try:
        cfg_off = write_cfg(tmp, f"{label}_off.cfg", "[labstream]\nFallbackDiscovery = off\n")
        cfg_auto = write_cfg(tmp, f"{label}_auto.cfg", "[labstream]\nFallbackDiscovery = auto\n")
        cfg_kp = write_cfg(
            tmp,
            f"{label}_kp.cfg",
            f"[lab]\nKnownPeers = {{{OUTLET_ADDR}}}\n[labstream]\nFallbackDiscovery = auto\n",
        )

        # 1. Multicast + 255.255.255.255 blocked, FallbackDiscovery = off -> 0 streams.
        link.block_multicast(also_block_subnet_bcast=False)
        res_off = resolve_in_inlet_ns(link, cfg_off, name, timeout=1.0)
        print(f"  [1] mcast blocked, fallback=off:   count={res_off['count']} in {res_off['elapsed_s']:.3f}s")
        assert res_off["count"] == 0, f"expected 0 streams with fallback=off, got {res_off}"

        # 2. Multicast + 255.255.255.255 blocked, FallbackDiscovery = auto -> found in < 2 s.
        res_bcast = resolve_in_inlet_ns(link, cfg_auto, name, timeout=2.5)
        print(
            f"  [2] mcast blocked, fallback=auto:  count={res_bcast['count']} "
            f"in {res_bcast['elapsed_s']:.3f}s ({res_bcast['names']})"
        )
        assert res_bcast["count"] >= 1 and name in res_bcast["names"], f"failed: {res_bcast}"
        assert res_bcast["elapsed_s"] < 2.0, f"too slow: {res_bcast}"

        # 3. Multicast + ALL broadcast (including 10.77.0.255) blocked, cold ARP -> /24 unicast < 2 s.
        link.block_multicast(also_block_subnet_bcast=True)
        link.flush_arp()
        res_uni = resolve_in_inlet_ns(link, cfg_auto, name, timeout=2.5)
        print(
            f"  [3] mcast+bcast blocked, cold ARP: count={res_uni['count']} "
            f"in {res_uni['elapsed_s']:.3f}s ({res_uni['names']})"
        )
        assert res_uni["count"] >= 1 and name in res_uni["names"], f"failed: {res_uni}"
        assert res_uni["elapsed_s"] < 2.0, f"too slow: {res_uni}"

        # 4. `labstream doctor --json` names multicast blocking and lists stream via fallback.
        rc, doc = run_doctor_in_inlet_ns(link, cfg_auto, ["--timeout", "1.4"])
        disc = next(c for c in doc["checks"] if c["name"] == "discovery")
        found = [s for s in doc["streams"] if s["name"] == name]
        print(
            f"  [4] doctor (mcast+bcast blocked):  rc={rc} status={disc['status']} "
            f"cause={disc['cause']!r} via={[s['via'] for s in found]}"
        )
        assert disc["status"] == "warn", f"expected warn on discovery check: {disc}"
        assert disc["cause"] and "multicast" in disc["cause"].lower(), f"cause did not name multicast: {disc}"
        assert found and found[0]["via"] == "fallback", f"expected fallback stream: {doc['streams']}"

        # 5. All UDP dropped + KnownPeers -> TCP LSL:shortinfo finds outlet in < 2 s.
        link.block_all_udp()
        res_tcp = resolve_in_inlet_ns(link, cfg_kp, name, timeout=2.5)
        print(
            f"  [5] all UDP blocked + KnownPeers:  count={res_tcp['count']} "
            f"in {res_tcp['elapsed_s']:.3f}s ({res_tcp['names']})"
        )
        assert res_tcp["count"] >= 1 and name in res_tcp["names"], f"TCP fallback failed: {res_tcp}"
        assert res_tcp["elapsed_s"] < 2.0, f"TCP fallback too slow: {res_tcp}"

        # 6. `labstream doctor --peer 10.77.0.1` names UDP blocking while TCP works.
        rc_peer, doc_peer = run_doctor_in_inlet_ns(
            link, cfg_auto, ["--peer", OUTLET_ADDR, "--timeout", "1.2"]
        )
        peer_chk = next(c for c in doc_peer["checks"] if c["name"] == f"peer:{OUTLET_ADDR}")
        print(
            f"  [6] doctor --peer (all UDP drop):  rc={rc_peer} status={peer_chk['status']} "
            f"cause={peer_chk['cause']!r}"
        )
        assert peer_chk["status"] == "warn", f"expected warn on peer check: {peer_chk}"
        assert peer_chk["cause"] and "udp" in peer_chk["cause"].lower(), f"cause did not name UDP: {peer_chk}"
    finally:
        outlet.kill()
        outlet.wait()


def main() -> int:
    if "--role" in sys.argv:
        role = sys.argv[sys.argv.index("--role") + 1]
        lib = sys.argv[sys.argv.index("--lib") + 1]
        name = sys.argv[sys.argv.index("--name") + 1]
        if role == "outlet":
            run_outlet_child(lib, name)
        elif role == "resolve":
            timeout = float(sys.argv[sys.argv.index("--timeout") + 1])
            run_resolve_child(lib, name, timeout)
        return 0

    for path in (LABSTREAM_LIB, DOCTOR_BIN):
        if not path.exists():
            sys.stderr.write(f"missing {path}; run `cargo build --release -p labstream-capi -p labstream-cli`\n")
            return 2

    if not os.environ.get(INSIDE):
        env = dict(os.environ, **{INSIDE: "1"})
        return subprocess.call(["unshare", "-Urn", sys.executable, __file__, *sys.argv[1:]], env=env)

    link = Link()
    try:
        with tempfile.TemporaryDirectory(prefix="labstream-doctor-net-") as td:
            tmp = Path(td)
            check_scenario(link, tmp, "labstream", LABSTREAM_LIB)
            if ORACLE_LIB.exists():
                check_scenario(link, tmp, "liblsl", ORACLE_LIB)
            else:
                print(f"\n(skipping liblsl oracle; {ORACLE_LIB} not built)")
        print("\nALL STEP 10 BLOCKED-MULTICAST SCENARIOS PASSED")
        return 0
    finally:
        link.close()


if __name__ == "__main__":
    sys.exit(main())
