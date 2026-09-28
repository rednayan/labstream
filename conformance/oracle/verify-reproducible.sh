#!/usr/bin/env bash
# Prove the M0 exit criterion: two independent builds produce the same binaries.
#
# The image pins the base by digest, the compiler and Catch2 by that digest, and
# pugixml by commit. If those pins hold, two builds must agree. If the hashes
# differ, the build embeds something that varies, and every golden vector taken
# from this image is then suspect.
#
# This runs two full builds with no cache. It takes a while.
#
# Usage: oracle/verify-reproducible.sh

set -euo pipefail

cd "$(dirname "$0")/.."

TAG_A=lsl-oracle:repro-a
TAG_B=lsl-oracle:repro-b
OUT=$(mktemp -d)
trap 'rm -rf "$OUT"' EXIT

echo "=== build A ==="
docker build --no-cache -f oracle/Dockerfile -t "$TAG_A" . >"$OUT/build-a.log" 2>&1 \
    || { echo "build A failed"; tail -30 "$OUT/build-a.log"; exit 1; }

echo "=== build B ==="
docker build --no-cache -f oracle/Dockerfile -t "$TAG_B" . >"$OUT/build-b.log" 2>&1 \
    || { echo "build B failed"; tail -30 "$OUT/build-b.log"; exit 1; }

# Hash every built artifact inside each image.
hash_artifacts() {
    docker run --rm --entrypoint /bin/sh "$1" -c '
        find /oracle/build -maxdepth 2 -type f \
             \( -executable -o -name "*.so*" \) -not -path "*/_deps/*" \
        | sort \
        | while read -r f; do
            printf "%s  %s\n" "$(sha256sum "$f" | cut -d" " -f1)" "${f#/oracle/build/}"
          done'
}

echo "=== hashing ==="
hash_artifacts "$TAG_A" >"$OUT/a.txt"
hash_artifacts "$TAG_B" >"$OUT/b.txt"

echo
echo "=== build A artifacts ==="
cat "$OUT/a.txt"
echo

if diff -u "$OUT/a.txt" "$OUT/b.txt" >"$OUT/diff.txt"; then
    echo "RESULT: reproducible. Both builds produced identical artifacts."
    exit 0
fi

echo "RESULT: NOT reproducible. The builds differ."
echo
cat "$OUT/diff.txt"
echo
echo "Next step: find what the build embeds. Common causes are an embedded"
echo "build path, an embedded timestamp, and a parallel link order."
exit 1
