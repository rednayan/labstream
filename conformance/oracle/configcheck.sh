#!/bin/sh
# Compare how the two libraries read a configuration file.
#
# `oracle/configprobe.c` publishes one stream and prints what the file did to
# it. This builds the probe twice, runs it once for each configuration below,
# and compares the two outputs.
#
# Usage: oracle/configcheck.sh [oracle-lib-dir] [rust-lib-dir]

set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)
ORACLE_DIR=${1:-$ROOT/.build/install/lib}
RUST_DIR=${2:-$ROOT/.build/rustlib}
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

cc -I "$ROOT/.build/install/include" "$ROOT/oracle/configprobe.c" -o "$WORK/probe_oracle" \
    -L "$ORACLE_DIR" -llsl -Wl,-rpath,"$ORACLE_DIR"
cc -I "$ROOT/.build/install/include" "$ROOT/oracle/configprobe.c" -o "$WORK/probe_rust" \
    -L "$RUST_DIR" -llsl -Wl,-rpath,"$RUST_DIR"

# --- the configurations to compare ---

cat > "$WORK/default.cfg" <<'EOF'
; nothing is set, so every default applies
EOF

cat > "$WORK/ports.cfg" <<'EOF'
[ports]
BasePort = 17300
PortRange = 8
EOF

cat > "$WORK/session.cfg" <<'EOF'
[lab]
SessionID = testlab
EOF

cat > "$WORK/stamps.cfg" <<'EOF'
[tuning]
ForceDefaultTimestamps = 1
EOF

# `from_string<bool>` only accepts the text `1`, so this reads as no.
cat > "$WORK/stamps_true.cfg" <<'EOF'
[tuning]
ForceDefaultTimestamps = true
EOF

# A `#` is not a comment. The line then has no `=`, the reader throws, and
# every default applies.
cat > "$WORK/hash.cfg" <<'EOF'
# this is not a comment
[ports]
BasePort = 17400
EOF

# A repeated key throws the file away as well.
cat > "$WORK/duplicate.cfg" <<'EOF'
[ports]
BasePort = 17500
BasePort = 17600
EOF

fails=0
run_case() {
    label=$1
    cfg=$2
    LSLAPICFG="$WORK/$cfg" "$WORK/probe_oracle" "Cfg$label" > "$WORK/o.txt" 2>/dev/null || true
    sleep 1
    LSLAPICFG="$WORK/$cfg" "$WORK/probe_rust" "Cfg$label" > "$WORK/r.txt" 2>/dev/null || true
    if diff -q "$WORK/o.txt" "$WORK/r.txt" > /dev/null 2>&1; then
        printf "  %-14s pass   %s\n" "$label" "$(tr '\n' '|' < "$WORK/o.txt" | sed 's/  */ /g')"
    else
        fails=$((fails + 1))
        printf "  %-14s FAIL\n" "$label"
        diff "$WORK/o.txt" "$WORK/r.txt" | sed 's/^/      /'
    fi
    sleep 1
}

echo "=== the same configuration must reach both libraries the same way ==="
run_case default default.cfg
run_case ports ports.cfg
run_case session session.cfg
run_case stamps stamps.cfg
run_case stampstrue stamps_true.cfg
run_case hash hash.cfg
run_case duplicate duplicate.cfg

echo
if [ "$fails" -gt 0 ]; then
    echo "$fails configuration(s) read differently"
    exit 1
fi
echo "every configuration reads the same way"
