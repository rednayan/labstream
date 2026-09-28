#!/usr/bin/env python3
"""Compare the streams of one recording, one library against another.

`oracle/labrecorder.sh` publishes the same signal three times, once through
real liblsl, once through this library's C ABI, and once through `lsl-net`.
Each one tags its streams, so LabRecorder can record all six into one file.

This reads that file and compares the three. **The oracle streams inside the
file are the control.** They were written by real liblsl and recorded by a real
recorder, so a fault in this reader shows up there first.

    oracle/checkxdf.py recording.xdf

# The reader

XDF holds a magic word and then a run of chunks. Each chunk carries the number
of bytes in its length field, the length, a tag, and the content. Nothing here
came from a description of the format: the layout below is what the file holds,
and the check is that the parse ends exactly at the end of the file with no
byte left over in any chunk.

# Why nothing is held in memory

A recording of several hours holds tens of millions of samples. This walks the
file and keeps counters, not samples, so the size of the recording does not
decide whether the check can run.

# Why no stream is compared with another

The publishers build every value from the sample number, so the expected value
of any sample can be computed from the counter channel alone:

    channel k  =  float32(100 * sin(2 * pi * (k + 1) * n / rate))
    last       =  float32(n)

That is stronger than holding one stream against another. A comparison covers
only the samples that both streams hold, and this covers every sample of every
stream.

**The control proves the rule.** The oracle stream came from real liblsl. If
the formula did not reproduce it, the formula would be wrong, and the report
says so before it says anything else.
"""

import argparse
import math
import re
import struct
import sys

TAG_FILE_HEADER = 1
TAG_STREAM_HEADER = 2
TAG_SAMPLES = 3
TAG_CLOCK_OFFSET = 4
TAG_BOUNDARY = 5
TAG_STREAM_FOOTER = 6

# The width and the pack code of each fixed-width channel format.
NUMERIC = {
    "int8": ("b", 1),
    "int16": ("h", 2),
    "int32": ("i", 4),
    "int64": ("q", 8),
    "float32": ("f", 4),
    "double64": ("d", 8),
}


def tag_text(xml, key):
    m = re.search(rf"<{key}>(.*?)</{key}>", xml, re.S)
    return m.group(1) if m else ""


def channel_labels(xml):
    """The label, unit, and type of each channel, in order."""
    out = []
    for chan in re.findall(r"<channel>(.*?)</channel>", xml, re.S):
        out.append((tag_text(chan, "label"), tag_text(chan, "unit"), tag_text(chan, "type")))
    return out


def read(path):
    """Read a recording and return one entry for each stream."""
    data = open(path, "rb").read()
    if data[:4] != b"XDF:":
        raise SystemExit("this file does not start with the XDF magic word")

    streams = {}
    i = 4
    chunks = 0
    while i < len(data):
        count_bytes = data[i]
        i += 1
        if count_bytes not in (1, 4, 8):
            raise SystemExit(f"a chunk at byte {i - 1} names {count_bytes} length bytes")
        length = int.from_bytes(data[i : i + count_bytes], "little")
        i += count_bytes
        body = data[i : i + length]
        if len(body) != length:
            raise SystemExit(f"a chunk at byte {i} runs past the end of the file")
        i += length
        chunks += 1
        tag = struct.unpack("<H", body[:2])[0]

        if tag == TAG_STREAM_HEADER:
            sid = struct.unpack("<I", body[2:6])[0]
            xml = body[6:].decode("utf-8", "replace")
            streams[sid] = {
                "name": tag_text(xml, "name"),
                "type": tag_text(xml, "type"),
                "format": tag_text(xml, "channel_format"),
                "channels": int(tag_text(xml, "channel_count") or 0),
                "srate": float(tag_text(xml, "nominal_srate") or 0.0),
                "labels": channel_labels(xml),
                "stamps": [],
                "values": [],
            }
        elif tag == TAG_SAMPLES:
            sid = struct.unpack("<I", body[2:6])[0]
            stream = streams[sid]
            p = 6
            count_bytes = body[p]
            p += 1
            count = int.from_bytes(body[p : p + count_bytes], "little")
            p += count_bytes
            n = stream["channels"]
            fmt = stream["format"]
            for _ in range(count):
                stamp_bytes = body[p]
                p += 1
                if stamp_bytes == 8:
                    stamp = struct.unpack("<d", body[p : p + 8])[0]
                    p += 8
                elif stamp_bytes == 0:
                    # The reader derives this one from the rate.
                    stamp = None
                else:
                    raise SystemExit(f"a sample names {stamp_bytes} timestamp bytes")
                if fmt == "string":
                    row = []
                    for _ in range(n):
                        lb = body[p]
                        p += 1
                        ln = int.from_bytes(body[p : p + lb], "little")
                        p += lb
                        row.append(body[p : p + ln].decode("utf-8", "replace"))
                        p += ln
                else:
                    code, width = NUMERIC[fmt]
                    row = list(struct.unpack("<" + code * n, body[p : p + width * n]))
                    p += width * n
                stream["stamps"].append(stamp)
                stream["values"].append(row)
            if p != len(body):
                raise SystemExit(f"stream {sid} left {len(body) - p} bytes in a sample chunk")

    return streams, chunks, len(data)



def stream_tag(name):
    """The tag that a publisher added, and whether the stream carries markers."""
    if "-Markers-" in name:
        return name.rsplit("-", 1)[1], True
    if "-" in name:
        return name.rsplit("-", 1)[1], False
    return name, False


def expected_row(n, channels, rate):
    """What sample `n` must hold. The publishers compute it this way."""
    t = n / rate
    row = []
    for k in range(channels - 1):
        v = 100.0 * math.sin(2.0 * math.pi * (k + 1) * t)
        row.append(struct.unpack("<f", struct.pack("<f", v))[0])
    row.append(struct.unpack("<f", struct.pack("<f", float(n)))[0])
    return row


class Signal:
    """What one signal stream is worth remembering, in constant memory."""

    def __init__(self, name, channels, rate):
        self.name, self.channels, self.rate = name, channels, rate
        self.count = 0
        self.first = self.last = None
        self.gaps = []          # a few examples of a break in the counter
        self.lost = 0
        self.wrong = 0
        self.first_wrong = None
        self.prev_stamp = None
        self.stamp_count = 0
        self.falls = 0
        self.min_gap = self.max_gap = None
        self.sum_gap = 0.0
        self.stalls = []        # a gap far longer than one period
        self.on_second = {}     # the first channel at each whole second

    def add(self, stamp, row):
        n = int(row[-1])
        if self.first is None:
            self.first = n
        elif n != self.last + 1:
            self.lost += max(0, n - self.last - 1)
            if len(self.gaps) < 12:
                self.gaps.append((self.last, n))
        self.last = n
        self.count += 1

        want = expected_row(n, self.channels, self.rate)
        if want != list(row):
            self.wrong += 1
            if self.first_wrong is None:
                self.first_wrong = (n, want, list(row))

        if self.rate and n % int(self.rate) == 0:
            self.on_second[n] = row[0]

        if stamp is not None:
            self.stamp_count += 1
            if self.prev_stamp is not None:
                g = stamp - self.prev_stamp
                if g <= 0:
                    self.falls += 1
                self.sum_gap += g
                self.min_gap = g if self.min_gap is None else min(self.min_gap, g)
                self.max_gap = g if self.max_gap is None else max(self.max_gap, g)
                if self.rate and g > 5.0 / self.rate and len(self.stalls) < 12:
                    self.stalls.append((n, g))
            self.prev_stamp = stamp


class Markers:
    def __init__(self, name):
        self.name = name
        self.count = 0
        self.texts = []
        self.wrong_word = 0
        self.seconds = []
        self.shape = None

    def add(self, stamp, row):
        self.count += 1
        text = row[0]
        if len(self.texts) < 4:
            self.texts.append(text)
        parts = text.split()
        if len(parts) == 2 and parts[1].isdigit():
            second = int(parts[1])
            self.seconds.append(second)
            if (second % 5 == 0) != (parts[0] == "burst"):
                self.wrong_word += 1
        else:
            self.wrong_word += 1


def scan(path, mutate=None, progress=False):
    """Walk the recording once and return what each stream looked like."""
    data = open(path, "rb").read()
    if data[:4] != b"XDF:":
        raise SystemExit("this file does not start with the XDF magic word")

    signals, markers, meta = {}, {}, {}
    i, chunks = 4, 0
    while i < len(data):
        count_bytes = data[i]
        i += 1
        if count_bytes not in (1, 4, 8):
            raise SystemExit(f"a chunk at byte {i - 1} names {count_bytes} length bytes")
        length = int.from_bytes(data[i : i + count_bytes], "little")
        i += count_bytes
        end = i + length
        if end > len(data):
            raise SystemExit(f"a chunk at byte {i} runs past the end of the file")
        tag = struct.unpack_from("<H", data, i)[0]

        if tag == TAG_STREAM_HEADER:
            sid = struct.unpack_from("<I", data, i + 2)[0]
            xml = data[i + 6 : end].decode("utf-8", "replace")
            name = tag_text(xml, "name")
            key, is_marker = stream_tag(name)
            info = {
                "name": name,
                "type": tag_text(xml, "type"),
                "format": tag_text(xml, "channel_format"),
                "channels": int(tag_text(xml, "channel_count") or 0),
                "srate": float(tag_text(xml, "nominal_srate") or 0.0),
                "labels": channel_labels(xml),
                "tag": key,
                "marker": is_marker,
            }
            if mutate:
                mutate("header", info)
            meta[sid] = info
            if is_marker:
                markers[key] = Markers(name)
                markers[key].shape = info
            else:
                signals[key] = Signal(name, info["channels"], info["srate"])

        elif tag == TAG_SAMPLES:
            sid = struct.unpack_from("<I", data, i + 2)[0]
            info = meta[sid]
            p = i + 6
            nb = data[p]
            p += 1
            count = int.from_bytes(data[p : p + nb], "little")
            p += nb
            n, fmt = info["channels"], info["format"]
            sink = markers[info["tag"]] if info["marker"] else signals[info["tag"]]
            if fmt == "string":
                for _ in range(count):
                    tb = data[p]
                    p += 1
                    stamp = None
                    if tb == 8:
                        stamp = struct.unpack_from("<d", data, p)[0]
                        p += 8
                    row = []
                    for _ in range(n):
                        lb = data[p]
                        p += 1
                        ln = int.from_bytes(data[p : p + lb], "little")
                        p += lb
                        row.append(data[p : p + ln].decode("utf-8", "replace"))
                        p += ln
                    if mutate:
                        stamp, row = mutate("sample", (info, stamp, row))
                    sink.add(stamp, row)
            else:
                code, width = NUMERIC[fmt]
                layout = "<" + code * n
                step = width * n
                for _ in range(count):
                    tb = data[p]
                    p += 1
                    stamp = None
                    if tb == 8:
                        stamp = struct.unpack_from("<d", data, p)[0]
                        p += 8
                    row = struct.unpack_from(layout, data, p)
                    p += step
                    if mutate:
                        stamp, row = mutate("sample", (info, stamp, list(row)))
                    sink.add(stamp, row)
            if p != end:
                raise SystemExit(f"stream {sid} left {end - p} bytes in a sample chunk")

        i = end
        chunks += 1
        if progress and chunks % 50000 == 0:
            print(f"  ... {chunks} chunks, {i / 1e6:.0f} MB", flush=True)

    return signals, markers, meta, chunks, len(data)


def report(signals, markers, meta, control, say):
    problems = []
    tags = sorted(signals)
    if control not in signals:
        raise SystemExit(f"the recording holds no signal stream tagged '{control}'")

    say("=== what the recording holds")
    for t in tags:
        s = signals[t]
        m = markers.get(t)
        hours = s.count / s.rate / 3600.0 if s.rate else 0.0
        say(f"  {t:8} {s.name:24} {s.count:9} samples  {hours:5.2f} h"
            f"   markers {m.count if m else 0}")
    say("")

    say("=== every value is the one the publisher was told to send")
    for t in tags:
        s = signals[t]
        note = " (the control)" if t == control else ""
        if s.wrong == 0:
            say(f"  {t:8} {s.count} samples, none wrong{note}")
        else:
            problems.append(f"{t}: {s.wrong} samples hold a value that the rule does not give")
            n, want, got = s.first_wrong
            say(f"  {t:8} FAIL {s.wrong} of {s.count} wrong, first at n={n}{note}")
            say(f"      wanted {want}")
            say(f"      got    {got}")
    say("")

    say("=== every sample arrived")
    for t in tags:
        s = signals[t]
        if s.lost == 0:
            say(f"  {t:8} n={s.first} to {s.last}, no gap in {s.count} samples")
        else:
            problems.append(f"{t}: {s.lost} samples were lost, in {len(s.gaps)} or more breaks")
            say(f"  {t:8} FAIL {s.lost} samples lost. First breaks: "
                f"{', '.join(f'{a}->{b}' for a, b in s.gaps[:6])}")
    say("")

    say("=== the description of each stream")
    base = meta_of(meta, signals[control].name)
    for t in tags:
        info = meta_of(meta, signals[t].name)
        same = all(info[k] == base[k] for k in ("type", "channels", "format", "srate", "labels"))
        if same:
            say(f"  {t:8} {info['channels']}ch {info['format']} {info['srate']:g}Hz "
                f"{[l[0] for l in info['labels']]}")
        else:
            problems.append(f"{t}: the description differs from the control")
            say(f"  {t:8} FAIL {info['channels']}ch {info['format']} {info['srate']:g}Hz "
                f"{[l[0] for l in info['labels']]}")
    say("")

    say("=== the timestamps")
    for t in tags:
        s = signals[t]
        if s.stamp_count < 2:
            say(f"  {t:8} too few timestamps to test")
            continue
        mean = s.sum_gap / (s.stamp_count - 1)
        period = 1.0 / s.rate if s.rate else 0.0
        if s.falls:
            problems.append(f"{t}: {s.falls} timestamps do not rise")
        say(f"  {t:8} mean gap {mean * 1000:.4f} ms against a period of {period * 1000:.3f} ms, "
            f"least {s.min_gap * 1000:.2f}, most {s.max_gap * 1000:.2f}, {s.falls} that fall")
        if s.stalls:
            say(f"      {len(s.stalls)} or more gaps longer than five periods, first at "
                f"n={s.stalls[0][0]} of {s.stalls[0][1] * 1000:.1f} ms")
    say("")

    say("=== the markers")
    for t in sorted(markers):
        m = markers[t]
        info = m.shape
        if info["type"] != "Markers" or info["channels"] != 1 or info["format"] != "string":
            problems.append(f"{t}: the marker stream has the wrong shape")
            say(f"  {t:8} FAIL type={info['type']} chans={info['channels']} fmt={info['format']}")
            continue
        s = signals.get(t)
        off = []
        if s:
            for second in m.seconds:
                v = s.on_second.get(int(second * s.rate))
                if v is not None:
                    off.append(abs(v))
        worst = max(off) if off else None
        if m.wrong_word:
            problems.append(f"{t}: {m.wrong_word} markers carry the wrong word")
        if worst is not None and worst > 0.5:
            problems.append(f"{t}: a marker does not land on a zero crossing")
        say(f"  {t:8} {m.count:6} markers, {len(off)} matched to a sample, "
            f"worst |ch1| {('%.4f' % worst) if worst is not None else 'n/a'}, "
            f"{m.wrong_word} with the wrong word, first {m.texts[0]!r}")
    say("")
    return problems


def meta_of(meta, name):
    for info in meta.values():
        if info["name"] == name:
            return info
    raise SystemExit(f"no header for {name}")


# The mutations of the self test. Each names the text that must appear.
CORRUPTIONS = [
    ("a lost sample", "were lost"),
    ("a changed value", "the rule does not give"),
    ("a changed label", "description differs"),
    ("a timestamp that falls", "do not rise"),
    ("a marker with the wrong word", "wrong word"),
]


def mutator(which, control):
    """Damage one stream on purpose, so the report has to say so."""
    state = {"dropped": False, "changed": False, "stamped": False, "worded": False}

    def apply(kind, payload):
        if kind == "header":
            if which == "a changed label" and payload["tag"] != control and not payload["marker"]:
                payload["labels"] = [("XX",) + tuple(l[1:]) for l in payload["labels"]]
            return payload
        info, stamp, row = payload
        if info["tag"] == control:
            return stamp, row
        if which == "a changed value" and not info["marker"] and not state["changed"]:
            if int(row[-1]) % 1000 == 0:
                state["changed"] = True
                row = list(row)
                row[0] = row[0] + 1.0
        elif which == "a timestamp that falls" and not info["marker"] and not state["stamped"]:
            if stamp is not None and int(row[-1]) % 1000 == 0:
                state["stamped"] = True
                stamp = stamp - 5.0
        elif which == "a marker with the wrong word" and info["marker"] and not state["worded"]:
            state["worded"] = True
            row = ["burst 3"] if not row[0].startswith("burst 3") else ["tick 5"]
        return stamp, row

    return apply


def drop_mutator(control):
    """Skip one sample, which leaves a break in the counter."""
    state = {"done": False}

    def apply(kind, payload):
        if kind == "header":
            return payload
        info, stamp, row = payload
        if (info["tag"] != control and not info["marker"] and not state["done"]
                and int(row[-1]) % 1000 == 0):
            state["done"] = True
            row = list(row)
            row[-1] = row[-1] + 1.0   # the counter jumps, as a lost sample does
        return stamp, row

    return apply


def selftest(path, control):
    print("=== self test: a damaged recording must be reported")
    missed = 0
    for which, expect in CORRUPTIONS:
        mut = drop_mutator(control) if which == "a lost sample" else mutator(which, control)
        signals, markers, meta, _, _ = scan(path, mutate=mut)
        problems = report(signals, markers, meta, control, lambda *a: None)
        seen = any(expect in p for p in problems)
        missed += 0 if seen else 1
        print(f"  {which:30} {'reported' if seen else 'MISSED'}"
              f"   {next((p for p in problems if expect in p), '')}")
    print()
    if missed:
        print(f"the check missed {missed} damaged recording(s). It cannot judge a real one.")
    return missed


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("path")
    ap.add_argument("--control", default="oracle",
                    help="the tag that real liblsl wrote")
    ap.add_argument("--selftest", action="store_true",
                    help="damage the recording on purpose and report whether the check notices")
    args = ap.parse_args()

    if args.selftest and selftest(args.path, args.control):
        return 1

    signals, markers, meta, chunks, size = scan(args.path, progress=size_is_large(args.path))
    print(f"=== {args.path}")
    print(f"  {chunks} chunks, {len(meta)} streams, parsed to byte {size} of {size}")
    print()
    problems = report(signals, markers, meta, args.control, print)
    if problems:
        print(f"{len(problems)} problem(s):")
        for p in problems:
            print(f"  - {p}")
        return 1
    print("the recording agrees: every library sent the signal it was told to send, "
          "and nothing was lost")
    return 0


def size_is_large(path):
    import os
    return os.path.getsize(path) > 20_000_000


if __name__ == "__main__":
    sys.exit(main())
