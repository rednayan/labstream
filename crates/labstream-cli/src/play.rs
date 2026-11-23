//! `labstream play`: replay an XDF 1.0 recording as live LSL outlets.
//!
//! `docs/tools.md` and `docs/roadmap.md` (step 12) specify the timing and
//! bit-exactness requirements.

use crate::xdf::{self, XdfFile};
use labstream_net::{clock, Outlet};
use labstream_wire::Sample;
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::io;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

/// Configuration for [`play_xdf`].
#[derive(Debug, Clone)]
pub struct PlayOptions {
    /// Path to the XDF file (CLI only; ignored by [`play_xdf`]).
    pub path: PathBuf,
    /// Playback speed multiplier (> 0.0). Default is `1.0` (real time).
    pub speed: f64,
    /// Replay continuously until stopped.
    pub loop_play: bool,
    /// Wait for at least one consumer on every outlet before pushing samples.
    pub wait_for_consumers: bool,
    /// Optional stream name filter; empty replays every stream in the file.
    pub streams: Vec<String>,
    /// Optional prefix prepended to every published stream name.
    pub name_prefix: String,
    /// How long to keep outlets alive after the last sample so inlets pull the tail.
    pub linger: Duration,
    /// Suppress progress output on stderr.
    pub quiet: bool,
}

impl Default for PlayOptions {
    fn default() -> Self {
        PlayOptions {
            path: PathBuf::new(),
            speed: 1.0,
            loop_play: false,
            wait_for_consumers: false,
            streams: Vec::new(),
            name_prefix: String::new(),
            linger: Duration::from_millis(300),
            quiet: false,
        }
    }
}

pub fn play_usage() -> &'static str {
    "Usage:
  labstream play <FILE.xdf> [OPTIONS]

Options:
  --speed FACTOR        Playback speed multiplier (default: 1.0).
  --loop                Replay continuously from the start when finished.
  --wait                Wait for a consumer on each outlet before starting.
  --stream NAME[,NAME]  Replay only matching stream names (repeatable).
  --name-prefix PREFIX  Prepend PREFIX to each published stream name.
  --linger SECONDS      Keep outlets open after the last sample (default: 0.3).
  --quiet               Suppress status messages on stderr.
  -h, --help            Show this help text."
}

pub fn parse_play_args(args: &[String]) -> Result<(PlayOptions, bool), String> {
    let mut opts = PlayOptions::default();
    let mut got_path = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => return Ok((opts, true)),
            "--loop" => opts.loop_play = true,
            "--wait" => opts.wait_for_consumers = true,
            "--quiet" => opts.quiet = true,
            "--speed" => {
                i += 1;
                let v = args
                    .get(i)
                    .ok_or_else(|| "missing value after --speed".to_string())?;
                opts.speed = parse_positive_f64("--speed", v)?;
            }
            "--linger" => {
                i += 1;
                let v = args
                    .get(i)
                    .ok_or_else(|| "missing value after --linger".to_string())?;
                let secs = parse_nonneg_f64("--linger", v)?;
                opts.linger = Duration::from_secs_f64(secs);
            }
            "--stream" => {
                i += 1;
                let v = args
                    .get(i)
                    .ok_or_else(|| "missing value after --stream".to_string())?;
                push_csv(&mut opts.streams, v);
            }
            "--name-prefix" => {
                i += 1;
                let v = args
                    .get(i)
                    .ok_or_else(|| "missing value after --name-prefix".to_string())?;
                opts.name_prefix = v.clone();
            }
            other => {
                if let Some(v) = other.strip_prefix("--speed=") {
                    opts.speed = parse_positive_f64("--speed", v)?;
                } else if let Some(v) = other.strip_prefix("--linger=") {
                    let secs = parse_nonneg_f64("--linger", v)?;
                    opts.linger = Duration::from_secs_f64(secs);
                } else if let Some(v) = other.strip_prefix("--stream=") {
                    push_csv(&mut opts.streams, v);
                } else if let Some(v) = other.strip_prefix("--name-prefix=") {
                    opts.name_prefix = v.to_string();
                } else if other.starts_with('-') {
                    return Err(format!("unknown argument: {other}"));
                } else if !got_path {
                    opts.path = PathBuf::from(other);
                    got_path = true;
                } else {
                    return Err(format!("unexpected argument: {other}"));
                }
            }
        }
        i += 1;
    }
    if !got_path {
        return Err("missing XDF file path".to_string());
    }
    Ok((opts, false))
}

fn parse_positive_f64(flag: &str, text: &str) -> Result<f64, String> {
    let v: f64 = text
        .parse()
        .map_err(|_| format!("invalid {flag} value: {text}"))?;
    if !v.is_finite() || v <= 0.0 {
        return Err(format!("{flag} must be a positive number: {text}"));
    }
    Ok(v)
}

fn parse_nonneg_f64(flag: &str, text: &str) -> Result<f64, String> {
    let v: f64 = text
        .parse()
        .map_err(|_| format!("invalid {flag} value: {text}"))?;
    if !v.is_finite() || v < 0.0 {
        return Err(format!("{flag} must be non-negative: {text}"));
    }
    Ok(v)
}

fn push_csv(dst: &mut Vec<String>, text: &str) {
    for part in text.split(',') {
        let trimmed = part.trim();
        if !trimmed.is_empty() {
            dst.push(trimmed.to_string());
        }
    }
}

pub fn run_play(args: &[String]) -> ExitCode {
    let (opts, help) = match parse_play_args(args) {
        Ok(x) => x,
        Err(err) => {
            eprintln!("error: {err}\n\n{}", play_usage());
            return ExitCode::from(2);
        }
    };
    if help {
        println!("{}", play_usage());
        return ExitCode::SUCCESS;
    }
    let file = match xdf::read_file(&opts.path) {
        Ok(f) => f,
        Err(err) => {
            eprintln!("error: cannot read {}: {err}", opts.path.display());
            return ExitCode::from(1);
        }
    };
    match play_xdf(&file, &opts) {
        Ok(total) => {
            if !opts.quiet {
                eprintln!("play complete: pushed {total} sample(s)");
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::from(1)
        }
    }
}

/// Entry in the k-way merge heap across streams, ordered by smallest
/// file-relative time `rel_t` (then stream index, then sample index).
#[derive(Debug, Clone, Copy)]
struct HeapItem {
    rel_t: f64,
    stream_idx: usize,
    sample_idx: usize,
}

impl PartialEq for HeapItem {
    fn eq(&self, other: &Self) -> bool {
        self.rel_t.to_bits() == other.rel_t.to_bits()
            && self.stream_idx == other.stream_idx
            && self.sample_idx == other.sample_idx
    }
}

impl Eq for HeapItem {}

impl PartialOrd for HeapItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for HeapItem {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse order for min-heap behavior in Rust's max-heap `BinaryHeap`.
        other
            .rel_t
            .total_cmp(&self.rel_t)
            .then_with(|| other.stream_idx.cmp(&self.stream_idx))
            .then_with(|| other.sample_idx.cmp(&self.sample_idx))
    }
}

struct ActiveStream<'a> {
    outlet: Outlet,
    samples: &'a [Sample],
    /// Raw file timestamp of sample 0 for this stream.
    t0: f64,
    /// Offset from `global_min_t` to this stream's aligned start `(t0 + clock_offset)`.
    stream_offset: f64,
    /// Relative time of the last sample of this stream (used for `--loop` spacing).
    last_rel_t: f64,
}

/// Replay `file` as live LSL outlets and return the total number of samples pushed.
pub fn play_xdf(file: &XdfFile, opts: &PlayOptions) -> io::Result<u64> {
    let selected: Vec<_> = file
        .streams
        .iter()
        .filter(|s| {
            !s.samples.is_empty()
                && (opts.streams.is_empty() || opts.streams.iter().any(|n| n == &s.info.name))
        })
        .collect();

    if selected.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "no matching non-empty streams found in XDF file",
        ));
    }

    // Align all streams onto one common file timeline while preserving exact
    // per-stream intervals `t_i - t0`.
    let global_min_t = selected
        .iter()
        .map(|s| s.samples[0].timestamp + s.initial_clock_offset)
        .fold(f64::INFINITY, f64::min);

    let mut active = Vec::with_capacity(selected.len());
    for s in &selected {
        let mut info = s.info.clone();
        // Clear network fields and UID so the new outlet binds fresh ports and
        // gets a unique instance identifier.
        info.uid.clear();
        info.v4address.clear();
        info.v6address.clear();
        info.v4data_port = 0;
        info.v4service_port = 0;
        info.v6data_port = 0;
        info.v6service_port = 0;
        if !opts.name_prefix.is_empty() {
            info.name = format!("{}{}", opts.name_prefix, info.name);
        }
        let outlet = Outlet::new(info)?;
        let t0 = s.samples[0].timestamp;
        let stream_offset = ((t0 + s.initial_clock_offset) - global_min_t).max(0.0);
        let last_ts = s.samples.last().unwrap().timestamp;
        let last_rel_t = stream_offset + (last_ts - t0).max(0.0);
        if !opts.quiet {
            eprintln!(
                "outlet #{}: {} ({}, {} ch, {:.1} Hz, {} samples)",
                s.stream_id,
                outlet.info().name,
                outlet.info().stream_type,
                outlet.info().channel_count,
                outlet.info().nominal_srate,
                s.samples.len()
            );
        }
        active.push(ActiveStream {
            outlet,
            samples: &s.samples,
            t0,
            stream_offset,
            last_rel_t,
        });
    }

    if opts.wait_for_consumers {
        if !opts.quiet {
            eprintln!("waiting for consumers on {} stream(s)...", active.len());
        }
        while active.iter().any(|a| a.outlet.consumer_count() == 0) {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    let recording_span = active
        .iter()
        .map(|a| a.last_rel_t)
        .fold(0.0f64, f64::max)
        .max(0.001);

    let mut total_pushed = 0u64;
    let mut min_next_origin = clock();

    loop {
        // Align each loop iteration with the current wall clock while keeping
        // timestamps strictly monotonic across loop boundaries.
        let play_origin = clock().max(min_next_origin);
        let wall_start = Instant::now();
        let mut heap = BinaryHeap::with_capacity(active.len());
        for (idx, a) in active.iter().enumerate() {
            heap.push(HeapItem {
                rel_t: a.stream_offset,
                stream_idx: idx,
                sample_idx: 0,
            });
        }

        while let Some(item) = heap.pop() {
            let target_wall = Duration::from_secs_f64((item.rel_t / opts.speed).max(0.0));
            let elapsed = wall_start.elapsed();
            if target_wall > elapsed {
                std::thread::sleep(target_wall - elapsed);
            }

            let a = &active[item.stream_idx];
            let src = &a.samples[item.sample_idx];
            // Exact per-stream interval preserved from the XDF file:
            // `pub_j - pub_i == (src_j.timestamp - t0) - (src_i.timestamp - t0) == src_j - src_i`.
            let published_ts = (play_origin + a.stream_offset) + (src.timestamp - a.t0);
            a.outlet.push(&Sample {
                timestamp: published_ts,
                values: src.values.clone(),
            });
            total_pushed += 1;

            let next_idx = item.sample_idx + 1;
            if next_idx < a.samples.len() {
                let next_rel =
                    (a.stream_offset + (a.samples[next_idx].timestamp - a.t0)).max(item.rel_t);
                heap.push(HeapItem {
                    rel_t: next_rel,
                    stream_idx: item.stream_idx,
                    sample_idx: next_idx,
                });
            }
        }

        if !opts.loop_play {
            break;
        }
        min_next_origin = play_origin + recording_span + 0.001;
    }

    if !opts.linger.is_zero() {
        std::thread::sleep(opts.linger);
    }
    Ok(total_pushed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xdf::{read_file, read_reader, write_bytes, XdfStream};
    use labstream_net::{build_property_query, resolve, Inlet, StreamInfo};
    use labstream_wire::{Format, Value};
    use std::path::PathBuf;

    #[test]
    fn parse_play_accepts_flags_and_rejects_bad_speed() {
        let args: Vec<String> = [
            "rec.xdf",
            "--speed",
            "2.5",
            "--loop",
            "--wait",
            "--stream",
            "EEG,Markers",
            "--name-prefix",
            "Replay_",
            "--linger",
            "0.5",
            "--quiet",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let (opts, help) = parse_play_args(&args).unwrap();
        assert!(!help);
        assert_eq!(opts.path, PathBuf::from("rec.xdf"));
        assert_eq!(opts.speed, 2.5);
        assert!(opts.loop_play);
        assert!(opts.wait_for_consumers);
        assert_eq!(opts.streams, vec!["EEG", "Markers"]);
        assert_eq!(opts.name_prefix, "Replay_");
        assert_eq!(opts.linger, Duration::from_millis(500));
        assert!(opts.quiet);

        let bad: Vec<String> = ["rec.xdf", "--speed", "0"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(parse_play_args(&bad).is_err());
    }

    #[test]
    fn play_replays_exact_bits_and_timestamp_intervals() {
        let pid = std::process::id();
        let eeg_name = format!("PlayBitExactEEG_{pid}");
        let mrk_name = format!("PlayBitExactMrk_{pid}");

        let mut eeg_info = StreamInfo::new(&eeg_name, "EEG", 2, Format::Float32, 200.0);
        eeg_info.uid = "src-eeg".to_string();
        let eeg_samples = vec![
            Sample {
                timestamp: 1000.000,
                values: vec![Value::F32(-0.0), Value::F32(1.25)],
            },
            Sample {
                timestamp: 1000.005,
                values: vec![Value::F32(f32::from_bits(0x3f80_0001)), Value::F32(-42.5)],
            },
            Sample {
                timestamp: 1000.012,
                values: vec![Value::F32(12345.678), Value::F32(0.0)],
            },
            Sample {
                timestamp: 1000.025,
                values: vec![Value::F32(-999.125), Value::F32(7.625)],
            },
        ];

        let mut mrk_info = StreamInfo::new(&mrk_name, "Markers", 1, Format::String, 0.0);
        mrk_info.uid = "src-mrk".to_string();
        let mrk_samples = vec![
            Sample {
                timestamp: 1000.002,
                values: vec![Value::Str(b"start".to_vec())],
            },
            Sample {
                timestamp: 1000.019,
                values: vec![Value::Str(b"event\0two".to_vec())],
            },
        ];

        let raw = write_bytes(&[
            XdfStream {
                stream_id: 1,
                info: eeg_info,
                initial_clock_offset: 0.0,
                samples: eeg_samples.clone(),
            },
            XdfStream {
                stream_id: 2,
                info: mrk_info,
                initial_clock_offset: 0.0,
                samples: mrk_samples.clone(),
            },
        ]);
        let xdf = read_reader(&raw[..]).unwrap();

        let opts = PlayOptions {
            speed: 5.0,
            wait_for_consumers: true,
            linger: Duration::from_millis(500),
            quiet: true,
            ..PlayOptions::default()
        };

        let xdf_for_thread = xdf.clone();
        let player = std::thread::spawn(move || play_xdf(&xdf_for_thread, &opts));

        // Resolve and connect inlets for both streams.
        let sid = labstream_net::config::get().session_id.clone();
        let eeg_found = match resolve(
            &build_property_query(&sid, "name", &eeg_name),
            1,
            Duration::from_secs(5),
        ) {
            Ok(v) if !v.is_empty() => v,
            _ => {
                eprintln!("skipping live play test (no local network)");
                return;
            }
        };
        let mrk_found = resolve(
            &build_property_query(&sid, "name", &mrk_name),
            1,
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(!mrk_found.is_empty());

        let mut eeg_in = Inlet::open(&eeg_found[0], Duration::from_secs(5)).unwrap();
        let mut mrk_in = Inlet::open(&mrk_found[0], Duration::from_secs(5)).unwrap();

        let mut got_eeg = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while got_eeg.len() < eeg_samples.len() && Instant::now() < deadline {
            if let Ok(Some(s)) = eeg_in.pull(Duration::from_millis(100)) {
                got_eeg.push(s);
            }
        }
        let mut got_mrk = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while got_mrk.len() < mrk_samples.len() && Instant::now() < deadline {
            if let Ok(Some(s)) = mrk_in.pull(Duration::from_millis(100)) {
                got_mrk.push(s);
            }
        }

        let pushed = player.join().unwrap().unwrap();
        assert_eq!(pushed, (eeg_samples.len() + mrk_samples.len()) as u64);
        assert_eq!(got_eeg.len(), eeg_samples.len());
        assert_eq!(got_mrk.len(), mrk_samples.len());

        // Check exact bits on every Float32 value and exact intervals.
        for i in 0..eeg_samples.len() {
            for ch in 0..2 {
                match (&got_eeg[i].values[ch], &eeg_samples[i].values[ch]) {
                    (Value::F32(a), Value::F32(b)) => {
                        assert_eq!(
                            a.to_bits(),
                            b.to_bits(),
                            "bit mismatch at sample {i} ch {ch}"
                        );
                    }
                    other => panic!("unexpected values: {other:?}"),
                }
            }
            if i > 0 {
                let want_dt = eeg_samples[i].timestamp - eeg_samples[i - 1].timestamp;
                let got_dt = got_eeg[i].timestamp - got_eeg[i - 1].timestamp;
                assert!(
                    (got_dt - want_dt).abs() < 1e-9,
                    "EEG interval mismatch at {i}: want {want_dt}, got {got_dt}"
                );
            }
        }

        // Check exact bytes on String markers and exact interval.
        for i in 0..mrk_samples.len() {
            assert_eq!(got_mrk[i].values, mrk_samples[i].values);
        }
        let want_mrk_dt = mrk_samples[1].timestamp - mrk_samples[0].timestamp;
        let got_mrk_dt = got_mrk[1].timestamp - got_mrk[0].timestamp;
        assert!((got_mrk_dt - want_mrk_dt).abs() < 1e-9);
    }

    #[test]
    fn play_replays_real_verify_xdf_stream_bit_exact() {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../conformance/captures/verify.xdf");
        if !path.exists() {
            return;
        }
        let xdf = read_file(&path).unwrap();
        let target_stream = xdf
            .streams
            .iter()
            .find(|s| s.info.name == "Verify-rust")
            .expect("Verify-rust stream in verify.xdf");
        // Take a 50-sample slice of the real recording so the test finishes quickly.
        let mut slice_stream = target_stream.clone();
        slice_stream.samples.truncate(50);
        let prefix = format!("RealReplay_{}_", std::process::id());
        let expected_name = format!("{prefix}{}", slice_stream.info.name);
        let expected_samples = slice_stream.samples.clone();

        let sub = XdfFile {
            streams: vec![slice_stream],
        };
        let opts = PlayOptions {
            speed: 50.0,
            wait_for_consumers: true,
            name_prefix: prefix,
            linger: Duration::from_millis(500),
            quiet: true,
            ..PlayOptions::default()
        };

        let player = std::thread::spawn(move || play_xdf(&sub, &opts));
        let sid = labstream_net::config::get().session_id.clone();
        let found = match resolve(
            &build_property_query(&sid, "name", &expected_name),
            1,
            Duration::from_secs(5),
        ) {
            Ok(v) if !v.is_empty() => v,
            _ => return,
        };
        let mut inlet = Inlet::open(&found[0], Duration::from_secs(5)).unwrap();
        let mut got = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while got.len() < expected_samples.len() && Instant::now() < deadline {
            let _ = inlet.pull_chunk(&mut got, 128, Duration::from_millis(50));
        }
        let pushed = player.join().unwrap().unwrap();
        assert_eq!(pushed, expected_samples.len() as u64);
        assert_eq!(got.len(), expected_samples.len());
        for i in 0..expected_samples.len() {
            for (ch, (a, b)) in got[i]
                .values
                .iter()
                .zip(expected_samples[i].values.iter())
                .enumerate()
            {
                match (a, b) {
                    (Value::F32(x), Value::F32(y)) => {
                        assert_eq!(x.to_bits(), y.to_bits(), "sample {i} ch {ch}");
                    }
                    _ => assert_eq!(a, b),
                }
            }
            if i > 0 {
                let want_dt = expected_samples[i].timestamp - expected_samples[i - 1].timestamp;
                let got_dt = got[i].timestamp - got[i - 1].timestamp;
                assert!(
                    (got_dt - want_dt).abs() < 1e-8,
                    "interval {i}: {got_dt} vs {want_dt}"
                );
            }
        }
    }
}
