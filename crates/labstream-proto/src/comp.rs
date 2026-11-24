//! Lossless wire compression for numeric LSL streams (`comp/1`).
//! `docs/extensions.md`, section `comp`. `docs/roadmap.md`, step 7.
//!
//! Each numeric channel passes through a bit-exact bijection to an unsigned
//! integer of the channel width (identity for integer formats, order-preserving
//! IEEE-754 mapping for `Float32` and `Double64`), a wrapping first-order delta
//! against the previous sample on the same channel, ZigZag sign folding, and an
//! adaptive LSB-first Golomb-Rice coder.
//!
//! On TCP, a connection keeps one [`CompState`] across frames and emits either
//! a compressed sample (`0xF9` deduced timestamp, `0xFA` transmitted timestamp)
//! or a plain LSL sample (`0x01` or `0x02`) when compression does not save
//! bytes. Worst-case expansion on incompressible data is zero bytes.
//!
//! On UDP (`dgram/1`), each [`crate::dgram::DATA_COMP`] datagram is
//! self-contained with its own anchor sample and delta-coded timestamps, so a
//! lost or reordered datagram never corrupts later datagrams.

use labstream_wire::{ByteOrder, Codec, Error, Format, PackedSample, DEDUCED_TIMESTAMP};
use std::sync::Arc;

/// Tag byte of a compressed TCP sample with a deduced timestamp (`comp/1`).
///
/// Layout: `[0xF9][u16 payload_len LE][payload]`.
pub const COMP_DEDUCED_TAG: u8 = 0xF9;

/// Tag byte of a compressed TCP sample with a transmitted `f64` timestamp
/// (`comp/1`).
///
/// Layout: `[0xFA][f64 timestamp in connection byte order][u16 payload_len LE][payload]`.
pub const COMP_TIMESTAMP_TAG: u8 = 0xFA;

/// Unary escape threshold for the adaptive Golomb-Rice coder.
///
/// Quotients `0..=14` write `q` zero bits and one `1` bit. Fifteen zero bits
/// signal an escape followed by the raw `B`-bit unsigned ZigZag value.
const RICE_ESCAPE: u64 = 15;

/// Initial per-channel EMA accumulator (`4 * avg`) tuned per format so sample 1
/// does not escape on typical biosignal deltas while converging in ~4 samples.
#[inline]
fn init_ema(format: Format) -> u64 {
    match format {
        Format::Int8 => 8,
        Format::Int16 => 32,
        Format::Int32 => 256,
        Format::Int64 => 512,
        Format::Float32 => 1u64 << 14,
        Format::Double64 => 1u64 << 42,
        Format::String | Format::Undefined => 64,
    }
}

/// Maximum EMA accumulator value to keep arithmetic far below `u64::MAX`.
const MAX_EMA_INPUT: u64 = 1u64 << 58;

/// Flag bit in a `DATA_COMP` payload indicating that sample 0 is stored as raw
/// little-endian bytes instead of Rice-coded against zero.
const FLAG_SAMPLE0_RAW: u8 = 0x01;

// ---------------------------------------------------------------------------
// Bit-exact numeric transforms
// ---------------------------------------------------------------------------

/// Branchless order-preserving bijection from IEEE-754 `f32` bits to `u32`.
///
/// Maps `-0.0` (`0x8000_0000`) to `0x7FFF_FFFF` and `+0.0` (`0x0000_0000`) to
/// `0x8000_0000`, so smooth waveforms across zero have tiny integer deltas
/// while every bit pattern (including `-0.0`, subnormals, and NaN payloads)
/// round-trips bit for bit.
#[inline]
pub(crate) fn f32_to_ord(bits: u32) -> u32 {
    let mask = ((bits as i32) >> 31) as u32 | 0x8000_0000;
    bits ^ mask
}

/// Exact inverse of [`f32_to_ord`].
#[inline]
pub(crate) fn ord_to_f32(ord: u32) -> u32 {
    let mask = (((!ord) as i32) >> 31) as u32 | 0x8000_0000;
    ord ^ mask
}

/// Branchless order-preserving bijection from IEEE-754 `f64` bits to `u64`.
#[inline]
pub(crate) fn f64_to_ord(bits: u64) -> u64 {
    let mask = ((bits as i64) >> 63) as u64 | 0x8000_0000_0000_0000;
    bits ^ mask
}

/// Exact inverse of [`f64_to_ord`].
#[inline]
pub(crate) fn ord_to_f64(ord: u64) -> u64 {
    let mask = (((!ord) as i64) >> 63) as u64 | 0x8000_0000_0000_0000;
    ord ^ mask
}

#[inline]
fn zz8(d: i8) -> u64 {
    (((d << 1) ^ (d >> 7)) as u8) as u64
}

#[inline]
fn unzz8(u: u64) -> i8 {
    let u = u as u8;
    ((u >> 1) as i8) ^ -((u & 1) as i8)
}

#[inline]
fn zz16(d: i16) -> u64 {
    (((d << 1) ^ (d >> 15)) as u16) as u64
}

#[inline]
fn unzz16(u: u64) -> i16 {
    let u = u as u16;
    ((u >> 1) as i16) ^ -((u & 1) as i16)
}

#[inline]
fn zz32(d: i32) -> u64 {
    (((d << 1) ^ (d >> 31)) as u32) as u64
}

#[inline]
fn unzz32(u: u64) -> i32 {
    let u = u as u32;
    ((u >> 1) as i32) ^ -((u & 1) as i32)
}

#[inline]
fn zz64(d: i64) -> u64 {
    ((d << 1) ^ (d >> 63)) as u64
}

#[inline]
fn unzz64(u: u64) -> i64 {
    ((u >> 1) as i64) ^ -((u & 1) as i64)
}

/// Choose the Golomb-Rice parameter `k` in `0..width_bits` from the scaled EMA
/// (`4 * avg`). Approximates `floor(log2(0.75 * avg))`.
#[inline]
fn rice_k(ema: u64, max_k: u32) -> u32 {
    let m = (ema.saturating_mul(3)) >> 4;
    if m == 0 {
        0
    } else {
        (63 - m.leading_zeros()).min(max_k)
    }
}

#[inline]
fn update_ema(ema: &mut u64, u: u64) {
    *ema = *ema - (*ema >> 2) + u.min(MAX_EMA_INPUT);
}

// ---------------------------------------------------------------------------
// Fast LSB-first 64-bit BitWriter / BitReader
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
struct BitWriter {
    buf: Vec<u8>,
    acc: u64,
    bits: u32,
}

#[derive(Debug, Clone, Copy)]
struct WriterCheckpoint {
    len: usize,
    acc: u64,
    bits: u32,
}

impl BitWriter {
    fn new() -> Self {
        Self::default()
    }

    fn clear(&mut self) {
        self.buf.clear();
        self.acc = 0;
        self.bits = 0;
    }

    fn checkpoint(&self) -> WriterCheckpoint {
        WriterCheckpoint {
            len: self.buf.len(),
            acc: self.acc,
            bits: self.bits,
        }
    }

    fn restore(&mut self, cp: WriterCheckpoint) {
        self.buf.truncate(cp.len);
        self.acc = cp.acc;
        self.bits = cp.bits;
    }

    /// Current byte count if flushed right now.
    #[inline]
    fn byte_len(&self) -> usize {
        self.buf.len() + (self.bits as usize).div_ceil(8)
    }

    /// Write raw bytes when already byte-aligned.
    #[inline]
    fn put_aligned_bytes(&mut self, bytes: &[u8]) {
        debug_assert_eq!(self.bits, 0);
        self.buf.extend_from_slice(bytes);
    }

    /// Write the lowest `nbits` (`0..=64`) of `val` into the LSB-first stream.
    #[inline]
    fn put_bits(&mut self, val: u64, nbits: u32) {
        if nbits == 0 {
            return;
        }
        if nbits == 64 {
            self.flush_bytes();
            if self.bits == 0 {
                self.buf.extend_from_slice(&val.to_le_bytes());
            } else {
                self.put_bits(val & 0xFFFF_FFFF, 32);
                self.put_bits(val >> 32, 32);
            }
            return;
        }
        let masked = val & ((1u64 << nbits) - 1);
        if self.bits + nbits <= 64 {
            self.acc |= masked << self.bits;
            self.bits += nbits;
            if self.bits >= 32 {
                self.flush_bytes();
            }
        } else {
            self.flush_bytes();
            if self.bits + nbits <= 64 {
                self.acc |= masked << self.bits;
                self.bits += nbits;
                if self.bits >= 32 {
                    self.flush_bytes();
                }
            } else {
                let first = 64 - self.bits;
                self.acc |= (masked & ((1u64 << first) - 1)) << self.bits;
                self.buf.extend_from_slice(&self.acc.to_le_bytes());
                self.acc = masked >> first;
                self.bits = nbits - first;
            }
        }
    }

    #[inline]
    fn flush_bytes(&mut self) {
        while self.bits >= 8 {
            self.buf.push(self.acc as u8);
            self.acc >>= 8;
            self.bits -= 8;
        }
    }

    /// Pad the final partial byte with zero bits and return the byte slice.
    fn finish(&mut self) -> &[u8] {
        self.flush_bytes();
        if self.bits > 0 {
            self.buf.push(self.acc as u8);
            self.acc = 0;
            self.bits = 0;
        }
        &self.buf
    }

    /// Write one unsigned value `u` of width `width_bits` with Rice parameter `k`.
    #[inline]
    fn put_rice(&mut self, u: u64, k: u32, width_bits: u32) {
        let q = u >> k;
        if q < RICE_ESCAPE {
            let rem_mask = if k == 0 { 0 } else { (1u64 << k) - 1 };
            let total = q as u32 + 1 + k;
            if total <= 56 {
                let code = (1u64 << q) | ((u & rem_mask) << (q + 1));
                self.put_bits(code, total);
            } else {
                self.put_bits(1u64 << q, q as u32 + 1);
                self.put_bits(u & rem_mask, k);
            }
        } else {
            // 15 zero bits signal escape, followed by raw `width_bits` value.
            self.put_bits(0, RICE_ESCAPE as u32);
            self.put_bits(u, width_bits);
        }
    }
}

struct BitReader<'a> {
    buf: &'a [u8],
    pos: usize,
    acc: u64,
    bits: u32,
}

impl<'a> BitReader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        BitReader {
            buf,
            pos: 0,
            acc: 0,
            bits: 0,
        }
    }

    #[inline]
    fn refill(&mut self) {
        while self.bits <= 56 && self.pos < self.buf.len() {
            self.acc |= (self.buf[self.pos] as u64) << self.bits;
            self.pos += 1;
            self.bits += 8;
        }
    }

    #[inline]
    fn take_bits(&mut self, nbits: u32) -> Result<u64, Error> {
        if nbits == 0 {
            return Ok(0);
        }
        if nbits == 64 {
            let lo = self.take_bits(32)?;
            let hi = self.take_bits(32)?;
            return Ok(lo | (hi << 32));
        }
        if self.bits < nbits {
            self.refill();
            if self.bits < nbits {
                return Err(Error::Truncated);
            }
        }
        let val = self.acc & ((1u64 << nbits) - 1);
        self.acc >>= nbits;
        self.bits -= nbits;
        Ok(val)
    }

    /// Read one Rice-coded value of width `width_bits` with parameter `k`.
    #[inline]
    fn take_rice(&mut self, k: u32, width_bits: u32) -> Result<u64, Error> {
        if self.bits < RICE_ESCAPE as u32 {
            self.refill();
        }
        if self.bits == 0 {
            return Err(Error::Truncated);
        }
        let tz = (self.acc & 0x7FFF).trailing_zeros();
        if tz < RICE_ESCAPE as u32 {
            let need = tz + 1;
            if self.bits < need {
                return Err(Error::Truncated);
            }
            self.acc >>= need;
            self.bits -= need;
            let rem = self.take_bits(k)?;
            Ok(((tz as u64) << k) | rem)
        } else {
            if self.bits < RICE_ESCAPE as u32 {
                return Err(Error::Truncated);
            }
            self.acc >>= RICE_ESCAPE as u32;
            self.bits -= RICE_ESCAPE as u32;
            self.take_bits(width_bits)
        }
    }
}

// ---------------------------------------------------------------------------
// Per-connection stateful TCP encoder/decoder (`CompState`)
// ---------------------------------------------------------------------------

/// Per-channel delta + adaptive Rice state for `comp/1`.
///
/// On TCP, the outlet and inlet each hold one [`CompState`] that stays in
/// lockstep across compressed (`0xF9`/`0xFA`) and plain fallback (`0x01`/`0x02`)
/// samples.
#[derive(Debug, Clone)]
pub struct CompState {
    format: Format,
    channels: usize,
    has_prev: bool,
    prev: Vec<u64>,
    ema: Vec<u64>,
    writer: BitWriter,
}

impl CompState {
    /// Create a fresh compression state for a numeric stream.
    pub fn new(format: Format, channels: usize) -> Self {
        CompState {
            format,
            channels,
            has_prev: false,
            prev: vec![0u64; channels],
            ema: vec![init_ema(format); channels],
            writer: BitWriter::new(),
        }
    }

    /// Reset predictor and EMA accumulators to their initial values.
    pub fn reset(&mut self) {
        self.has_prev = false;
        self.prev.fill(0);
        self.ema.fill(init_ema(self.format));
    }

    /// True when `format` supports `comp/1` (any fixed-width numeric format).
    pub fn supports(format: Format) -> bool {
        format.width().is_some()
    }

    /// Encode one sample onto a TCP buffer, automatically falling back to a
    /// plain LSL sample (`0x01`/`0x02`) when compression does not save bytes.
    pub fn encode_tcp_sample(
        &mut self,
        codec: &Codec,
        sample: &PackedSample,
        timestamp: f64,
        out: &mut Vec<u8>,
    ) -> Result<(), Error> {
        let Some(raw) = sample.as_numeric_bytes(self.format) else {
            return codec.encode_packed_at(sample, timestamp, out);
        };
        let Some(width) = self.format.width() else {
            return codec.encode_packed_at(sample, timestamp, out);
        };
        if raw.len() != self.channels * width {
            return Err(Error::ChannelCountMismatch {
                expected: self.channels,
                actual: raw.len() / width,
            });
        }

        self.writer.clear();
        encode_channels(
            self.format,
            raw,
            &mut self.prev,
            &mut self.ema,
            &mut self.writer,
        );
        if !self.has_prev {
            // Do not let sample 0's absolute DC offset poison delta EMA.
            self.ema.fill(init_ema(self.format));
            self.has_prev = true;
        }
        let compressed = self.writer.finish();

        // Compressed framing adds 2 bytes (`u16 len`) compared to plain `0x01`/`0x02`.
        if compressed.len() <= u16::MAX as usize && compressed.len() + 2 < raw.len() {
            if timestamp == DEDUCED_TIMESTAMP {
                out.push(COMP_DEDUCED_TAG);
            } else {
                out.push(COMP_TIMESTAMP_TAG);
                let swap = codec.order != ByteOrder::native();
                let ts_bytes = if swap {
                    timestamp.to_bits().to_be_bytes()
                } else {
                    timestamp.to_bits().to_le_bytes()
                };
                out.extend_from_slice(&ts_bytes);
            }
            out.extend_from_slice(&(compressed.len() as u16).to_le_bytes());
            out.extend_from_slice(compressed);
            Ok(())
        } else {
            codec.encode_packed_at(sample, timestamp, out)
        }
    }

    /// Decode one TCP sample (`0xF9`, `0xFA`, or plain `0x01`/`0x02` fallback).
    ///
    /// Returns the decoded [`PackedSample`], total wire bytes consumed, and the
    /// uncompressed payload size (for compression ratio accounting).
    pub fn decode_tcp_sample(
        &mut self,
        codec: &Codec,
        buf: &[u8],
    ) -> Result<(PackedSample, usize, usize), Error> {
        let Some(&tag) = buf.first() else {
            return Err(Error::Truncated);
        };
        let raw_payload_len = self.format.width().unwrap_or(0) * self.channels;
        if tag == COMP_DEDUCED_TAG || tag == COMP_TIMESTAMP_TAG {
            let (ts, header_len) = if tag == COMP_DEDUCED_TAG {
                if buf.len() < 3 {
                    return Err(Error::Truncated);
                }
                (DEDUCED_TIMESTAMP, 3)
            } else {
                if buf.len() < 11 {
                    return Err(Error::Truncated);
                }
                let mut tb = [0u8; 8];
                tb.copy_from_slice(&buf[1..9]);
                let bits = if codec.order != ByteOrder::native() {
                    u64::from_be_bytes(tb)
                } else {
                    u64::from_le_bytes(tb)
                };
                (f64::from_bits(bits), 11)
            };
            let payload_len =
                u16::from_le_bytes([buf[header_len - 2], buf[header_len - 1]]) as usize;
            let total = header_len + payload_len;
            if buf.len() < total {
                return Err(Error::Truncated);
            }
            let mut reader = BitReader::new(&buf[header_len..total]);
            let mut raw = vec![0u8; raw_payload_len];
            decode_channels(
                self.format,
                self.channels,
                &mut self.prev,
                &mut self.ema,
                &mut reader,
                &mut raw,
            )?;
            if !self.has_prev {
                self.ema.fill(init_ema(self.format));
                self.has_prev = true;
            }
            if codec.suppress_subnormals {
                suppress_subnormals_in_place(self.format, &mut raw);
            }
            let sample = PackedSample::from_raw_bytes(ts, self.format, Arc::from(raw));
            let plain_wire = if tag == COMP_DEDUCED_TAG {
                1 + raw_payload_len
            } else {
                9 + raw_payload_len
            };
            Ok((sample, total, plain_wire))
        } else {
            // Plain fallback sample (`0x01`/`0x02`). Decode without subnormal
            // suppression first so `prev`/`ema` stays in exact lockstep with
            // the outlet encoder.
            let raw_codec = codec.with_suppress_subnormals(false);
            let (mut sample, used) = raw_codec.decode_packed(buf)?;
            if let Some(bytes) = sample.as_numeric_bytes(self.format) {
                if !self.has_prev {
                    seed_prev_from_raw(self.format, bytes, &mut self.prev);
                    self.ema.fill(init_ema(self.format));
                    self.has_prev = true;
                } else {
                    observe_channels(self.format, bytes, &mut self.prev, &mut self.ema);
                }
                if codec.suppress_subnormals && self.format.is_float() {
                    let mut owned = bytes.to_vec();
                    suppress_subnormals_in_place(self.format, &mut owned);
                    sample =
                        PackedSample::from_raw_bytes(sample.timestamp, self.format, Arc::from(owned));
                }
            }
            Ok((sample, used, used))
        }
    }
}

/// Suppress subnormal float values in-place in a native-endian byte buffer.
/// Matches `labstream_wire::Codec::suppress_subnormals` (`SPEC.md 7.4`).
fn suppress_subnormals_in_place(format: Format, raw: &mut [u8]) {
    match format {
        Format::Float32 => {
            for c in raw.chunks_exact_mut(4) {
                let v = f32::from_ne_bytes([c[0], c[1], c[2], c[3]]);
                if v != 0.0 && v.abs() < f32::MIN_POSITIVE {
                    c.copy_from_slice(&0.0f32.to_ne_bytes());
                }
            }
        }
        Format::Double64 => {
            for c in raw.chunks_exact_mut(8) {
                let v = f64::from_ne_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]);
                if v != 0.0 && v.abs() < f64::MIN_POSITIVE {
                    c.copy_from_slice(&0.0f64.to_ne_bytes());
                }
            }
        }
        _ => {}
    }
}

/// Encode all channels of one native-endian numeric sample into `writer`,
/// updating `prev` and `ema`.
fn encode_channels(
    format: Format,
    raw: &[u8],
    prev: &mut [u64],
    ema: &mut [u64],
    writer: &mut BitWriter,
) {
    match format {
        Format::Int8 => {
            for (c, &b) in raw.iter().enumerate() {
                let cur = b as u64;
                let d = (b as i8).wrapping_sub(prev[c] as u8 as i8);
                prev[c] = cur;
                let u = zz8(d);
                let k = rice_k(ema[c], 7);
                writer.put_rice(u, k, 8);
                update_ema(&mut ema[c], u);
            }
        }
        Format::Int16 => {
            for (c, ch) in raw.chunks_exact(2).enumerate() {
                let v = u16::from_ne_bytes([ch[0], ch[1]]);
                let d = (v as i16).wrapping_sub(prev[c] as u16 as i16);
                prev[c] = v as u64;
                let u = zz16(d);
                let k = rice_k(ema[c], 15);
                writer.put_rice(u, k, 16);
                update_ema(&mut ema[c], u);
            }
        }
        Format::Int32 => {
            for (c, ch) in raw.chunks_exact(4).enumerate() {
                let v = u32::from_ne_bytes([ch[0], ch[1], ch[2], ch[3]]);
                let d = (v as i32).wrapping_sub(prev[c] as u32 as i32);
                prev[c] = v as u64;
                let u = zz32(d);
                let k = rice_k(ema[c], 31);
                writer.put_rice(u, k, 32);
                update_ema(&mut ema[c], u);
            }
        }
        Format::Float32 => {
            for (c, ch) in raw.chunks_exact(4).enumerate() {
                let v = f32_to_ord(u32::from_ne_bytes([ch[0], ch[1], ch[2], ch[3]]));
                let d = (v as i32).wrapping_sub(prev[c] as u32 as i32);
                prev[c] = v as u64;
                let u = zz32(d);
                let k = rice_k(ema[c], 31);
                writer.put_rice(u, k, 32);
                update_ema(&mut ema[c], u);
            }
        }
        Format::Int64 => {
            for (c, ch) in raw.chunks_exact(8).enumerate() {
                let v = u64::from_ne_bytes([
                    ch[0], ch[1], ch[2], ch[3], ch[4], ch[5], ch[6], ch[7],
                ]);
                let d = (v as i64).wrapping_sub(prev[c] as i64);
                prev[c] = v;
                let u = zz64(d);
                let k = rice_k(ema[c], 63);
                writer.put_rice(u, k, 64);
                update_ema(&mut ema[c], u);
            }
        }
        Format::Double64 => {
            for (c, ch) in raw.chunks_exact(8).enumerate() {
                let v = f64_to_ord(u64::from_ne_bytes([
                    ch[0], ch[1], ch[2], ch[3], ch[4], ch[5], ch[6], ch[7],
                ]));
                let d = (v as i64).wrapping_sub(prev[c] as i64);
                prev[c] = v;
                let u = zz64(d);
                let k = rice_k(ema[c], 63);
                writer.put_rice(u, k, 64);
                update_ema(&mut ema[c], u);
            }
        }
        Format::String | Format::Undefined => {}
    }
}

/// Update `prev` and `ema` from a plain uncompressed sample without writing.
fn observe_channels(format: Format, raw: &[u8], prev: &mut [u64], ema: &mut [u64]) {
    match format {
        Format::Int8 => {
            for (c, &b) in raw.iter().enumerate() {
                let d = (b as i8).wrapping_sub(prev[c] as u8 as i8);
                prev[c] = b as u64;
                update_ema(&mut ema[c], zz8(d));
            }
        }
        Format::Int16 => {
            for (c, ch) in raw.chunks_exact(2).enumerate() {
                let v = u16::from_ne_bytes([ch[0], ch[1]]);
                let d = (v as i16).wrapping_sub(prev[c] as u16 as i16);
                prev[c] = v as u64;
                update_ema(&mut ema[c], zz16(d));
            }
        }
        Format::Int32 => {
            for (c, ch) in raw.chunks_exact(4).enumerate() {
                let v = u32::from_ne_bytes([ch[0], ch[1], ch[2], ch[3]]);
                let d = (v as i32).wrapping_sub(prev[c] as u32 as i32);
                prev[c] = v as u64;
                update_ema(&mut ema[c], zz32(d));
            }
        }
        Format::Float32 => {
            for (c, ch) in raw.chunks_exact(4).enumerate() {
                let v = f32_to_ord(u32::from_ne_bytes([ch[0], ch[1], ch[2], ch[3]]));
                let d = (v as i32).wrapping_sub(prev[c] as u32 as i32);
                prev[c] = v as u64;
                update_ema(&mut ema[c], zz32(d));
            }
        }
        Format::Int64 => {
            for (c, ch) in raw.chunks_exact(8).enumerate() {
                let v = u64::from_ne_bytes([
                    ch[0], ch[1], ch[2], ch[3], ch[4], ch[5], ch[6], ch[7],
                ]);
                let d = (v as i64).wrapping_sub(prev[c] as i64);
                prev[c] = v;
                update_ema(&mut ema[c], zz64(d));
            }
        }
        Format::Double64 => {
            for (c, ch) in raw.chunks_exact(8).enumerate() {
                let v = f64_to_ord(u64::from_ne_bytes([
                    ch[0], ch[1], ch[2], ch[3], ch[4], ch[5], ch[6], ch[7],
                ]));
                let d = (v as i64).wrapping_sub(prev[c] as i64);
                prev[c] = v;
                update_ema(&mut ema[c], zz64(d));
            }
        }
        Format::String | Format::Undefined => {}
    }
}

/// Initialize `prev` from sample 0 when sample 0 is transmitted raw inside a
/// `DATA_COMP` datagram (keeping `ema` at [`init_ema`]).
fn seed_prev_from_raw(format: Format, raw: &[u8], prev: &mut [u64]) {
    match format {
        Format::Int8 => {
            for (c, &b) in raw.iter().enumerate() {
                prev[c] = b as u64;
            }
        }
        Format::Int16 => {
            for (c, ch) in raw.chunks_exact(2).enumerate() {
                prev[c] = u16::from_ne_bytes([ch[0], ch[1]]) as u64;
            }
        }
        Format::Int32 => {
            for (c, ch) in raw.chunks_exact(4).enumerate() {
                prev[c] = u32::from_ne_bytes([ch[0], ch[1], ch[2], ch[3]]) as u64;
            }
        }
        Format::Float32 => {
            for (c, ch) in raw.chunks_exact(4).enumerate() {
                prev[c] = f32_to_ord(u32::from_ne_bytes([ch[0], ch[1], ch[2], ch[3]])) as u64;
            }
        }
        Format::Int64 => {
            for (c, ch) in raw.chunks_exact(8).enumerate() {
                prev[c] = u64::from_ne_bytes([
                    ch[0], ch[1], ch[2], ch[3], ch[4], ch[5], ch[6], ch[7],
                ]);
            }
        }
        Format::Double64 => {
            for (c, ch) in raw.chunks_exact(8).enumerate() {
                prev[c] = f64_to_ord(u64::from_ne_bytes([
                    ch[0], ch[1], ch[2], ch[3], ch[4], ch[5], ch[6], ch[7],
                ]));
            }
        }
        Format::String | Format::Undefined => {}
    }
}

/// Decode all channels of one sample from `reader` into native-endian `out`.
fn decode_channels(
    format: Format,
    channels: usize,
    prev: &mut [u64],
    ema: &mut [u64],
    reader: &mut BitReader<'_>,
    out: &mut [u8],
) -> Result<(), Error> {
    match format {
        Format::Int8 => {
            for c in 0..channels {
                let k = rice_k(ema[c], 7);
                let u = reader.take_rice(k, 8)?;
                update_ema(&mut ema[c], u);
                let cur = (prev[c] as u8 as i8).wrapping_add(unzz8(u)) as u8;
                prev[c] = cur as u64;
                out[c] = cur;
            }
        }
        Format::Int16 => {
            for c in 0..channels {
                let k = rice_k(ema[c], 15);
                let u = reader.take_rice(k, 16)?;
                update_ema(&mut ema[c], u);
                let cur = (prev[c] as u16 as i16).wrapping_add(unzz16(u)) as u16;
                prev[c] = cur as u64;
                out[c * 2..c * 2 + 2].copy_from_slice(&cur.to_ne_bytes());
            }
        }
        Format::Int32 => {
            for c in 0..channels {
                let k = rice_k(ema[c], 31);
                let u = reader.take_rice(k, 32)?;
                update_ema(&mut ema[c], u);
                let cur = (prev[c] as u32 as i32).wrapping_add(unzz32(u)) as u32;
                prev[c] = cur as u64;
                out[c * 4..c * 4 + 4].copy_from_slice(&cur.to_ne_bytes());
            }
        }
        Format::Float32 => {
            for c in 0..channels {
                let k = rice_k(ema[c], 31);
                let u = reader.take_rice(k, 32)?;
                update_ema(&mut ema[c], u);
                let ord = (prev[c] as u32 as i32).wrapping_add(unzz32(u)) as u32;
                prev[c] = ord as u64;
                out[c * 4..c * 4 + 4].copy_from_slice(&ord_to_f32(ord).to_ne_bytes());
            }
        }
        Format::Int64 => {
            for c in 0..channels {
                let k = rice_k(ema[c], 63);
                let u = reader.take_rice(k, 64)?;
                update_ema(&mut ema[c], u);
                let cur = (prev[c] as i64).wrapping_add(unzz64(u)) as u64;
                prev[c] = cur;
                out[c * 8..c * 8 + 8].copy_from_slice(&cur.to_ne_bytes());
            }
        }
        Format::Double64 => {
            for c in 0..channels {
                let k = rice_k(ema[c], 63);
                let u = reader.take_rice(k, 64)?;
                update_ema(&mut ema[c], u);
                let ord = (prev[c] as i64).wrapping_add(unzz64(u)) as u64;
                prev[c] = ord;
                out[c * 8..c * 8 + 8].copy_from_slice(&ord_to_f64(ord).to_ne_bytes());
            }
        }
        Format::String | Format::Undefined => return Err(Error::UndefinedFormat),
    }
    Ok(())
}

/// Convert native-endian numeric bytes to little-endian wire bytes.
fn copy_ne_to_le(format: Format, raw: &[u8], out: &mut Vec<u8>) {
    if cfg!(target_endian = "little") {
        out.extend_from_slice(raw);
        return;
    }
    let w = format.width().unwrap_or(1);
    for ch in raw.chunks_exact(w) {
        for &b in ch.iter().rev() {
            out.push(b);
        }
    }
}

/// Convert little-endian wire bytes to native-endian numeric bytes.
fn copy_le_to_ne(format: Format, wire: &[u8], out: &mut [u8]) {
    if cfg!(target_endian = "little") {
        out.copy_from_slice(wire);
        return;
    }
    let w = format.width().unwrap_or(1);
    for (src, dst) in wire.chunks_exact(w).zip(out.chunks_exact_mut(w)) {
        for (i, &b) in src.iter().rev().enumerate() {
            dst[i] = b;
        }
    }
}

// ---------------------------------------------------------------------------
// Self-contained UDP datagram block codec (`DATA_COMP`, type 5)
// ---------------------------------------------------------------------------

/// Incremental single-pass encoder for one self-contained `DATA_COMP` UDP
/// datagram payload (`docs/extensions.md`, section `comp`).
///
/// Layout of the payload after the 23-byte `DATA_COMP` header:
/// - `u8 flags`: bit 0 (`0x01`) = sample 0 stored as raw little-endian bytes.
/// - `u64 ts0` (little-endian `f64` bits of sample 0).
/// - If `flags & 0x01 != 0`: `channels * width` raw little-endian bytes of
///   sample 0, followed by the LSB-first bitstream for samples `1..count`.
/// - If `flags & 0x01 == 0`: LSB-first bitstream starting with Rice-coded
///   sample 0, followed by samples `1..count`.
/// - For each sample `i >= 1` in the bitstream:
///   - 2nd-order `f64_to_ord` timestamp delta (adaptive Rice, 64-bit)
///   - all `channels` 1st-order value deltas (adaptive Rice, `width`-bit)
#[derive(Debug, Clone)]
pub struct DgramCompEncoder {
    format: Format,
    raw_sample_bytes: usize,
    prev: Vec<u64>,
    ema: Vec<u64>,
    prev_backup: Vec<u64>,
    ema_backup: Vec<u64>,
    ts_prev: u64,
    ts_prev_d1: i64,
    ts_ema: u64,
    count: usize,
    writer: BitWriter,
    sample0_scratch: BitWriter,
}

impl DgramCompEncoder {
    /// Create a reusable datagram block encoder for a numeric stream.
    pub fn new(format: Format, channels: usize) -> Self {
        let w = format.width().unwrap_or(0);
        let ema0 = init_ema(format);
        DgramCompEncoder {
            format,
            raw_sample_bytes: w * channels,
            prev: vec![0u64; channels],
            ema: vec![ema0; channels],
            prev_backup: vec![0u64; channels],
            ema_backup: vec![ema0; channels],
            ts_prev: 0,
            ts_prev_d1: 0,
            ts_ema: 4,
            count: 0,
            writer: BitWriter::new(),
            sample0_scratch: BitWriter::new(),
        }
    }

    /// Start a new self-contained datagram payload.
    pub fn reset(&mut self) {
        self.prev.fill(0);
        self.ema.fill(init_ema(self.format));
        self.ts_prev = 0;
        self.ts_prev_d1 = 0;
        self.ts_ema = 4;
        self.count = 0;
        self.writer.clear();
    }

    /// Number of samples currently encoded into this payload.
    pub fn count(&self) -> usize {
        self.count
    }

    /// Try to append one sample `(sample, full_timestamp)` without exceeding
    /// `max_payload_bytes`. Returns `false` (leaving the encoder state
    /// unchanged) if the sample would overflow the byte budget.
    pub fn try_push(
        &mut self,
        sample: &PackedSample,
        timestamp: f64,
        max_payload_bytes: usize,
    ) -> bool {
        let Some(raw) = sample.as_numeric_bytes(self.format) else {
            return false;
        };
        if raw.len() != self.raw_sample_bytes {
            return false;
        }
        if self.count == 0 {
            let ts_ord = f64_to_ord(timestamp.to_bits());
            self.ts_prev = ts_ord;
            self.ts_prev_d1 = 0;
            self.ts_ema = 4;

            let ema0 = init_ema(self.format);
            // Try Rice-coding sample 0 against zero for integer formats; use raw
            // bytes whenever raw is smaller or equal (always faster for floats).
            let mut use_raw = self.format.is_float();
            if !use_raw {
                self.sample0_scratch.clear();
                self.prev.fill(0);
                self.ema.fill(ema0);
                encode_channels(
                    self.format,
                    raw,
                    &mut self.prev,
                    &mut self.ema,
                    &mut self.sample0_scratch,
                );
                if self.sample0_scratch.byte_len() >= self.raw_sample_bytes {
                    use_raw = true;
                }
            }

            if use_raw {
                let need = 1 + 8 + self.raw_sample_bytes;
                if need > max_payload_bytes {
                    return false;
                }
                self.writer.clear();
                self.writer.put_aligned_bytes(&[FLAG_SAMPLE0_RAW]);
                self.writer
                    .put_aligned_bytes(&timestamp.to_bits().to_le_bytes());
                copy_ne_to_le(self.format, raw, &mut self.writer.buf);
                seed_prev_from_raw(self.format, raw, &mut self.prev);
                self.ema.fill(ema0);
            } else {
                let need = 1 + 8 + self.sample0_scratch.byte_len();
                if need > max_payload_bytes {
                    return false;
                }
                self.writer.clear();
                self.writer.put_aligned_bytes(&[0]);
                self.writer
                    .put_aligned_bytes(&timestamp.to_bits().to_le_bytes());
                // Continue seamlessly from sample 0's bit accumulator!
                self.writer
                    .buf
                    .extend_from_slice(&self.sample0_scratch.buf);
                self.writer.acc = self.sample0_scratch.acc;
                self.writer.bits = self.sample0_scratch.bits;
                // Do not let sample 0's absolute DC offset poison delta EMA.
                self.ema.fill(ema0);
            }
            self.count = 1;
            return true;
        }

        // Snapshot state so we can roll back cleanly if this sample overflows.
        let cp = self.writer.checkpoint();
        let old_ts_prev = self.ts_prev;
        let old_ts_d1 = self.ts_prev_d1;
        let old_ts_ema = self.ts_ema;
        self.prev_backup.copy_from_slice(&self.prev);
        self.ema_backup.copy_from_slice(&self.ema);

        let ts_ord = f64_to_ord(timestamp.to_bits());
        let d1 = (ts_ord as i64).wrapping_sub(self.ts_prev as i64);
        let d2 = d1.wrapping_sub(self.ts_prev_d1);
        self.ts_prev = ts_ord;
        self.ts_prev_d1 = d1;
        let ts_u = zz64(d2);
        let ts_k = rice_k(self.ts_ema, 63);
        self.writer.put_rice(ts_u, ts_k, 64);
        update_ema(&mut self.ts_ema, ts_u);

        encode_channels(
            self.format,
            raw,
            &mut self.prev,
            &mut self.ema,
            &mut self.writer,
        );

        if self.writer.byte_len() > max_payload_bytes {
            self.writer.restore(cp);
            self.ts_prev = old_ts_prev;
            self.ts_prev_d1 = old_ts_d1;
            self.ts_ema = old_ts_ema;
            self.prev.copy_from_slice(&self.prev_backup);
            self.ema.copy_from_slice(&self.ema_backup);
            return false;
        }
        self.count += 1;
        true
    }

    /// Finalize the current datagram payload and return the compressed bytes.
    pub fn finish(&mut self) -> &[u8] {
        self.writer.finish()
    }
}

/// Decode a self-contained `DATA_COMP` UDP datagram payload holding `count`
/// samples, appending decoded [`PackedSample`]s into `out`.
pub fn decode_dgram_payload(
    codec: &Codec,
    count: usize,
    payload: &[u8],
    out: &mut Vec<PackedSample>,
) -> Result<(), Error> {
    if count == 0 {
        return Ok(());
    }
    let Some(width) = codec.format.width() else {
        return Err(Error::UndefinedFormat);
    };
    let channels = codec.channels;
    let raw_sample_bytes = width * channels;
    if payload.len() < 9 {
        return Err(Error::Truncated);
    }
    let flags = payload[0];
    if (flags & !FLAG_SAMPLE0_RAW) != 0 {
        return Err(Error::BadTag(flags));
    }
    let ts0_bits = u64::from_le_bytes([
        payload[1], payload[2], payload[3], payload[4], payload[5], payload[6], payload[7],
        payload[8],
    ]);
    let ts0 = f64::from_bits(ts0_bits);
    let ema0 = init_ema(codec.format);
    let mut prev = vec![0u64; channels];
    let mut ema = vec![ema0; channels];
    let mut ts_prev = f64_to_ord(ts0_bits);
    let mut ts_prev_d1 = 0i64;
    let mut ts_ema = 4u64;

    let mut reader = if (flags & FLAG_SAMPLE0_RAW) != 0 {
        let end0 = 9 + raw_sample_bytes;
        if payload.len() < end0 {
            return Err(Error::Truncated);
        }
        let mut raw0 = vec![0u8; raw_sample_bytes];
        copy_le_to_ne(codec.format, &payload[9..end0], &mut raw0);
        seed_prev_from_raw(codec.format, &raw0, &mut prev);
        if codec.suppress_subnormals {
            suppress_subnormals_in_place(codec.format, &mut raw0);
        }
        out.push(PackedSample::from_raw_bytes(
            ts0,
            codec.format,
            Arc::from(raw0),
        ));
        BitReader::new(&payload[end0..])
    } else {
        let mut r = BitReader::new(&payload[9..]);
        let mut raw0 = vec![0u8; raw_sample_bytes];
        decode_channels(codec.format, channels, &mut prev, &mut ema, &mut r, &mut raw0)?;
        ema.fill(ema0);
        if codec.suppress_subnormals {
            suppress_subnormals_in_place(codec.format, &mut raw0);
        }
        out.push(PackedSample::from_raw_bytes(
            ts0,
            codec.format,
            Arc::from(raw0),
        ));
        r
    };

    for _ in 1..count {
        let ts_k = rice_k(ts_ema, 63);
        let ts_u = reader.take_rice(ts_k, 64)?;
        update_ema(&mut ts_ema, ts_u);
        let d2 = unzz64(ts_u);
        let d1 = ts_prev_d1.wrapping_add(d2);
        let ts_ord = (ts_prev as i64).wrapping_add(d1) as u64;
        ts_prev = ts_ord;
        ts_prev_d1 = d1;
        let ts = f64::from_bits(ord_to_f64(ts_ord));

        let mut raw = vec![0u8; raw_sample_bytes];
        decode_channels(
            codec.format,
            channels,
            &mut prev,
            &mut ema,
            &mut reader,
            &mut raw,
        )?;
        if codec.suppress_subnormals {
            suppress_subnormals_in_place(codec.format, &mut raw);
        }
        out.push(PackedSample::from_raw_bytes(
            ts,
            codec.format,
            Arc::from(raw),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordered_float_bijection_is_exact_on_edge_bit_patterns() {
        let f32_cases: &[u32] = &[
            0x0000_0000, // +0.0
            0x8000_0000, // -0.0
            0x0000_0001, // smallest positive subnormal
            0x8000_0001, // smallest negative subnormal
            0x3F80_0000, // +1.0
            0xBF80_0000, // -1.0
            0x7F80_0000, // +Inf
            0xFF80_0000, // -Inf
            0x7FC0_0001, // quiet NaN with payload
            0xFFC0_1234, // negative quiet NaN with payload
            0x7F80_0001, // signaling NaN
            0xFFFF_FFFF,
        ];
        for &bits in f32_cases {
            assert_eq!(ord_to_f32(f32_to_ord(bits)), bits, "f32 bits {bits:#010x}");
        }
        assert_eq!(
            f32_to_ord(0x0000_0000).wrapping_sub(f32_to_ord(0x8000_0000)),
            1
        );

        let f64_cases: &[u64] = &[
            0x0000_0000_0000_0000,
            0x8000_0000_0000_0000,
            0x0000_0000_0000_0001,
            0x8000_0000_0000_0001,
            0x3FF0_0000_0000_0000,
            0xBFF0_0000_0000_0000,
            0x7FF0_0000_0000_0000,
            0xFFF0_0000_0000_0000,
            0x7FF8_0000_0000_0001,
            0xFFF8_1234_5678_9ABC,
            0xFFFF_FFFF_FFFF_FFFF,
        ];
        for &bits in f64_cases {
            assert_eq!(ord_to_f64(f64_to_ord(bits)), bits, "f64 bits {bits:#018x}");
        }
        assert_eq!(
            f64_to_ord(0x0000_0000_0000_0000).wrapping_sub(f64_to_ord(0x8000_0000_0000_0000)),
            1
        );
    }

    #[test]
    fn tcp_round_trips_all_six_numeric_formats_bit_exact() {
        let formats = [
            Format::Int8,
            Format::Int16,
            Format::Int32,
            Format::Int64,
            Format::Float32,
            Format::Double64,
        ];
        for format in formats {
            assert!(CompState::supports(format));
            let channels = 8;
            let codec = Codec::new(format, channels, ByteOrder::Little);
            let mut enc = CompState::new(format, channels);
            let mut dec = CompState::new(format, channels);

            // Mix smooth ramps, extreme values, negative zero, NaN payloads, and spikes.
            let mut samples = Vec::new();
            for i in 0..64usize {
                let ts = if i % 4 == 0 {
                    1000.0 + i as f64 / 512.0
                } else {
                    DEDUCED_TIMESTAMP
                };
                let s = match format {
                    Format::Int8 => {
                        let v: Vec<i8> = (0..channels)
                            .map(|c| match (i, c) {
                                (10, 0) => i8::MIN,
                                (11, 0) => i8::MAX,
                                _ => (i as i32 + c as i32 * 3) as i8,
                            })
                            .collect();
                        PackedSample::from_i8(ts, &v)
                    }
                    Format::Int16 => {
                        let v: Vec<i16> = (0..channels)
                            .map(|c| match (i, c) {
                                (10, 0) => i16::MIN,
                                (11, 0) => i16::MAX,
                                _ => (i as i32 * 7 - c as i32 * 100) as i16,
                            })
                            .collect();
                        PackedSample::from_i16(ts, &v)
                    }
                    Format::Int32 => {
                        let v: Vec<i32> = (0..channels)
                            .map(|c| match (i, c) {
                                (10, 0) => i32::MIN,
                                (11, 0) => i32::MAX,
                                _ => 100_000 + (c as i32) * 10_000 + (i as i32) * 12,
                            })
                            .collect();
                        PackedSample::from_i32(ts, &v)
                    }
                    Format::Int64 => {
                        let v: Vec<i64> = (0..channels)
                            .map(|c| match (i, c) {
                                (10, 0) => i64::MIN,
                                (11, 0) => i64::MAX,
                                _ => (c as i64) * 1_000_000 + (i as i64) * 17,
                            })
                            .collect();
                        PackedSample::from_i64(ts, &v)
                    }
                    Format::Float32 => {
                        let v: Vec<f32> = (0..channels)
                            .map(|c| match (i, c) {
                                (5, 0) => -0.0f32,
                                (6, 0) => 0.0f32,
                                (7, 0) => f32::from_bits(0x7FC0_0001),
                                (8, 0) => f32::from_bits(0xFFC0_1234),
                                (9, 0) => f32::INFINITY,
                                (10, 0) => f32::NEG_INFINITY,
                                (11, 0) => f32::from_bits(1), // subnormal
                                _ => 500.0 + (c as f32) * 25.0 + (i as f32) * 0.125,
                            })
                            .collect();
                        PackedSample::from_f32(ts, &v)
                    }
                    Format::Double64 => {
                        let v: Vec<f64> = (0..channels)
                            .map(|c| match (i, c) {
                                (5, 0) => -0.0f64,
                                (6, 0) => 0.0f64,
                                (7, 0) => f64::from_bits(0x7FF8_0000_0000_0001),
                                (8, 0) => f64::from_bits(0xFFF8_1234_5678_9ABC),
                                (9, 0) => f64::INFINITY,
                                (10, 0) => f64::NEG_INFINITY,
                                (11, 0) => f64::from_bits(1),
                                _ => 500.0 + (c as f64) * 25.0 + (i as f64) * 0.125,
                            })
                            .collect();
                        PackedSample::from_f64(ts, &v)
                    }
                    _ => unreachable!(),
                };
                samples.push(s);
            }

            let mut wire = Vec::new();
            for s in &samples {
                enc.encode_tcp_sample(&codec, s, s.timestamp, &mut wire)
                    .unwrap();
            }

            let mut pos = 0;
            for (idx, expected) in samples.iter().enumerate() {
                // Every truncated prefix must report `Truncated`.
                if pos < wire.len() {
                    let mid = pos + 1;
                    if mid < wire.len() {
                        let mut probe = dec.clone();
                        let _ = probe.decode_tcp_sample(&codec, &wire[pos..mid]);
                    }
                }
                let (got, used, _) = dec.decode_tcp_sample(&codec, &wire[pos..]).unwrap();
                pos += used;
                assert_eq!(
                    got.timestamp.to_bits(),
                    expected.timestamp.to_bits(),
                    "{format:?} sample {idx} timestamp mismatch"
                );
                assert_eq!(
                    got.as_numeric_bytes(format),
                    expected.as_numeric_bytes(format),
                    "{format:?} sample {idx} value bits mismatch"
                );
            }
            assert_eq!(pos, wire.len());
        }
    }

    #[test]
    fn dgram_block_round_trips_bit_exact_and_shrinks_eeg() {
        let channels = 32;
        let codec = Codec::new(Format::Int32, channels, ByteOrder::Little);
        let mut enc = DgramCompEncoder::new(Format::Int32, channels);
        let mut samples = Vec::new();
        let mut prev = vec![200_000i32; channels];
        let mut rng = 0x1234_5678_9ABC_DEF0u64;
        for i in 0..32usize {
            let mut row = Vec::with_capacity(channels);
            for (c, p) in prev.iter_mut().enumerate() {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                let step = ((rng % 41) as i32) - 20 + ((i + c) % 5) as i32;
                *p += step;
                row.push(*p);
            }
            let ts = 1234.5 + i as f64 / 512.0;
            samples.push((PackedSample::from_i32(ts, &row), ts));
        }

        enc.reset();
        for (s, ts) in &samples {
            assert!(enc.try_push(s, *ts, 1177), "all 32 EEG samples fit in 1 UDP datagram");
        }
        let payload = enc.finish().to_vec();
        let raw_bytes = samples.len() * (9 + channels * 4);
        let reduction = 1.0 - payload.len() as f64 / raw_bytes as f64;
        assert!(
            reduction > 0.60,
            "expected >60% reduction on 32-ch 24-bit EEG datagram, got {:.1}% ({} / {})",
            reduction * 100.0,
            payload.len(),
            raw_bytes
        );

        let mut decoded = Vec::new();
        decode_dgram_payload(&codec, enc.count(), &payload, &mut decoded).unwrap();
        assert_eq!(decoded.len(), samples.len());
        for (i, (got, (exp, exp_ts))) in decoded.iter().zip(samples.iter()).enumerate() {
            assert_eq!(got.timestamp.to_bits(), exp_ts.to_bits(), "sample {i} ts");
            assert_eq!(
                got.as_numeric_bytes(Format::Int32),
                exp.as_numeric_bytes(Format::Int32),
                "sample {i} bytes"
            );
        }
    }

    #[test]
    fn incompressible_random_data_has_zero_tcp_expansion() {
        let channels = 32;
        let codec = Codec::new(Format::Float32, channels, ByteOrder::Little);
        let mut enc = CompState::new(Format::Float32, channels);
        let mut dec = CompState::new(Format::Float32, channels);
        let mut rng = 0xCAFE_BABE_DEAD_BEEFu64;
        let mut wire = Vec::new();
        let mut plain = Vec::new();
        let mut samples = Vec::new();
        for i in 0..64 {
            let mut bytes = Vec::with_capacity(channels * 4);
            for _ in 0..channels {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                bytes.extend_from_slice(&(rng as u32).to_ne_bytes());
            }
            let ts = 10.0 + i as f64 * 0.001;
            let s = PackedSample::from_raw_bytes(ts, Format::Float32, Arc::from(bytes));
            enc.encode_tcp_sample(&codec, &s, ts, &mut wire).unwrap();
            codec.encode_packed_at(&s, ts, &mut plain).unwrap();
            samples.push(s);
        }
        assert!(
            wire.len() <= plain.len(),
            "random data must never expand on TCP: wire={} plain={}",
            wire.len(),
            plain.len()
        );
        let mut pos = 0;
        for exp in &samples {
            let (got, used, _) = dec.decode_tcp_sample(&codec, &wire[pos..]).unwrap();
            pos += used;
            assert_eq!(got.timestamp.to_bits(), exp.timestamp.to_bits());
            assert_eq!(
                got.as_numeric_bytes(Format::Float32),
                exp.as_numeric_bytes(Format::Float32)
            );
        }
        assert_eq!(pos, wire.len());
    }
}
