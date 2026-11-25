#!/usr/bin/env bash
set -euo pipefail

DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$DIR/../.." && pwd)"
ORACLE_LIB="$ROOT/conformance/.build/install/lib"
ORACLE_INC="$ROOT/conformance/.build/install/include"
ORACLE_BIN="$DIR/oracle_inlet"

if [ ! -f "$ORACLE_LIB/liblsl.so" ]; then
  echo "error: pinned liblsl not found at $ORACLE_LIB/liblsl.so" >&2
  exit 1
fi

echo "[demo1] building genuine C++ liblsl sidecar (oracle_inlet)..."
cc -O2 -Wall "$DIR/oracle_inlet.c" \
  -I"$ORACLE_INC" \
  -L"$ORACLE_LIB" \
  -Wl,-rpath,"$ORACLE_LIB" \
  -llsl -lm -lpthread \
  -o "$ORACLE_BIN"

echo "[demo1] starting demo1-viewer (open http://127.0.0.1:8780)..."
exec cargo run --release --manifest-path "$DIR/Cargo.toml" --bin demo1-viewer -- \
  --oracle-bin "$ORACLE_BIN" "$@"
