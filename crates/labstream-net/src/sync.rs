//! Live multi-inlet time-grid aligner and resampler (`docs/roadmap.md`, step 18).
//!
//! [`SyncGroup`] combines `N` live numeric [`Inlet`]s (enabling the opt-in
//! drift clock from step 17 and calibrated device delay from step 15 by
//! default), forwards inlet gap descriptors, and resamples all streams onto one
//! common monotonic time grid with bounded lookahead and bounded memory.

use crate::inlet::Inlet;
use labstream_time::flags;
pub use labstream_time::sync::{
    AlignedSample, StreamAligner, StreamSpec, SyncConfig, DEFAULT_GAP_FACTOR, DEFAULT_HALF_WIDTH,
    DEFAULT_MAX_BUFFER_SAMPLES,
};
use labstream_wire::{Format, PackedSample, Value};
use std::io;
use std::time::{Duration, Instant};

/// Live multi-stream aligner combining `N` numeric [`Inlet`]s onto one common
/// monotonic time grid.
pub struct SyncGroup {
    inlets: Vec<Inlet>,
    aligner: StreamAligner,
    scratch: Vec<PackedSample>,
    val_scratch: Vec<f64>,
}

impl SyncGroup {
    /// Build a [`SyncGroup`] from opened numeric [`Inlet`]s and `cfg`.
    ///
    /// When `cfg.auto_postprocess` is true (the default), enables
    /// `CLOCKSYNC | DRIFT | DEVICE_DELAY` on each inlet so clock drift and
    /// calibrated device delays are corrected before resampling.
    pub fn new(mut inlets: Vec<Inlet>, cfg: SyncConfig) -> io::Result<Self> {
        if inlets.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "SyncGroup requires at least one inlet",
            ));
        }
        let mut specs = Vec::with_capacity(inlets.len());
        for inlet in &mut inlets {
            let (fmt, channels, nominal_srate) = {
                let info = inlet.info();
                (info.format, info.channel_count as usize, info.nominal_srate)
            };
            if fmt == Format::String || fmt == Format::Undefined {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "SyncGroup supports numeric streams only",
                ));
            }
            if !nominal_srate.is_finite() || nominal_srate <= 0.0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "SyncGroup requires regular streams with nominal_srate > 0",
                ));
            }
            if cfg.auto_postprocess {
                inlet.set_postprocessing(flags::CLOCKSYNC | flags::DRIFT | flags::DEVICE_DELAY);
            }
            specs.push(StreamSpec {
                channels,
                nominal_srate,
            });
        }
        let aligner = StreamAligner::new(specs, cfg)
            .map_err(|msg| io::Error::new(io::ErrorKind::InvalidInput, msg))?;
        Ok(SyncGroup {
            inlets,
            aligner,
            scratch: Vec::with_capacity(256),
            val_scratch: Vec::with_capacity(64),
        })
    }

    /// Number of input streams in this group.
    pub fn stream_count(&self) -> usize {
        self.aligner.stream_count()
    }

    /// Total concatenated channels across all input streams.
    pub fn total_channels(&self) -> usize {
        self.aligner.total_channels()
    }

    /// Common output grid sampling rate in Hz.
    pub fn output_rate(&self) -> f64 {
        self.aligner.output_rate()
    }

    /// Lookahead latency in seconds (`half_width / min_input_srate`).
    pub fn lookahead_seconds(&self) -> f64 {
        self.aligner.lookahead_seconds()
    }

    /// Borrow the underlying inlets.
    pub fn inlets(&self) -> &[Inlet] {
        &self.inlets
    }

    /// Drain all currently available samples and gaps from each inlet into the
    /// aligner without blocking.
    fn pump_inlets(&mut self) -> io::Result<usize> {
        let mut pulled_any = 0usize;
        for (idx, inlet) in self.inlets.iter_mut().enumerate() {
            for gap in inlet.take_gaps() {
                self.aligner.mark_gap(idx, gap.start, gap.end);
            }
            self.scratch.clear();
            let got = inlet.pull_chunk_packed(&mut self.scratch, 1024, Duration::ZERO)?;
            pulled_any += got;
            let ch = inlet.info().channel_count as usize;
            let fmt = inlet.info().format;
            for sample in &self.scratch {
                self.val_scratch.clear();
                decode_packed_to_f64(sample, fmt, ch, &mut self.val_scratch);
                if self.val_scratch.len() == ch {
                    self.aligner.push(idx, sample.timestamp, &self.val_scratch);
                }
            }
        }
        Ok(pulled_any)
    }

    /// Pull up to `max_samples` aligned samples onto the common monotonic time
    /// grid, waiting up to `timeout` for enough lookahead samples to arrive.
    pub fn pull(
        &mut self,
        out: &mut Vec<AlignedSample>,
        max_samples: usize,
        timeout: Duration,
    ) -> io::Result<usize> {
        if max_samples == 0 {
            return Ok(0);
        }
        let deadline = Instant::now() + timeout;
        loop {
            let _ = self.pump_inlets()?;
            let emitted = self.aligner.pull_aligned(out, max_samples);
            if emitted > 0 || Instant::now() >= deadline {
                return Ok(emitted);
            }
            let rem = deadline.saturating_duration_since(Instant::now());
            std::thread::sleep(rem.min(Duration::from_millis(5)));
        }
    }
}

fn decode_packed_to_f64(sample: &PackedSample, fmt: Format, channels: usize, dst: &mut Vec<f64>) {
    if let Some(raw) = sample.as_numeric_bytes(fmt) {
        match fmt {
            Format::Float32 => {
                for b in raw.chunks_exact(4).take(channels) {
                    dst.push(f32::from_ne_bytes(b.try_into().unwrap()) as f64);
                }
            }
            Format::Double64 => {
                for b in raw.chunks_exact(8).take(channels) {
                    dst.push(f64::from_ne_bytes(b.try_into().unwrap()));
                }
            }
            Format::Int32 => {
                for b in raw.chunks_exact(4).take(channels) {
                    dst.push(i32::from_ne_bytes(b.try_into().unwrap()) as f64);
                }
            }
            Format::Int16 => {
                for b in raw.chunks_exact(2).take(channels) {
                    dst.push(i16::from_ne_bytes(b.try_into().unwrap()) as f64);
                }
            }
            Format::Int8 => {
                for &b in raw.iter().take(channels) {
                    dst.push((b as i8) as f64);
                }
            }
            Format::Int64 => {
                for b in raw.chunks_exact(8).take(channels) {
                    dst.push(i64::from_ne_bytes(b.try_into().unwrap()) as f64);
                }
            }
            Format::String | Format::Undefined => {}
        }
        return;
    }
    let s = sample.to_sample();
    for v in s.values.into_iter().take(channels) {
        match v {
            Value::F32(x) => dst.push(x as f64),
            Value::F64(x) => dst.push(x),
            Value::I32(x) => dst.push(x as f64),
            Value::I16(x) => dst.push(x as f64),
            Value::I8(x) => dst.push(x as f64),
            Value::I64(x) => dst.push(x as f64),
            Value::Str(_) => {}
        }
    }
}
