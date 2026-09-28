#!/bin/sh
# Publish a stream for a recorder on another machine.
#
# This is a live test across a real network. LabRecorder runs on the second
# machine, and this runs here. Three publishers are available, and the point of
# having three is attribution: a failure of one and a pass of another names the
# cause.
#
#   rust    lsl-net directly. No C ABI on the path.
#   capi    the example program SendData of liblsl, linked against this library.
#   oracle  the same program, linked against real liblsl. THE CONTROL.
#
# Read the control first. If the other machine cannot see the oracle stream,
# the network blocks the test and nothing here is a defect of this library.
#
# Usage:
#   oracle/labrecorder.sh oracle          # start here
#   oracle/labrecorder.sh capi
#   oracle/labrecorder.sh rust
#   oracle/labrecorder.sh rust --name MyTest --channels 4 --rate 250

set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)
MODE=${1:-help}
[ $# -gt 0 ] && shift || true

ORACLE_LIB=$ROOT/.build/install/lib
RUST_LIB=$ROOT/.build/rustlib

# `--ipv6` writes a configuration that leaves IPv6 alone and points the
# library at it. The stream then has no IPv4 port at all, so a consumer that
# finds it has found it over IPv6. **Nothing has to change on the other
# machine.**
for arg in "$@"; do
    if [ "$arg" = "--ipv6" ]; then
        CFG=$(mktemp /tmp/lsl_api_v6_XXXXXX.cfg)
        printf '; only IPv6, so a consumer has no other way in\n[ports]\nIPv6 = force\n' > "$CFG"
        LSLAPICFG=$CFG
        export LSLAPICFG
        echo "=== IPv6 only: $CFG"
        echo
    fi
done
# The flag is for this script, so it is taken out before the publisher runs.
set -- $(for a in "$@"; do [ "$a" = "--ipv6" ] || printf '%s ' "$a"; done)

# Each mode tags its streams with its own name, so the three can run at the
# same time and a recorder shows which library sent which stream. A `--tag`
# among the arguments wins.
has_tag=0
for arg in "$@"; do
    [ "$arg" = "--tag" ] && has_tag=1
done
if [ "$has_tag" = 0 ] && [ "$MODE" != help ] && [ "$MODE" != -h ] && [ "$MODE" != --help ]; then
    set -- "$@" --tag "$MODE"
fi

usage() {
    cat <<'EOF'
Publish a stream for LabRecorder on another machine.

  oracle/labrecorder.sh oracle [args]   real liblsl. Run this first.
  oracle/labrecorder.sh capi   [args]   this library, through the C ABI.
  oracle/labrecorder.sh rust   [args]   this library, through lsl-net.

Every mode takes the same options and sends the same signal:

  --name N --type T --channels C --rate R --seconds S --markers 0|1 --tag W

Add `--ipv6` to publish over IPv6 alone. The stream then reports no IPv4 port,
so a consumer that finds it has found it over IPv6. Nothing has to change on
the other machine.

Each mode tags its streams with its own name, so the three can run at the same
time and a recorder shows which library sent which stream:

  LabTest-oracle   LabTest-Markers-oracle
  LabTest-capi     LabTest-Markers-capi
  LabTest-rust     LabTest-Markers-rust

Pass `--tag` yourself to override it, or `--tag ""` to drop it.

Channel k carries a sine wave of k + 1 Hz, and the last channel carries the
sample number. The values come from the sample number and not from a clock or
a random source, so a plot of one mode must look exactly like a plot of
another. `oracle/samesignal.sh` measures that.

What to look for, in this order:

  1. Does the stream appear in the list of LabRecorder?
     Yes means that discovery works: the query arrived, and the answer got
     back. No means UDP is blocked, or the two machines are on different
     subnets, or a firewall drops the answer.
  2. Does the record start and the sample count rise?
     Yes means that the TCP feed works as well. No, with the stream listed,
     means that discovery works and the data port does not.
  3. Does the recording hold the channel labels Fp1, Fp2, C3 ...?
     That is the description tree, which travels on the data port.

Every publisher writes the count of connected consumers. The count rises the
moment LabRecorder links the stream, so watch this window as well.
EOF
}

show_network() {
    echo "=== this machine"
    ip -4 -o addr show 2>/dev/null | awk '{printf "  %-10s %s\n", $2, $4}' || true
    echo
    echo "  Both machines must be able to reach each other. Test it with:"
    echo "    ping <the other machine>"
    echo "  A wireless access point often blocks multicast between clients."
    echo "  A broadcast query still arrives, so the test can still work."
    echo
}

case "$MODE" in
help | -h | --help)
    usage
    exit 0
    ;;

rust)
    show_network
    cargo build --release -p lsl-net --example publish --manifest-path "$ROOT/Cargo.toml" \
        >/dev/null 2>&1
    echo "=== publisher: lsl-net, no C ABI"
    exec "$ROOT/target/release/examples/publish" "$@"
    ;;

capi | oracle)
    if [ "$MODE" = capi ]; then
        LIBDIR=$RUST_LIB
        WHAT="this library, through the C ABI"
        if [ ! -f "$LIBDIR/liblsl.so" ]; then
            echo "no library at $LIBDIR/liblsl.so"
            echo "run: cargo build -p lsl-capi --release && cp target/release/liblsl.so $LIBDIR/"
            exit 1
        fi
    else
        LIBDIR=$ORACLE_LIB
        WHAT="real liblsl (the control)"
    fi

    BIN=$ROOT/.build/publish_$MODE
    cc -I "$ROOT/.build/install/include" "$ROOT/oracle/publish.c" -o "$BIN" -lm \
        -L "$LIBDIR" -llsl -Wl,-rpath,"$LIBDIR"

    show_network
    echo "=== publisher: $WHAT"
    echo "  program  oracle/publish.c, the same signal as the Rust publisher"
    echo "  library  $LIBDIR/liblsl.so"
    echo
    exec "$BIN" "$@"
    ;;

*)
    echo "unknown mode: $MODE"
    echo
    usage
    exit 1
    ;;
esac
