// Browser / Node.js ES module for labstream WebSocket + WebAssembly decoder
// (`docs/roadmap.md`, step 13). Zero external dependencies.

const FORMAT_TYPED_ARRAY = {
  1: Float32Array,
  2: Float64Array,
  4: Int32Array,
  5: Int16Array,
  6: Int8Array,
  7: BigInt64Array,
};

export async function loadLabstreamWasm(wasmUrlOrBytes) {
  let bytes;
  if (wasmUrlOrBytes instanceof Uint8Array || wasmUrlOrBytes instanceof ArrayBuffer) {
    bytes = wasmUrlOrBytes;
  } else {
    const resp = await fetch(wasmUrlOrBytes);
    bytes = await resp.arrayBuffer();
  }
  const { instance } = await WebAssembly.instantiate(bytes, {});
  return instance.exports;
}

export class WasmStreamDecoder {
  constructor(wasm, format, channels) {
    this.wasm = wasm;
    this.format = format;
    this.channels = channels;
    this.handle = wasm.labstream_wasm_decoder_new(format, channels);
    if (!this.handle) {
      throw new Error(`invalid format=${format} or channels=${channels}`);
    }
  }

  decodeFrame(arrayBuffer) {
    const u8 = new Uint8Array(arrayBuffer);
    const ptr = this.wasm.labstream_wasm_alloc(u8.length);
    try {
      new Uint8Array(this.wasm.memory.buffer, ptr, u8.length).set(u8);
      const count = this.wasm.labstream_wasm_decode_frame(this.handle, ptr, u8.length);
      if (count < 0) {
        throw new Error(`labstream_wasm_decode_frame failed (${count})`);
      }
      const streamId = this.wasm.labstream_wasm_stream_id(this.handle);
      const tsPtr = this.wasm.labstream_wasm_timestamps_ptr(this.handle);
      const valPtr = this.wasm.labstream_wasm_values_ptr(this.handle);
      const valLen = this.wasm.labstream_wasm_values_len(this.handle);

      const timestamps = new Float64Array(
        this.wasm.memory.buffer.slice(tsPtr, tsPtr + count * 8),
      );
      const Typed = FORMAT_TYPED_ARRAY[this.format];
      const values = Typed
        ? new Typed(this.wasm.memory.buffer.slice(valPtr, valPtr + valLen))
        : new Uint8Array(this.wasm.memory.buffer.slice(valPtr, valPtr + valLen));
      return { streamId, count, timestamps, values };
    } finally {
      this.wasm.labstream_wasm_dealloc(ptr, u8.length);
    }
  }

  free() {
    if (this.handle) {
      this.wasm.labstream_wasm_decoder_free(this.handle);
      this.handle = 0;
    }
  }
}

export class LabstreamBrowserClient {
  constructor(wsUrl, wasm) {
    this.wsUrl = wsUrl;
    this.wasm = wasm;
    this.ws = null;
    this.decoders = new Map();
    this.onSamples = null;
    this.onMetadata = null;
    this.onMessage = null;
  }

  connect() {
    return new Promise((resolve, reject) => {
      const ws = new WebSocket(this.wsUrl);
      ws.binaryType = "arraybuffer";
      ws.addEventListener("open", () => {
        this.ws = ws;
        resolve();
      });
      ws.addEventListener("error", (err) => reject(err));
      ws.addEventListener("message", (ev) => {
        if (typeof ev.data === "string") {
          const msg = JSON.parse(ev.data);
          if (msg.op === "subscribed") {
            const dec = new WasmStreamDecoder(this.wasm, msg.format, msg.channel_count);
            this.decoders.set(msg.id, dec);
          } else if (msg.op === "meta" && this.onMetadata) {
            this.onMetadata(msg);
          }
          if (this.onMessage) this.onMessage(msg);
        } else {
          const buf = ev.data;
          const view = new DataView(buf);
          const id = view.getUint32(0, true);
          const dec = this.decoders.get(id);
          if (dec && this.onSamples) {
            this.onSamples(dec.decodeFrame(buf));
          }
        }
      });
    });
  }

  send(obj) {
    this.ws.send(JSON.stringify(obj));
  }

  listStreams() {
    this.send({ op: "list" });
  }

  subscribe(stream, postprocess = 0) {
    this.send({ op: "subscribe", stream, postprocess });
  }

  sendMarker(value, stream = "", timestamp = 0.0) {
    this.send({ op: "marker", stream, value, timestamp });
  }

  close() {
    for (const dec of this.decoders.values()) dec.free();
    this.decoders.clear();
    if (this.ws) this.ws.close();
  }
}
