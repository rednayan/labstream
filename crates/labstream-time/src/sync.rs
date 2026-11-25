//! Real-time multi-stream time-grid alignment and windowed-sinc resampler
//! (`docs/roadmap.md`, step 18).
//!
//! Two asynchronous ADCs without a shared hardware clock sample the same
//! physical signal with a worst-case sub-sample offset of `T_s / 2` (up to
//! 72 degrees at 100 Hz for 250 SPS streams). [`StreamAligner`] buffers each
//! stream in a bounded ring, tracks explicit and timestamp-inferred gaps, and
//! resamples all streams onto one strictly monotonic common time grid using
//! Blackman-windowed sinc fractional-delay interpolation with bounded
//! lookahead.

use std::collections::VecDeque;

/// Default half-width of the windowed-sinc kernel in source samples (`2 * 16 = 32` taps).
pub const DEFAULT_HALF_WIDTH: usize = 16;

/// Default per-stream ring buffer capacity in samples.
pub const DEFAULT_MAX_BUFFER_SAMPLES: usize = 8192;

/// Default timestamp-jump multiple of `1 / nominal_srate` treated as a gap.
pub const DEFAULT_GAP_FACTOR: f64 = 1.8;

/// Description of one input stream fed into [`StreamAligner`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StreamSpec {
    /// Number of numeric channels in this stream.
    pub channels: usize,
    /// Nominal sampling rate of this stream in Hz (must be positive and finite).
    pub nominal_srate: f64,
}

/// Configuration for [`StreamAligner`] and `SyncGroup`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SyncConfig {
    /// Common output grid sampling rate in Hz.
    pub output_rate: f64,
    /// Sinc kernel half-width in source samples (lookahead = `half_width / min_rate`).
    pub half_width: usize,
    /// Maximum samples buffered per input stream (bounds memory).
    pub max_buffer_samples: usize,
    /// Gap threshold as a multiple of nominal sample period (`1.8` default).
    pub gap_threshold_factor: f64,
    /// When true (default), `SyncGroup` enables `CLOCKSYNC | DRIFT | DEVICE_DELAY`
    /// on each inlet automatically.
    pub auto_postprocess: bool,
}

impl SyncConfig {
    /// Create a default configuration for `output_rate` Hz.
    pub fn new(output_rate: f64) -> Self {
        SyncConfig {
            output_rate,
            half_width: DEFAULT_HALF_WIDTH,
            max_buffer_samples: DEFAULT_MAX_BUFFER_SAMPLES,
            gap_threshold_factor: DEFAULT_GAP_FACTOR,
            auto_postprocess: true,
        }
    }
}

/// One aligned sample on the common monotonic output grid.
#[derive(Debug, Clone, PartialEq)]
pub struct AlignedSample {
    /// Monotonic timestamp on the common output grid, in seconds.
    pub timestamp: f64,
    /// Concatenated channel values across all input streams, in stream order.
    /// Channels belonging to a stream with a gap at this timestamp are `NaN`.
    pub values: Vec<f64>,
    /// Bitmask of streams that experienced a gap covering this timestamp
    /// (bit `i` = stream `i`; `0` means every stream is clean).
    pub gap_mask: u64,
}

impl AlignedSample {
    /// True when at least one input stream had a gap at this grid timestamp.
    pub fn any_gap(&self) -> bool {
        self.gap_mask != 0
    }
}

#[derive(Debug)]
struct StreamBuf {
    spec: StreamSpec,
    dt: f64,
    timestamps: VecDeque<f64>,
    /// Interleaved channel values (`timestamps.len() * spec.channels`).
    values: VecDeque<f64>,
    /// Explicit gap intervals `(start, end)` reported by the inlet.
    gaps: VecDeque<(f64, f64)>,
}

impl StreamBuf {
    fn new(spec: StreamSpec) -> Self {
        StreamBuf {
            spec,
            dt: 1.0 / spec.nominal_srate,
            timestamps: VecDeque::new(),
            values: VecDeque::new(),
            gaps: VecDeque::new(),
        }
    }

    fn push(&mut self, ts: f64, vals: &[f64], max_samples: usize, gap_factor: f64) {
        if !ts.is_finite() || vals.len() != self.spec.channels {
            return;
        }
        if let Some(&prev) = self.timestamps.back() {
            if ts <= prev {
                // Keep per-stream buffer strictly monotonic.
                return;
            }
            if ts - prev > self.dt * gap_factor {
                self.record_gap(prev, ts);
            }
        }
        if self.timestamps.len() >= max_samples {
            self.timestamps.pop_front();
            for _ in 0..self.spec.channels {
                self.values.pop_front();
            }
        }
        self.timestamps.push_back(ts);
        self.values.extend(vals.iter().copied());
    }

    fn record_gap(&mut self, start: f64, end: f64) {
        if start.is_finite() && end.is_finite() && end > start {
            if self.gaps.len() >= 256 {
                self.gaps.pop_front();
            }
            self.gaps.push_back((start, end));
        }
    }

    fn has_explicit_gap(&self, t: f64) -> bool {
        self.gaps.iter().any(|&(a, b)| t > a && t < b)
    }

    fn oldest_ts(&self) -> Option<f64> {
        self.timestamps.front().copied()
    }

    fn latest_ts(&self) -> Option<f64> {
        self.timestamps.back().copied()
    }

    /// Trim samples strictly older than `cutoff` and expired gap intervals.
    fn trim_before(&mut self, cutoff: f64) {
        while let Some(&front) = self.timestamps.front() {
            if front < cutoff {
                self.timestamps.pop_front();
                for _ in 0..self.spec.channels {
                    self.values.pop_front();
                }
            } else {
                break;
            }
        }
        while let Some(&(_, end)) = self.gaps.front() {
            if end < cutoff {
                self.gaps.pop_front();
            } else {
                break;
            }
        }
    }

    /// Evaluate windowed-sinc interpolation at `t_target`.
    ///
    /// Returns `true` if the interpolation is clean, or `false` if a gap covers
    /// `t_target` (in which case `dst` is filled with `f64::NAN`).
    fn interpolate(&self, t_target: f64, half_width: usize, gap_factor: f64, dst: &mut [f64]) -> bool {
        let ch = self.spec.channels;
        if self.timestamps.is_empty() || self.has_explicit_gap(t_target) {
            dst.fill(f64::NAN);
            return false;
        }
        let n = self.timestamps.len();
        // Binary search for the first index with timestamp >= t_target.
        let mut lo = 0usize;
        let mut hi = n;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.timestamps[mid] < t_target {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }

        // Check bracketing samples around `t_target`.
        if lo == 0 {
            if (self.timestamps[0] - t_target).abs() > self.dt * 0.5 {
                dst.fill(f64::NAN);
                return false;
            }
        } else if lo == n {
            if (t_target - self.timestamps[n - 1]).abs() > self.dt * 0.5 {
                dst.fill(f64::NAN);
                return false;
            }
        } else if self.timestamps[lo] - self.timestamps[lo - 1] > self.dt * gap_factor {
            dst.fill(f64::NAN);
            return false;
        }

        let center = lo.saturating_sub(1);
        let start = center.saturating_sub(half_width);
        let end = (center + half_width + 2).min(n);

        // If any step inside the active kernel window is a gap, keep the window
        // strictly on the same side of the gap or report a gap if it touches
        // the immediate neighborhood.
        let l_f = half_width as f64;
        dst.fill(0.0);
        let mut w_sum = 0.0f64;

        for idx in start..end {
            let ti = self.timestamps[idx];
            // Do not cross a gap boundary when gathering sinc taps.
            if idx < lo && self.has_explicit_gap(0.5 * (ti + t_target)) {
                continue;
            }
            if idx >= lo && self.has_explicit_gap(0.5 * (t_target + ti)) {
                continue;
            }
            let u = (t_target - ti) / self.dt;
            if u.abs() >= l_f {
                continue;
            }
            let w = blackman_sinc(u, l_f);
            w_sum += w;
            let base = idx * ch;
            for (c, out_c) in dst.iter_mut().enumerate().take(ch) {
                *out_c += w * self.values[base + c];
            }
        }

        if w_sum.abs() < 1e-12 {
            dst.fill(f64::NAN);
            return false;
        }
        let inv = 1.0 / w_sum;
        for v in dst.iter_mut() {
            *v *= inv;
        }
        true
    }
}

/// Blackman-windowed normalized sinc kernel evaluated at `u` (in samples) with
/// half-width `half_width`.
#[inline]
fn blackman_sinc(u: f64, half_width: f64) -> f64 {
    let pi = core::f64::consts::PI;
    let sinc = if u.abs() < 1e-12 {
        1.0
    } else {
        let x = pi * u;
        x.sin() / x
    };
    let ratio = u / half_width;
    let win = 0.42 + 0.5 * (pi * ratio).cos() + 0.08 * (2.0 * pi * ratio).cos();
    sinc * win
}

/// Real-time multi-stream aligner and fractional-delay sinc resampler.
#[derive(Debug)]
pub struct StreamAligner {
    cfg: SyncConfig,
    streams: Vec<StreamBuf>,
    total_channels: usize,
    grid_dt: f64,
    next_grid_ts: Option<f64>,
    last_emitted_ts: Option<f64>,
}

impl StreamAligner {
    /// Create a new aligner for `specs` using `cfg`.
    pub fn new(specs: Vec<StreamSpec>, cfg: SyncConfig) -> Result<Self, &'static str> {
        if specs.is_empty() {
            return Err("SyncGroup requires at least one stream");
        }
        if specs.len() > 64 {
            return Err("SyncGroup supports at most 64 streams");
        }
        if !cfg.output_rate.is_finite() || cfg.output_rate <= 0.0 {
            return Err("output_rate must be positive and finite");
        }
        if cfg.half_width < 2 {
            return Err("half_width must be at least 2");
        }
        if cfg.max_buffer_samples < cfg.half_width * 4 {
            return Err("max_buffer_samples must be at least 4 * half_width");
        }
        let mut total_channels = 0usize;
        let mut streams = Vec::with_capacity(specs.len());
        for s in specs {
            if s.channels == 0 || !s.nominal_srate.is_finite() || s.nominal_srate <= 0.0 {
                return Err("each stream must have channels > 0 and finite nominal_srate > 0");
            }
            total_channels += s.channels;
            streams.push(StreamBuf::new(s));
        }
        Ok(StreamAligner {
            cfg,
            streams,
            total_channels,
            grid_dt: 1.0 / cfg.output_rate,
            next_grid_ts: None,
            last_emitted_ts: None,
        })
    }

    /// Number of input streams.
    pub fn stream_count(&self) -> usize {
        self.streams.len()
    }

    /// Total concatenated channels across all streams.
    pub fn total_channels(&self) -> usize {
        self.total_channels
    }

    /// Common output sampling rate in Hz.
    pub fn output_rate(&self) -> f64 {
        self.cfg.output_rate
    }

    /// Lookahead latency in seconds (`half_width / min_input_srate`).
    pub fn lookahead_seconds(&self) -> f64 {
        let min_rate = self
            .streams
            .iter()
            .map(|s| s.spec.nominal_srate)
            .fold(f64::INFINITY, f64::min);
        self.cfg.half_width as f64 / min_rate
    }

    /// Push one `f64` sample for `stream_idx`.
    pub fn push(&mut self, stream_idx: usize, timestamp: f64, values: &[f64]) {
        if let Some(buf) = self.streams.get_mut(stream_idx) {
            buf.push(
                timestamp,
                values,
                self.cfg.max_buffer_samples,
                self.cfg.gap_threshold_factor,
            );
        }
    }

    /// Push one `f32` sample for `stream_idx`.
    pub fn push_f32(&mut self, stream_idx: usize, timestamp: f64, values: &[f32]) {
        if let Some(buf) = self.streams.get_mut(stream_idx) {
            if values.len() != buf.spec.channels {
                return;
            }
            let tmp: Vec<f64> = values.iter().map(|&v| v as f64).collect();
            buf.push(
                timestamp,
                &tmp,
                self.cfg.max_buffer_samples,
                self.cfg.gap_threshold_factor,
            );
        }
    }

    /// Record an explicit gap interval `(start, end)` from `Inlet::take_gaps`.
    pub fn mark_gap(&mut self, stream_idx: usize, start_ts: f64, end_ts: f64) {
        if let Some(buf) = self.streams.get_mut(stream_idx) {
            buf.record_gap(start_ts, end_ts);
        }
    }

    /// Number of buffered samples currently held for `stream_idx`.
    pub fn buffered_samples(&self, stream_idx: usize) -> usize {
        self.streams.get(stream_idx).map_or(0, |b| b.timestamps.len())
    }

    /// Pull up to `max_samples` aligned samples onto the common monotonic grid.
    pub fn pull_aligned(&mut self, out: &mut Vec<AlignedSample>, max_samples: usize) -> usize {
        if max_samples == 0 {
            return 0;
        }
        // Every stream must have at least one sample before we know the common horizon.
        let mut ready_horizon = f64::INFINITY;
        let mut init_start = f64::NEG_INFINITY;
        for s in &self.streams {
            let Some(latest) = s.latest_ts() else {
                return 0;
            };
            let Some(oldest) = s.oldest_ts() else {
                return 0;
            };
            let margin = self.cfg.half_width as f64 * s.dt;
            ready_horizon = ready_horizon.min(latest - margin);
            init_start = init_start.max(oldest + margin);
        }

        let mut t = match self.next_grid_ts {
            Some(existing) => existing,
            None => {
                if init_start > ready_horizon {
                    return 0;
                }
                self.next_grid_ts = Some(init_start);
                init_start
            }
        };

        let mut emitted = 0usize;
        while emitted < max_samples && t <= ready_horizon {
            // Enforce strict output monotonicity even across clock resets.
            if let Some(last) = self.last_emitted_ts {
                if t <= last {
                    t = last + self.grid_dt;
                    continue;
                }
            }

            let mut values = vec![0.0f64; self.total_channels];
            let mut gap_mask = 0u64;
            let mut offset = 0usize;
            for (idx, s) in self.streams.iter().enumerate() {
                let ch = s.spec.channels;
                let clean = s.interpolate(
                    t,
                    self.cfg.half_width,
                    self.cfg.gap_threshold_factor,
                    &mut values[offset..offset + ch],
                );
                if !clean {
                    gap_mask |= 1u64 << idx;
                }
                offset += ch;
            }

            out.push(AlignedSample {
                timestamp: t,
                values,
                gap_mask,
            });
            self.last_emitted_ts = Some(t);
            t += self.grid_dt;
            emitted += 1;
        }

        self.next_grid_ts = Some(t);

        // Trim old samples behind the active kernel window so memory stays bounded.
        for s in &mut self.streams {
            let cutoff = t - (self.cfg.half_width as f64 + 2.0) * s.dt;
            s.trim_before(cutoff);
        }

        emitted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Criterion 1 of step 18:
    /// Two simulated asynchronous 250 SPS streams of a 100 Hz sine wave with a
    /// half-sample (2 ms) clock phase offset align to < 2 degrees of phase
    /// error (down from 72 degrees with nearest-sample pairing).
    #[test]
    fn two_async_250sps_100hz_streams_with_half_sample_offset_align_below_2_deg() {
        let fs = 250.0f64;
        let f_sig = 100.0f64;
        let dt = 1.0 / fs;
        let half_sample = 0.5 * dt; // 2 ms -> 72 degrees at 100 Hz!

        let mut aligner = StreamAligner::new(
            vec![
                StreamSpec {
                    channels: 1,
                    nominal_srate: fs,
                },
                StreamSpec {
                    channels: 1,
                    nominal_srate: fs,
                },
            ],
            SyncConfig::new(fs),
        )
        .unwrap();

        let n = 400;
        let mut aligned = Vec::new();
        for k in 0..n {
            let t0 = 10.0 + k as f64 * dt;
            let t1 = 10.0 + k as f64 * dt + half_sample;
            let v0 = (2.0 * core::f64::consts::PI * f_sig * t0).sin();
            let v1 = (2.0 * core::f64::consts::PI * f_sig * t1).sin();
            aligner.push(0, t0, &[v0]);
            aligner.push(1, t1, &[v1]);
            aligner.pull_aligned(&mut aligned, 64);
        }

        assert!(aligned.len() > 200, "got {} aligned samples", aligned.len());
        // Skip first few and check steady-state phase difference between stream 0 and stream 1
        // via cross-correlation / quadrature phase estimation.
        let slice = &aligned[10..aligned.len() - 10];
        let mut sum_sq_err = 0.0f64;
        let mut sum_sq_ref = 0.0f64;
        for (i, s) in slice.iter().enumerate() {
            assert!(!s.any_gap());
            if i > 0 {
                assert!(s.timestamp > slice[i - 1].timestamp);
            }
            let diff = s.values[0] - s.values[1];
            sum_sq_err += diff * diff;
            sum_sq_ref += s.values[0] * s.values[0];
        }
        // For two equal-amplitude sine waves differing by phase dphi:
        // RMS(sin(wt) - sin(wt + dphi)) / RMS(sin(wt)) = 2 * sin(|dphi| / 2) ≈ |dphi| (radians).
        let rel_rms = (sum_sq_err / sum_sq_ref).sqrt();
        let phase_err_deg = 2.0 * (rel_rms / 2.0).min(1.0).asin() * 180.0 / core::f64::consts::PI;
        assert!(
            phase_err_deg < 2.0,
            "expected phase error < 2 deg (vs 72 deg nearest-sample), got {phase_err_deg:.4} deg"
        );
    }

    /// Criterion 2 of step 18:
    /// Output timestamps stay monotonic across clock drift and recovery, and
    /// every inlet gap propagates as an explicit gap marker.
    #[test]
    fn gaps_propagate_explicitly_and_memory_stays_bounded() {
        let fs = 250.0f64;
        let dt = 1.0 / fs;
        let mut cfg = SyncConfig::new(fs);
        cfg.max_buffer_samples = 128;

        let mut aligner = StreamAligner::new(
            vec![
                StreamSpec {
                    channels: 1,
                    nominal_srate: fs,
                },
                StreamSpec {
                    channels: 1,
                    nominal_srate: fs,
                },
            ],
            cfg,
        )
        .unwrap();

        let mut aligned = Vec::new();
        for k in 0..300 {
            let t0 = 100.0 + k as f64 * dt;
            // Stream 1 has a +100 ppm drift and drops samples 120..140.
            let t1 = 100.0 + k as f64 * dt * 1.0001;
            aligner.push(0, t0, &[1.0]);
            if !(120..140).contains(&k) {
                aligner.push(1, t1, &[2.0]);
            }
            aligner.pull_aligned(&mut aligned, 64);
            assert!(aligner.buffered_samples(0) <= cfg.max_buffer_samples);
            assert!(aligner.buffered_samples(1) <= cfg.max_buffer_samples);
        }

        // Strict monotonicity.
        for w in aligned.windows(2) {
            assert!(w[1].timestamp > w[0].timestamp);
        }
        // Gap on stream 1 (bit 1 = 0b10) must be explicitly flagged.
        let gap_samples: Vec<_> = aligned.iter().filter(|s| (s.gap_mask & 0b10) != 0).collect();
        assert!(
            !gap_samples.is_empty(),
            "expected explicit gap markers on stream 1"
        );
        for g in &gap_samples {
            assert!(g.values[0].is_finite());
            assert!(g.values[1].is_nan());
        }
        // Clean samples after the gap recover to 2.0.
        let last = aligned.last().unwrap();
        assert_eq!(last.gap_mask, 0);
        assert!((last.values[1] - 2.0).abs() < 1e-6);
    }
}
