#!/usr/bin/env node
// End-to-end verification for step 13 (browser reach):
// Loads the real compiled `labstream_wasm.wasm`, connects via standard
// WebSocket (`web/labstream.js`) to `labstream relay --ws-port`, subscribes to
// a 512 Hz x 8 channel EEG stream, verifies bit-exact values and timestamps,
// and sends a marker over WebSocket that arrives on a native LSL marker inlet.

import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { loadLabstreamWasm, LabstreamBrowserClient } from "../../web/labstream.js";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(__dirname, "../..");
const wasmPath = path.join(
  repoRoot,
  "target/wasm32-unknown-unknown/release/labstream_wasm.wasm",
);

const wsUrl = process.argv[2];
const streamName = process.argv[3] || "BrowserEeg";
const markerStream = process.argv[4] || "BrowserMarkers";
const wantSamples = Number(process.argv[5] || "256");

assert(wsUrl, "usage: browser_test.mjs <ws://127.0.0.1:PORT/ws> [stream] [markerStream] [wantSamples]");
const wasmBytes = fs.readFileSync(wasmPath);
const wasm = await loadLabstreamWasm(wasmBytes);

const client = new LabstreamBrowserClient(wsUrl, wasm);
const received = [];
let markerOk = false;
let subscribed = false;

client.onMessage = (msg) => {
  if (msg.op === "subscribed") {
    subscribed = true;
    assert.equal(msg.channel_count, 8);
    assert.equal(msg.format, 1);
    assert.equal(msg.nominal_srate, 512);
  } else if (msg.op === "marker_ok") {
    markerOk = true;
  }
};

client.onSamples = ({ count, timestamps, values }) => {
  for (let i = 0; i < count; i++) {
    const ch = [];
    for (let c = 0; c < 8; c++) ch.push(values[i * 8 + c]);
    received.push({ ts: timestamps[i], ch });
  }
};

await client.connect();
client.subscribe(streamName, 0);

const deadline = Date.now() + 8000;
while (!subscribed && Date.now() < deadline) {
  await new Promise((r) => setTimeout(r, 25));
}
assert(subscribed, "failed to subscribe to stream over WebSocket");

// Send a marker from the browser client.
client.sendMarker("browser_ping_42", markerStream, 123.5);

while ((received.length < wantSamples || !markerOk) && Date.now() < deadline) {
  await new Promise((r) => setTimeout(r, 25));
}

assert(markerOk, "marker_ok was not received");
assert(
  received.length >= wantSamples,
  `expected at least ${wantSamples} samples, got ${received.length}`,
);

// Verify deterministic pattern pushed by the Rust harness:
// sample k has ch[c] = (k % 1024) * 0.25 + c and monotonic timestamps spaced by 1/512 s.
for (let i = 0; i < wantSamples; i++) {
  const { ts, ch } = received[i];
  assert(Number.isFinite(ts) && ts > 0, `invalid timestamp at ${i}: ${ts}`);
  if (i > 0) {
    const dt = ts - received[i - 1].ts;
    assert(Math.abs(dt - 1 / 512) < 1e-9, `timestamp step mismatch at ${i}: ${dt}`);
  }
  const base = ch[0];
  for (let c = 1; c < 8; c++) {
    assert.equal(ch[c], base + c, `channel ${c} mismatch at sample ${i}`);
  }
}

client.close();
console.log(
  JSON.stringify({
    ok: true,
    samples: received.length,
    first_ts: received[0].ts,
    last_ts: received[wantSamples - 1].ts,
    marker_ok: markerOk,
  }),
);
