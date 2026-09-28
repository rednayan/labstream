#!/bin/sh
# Entry point for the oracle image.
#
# The upstream suite splits into two groups, and the split matches the tier
# model in CONFORMANCE-PLAN.md:
#   lsl_test_internal   no sockets. Runs the same way on every machine.
#   lsl_test_exported   opens real ports. Can fail for reasons outside the code.
#
# Usage: docker run --rm lsl-oracle <command>

set -eu

BUILD=/oracle/build
SRC=/oracle/src

usage() {
    echo "Commands:"
    echo "  info        Show the pinned revision and the built artifacts."
    echo "  test        Run every test group."
    echo "  test-gate   Run the hermetic subset. This is the merge gate."
    echo "  test-int    Run the whole internal group. Some tests open sockets."
    echo "  test-ext    Run the exported group. Opens real ports."
    echo "  shell       Start an interactive shell."
}

# The hermetic subset. The internal binary is not hermetic on its own: four of
# its files open sockets, and one test carries the Catch2 tag [!mayfail], which
# hides a real failure inside a container. The [network] tag excludes them.
#
# Treat any failure here as a real defect. Nothing else gates a merge.
cmd_test_gate() {
    echo "=== hermetic gate (internal group, [network] excluded) ==="
    "$BUILD/testing/lsl_test_internal" "~[network]" --reporter compact --durations no
}

cmd_info() {
    echo "=== pins ==="
    cat /oracle/PINS | grep -v '^#' | grep -v '^$'
    echo
    echo "=== revision ==="
    git -C "$SRC" describe --tags --always
    git -C "$SRC" rev-parse HEAD
    echo
    echo "=== toolchain ==="
    cmake --version | head -1
    c++ --version | head -1
    echo
    echo "=== built artifacts ==="
    find "$BUILD" -maxdepth 2 -type f -executable -not -path '*/_deps/*' \
        -exec sh -c 'printf "%-34s %s\n" "$(basename "$1")" "$(sha256sum "$1" | cut -c1-16)"' _ {} \;
}

# The internal group carries the hermetic tests. Treat a failure here as a real
# defect. This group gates everything downstream.
cmd_test_int() {
    echo "=== internal group (hermetic) ==="
    "$BUILD/testing/lsl_test_internal" --reporter compact --durations no
}

# The exported group opens ports. A failure here can come from the host network
# rather than from the code. Read the output before you act on it.
cmd_test_ext() {
    echo "=== exported group (uses real sockets) ==="
    "$BUILD/testing/lsl_test_exported" --reporter compact --durations no
}

cmd_test() {
    cmd_test_int
    echo
    cmd_test_ext
}

case "${1:-info}" in
    info)      cmd_info ;;
    test)      cmd_test ;;
    test-gate) cmd_test_gate ;;
    test-int)  cmd_test_int ;;
    test-ext)  cmd_test_ext ;;
    shell)     exec /bin/bash ;;
    *)         usage; exit 1 ;;
esac
