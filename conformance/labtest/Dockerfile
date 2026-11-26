# Lean runtime-only Virtual Lab image (< 220 MB).
# Uses Ubuntu 24.04 (matching GitHub Actions ubuntu-latest glibc 2.39) and
# copies Node.js 22 (with native global WebSocket) from node:22-slim.
FROM node:22-slim AS node22

FROM ubuntu:24.04

RUN apt-get update && apt-get install -y --no-install-recommends \
        python3 \
        iproute2 \
        iptables \
        iputils-ping \
        ca-certificates \
        libstdc++6 \
        procps \
    && rm -rf /var/lib/apt/lists/*

COPY --from=node22 /usr/local/bin/node /usr/local/bin/node

WORKDIR /labtest

# Copy pre-built binaries and web/WASM decoder from workspace root.
COPY target/release/liblsl.so /opt/labstream/liblsl.so
COPY target/release/labstream /usr/local/bin/labstream
COPY target/wasm32-unknown-unknown/release/labstream_wasm.wasm /opt/labstream/labstream_wasm.wasm
COPY web/labstream.js /opt/labstream/labstream.mjs
COPY conformance/.build/install/lib/liblsl.so* /opt/oracle/
COPY conformance/labtest/nodes.py /labtest/nodes.py
COPY conformance/labtest/browser_node.mjs /labtest/browser_node.mjs

RUN ldconfig /opt/oracle || true
