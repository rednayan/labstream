//! Channel projection and windowed-sinc FIR decimation for `select/1`
//! (`docs/roadmap.md`, step 6).
//!
//! # Decimation filter specification
//!
//! For an integer decimation factor `D >= 2`, the outlet applies a symmetric
//! Type I linear-phase Hamming-windowed sinc low-pass FIR filter to each
//! selected numeric channel before keeping one sample every `D` input samples:
//!
//! - Tap count: `N = (4 * D + 1).min(257)` (always odd, center `M = (N - 1) / 2`).
//! - Normalized cutoff frequency: `fc = 0.45 / D` (cycles per input sample).
//! - Unnormalized impulse response for `k = 0..N`:
//!   - `h_raw[M] = 2 * fc`
//!   - `h_raw[k] = sin(2 * pi * fc * (k - M)) / (pi * (k - M))` for `k != M`
//! - Hamming window: `w[k] = 0.54 - 0.46 * cos(2 * pi * k / (N - 1))`.
//! - Normalized taps: `h[k] = (h_raw[k] * w[k]) / sum_j(h_raw[j] * w[j])`.
//!
//! The DC gain is exactly `1.0`. Each decimated output sample carries the
//! timestamp of the center tap (`t_last - M / nominal_srate`), removing the
//! constant group delay of the linear-phase filter.

use crate::features::SelectParam;
use labstream_wire::{Format, PackedSample, PackedValues};
use std::collections::VecDeque;
use std::f64::consts::PI;
use std::sync::Arc;

/// Maximum FIR tap count so very large decimation factors stay fast.
const MAX_TAPS: usize = 257;

/// Compute the normalized Hamming-windowed sinc low-pass FIR taps for
/// integer decimation factor `decimate`.
///
/// For `decimate <= 1`, returns `[1.0]`.
pub fn fir_taps(decimate: u32) -> Vec<f64> {
    if decimate <= 1 {
        return vec![1.0];
    }
    let mut n = (4 * decimate as usize + 1).min(MAX_TAPS);
    if n % 2 == 0 {
        n -= 1;
    }
    let m = (n - 1) / 2;
    let fc = 0.45 / decimate as f64;
    let mut taps = Vec::with_capacity(n);
    let mut sum = 0.0;
    for k in 0..n {
        let x = k as f64 - m as f64;
        let sinc = if k == m {
            2.0 * fc
        } else {
            (2.0 * PI * fc * x).sin() / (PI * x)
        };
        let window = 0.54 - 0.46 * (2.0 * PI * k as f64 / (n - 1) as f64).cos();
        let v = sinc * window;
        taps.push(v);
        sum += v;
    }
    for t in &mut taps {
        *t /= sum;
    }
    taps
}

/// Project one [`PackedSample`] to the ordered channel indices in `channels`,
/// keeping the exact bits of every selected channel across all seven formats.
///
/// When `channels` is empty, returns a cheap `Arc` clone of `sample`.
pub fn project_packed(sample: &PackedSample, channels: &[u16]) -> PackedSample {
    if channels.is_empty() {
        return sample.clone();
    }
    match &sample.values {
        PackedValues::Numeric { format, bytes } => {
            let width = format.width().unwrap_or(1);
            let mut out = Vec::with_capacity(channels.len() * width);
            for &ch in channels {
                let start = (ch as usize) * width;
                let end = start + width;
                if end <= bytes.len() {
                    out.extend_from_slice(&bytes[start..end]);
                } else {
                    out.resize(out.len() + width, 0);
                }
            }
            PackedSample::from_raw_bytes(sample.timestamp, *format, Arc::from(out))
        }
        PackedValues::Strings(list) => {
            let mut out = Vec::with_capacity(channels.len());
            for &ch in channels {
                if let Some(s) = list.get(ch as usize) {
                    out.push(s.clone());
                } else {
                    out.push(Vec::new());
                }
            }
            PackedSample::from_strings(sample.timestamp, out)
        }
    }
}

/// Per-consumer channel selector and streaming FIR decimator.
#[derive(Debug, Clone)]
pub struct StreamSelector {
    param: SelectParam,
    format: Format,
    out_channels: usize,
    taps: Vec<f64>,
    half_delay: usize,
    /// Circular history of projected channel values (each entry has `out_channels` f64s).
    history: VecDeque<(f64, Vec<f64>)>,
    /// Input samples seen since the last emitted decimated sample.
    phase: u32,
    /// Reusable scratch buffer for numeric byte output.
    out_bytes: Vec<u8>,
}

impl StreamSelector {
    /// Build a selector for a source stream of `format` and `source_channels`.
    pub fn new(param: SelectParam, format: Format, source_channels: usize) -> Self {
        let out_channels = param.output_channels(source_channels);
        let taps = fir_taps(param.decimate);
        let half_delay = (taps.len() - 1) / 2;
        StreamSelector {
            param,
            format,
            out_channels,
            taps,
            half_delay,
            history: VecDeque::new(),
            phase: 0,
            out_bytes: Vec::new(),
        }
    }

    /// The validated selection parameters.
    pub fn param(&self) -> &SelectParam {
        &self.param
    }

    /// Feed one full-rate sample with its full deduced `timestamp`.
    ///
    /// Returns `Some(PackedSample)` whenever an output sample is ready:
    /// - For `decimate == 1`, always returns `Some` (bit-exact projection).
    /// - For `decimate > 1`, evaluates the Hamming-windowed sinc FIR filter
    ///   every `decimate` input samples and emits a sample timestamped at the
    ///   filter center tap.
    pub fn push(&mut self, sample: &PackedSample, full_timestamp: f64) -> Option<PackedSample> {
        if self.param.decimate <= 1 {
            return Some(project_packed(sample, &self.param.channels));
        }

        // Convert selected channels into f64 for filtering.
        let vals = extract_f64_channels(sample, &self.param.channels, self.out_channels);
        let tap_len = self.taps.len();
        if self.history.is_empty() {
            // Pre-fill the left edge with the first sample so there is no
            // artificial zero transient at startup and output starts immediately.
            for _ in 0..self.half_delay {
                self.history.push_back((full_timestamp, vals.clone()));
            }
        }
        self.history.push_back((full_timestamp, vals));
        while self.history.len() > tap_len {
            self.history.pop_front();
        }
        if self.history.len() < tap_len {
            return None;
        }

        let emit = self.phase == 0;
        self.phase += 1;
        if self.phase >= self.param.decimate {
            self.phase = 0;
        }
        if !emit {
            return None;
        }

        // Evaluate linear-phase FIR across all selected channels.
        let center_ts = self.history[self.half_delay].0;
        let mut acc = vec![0.0f64; self.out_channels];
        for (h, (_, row)) in self.taps.iter().zip(self.history.iter()) {
            let h = *h;
            for (a, &x) in acc.iter_mut().zip(row.iter()) {
                *a += h * x;
            }
        }
        Some(pack_f64_channels(
            self.format,
            center_ts,
            &acc,
            &mut self.out_bytes,
        ))
    }
}

fn extract_f64_channels(sample: &PackedSample, channels: &[u16], out_ch: usize) -> Vec<f64> {
    let mut vals = Vec::with_capacity(out_ch);
    if let PackedValues::Numeric { format, bytes } = &sample.values {
        let width = format.width().unwrap_or(1);
        let read_at = |ch: usize| -> f64 {
            let off = ch * width;
            if off + width > bytes.len() {
                return 0.0;
            }
            let c = &bytes[off..off + width];
            match format {
                Format::Float32 => f32::from_ne_bytes([c[0], c[1], c[2], c[3]]) as f64,
                Format::Double64 => {
                    f64::from_ne_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]])
                }
                Format::Int32 => i32::from_ne_bytes([c[0], c[1], c[2], c[3]]) as f64,
                Format::Int16 => i16::from_ne_bytes([c[0], c[1]]) as f64,
                Format::Int8 => (c[0] as i8) as f64,
                Format::Int64 => {
                    i64::from_ne_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]) as f64
                }
                Format::String | Format::Undefined => 0.0,
            }
        };
        if channels.is_empty() {
            for ch in 0..out_ch {
                vals.push(read_at(ch));
            }
        } else {
            for &ch in channels {
                vals.push(read_at(ch as usize));
            }
        }
    }
    vals
}

fn pack_f64_channels(
    format: Format,
    timestamp: f64,
    vals: &[f64],
    scratch: &mut Vec<u8>,
) -> PackedSample {
    scratch.clear();
    match format {
        Format::Float32 => {
            for &v in vals {
                scratch.extend_from_slice(&(v as f32).to_ne_bytes());
            }
        }
        Format::Double64 => {
            for &v in vals {
                scratch.extend_from_slice(&v.to_ne_bytes());
            }
        }
        Format::Int32 => {
            for &v in vals {
                let q = v.round().clamp(i32::MIN as f64, i32::MAX as f64) as i32;
                scratch.extend_from_slice(&q.to_ne_bytes());
            }
        }
        Format::Int16 => {
            for &v in vals {
                let q = v.round().clamp(i16::MIN as f64, i16::MAX as f64) as i16;
                scratch.extend_from_slice(&q.to_ne_bytes());
            }
        }
        Format::Int8 => {
            for &v in vals {
                let q = v.round().clamp(i8::MIN as f64, i8::MAX as f64) as i8;
                scratch.push(q as u8);
            }
        }
        Format::Int64 => {
            for &v in vals {
                let q = v.round().clamp(i64::MIN as f64, i64::MAX as f64) as i64;
                scratch.extend_from_slice(&q.to_ne_bytes());
            }
        }
        Format::String | Format::Undefined => {}
    }
    PackedSample::from_raw_bytes(timestamp, format, Arc::from(scratch.as_slice()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fir_taps_are_symmetric_normalized_and_match_reference_convolution() {
        let d = 4u32;
        let taps = fir_taps(d);
        assert_eq!(taps.len(), 17);
        let sum: f64 = taps.iter().sum();
        assert!((sum - 1.0).abs() < 1e-12, "DC gain must be 1.0");
        for k in 0..taps.len() {
            assert!(
                (taps[k] - taps[taps.len() - 1 - k]).abs() < 1e-15,
                "taps must be symmetric"
            );
        }

        // Feed a deterministic signal and compare every decimated output sample
        // against direct reference convolution of the named Hamming-sinc filter.
        let n_in = 200usize;
        let signal: Vec<f64> = (0..n_in)
            .map(|i| {
                let t = i as f64 / 400.0;
                (2.0 * PI * 10.0 * t).sin() + 0.5 * (2.0 * PI * 140.0 * t).cos()
            })
            .collect();

        let mut sel = StreamSelector::new(SelectParam::decimate(d), Format::Double64, 1);
        let mut got = Vec::new();
        for (i, &x) in signal.iter().enumerate() {
            let ts = i as f64 / 400.0;
            let s = PackedSample::from_f64(ts, &[x]);
            if let Some(out) = sel.push(&s, ts) {
                let mut v = Vec::new();
                out.push_into_f64(&mut v);
                got.push((out.timestamp, v[0]));
            }
        }

        // Independent direct reference convolution with the same edge padding.
        let m = (taps.len() - 1) / 2;
        let mut padded: Vec<f64> = vec![signal[0]; m];
        padded.extend_from_slice(&signal);
        let mut ref_out = Vec::new();
        let mut idx = taps.len() - 1;
        while idx < padded.len() {
            let mut acc = 0.0;
            for k in 0..taps.len() {
                acc += taps[k] * padded[idx + 1 - taps.len() + k];
            }
            let center_in = idx - 2 * m;
            ref_out.push((center_in as f64 / 400.0, acc));
            idx += d as usize;
        }

        assert_eq!(got.len(), ref_out.len());
        for (k, ((gt, gv), (rt, rv))) in got.iter().zip(&ref_out).enumerate() {
            assert!((gt - rt).abs() < 1e-12, "timestamp mismatch at {k}");
            assert!(
                (gv - rv).abs() < 1e-12,
                "value mismatch at {k}: {gv} vs {rv}"
            );
        }
    }

    #[test]
    fn decimator_preserves_dc_and_passband_and_attenuates_aliasing_stopband() {
        let d = 16u32; // 4000 Hz -> 250 Hz (Nyquist_out = 125 Hz)
        let fs = 4000.0;

        // 1. Constant DC signal: exact 3.5 after filter warmup.
        let mut dc_sel = StreamSelector::new(SelectParam::decimate(d), Format::Float32, 1);
        let mut dc_vals = Vec::new();
        for i in 0..512 {
            let ts = i as f64 / fs;
            if let Some(o) = dc_sel.push(&PackedSample::from_f32(ts, &[3.5]), ts) {
                o.push_into_f32(&mut dc_vals);
            }
        }
        assert!(!dc_vals.is_empty());
        for &v in &dc_vals {
            assert!((v - 3.5).abs() < 1e-5, "DC drift: {v}");
        }

        // 2. Passband 20 Hz sine wave (well below 112.5 Hz cutoff): amplitude ~ 1.0.
        let mut pb_sel = StreamSelector::new(SelectParam::decimate(d), Format::Double64, 1);
        let mut pb_vals = Vec::new();
        for i in 0..2000 {
            let ts = i as f64 / fs;
            let x = (2.0 * PI * 20.0 * ts).sin();
            if let Some(o) = pb_sel.push(&PackedSample::from_f64(ts, &[x]), ts) {
                o.push_into_f64(&mut pb_vals);
            }
        }
        let pb_peak = pb_vals[10..].iter().map(|v| v.abs()).fold(0.0f64, f64::max);
        assert!(pb_peak > 0.95, "passband attenuated too much: {pb_peak}");

        // 3. Stopband 300 Hz sine wave (well above 125 Hz Nyquist_out): attenuated > 35 dB (< 0.018).
        let mut sb_sel = StreamSelector::new(SelectParam::decimate(d), Format::Double64, 1);
        let mut sb_vals = Vec::new();
        for i in 0..2000 {
            let ts = i as f64 / fs;
            let x = (2.0 * PI * 300.0 * ts).sin();
            if let Some(o) = sb_sel.push(&PackedSample::from_f64(ts, &[x]), ts) {
                o.push_into_f64(&mut sb_vals);
            }
        }
        let sb_peak = sb_vals[10..].iter().map(|v| v.abs()).fold(0.0f64, f64::max);
        assert!(
            sb_peak < 0.018,
            "stopband 300 Hz must be attenuated > 35 dB, got peak {sb_peak}"
        );
    }

    #[test]
    fn channel_projection_preserves_exact_bits_including_negative_zero_and_nan() {
        let neg_zero = -0.0f32;
        let nan_payload = f32::from_bits(0x7fc0_1234);
        let src = PackedSample::from_f32(12.5, &[1.0, neg_zero, 2.0, nan_payload, 4.0]);
        let projected = project_packed(&src, &[3, 1, 0]);
        let mut vals = Vec::new();
        projected.push_into_f32(&mut vals);
        assert_eq!(vals.len(), 3);
        assert_eq!(vals[0].to_bits(), nan_payload.to_bits());
        assert_eq!(vals[1].to_bits(), neg_zero.to_bits());
        assert_eq!(vals[2].to_bits(), 1.0f32.to_bits());
    }
}
