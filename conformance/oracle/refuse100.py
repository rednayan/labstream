#!/usr/bin/env python3
"""Test what happens when a peer asks for protocol 1.00.

Protocol 1.00 carries every sample in a Boost archive (`src/sample.cpp:330`),
vendored inside liblsl at `src/portable_archive/`. This implementation does not
write or read one, and that is a decision rather than an omission.

**A decision still has to behave.** Refusing well means two things:

1. Our inlet must stop with a message that names the version, and not decode
   the bytes of an archive as if they were 1.10 samples.
2. Our outlet must let a 1.00 peer stop, rather than leave it waiting.

The control is liblsl on both sides with the same configuration. If that pair
does not work, the configuration never took effect and nothing below means
anything.

    oracle/refuse100.py
"""

import os
import pathlib
import subprocess
import sys
import tempfile
import time

ROOT = pathlib.Path(__file__).resolve().parent.parent
ORACLE = ROOT / ".build" / "peer"
RUST = ROOT / "target" / "release" / "lsl-peer"

# Two samples, written the way the peers read them: the bits of a double, then
# one bit pattern for each channel.
INPUT = "3ff0000000000000 3f800000 40000000\n4000000000000000 40400000 40800000\n"


def run(outlet, inlet, cfg, name, seconds=12):
    """Drive one pair and report how the inlet ended."""
    env = dict(os.environ, LSLAPICFG=str(cfg))
    out = subprocess.Popen(
        [str(outlet), "outlet", "--name", name, "--format", "float32",
         "--channels", "2", "--input", str(cfg.parent / "samples.txt")],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        text=True, env=env)
    try:
        time.sleep(2)
        out.stdin.write("GO\n")
        out.stdin.flush()
        started = time.time()
        try:
            p = subprocess.run(
                [str(inlet), "inlet", "--name", name, "--format", "float32",
                 "--channels", "2", "--count", "2", "--output", os.devnull,
                 "--timeout", "6"],
                capture_output=True, text=True, env=env, timeout=seconds)
            note = (p.stderr.strip().splitlines() or [""])[-1]
            return p.returncode, time.time() - started, note
        except subprocess.TimeoutExpired:
            return None, time.time() - started, "the inlet never stopped"
    finally:
        out.terminate()
        try:
            out.wait(timeout=5)
        except subprocess.TimeoutExpired:
            out.kill()


def main():
    if not ORACLE.exists() or not RUST.exists():
        print(f"build both peers first: {ORACLE} and {RUST}")
        return 1

    work = pathlib.Path(tempfile.mkdtemp(prefix="lsl-100-"))
    cfg = work / "v100.cfg"
    cfg.write_text("; ask for the old protocol\n[tuning]\nUseProtocolVersion = 100\n")
    (work / "samples.txt").write_text(INPUT)

    print("=== a peer that asks for protocol 1.00")
    failed = 0

    code, secs, note = run(ORACLE, ORACLE, cfg, "R100a")
    if code == 0:
        print(f"  control, liblsl on both sides      pass   the pair works, so the file took effect")
    else:
        failed += 1
        print(f"  control, liblsl on both sides      FAIL   exit={code} {note}")
        print("  Nothing below can be read while the control fails.")

    # Our inlet against a liblsl outlet set to 1.00. The server agrees to 1.00,
    # and the inlet has to name that rather than read the bytes as 1.10.
    code, secs, note = run(ORACLE, RUST, cfg, "R100b")
    named = "protocol 100" in note
    if code not in (0, None) and named:
        print(f"  our inlet, a 1.00 server           pass   stopped in {secs:.1f}s: {note}")
    else:
        failed += 1
        print(f"  our inlet, a 1.00 server           FAIL   exit={code} {note}")

    # A liblsl inlet set to 1.00 against our outlet. We close the connection,
    # and the peer has to stop rather than wait for ever.
    code, secs, note = run(RUST, ORACLE, cfg, "R100c")
    if code is not None and code != 0:
        print(f"  a 1.00 inlet, our outlet           pass   stopped in {secs:.1f}s: {note}")
    else:
        failed += 1
        state = "it kept waiting" if code is None else "it read a stream it cannot read"
        print(f"  a 1.00 inlet, our outlet           FAIL   {state}")

    print()
    if failed:
        print(f"{failed} check(s) failed")
        return 1
    print("protocol 1.00 is refused, and both sides stop with a reason")
    return 0


if __name__ == "__main__":
    sys.exit(main())
