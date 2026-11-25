//! Device-delay calibration against a reference trigger stream
//! (`docs/roadmap.md`, step 15).
//!
//! A physical sensor or acquisition pipeline introduces a fixed delay between
//! the physical event and the timestamp stamped at `Outlet::push`. This module
//! measures that delay by pairing rising edges on a numeric device channel
//! (with sub-sample linear interpolation) against reference event timestamps,
//! stores the result in `<desc><synchronization><offset_mean>` (an XDF-retained
//! LSL metadata field), and lets an inlet subtract it opt-in via
//! `labstream_time::flags::DEVICE_DELAY`.

use crate::{Inlet, StreamInfo};
use labstream_time::flags;
use labstream_wire::Format;
use std::time::{Duration, Instant};

/// Options controlling [`calibrate_streams`].
#[derive(Debug, Clone)]
pub struct CalibrationOptions {
    /// Zero-based channel index on the device stream that carries the
    /// calibration pulse (default: `0`).
    pub channel: usize,
    /// Optional explicit rising-edge threshold. When `None`, the midpoint
    /// between the observed baseline and peak is computed automatically.
    pub threshold: Option<f64>,
    /// Target number of matched pulses to collect before returning (default: `20`).
    pub target_pulses: usize,
    /// Minimum plausible delay in seconds (default: `-0.05`, allowing slight
    /// negative offsets if the reference itself is delayed).
    pub min_delay_s: f64,
    /// Maximum plausible delay in seconds (default: `0.50`).
    pub max_delay_s: f64,
    /// Minimum refractory interval between two detected device rising edges in
    /// seconds (default: `0.05`).
    pub refractory_s: f64,
    /// Maximum total wall-clock time to wait for calibration pulses (default: `30 s`).
    pub timeout: Duration,
}

impl Default for CalibrationOptions {
    fn default() -> Self {
        CalibrationOptions {
            channel: 0,
            threshold: None,
            target_pulses: 20,
            min_delay_s: -0.05,
            max_delay_s: 0.50,
            refractory_s: 0.05,
            timeout: Duration::from_secs(30),
        }
    }
}

/// Summary statistics of a device-delay calibration run.
#[derive(Debug, Clone, PartialEq)]
pub struct CalibrationResult {
    /// Trimmed mean delay `t_device - t_reference` in seconds (positive means
    /// the device sample timestamp lags the reference event).
    pub delay_mean_s: f64,
    /// Median delay in seconds.
    pub delay_median_s: f64,
    /// Root-mean-square jitter around `delay_mean_s` in seconds.
    pub delay_rms_s: f64,
    /// 5th percentile delay in seconds.
    pub delay_p5_s: f64,
    /// 95th percentile delay in seconds.
    pub delay_p95_s: f64,
    /// Number of paired reference-to-device pulses used in the estimate.
    pub matched_pulses: usize,
    /// Total reference events observed.
    pub reference_events: usize,
    /// Total rising edges detected on the device channel.
    pub device_edges: usize,
    /// Threshold used to detect device rising edges.
    pub threshold_used: f64,
}

impl CalibrationResult {
    /// Write this calibration into `info`'s `<desc><synchronization>` subtree.
    ///
    /// `liblsl` tools and `LabRecorder` keep `<desc>` verbatim in XDF files.
    pub fn apply_to_info(&self, info: &mut StreamInfo) {
        info.set_device_delay_stats(
            self.delay_mean_s,
            Some(self.delay_rms_s),
            Some(self.delay_median_s),
            Some(self.delay_p5_s),
            Some(self.delay_p95_s),
        );
    }

    /// Format the `<synchronization>` XML snippet for embedding in a stream
    /// description.
    pub fn to_xml_snippet(&self) -> String {
        format!(
            "<synchronization>\n\
             \t<offset_mean>{:.6}</offset_mean>\n\
             \t<offset_rms>{:.6}</offset_rms>\n\
             \t<offset_median>{:.6}</offset_median>\n\
             \t<offset_5_percentile>{:.6}</offset_5_percentile>\n\
             \t<offset_95_percentile>{:.6}</offset_95_percentile>\n\
             </synchronization>\n",
            self.delay_mean_s,
            self.delay_rms_s,
            self.delay_median_s,
            self.delay_p5_s,
            self.delay_p95_s,
        )
    }

    /// Format this calibration as a single-line JSON object.
    pub fn to_json(&self) -> String {
        format!(
            "{{\"delay_mean_s\":{:.6},\"delay_median_s\":{:.6},\"delay_rms_s\":{:.6},\"delay_p5_s\":{:.6},\"delay_p95_s\":{:.6},\"matched_pulses\":{},\"reference_events\":{},\"device_edges\":{},\"threshold_used\":{:.6}}}",
            self.delay_mean_s,
            self.delay_median_s,
            self.delay_rms_s,
            self.delay_p5_s,
            self.delay_p95_s,
            self.matched_pulses,
            self.reference_events,
            self.device_edges,
            self.threshold_used,
        )
    }
}

/// Detect rising-edge timestamps on a scalar `(timestamp, value)` sequence
/// using sub-sample linear interpolation between adjacent samples.
pub fn detect_rising_edges(
    samples: &[(f64, f64)],
    threshold: f64,
    refractory_s: f64,
) -> Vec<f64> {
    let mut edges = Vec::new();
    let mut last_edge = f64::NEG_INFINITY;
    for w in samples.windows(2) {
        let (t0, v0) = w[0];
        let (t1, v1) = w[1];
        if !t0.is_finite() || !t1.is_finite() || !v0.is_finite() || !v1.is_finite() {
            continue;
        }
        if v0 < threshold && v1 >= threshold {
            let dv = v1 - v0;
            let frac = if dv.abs() > 0.0 {
                ((threshold - v0) / dv).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let t_cross = t0 + frac * (t1 - t0);
            if t_cross - last_edge >= refractory_s {
                edges.push(t_cross);
                last_edge = t_cross;
            }
        }
    }
    edges
}

/// Measure device delay from recorded reference timestamps and device
/// `(timestamp, value)` samples.
///
/// Pairs each reference timestamp `t_ref` with the earliest device rising edge
/// in `[t_ref + min_delay_s, t_ref + max_delay_s]`, filters outliers via MAD,
/// and computes mean, median, RMS jitter, and 5th/95th percentiles.
pub fn measure_pulse_delays(
    reference_times: &[f64],
    device_samples: &[(f64, f64)],
    opts: &CalibrationOptions,
) -> Result<CalibrationResult, String> {
    if reference_times.is_empty() {
        return Err("no reference events recorded".to_string());
    }
    if device_samples.len() < 2 {
        return Err("not enough device samples to detect edges".to_string());
    }

    let threshold = match opts.threshold {
        Some(th) if th.is_finite() => th,
        _ => {
            let mut vals: Vec<f64> = device_samples
                .iter()
                .map(|&(_, v)| v)
                .filter(|v| v.is_finite())
                .collect();
            if vals.len() < 2 {
                return Err("device channel contains no finite values".to_string());
            }
            vals.sort_by(f64::total_cmp);
            let low = vals[vals.len() / 10];
            let high = vals[(vals.len() * 99) / 100];
            if (high - low).abs() < 1e-12 {
                return Err("device channel is flat; no calibration pulses found".to_string());
            }
            0.5 * (low + high)
        }
    };

    let edges = detect_rising_edges(device_samples, threshold, opts.refractory_s);
    if edges.is_empty() {
        return Err(format!(
            "no rising edges crossed threshold {threshold:.6} on device channel"
        ));
    }

    let mut refs: Vec<f64> = reference_times
        .iter()
        .copied()
        .filter(|t| t.is_finite())
        .collect();
    refs.sort_by(f64::total_cmp);

    let mut deltas = Vec::new();
    let mut e_idx = 0usize;
    for &t_ref in &refs {
        while e_idx < edges.len() && edges[e_idx] < t_ref + opts.min_delay_s {
            e_idx += 1;
        }
        if e_idx < edges.len() {
            let d = edges[e_idx] - t_ref;
            if d >= opts.min_delay_s && d <= opts.max_delay_s {
                deltas.push(d);
                e_idx += 1;
            }
        }
    }

    if deltas.is_empty() {
        return Err("no device pulses fell within the delay search window".to_string());
    }

    deltas.sort_by(f64::total_cmp);
    let med = quantile_sorted(&deltas, 0.5);

    // Reject gross outliers when >= 5 pulses were paired.
    let kept: Vec<f64> = if deltas.len() >= 5 {
        let mut abs_dev: Vec<f64> = deltas.iter().map(|d| (d - med).abs()).collect();
        abs_dev.sort_by(f64::total_cmp);
        let mad = abs_dev[abs_dev.len() / 2];
        let limit = (3.5 * mad).max(0.002);
        let filtered: Vec<f64> = deltas
            .iter()
            .copied()
            .filter(|d| (d - med).abs() <= limit)
            .collect();
        if filtered.is_empty() {
            deltas.clone()
        } else {
            filtered
        }
    } else {
        deltas.clone()
    };

    let n = kept.len() as f64;
    let mean = kept.iter().sum::<f64>() / n;
    let var = kept.iter().map(|d| (d - mean) * (d - mean)).sum::<f64>() / n;
    let rms = var.sqrt();

    Ok(CalibrationResult {
        delay_mean_s: mean,
        delay_median_s: quantile_sorted(&kept, 0.5),
        delay_rms_s: rms,
        delay_p5_s: quantile_sorted(&kept, 0.05),
        delay_p95_s: quantile_sorted(&kept, 0.95),
        matched_pulses: kept.len(),
        reference_events: refs.len(),
        device_edges: edges.len(),
        threshold_used: threshold,
    })
}

fn quantile_sorted(sorted: &[f64], q: f64) -> f64 {
    let idx = ((sorted.len().saturating_sub(1)) as f64 * q).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// Live device-delay calibration between a numeric device stream and a
/// reference trigger/marker stream.
///
/// Both inlets enable `CLOCKSYNC | DRIFT` so cross-machine calibration uses
/// smooth drift-corrected timestamps on the local clock.
pub fn calibrate_streams(
    device_info: &StreamInfo,
    reference_info: &StreamInfo,
    opts: &CalibrationOptions,
) -> Result<CalibrationResult, String> {
    if device_info.format == Format::String {
        return Err("device stream must carry numeric samples".to_string());
    }
    if opts.channel >= device_info.channel_count as usize {
        return Err(format!(
            "channel {} is out of range for stream with {} channels",
            opts.channel, device_info.channel_count
        ));
    }

    let mut dev_inlet = Inlet::open_with_buffer(device_info, Duration::from_secs(5), 65_536)
        .map_err(|e| format!("failed to open device stream: {e}"))?;
    let mut ref_inlet = Inlet::open_with_buffer(reference_info, Duration::from_secs(5), 16_384)
        .map_err(|e| format!("failed to open reference stream: {e}"))?;

    let post = flags::CLOCKSYNC | flags::DRIFT;
    dev_inlet.set_postprocessing(post);
    ref_inlet.set_postprocessing(post);

    // Wait briefly for initial clock offset on both inlets.
    let _ = dev_inlet.wait_for_time_correction(Duration::from_secs(2));
    let _ = ref_inlet.wait_for_time_correction(Duration::from_secs(2));

    let mut dev_samples: Vec<(f64, f64)> = Vec::new();
    let mut ref_times: Vec<f64> = Vec::new();
    let mut ref_numeric: Vec<(f64, f64)> = Vec::new();
    let ref_is_string = reference_info.format == Format::String;
    let mut dev_batch = Vec::new();
    let mut ref_batch = Vec::new();

    let deadline = Instant::now() + opts.timeout;
    let mut last_ref_count = 0usize;
    let mut tail_until: Option<Instant> = None;

    while Instant::now() < deadline {
        dev_batch.clear();
        let _ = dev_inlet.pull_chunk(&mut dev_batch, 4096, Duration::from_millis(20));
        for s in &dev_batch {
            if let Some(v) = s.values.get(opts.channel).and_then(value_as_f64) {
                dev_samples.push((s.timestamp, v));
            }
        }

        ref_batch.clear();
        let _ = ref_inlet.pull_chunk(&mut ref_batch, 1024, Duration::ZERO);
        for s in &ref_batch {
            if ref_is_string {
                ref_times.push(s.timestamp);
            } else if let Some(v) = s.values.first().and_then(value_as_f64) {
                ref_numeric.push((s.timestamp, v));
            }
        }

        let current_refs = if ref_is_string {
            ref_times.len()
        } else {
            // Estimate rising edges on numeric reference stream.
            if ref_numeric.len() >= 2 {
                let mut vals: Vec<f64> = ref_numeric.iter().map(|&(_, v)| v).collect();
                vals.sort_by(f64::total_cmp);
                let low = vals[vals.len() / 10];
                let high = vals[(vals.len() * 99) / 100];
                if (high - low).abs() > 1e-12 {
                    detect_rising_edges(&ref_numeric, 0.5 * (low + high), opts.refractory_s).len()
                } else {
                    0
                }
            } else {
                0
            }
        };

        if current_refs >= opts.target_pulses && tail_until.is_none() {
            // Wait one `max_delay_s` tail so the final device pulse arrives.
            let tail = Duration::from_secs_f64(opts.max_delay_s.clamp(0.05, 1.0));
            tail_until = Some(Instant::now() + tail);
        }
        if let Some(t_end) = tail_until {
            if Instant::now() >= t_end {
                break;
            }
        }
        if current_refs != last_ref_count {
            last_ref_count = current_refs;
        }
    }

    if !ref_is_string {
        if ref_numeric.len() < 2 {
            return Err("not enough numeric reference samples".to_string());
        }
        let mut vals: Vec<f64> = ref_numeric.iter().map(|&(_, v)| v).collect();
        vals.sort_by(f64::total_cmp);
        let low = vals[vals.len() / 10];
        let high = vals[(vals.len() * 99) / 100];
        if (high - low).abs() < 1e-12 {
            return Err("numeric reference channel is flat".to_string());
        }
        ref_times = detect_rising_edges(&ref_numeric, 0.5 * (low + high), opts.refractory_s);
    }

    measure_pulse_delays(&ref_times, &dev_samples, opts)
}

fn value_as_f64(v: &labstream_wire::Value) -> Option<f64> {
    use labstream_wire::Value;
    Some(match *v {
        Value::F32(x) => x as f64,
        Value::F64(x) => x,
        Value::I32(x) => x as f64,
        Value::I16(x) => x as f64,
        Value::I8(x) => x as f64,
        Value::I64(x) => x as f64,
        Value::Str(_) => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sub_sample_pulse_calibration_measures_known_delay_within_one_ms() {
        // Simulate a 500 Hz device with a true 23.4 ms delay and 0.2 ms jitter.
        let srate = 500.0;
        let true_delay = 0.0234;
        let mut ref_times = Vec::new();
        for k in 0..25 {
            ref_times.push(100.0 + k as f64 * 0.2);
        }

        let mut dev_samples = Vec::new();
        let total = (6.0 * srate) as usize;
        for i in 0..total {
            let t = 99.8 + i as f64 / srate;
            // Pulse high for 20 ms starting at `t_ref + true_delay`.
            let mut val = 0.0;
            for (k, &tr) in ref_times.iter().enumerate() {
                let jitter = ((k % 3) as f64 - 1.0) * 0.0002;
                let edge = tr + true_delay + jitter;
                if t >= edge && t < edge + 0.02 {
                    val = 1.0;
                    break;
                }
            }
            dev_samples.push((t, val));
        }

        let res = measure_pulse_delays(&ref_times, &dev_samples, &CalibrationOptions::default())
            .expect("calibration succeeds");
        assert_eq!(res.matched_pulses, 25);
        assert!(
            (res.delay_mean_s - true_delay).abs() < 0.001,
            "expected error < 1 ms, got {:.3} ms (measured {:.3} ms)",
            (res.delay_mean_s - true_delay).abs() * 1000.0,
            res.delay_mean_s * 1000.0
        );
        assert!(res.delay_rms_s < 0.001);
    }
}
