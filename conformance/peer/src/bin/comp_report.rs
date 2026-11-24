//! Measure lossless `comp/1` byte reduction and codec throughput on recorded
//! XDF captures and realistic integer-ADC EEG datasets (`docs/roadmap.md`,
//! step 7, criteria 1, 2, and 4).

use labstream_proto::comp::{decode_dgram_payload, CompState, DgramCompEncoder};
use labstream_wire::{ByteOrder, Codec, Format, PackedSample};
use std::fs;
use std::path::Path;
use std::time::Instant;

fn read_varint(data: &[u8], pos: &mut usize) -> Option<usize> {
    let vlen = *data.get(*pos)? as usize;
    *pos += 1;
    let slice = data.get(*pos..*pos + vlen)?;
    *pos += vlen;
    let mut val = 0u64;
    for (i, &b) in slice.iter().enumerate() {
        val |= (b as u64) << (i * 8);
    }
    Some(val as usize)
}

type XdfLoaded = (String, usize, Vec<(PackedSample, f64)>);

/// Extract the first numeric `float32` stream (up to `max_samples`) from an XDF file.
fn load_xdf_first_f32(path: &Path, max_samples: usize) -> Option<XdfLoaded> {
    let data = fs::read(path).ok()?;
    if data.get(..4) != Some(b"XDF:") {
        return None;
    }
    let mut pos = 4usize;
    let mut target_sid: Option<u32> = None;
    let mut target_name = String::new();
    let mut target_ch = 0usize;
    let mut out = Vec::new();
    let mut last_ts = 0.0f64;

    while pos < data.len() && out.len() < max_samples {
        let clen = read_varint(&data, &mut pos)?;
        let end = pos.checked_add(clen)?;
        if end > data.len() || clen < 2 {
            break;
        }
        let tag = u16::from_le_bytes([data[pos], data[pos + 1]]);
        let mut p = pos + 2;
        if tag == 2 && p + 4 <= end && target_sid.is_none() {
            let sid = u32::from_le_bytes([data[p], data[p + 1], data[p + 2], data[p + 3]]);
            let xml = String::from_utf8_lossy(&data[p + 4..end]);
            if xml.contains("<channel_format>float32</channel_format>") {
                let name = extract_tag(&xml, "name").unwrap_or_else(|| format!("sid-{sid}"));
                let ch: usize = extract_tag(&xml, "channel_count")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                if ch > 0 {
                    target_sid = Some(sid);
                    target_name = name;
                    target_ch = ch;
                }
            }
        } else if tag == 3 && p + 4 <= end {
            let sid = u32::from_le_bytes([data[p], data[p + 1], data[p + 2], data[p + 3]]);
            p += 4;
            if Some(sid) == target_sid {
                if let Some(ns) = read_varint(&data, &mut p) {
                    let row_bytes = target_ch * 4;
                    for _ in 0..ns {
                        if out.len() >= max_samples || p >= end {
                            break;
                        }
                        let ts_flag = data[p];
                        p += 1;
                        let ts = if ts_flag == 8 && p + 8 <= end {
                            let t = f64::from_le_bytes([
                                data[p], data[p + 1], data[p + 2], data[p + 3],
                                data[p + 4], data[p + 5], data[p + 6], data[p + 7],
                            ]);
                            p += 8;
                            last_ts = t;
                            t
                        } else {
                            last_ts += 0.01;
                            last_ts
                        };
                        if p + row_bytes > end {
                            break;
                        }
                        let mut vals = Vec::with_capacity(target_ch);
                        for ch_bytes in data[p..p + row_bytes].chunks_exact(4) {
                            vals.push(f32::from_le_bytes([
                                ch_bytes[0],
                                ch_bytes[1],
                                ch_bytes[2],
                                ch_bytes[3],
                            ]));
                        }
                        p += row_bytes;
                        out.push((PackedSample::from_f32(ts, &vals), ts));
                    }
                }
            }
        }
        pos = end;
    }
    Some((target_name, target_ch, out))
}

fn extract_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let a = xml.find(&open)? + open.len();
    let b = xml[a..].find(&close)?;
    Some(xml[a..a + b].to_string())
}

/// Generate realistic scalp EEG from an integer ADC converter (1/f low-pass
/// brain spectrum + 10 Hz alpha + 50 Hz mains + thermal ADC noise + DC offset).
fn gen_adc_eeg(
    channels: usize,
    srate: f64,
    count: usize,
    format: Format,
) -> Vec<(PackedSample, f64)> {
    let mut rng = 0xD1B5_4A32_D192_ED03u64;
    let mut next_u = || -> f64 {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        // Approximate zero-mean Gaussian via sum of 3 uniforms in [-1, 1]
        let a = ((rng & 0xFFFF) as f64 / 32768.0) - 1.0;
        let b = (((rng >> 16) & 0xFFFF) as f64 / 32768.0) - 1.0;
        let c = (((rng >> 32) & 0xFFFF) as f64 / 32768.0) - 1.0;
        a + b + c
    };
    let dc: Vec<f64> = (0..channels).map(|_| next_u() * 4000.0).collect();
    let phase: Vec<f64> = (0..channels).map(|c| (c as f64) * 0.41).collect();
    let mut s1 = vec![0.0f64; channels];
    let mut s2 = vec![0.0f64; channels];
    let alpha_lp = (2.0 * std::f64::consts::PI * 30.0 / srate).min(0.45);
    let mut out = Vec::with_capacity(count);
    for t in 0..count {
        let ts = 1000.0 + (t as f64) / srate;
        let mains = 4.0 * (2.0 * std::f64::consts::PI * 50.0 * ts).sin();
        let mut uv_row = Vec::with_capacity(channels);
        for c in 0..channels {
            let drive = next_u() * 18.0;
            s1[c] += alpha_lp * (drive - s1[c]);
            s2[c] += alpha_lp * (s1[c] - s2[c]);
            let alpha = 12.0 * (2.0 * std::f64::consts::PI * 10.0 * ts + phase[c]).sin();
            let noise = next_u() * 0.15;
            uv_row.push(dc[c] + s2[c] * 2.5 + alpha + mains + noise);
        }
        let sample = match format {
            Format::Int32 => {
                // 24-bit ADS1299 LSB = 0.02235174 uV
                let vals: Vec<i32> = uv_row
                    .iter()
                    .map(|&uv| (uv / 0.0223517418).round() as i32)
                    .collect();
                PackedSample::from_i32(ts, &vals)
            }
            Format::Int16 => {
                // 16-bit clinical EEG ADC LSB = 0.1 uV (AC-coupled)
                let vals: Vec<i16> = uv_row
                    .iter()
                    .enumerate()
                    .map(|(c, &uv)| ((uv - dc[c]) / 0.1).round().clamp(-32768.0, 32767.0) as i16)
                    .collect();
                PackedSample::from_i16(ts, &vals)
            }
            Format::Float32 => {
                // 24-bit ADC counts scaled to microvolts in f32
                let vals: Vec<f32> = uv_row
                    .iter()
                    .map(|&uv| ((uv / 0.0223517418).round() * 0.0223517418) as f32)
                    .collect();
                PackedSample::from_f32(ts, &vals)
            }
            _ => unreachable!(),
        };
        out.push((sample, ts));
    }
    out
}

fn bench_dataset(label: &str, format: Format, channels: usize, samples: &[(PackedSample, f64)]) {
    let codec = Codec::new(format, channels, ByteOrder::Little);
    let mut enc = CompState::new(format, channels);
    let mut dec = CompState::new(format, channels);

    let mut plain_tcp = Vec::new();
    for (s, ts) in samples {
        codec.encode_packed_at(s, *ts, &mut plain_tcp).unwrap();
    }

    let t0 = Instant::now();
    let mut comp_tcp = Vec::with_capacity(plain_tcp.len());
    for (s, ts) in samples {
        enc.encode_tcp_sample(&codec, s, *ts, &mut comp_tcp).unwrap();
    }
    let enc_s = t0.elapsed().as_secs_f64();

    // Verify 100% bit-exact TCP decode.
    let mut pos = 0;
    for (idx, (exp, exp_ts)) in samples.iter().enumerate() {
        let (got, used, _) = dec.decode_tcp_sample(&codec, &comp_tcp[pos..]).unwrap();
        pos += used;
        assert_eq!(got.timestamp.to_bits(), exp_ts.to_bits(), "{label} tcp ts {idx}");
        assert_eq!(
            got.as_numeric_bytes(format),
            exp.as_numeric_bytes(format),
            "{label} tcp bits {idx}"
        );
    }
    assert_eq!(pos, comp_tcp.len());

    // Measure UDP DATA_COMP in 1200-byte datagrams (chunk-16 blocks).
    let mut udp_enc = DgramCompEncoder::new(format, channels);
    let mut udp_wire = 0usize;
    let mut udp_decoded = Vec::new();
    let room = labstream_proto::dgram::MAX_DATAGRAM - labstream_proto::dgram::DATA_HEADER;
    let mut idx = 0;
    while idx < samples.len() {
        udp_enc.reset();
        let start = idx;
        while idx < samples.len() && idx - start < 16 {
            if !udp_enc.try_push(&samples[idx].0, samples[idx].1, room) {
                break;
            }
            idx += 1;
        }
        let count = udp_enc.count();
        let payload = udp_enc.finish();
        udp_wire += payload.len();
        udp_decoded.clear();
        decode_dgram_payload(&codec, count, payload, &mut udp_decoded).unwrap();
        for (k, got) in udp_decoded.iter().enumerate() {
            let (exp, exp_ts) = &samples[start + k];
            assert_eq!(got.timestamp.to_bits(), exp_ts.to_bits());
            assert_eq!(got.as_numeric_bytes(format), exp.as_numeric_bytes(format));
        }
    }

    let raw_payload = samples.len() * channels * format.width().unwrap();
    let tcp_red = 100.0 * (1.0 - comp_tcp.len() as f64 / plain_tcp.len() as f64);
    let udp_red = 100.0 * (1.0 - udp_wire as f64 / plain_tcp.len() as f64);
    let mb_s = (raw_payload as f64 / (1024.0 * 1024.0)) / enc_s.max(1e-9);
    println!(
        "| {:<34} | {:<7} | {:>3} | {:>6} | {:>7} -> {:>7} ({:+5.1}%) | {:>7} ({:+5.1}%) | {:>6.1} MB/s |",
        label,
        format!("{format:?}"),
        channels,
        samples.len(),
        plain_tcp.len(),
        comp_tcp.len(),
        tcp_red,
        udp_wire,
        udp_red,
        mb_s
    );
}

fn main() {
    println!("| Dataset | Format | Ch | Samples | TCP wire B (reduction) | UDP wire B (reduction) | Enc speed |");
    println!("|---|---|---:|---:|---|---|---:|");

    let cap_dir = Path::new("conformance/captures");
    for file in ["verify.xdf", "ipv6.xdf", "soak.xdf"] {
        let p = cap_dir.join(file);
        if let Some((name, ch, rows)) = load_xdf_first_f32(&p, 10_000) {
            if !rows.is_empty() {
                bench_dataset(&format!("{file} ({name})"), Format::Float32, ch, &rows);
            }
        }
    }

    let ds_i32_512 = gen_adc_eeg(64, 512.0, 2560, Format::Int32);
    bench_dataset("24-bit ADC EEG 512 Hz", Format::Int32, 64, &ds_i32_512);

    let ds_i32_4k = gen_adc_eeg(64, 4000.0, 20_000, Format::Int32);
    bench_dataset("24-bit ADC EEG 4000 Hz", Format::Int32, 64, &ds_i32_4k);

    let ds_i16_512 = gen_adc_eeg(32, 512.0, 2560, Format::Int16);
    bench_dataset("16-bit ADC EEG 512 Hz", Format::Int16, 32, &ds_i16_512);

    let ds_f32_512 = gen_adc_eeg(64, 512.0, 2560, Format::Float32);
    bench_dataset("24-bit ADC scaled f32 512 Hz", Format::Float32, 64, &ds_f32_512);
}
