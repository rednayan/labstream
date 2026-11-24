//! The LSL sample codec for protocol 1.10.
//!
//! This crate holds no input and no output. It takes bytes and returns bytes.
//! Every rule here comes from `SPEC.md`, which cites the pinned oracle.
//!
//! The codec is the one part of LSL that a test can compare byte for byte. A
//! fixed sample with a fixed byte order always produces the same bytes.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use core::fmt;
use std::sync::Arc;

/// Tag that starts a sample with no timestamp bytes. SPEC.md 7.1.
pub const TAG_DEDUCED_TIMESTAMP: u8 = 1;
/// Tag that starts a sample with an `f64` timestamp. SPEC.md 7.1.
pub const TAG_TRANSMITTED_TIMESTAMP: u8 = 2;

/// The timestamp value that means "deduce this on the reading side".
///
/// liblsl uses `-1.0` (`include/lsl/common.h:48`).
pub const DEDUCED_TIMESTAMP: f64 = -1.0;

/// The nominal rate that means "this stream has no fixed rate".
///
/// liblsl uses `0.0` (`include/lsl/common.h:39`).
pub const IRREGULAR_RATE: f64 = 0.0;

/// A channel format. The values match `include/lsl/common.h:64-84`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Format {
    /// No format. A stream never carries this.
    Undefined = 0,
    /// 32-bit float.
    Float32 = 1,
    /// 64-bit float.
    Double64 = 2,
    /// A length-prefixed byte string.
    String = 3,
    /// 32-bit signed integer.
    Int32 = 4,
    /// 16-bit signed integer.
    Int16 = 5,
    /// 8-bit signed integer.
    Int8 = 6,
    /// 64-bit signed integer.
    Int64 = 7,
}

impl Format {
    /// Build a format from its wire value.
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => Format::Undefined,
            1 => Format::Float32,
            2 => Format::Double64,
            3 => Format::String,
            4 => Format::Int32,
            5 => Format::Int16,
            6 => Format::Int8,
            7 => Format::Int64,
            _ => return None,
        })
    }

    /// The width of one value in bytes. A string has no fixed width.
    pub fn width(self) -> Option<usize> {
        Some(match self {
            Format::Undefined | Format::String => return None,
            Format::Float32 | Format::Int32 => 4,
            Format::Double64 | Format::Int64 => 8,
            Format::Int16 => 2,
            Format::Int8 => 1,
        })
    }

    /// True when the format holds a floating point value.
    ///
    /// liblsl calls this table `format_float` (`src/sample.h:32`).
    pub fn is_float(self) -> bool {
        matches!(self, Format::Float32 | Format::Double64)
    }
}

/// One channel value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// A 32-bit float.
    F32(f32),
    /// A 64-bit float.
    F64(f64),
    /// A byte string. LSL does not require valid UTF-8.
    Str(Vec<u8>),
    /// A 32-bit signed integer.
    I32(i32),
    /// A 16-bit signed integer.
    I16(i16),
    /// An 8-bit signed integer.
    I8(i8),
    /// A 64-bit signed integer.
    I64(i64),
}

/// One sample: a timestamp and one value per channel.
#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    /// The timestamp. `DEDUCED_TIMESTAMP` means the reader derives it.
    pub timestamp: f64,
    /// One value per channel. Every value carries the same format.
    pub values: Vec<Value>,
}

/// Compact, reference-counted channel values for one sample.
///
/// For all fixed-width numeric formats, `Numeric` holds the native-endian
/// channel bytes contiguously in an `Arc<[u8]>` (e.g., 256 bytes for 64 `f32`
/// channels instead of 2048 bytes for `Vec<Value>`). Cloning a
/// [`PackedSample`] across multiple consumer queues and the `resume` history
/// copies only the `Arc` pointer.
#[derive(Debug, Clone, PartialEq)]
pub enum PackedValues {
    /// Contiguous native-endian channel bytes for a fixed-width format.
    Numeric {
        /// Channel format (never `String` or `Undefined`).
        format: Format,
        /// Contiguous native-endian bytes (`channels * format.width()`).
        bytes: Arc<[u8]>,
    },
    /// Channel byte strings for `Format::String`.
    Strings(Arc<[Vec<u8>]>),
}

/// One sample with reference-counted flat payload (`docs/roadmap.md`, step 8).
#[derive(Debug, Clone, PartialEq)]
pub struct PackedSample {
    /// The timestamp. `DEDUCED_TIMESTAMP` means the reader derives it.
    pub timestamp: f64,
    /// Shared payload for all channels.
    pub values: PackedValues,
}

impl PackedSample {
    /// Build a packed numeric sample from pre-packed native-endian bytes.
    pub fn from_raw_bytes(timestamp: f64, format: Format, bytes: Arc<[u8]>) -> Self {
        PackedSample {
            timestamp,
            values: PackedValues::Numeric { format, bytes },
        }
    }

    /// Build a packed `Float32` sample directly from a flat slice.
    pub fn from_f32(timestamp: f64, values: &[f32]) -> Self {
        let mut bytes = Vec::with_capacity(values.len() * 4);
        for &v in values {
            bytes.extend_from_slice(&v.to_ne_bytes());
        }
        Self::from_raw_bytes(timestamp, Format::Float32, Arc::from(bytes))
    }

    /// Build a packed `Double64` sample directly from a flat slice.
    pub fn from_f64(timestamp: f64, values: &[f64]) -> Self {
        let mut bytes = Vec::with_capacity(values.len() * 8);
        for &v in values {
            bytes.extend_from_slice(&v.to_ne_bytes());
        }
        Self::from_raw_bytes(timestamp, Format::Double64, Arc::from(bytes))
    }

    /// Build a packed `Int32` sample directly from a flat slice.
    pub fn from_i32(timestamp: f64, values: &[i32]) -> Self {
        let mut bytes = Vec::with_capacity(values.len() * 4);
        for &v in values {
            bytes.extend_from_slice(&v.to_ne_bytes());
        }
        Self::from_raw_bytes(timestamp, Format::Int32, Arc::from(bytes))
    }

    /// Build a packed `Int16` sample directly from a flat slice.
    pub fn from_i16(timestamp: f64, values: &[i16]) -> Self {
        let mut bytes = Vec::with_capacity(values.len() * 2);
        for &v in values {
            bytes.extend_from_slice(&v.to_ne_bytes());
        }
        Self::from_raw_bytes(timestamp, Format::Int16, Arc::from(bytes))
    }

    /// Build a packed `Int8` sample directly from a flat slice.
    pub fn from_i8(timestamp: f64, values: &[i8]) -> Self {
        let bytes: Vec<u8> = values.iter().map(|&v| v as u8).collect();
        Self::from_raw_bytes(timestamp, Format::Int8, Arc::from(bytes))
    }

    /// Build a packed `Int64` sample directly from a flat slice.
    pub fn from_i64(timestamp: f64, values: &[i64]) -> Self {
        let mut bytes = Vec::with_capacity(values.len() * 8);
        for &v in values {
            bytes.extend_from_slice(&v.to_ne_bytes());
        }
        Self::from_raw_bytes(timestamp, Format::Int64, Arc::from(bytes))
    }

    /// Build a packed `String` sample from channel byte vectors.
    pub fn from_strings(timestamp: f64, values: Vec<Vec<u8>>) -> Self {
        PackedSample {
            timestamp,
            values: PackedValues::Strings(Arc::from(values)),
        }
    }

    /// Convert a [`Sample`] into a [`PackedSample`].
    pub fn from_sample(s: &Sample) -> Self {
        let Some(first) = s.values.first() else {
            return PackedSample::from_strings(s.timestamp, Vec::new());
        };
        match first {
            Value::F32(_) => {
                let mut bytes = Vec::with_capacity(s.values.len() * 4);
                for v in &s.values {
                    if let Value::F32(x) = v {
                        bytes.extend_from_slice(&x.to_ne_bytes());
                    }
                }
                Self::from_raw_bytes(s.timestamp, Format::Float32, Arc::from(bytes))
            }
            Value::F64(_) => {
                let mut bytes = Vec::with_capacity(s.values.len() * 8);
                for v in &s.values {
                    if let Value::F64(x) = v {
                        bytes.extend_from_slice(&x.to_ne_bytes());
                    }
                }
                Self::from_raw_bytes(s.timestamp, Format::Double64, Arc::from(bytes))
            }
            Value::I32(_) => {
                let mut bytes = Vec::with_capacity(s.values.len() * 4);
                for v in &s.values {
                    if let Value::I32(x) = v {
                        bytes.extend_from_slice(&x.to_ne_bytes());
                    }
                }
                Self::from_raw_bytes(s.timestamp, Format::Int32, Arc::from(bytes))
            }
            Value::I16(_) => {
                let mut bytes = Vec::with_capacity(s.values.len() * 2);
                for v in &s.values {
                    if let Value::I16(x) = v {
                        bytes.extend_from_slice(&x.to_ne_bytes());
                    }
                }
                Self::from_raw_bytes(s.timestamp, Format::Int16, Arc::from(bytes))
            }
            Value::I8(_) => {
                let mut bytes = Vec::with_capacity(s.values.len());
                for v in &s.values {
                    if let Value::I8(x) = v {
                        bytes.push(*x as u8);
                    }
                }
                Self::from_raw_bytes(s.timestamp, Format::Int8, Arc::from(bytes))
            }
            Value::I64(_) => {
                let mut bytes = Vec::with_capacity(s.values.len() * 8);
                for v in &s.values {
                    if let Value::I64(x) = v {
                        bytes.extend_from_slice(&x.to_ne_bytes());
                    }
                }
                Self::from_raw_bytes(s.timestamp, Format::Int64, Arc::from(bytes))
            }
            Value::Str(_) => {
                let strings: Vec<Vec<u8>> = s
                    .values
                    .iter()
                    .map(|v| match v {
                        Value::Str(b) => b.clone(),
                        other => format!("{other:?}").into_bytes(),
                    })
                    .collect();
                Self::from_strings(s.timestamp, strings)
            }
        }
    }

    /// Return a copy of this sample with a different timestamp, sharing the
    /// underlying `Arc` payload with zero byte copies.
    pub fn with_timestamp(&self, timestamp: f64) -> Self {
        PackedSample {
            timestamp,
            values: self.values.clone(),
        }
    }

    /// Number of channels held in this sample.
    pub fn channel_count(&self) -> usize {
        match &self.values {
            PackedValues::Numeric { format, bytes } => {
                format.width().map_or(0, |w| bytes.len() / w)
            }
            PackedValues::Strings(list) => list.len(),
        }
    }

    /// Payload size in bytes, for bounded history accounting.
    pub fn payload_bytes(&self) -> usize {
        match &self.values {
            PackedValues::Numeric { bytes, .. } => bytes.len(),
            PackedValues::Strings(list) => list
                .iter()
                .map(|b| core::mem::size_of::<Vec<u8>>() + b.len())
                .sum(),
        }
    }

    /// Raw native-endian bytes when this sample holds numeric values in `expected` format.
    pub fn as_numeric_bytes(&self, expected: Format) -> Option<&[u8]> {
        match &self.values {
            PackedValues::Numeric { format, bytes } if *format == expected => Some(bytes),
            _ => None,
        }
    }

    /// Append channel values into `dst` as `f32`, converting if the format differs.
    pub fn push_into_f32(&self, dst: &mut Vec<f32>) {
        if let Some(bytes) = self.as_numeric_bytes(Format::Float32) {
            dst.reserve(bytes.len() / 4);
            for c in bytes.chunks_exact(4) {
                dst.push(f32::from_ne_bytes([c[0], c[1], c[2], c[3]]));
            }
            return;
        }
        for v in self.to_sample().values {
            dst.push(match v {
                Value::F32(x) => x,
                Value::F64(x) => x as f32,
                Value::I32(x) => x as f32,
                Value::I16(x) => x as f32,
                Value::I8(x) => x as f32,
                Value::I64(x) => x as f32,
                Value::Str(_) => 0.0,
            });
        }
    }

    /// Append channel values into `dst` as `f64`, converting if the format differs.
    pub fn push_into_f64(&self, dst: &mut Vec<f64>) {
        if let Some(bytes) = self.as_numeric_bytes(Format::Double64) {
            dst.reserve(bytes.len() / 8);
            for c in bytes.chunks_exact(8) {
                dst.push(f64::from_ne_bytes([
                    c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7],
                ]));
            }
            return;
        }
        for v in self.to_sample().values {
            dst.push(match v {
                Value::F32(x) => x as f64,
                Value::F64(x) => x,
                Value::I32(x) => x as f64,
                Value::I16(x) => x as f64,
                Value::I8(x) => x as f64,
                Value::I64(x) => x as f64,
                Value::Str(_) => 0.0,
            });
        }
    }

    /// Append channel values into `dst` as `i32`, converting if the format differs.
    pub fn push_into_i32(&self, dst: &mut Vec<i32>) {
        if let Some(bytes) = self.as_numeric_bytes(Format::Int32) {
            dst.reserve(bytes.len() / 4);
            for c in bytes.chunks_exact(4) {
                dst.push(i32::from_ne_bytes([c[0], c[1], c[2], c[3]]));
            }
            return;
        }
        for v in self.to_sample().values {
            dst.push(match v {
                Value::F32(x) => x as i32,
                Value::F64(x) => x as i32,
                Value::I32(x) => x,
                Value::I16(x) => x as i32,
                Value::I8(x) => x as i32,
                Value::I64(x) => x as i32,
                Value::Str(_) => 0,
            });
        }
    }

    /// Append channel values into `dst` as `i16`, converting if the format differs.
    pub fn push_into_i16(&self, dst: &mut Vec<i16>) {
        if let Some(bytes) = self.as_numeric_bytes(Format::Int16) {
            dst.reserve(bytes.len() / 2);
            for c in bytes.chunks_exact(2) {
                dst.push(i16::from_ne_bytes([c[0], c[1]]));
            }
            return;
        }
        for v in self.to_sample().values {
            dst.push(match v {
                Value::F32(x) => x as i16,
                Value::F64(x) => x as i16,
                Value::I32(x) => x as i16,
                Value::I16(x) => x,
                Value::I8(x) => x as i16,
                Value::I64(x) => x as i16,
                Value::Str(_) => 0,
            });
        }
    }

    /// Append channel values into `dst` as `i8`, converting if the format differs.
    pub fn push_into_i8(&self, dst: &mut Vec<i8>) {
        if let Some(bytes) = self.as_numeric_bytes(Format::Int8) {
            dst.reserve(bytes.len());
            for &b in bytes {
                dst.push(b as i8);
            }
            return;
        }
        for v in self.to_sample().values {
            dst.push(match v {
                Value::F32(x) => x as i8,
                Value::F64(x) => x as i8,
                Value::I32(x) => x as i8,
                Value::I16(x) => x as i8,
                Value::I8(x) => x,
                Value::I64(x) => x as i8,
                Value::Str(_) => 0,
            });
        }
    }

    /// Append channel values into `dst` as `i64`, converting if the format differs.
    pub fn push_into_i64(&self, dst: &mut Vec<i64>) {
        if let Some(bytes) = self.as_numeric_bytes(Format::Int64) {
            dst.reserve(bytes.len() / 8);
            for c in bytes.chunks_exact(8) {
                dst.push(i64::from_ne_bytes([
                    c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7],
                ]));
            }
            return;
        }
        for v in self.to_sample().values {
            dst.push(match v {
                Value::F32(x) => x as i64,
                Value::F64(x) => x as i64,
                Value::I32(x) => x as i64,
                Value::I16(x) => x as i64,
                Value::I8(x) => x as i64,
                Value::I64(x) => x,
                Value::Str(_) => 0,
            });
        }
    }

    /// Expand this packed sample back into a [`Sample`] holding `Vec<Value>`.
    pub fn to_sample(&self) -> Sample {
        let values = match &self.values {
            PackedValues::Numeric { format, bytes } => match format {
                Format::Float32 => bytes
                    .chunks_exact(4)
                    .map(|c| Value::F32(f32::from_ne_bytes([c[0], c[1], c[2], c[3]])))
                    .collect(),
                Format::Double64 => bytes
                    .chunks_exact(8)
                    .map(|c| {
                        Value::F64(f64::from_ne_bytes([
                            c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7],
                        ]))
                    })
                    .collect(),
                Format::Int32 => bytes
                    .chunks_exact(4)
                    .map(|c| Value::I32(i32::from_ne_bytes([c[0], c[1], c[2], c[3]])))
                    .collect(),
                Format::Int16 => bytes
                    .chunks_exact(2)
                    .map(|c| Value::I16(i16::from_ne_bytes([c[0], c[1]])))
                    .collect(),
                Format::Int8 => bytes.iter().map(|&b| Value::I8(b as i8)).collect(),
                Format::Int64 => bytes
                    .chunks_exact(8)
                    .map(|c| {
                        Value::I64(i64::from_ne_bytes([
                            c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7],
                        ]))
                    })
                    .collect(),
                Format::String | Format::Undefined => Vec::new(),
            },
            PackedValues::Strings(list) => list.iter().map(|b| Value::Str(b.clone())).collect(),
        };
        Sample {
            timestamp: self.timestamp,
            values,
        }
    }
}

impl From<&Sample> for PackedSample {
    fn from(s: &Sample) -> Self {
        PackedSample::from_sample(s)
    }
}

impl From<Sample> for PackedSample {
    fn from(s: Sample) -> Self {
        PackedSample::from_sample(&s)
    }
}

/// The byte order that a connection selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteOrder {
    /// Least significant byte first. liblsl writes `1234`.
    Little,
    /// Most significant byte first. liblsl writes `4321`.
    Big,
}

impl ByteOrder {
    /// The value that liblsl writes in the `Byte-Order` header. SPEC.md 4.6.
    pub fn wire_value(self) -> i32 {
        match self {
            ByteOrder::Little => 1234,
            ByteOrder::Big => 4321,
        }
    }

    /// Read a byte order from a `Byte-Order` header value.
    ///
    /// A value of `0` means the native order of the reader. That case exists
    /// for liblsl 1.13 and earlier (`src/data_receiver.cpp:231`).
    pub fn from_wire(v: i32) -> Option<Self> {
        match v {
            1234 => Some(ByteOrder::Little),
            4321 => Some(ByteOrder::Big),
            0 => Some(ByteOrder::native()),
            _ => None,
        }
    }

    /// The byte order of this machine.
    pub const fn native() -> Self {
        if cfg!(target_endian = "little") {
            ByteOrder::Little
        } else {
            ByteOrder::Big
        }
    }
}

/// Everything a codec call needs to know about the connection.
#[derive(Debug, Clone, Copy)]
pub struct Codec {
    /// The format of every channel in the stream.
    pub format: Format,
    /// The number of channels in one sample.
    pub channels: usize,
    /// The byte order that the connection selected.
    pub order: ByteOrder,
    /// True when the reader clears subnormal values. SPEC.md 7.4.
    pub suppress_subnormals: bool,
}

impl Codec {
    /// Build a codec for a stream.
    pub fn new(format: Format, channels: usize, order: ByteOrder) -> Self {
        Codec {
            format,
            channels,
            order,
            suppress_subnormals: false,
        }
    }

    /// Return a copy that clears subnormal values on read.
    pub fn with_suppress_subnormals(mut self, on: bool) -> Self {
        self.suppress_subnormals = on;
        self
    }

    /// True when a value needs a byte swap.
    ///
    /// A value of one byte never needs one (`src/sample.cpp:219`).
    fn swaps(&self) -> bool {
        self.order != ByteOrder::native() && self.format.width().map_or(true, |w| w > 1)
    }
}

/// A decode error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The buffer ended before the value did.
    Truncated,
    /// The tag byte was neither 1 nor 2.
    BadTag(u8),
    /// The width byte of a string length was not 1, 2, 4, or 8.
    ///
    /// liblsl reports `Stream contents corrupted (invalid varlen int).`
    /// (`src/sample.cpp:256`).
    BadLengthWidth(u8),
    /// A string length did not fit in this platform's `usize`.
    LengthTooLarge,
    /// The codec holds a format that a stream never carries.
    UndefinedFormat,
    /// A value did not match the format of the codec.
    FormatMismatch,
    /// The sample held a different number of values than the codec expects.
    ChannelCountMismatch {
        /// The count that the codec expects.
        expected: usize,
        /// The count that the sample holds.
        actual: usize,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Truncated => write!(f, "the buffer ended inside a value"),
            Error::BadTag(t) => write!(f, "the tag byte {t} is not 1 or 2"),
            Error::BadLengthWidth(w) => write!(f, "the length width {w} is not 1, 2, 4, or 8"),
            Error::LengthTooLarge => write!(f, "the string length does not fit in usize"),
            Error::UndefinedFormat => write!(f, "the format is undefined"),
            Error::FormatMismatch => write!(f, "a value does not match the codec format"),
            Error::ChannelCountMismatch { expected, actual } => {
                write!(
                    f,
                    "the sample holds {actual} values and the codec expects {expected}"
                )
            }
        }
    }
}

impl std::error::Error for Error {}

// ---------------------------------------------------------------------------
// encode
// ---------------------------------------------------------------------------

macro_rules! put {
    ($out:expr, $v:expr, $swap:expr) => {{
        let b = if $swap {
            $v.to_be_bytes()
        } else {
            $v.to_le_bytes()
        };
        $out.extend_from_slice(&b);
    }};
}

impl Codec {
    /// Write one sample to the end of `out`.
    ///
    /// The layout is the tag byte, then an optional timestamp, then one value
    /// per channel. SPEC.md 7.1.
    pub fn encode(&self, s: &Sample, out: &mut Vec<u8>) -> Result<(), Error> {
        if self.format == Format::Undefined {
            return Err(Error::UndefinedFormat);
        }
        if s.values.len() != self.channels {
            return Err(Error::ChannelCountMismatch {
                expected: self.channels,
                actual: s.values.len(),
            });
        }

        // A deduced timestamp writes the tag and nothing else.
        if s.timestamp == DEDUCED_TIMESTAMP {
            out.push(TAG_DEDUCED_TIMESTAMP);
        } else {
            out.push(TAG_TRANSMITTED_TIMESTAMP);
            // The timestamp always swaps with the connection, because it is
            // eight bytes wide. The format width does not apply to it.
            let swap = self.order != ByteOrder::native();
            put!(out, s.timestamp.to_bits(), swap);
        }

        let swap = self.swaps();
        for v in &s.values {
            match (self.format, v) {
                (Format::Float32, Value::F32(x)) => put!(out, x.to_bits(), swap),
                (Format::Double64, Value::F64(x)) => put!(out, x.to_bits(), swap),
                (Format::Int32, Value::I32(x)) => put!(out, x, swap),
                (Format::Int16, Value::I16(x)) => put!(out, x, swap),
                (Format::Int8, Value::I8(x)) => out.push(*x as u8),
                (Format::Int64, Value::I64(x)) => put!(out, x, swap),
                (Format::String, Value::Str(b)) => {
                    encode_string_len(b.len(), swap, out);
                    out.extend_from_slice(b);
                }
                _ => return Err(Error::FormatMismatch),
            }
        }
        Ok(())
    }

    /// Write one sample and return the bytes.
    pub fn encode_to_vec(&self, s: &Sample) -> Result<Vec<u8>, Error> {
        let mut v = Vec::new();
        self.encode(s, &mut v)?;
        Ok(v)
    }

    /// Write one [`PackedSample`] to the end of `out` using a single slice copy
    /// on native byte order.
    pub fn encode_packed(&self, s: &PackedSample, out: &mut Vec<u8>) -> Result<(), Error> {
        self.encode_packed_at(s, s.timestamp, out)
    }

    /// Write one [`PackedSample`] with an explicit timestamp override, without
    /// cloning the sample.
    pub fn encode_packed_at(
        &self,
        s: &PackedSample,
        timestamp: f64,
        out: &mut Vec<u8>,
    ) -> Result<(), Error> {
        if self.format == Format::Undefined {
            return Err(Error::UndefinedFormat);
        }
        let actual = s.channel_count();
        if actual != self.channels {
            return Err(Error::ChannelCountMismatch {
                expected: self.channels,
                actual,
            });
        }

        if timestamp == DEDUCED_TIMESTAMP {
            out.push(TAG_DEDUCED_TIMESTAMP);
        } else {
            out.push(TAG_TRANSMITTED_TIMESTAMP);
            let swap = self.order != ByteOrder::native();
            put!(out, timestamp.to_bits(), swap);
        }

        let swap = self.swaps();
        match (self.format, &s.values) {
            (
                fmt,
                PackedValues::Numeric {
                    format: sfmt,
                    bytes,
                },
            ) if fmt == *sfmt => {
                if !swap {
                    out.extend_from_slice(bytes);
                } else {
                    let w = fmt.width().unwrap_or(1);
                    for chunk in bytes.chunks_exact(w) {
                        for &b in chunk.iter().rev() {
                            out.push(b);
                        }
                    }
                }
                Ok(())
            }
            (Format::String, PackedValues::Strings(list)) => {
                for b in list.iter() {
                    encode_string_len(b.len(), swap, out);
                    out.extend_from_slice(b);
                }
                Ok(())
            }
            _ => Err(Error::FormatMismatch),
        }
    }
}

/// Write a string length as a width byte and then the length.
///
/// The encoder writes the widths 1, 4, and 8 only. It never writes 2. The
/// decoder still accepts 2. SPEC.md 7.3 explains why that matters.
fn encode_string_len(len: usize, swap: bool, out: &mut Vec<u8>) {
    if len <= 0xFF {
        out.push(1);
        out.push(len as u8);
    } else if len <= 0xFFFF_FFFF {
        out.push(4);
        put!(out, (len as u32), swap);
    } else {
        out.push(8);
        put!(out, (len as u64), swap);
    }
}

// ---------------------------------------------------------------------------
// decode
// ---------------------------------------------------------------------------

/// A cursor over a byte slice.
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    fn byte(&mut self) -> Result<u8, Error> {
        let b = *self.buf.get(self.pos).ok_or(Error::Truncated)?;
        self.pos += 1;
        Ok(b)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let end = self.pos.checked_add(n).ok_or(Error::Truncated)?;
        let s = self.buf.get(self.pos..end).ok_or(Error::Truncated)?;
        self.pos = end;
        Ok(s)
    }
}

macro_rules! get {
    ($r:expr, $t:ty, $swap:expr) => {{
        const N: usize = core::mem::size_of::<$t>();
        let s = $r.take(N)?;
        let mut a = [0u8; N];
        a.copy_from_slice(s);
        if $swap {
            <$t>::from_be_bytes(a)
        } else {
            <$t>::from_le_bytes(a)
        }
    }};
}

impl Codec {
    /// Read one sample from the start of `buf`.
    ///
    /// Returns the sample and the number of bytes that it used.
    pub fn decode(&self, buf: &[u8]) -> Result<(Sample, usize), Error> {
        if self.format == Format::Undefined {
            return Err(Error::UndefinedFormat);
        }
        let mut r = Reader::new(buf);

        let tag = r.byte()?;
        let timestamp = match tag {
            TAG_DEDUCED_TIMESTAMP => DEDUCED_TIMESTAMP,
            TAG_TRANSMITTED_TIMESTAMP => {
                let swap = self.order != ByteOrder::native();
                f64::from_bits(get!(r, u64, swap))
            }
            other => return Err(Error::BadTag(other)),
        };

        let swap = self.swaps();
        let mut values = Vec::with_capacity(self.channels);
        for _ in 0..self.channels {
            values.push(match self.format {
                Format::Float32 => {
                    let mut bits = get!(r, u32, swap);
                    // Suppression runs after the byte order conversion.
                    // SPEC.md 7.4, `src/sample.cpp:267-280`.
                    if self.suppress_subnormals && bits != 0 && (bits & 0x7fff_ffff) <= 0x007f_ffff
                    {
                        bits &= 0x8000_0000;
                    }
                    Value::F32(f32::from_bits(bits))
                }
                Format::Double64 => {
                    let mut bits = get!(r, u64, swap);
                    if self.suppress_subnormals
                        && bits != 0
                        && (bits & 0x7fff_ffff_ffff_ffff) <= 0x000f_ffff_ffff_ffff
                    {
                        bits &= 0x8000_0000_0000_0000;
                    }
                    Value::F64(f64::from_bits(bits))
                }
                Format::Int32 => Value::I32(get!(r, i32, swap)),
                Format::Int16 => Value::I16(get!(r, i16, swap)),
                Format::Int8 => Value::I8(r.byte()? as i8),
                Format::Int64 => Value::I64(get!(r, i64, swap)),
                Format::String => {
                    let len = decode_string_len(&mut r, swap)?;
                    Value::Str(r.take(len)?.to_vec())
                }
                Format::Undefined => return Err(Error::UndefinedFormat),
            });
        }

        Ok((Sample { timestamp, values }, r.pos))
    }

    /// Read one [`PackedSample`] directly from the start of `buf` without
    /// allocating a `Vec<Value>`.
    pub fn decode_packed(&self, buf: &[u8]) -> Result<(PackedSample, usize), Error> {
        if self.format == Format::Undefined {
            return Err(Error::UndefinedFormat);
        }
        let mut r = Reader::new(buf);

        let tag = r.byte()?;
        let timestamp = match tag {
            TAG_DEDUCED_TIMESTAMP => DEDUCED_TIMESTAMP,
            TAG_TRANSMITTED_TIMESTAMP => {
                let swap = self.order != ByteOrder::native();
                f64::from_bits(get!(r, u64, swap))
            }
            other => return Err(Error::BadTag(other)),
        };

        let swap = self.swaps();
        if let Some(width) = self.format.width() {
            let total = self.channels.checked_mul(width).ok_or(Error::Truncated)?;
            let raw = r.take(total)?;
            let bytes: Arc<[u8]> = if !swap
                && (!self.suppress_subnormals || !self.format.is_float())
            {
                Arc::from(raw)
            } else {
                let mut out = Vec::with_capacity(total);
                match self.format {
                    Format::Float32 => {
                        for c in raw.chunks_exact(4) {
                            let mut bits = if swap {
                                u32::from_be_bytes([c[0], c[1], c[2], c[3]])
                            } else {
                                u32::from_le_bytes([c[0], c[1], c[2], c[3]])
                            };
                            if self.suppress_subnormals
                                && bits != 0
                                && (bits & 0x7fff_ffff) <= 0x007f_ffff
                            {
                                bits &= 0x8000_0000;
                            }
                            out.extend_from_slice(&bits.to_ne_bytes());
                        }
                    }
                    Format::Double64 => {
                        for c in raw.chunks_exact(8) {
                            let mut bits = if swap {
                                u64::from_be_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]])
                            } else {
                                u64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]])
                            };
                            if self.suppress_subnormals
                                && bits != 0
                                && (bits & 0x7fff_ffff_ffff_ffff) <= 0x000f_ffff_ffff_ffff
                            {
                                bits &= 0x8000_0000_0000_0000;
                            }
                            out.extend_from_slice(&bits.to_ne_bytes());
                        }
                    }
                    _ => {
                        for chunk in raw.chunks_exact(width) {
                            if swap {
                                for &b in chunk.iter().rev() {
                                    out.push(b);
                                }
                            } else {
                                out.extend_from_slice(chunk);
                            }
                        }
                    }
                }
                Arc::from(out)
            };
            return Ok((
                PackedSample::from_raw_bytes(timestamp, self.format, bytes),
                r.pos,
            ));
        }

        let mut strings = Vec::with_capacity(self.channels);
        for _ in 0..self.channels {
            let len = decode_string_len(&mut r, swap)?;
            strings.push(r.take(len)?.to_vec());
        }
        Ok((PackedSample::from_strings(timestamp, strings), r.pos))
    }

    /// Read as many whole samples as `buf` holds.
    ///
    /// Returns the samples and the number of bytes that they used. Trailing
    /// bytes that do not form a whole sample stay unread.
    pub fn decode_all(&self, buf: &[u8]) -> Result<(Vec<Sample>, usize), Error> {
        let mut out = Vec::new();
        let mut off = 0;
        loop {
            match self.decode(&buf[off..]) {
                Ok((s, n)) => {
                    out.push(s);
                    off += n;
                    if off >= buf.len() {
                        break;
                    }
                }
                Err(Error::Truncated) => break,
                Err(e) => return Err(e),
            }
        }
        Ok((out, off))
    }
}

/// Read a string length: a width byte, then the length in that width.
///
/// The decoder accepts the width 2. No liblsl outlet writes it.
/// SPEC.md 7.3, `src/sample.cpp:251`.
fn decode_string_len(r: &mut Reader<'_>, swap: bool) -> Result<usize, Error> {
    let width = r.byte()?;
    let len: u64 = match width {
        1 => r.byte()? as u64,
        2 => get!(r, u16, swap) as u64,
        4 => get!(r, u32, swap) as u64,
        8 => get!(r, u64, swap),
        other => return Err(Error::BadLengthWidth(other)),
    };
    usize::try_from(len).map_err(|_| Error::LengthTooLarge)
}

// ---------------------------------------------------------------------------
// the test pattern
// ---------------------------------------------------------------------------

/// The timestamp that every test pattern sample carries.
///
/// `src/sample.cpp:366`.
pub const TEST_PATTERN_TIMESTAMP: f64 = 123456.789;

/// The two offsets that a feed sends, in order. SPEC.md 6.1.
pub const TEST_PATTERN_OFFSETS: [i64; 2] = [4, 2];

/// Build the test pattern sample that a feed sends.
///
/// A feed opens with two of these, at the offsets in `TEST_PATTERN_OFFSETS`.
/// An implementation that skips them loses the whole stream. SPEC.md 6.
pub fn test_pattern(format: Format, channels: usize, offset: i64) -> Sample {
    // The caller offset adds to a base that depends on the format.
    // `src/sample.cpp:368-400`.
    let base: i64 = match format {
        Format::Float32 => 0,
        Format::Double64 => 16_777_217,
        Format::Int32 => 65_537,
        Format::Int16 => 257,
        Format::Int8 => 1,
        Format::Int64 => 2_147_483_649,
        Format::String | Format::Undefined => 0,
    };
    let off = base + offset;

    let values = (0..channels)
        .map(|k| {
            let k = k as i64;
            // A string channel ignores the offset (`src/sample.cpp:375-380`).
            if format == Format::String {
                let v = (k + 10) * if k % 2 == 0 { 1 } else { -1 };
                return Value::Str(v.to_string().into_bytes());
            }
            // liblsl computes `k + offset` in an unsigned type, takes it
            // modulo the type maximum for an integer format, and then applies
            // the sign (`src/sample.cpp:356-362`).
            let raw = (k as u64).wrapping_add(off as u64);
            let sign = k % 2 == 0;
            match format {
                Format::Float32 => {
                    let v = raw as f32;
                    Value::F32(if sign { v } else { -v })
                }
                Format::Double64 => {
                    let v = raw as f64;
                    Value::F64(if sign { v } else { -v })
                }
                Format::Int32 => {
                    let v = (raw % i32::MAX as u64) as i32;
                    Value::I32(if sign { v } else { -v })
                }
                Format::Int16 => {
                    let v = (raw % i16::MAX as u64) as i16;
                    Value::I16(if sign { v } else { -v })
                }
                Format::Int8 => {
                    let v = (raw % i8::MAX as u64) as i8;
                    Value::I8(if sign { v } else { -v })
                }
                Format::Int64 => {
                    let v = (raw % i64::MAX as u64) as i64;
                    Value::I64(if sign { v } else { -v })
                }
                Format::String | Format::Undefined => unreachable!(),
            }
        })
        .collect();

    Sample {
        timestamp: TEST_PATTERN_TIMESTAMP,
        values,
    }
}

// ---------------------------------------------------------------------------
// timestamp reconstruction
// ---------------------------------------------------------------------------

/// Rebuilds a deduced timestamp from the position in the stream.
///
/// The accumulator runs for the life of the connection. Every deduced
/// timestamp depends on every sample before it. SPEC.md 8.1.
#[derive(Debug, Clone)]
pub struct TimestampDeducer {
    last: f64,
    srate: f64,
}

impl TimestampDeducer {
    /// Build a deducer for a stream with the given nominal rate.
    ///
    /// liblsl starts the accumulator at `0.0` (`src/data_receiver.cpp:310`).
    pub fn new(srate: f64) -> Self {
        TimestampDeducer { last: 0.0, srate }
    }

    /// Replace a deduced timestamp with a real one.
    pub fn apply(&mut self, timestamp: f64) -> f64 {
        let mut t = timestamp;
        if t == DEDUCED_TIMESTAMP {
            t = self.last;
            if self.srate != IRREGULAR_RATE {
                t += 1.0 / self.srate;
            }
        }
        self.last = t;
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt(c: &Codec, s: &Sample) -> Sample {
        let b = c.encode_to_vec(s).unwrap();
        let (got, n) = c.decode(&b).unwrap();
        assert_eq!(n, b.len(), "decode did not use every byte");
        got
    }

    #[test]
    fn deduced_tag_is_one_byte() {
        let c = Codec::new(Format::Float32, 2, ByteOrder::Little);
        let s = Sample {
            timestamp: DEDUCED_TIMESTAMP,
            values: vec![Value::F32(1.0), Value::F32(2.0)],
        };
        let b = c.encode_to_vec(&s).unwrap();
        assert_eq!(b[0], TAG_DEDUCED_TIMESTAMP);
        assert_eq!(b.len(), 1 + 8);
        assert_eq!(rt(&c, &s), s);
    }

    #[test]
    fn transmitted_tag_carries_eight_bytes() {
        let c = Codec::new(Format::Int8, 1, ByteOrder::Little);
        let s = Sample {
            timestamp: 1.5,
            values: vec![Value::I8(-3)],
        };
        let b = c.encode_to_vec(&s).unwrap();
        assert_eq!(b[0], TAG_TRANSMITTED_TIMESTAMP);
        assert_eq!(b.len(), 1 + 8 + 1);
        assert_eq!(rt(&c, &s), s);
    }

    #[test]
    fn string_width_boundaries() {
        let c = Codec::new(Format::String, 1, ByteOrder::Little);
        for (len, want_width) in [(0usize, 1u8), (255, 1), (256, 4), (257, 4)] {
            let s = Sample {
                timestamp: 0.0,
                values: vec![Value::Str(vec![b'x'; len])],
            };
            let b = c.encode_to_vec(&s).unwrap();
            assert_eq!(b[9], want_width, "length {len} picked the wrong width");
            assert_eq!(rt(&c, &s), s);
        }
    }

    #[test]
    fn decoder_accepts_width_two_that_no_encoder_writes() {
        // SPEC.md 7.3. liblsl never writes this, and it always reads it.
        let c = Codec::new(Format::String, 1, ByteOrder::Little);
        let mut b = vec![TAG_TRANSMITTED_TIMESTAMP];
        b.extend_from_slice(&0.0f64.to_le_bytes());
        b.push(2);
        b.extend_from_slice(&3u16.to_le_bytes());
        b.extend_from_slice(b"abc");
        let (s, n) = c.decode(&b).unwrap();
        assert_eq!(n, b.len());
        assert_eq!(s.values, vec![Value::Str(b"abc".to_vec())]);
    }

    #[test]
    fn bad_length_width_is_an_error() {
        let c = Codec::new(Format::String, 1, ByteOrder::Little);
        let mut b = vec![TAG_TRANSMITTED_TIMESTAMP];
        b.extend_from_slice(&0.0f64.to_le_bytes());
        b.push(3);
        assert_eq!(c.decode(&b), Err(Error::BadLengthWidth(3)));
    }

    #[test]
    fn bad_tag_is_an_error() {
        let c = Codec::new(Format::Int8, 1, ByteOrder::Little);
        assert_eq!(c.decode(&[7, 0]), Err(Error::BadTag(7)));
    }

    #[test]
    fn truncated_input_is_an_error() {
        let c = Codec::new(Format::Double64, 4, ByteOrder::Little);
        let b = vec![TAG_DEDUCED_TIMESTAMP, 0, 0, 0];
        assert_eq!(c.decode(&b), Err(Error::Truncated));
    }

    #[test]
    fn nan_and_infinity_survive_a_round_trip() {
        let c = Codec::new(Format::Double64, 4, ByteOrder::Little);
        let s = Sample {
            timestamp: 0.0,
            values: vec![
                Value::F64(f64::INFINITY),
                Value::F64(f64::NEG_INFINITY),
                Value::F64(-0.0),
                Value::F64(f64::MIN_POSITIVE / 2.0),
            ],
        };
        let got = rt(&c, &s);
        assert_eq!(got, s);
        // -0.0 == 0.0 compares true, so compare the bits.
        if let (Value::F64(a), Value::F64(b)) = (&got.values[2], &s.values[2]) {
            assert_eq!(a.to_bits(), b.to_bits());
        }
    }

    #[test]
    fn big_endian_differs_from_little_endian() {
        let le = Codec::new(Format::Int32, 1, ByteOrder::Little);
        let be = Codec::new(Format::Int32, 1, ByteOrder::Big);
        let s = Sample {
            timestamp: 0.0,
            values: vec![Value::I32(1)],
        };
        assert_ne!(le.encode_to_vec(&s).unwrap(), be.encode_to_vec(&s).unwrap());
        assert_eq!(be.decode(&be.encode_to_vec(&s).unwrap()).unwrap().0, s);
    }

    #[test]
    fn int8_never_swaps() {
        let le = Codec::new(Format::Int8, 3, ByteOrder::Little);
        let be = Codec::new(Format::Int8, 3, ByteOrder::Big);
        let s = Sample {
            timestamp: DEDUCED_TIMESTAMP,
            values: vec![Value::I8(1), Value::I8(-2), Value::I8(3)],
        };
        assert_eq!(le.encode_to_vec(&s).unwrap(), be.encode_to_vec(&s).unwrap());
    }

    #[test]
    fn subnormal_suppression_keeps_the_sign() {
        let c = Codec::new(Format::Float32, 2, ByteOrder::Little).with_suppress_subnormals(true);
        let plain = Codec::new(Format::Float32, 2, ByteOrder::Little);
        let s = Sample {
            timestamp: 0.0,
            values: vec![
                Value::F32(f32::from_bits(0x0000_0001)),
                Value::F32(f32::from_bits(0x8000_0001)),
            ],
        };
        let b = plain.encode_to_vec(&s).unwrap();
        let (got, _) = c.decode(&b).unwrap();
        assert_eq!(got.values[0], Value::F32(0.0));
        match got.values[1] {
            Value::F32(v) => assert_eq!(v.to_bits(), 0x8000_0000),
            _ => panic!("wrong format"),
        }
    }

    #[test]
    fn channel_count_mismatch_is_an_error() {
        let c = Codec::new(Format::Int16, 4, ByteOrder::Little);
        let s = Sample {
            timestamp: 0.0,
            values: vec![Value::I16(1)],
        };
        assert_eq!(
            c.encode(&s, &mut Vec::new()),
            Err(Error::ChannelCountMismatch {
                expected: 4,
                actual: 1
            })
        );
    }

    #[test]
    fn deducer_adds_one_period() {
        let mut d = TimestampDeducer::new(100.0);
        assert!((d.apply(DEDUCED_TIMESTAMP) - 0.01).abs() < 1e-12);
        assert!((d.apply(DEDUCED_TIMESTAMP) - 0.02).abs() < 1e-12);
        assert_eq!(d.apply(5.0), 5.0);
        assert!((d.apply(DEDUCED_TIMESTAMP) - 5.01).abs() < 1e-12);
    }

    #[test]
    fn deducer_holds_the_value_for_an_irregular_rate() {
        let mut d = TimestampDeducer::new(IRREGULAR_RATE);
        assert_eq!(d.apply(DEDUCED_TIMESTAMP), 0.0);
        assert_eq!(d.apply(7.0), 7.0);
        assert_eq!(d.apply(DEDUCED_TIMESTAMP), 7.0);
    }

    #[test]
    fn test_pattern_matches_the_captured_stream() {
        // From captures/feed.json. SPEC.md 6.4.
        let s = test_pattern(Format::Float32, 8, 4);
        assert_eq!(s.timestamp, TEST_PATTERN_TIMESTAMP);
        let want: Vec<f32> = vec![4.0, -5.0, 6.0, -7.0, 8.0, -9.0, 10.0, -11.0];
        let got: Vec<f32> = s
            .values
            .iter()
            .map(|v| match v {
                Value::F32(x) => *x,
                _ => panic!("wrong format"),
            })
            .collect();
        assert_eq!(got, want);
    }

    #[test]
    fn packed_codec_matches_sample_codec_across_formats_and_endianness() {
        let cases = [
            (Format::Float32, test_pattern(Format::Float32, 8, 4)),
            (Format::Double64, test_pattern(Format::Double64, 4, 2)),
            (Format::Int32, test_pattern(Format::Int32, 6, 4)),
            (Format::Int16, test_pattern(Format::Int16, 6, 2)),
            (Format::Int8, test_pattern(Format::Int8, 6, 4)),
            (Format::Int64, test_pattern(Format::Int64, 4, 2)),
            (Format::String, test_pattern(Format::String, 3, 4)),
        ];
        for order in [ByteOrder::Little, ByteOrder::Big] {
            for suppress in [false, true] {
                for (fmt, sample) in &cases {
                    let codec = Codec::new(*fmt, sample.values.len(), order)
                        .with_suppress_subnormals(suppress);
                    let packed = PackedSample::from_sample(sample);
                    let mut wire_old = Vec::new();
                    let mut wire_new = Vec::new();
                    codec.encode(sample, &mut wire_old).unwrap();
                    codec.encode_packed(&packed, &mut wire_new).unwrap();
                    assert_eq!(wire_new, wire_old, "encode {fmt:?} {order:?}");

                    let (dec_old, used_old) = codec.decode(&wire_old).unwrap();
                    let (dec_new, used_new) = codec.decode_packed(&wire_old).unwrap();
                    assert_eq!(used_new, used_old);
                    assert_eq!(dec_new.to_sample(), dec_old, "decode {fmt:?} {order:?}");
                }
            }
        }
    }
}
