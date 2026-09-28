#!/bin/sh
# Make sure that the three publishers send the same numbers.
#
# `oracle/labrecorder.sh` offers three publishers so that a failure on a second
# machine names its own cause. That only works when all three send the same
# signal. If they differ, two plots that look different prove nothing.
#
# This records from each publisher in turn and compares the values channel by
# channel. The last channel carries the sample number, so rows are matched by
# sample and not by arrival order.
#
# The timestamps are left out on purpose. Each run starts at its own moment, so
# the timestamps must differ. `oracle/interop.py --zero-stamps` tests the
# timestamp rule instead.
#
# Usage: oracle/samesignal.sh [count]

set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)
COUNT=${1:-400}
ORACLE_LIB=$ROOT/.build/install/lib
RUST_LIB=$ROOT/.build/rustlib
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

cc -I "$ROOT/.build/install/include" "$ROOT/oracle/publish.c" -o "$WORK/pub_oracle" -lm \
    -L "$ORACLE_LIB" -llsl -Wl,-rpath,"$ORACLE_LIB"
cc -I "$ROOT/.build/install/include" "$ROOT/oracle/publish.c" -o "$WORK/pub_capi" -lm \
    -L "$RUST_LIB" -llsl -Wl,-rpath,"$RUST_LIB"
cargo build --release -p lsl-net --example publish --manifest-path "$ROOT/Cargo.toml" >/dev/null 2>&1

cat > "$WORK/capture.py" <<'PY'
import sys, time, struct, pylsl
name, out_path, want = sys.argv[1], sys.argv[2], int(sys.argv[3])
hits = pylsl.resolve_byprop("name", name, 1, 10)
if not hits:
    print("resolve failed"); sys.exit(1)
inlet = pylsl.StreamInlet(hits[0])
rows = {}
t0 = time.time()
while len(rows) < want and time.time() - t0 < 30:
    s, ts = inlet.pull_sample(1.0)
    if not s:
        continue
    rows[int(s[-1])] = s
with open(out_path, "w") as f:
    for n in sorted(rows):
        bits = " ".join(f"{struct.unpack('<I', struct.pack('<f', v))[0]:08x}" for v in rows[n])
        f.write(f"{n} {bits}\n")
print(f"{len(rows)}")
PY

capture() {
    tag=$1
    bin=$2
    # Each publisher tags its stream, so the three never collide.
    "$bin" --name SameSignal --tag "$tag" --channels 8 --rate 100 --seconds 26 \
        >/dev/null 2>&1 &
    pid=$!
    sleep 2
    n=$(PYLSL_LIB=$ORACLE_LIB/liblsl.so timeout 30 python3 "$WORK/capture.py" \
        "SameSignal-$tag" "$WORK/$tag.txt" "$COUNT" 2>/dev/null || echo 0)
    printf "  %-8s %s samples\n" "$tag" "$n"
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    sleep 1
}

echo "=== recording from each publisher"
capture oracle "$WORK/pub_oracle"
capture capi "$WORK/pub_capi"
capture rust "$ROOT/target/release/examples/publish"

echo
echo "=== comparing the values"
WORK=$WORK python3 - <<'PY'
import os, sys
w = os.environ["WORK"]
def load(tag):
    d = {}
    for line in open(f"{w}/{tag}.txt"):
        p = line.split()
        d[int(p[0])] = p[1:]
    return d
try:
    o, c, r = load("oracle"), load("capi"), load("rust")
except OSError as e:
    print(f"  a capture is missing: {e}")
    sys.exit(1)
common = sorted(set(o) & set(c) & set(r))
if not common:
    print("  no sample number is present in all three recordings")
    sys.exit(1)
bad = 0
for n in common:
    for tag, other in (("capi", c), ("rust", r)):
        if o[n] != other[n]:
            if bad == 0:
                print(f"  the first difference is in {tag}, at sample {n}")
                print(f"    oracle {' '.join(o[n])}")
                print(f"    {tag:6} {' '.join(other[n])}")
            bad += 1
print(f"  {len(common)} samples present in all three, n={common[0]} to {common[-1]}")
if bad:
    print(f"  {bad} rows differ. The three publishers do not send the same signal.")
    sys.exit(1)
print("  every channel value is the same in all three, bit for bit")
PY
