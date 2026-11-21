//! Two-machine Wi-Fi test for `dgram/1` and TCP. `docs/extensions.md`,
//! section `dgram`, conformance item 13.
//!
//! Runs on Linux, Windows, and macOS with no C toolchain. One machine runs
//! `outlet`, and the second machine runs `inlet`. Each side runs the same
//! list of loads and writes a summary table at the end.
//!
//! Usage:
//!   wifitest outlet [--mode udp|tcp] [--peer IP] [--loads LIST] [--seconds S] [--name PREFIX]
//!   wifitest inlet  [--mode udp|tcp] [--peer IP] [--loads LIST] [--seconds S] [--name PREFIX]
//!
//! Default loads: `512x8x1,1000x32x1,4000x64x32`.

use labstream_net::{build_property_query, clock, resolve, Inlet, Outlet, StreamInfo};
use labstream_wire::{Format, Sample, Value};
use std::path::PathBuf;
use std::time::{Duration, Instant};

const DEFAULT_LOADS: &str = "512x8x1,1000x32x1,4000x64x32";
const WARMUP_SECS: f64 = 2.0;

#[derive(Clone, Copy)]
struct Load {
    rate: f64,
    channels: usize,
    chunk: usize,
}

impl Load {
    fn parse(text: &str) -> Result<Load, String> {
        let parts: Vec<&str> = text.split('x').collect();
        if parts.len() != 3 {
            return Err(format!("bad load {text}: expected RATExCHANNELSxCHUNK"));
        }
        let rate: f64 = parts[0]
            .parse()
            .map_err(|_| format!("bad rate in {text}"))?;
        let channels: usize = parts[1]
            .parse()
            .map_err(|_| format!("bad channels in {text}"))?;
        let chunk: usize = parts[2]
            .parse()
            .map_err(|_| format!("bad chunk in {text}"))?;
        if rate <= 0.0 || channels == 0 || chunk == 0 {
            return Err(format!("non-positive value in {text}"));
        }
        Ok(Load {
            rate,
            channels,
            chunk,
        })
    }

    fn label(self) -> String {
        format!("{}x{}x{}", self.rate as u64, self.channels, self.chunk)
    }
}

struct Args {
    role: String,
    mode: String,
    peers: Vec<String>,
    loads: Vec<Load>,
    seconds: f64,
    prefix: String,
    timeout: f64,
}

fn parse_args() -> Result<Args, String> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    if raw.is_empty() || raw[0] == "-h" || raw[0] == "--help" {
        return Err(usage());
    }
    let role = raw[0].clone();
    if role != "outlet" && role != "inlet" {
        return Err(format!(
            "first argument must be outlet or inlet\n{}",
            usage()
        ));
    }
    let mut mode = "udp".to_string();
    let mut peers: Vec<String> = Vec::new();
    let mut loads_text = DEFAULT_LOADS.to_string();
    let mut seconds = 200.0;
    let mut prefix = "Wifi".to_string();
    let mut timeout = 15.0;

    let mut i = 1;
    while i < raw.len() {
        let flag = raw[i].as_str();
        let next = |idx: usize| -> Result<String, String> {
            raw.get(idx + 1)
                .cloned()
                .ok_or_else(|| format!("missing value for {flag}"))
        };
        match flag {
            "--mode" => {
                mode = next(i)?;
                i += 2;
            }
            "--peer" => {
                for p in next(i)?.split(',') {
                    let p = p.trim();
                    if !p.is_empty() {
                        peers.push(p.to_string());
                    }
                }
                i += 2;
            }
            "--loads" => {
                loads_text = next(i)?;
                i += 2;
            }
            "--seconds" => {
                seconds = next(i)?.parse().map_err(|_| "bad --seconds".to_string())?;
                i += 2;
            }
            "--name" => {
                prefix = next(i)?;
                i += 2;
            }
            "--timeout" => {
                timeout = next(i)?.parse().map_err(|_| "bad --timeout".to_string())?;
                i += 2;
            }
            other => return Err(format!("unknown option {other}\n{}", usage())),
        }
    }
    if mode != "udp" && mode != "tcp" {
        return Err(format!("--mode must be udp or tcp, not {mode}"));
    }
    if seconds <= WARMUP_SECS {
        return Err(format!("--seconds must be more than {WARMUP_SECS}"));
    }
    let mut loads = Vec::new();
    for item in loads_text.split(',') {
        let item = item.trim();
        if !item.is_empty() {
            loads.push(Load::parse(item)?);
        }
    }
    if loads.is_empty() {
        return Err("no loads given".to_string());
    }
    Ok(Args {
        role,
        mode,
        peers,
        loads,
        seconds,
        prefix,
        timeout,
    })
}

fn usage() -> String {
    format!(
        "usage: wifitest outlet|inlet [--mode udp|tcp] [--peer IP[,IP]] [--loads {DEFAULT_LOADS}] [--seconds 200] [--name Wifi] [--timeout 15]"
    )
}

/// Write a configuration file and set `LSLAPICFG` before any network call.
fn write_config(mode: &str, peers: &[String]) -> Result<PathBuf, String> {
    let features = if mode == "udp" {
        "{seq, resume, dgram}"
    } else {
        "{seq, resume}"
    };
    let mut text = format!("[labstream]\nFeatures = {features}\n");
    if !peers.is_empty() {
        text.push_str(&format!("[lab]\nKnownPeers = {{{}}}\n", peers.join(", ")));
    }
    let path = std::env::temp_dir().join(format!("labstream-wifitest-{}.cfg", std::process::id()));
    std::fs::write(&path, &text).map_err(|e| format!("cannot write config: {e}"))?;
    std::env::set_var("LSLAPICFG", &path);
    eprintln!("config ({}):\n{}", path.display(), text.trim_end());
    Ok(path)
}

/// Sleep until `target`, with a short spin at the end for Windows timers.
fn wait_until(target: Instant) {
    loop {
        let now = Instant::now();
        if now >= target {
            return;
        }
        let left = target.duration_since(now);
        if left > Duration::from_millis(3) {
            std::thread::sleep(left - Duration::from_millis(2));
        } else {
            std::hint::spin_loop();
        }
    }
}

fn run_outlet(args: &Args) -> Result<(), String> {
    for load in &args.loads {
        let name = format!("{}_{}_{}", args.prefix, args.mode, load.label());
        let mut info = StreamInfo::new(
            &name,
            "WifiTest",
            load.channels as u32,
            Format::Float32,
            load.rate,
        );
        info.source_id = name.clone();
        let out = Outlet::new(info).map_err(|e| format!("outlet {name}: {e}"))?;
        eprintln!(
            "=== outlet {name} ({} Hz x {} ch, chunk {}, {} s): wait for inlet ===",
            load.rate, load.channels, load.chunk, args.seconds
        );
        if !out.wait_for_consumers(Duration::from_secs(120)) {
            return Err(format!("outlet {name}: no inlet in 120 s"));
        }
        eprintln!("inlet connected to {name}");
        // Give the inlet time to start its pull loop and send its UDP hello.
        std::thread::sleep(Duration::from_millis(500));

        let total = (load.rate * args.seconds) as usize;
        let t0 = Instant::now();
        let mut k = 0usize;
        let mut next_report = Duration::from_secs(10);
        while k < total {
            let n = (total - k).min(load.chunk);
            let due = t0 + Duration::from_secs_f64((k + n) as f64 / load.rate);
            wait_until(due);
            let ts = clock();
            if n == 1 {
                let mut values = Vec::with_capacity(load.channels);
                values.push(Value::F32(k as f32));
                for c in 1..load.channels {
                    values.push(Value::F32(c as f32));
                }
                out.push(&Sample {
                    timestamp: ts,
                    values,
                });
            } else {
                let mut block = Vec::with_capacity(n);
                for i in 0..n {
                    let mut values = Vec::with_capacity(load.channels);
                    values.push(Value::F32((k + i) as f32));
                    for c in 1..load.channels {
                        values.push(Value::F32(c as f32));
                    }
                    block.push(Sample {
                        timestamp: ts,
                        values,
                    });
                }
                out.push_samples(block);
            }
            k += n;
            let elapsed = t0.elapsed();
            if elapsed >= next_report {
                eprintln!(
                    "  [{:.0}s/{:.0}s] sent {k}/{total}, consumers {}",
                    elapsed.as_secs_f64(),
                    args.seconds,
                    out.consumer_count()
                );
                next_report += Duration::from_secs(10);
            }
        }
        eprintln!(
            "done {name}: sent {total} in {:.2} s (wait 2 s for tail resends)",
            t0.elapsed().as_secs_f64()
        );
        std::thread::sleep(Duration::from_secs(2));
        drop(out);
        std::thread::sleep(Duration::from_secs(1));
    }
    Ok(())
}

struct Row {
    load: String,
    mode: String,
    expected: usize,
    received: usize,
    lost: usize,
    repeated: usize,
    reordered: usize,
    seq_lost: Option<u64>,
    resent: Option<u64>,
    dropped: u64,
    recoveries: u64,
    udp_pct: f64,
    j99_ms: f64,
    j999_ms: f64,
    jmax_ms: f64,
}

fn quantile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let idx = ((q * (sorted.len() - 1) as f64) + 0.5) as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn run_inlet(args: &Args) -> Result<(), String> {
    let mut rows = Vec::new();
    for load in &args.loads {
        let name = format!("{}_{}_{}", args.prefix, args.mode, load.label());
        eprintln!(
            "=== inlet {name} ({} Hz x {} ch, chunk {}, {} s): resolve ===",
            load.rate, load.channels, load.chunk, args.seconds
        );
        let query = build_property_query("default", "name", &name);
        let found =
            resolve(&query, 1, Duration::from_secs(120)).map_err(|e| format!("resolve: {e}"))?;
        let info = found
            .into_iter()
            .next()
            .ok_or_else(|| format!("stream {name} not found in 120 s"))?;
        eprintln!("found {name} at {} ({})", info.hostname, info.uid);

        let total = (load.rate * args.seconds) as usize;
        let skip = (load.rate * WARMUP_SECS) as usize;
        let mut inlet = Inlet::open_recovering(&info, Duration::from_secs(20), 200_000)
            .map_err(|e| format!("open {name}: {e}"))?;

        let mut seen = vec![false; total];
        let mut delays: Vec<f64> = Vec::with_capacity(total);
        let mut got = 0usize;
        let mut repeated = 0usize;
        let mut reordered = 0usize;
        let mut last: Option<usize> = None;
        let mut udp_ticks = 0u64;
        let mut total_ticks = 0u64;
        let pull_timeout = Duration::from_secs_f64(args.timeout);
        let t0 = Instant::now();
        let mut next_report = Duration::from_secs(10);

        loop {
            let sample = match inlet.pull(pull_timeout) {
                Ok(Some(s)) => s,
                Ok(None) => {
                    eprintln!("  timeout after {} s with no sample", args.timeout);
                    break;
                }
                Err(e) => {
                    eprintln!("  pull error: {e}");
                    break;
                }
            };
            let t = clock();
            got += 1;
            let idx = match sample.values.first() {
                Some(Value::F32(v)) if *v >= 0.0 => *v as usize,
                _ => continue,
            };
            if idx >= total {
                continue;
            }
            if seen[idx] {
                repeated += 1;
                continue;
            }
            seen[idx] = true;
            if let Some(prev) = last {
                if idx < prev {
                    reordered += 1;
                }
            }
            last = Some(idx);
            if idx >= skip {
                delays.push(t - sample.timestamp);
            }
            if got % 256 == 0 {
                total_ticks += 1;
                if inlet.on_udp() {
                    udp_ticks += 1;
                }
            }
            let elapsed = t0.elapsed();
            if elapsed >= next_report {
                let udp_now = inlet.on_udp();
                eprintln!(
                    "  [{:.0}s/{:.0}s] got {got}/{total}, seq_lost {:?}, resent {:?}, rec {}, on_udp {udp_now}",
                    elapsed.as_secs_f64(),
                    args.seconds,
                    inlet.samples_lost(),
                    inlet.samples_resent(),
                    inlet.recoveries(),
                );
                next_report += Duration::from_secs(10);
            }
            if idx == total - 1 {
                break;
            }
        }

        let lost = seen.iter().filter(|&&s| !s).count();
        delays.sort_by(|a, b| a.total_cmp(b));
        let p50 = quantile(&delays, 0.50);
        let j99_ms = (quantile(&delays, 0.99) - p50) * 1e3;
        let j999_ms = (quantile(&delays, 0.999) - p50) * 1e3;
        let jmax_ms = (delays.last().copied().unwrap_or(f64::NAN) - p50) * 1e3;
        let udp_pct = if total_ticks > 0 {
            100.0 * udp_ticks as f64 / total_ticks as f64
        } else if inlet.on_udp() {
            100.0
        } else {
            0.0
        };
        let row = Row {
            load: load.label(),
            mode: args.mode.clone(),
            expected: total,
            received: got,
            lost,
            repeated,
            reordered,
            seq_lost: inlet.samples_lost(),
            resent: inlet.samples_resent(),
            dropped: inlet.samples_dropped(),
            recoveries: inlet.recoveries(),
            udp_pct,
            j99_ms,
            j999_ms,
            jmax_ms,
        };
        eprintln!(
            "result {name}: lost {}, seq_lost {:?}, resent {:?}, rec {}, on_udp {:.0}%, j99 {:.1} ms, jmax {:.1} ms",
            row.lost, row.seq_lost, row.resent, row.recoveries, row.udp_pct, row.j99_ms, row.jmax_ms
        );
        rows.push(row);
    }

    println!();
    println!("| mode | load | expected | received | lost | rep | reord | seq_lost | resent | dropped | rec | on_udp % | p99-p50 ms | p99.9-p50 ms | max-p50 ms |");
    println!("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|");
    for r in &rows {
        println!(
            "| {} | {} | {} | {} | {} | {} | {} | {:?} | {:?} | {} | {} | {:.0} | {:.1} | {:.1} | {:.1} |",
            r.mode,
            r.load,
            r.expected,
            r.received,
            r.lost,
            r.repeated,
            r.reordered,
            r.seq_lost,
            r.resent,
            r.dropped,
            r.recoveries,
            r.udp_pct,
            r.j99_ms,
            r.j999_ms,
            r.jmax_ms
        );
    }
    Ok(())
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    let cfg = match write_config(&args.mode, &args.peers) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    let res = if args.role == "outlet" {
        run_outlet(&args)
    } else {
        run_inlet(&args)
    };
    let _ = std::fs::remove_file(&cfg);
    if let Err(e) = res {
        eprintln!("ERROR: {e}");
        std::process::exit(1);
    }
}
