# Lean runtime-only Virtual Lab image (< 200 MB).
# Copies pre-built labstream + pinned C++ liblsl binaries from the workspace.
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
        python3 \
        nodejs \
        iproute2 \
        iptables \
        iputils-ping \
        ca-certificates \
        libstdc++6 \
        procps \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /labtest

# Copy pre-built binaries and web/WASM decoder from workspace root.
COPY target/release/liblsl.so /opt/labstream/liblsl.so
COPY target/release/labstream /usr/local/bin/labstream
COPY target/wasm32-unknown-unknown/release/labstream_wasm.wasm /opt/labstream/labstream_wasm.wasm
COPY web/labstream.js /opt/labstream/labstream.js
COPY conformance/.build/install/lib/liblsl.so* /opt/oracle/
COPY conformance/labtest/nodes.py /labtest/nodes.py
COPY conformance/labtest/browser_node.mjs /labtest/browser_node.mjs

RUN ldconfig /opt/oracle || true
