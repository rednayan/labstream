#!/bin/sh
# Compare the description tree calls of the two libraries.
#
# `oracle/desctree.c` uses nothing but the C ABI. This builds it twice, once
# against real liblsl and once against the Rust library, runs both, and
# compares. The program prints no pointer value and no clock reading, so two
# correct libraries produce the same bytes.
#
# Usage: oracle/abitree.sh [oracle-lib-dir] [rust-lib-dir]

set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)
ORACLE_DIR=${1:-$ROOT/.build/install/lib}
RUST_DIR=${2:-$ROOT/.build/rustlib}
BUILD=$ROOT/.build
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

if [ ! -f "$ORACLE_DIR/liblsl.so" ]; then
    echo "no oracle library at $ORACLE_DIR/liblsl.so"
    exit 1
fi
if [ ! -f "$RUST_DIR/liblsl.so" ]; then
    echo "no Rust library at $RUST_DIR/liblsl.so"
    echo "run: cargo build -p lsl-capi --release && cp target/release/liblsl.so $RUST_DIR/"
    exit 1
fi

build_and_run() {
    dir=$1
    out=$2
    cc -I "$BUILD/install/include" "$ROOT/oracle/desctree.c" -o "$WORK/desctree" \
        -L "$dir" -llsl -Wl,-rpath,"$dir"
    "$WORK/desctree" > "$out" 2>/dev/null
}

build_and_run "$ORACLE_DIR" "$WORK/oracle.txt"
build_and_run "$RUST_DIR" "$WORK/rust.txt"

lines=$(wc -l < "$WORK/oracle.txt")
if diff -u "$WORK/oracle.txt" "$WORK/rust.txt" > "$WORK/diff.txt"; then
    echo "the description tree matches: $lines lines, no difference"
    exit 0
fi

echo "the description tree differs:"
cat "$WORK/diff.txt"
exit 1
