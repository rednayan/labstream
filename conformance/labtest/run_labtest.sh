#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LABDIR="$ROOT/conformance/labtest"
DURATION=45
CHAOS=1

while [[ $# -gt 0 ]]; do
  case "$1" in
    --profile)
      shift
      case "${1:-quick}" in
        quick) DURATION=35 ;;
        full)  DURATION=120 ;;
        soak)  DURATION=14400 ;;
        *) echo "unknown profile: $1" >&2; exit 2 ;;
      esac
      ;;
    --seconds) shift; DURATION="${1:-45}" ;;
    --minutes) shift; DURATION=$(( ${1:-5} * 60 )) ;;
    --no-chaos) CHAOS=0 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
  shift
done

for req in \
  "$ROOT/target/release/liblsl.so" \
  "$ROOT/target/release/labstream" \
  "$ROOT/target/wasm32-unknown-unknown/release/labstream_wasm.wasm" \
  "$ROOT/conformance/.build/install/lib/liblsl.so.2"; do
  if [[ ! -f "$req" ]]; then
    echo "[labtest] missing required build artifact: $req" >&2
    echo "Build with:" >&2
    echo "  cargo build --release -p labstream-capi -p labstream-cli" >&2
    echo "  cargo build --release --target wasm32-unknown-unknown -p labstream-wasm" >&2
    exit 1
  fi
done

echo "[labtest] building lean runtime Docker image..."
docker build -f "$LABDIR/Dockerfile" -t labstream-labtest:latest "$ROOT"

echo "[labtest] running 4-node Virtual Lab (duration=${DURATION}s, chaos=${CHAOS})..."
set +e
LABTEST_DURATION="$DURATION" LABTEST_CHAOS="$CHAOS" \
  docker compose -f "$LABDIR/docker-compose.yml" up \
  --abort-on-container-exit --exit-code-from recorder-node
RC=$?
set -e

docker compose -f "$LABDIR/docker-compose.yml" down -v >/dev/null 2>&1 || true
exit "$RC"
