#!/usr/bin/env node
// Virtual Lab browser-node: connects over the Docker LAN to hd-eeg-node's WsBridge,
// decodes live binary LSL frames with the 27 KB labstream_wasm.wasm decoder,
// confirms live meta/1 updates, and sends a browser marker back to LSL.

import fs from "node:fs";
import { loadLabstreamWasm, LabstreamBrowserClient } from "/opt/labstream/labstream.mjs";

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function main() {
  await sleep(4500); // allow hd-eeg-node WsBridge to start and discover LabTest_HDEEG-ws
  const wasmBytes = fs.readFileSync("/opt/labstream/labstream_wasm.wasm");
  const wasm = await loadLabstreamWasm(wasmBytes);

  const client = new LabstreamBrowserClient("ws://172.29.0.11:8765/ws", wasm);
  let samples = 0;
  let metaFrames = 0;
  let markerAck = false;

  client.onSamples = ({ count }) => {
    samples += count;
  };
  client.onMetadata = () => {
    metaFrames++;
  };
  client.onMessage = (msg) => {
    if (msg.op === "marker_ok") markerAck = true;
  };

  await client.connect();
  client.subscribe("LabTest_HDEEG", 0);

  const deadline = Date.now() + 15000;
  while (samples < 100 && Date.now() < deadline) {
    await sleep(100);
  }
  client.sendMarker("LABTEST_BROWSER_OK", "LabTest_BrowserMarkers", 0.0);
  const mkDeadline = Date.now() + 3000;
  while (!markerAck && Date.now() < mkDeadline) {
    await sleep(50);
  }
  client.close();

  const result = {
    ok: samples >= 100 && markerAck,
    samples,
    meta_frames: metaFrames,
    marker_ack: markerAck,
  };
  fs.writeFileSync("/shared/browser_result.json", JSON.stringify(result, null, 2));
  console.log("[browser-node] result:", JSON.stringify(result));
  // Keep an active event-loop handle until recorder-node finishes and compose tears down.
  setInterval(() => {}, 60_000);
}

main().catch((err) => {
  const result = { ok: false, error: String(err) };
  fs.writeFileSync("/shared/browser_result.json", JSON.stringify(result, null, 2));
  console.error("[browser-node] error:", err);
  setInterval(() => {}, 60_000);
});
