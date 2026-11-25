//! WebAssembly decoder for LSL wire sample frames sent by the `labstream`
//! WebSocket bridge (`docs/roadmap.md`, step 13).
//!
//! This crate depends only on `labstream-wire` and builds cleanly for both
//! `wasm32-unknown-unknown` (`cdylib`) and native test targets (`rlib`).
//!
//! Binary WebSocket frame layout:
//! ```text
//! u32  stream_id     little-endian subscription identifier
//! u32  sample_count  little-endian count of LSL samples in this frame
//! ...                `sample_count` LSL wire samples (little-endian, full timestamps)
//! ```

#![deny(unsafe_code)]
#![deny(missing_docs)]

use labstream_wire::{ByteOrder, Codec, Format, Value};

/// Stateful decoder that reuses output buffers across incoming WebSocket frames.
pub struct WasmDecoder {
    codec: Codec,
    stream_id: u32,
    timestamps: Vec<f64>,
    values_bytes: Vec<u8>,
    string_bytes: Vec<u8>,
    string_offsets: Vec<u32>,
}

impl WasmDecoder {
    /// Create a decoder for a stream with `format` and `channels` channels.
    pub fn new(format: Format, channels: usize) -> Option<Self> {
        if format == Format::Undefined || channels == 0 {
            return None;
        }
        Some(WasmDecoder {
            codec: Codec::new(format, channels, ByteOrder::Little),
            stream_id: 0,
            timestamps: Vec::new(),
            values_bytes: Vec::new(),
            string_bytes: Vec::new(),
            string_offsets: Vec::new(),
        })
    }

    /// Decode one binary WebSocket frame into flat timestamp and value buffers.
    ///
    /// Returns the number of decoded samples, or a negative error code:
    /// - `-1`: frame shorter than the 8-byte header
    /// - `-2`: truncated sample payload
    /// - `-3`: malformed LSL wire sample
    pub fn decode_frame(&mut self, frame: &[u8]) -> i32 {
        if frame.len() < 8 {
            return -1;
        }
        self.stream_id = u32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]);
        let expected = u32::from_le_bytes([frame[4], frame[5], frame[6], frame[7]]) as usize;

        self.timestamps.clear();
        self.values_bytes.clear();
        self.string_bytes.clear();
        self.string_offsets.clear();

        let mut pos = 8usize;
        let mut decoded = 0usize;
        while decoded < expected && pos < frame.len() {
            match self.codec.decode(&frame[pos..]) {
                Ok((sample, used)) => {
                    pos += used;
                    self.timestamps.push(sample.timestamp);
                    for v in sample.values {
                        match v {
                            Value::F32(x) => self.values_bytes.extend_from_slice(&x.to_le_bytes()),
                            Value::F64(x) => self.values_bytes.extend_from_slice(&x.to_le_bytes()),
                            Value::I32(x) => self.values_bytes.extend_from_slice(&x.to_le_bytes()),
                            Value::I16(x) => self.values_bytes.extend_from_slice(&x.to_le_bytes()),
                            Value::I8(x) => self.values_bytes.push(x as u8),
                            Value::I64(x) => self.values_bytes.extend_from_slice(&x.to_le_bytes()),
                            Value::Str(bytes) => {
                                self.string_offsets.push(self.string_bytes.len() as u32);
                                self.string_bytes.extend_from_slice(&bytes);
                            }
                        }
                    }
                    decoded += 1;
                }
                Err(labstream_wire::Error::Truncated) => return -2,
                Err(_) => return -3,
            }
        }
        if decoded != expected {
            return -2;
        }
        if self.codec.format == Format::String {
            self.string_offsets.push(self.string_bytes.len() as u32);
        }
        decoded as i32
    }

    /// Subscription identifier from the last decoded frame.
    pub fn stream_id(&self) -> u32 {
        self.stream_id
    }

    /// Decoded sample timestamps (`f64`, little-endian).
    pub fn timestamps(&self) -> &[f64] {
        &self.timestamps
    }

    /// Flat little-endian numeric sample values (`sample_count * channels * width` bytes).
    pub fn values_bytes(&self) -> &[u8] {
        &self.values_bytes
    }

    /// Concatenated string bytes for `Format::String` frames.
    pub fn string_bytes(&self) -> &[u8] {
        &self.string_bytes
    }

    /// Byte offsets into [`WasmDecoder::string_bytes`] (length = `strings + 1`).
    pub fn string_offsets(&self) -> &[u32] {
        &self.string_offsets
    }
}

/// Allocate `len` bytes inside WebAssembly linear memory.
#[no_mangle]
#[allow(unsafe_code)]
pub extern "C" fn labstream_wasm_alloc(len: usize) -> *mut u8 {
    if len == 0 {
        return core::ptr::null_mut();
    }
    let mut buf = Vec::<u8>::with_capacity(len);
    let ptr = buf.as_mut_ptr();
    core::mem::forget(buf);
    ptr
}

/// Free a buffer previously allocated with [`labstream_wasm_alloc`].
///
/// # Safety
/// `ptr` must have been returned by `labstream_wasm_alloc(len)` and not freed.
#[no_mangle]
#[allow(unsafe_code)]
pub unsafe extern "C" fn labstream_wasm_dealloc(ptr: *mut u8, len: usize) {
    if !ptr.is_null() && len > 0 {
        drop(Vec::from_raw_parts(ptr, 0, len));
    }
}

/// Create a new [`WasmDecoder`] handle for `format` (`1..=7`) and `channels`.
#[no_mangle]
#[allow(unsafe_code)]
pub extern "C" fn labstream_wasm_decoder_new(format: u8, channels: u32) -> *mut WasmDecoder {
    let Some(fmt) = Format::from_u8(format) else {
        return core::ptr::null_mut();
    };
    match WasmDecoder::new(fmt, channels as usize) {
        Some(dec) => Box::into_raw(Box::new(dec)),
        None => core::ptr::null_mut(),
    }
}

/// Destroy a [`WasmDecoder`] handle.
///
/// # Safety
/// `dec` must be null or a valid handle from [`labstream_wasm_decoder_new`].
#[no_mangle]
#[allow(unsafe_code)]
pub unsafe extern "C" fn labstream_wasm_decoder_free(dec: *mut WasmDecoder) {
    if !dec.is_null() {
        drop(Box::from_raw(dec));
    }
}

/// Decode a binary WebSocket frame in WASM linear memory.
///
/// # Safety
/// `dec` must be a valid handle and `frame_ptr..frame_ptr + frame_len` must be readable.
#[no_mangle]
#[allow(unsafe_code)]
pub unsafe extern "C" fn labstream_wasm_decode_frame(
    dec: *mut WasmDecoder,
    frame_ptr: *const u8,
    frame_len: usize,
) -> i32 {
    let Some(d) = dec.as_mut() else {
        return -1;
    };
    if frame_ptr.is_null() {
        return -1;
    }
    let slice = core::slice::from_raw_parts(frame_ptr, frame_len);
    d.decode_frame(slice)
}

/// Return the subscription `stream_id` from the last decoded frame.
///
/// # Safety
/// `dec` must be null or a valid handle.
#[no_mangle]
#[allow(unsafe_code)]
pub unsafe extern "C" fn labstream_wasm_stream_id(dec: *const WasmDecoder) -> u32 {
    dec.as_ref().map_or(0, WasmDecoder::stream_id)
}

/// Pointer to decoded `f64` timestamps in linear memory.
///
/// # Safety
/// `dec` must be null or a valid handle.
#[no_mangle]
#[allow(unsafe_code)]
pub unsafe extern "C" fn labstream_wasm_timestamps_ptr(dec: *const WasmDecoder) -> *const f64 {
    dec.as_ref()
        .map_or(core::ptr::null(), |d| d.timestamps.as_ptr())
}

/// Pointer to decoded flat little-endian numeric sample bytes in linear memory.
///
/// # Safety
/// `dec` must be null or a valid handle.
#[no_mangle]
#[allow(unsafe_code)]
pub unsafe extern "C" fn labstream_wasm_values_ptr(dec: *const WasmDecoder) -> *const u8 {
    dec.as_ref()
        .map_or(core::ptr::null(), |d| d.values_bytes.as_ptr())
}

/// Byte length of the decoded numeric sample buffer.
///
/// # Safety
/// `dec` must be null or a valid handle.
#[no_mangle]
#[allow(unsafe_code)]
pub unsafe extern "C" fn labstream_wasm_values_len(dec: *const WasmDecoder) -> usize {
    dec.as_ref().map_or(0, |d| d.values_bytes.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use labstream_wire::Sample;

    #[test]
    fn wasm_decoder_decodes_512hz_8ch_frame_bit_for_bit() {
        let codec = Codec::new(Format::Float32, 8, ByteOrder::Little);
        let mut frame = Vec::new();
        frame.extend_from_slice(&7u32.to_le_bytes()); // stream_id
        frame.extend_from_slice(&4u32.to_le_bytes()); // 4 samples
        let mut expected_ts = Vec::new();
        let mut expected_vals = Vec::new();
        for s in 0..4 {
            let ts = 100.0 + s as f64 / 512.0;
            expected_ts.push(ts);
            let mut vals = Vec::new();
            for c in 0..8 {
                let v = (s * 8 + c) as f32 * 0.5 - 10.25;
                expected_vals.push(v);
                vals.push(Value::F32(v));
            }
            codec
                .encode(
                    &Sample {
                        timestamp: ts,
                        values: vals,
                    },
                    &mut frame,
                )
                .unwrap();
        }

        let mut dec = WasmDecoder::new(Format::Float32, 8).unwrap();
        assert_eq!(dec.decode_frame(&frame), 4);
        assert_eq!(dec.stream_id(), 7);
        assert_eq!(dec.timestamps().len(), 4);
        for (a, b) in dec.timestamps().iter().zip(&expected_ts) {
            assert_eq!(a.to_bits(), b.to_bits());
        }
        let raw = dec.values_bytes();
        assert_eq!(raw.len(), 4 * 8 * 4);
        for (i, chunk) in raw.chunks_exact(4).enumerate() {
            let got = f32::from_le_bytes(chunk.try_into().unwrap());
            assert_eq!(got.to_bits(), expected_vals[i].to_bits());
        }
    }
}
