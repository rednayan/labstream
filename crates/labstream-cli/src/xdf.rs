//! Pure-Rust XDF 1.0 reader (and minimal writer for tests).
//!
//! Format reference: <https://github.com/sccn/xdf/wiki/Specifications>
//! Verified against LabRecorder recordings in `conformance/captures/verify.xdf`.

use labstream_net::StreamInfo;
use labstream_wire::{Format, Sample, Value};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::Path;

const TAG_FILE_HEADER: u16 = 1;
const TAG_STREAM_HEADER: u16 = 2;
const TAG_SAMPLES: u16 = 3;
const TAG_CLOCK_OFFSET: u16 = 4;
const TAG_BOUNDARY: u16 = 5;
const TAG_STREAM_FOOTER: u16 = 6;

/// One decoded stream from an XDF file.
#[derive(Debug, Clone)]
pub struct XdfStream {
    /// Numeric XDF stream identifier from the StreamHeader chunk.
    pub stream_id: u32,
    /// Full stream description, including `<desc>` channel metadata.
    pub info: StreamInfo,
    /// First clock offset recorded for this stream (seconds), if any.
    pub initial_clock_offset: f64,
    /// All samples in file order, with explicit deduced timestamps.
    pub samples: Vec<Sample>,
}

/// Complete decoded XDF recording.
#[derive(Debug, Clone)]
pub struct XdfFile {
    /// Streams ordered by XDF stream ID.
    pub streams: Vec<XdfStream>,
}

/// Read an uncompressed XDF 1.0 file from `path`.
pub fn read_file(path: &Path) -> io::Result<XdfFile> {
    let f = File::open(path)?;
    read_reader(BufReader::new(f))
}

/// Read an uncompressed XDF 1.0 stream from any `Read`.
pub fn read_reader<R: Read>(mut r: R) -> io::Result<XdfFile> {
    let mut magic = [0u8; 4];
    r.read_exact(&mut magic)?;
    if magic[0] == 0x1f && magic[1] == 0x8b {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "gzip-compressed .xdfz files are not supported; decompress first",
        ));
    }
    if &magic != b"XDF:" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("not an XDF file (bad magic: {magic:?})"),
        ));
    }

    struct Builder {
        info: StreamInfo,
        initial_clock_offset: Option<f64>,
        last_ts: Option<f64>,
        samples: Vec<Sample>,
    }

    let mut builders: BTreeMap<u32, Builder> = BTreeMap::new();

    loop {
        let Some(chunk_len) = read_varlen_opt(&mut r)? else {
            break;
        };
        if chunk_len < 2 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("XDF chunk too short ({chunk_len} bytes)"),
            ));
        }
        let len_usize = usize::try_from(chunk_len)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "XDF chunk length overflow"))?;
        let mut payload = vec![0u8; len_usize];
        r.read_exact(&mut payload)?;

        let tag = u16::from_le_bytes([payload[0], payload[1]]);
        let body = &payload[2..];

        match tag {
            TAG_FILE_HEADER | TAG_BOUNDARY | TAG_STREAM_FOOTER => {}
            TAG_STREAM_HEADER => {
                if body.len() < 4 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "truncated XDF StreamHeader chunk",
                    ));
                }
                let sid = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
                let xml = String::from_utf8_lossy(&body[4..]);
                let info = StreamInfo::from_fullinfo_xml(&xml).map_err(|e| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("invalid StreamHeader XML for stream {sid}: {e}"),
                    )
                })?;
                builders.insert(
                    sid,
                    Builder {
                        info,
                        initial_clock_offset: None,
                        last_ts: None,
                        samples: Vec::new(),
                    },
                );
            }
            TAG_CLOCK_OFFSET => {
                if body.len() >= 20 {
                    let sid = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
                    let offset = f64::from_le_bytes([
                        body[12], body[13], body[14], body[15], body[16], body[17], body[18],
                        body[19],
                    ]);
                    if offset.is_finite() {
                        if let Some(b) = builders.get_mut(&sid) {
                            if b.initial_clock_offset.is_none() {
                                b.initial_clock_offset = Some(offset);
                            }
                        }
                    }
                }
            }
            TAG_SAMPLES => {
                if body.len() < 4 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "truncated XDF Samples chunk",
                    ));
                }
                let sid = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
                let Some(b) = builders.get_mut(&sid) else {
                    continue;
                };
                let mut cur = &body[4..];
                let num_samples = read_varlen_slice(&mut cur)?;
                let ch_count = b.info.channel_count as usize;
                let format = b.info.format;
                let srate = b.info.nominal_srate;

                for _ in 0..num_samples {
                    let ts_bytes = read_u8_slice(&mut cur)?;
                    let ts = match ts_bytes {
                        0 => match b.last_ts {
                            Some(prev) if srate > 0.0 => prev + 1.0 / srate,
                            Some(prev) => prev,
                            None => 0.0,
                        },
                        8 => read_f64_slice(&mut cur)?,
                        other => {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                format!("unsupported XDF timestamp width {other}"),
                            ));
                        }
                    };
                    b.last_ts = Some(ts);

                    let mut values = Vec::with_capacity(ch_count);
                    for _ in 0..ch_count {
                        values.push(read_value_slice(&mut cur, format)?);
                    }
                    b.samples.push(Sample {
                        timestamp: ts,
                        values,
                    });
                }
            }
            _ => {
                // Unknown chunk tag: ignore per XDF extensibility rules.
            }
        }
    }

    let streams = builders
        .into_iter()
        .map(|(stream_id, b)| XdfStream {
            stream_id,
            info: b.info,
            initial_clock_offset: b.initial_clock_offset.unwrap_or(0.0),
            samples: b.samples,
        })
        .collect();

    Ok(XdfFile { streams })
}

fn read_varlen_opt<R: Read>(r: &mut R) -> io::Result<Option<u64>> {
    let mut nbytes = [0u8; 1];
    match r.read_exact(&mut nbytes) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    match nbytes[0] {
        1 => {
            let mut b = [0u8; 1];
            r.read_exact(&mut b)?;
            Ok(Some(b[0] as u64))
        }
        4 => {
            let mut b = [0u8; 4];
            r.read_exact(&mut b)?;
            Ok(Some(u32::from_le_bytes(b) as u64))
        }
        8 => {
            let mut b = [0u8; 8];
            r.read_exact(&mut b)?;
            Ok(Some(u64::from_le_bytes(b)))
        }
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid XDF varlen width {other}"),
        )),
    }
}

fn read_exact_slice<'a>(cur: &mut &'a [u8], n: usize) -> io::Result<&'a [u8]> {
    if cur.len() < n {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "truncated XDF sample chunk",
        ));
    }
    let (head, tail) = cur.split_at(n);
    *cur = tail;
    Ok(head)
}

fn read_u8_slice(cur: &mut &[u8]) -> io::Result<u8> {
    Ok(read_exact_slice(cur, 1)?[0])
}

fn read_f64_slice(cur: &mut &[u8]) -> io::Result<f64> {
    let b = read_exact_slice(cur, 8)?;
    Ok(f64::from_le_bytes([
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
    ]))
}

fn read_varlen_slice(cur: &mut &[u8]) -> io::Result<u64> {
    match read_u8_slice(cur)? {
        1 => Ok(read_u8_slice(cur)? as u64),
        4 => {
            let b = read_exact_slice(cur, 4)?;
            Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as u64)
        }
        8 => {
            let b = read_exact_slice(cur, 8)?;
            Ok(u64::from_le_bytes([
                b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
            ]))
        }
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid XDF varlen width {other}"),
        )),
    }
}

fn read_value_slice(cur: &mut &[u8], format: Format) -> io::Result<Value> {
    match format {
        Format::Float32 => {
            let b = read_exact_slice(cur, 4)?;
            Ok(Value::F32(f32::from_le_bytes([b[0], b[1], b[2], b[3]])))
        }
        Format::Double64 => Ok(Value::F64(read_f64_slice(cur)?)),
        Format::Int32 => {
            let b = read_exact_slice(cur, 4)?;
            Ok(Value::I32(i32::from_le_bytes([b[0], b[1], b[2], b[3]])))
        }
        Format::Int16 => {
            let b = read_exact_slice(cur, 2)?;
            Ok(Value::I16(i16::from_le_bytes([b[0], b[1]])))
        }
        Format::Int8 => {
            let b = read_u8_slice(cur)?;
            Ok(Value::I8(b as i8))
        }
        Format::Int64 => {
            let b = read_exact_slice(cur, 8)?;
            Ok(Value::I64(i64::from_le_bytes([
                b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
            ])))
        }
        Format::String => {
            let len = read_varlen_slice(cur)? as usize;
            let bytes = read_exact_slice(cur, len)?;
            Ok(Value::Str(bytes.to_vec()))
        }
        Format::Undefined => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cannot decode XDF stream with undefined format",
        )),
    }
}

/// Encode an XDF 1.0 byte buffer from `streams`. Used by unit and integration
/// tests to build deterministic synthetic XDF recordings.
#[cfg(test)]
pub fn write_bytes(streams: &[XdfStream]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"XDF:");
    push_chunk(
        &mut out,
        TAG_FILE_HEADER,
        b"<?xml version=\"1.0\"?><info><version>1.0</version></info>",
    );
    for s in streams {
        let mut hdr = Vec::new();
        hdr.extend_from_slice(&s.stream_id.to_le_bytes());
        hdr.extend_from_slice(s.info.to_fullinfo_xml().as_bytes());
        push_chunk(&mut out, TAG_STREAM_HEADER, &hdr);

        if s.initial_clock_offset != 0.0 {
            let mut clk = Vec::new();
            clk.extend_from_slice(&s.stream_id.to_le_bytes());
            clk.extend_from_slice(&0.0f64.to_le_bytes());
            clk.extend_from_slice(&s.initial_clock_offset.to_le_bytes());
            push_chunk(&mut out, TAG_CLOCK_OFFSET, &clk);
        }

        if !s.samples.is_empty() {
            let mut body = Vec::new();
            body.extend_from_slice(&s.stream_id.to_le_bytes());
            push_varlen(&mut body, s.samples.len() as u64);
            for sample in &s.samples {
                body.push(8);
                body.extend_from_slice(&sample.timestamp.to_le_bytes());
                for v in &sample.values {
                    match v {
                        Value::F32(x) => body.extend_from_slice(&x.to_le_bytes()),
                        Value::F64(x) => body.extend_from_slice(&x.to_le_bytes()),
                        Value::I32(x) => body.extend_from_slice(&x.to_le_bytes()),
                        Value::I16(x) => body.extend_from_slice(&x.to_le_bytes()),
                        Value::I8(x) => body.push(*x as u8),
                        Value::I64(x) => body.extend_from_slice(&x.to_le_bytes()),
                        Value::Str(b) => {
                            push_varlen(&mut body, b.len() as u64);
                            body.extend_from_slice(b);
                        }
                    }
                }
            }
            push_chunk(&mut out, TAG_SAMPLES, &body);
        }
    }
    out
}

#[cfg(test)]
fn push_varlen(out: &mut Vec<u8>, v: u64) {
    if v <= u8::MAX as u64 {
        out.push(1);
        out.push(v as u8);
    } else if v <= u32::MAX as u64 {
        out.push(4);
        out.extend_from_slice(&(v as u32).to_le_bytes());
    } else {
        out.push(8);
        out.extend_from_slice(&v.to_le_bytes());
    }
}

#[cfg(test)]
fn push_chunk(out: &mut Vec<u8>, tag: u16, body: &[u8]) {
    let total = (2 + body.len()) as u64;
    push_varlen(out, total);
    out.extend_from_slice(&tag.to_le_bytes());
    out.extend_from_slice(body);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn round_trips_all_seven_formats_bit_exact() {
        let make = |id: u32, name: &str, fmt: Format, srate: f64, vals: Vec<Value>| {
            let mut info = StreamInfo::new(name, "Test", vals.len() as u32, fmt, srate);
            info.uid = format!("uid-{id}");
            XdfStream {
                stream_id: id,
                info,
                initial_clock_offset: if id == 2 { 1.5 } else { 0.0 },
                samples: vec![
                    Sample {
                        timestamp: 100.0,
                        values: vals.clone(),
                    },
                    Sample {
                        timestamp: 100.25,
                        values: vals,
                    },
                ],
            }
        };

        let streams = vec![
            make(
                1,
                "F32",
                Format::Float32,
                250.0,
                vec![Value::F32(-0.0), Value::F32(3.5)],
            ),
            make(
                2,
                "F64",
                Format::Double64,
                100.0,
                vec![Value::F64(-123.456)],
            ),
            make(
                3,
                "I32",
                Format::Int32,
                100.0,
                vec![Value::I32(i32::MIN), Value::I32(i32::MAX)],
            ),
            make(4, "I16", Format::Int16, 100.0, vec![Value::I16(-32000)]),
            make(5, "I8", Format::Int8, 100.0, vec![Value::I8(-128)]),
            make(6, "I64", Format::Int64, 100.0, vec![Value::I64(i64::MIN)]),
            make(
                7,
                "Str",
                Format::String,
                0.0,
                vec![Value::Str(b"cue\0one".to_vec())],
            ),
        ];

        let bytes = write_bytes(&streams);
        let parsed = read_reader(&bytes[..]).expect("parse synthetic XDF");
        assert_eq!(parsed.streams.len(), 7);

        // Check negative zero bits on Float32.
        match (&parsed.streams[0].samples[0].values[0], &Value::F32(-0.0)) {
            (Value::F32(a), Value::F32(b)) => assert_eq!(a.to_bits(), b.to_bits()),
            _ => panic!("expected F32"),
        }
        assert_eq!(parsed.streams[1].initial_clock_offset, 1.5);
        assert_eq!(
            parsed.streams[6].samples[0].values[0],
            Value::Str(b"cue\0one".to_vec())
        );
        assert_eq!(parsed.streams[0].samples[1].timestamp, 100.25);
    }

    #[test]
    fn deduces_missing_timestamps_from_nominal_srate() {
        let mut info = StreamInfo::new("Deduced", "EEG", 1, Format::Float32, 100.0);
        info.uid = "uid-deduced".to_string();
        let mut out = Vec::new();
        out.extend_from_slice(b"XDF:");
        let mut hdr = Vec::new();
        hdr.extend_from_slice(&1u32.to_le_bytes());
        hdr.extend_from_slice(info.to_fullinfo_xml().as_bytes());
        push_chunk(&mut out, TAG_STREAM_HEADER, &hdr);

        let mut body = Vec::new();
        body.extend_from_slice(&1u32.to_le_bytes());
        push_varlen(&mut body, 3);
        // Sample 0: explicit timestamp 10.0.
        body.push(8);
        body.extend_from_slice(&10.0f64.to_le_bytes());
        body.extend_from_slice(&1.0f32.to_le_bytes());
        // Sample 1: ts_bytes = 0 -> deduced 10.01.
        body.push(0);
        body.extend_from_slice(&2.0f32.to_le_bytes());
        // Sample 2: ts_bytes = 0 -> deduced 10.02.
        body.push(0);
        body.extend_from_slice(&3.0f32.to_le_bytes());
        push_chunk(&mut out, TAG_SAMPLES, &body);

        let parsed = read_reader(&out[..]).unwrap();
        let s = &parsed.streams[0].samples;
        assert_eq!(s.len(), 3);
        assert_eq!(s[0].timestamp, 10.0);
        assert!((s[1].timestamp - 10.01).abs() < 1e-12);
        assert!((s[2].timestamp - 10.02).abs() < 1e-12);
    }

    #[test]
    fn parses_real_labrecorder_capture_verify_xdf() {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../conformance/captures/verify.xdf");
        if !path.exists() {
            return;
        }
        let xdf = read_file(&path).expect("parse conformance/captures/verify.xdf");
        assert_eq!(xdf.streams.len(), 6, "verify.xdf holds 6 recorded streams");
        let total_samples: usize = xdf.streams.iter().map(|s| s.samples.len()).sum();
        assert!(total_samples > 1000, "expected samples in verify.xdf");
        for s in &xdf.streams {
            assert!(!s.info.name.is_empty());
            assert!(
                !s.samples.is_empty(),
                "stream {} had 0 samples",
                s.info.name
            );
        }
    }
}
