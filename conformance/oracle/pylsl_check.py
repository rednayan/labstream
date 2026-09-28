#!/usr/bin/env python3
"""Run pylsl against a library and report what works.

pylsl is an application that was written against liblsl and knows nothing
about this project. It reaches the C ABI through `ctypes`, so it exercises the
boundary the way a real user does.

Point it at either library with `--lib`. The same script must pass against
both. A difference is a conformance failure.

    oracle/pylsl_check.py --lib .build/install/lib/liblsl.so
    oracle/pylsl_check.py --lib .build/rustlib/liblsl.so
"""

import argparse
import os
import pathlib
import subprocess
import sys
import textwrap

# Each check runs in its own process, because pylsl loads the library once per
# process and a crash in one check must not hide the rest.
CHECKS = {}


def check(name):
    def add(fn):
        CHECKS[name] = textwrap.dedent(fn.__doc__)
        return fn

    return add


@check("import")
def _import():
    """
    import pylsl
    # `src/common.h:49` holds 117 for release 1.17.
    v = pylsl.library_version()
    assert v == 117, v
    print("PASS library version", v)
    """


@check("round trip")
def _round_trip():
    """
    import time, pylsl
    info = pylsl.StreamInfo("PyRound", "EEG", 2, 100.0, "float32", "pyround_src")
    out = pylsl.StreamOutlet(info)
    time.sleep(0.5)
    found = pylsl.resolve_byprop("name", "PyRound", 1, 10)
    assert found, "the stream was not resolved"
    inlet = pylsl.StreamInlet(found[0])
    inlet.open_stream(5.0)
    time.sleep(0.3)
    for k in range(5):
        out.push_sample([1.0 + k, 2.0], 7.125 + k)
    time.sleep(0.3)
    for k in range(5):
        values, ts = inlet.pull_sample(5.0)
        assert values == [1.0 + k, 2.0], values
        assert ts == 7.125 + k, ts
    print("PASS 5 samples, exact values and timestamps")
    """


@check("pull with no open")
def _implicit_open():
    """
    import time, pylsl
    # liblsl starts the reader on the first pull, so open_stream is optional.
    # `src/data_receiver.cpp:88`. A pull here must report "no sample" and not
    # "the stream is lost".
    info = pylsl.StreamInfo("PyLazy", "EEG", 2, 100.0, "float32", "pylazy_src")
    out = pylsl.StreamOutlet(info)
    time.sleep(0.5)
    found = pylsl.resolve_byprop("name", "PyLazy", 1, 10)
    assert found, "the stream was not resolved"
    inlet = pylsl.StreamInlet(found[0])
    values, ts = inlet.pull_sample(1.0)
    assert values is None and ts is None, (values, ts)
    for k in range(3):
        out.push_sample([9.0 + k, 8.0], 1.5 + k)
    time.sleep(0.3)
    values, ts = inlet.pull_sample(5.0)
    assert values == [9.0, 8.0], values
    print("PASS an empty pull reports no sample, and the reader started")
    """


@check("description tree")
def _desc():
    """
    import time, pylsl
    info = pylsl.StreamInfo("PyDesc", "EEG", 3, 100.0, "float32", "pydesc_src")
    chans = info.desc().append_child("channels")
    for lab in ["C3", "C4", "Cz"]:
        c = chans.append_child("channel")
        c.append_child_value("label", lab)
        c.append_child_value("unit", "microvolts")
    info.desc().append_child_value("manufacturer", "Acme & Co <test>")
    out = pylsl.StreamOutlet(info)
    time.sleep(0.5)
    found = pylsl.resolve_byprop("name", "PyDesc", 1, 10)
    assert found, "the stream was not resolved"
    # The discovery answer carries no tree. Only the whole description does.
    assert found[0].desc().child("channels").empty(), "a short answer carried a tree"
    inlet = pylsl.StreamInlet(found[0])
    full = inlet.info(10)
    d = full.desc()
    labels = []
    c = d.child("channels").child("channel")
    while not c.empty():
        labels.append(c.child_value("label"))
        c = c.next_sibling()
    assert labels == ["C3", "C4", "Cz"], labels
    assert d.child_value("manufacturer") == "Acme & Co <test>"
    assert full.channel_count() == 3
    assert full.nominal_srate() == 100.0
    print("PASS 3 channel labels, one escaped value, and no tree in the answer")
    """


@check("chunks")
def _chunks():
    """
    import time, pylsl
    info = pylsl.StreamInfo("PyChunk", "EEG", 2, 100.0, "float32", "pychunk_src")
    out = pylsl.StreamOutlet(info)
    time.sleep(0.5)
    found = pylsl.resolve_byprop("name", "PyChunk", 1, 10)
    assert found, "the stream was not resolved"
    inlet = pylsl.StreamInlet(found[0])
    inlet.open_stream(5.0)
    time.sleep(0.3)
    # The timestamp names the last sample. The rest count backward by 1/rate.
    out.push_chunk([[1.0, 2.0], [3.0, 4.0], [5.0, 6.0]], 10.02)
    time.sleep(0.3)
    values, stamps = inlet.pull_chunk(2.0, 10)
    assert values == [[1.0, 2.0], [3.0, 4.0], [5.0, 6.0]], values
    assert stamps[0] == 10.0, stamps
    assert stamps[1] == 10.0 + 1.0 / 100.0, stamps
    assert stamps[2] == 10.0 + 1.0 / 100.0 + 1.0 / 100.0, stamps
    print("PASS 3 samples, timestamps rebuilt in the same order")
    """


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--lib", required=True, help="the liblsl.so to load")
    ap.add_argument("--only", default=None, help="run one check by name")
    args = ap.parse_args()

    lib = pathlib.Path(args.lib).resolve()
    if not lib.exists():
        print(f"no library at {lib}")
        return 1

    env = dict(os.environ, PYLSL_LIB=str(lib))
    print(f"=== pylsl against {lib}")
    failed = 0
    for name, body in CHECKS.items():
        if args.only and args.only != name:
            continue
        proc = subprocess.run([sys.executable, "-c", body], env=env,
                              capture_output=True, text=True, timeout=120)
        line = ""
        for l in proc.stdout.splitlines():
            if l.startswith("PASS"):
                line = l[5:]
        if proc.returncode == 0 and line:
            print(f"  {name:22} pass   {line}")
        else:
            failed += 1
            reason = (proc.stderr.strip().splitlines() or ["no output"])[-1]
            print(f"  {name:22} FAIL   {reason}")

    print()
    total = len(CHECKS) if not args.only else 1
    print(f"{total - failed} of {total} checks pass")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
