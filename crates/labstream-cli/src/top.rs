//! `labstream top`: live terminal monitor of active LSL streams using the
//! step 11 health API (`Inlet::health`).
//!
//! `docs/tools.md` and `docs/roadmap.md` (step 12).

use labstream_net::{ContinuousResolver, Inlet, InletHealth, StreamInfo};
use labstream_wire::Sample;
use std::collections::{HashMap, HashSet};
use std::io::{self, IsTerminal, Write};
use std::process::ExitCode;
use std::time::{Duration, Instant};

/// Configuration for `labstream top`.
#[derive(Debug, Clone)]
pub struct TopOptions {
    /// Update period (default: 1.0 s).
    pub interval: Duration,
    /// Optional limit on the number of snapshots before exiting.
    pub count: Option<u64>,
    /// Optional session override; defaults to `config::get().session_id`.
    pub session: Option<String>,
    /// Suppress ANSI screen-clear escape codes.
    pub no_clear: bool,
    /// Write one JSON object per update instead of an aligned text table.
    pub json: bool,
}

impl Default for TopOptions {
    fn default() -> Self {
        TopOptions {
            interval: Duration::from_secs(1),
            count: None,
            session: None,
            no_clear: false,
            json: false,
        }
    }
}

pub fn top_usage() -> &'static str {
    "Usage:
  labstream top [OPTIONS]

Options:
  --interval SECONDS  Update interval in seconds (default: 1.0).
  --count N           Stop after N snapshots (default: run until interrupted).
  --session ID        Monitor streams in session ID instead of the default.
  --no-clear          Do not clear the terminal between updates.
  --json              Emit one JSON object per update tick.
  -h, --help          Show this help text."
}

pub fn parse_top_args(args: &[String]) -> Result<(TopOptions, bool), String> {
    let mut opts = TopOptions::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => return Ok((opts, true)),
            "--no-clear" => opts.no_clear = true,
            "--json" => opts.json = true,
            "--interval" => {
                i += 1;
                let v = args
                    .get(i)
                    .ok_or_else(|| "missing value after --interval".to_string())?;
                opts.interval = parse_interval(v)?;
            }
            "--count" => {
                i += 1;
                let v = args
                    .get(i)
                    .ok_or_else(|| "missing value after --count".to_string())?;
                opts.count = Some(parse_count(v)?);
            }
            "--session" => {
                i += 1;
                let v = args
                    .get(i)
                    .ok_or_else(|| "missing value after --session".to_string())?;
                opts.session = Some(v.clone());
            }
            other => {
                if let Some(v) = other.strip_prefix("--interval=") {
                    opts.interval = parse_interval(v)?;
                } else if let Some(v) = other.strip_prefix("--count=") {
                    opts.count = Some(parse_count(v)?);
                } else if let Some(v) = other.strip_prefix("--session=") {
                    opts.session = Some(v.to_string());
                } else {
                    return Err(format!("unknown argument: {other}"));
                }
            }
        }
        i += 1;
    }
    Ok((opts, false))
}

fn parse_interval(text: &str) -> Result<Duration, String> {
    let secs: f64 = text
        .parse()
        .map_err(|_| format!("invalid --interval value: {text}"))?;
    if !secs.is_finite() || secs <= 0.0 {
        return Err(format!("--interval must be a positive number: {text}"));
    }
    Ok(Duration::from_secs_f64(secs))
}

fn parse_count(text: &str) -> Result<u64, String> {
    let n: u64 = text
        .parse()
        .map_err(|_| format!("invalid --count value: {text}"))?;
    if n == 0 {
        return Err("--count must be at least 1".to_string());
    }
    Ok(n)
}

/// Health snapshot of one stream row displayed by `labstream top`.
#[derive(Debug, Clone)]
pub struct StreamRow {
    /// Stream instance identifier.
    pub uid: String,
    /// Stream name.
    pub name: String,
    /// Stream content type.
    pub stream_type: String,
    /// Hostname of the source machine.
    pub hostname: String,
    /// Channel count.
    pub channels: u32,
    /// Nominal sampling rate in Hz (0 for irregular).
    pub nominal_srate: f64,
    /// Observed arrival rate in Hz over the last tick interval.
    pub measured_srate: f64,
    /// Exact snapshot returned by [`Inlet::health`].
    pub health: InletHealth,
}

/// One full snapshot produced by [`TopMonitor::tick`].
#[derive(Debug, Clone)]
pub struct TopSnapshot {
    /// Session queried.
    pub session_id: String,
    /// Monotonic tick counter starting at 1.
    pub tick: u64,
    /// Streams sorted by name then UID.
    pub streams: Vec<StreamRow>,
}

struct Tracked {
    info: StreamInfo,
    inlet: Inlet,
    last_tick: Instant,
    scratch: Vec<Sample>,
}

/// Live stream monitor backed by `ContinuousResolver` and `Inlet::health`.
pub struct TopMonitor {
    session_id: String,
    resolver: ContinuousResolver,
    tracked: HashMap<String, Tracked>,
    ticks: u64,
}

impl TopMonitor {
    /// Start discovering streams in `session_id`.
    pub fn new(session_id: String) -> Self {
        let query = format!("session_id='{session_id}'");
        let resolver = ContinuousResolver::new(&query, 5.0);
        TopMonitor {
            session_id,
            resolver,
            tracked: HashMap::new(),
            ticks: 0,
        }
    }

    /// Discover new streams, drain samples to measure live rate, and read
    /// `Inlet::health` for every active stream.
    pub fn tick(&mut self) -> TopSnapshot {
        self.ticks += 1;
        let now = Instant::now();
        let visible = self.resolver.results(64);
        let visible_uids: HashSet<String> = visible.iter().map(|i| i.uid.clone()).collect();

        for info in visible {
            if self.tracked.contains_key(&info.uid) {
                continue;
            }
            let buf_samples = if info.nominal_srate > 0.0 {
                ((info.nominal_srate * 4.0).ceil() as i32).clamp(256, 65_536)
            } else {
                1024
            };
            if let Ok(inlet) =
                Inlet::open_recovering(&info, Duration::from_millis(800), buf_samples)
            {
                self.tracked.insert(
                    info.uid.clone(),
                    Tracked {
                        info,
                        inlet,
                        last_tick: now,
                        scratch: Vec::new(),
                    },
                );
            }
        }

        // Remove streams that ended and are no longer announced on the network.
        self.tracked
            .retain(|uid, t| !t.inlet.is_finished() || visible_uids.contains(uid));

        let mut rows = Vec::with_capacity(self.tracked.len());
        for (uid, t) in &mut self.tracked {
            t.scratch.clear();
            let got = t
                .inlet
                .pull_chunk(&mut t.scratch, 65_536, Duration::ZERO)
                .unwrap_or(0);
            let dt = now.saturating_duration_since(t.last_tick).as_secs_f64();
            t.last_tick = now;
            let measured_srate = if dt > 0.0 { got as f64 / dt } else { 0.0 };
            let health = t.inlet.health();
            rows.push(StreamRow {
                uid: uid.clone(),
                name: t.info.name.clone(),
                stream_type: t.info.stream_type.clone(),
                hostname: t.info.hostname.clone(),
                channels: t.info.channel_count,
                nominal_srate: t.info.nominal_srate,
                measured_srate,
                health,
            });
        }

        rows.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.uid.cmp(&b.uid)));

        TopSnapshot {
            session_id: self.session_id.clone(),
            tick: self.ticks,
            streams: rows,
        }
    }

    /// Borrow the underlying [`Inlet`] for a tracked stream UID (used by tests
    /// to compare `TopSnapshot` values directly against `Inlet::health`).
    #[cfg(test)]
    pub(crate) fn inlet(&self, uid: &str) -> Option<&Inlet> {
        self.tracked.get(uid).map(|t| &t.inlet)
    }
}

/// Format a [`TopSnapshot`] as an aligned terminal table.
pub fn format_table(snap: &TopSnapshot) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "labstream top  session={}  streams={}  update=#{}\n",
        snap.session_id,
        snap.streams.len(),
        snap.tick
    ));
    if snap.streams.is_empty() {
        out.push_str("  (no streams discovered yet)\n");
        return out;
    }
    out.push_str(&format!(
        "{:<22} {:<10} {:<12} {:>3} {:>9} {:<4} {:>8} {:>6} {:>6} {:>6} {:>4} {:>4}\n",
        "NAME",
        "TYPE",
        "HOST",
        "CH",
        "RATE Hz",
        "PATH",
        "LAT ms",
        "LOST",
        "DROP",
        "RESENT",
        "REC",
        "GAPS"
    ));
    for r in &snap.streams {
        let name = truncate(&r.name, 22);
        let stype = truncate(&r.stream_type, 10);
        let host = truncate(&r.hostname, 12);
        let path = if !r.health.connected {
            "DOWN"
        } else if r.health.on_shm {
            "SHM"
        } else if r.health.on_udp {
            "UDP"
        } else {
            "TCP"
        };
        let lat = match r.health.latency {
            Some(s) => format!("{:.1}", s * 1000.0),
            None => "-".to_string(),
        };
        let lost = match r.health.lost {
            Some(n) => n.to_string(),
            None => "-".to_string(),
        };
        let resent = match r.health.resent {
            Some(n) => n.to_string(),
            None => "-".to_string(),
        };
        out.push_str(&format!(
            "{:<22} {:<10} {:<12} {:>3} {:>9.1} {:<4} {:>8} {:>6} {:>6} {:>6} {:>4} {:>4}\n",
            name,
            stype,
            host,
            r.channels,
            r.measured_srate,
            path,
            lat,
            lost,
            r.health.dropped,
            resent,
            r.health.recoveries,
            r.health.gap_count
        ));
    }
    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else if max > 1 {
        let mut t: String = s.chars().take(max - 1).collect();
        t.push('~');
        t
    } else {
        s.chars().take(max).collect()
    }
}

/// Encode a [`TopSnapshot`] as a single-line JSON object.
pub fn to_json(snap: &TopSnapshot) -> String {
    let mut s = String::new();
    s.push('{');
    push_kv_str(&mut s, "session_id", &snap.session_id, true);
    s.push_str(&format!(",\"tick\":{},\"streams\":[", snap.tick));
    for (i, r) in snap.streams.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push('{');
        push_kv_str(&mut s, "uid", &r.uid, true);
        push_kv_str(&mut s, "name", &r.name, false);
        push_kv_str(&mut s, "type", &r.stream_type, false);
        push_kv_str(&mut s, "hostname", &r.hostname, false);
        s.push_str(&format!(
            ",\"channels\":{},\"nominal_srate\":{},\"measured_srate\":{:.3}",
            r.channels, r.nominal_srate, r.measured_srate
        ));
        push_opt_f64(&mut s, "latency_s", r.health.latency);
        push_opt_f64(&mut s, "clock_uncertainty_s", r.health.clock_uncertainty);
        push_opt_u64(&mut s, "lost", r.health.lost);
        s.push_str(&format!(",\"dropped\":{}", r.health.dropped));
        push_opt_u64(&mut s, "resent", r.health.resent);
        s.push_str(&format!(
            ",\"recoveries\":{},\"connected\":{},\"on_udp\":{},\"on_shm\":{},\"compressed\":{},\"buffered\":{},\"buffer_capacity\":{},\"gap_count\":{}",
            r.health.recoveries,
            r.health.connected,
            r.health.on_udp,
            r.health.on_shm,
            r.health.compressed,
            r.health.buffered,
            r.health.buffer_capacity,
            r.health.gap_count
        ));
        push_opt_f64(&mut s, "compression_ratio", r.health.compression_ratio);
        s.push('}');
    }
    s.push_str("]}");
    s
}

fn push_opt_f64(out: &mut String, key: &str, val: Option<f64>) {
    out.push(',');
    push_json_str(out, key);
    out.push(':');
    match val {
        Some(v) if v.is_finite() => out.push_str(&format!("{v:.6}")),
        _ => out.push_str("null"),
    }
}

fn push_opt_u64(out: &mut String, key: &str, val: Option<u64>) {
    out.push(',');
    push_json_str(out, key);
    out.push(':');
    match val {
        Some(v) => out.push_str(&v.to_string()),
        None => out.push_str("null"),
    }
}

fn push_kv_str(out: &mut String, key: &str, val: &str, first: bool) {
    if !first {
        out.push(',');
    }
    push_json_str(out, key);
    out.push(':');
    push_json_str(out, val);
}

fn push_json_str(out: &mut String, text: &str) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

pub fn run_top(args: &[String]) -> ExitCode {
    let (opts, help) = match parse_top_args(args) {
        Ok(x) => x,
        Err(err) => {
            eprintln!("error: {err}\n\n{}", top_usage());
            return ExitCode::from(2);
        }
    };
    if help {
        println!("{}", top_usage());
        return ExitCode::SUCCESS;
    }

    let session_id = opts
        .session
        .unwrap_or_else(|| labstream_net::config::get().session_id.clone());
    let mut monitor = TopMonitor::new(session_id);
    let clear = !opts.json && !opts.no_clear && io::stdout().is_terminal();

    // Give the resolver one short initial wave so tick 1 sees local streams.
    std::thread::sleep(Duration::from_millis(250));

    let mut stdout = io::stdout().lock();
    let mut iteration = 0u64;
    let mut next_tick = Instant::now();
    loop {
        let snap = monitor.tick();
        let write_res = if opts.json {
            writeln!(stdout, "{}", to_json(&snap))
        } else {
            if clear {
                let _ = write!(stdout, "\x1b[2J\x1b[H");
            }
            write!(stdout, "{}", format_table(&snap))
        };
        if write_res.is_err() || stdout.flush().is_err() {
            return ExitCode::SUCCESS;
        }
        iteration += 1;
        if let Some(limit) = opts.count {
            if iteration >= limit {
                return ExitCode::SUCCESS;
            }
        }
        next_tick += opts.interval;
        let now = Instant::now();
        if next_tick > now {
            std::thread::sleep(next_tick - now);
        } else {
            next_tick = now;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use labstream_net::{clock, Outlet};
    use labstream_wire::{Format, Value};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    #[test]
    fn parse_top_accepts_flags_and_rejects_zero_count() {
        let args: Vec<String> = [
            "--interval",
            "0.5",
            "--count",
            "3",
            "--session",
            "lab1",
            "--no-clear",
            "--json",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let (opts, help) = parse_top_args(&args).unwrap();
        assert!(!help);
        assert_eq!(opts.interval, Duration::from_millis(500));
        assert_eq!(opts.count, Some(3));
        assert_eq!(opts.session.as_deref(), Some("lab1"));
        assert!(opts.no_clear);
        assert!(opts.json);

        let bad = vec!["--count".to_string(), "0".to_string()];
        assert!(parse_top_args(&bad).is_err());
    }

    #[test]
    fn top_monitor_matches_inlet_health_and_updates_every_second() {
        let pid = std::process::id();
        let name = format!("TopLive_{pid}");
        let session = format!("top-test-{pid}");
        let mut info = StreamInfo::new(&name, "EEG", 2, Format::Float32, 100.0);
        info.session_id = session.clone();
        let outlet = match Outlet::new(info) {
            Ok(o) => Arc::new(o),
            Err(e) => {
                eprintln!("skipping live top test (no local network): {e}");
                return;
            }
        };
        let target_uid = outlet.info().uid.clone();

        // Push at ~100 Hz in the background so `top` measures rate and latency.
        let stop = Arc::new(AtomicBool::new(false));
        let pusher = {
            let outlet = Arc::clone(&outlet);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    outlet.push(&Sample {
                        timestamp: clock(),
                        values: vec![Value::F32(1.0), Value::F32(2.0)],
                    });
                    std::thread::sleep(Duration::from_millis(10));
                }
            })
        };

        let mut monitor = TopMonitor::new(session);

        // Wait until the stream is discovered and attached.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let snap = monitor.tick();
            if snap.streams.iter().any(|r| r.uid == target_uid) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }

        // Wait 1.0 s (the default `labstream top` cadence) and take a snapshot.
        let t0 = Instant::now();
        std::thread::sleep(Duration::from_secs(1));
        let snap = monitor.tick();
        let elapsed = t0.elapsed();
        assert!(elapsed >= Duration::from_millis(950));

        let row = snap
            .streams
            .iter()
            .find(|r| r.uid == target_uid)
            .expect("expected live outlet in TopSnapshot");
        let direct = monitor.inlet(&target_uid).expect("tracked inlet").health();

        // `top` must show the exact same values as the Health API.
        assert_eq!(row.health.lost, direct.lost);
        assert_eq!(row.health.dropped, direct.dropped);
        assert_eq!(row.health.resent, direct.resent);
        assert_eq!(row.health.recoveries, direct.recoveries);
        assert_eq!(row.health.connected, direct.connected);
        assert_eq!(row.health.on_udp, direct.on_udp);
        assert_eq!(row.health.gap_count, direct.gap_count);
        assert!(row.health.connected);
        assert!(
            row.measured_srate > 20.0,
            "expected live measured rate, got {}",
            row.measured_srate
        );
        assert!(row.health.latency.is_some(), "expected measured latency");

        let table = format_table(&snap);
        assert!(table.contains(&name), "table missing stream:\n{table}");
        let json = to_json(&snap);
        assert!(json.starts_with('{') && json.ends_with('}'));
        assert!(json.contains(&name));

        stop.store(true, Ordering::Relaxed);
        let _ = pusher.join();
    }
}
