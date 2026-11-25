//! Demo 3: Bandwidth Squeeze (`comp/1` lossless compression + `select/1` outlet-side
//! channel selection and anti-aliased FIR decimation) on a 128-channel 1000 Hz EEG stream.

use labstream::{Buffer, Format, Inlet, Outlet, Query, StreamInfo};
use std::collections::VecDeque;
use std::f64::consts::TAU;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const STREAM_NAME: &str = "Demo3HD_EEG";
const CHANNELS: usize = 128;
const SRATE: f64 = 1000.0;
const CHUNK_SAMPLES: usize = 20; // 50 Hz push cadence (20 ms chunks)
const BASE_TS: f64 = 1000.0;
const HISTORY_LEN: usize = 60; // 30 s at 2 Hz sampling
const WAVE_POINTS: usize = 400; // 0.4 s window

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Both,
    Sender,
    Viewer,
}

struct Args {
    mode: Mode,
    peer: Option<String>,
    port: u16,
}

fn parse_args() -> Args {
    let mut mode = Mode::Both;
    let mut peer = None;
    let mut port = 8782u16;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--mode" => {
                if let Some(v) = it.next() {
                    mode = match v.as_str() {
                        "sender" => Mode::Sender,
                        "viewer" => Mode::Viewer,
                        _ => Mode::Both,
                    };
                }
            }
            "--peer" => peer = it.next(),
            "--port" => {
                if let Some(v) = it.next() {
                    if let Ok(p) = v.parse() {
                        port = p;
                    }
                }
            }
            _ => {}
        }
    }
    Args { mode, peer, port }
}

/// Deterministic realistic EEG value (microvolts) for sample `seq` and channel `ch`.
///
/// Uses a realistic monopolar electrode DC baseline plus alpha (10 Hz), beta (22 Hz),
/// and slow delta (2.5 Hz) oscillations so `comp/1` sees representative smooth EEG
/// mantissa deltas and the viewer can verify exact IEEE-754 bit equality.
#[inline]
fn eeg_sample(seq: u64, ch: usize) -> f32 {
    let t = seq as f64 / SRATE;
    let phase = (ch as f64) * 0.13;
    let dc = 250.0 + (ch as f64) * 2.5;
    let alpha = 24.0 * (TAU * 10.0 * t + phase).sin();
    let beta = 7.5 * (TAU * 22.0 * t + phase * 1.7).sin();
    let delta = 11.0 * (TAU * 2.5 * t + phase * 0.4).sin();
    // Quantize to 0.05 uV steps (24-bit ADC resolution in microvolts).
    let uv = ((dc + alpha + beta + delta) * 20.0).round() * 0.05;
    uv as f32
}

struct SelectTarget {
    channels: AtomicUsize,
    decimate: AtomicUsize,
    generation: AtomicUsize,
}

#[derive(Clone, Copy)]
struct HistoryPoint {
    raw_mbps: f64,
    comp_mbps: f64,
    sel_mbps: f64,
}

struct SharedState {
    // Full 128-ch comp/1 inlet metrics
    full_connected: bool,
    full_compressed: bool,
    full_ratio: f64,
    full_samples: u64,
    full_rate_hz: f64,
    raw_mbps: f64,
    comp_mbps: f64,
    bit_checked: u64,
    bit_errors: u64,

    // Selected comp/1 + select/1 inlet metrics
    sel_connected: bool,
    sel_compressed: bool,
    sel_on_udp: bool,
    sel_channels: usize,
    sel_decimate: usize,
    sel_ratio: f64,
    sel_samples: u64,
    sel_rate_hz: f64,
    sel_mbps: f64,

    // Recent AC-coupled Ch 0 waveforms: (time_sec, microvolts)
    wave_full: VecDeque<(f64, f32)>,
    wave_sel: VecDeque<(f64, f32)>,

    // 30-second throughput history
    history: VecDeque<HistoryPoint>,
}

impl Default for SharedState {
    fn default() -> Self {
        Self {
            full_connected: false,
            full_compressed: false,
            full_ratio: 1.0,
            full_samples: 0,
            full_rate_hz: 0.0,
            raw_mbps: 0.0,
            comp_mbps: 0.0,
            bit_checked: 0,
            bit_errors: 0,
            sel_connected: false,
            sel_compressed: false,
            sel_on_udp: false,
            sel_channels: 8,
            sel_decimate: 4,
            sel_ratio: 1.0,
            sel_samples: 0,
            sel_rate_hz: 0.0,
            sel_mbps: 0.0,
            wave_full: VecDeque::with_capacity(WAVE_POINTS + 32),
            wave_sel: VecDeque::with_capacity(WAVE_POINTS + 32),
            history: VecDeque::with_capacity(HISTORY_LEN + 4),
        }
    }
}

fn run_sender(stop: Arc<AtomicBool>) {
    let labels: Vec<String> = (1..=CHANNELS).map(|i| format!("EEG{i:03}")).collect();
    let info = StreamInfo::builder(STREAM_NAME, "EEG", Format::Float32)
        .rate(SRATE)
        .source_id("demo3-hd-eeg-128")
        .channel_labels(labels.iter().map(String::as_str))
        .build()
        .expect("build Demo3 StreamInfo");

    let outlet = Outlet::new(info).expect("create Demo3 Outlet");
    eprintln!("[demo3-sender] streaming {STREAM_NAME} ({CHANNELS} ch @ {SRATE} Hz, Float32)");

    let mut seq: u64 = 0;
    let mut flat = vec![0.0f32; CHUNK_SAMPLES * CHANNELS];
    let t0 = Instant::now();

    while !stop.load(Ordering::Relaxed) {
        for k in 0..CHUNK_SAMPLES {
            let s = seq + k as u64;
            let row = &mut flat[k * CHANNELS..(k + 1) * CHANNELS];
            for (ch, slot) in row.iter_mut().enumerate() {
                *slot = eeg_sample(s, ch);
            }
        }
        let end_ts = BASE_TS + (seq + CHUNK_SAMPLES as u64 - 1) as f64 / SRATE;
        if let Err(e) = outlet.push_chunk(&flat, end_ts) {
            eprintln!("[demo3-sender] push error: {e}");
        }
        seq += CHUNK_SAMPLES as u64;

        let target = t0 + Duration::from_secs_f64(seq as f64 / SRATE);
        let now = Instant::now();
        if target > now {
            thread::sleep(target - now);
        }
    }
}

fn resolve_stream(stop: &AtomicBool) -> Option<StreamInfo> {
    let q = Query::name(STREAM_NAME);
    while !stop.load(Ordering::Relaxed) {
        if let Ok(Some(info)) = labstream::resolve_first(&q, Duration::from_millis(500)) {
            return Some(info);
        }
    }
    None
}

fn run_full_inlet(state: Arc<Mutex<SharedState>>, stop: Arc<AtomicBool>) {
    let Some(info) = resolve_stream(&stop) else {
        return;
    };
    let mut inlet = match Inlet::builder(&info)
        .buffer(Buffer::Seconds(4.0))
        .open(Duration::from_secs(5))
    {
        Ok(i) => i,
        Err(e) => {
            eprintln!("[demo3-full] open error: {e}");
            return;
        }
    };
    eprintln!(
        "[demo3-full] connected ({} ch @ {} Hz, comp={})",
        inlet.info().channel_count(),
        inlet.info().rate(),
        inlet.health().compressed
    );

    let mut data: Vec<f32> = Vec::with_capacity(256 * CHANNELS);
    let mut ts: Vec<f64> = Vec::with_capacity(256);
    let mut win_samples: u64 = 0;
    let mut win_start = Instant::now();
    let mut rate_ema = SRATE;

    while !stop.load(Ordering::Relaxed) {
        data.clear();
        ts.clear();
        let n = match inlet.pull_chunk_f32(&mut data, &mut ts, 256, Duration::from_millis(50)) {
            Ok(n) => n,
            Err(_) => continue,
        };
        if n == 0 {
            continue;
        }

        let mut checked = 0u64;
        let mut errs = 0u64;
        for i in 0..n {
            let t = ts[i];
            if t >= BASE_TS - 0.0005 {
                let seq = ((t - BASE_TS) * SRATE).round() as u64;
                let row = &data[i * CHANNELS..(i + 1) * CHANNELS];
                for (ch, &val) in row.iter().enumerate() {
                    checked += 1;
                    if val.to_bits() != eeg_sample(seq, ch).to_bits() {
                        errs += 1;
                    }
                }
            }
        }

        win_samples += n as u64;
        let dt = win_start.elapsed().as_secs_f64();
        if dt >= 0.25 {
            let inst_rate = win_samples as f64 / dt;
            rate_ema = rate_ema * 0.6 + inst_rate * 0.4;
            win_samples = 0;
            win_start = Instant::now();
        }

        let health = inlet.health();
        let ratio = health.compression_ratio.unwrap_or(1.0);
        // Plain LSL TCP frame: 1 byte tag + 8 byte timestamp + channels * 4 bytes.
        let bytes_per_sample = (CHANNELS * 4 + 9) as f64;
        let raw_mbps = (rate_ema * bytes_per_sample * 8.0) / 1_000_000.0;
        let comp_mbps = raw_mbps * ratio;

        let mut st = state.lock().unwrap();
        st.full_connected = health.connected;
        st.full_compressed = health.compressed;
        st.full_ratio = ratio;
        st.full_samples += n as u64;
        st.full_rate_hz = rate_ema;
        st.raw_mbps = raw_mbps;
        st.comp_mbps = comp_mbps;
        st.bit_checked += checked;
        st.bit_errors += errs;

        // Subsample waveform display by 2x (500 pts/s) to keep 0.4 s smooth and light.
        for i in (0..n).step_by(2) {
            let v_ac = data[i * CHANNELS] - 250.0;
            st.wave_full.push_back((ts[i], v_ac));
            while st.wave_full.len() > WAVE_POINTS {
                st.wave_full.pop_front();
            }
        }
    }
}

fn run_selected_inlet(
    state: Arc<Mutex<SharedState>>,
    target: Arc<SelectTarget>,
    stop: Arc<AtomicBool>,
) {
    let Some(info) = resolve_stream(&stop) else {
        return;
    };

    while !stop.load(Ordering::Relaxed) {
        let gen = target.generation.load(Ordering::Relaxed);
        let sel_ch = target.channels.load(Ordering::Relaxed).clamp(1, CHANNELS);
        let sel_dec = target.decimate.load(Ordering::Relaxed).clamp(1, 16) as u32;

        let mut builder = Inlet::builder(&info)
            .buffer(Buffer::Seconds(4.0))
            .decimate(sel_dec);
        if sel_ch < CHANNELS {
            builder = builder.channels(0..sel_ch);
        }

        let mut inlet = match builder.open(Duration::from_secs(5)) {
            Ok(i) => i,
            Err(e) => {
                eprintln!("[demo3-select] open error: {e}");
                thread::sleep(Duration::from_millis(250));
                continue;
            }
        };

        let eff_ch = inlet.info().channel_count();
        let eff_rate = SRATE / (sel_dec as f64);
        eprintln!(
            "[demo3-select] connected ({eff_ch} ch @ {eff_rate} Hz, decimate={sel_dec}x, comp={})",
            inlet.health().compressed
        );

        {
            let mut st = state.lock().unwrap();
            st.sel_channels = eff_ch;
            st.sel_decimate = sel_dec as usize;
            st.sel_samples = 0;
            st.wave_sel.clear();
        }

        let mut data: Vec<f32> = Vec::with_capacity(256 * eff_ch);
        let mut ts: Vec<f64> = Vec::with_capacity(256);
        let mut win_samples: u64 = 0;
        let mut win_start = Instant::now();
        let mut rate_ema = eff_rate;

        while !stop.load(Ordering::Relaxed) && target.generation.load(Ordering::Relaxed) == gen {
            data.clear();
            ts.clear();
            let n = match inlet.pull_chunk_f32(&mut data, &mut ts, 256, Duration::from_millis(50)) {
                Ok(n) => n,
                Err(_) => continue,
            };
            if n == 0 {
                continue;
            }

            win_samples += n as u64;
            let dt = win_start.elapsed().as_secs_f64();
            if dt >= 0.25 {
                let inst_rate = win_samples as f64 / dt;
                rate_ema = rate_ema * 0.6 + inst_rate * 0.4;
                win_samples = 0;
                win_start = Instant::now();
            }

            let health = inlet.health();
            let ratio = health.compression_ratio.unwrap_or(1.0);
            let bytes_per_sample = (eff_ch * 4 + 9) as f64;
            let sel_mbps = (rate_ema * bytes_per_sample * 8.0 / 1_000_000.0) * ratio;

            let mut st = state.lock().unwrap();
            st.sel_connected = health.connected;
            st.sel_compressed = health.compressed;
            st.sel_on_udp = health.on_udp;
            st.sel_channels = eff_ch;
            st.sel_decimate = sel_dec as usize;
            st.sel_ratio = ratio;
            st.sel_samples += n as u64;
            st.sel_rate_hz = rate_ema;
            st.sel_mbps = sel_mbps;

            let stride = if sel_dec == 1 { 2 } else { 1 };
            for i in (0..n).step_by(stride) {
                let v_ac = data[i * eff_ch] - 250.0;
                st.wave_sel.push_back((ts[i], v_ac));
                while st.wave_sel.len() > WAVE_POINTS {
                    st.wave_sel.pop_front();
                }
            }
        }
    }
}

fn run_history_sampler(state: Arc<Mutex<SharedState>>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(500));
        let mut st = state.lock().unwrap();
        if st.raw_mbps > 0.01 {
            let pt = HistoryPoint {
                raw_mbps: st.raw_mbps,
                comp_mbps: st.comp_mbps,
                sel_mbps: st.sel_mbps,
            };
            st.history.push_back(pt);
            while st.history.len() > HISTORY_LEN {
                st.history.pop_front();
            }
        }
    }
}

fn state_json(st: &SharedState) -> String {
    let comp_saved = if st.raw_mbps > 0.0 {
        ((1.0 - st.comp_mbps / st.raw_mbps) * 100.0).clamp(0.0, 99.99)
    } else {
        0.0
    };
    let sel_saved = if st.raw_mbps > 0.0 {
        ((1.0 - st.sel_mbps / st.raw_mbps) * 100.0).clamp(0.0, 99.99)
    } else {
        0.0
    };

    let mut out = String::with_capacity(8192);
    use std::fmt::Write as _;
    let _ = write!(
        out,
        "{{\"channels_total\":{CHANNELS},\"srate_total\":{SRATE:.1},\
         \"full_connected\":{},\"full_compressed\":{},\"full_ratio\":{:.4},\
         \"full_samples\":{},\"full_rate_hz\":{:.1},\
         \"raw_mbps\":{:.3},\"comp_mbps\":{:.3},\"comp_saved_pct\":{:.1},\
         \"bit_checked\":{},\"bit_errors\":{},\
         \"sel_connected\":{},\"sel_compressed\":{},\"sel_on_udp\":{},\
         \"sel_channels\":{},\"sel_decimate\":{},\"sel_ratio\":{:.4},\
         \"sel_samples\":{},\"sel_rate_hz\":{:.1},\
         \"sel_mbps\":{:.4},\"sel_saved_pct\":{:.2},\"history\":[",
        st.full_connected,
        st.full_compressed,
        st.full_ratio,
        st.full_samples,
        st.full_rate_hz,
        st.raw_mbps,
        st.comp_mbps,
        comp_saved,
        st.bit_checked,
        st.bit_errors,
        st.sel_connected,
        st.sel_compressed,
        st.sel_on_udp,
        st.sel_channels,
        st.sel_decimate,
        st.sel_ratio,
        st.sel_samples,
        st.sel_rate_hz,
        st.sel_mbps,
        sel_saved
    );
    for (i, h) in st.history.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(
            out,
            "[{:.3},{:.3},{:.4}]",
            h.raw_mbps, h.comp_mbps, h.sel_mbps
        );
    }
    out.push_str("],\"wave_full\":[");
    for (i, (t, v)) in st.wave_full.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "[{t:.4},{v:.2}]");
    }
    out.push_str("],\"wave_sel\":[");
    for (i, (t, v)) in st.wave_sel.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "[{t:.4},{v:.2}]");
    }
    out.push_str("]}");
    out
}

fn parse_query_usize(query: &str, key: &str) -> Option<usize> {
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == key {
                return v.parse().ok();
            }
        }
    }
    None
}

fn handle_http(
    mut stream: TcpStream,
    state: &Arc<Mutex<SharedState>>,
    target: &Arc<SelectTarget>,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let mut buf = [0u8; 2048];
    let n = match stream.read(&mut buf) {
        Ok(n) if n > 0 => n,
        _ => return,
    };
    let req = String::from_utf8_lossy(&buf[..n]);
    let line = req.lines().next().unwrap_or("");
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path_full = parts.next().unwrap_or("/");
    let (path, query) = path_full.split_once('?').unwrap_or((path_full, ""));

    let (status, content_type, body) = match (method, path) {
        ("GET", "/") => ("200 OK", "text/html; charset=utf-8", INDEX_HTML.to_string()),
        ("GET", "/api/state") => {
            let st = state.lock().unwrap();
            ("200 OK", "application/json", state_json(&st))
        }
        ("POST", "/api/select") => {
            if let Some(ch) = parse_query_usize(query, "channels") {
                target.channels.store(ch.clamp(1, CHANNELS), Ordering::Relaxed);
            }
            if let Some(dec) = parse_query_usize(query, "decimate") {
                target.decimate.store(dec.clamp(1, 16), Ordering::Relaxed);
            }
            target.generation.fetch_add(1, Ordering::Relaxed);
            ("200 OK", "application/json", "{\"ok\":true}".to_string())
        }
        _ => ("404 Not Found", "text/plain", "not found".to_string()),
    };

    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes());
}

fn main() {
    let args = parse_args();

    // Disable `shm` so 1-machine mode exercises real network sockets and real
    // `comp/1` + `select/1` wire encoding.
    let mut cfg = String::from("[labstream]\nFeatures = {seq, resume, dgram, select, comp}\n");
    if let Some(ref peer) = args.peer {
        cfg.push_str(&format!("\n[lab]\nKnownPeers = {{{peer}}}\n"));
    }
    labstream_net::config::set_content(&cfg);

    let stop = Arc::new(AtomicBool::new(false));

    if args.mode == Mode::Sender {
        run_sender(stop);
        return;
    }

    if args.mode == Mode::Both {
        let s = Arc::clone(&stop);
        thread::spawn(move || run_sender(s));
    }

    let state = Arc::new(Mutex::new(SharedState::default()));
    let target = Arc::new(SelectTarget {
        channels: AtomicUsize::new(8),
        decimate: AtomicUsize::new(4),
        generation: AtomicUsize::new(1),
    });

    {
        let st = Arc::clone(&state);
        let sp = Arc::clone(&stop);
        thread::spawn(move || run_full_inlet(st, sp));
    }
    {
        let st = Arc::clone(&state);
        let tg = Arc::clone(&target);
        let sp = Arc::clone(&stop);
        thread::spawn(move || run_selected_inlet(st, tg, sp));
    }
    {
        let st = Arc::clone(&state);
        let sp = Arc::clone(&stop);
        thread::spawn(move || run_history_sampler(st, sp));
    }

    let addr = format!("127.0.0.1:{}", args.port);
    let listener = TcpListener::bind(&addr).expect("bind HTTP server");
    println!("============================================================");
    println!("  labstream Demo 3 — Bandwidth Squeeze (comp/1 + select/1)");
    println!("  Open in browser:  http://{addr}/");
    println!("============================================================");

    for conn in listener.incoming().flatten() {
        let st = Arc::clone(&state);
        let tg = Arc::clone(&target);
        thread::spawn(move || handle_http(conn, &st, &tg));
    }
}

const INDEX_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>labstream Demo 3 — Bandwidth Squeeze (comp/1 + select/1)</title>
<style>
  :root {
    --bg: #0b0f17;
    --panel: #131a26;
    --border: #233046;
    --text: #e6edf7;
    --muted: #8ea0b8;
    --red: #f87171;
    --blue: #38bdf8;
    --green: #34d399;
    --gold: #fbbf24;
  }
  * { box-sizing: border-box; }
  body {
    margin: 0; padding: 20px; background: var(--bg); color: var(--text);
    font-family: ui-sans-serif, system-ui, -apple-system, Segoe UI, Roboto, sans-serif;
  }
  .wrap { max-width: 1180px; margin: 0 auto; }
  header {
    display: flex; flex-wrap: wrap; justify-content: space-between; align-items: center;
    gap: 12px; margin-bottom: 16px;
  }
  h1 { margin: 0; font-size: 21px; letter-spacing: -0.01em; }
  .sub { color: var(--muted); font-size: 13px; margin-top: 3px; }
  .toolbar { display: flex; gap: 8px; flex-wrap: wrap; align-items: center; }
  button {
    background: #1b2536; color: var(--text); border: 1px solid var(--border);
    border-radius: 7px; padding: 7px 12px; font-size: 13px; cursor: pointer;
    font-weight: 600;
  }
  button:hover { border-color: #4b6588; }
  button.active { background: #164e63; border-color: var(--blue); color: #e0f2fe; }
  button.primary { background: #065f46; border-color: var(--green); color: #ecfdf5; }
  .badge {
    display: inline-flex; align-items: center; gap: 6px; padding: 5px 10px;
    border-radius: 999px; font-size: 12px; font-weight: 700;
    border: 1px solid #14532d; background: #052e16; color: var(--green);
  }
  .badge.err { border-color: #7f1d1d; background: #450a0a; color: var(--red); }
  .grid { display: grid; grid-template-columns: 1fr; gap: 14px; }
  .card {
    background: var(--panel); border: 1px solid var(--border);
    border-radius: 12px; padding: 16px;
  }
  .card h2 { margin: 0 0 12px 0; font-size: 15px; color: #cbd5e1; display: flex; justify-content: space-between; align-items: center; flex-wrap: wrap; gap: 8px; }
  .bar-row { margin-bottom: 14px; }
  .bar-row:last-child { margin-bottom: 0; }
  .bar-head {
    display: flex; justify-content: space-between; align-items: baseline;
    font-size: 13px; margin-bottom: 6px; gap: 8px; flex-wrap: wrap;
  }
  .bar-name { font-weight: 700; }
  .bar-val { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 13px; }
  .track {
    position: relative; height: 26px; background: #0d131f;
    border: 1px solid #1e2a3d; border-radius: 6px; overflow: hidden;
  }
  .fill { height: 100%; transition: width 0.25s ease; border-radius: 5px; }
  .limit-line {
    position: absolute; top: 0; bottom: 0; width: 2px; background: var(--gold);
    z-index: 3;
  }
  .limit-tag {
    position: absolute; top: 4px; font-size: 10px; font-weight: 700;
    color: var(--gold); background: rgba(11,15,23,0.82); padding: 1px 4px;
    border-radius: 3px; z-index: 4;
  }
  .controls {
    display: flex; flex-wrap: wrap; gap: 18px; align-items: center;
    padding: 12px 14px; background: #0e1522; border-radius: 9px;
    border: 1px solid #1e293b; margin-top: 12px;
  }
  .ctrl-group { display: flex; align-items: center; gap: 6px; flex-wrap: wrap; }
  .ctrl-label { font-size: 12px; color: var(--muted); font-weight: 700; text-transform: uppercase; margin-right: 4px; }
  .two-col { display: grid; grid-template-columns: 1fr 1fr; gap: 14px; }
  @media (max-width: 860px) { .two-col { grid-template-columns: 1fr; } }
  canvas { width: 100%; height: 220px; display: block; background: #090d14; border-radius: 8px; border: 1px solid #1c2638; }
  .legend { display: flex; gap: 14px; font-size: 12px; color: var(--muted); }
  .dot { display: inline-block; width: 10px; height: 10px; border-radius: 50%; margin-right: 5px; vertical-align: middle; }
</style>
</head>
<body>
<div class="wrap">
  <header>
    <div>
      <h1>Demo 3 — Bandwidth Squeeze (128-ch @ 1000 Hz EEG)</h1>
      <div class="sub">Real-time wire throughput: Standard LSL vs <code>comp/1</code> lossless vs <code>comp/1 + select/1</code> outlet-side subset</div>
    </div>
    <div class="toolbar">
      <span id="bitBadge" class="badge">✓ LOSSLESS BIT-EXACT (0 errors)</span>
      <button id="pauseBtn">Pause</button>
      <button id="exportBtn" class="primary">Export PNG</button>
    </div>
  </header>

  <div class="grid">
    <div class="card">
      <h2>
        <span>Live Wire Throughput (1 Mbit/s link budget marked in gold)</span>
        <span id="transportTag" style="font-size:12px;color:var(--muted);font-weight:600"></span>
      </h2>

      <div class="bar-row">
        <div class="bar-head">
          <span class="bar-name" style="color:var(--red)">Standard LSL (128 ch @ 1000 Hz, uncompressed)</span>
          <span class="bar-val" id="rawVal">-- Mbit/s</span>
        </div>
        <div class="track">
          <div class="fill" id="rawBar" style="width:0%;background:linear-gradient(90deg,#dc2626,#f87171)"></div>
          <div class="limit-line" style="left:22%"></div>
          <div class="limit-tag" style="left:calc(22% + 4px)">1.0 Mbit/s link</div>
        </div>
      </div>

      <div class="bar-row">
        <div class="bar-head">
          <span class="bar-name" style="color:var(--blue)">labstream comp/1 (128 ch @ 1000 Hz, lossless bit-exact)</span>
          <span class="bar-val" id="compVal">-- Mbit/s</span>
        </div>
        <div class="track">
          <div class="fill" id="compBar" style="width:0%;background:linear-gradient(90deg,#0284c7,#38bdf8)"></div>
          <div class="limit-line" style="left:22%"></div>
        </div>
      </div>

      <div class="bar-row">
        <div class="bar-head">
          <span class="bar-name" style="color:var(--green)" id="selLabel">labstream comp/1 + select/1 (8 ch @ 250 Hz, FIR decimated)</span>
          <span class="bar-val" id="selVal">-- Mbit/s</span>
        </div>
        <div class="track">
          <div class="fill" id="selBar" style="width:0%;background:linear-gradient(90deg,#059669,#34d399)"></div>
          <div class="limit-line" style="left:22%"></div>
        </div>
      </div>

      <div class="controls">
        <div class="ctrl-group">
          <span class="ctrl-label">select/1 Channels:</span>
          <button data-ch="128">128 ch</button>
          <button data-ch="32">32 ch</button>
          <button data-ch="8" class="active">8 ch</button>
          <button data-ch="4">4 ch</button>
        </div>
        <div class="ctrl-group">
          <span class="ctrl-label">select/1 Rate (FIR):</span>
          <button data-dec="1">1000 Hz (1×)</button>
          <button data-dec="2">500 Hz (2×)</button>
          <button data-dec="4" class="active">250 Hz (4×)</button>
          <button data-dec="8">125 Hz (8×)</button>
        </div>
      </div>
    </div>

    <div class="two-col">
      <div class="card">
        <h2>
          <span>Throughput History (last 30 s)</span>
          <span class="legend">
            <span><i class="dot" style="background:var(--red)"></i>Raw</span>
            <span><i class="dot" style="background:var(--blue)"></i>comp/1</span>
            <span><i class="dot" style="background:var(--green)"></i>comp/1+select/1</span>
          </span>
        </h2>
        <canvas id="histCanvas" width="540" height="220"></canvas>
      </div>

      <div class="card">
        <h2>
          <span>Ch 0 Waveform (1000 Hz vs select/1 FIR)</span>
          <span class="legend">
            <span><i class="dot" style="background:var(--blue)"></i>1000 Hz comp/1</span>
            <span><i class="dot" style="background:var(--green)"></i>select/1 FIR</span>
          </span>
        </h2>
        <canvas id="waveCanvas" width="540" height="220"></canvas>
      </div>
    </div>
  </div>
</div>

<script>
let paused = false;
let lastState = null;
let curCh = 8;
let curDec = 4;

const maxMbps = 4.55; // 1.0 Mbit/s sits at ~22%

function setSelect(ch, dec) {
  curCh = ch; curDec = dec;
  document.querySelectorAll('[data-ch]').forEach(b => b.classList.toggle('active', +b.dataset.ch === ch));
  document.querySelectorAll('[data-dec]').forEach(b => b.classList.toggle('active', +b.dataset.dec === dec));
  fetch(`/api/select?channels=${ch}&decimate=${dec}`, { method: 'POST' });
}

document.querySelectorAll('[data-ch]').forEach(b => b.onclick = () => setSelect(+b.dataset.ch, curDec));
document.querySelectorAll('[data-dec]').forEach(b => b.onclick = () => setSelect(curCh, +b.dataset.dec));

document.getElementById('pauseBtn').onclick = () => {
  paused = !paused;
  document.getElementById('pauseBtn').textContent = paused ? 'Resume' : 'Pause';
};

function fmtNum(n) { return n.toLocaleString('en-US'); }

function drawHistory(st) {
  const c = document.getElementById('histCanvas');
  const ctx = c.getContext('2d');
  const W = c.width, H = c.height;
  ctx.clearRect(0, 0, W, H);

  // Grid + 1 Mbit/s line
  ctx.strokeStyle = '#1b2638'; ctx.lineWidth = 1;
  for (let m = 1; m <= 4; m++) {
    const y = H - (m / maxMbps) * (H - 20) - 10;
    ctx.beginPath(); ctx.moveTo(0, y); ctx.lineTo(W, y); ctx.stroke();
    ctx.fillStyle = '#64748b'; ctx.font = '11px monospace';
    ctx.fillText(m + ' Mbit/s', 6, y - 3);
  }
  const y1 = H - (1.0 / maxMbps) * (H - 20) - 10;
  ctx.strokeStyle = '#fbbf24'; ctx.setLineDash([5, 4]);
  ctx.beginPath(); ctx.moveTo(0, y1); ctx.lineTo(W, y1); ctx.stroke();
  ctx.setLineDash([]);

  const hist = st.history || [];
  if (hist.length < 2) return;
  const series = [
    { idx: 0, color: '#f87171', w: 2 },
    { idx: 1, color: '#38bdf8', w: 2.2 },
    { idx: 2, color: '#34d399', w: 2.6 },
  ];
  for (const s of series) {
    ctx.strokeStyle = s.color; ctx.lineWidth = s.w;
    ctx.beginPath();
    hist.forEach((pt, i) => {
      const x = (i / (hist.length - 1)) * (W - 12) + 6;
      const y = H - (Math.min(pt[s.idx], maxMbps) / maxMbps) * (H - 20) - 10;
      if (i === 0) ctx.moveTo(x, y); else ctx.lineTo(x, y);
    });
    ctx.stroke();
  }
}

function drawWave(st) {
  const c = document.getElementById('waveCanvas');
  const ctx = c.getContext('2d');
  const W = c.width, H = c.height;
  ctx.clearRect(0, 0, W, H);

  ctx.strokeStyle = '#1b2638'; ctx.lineWidth = 1;
  ctx.beginPath(); ctx.moveTo(0, H/2); ctx.lineTo(W, H/2); ctx.stroke();

  const wf = st.wave_full || [];
  const ws = st.wave_sel || [];
  if (wf.length < 2) return;
  const tMax = wf[wf.length - 1][0];
  const tMin = tMax - 0.4;
  const toX = t => ((t - tMin) / 0.4) * W;
  const toY = v => H / 2 - (v / 55.0) * (H * 0.42);

  // Full 1000 Hz trace
  ctx.strokeStyle = '#38bdf8'; ctx.lineWidth = 1.6;
  ctx.beginPath();
  let started = false;
  for (const [t, v] of wf) {
    if (t < tMin) continue;
    const x = toX(t), y = toY(v);
    if (!started) { ctx.moveTo(x, y); started = true; } else ctx.lineTo(x, y);
  }
  ctx.stroke();

  // Selected FIR-decimated trace
  ctx.strokeStyle = '#34d399'; ctx.lineWidth = 2.2;
  ctx.beginPath();
  started = false;
  for (const [t, v] of ws) {
    if (t < tMin) continue;
    const x = toX(t), y = toY(v);
    if (!started) { ctx.moveTo(x, y); started = true; } else ctx.lineTo(x, y);
  }
  ctx.stroke();
}

function updateUI(st) {
  lastState = st;
  const pct = v => Math.min(100, Math.max(1.2, (v / maxMbps) * 100)).toFixed(1) + '%';

  document.getElementById('rawBar').style.width = pct(st.raw_mbps);
  document.getElementById('compBar').style.width = pct(st.comp_mbps);
  document.getElementById('selBar').style.width = pct(st.sel_mbps);

  document.getElementById('rawVal').textContent =
    `${st.raw_mbps.toFixed(2)} Mbit/s (${(st.raw_mbps * 125).toFixed(0)} KB/s) — 100%`;
  document.getElementById('compVal').textContent =
    `${st.comp_mbps.toFixed(2)} Mbit/s (${(st.comp_mbps * 125).toFixed(0)} KB/s) — −${st.comp_saved_pct.toFixed(1)}% (ratio ${st.full_ratio.toFixed(2)})`;
  document.getElementById('selVal').textContent =
    `${st.sel_mbps.toFixed(3)} Mbit/s (${(st.sel_mbps * 125).toFixed(1)} KB/s) — −${st.sel_saved_pct.toFixed(2)}%`;

  const selRate = (1000 / st.sel_decimate).toFixed(0);
  document.getElementById('selLabel').textContent =
    `labstream comp/1 + select/1 (${st.sel_channels} ch @ ${selRate} Hz, FIR decimated)`;
  document.getElementById('transportTag').textContent =
    `Full: TCP+comp/1 | Subset: ${st.sel_on_udp ? 'UDP dgram/1+comp/1' : 'TCP+comp/1'}`;

  const badge = document.getElementById('bitBadge');
  if (st.bit_errors === 0) {
    badge.className = 'badge';
    badge.textContent = `✓ LOSSLESS BIT-EXACT (${fmtNum(st.bit_checked)} floats verified, 0 errors)`;
  } else {
    badge.className = 'badge err';
    badge.textContent = `✗ ${st.bit_errors} BIT ERRORS`;
  }

  drawHistory(st);
  drawWave(st);
}

document.getElementById('exportBtn').onclick = () => {
  if (!lastState) return;
  const st = lastState;
  const out = document.createElement('canvas');
  out.width = 1100; out.height = 560;
  const ctx = out.getContext('2d');
  ctx.fillStyle = '#0b0f17'; ctx.fillRect(0, 0, out.width, out.height);

  ctx.fillStyle = '#e6edf7'; ctx.font = 'bold 22px sans-serif';
  ctx.fillText('labstream v0.2.0 — Bandwidth Squeeze (128-ch @ 1000 Hz EEG)', 32, 42);
  ctx.fillStyle = '#34d399'; ctx.font = 'bold 14px monospace';
  ctx.fillText(`✓ LOSSLESS BIT-EXACT: ${fmtNum(st.bit_checked)} floats verified, ${st.bit_errors} errors`, 32, 68);

  const rows = [
    { label: 'Standard LSL (128 ch @ 1000 Hz, uncompressed)', val: st.raw_mbps, txt: `${st.raw_mbps.toFixed(2)} Mbit/s`, color: '#f87171' },
    { label: 'labstream comp/1 (128 ch @ 1000 Hz, lossless)', val: st.comp_mbps, txt: `${st.comp_mbps.toFixed(2)} Mbit/s (−${st.comp_saved_pct.toFixed(1)}%)`, color: '#38bdf8' },
    { label: `labstream comp/1 + select/1 (${st.sel_channels} ch @ ${(1000/st.sel_decimate).toFixed(0)} Hz)`, val: st.sel_mbps, txt: `${st.sel_mbps.toFixed(3)} Mbit/s (−${st.sel_saved_pct.toFixed(2)}%)`, color: '#34d399' },
  ];
  rows.forEach((r, i) => {
    const y = 105 + i * 62;
    ctx.fillStyle = r.color; ctx.font = 'bold 14px sans-serif';
    ctx.fillText(r.label, 32, y);
    ctx.fillStyle = '#e6edf7'; ctx.font = 'bold 14px monospace';
    ctx.fillText(r.txt, 740, y);
    ctx.fillStyle = '#131a26'; ctx.fillRect(32, y + 8, 1036, 26);
    const w = Math.max(8, Math.min(1036, (r.val / maxMbps) * 1036));
    ctx.fillStyle = r.color; ctx.fillRect(32, y + 8, w, 26);
    const lx = 32 + (1.0 / maxMbps) * 1036;
    ctx.fillStyle = '#fbbf24'; ctx.fillRect(lx, y + 8, 2, 26);
  });

  ctx.drawImage(document.getElementById('histCanvas'), 32, 310, 505, 220);
  ctx.drawImage(document.getElementById('waveCanvas'), 563, 310, 505, 220);

  const a = document.createElement('a');
  a.download = 'labstream_demo3_bandwidth.png';
  a.href = out.toDataURL('image/png');
  a.click();
};

async function poll() {
  if (!paused) {
    try {
      const r = await fetch('/api/state');
      if (r.ok) updateUI(await r.json());
    } catch (_) {}
  }
  setTimeout(poll, 200);
}
poll();
</script>
</body>
</html>
"#;
