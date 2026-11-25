//! Demo 4: Cross-process local IPC latency comparison (`shm/1` shared-memory ring
//! vs standard LSL TCP loopback) on a 64-channel Float32 closed-loop stream.

use labstream::{Buffer, Format, Inlet, Outlet, Query, StreamInfo};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const STREAM_NAME: &str = "Demo4ClosedLoop";
const CHANNELS: usize = 64;
const HIST_BINS: usize = 48; // log10 scale from 1 us (10^0) to 2000 us (10^3.3)
const LOG_MIN: f64 = 0.0; // 1 us
const LOG_MAX: f64 = 3.30103; // 2000 us
const TIMELINE_LEN: usize = 100; // 10 s at 10 Hz

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Coordinator,
    TcpInlet,
    ShmInlet,
}

struct Args {
    role: Role,
    port: u16,
}

fn parse_args() -> Args {
    let mut role = Role::Coordinator;
    let mut port = 8783u16;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--role" => {
                if let Some(v) = it.next() {
                    role = match v.as_str() {
                        "tcp-inlet" => Role::TcpInlet,
                        "shm-inlet" => Role::ShmInlet,
                        _ => Role::Coordinator,
                    };
                }
            }
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
    Args { role, port }
}

#[inline]
fn hist_bin(us: f64) -> usize {
    let v = us.max(1.0).log10();
    let norm = ((v - LOG_MIN) / (LOG_MAX - LOG_MIN)).clamp(0.0, 0.9999);
    (norm * HIST_BINS as f64) as usize
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// Child worker process: attaches to `Demo4ClosedLoop` and emits 10 Hz latency reports.
fn run_inlet_worker(use_shm: bool) {
    if use_shm {
        labstream_net::config::set_content("[labstream]\nFeatures = {seq, resume, shm}\n");
    } else {
        labstream_net::config::set_content("[labstream]\nFeatures = {seq}\n");
    }

    let q = Query::name(STREAM_NAME);
    let info = loop {
        if let Ok(Some(i)) = labstream::resolve_first(&q, Duration::from_millis(400)) {
            break i;
        }
    };

    let mut inlet = Inlet::builder(&info)
        .buffer(Buffer::Seconds(2.0))
        .open(Duration::from_secs(5))
        .expect("open worker inlet");

    let mut data: Vec<f32> = Vec::with_capacity(64 * CHANNELS);
    let mut ts: Vec<f64> = Vec::with_capacity(64);
    let mut win: Vec<f64> = Vec::with_capacity(256);
    let mut total_samples: u64 = 0;
    let mut last_report = Instant::now();
    let mut warmup_left: u64 = 50; // skip initial handshake burst
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    loop {
        data.clear();
        ts.clear();
        let n = match inlet.pull_chunk_f32(&mut data, &mut ts, 64, Duration::from_millis(5)) {
            Ok(n) => n,
            Err(_) => continue,
        };
        if n == 0 {
            continue;
        }
        let now = labstream_net::clock();
        for &t in &ts {
            if warmup_left > 0 {
                warmup_left -= 1;
                continue;
            }
            let lat_us = ((now - t) * 1_000_000.0).max(0.1);
            if lat_us < 50_000.0 {
                win.push(lat_us);
                total_samples += 1;
            }
        }

        if last_report.elapsed() >= Duration::from_millis(100) && !win.is_empty() {
            win.sort_by(|a, b| a.total_cmp(b));
            let p50 = percentile(&win, 0.50);
            let p95 = percentile(&win, 0.95);
            let p99 = percentile(&win, 0.99);
            let max_v = *win.last().unwrap_or(&0.0);
            let on_shm = if inlet.health().on_shm { 1 } else { 0 };

            // Encode window histogram bins as compact CSV after the 6 summary fields.
            let mut bins = [0u32; HIST_BINS];
            for &v in &win {
                bins[hist_bin(v)] += 1;
            }
            let _ = write!(
                out,
                "{p50:.2},{p95:.2},{p99:.2},{max_v:.2},{total_samples},{on_shm}"
            );
            for b in bins {
                let _ = write!(out, ",{b}");
            }
            if writeln!(out).is_err() || out.flush().is_err() {
                break;
            }

            win.clear();
            last_report = Instant::now();
        }
    }
}

#[derive(Clone)]
struct TrackStats {
    connected: bool,
    on_shm: bool,
    samples: u64,
    p50_us: f64,
    p95_us: f64,
    p99_us: f64,
    max_us: f64,
    hist: [u64; HIST_BINS],
    p50_ring: VecDeque<f64>,
    p99_ring: VecDeque<f64>,
}

impl Default for TrackStats {
    fn default() -> Self {
        Self {
            connected: false,
            on_shm: false,
            samples: 0,
            p50_us: 0.0,
            p95_us: 0.0,
            p99_us: 0.0,
            max_us: 0.0,
            hist: [0u64; HIST_BINS],
            p50_ring: VecDeque::with_capacity(TIMELINE_LEN + 4),
            p99_ring: VecDeque::with_capacity(TIMELINE_LEN + 4),
        }
    }
}

impl TrackStats {
    fn reset(&mut self) {
        self.max_us = 0.0;
        self.hist = [0u64; HIST_BINS];
        self.p50_ring.clear();
        self.p99_ring.clear();
    }

    fn ingest(&mut self, line: &str) {
        let mut it = line.trim().split(',');
        let p50: f64 = it.next().and_then(|s| s.parse().ok()).unwrap_or(0.0);
        let p95: f64 = it.next().and_then(|s| s.parse().ok()).unwrap_or(0.0);
        let p99: f64 = it.next().and_then(|s| s.parse().ok()).unwrap_or(0.0);
        let max_v: f64 = it.next().and_then(|s| s.parse().ok()).unwrap_or(0.0);
        let samples: u64 = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let on_shm: u8 = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);

        self.connected = true;
        self.on_shm = on_shm != 0;
        self.samples = samples;
        self.p50_us = p50;
        self.p95_us = p95;
        self.p99_us = p99;
        if max_v > self.max_us {
            self.max_us = max_v;
        }
        for b in &mut self.hist {
            if let Some(c) = it.next().and_then(|s| s.parse::<u64>().ok()) {
                *b += c;
            }
        }
        self.p50_ring.push_back(p50);
        while self.p50_ring.len() > TIMELINE_LEN {
            self.p50_ring.pop_front();
        }
        self.p99_ring.push_back(p99);
        while self.p99_ring.len() > TIMELINE_LEN {
            self.p99_ring.pop_front();
        }
    }
}

struct SharedState {
    rate_hz: usize,
    tcp: TrackStats,
    shm: TrackStats,
}

fn spawn_worker(role_arg: &str) -> Child {
    let exe = std::env::current_exe().expect("current exe path");
    Command::new(exe)
        .args(["--role", role_arg])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn inlet worker")
}

fn run_sender(rate_hz: Arc<AtomicUsize>, stop: Arc<AtomicBool>) {
    let labels: Vec<String> = (1..=CHANNELS).map(|i| format!("CH{i:02}")).collect();
    let info = StreamInfo::builder(STREAM_NAME, "EEG", Format::Float32)
        .rate(1000.0)
        .source_id("demo4-closed-loop-64")
        .channel_labels(labels.iter().map(String::as_str))
        .build()
        .expect("build Demo4 StreamInfo");

    let outlet = Outlet::new(info).expect("create Demo4 Outlet");
    eprintln!("[demo4-sender] publishing {STREAM_NAME} ({CHANNELS} ch Float32, shm/1 + TCP)");

    let mut sample = vec![0.0f32; CHANNELS];
    let mut seq: u64 = 0;
    let mut next_tick = Instant::now();

    while !stop.load(Ordering::Relaxed) {
        let hz = rate_hz.load(Ordering::Relaxed).clamp(100, 4000) as f64;
        let period = Duration::from_secs_f64(1.0 / hz);
        let phase = (seq as f32) * 0.05;
        for (c, v) in sample.iter_mut().enumerate() {
            *v = (phase + c as f32 * 0.1).sin() * 25.0;
        }
        let _ = outlet.push(&sample);
        seq += 1;

        next_tick += period;
        let now = Instant::now();
        if next_tick > now {
            thread::sleep(next_tick - now);
        } else if now.duration_since(next_tick) > Duration::from_millis(50) {
            next_tick = now;
        }
    }
}

fn state_json(st: &SharedState) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(4096);
    let speedup_p50 = if st.shm.p50_us > 0.05 {
        st.tcp.p50_us / st.shm.p50_us
    } else {
        0.0
    };
    let speedup_p99 = if st.shm.p99_us > 0.05 {
        st.tcp.p99_us / st.shm.p99_us
    } else {
        0.0
    };
    let _ = write!(
        out,
        "{{\"channels\":{CHANNELS},\"rate_hz\":{},\"speedup_p50\":{speedup_p50:.2},\"speedup_p99\":{speedup_p99:.2},",
        st.rate_hz
    );
    write_track(&mut out, "tcp", &st.tcp);
    out.push(',');
    write_track(&mut out, "shm", &st.shm);
    out.push('}');
    out
}

fn write_track(out: &mut String, name: &str, t: &TrackStats) {
    use std::fmt::Write as _;
    let _ = write!(
        out,
        "\"{name}\":{{\"connected\":{},\"on_shm\":{},\"samples\":{},\"p50\":{:.2},\"p95\":{:.2},\"p99\":{:.2},\"max\":{:.2},\"hist\":[",
        t.connected, t.on_shm, t.samples, t.p50_us, t.p95_us, t.p99_us, t.max_us
    );
    for (i, &c) in t.hist.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "{c}");
    }
    out.push_str("],\"p50_ring\":[");
    for (i, &v) in t.p50_ring.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "{v:.2}");
    }
    out.push_str("],\"p99_ring\":[");
    for (i, &v) in t.p99_ring.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "{v:.2}");
    }
    out.push_str("]}");
}

fn handle_http(
    mut stream: TcpStream,
    state: &Arc<Mutex<SharedState>>,
    rate_hz: &Arc<AtomicUsize>,
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
    let (path, _) = path_full.split_once('?').unwrap_or((path_full, ""));

    let (status, ct, body) = match (method, path) {
        ("GET", "/") => ("200 OK", "text/html; charset=utf-8", INDEX_HTML.to_string()),
        ("GET", "/api/state") => {
            let st = state.lock().unwrap();
            ("200 OK", "application/json", state_json(&st))
        }
        ("POST", "/api/reset") => {
            let mut st = state.lock().unwrap();
            st.tcp.reset();
            st.shm.reset();
            ("200 OK", "application/json", "{\"ok\":true}".to_string())
        }
        ("POST", p) if p.starts_with("/api/rate/") => {
            if let Ok(hz) = p["/api/rate/".len()..].parse::<usize>() {
                let hz = hz.clamp(100, 4000);
                rate_hz.store(hz, Ordering::Relaxed);
                let mut st = state.lock().unwrap();
                st.rate_hz = hz;
                st.tcp.reset();
                st.shm.reset();
            }
            ("200 OK", "application/json", "{\"ok\":true}".to_string())
        }
        _ => ("404 Not Found", "text/plain", "not found".to_string()),
    };

    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes());
}

fn main() {
    let args = parse_args();
    match args.role {
        Role::TcpInlet => {
            run_inlet_worker(false);
            return;
        }
        Role::ShmInlet => {
            run_inlet_worker(true);
            return;
        }
        Role::Coordinator => {}
    }

    // Coordinator enables `shm` on the outlet; TCP-only clients automatically get
    // standard TCP while `shm/1` clients get the lock-free shared-memory ring.
    labstream_net::config::set_content("[labstream]\nFeatures = {seq, resume, shm}\n");

    let stop = Arc::new(AtomicBool::new(false));
    let rate_hz = Arc::new(AtomicUsize::new(1000));
    let state = Arc::new(Mutex::new(SharedState {
        rate_hz: 1000,
        tcp: TrackStats::default(),
        shm: TrackStats::default(),
    }));

    {
        let r = Arc::clone(&rate_hz);
        let s = Arc::clone(&stop);
        thread::spawn(move || run_sender(r, s));
    }

    // Give the outlet 150 ms to bind before spawning both child OS processes.
    thread::sleep(Duration::from_millis(150));

    let mut tcp_child = spawn_worker("tcp-inlet");
    let mut shm_child = spawn_worker("shm-inlet");

    if let Some(stdout) = tcp_child.stdout.take() {
        let st = Arc::clone(&state);
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                st.lock().unwrap().tcp.ingest(&line);
            }
        });
    }
    if let Some(stdout) = shm_child.stdout.take() {
        let st = Arc::clone(&state);
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                st.lock().unwrap().shm.ingest(&line);
            }
        });
    }

    let addr = format!("127.0.0.1:{}", args.port);
    let listener = TcpListener::bind(&addr).expect("bind HTTP server");
    println!("============================================================");
    println!("  labstream Demo 4 — Local Microsecond IPC (shm/1 vs TCP)");
    println!("  Open in browser:  http://{addr}/");
    println!("============================================================");

    for conn in listener.incoming().flatten() {
        let st = Arc::clone(&state);
        let rh = Arc::clone(&rate_hz);
        thread::spawn(move || handle_http(conn, &st, &rh));
    }

    let _ = tcp_child.kill();
    let _ = shm_child.kill();
}

const INDEX_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>labstream Demo 4 — Local Microsecond IPC (shm/1 vs TCP)</title>
<style>
  :root {
    --bg: #0b0f17;
    --panel: #131a26;
    --border: #233046;
    --text: #e6edf7;
    --muted: #8ea0b8;
    --red: #f87171;
    --green: #34d399;
    --gold: #fbbf24;
    --blue: #38bdf8;
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
  h1 { margin: 0; font-size: 21px; }
  .sub { color: var(--muted); font-size: 13px; margin-top: 3px; }
  .toolbar { display: flex; gap: 8px; flex-wrap: wrap; align-items: center; }
  button {
    background: #1b2536; color: var(--text); border: 1px solid var(--border);
    border-radius: 7px; padding: 7px 12px; font-size: 13px; cursor: pointer; font-weight: 600;
  }
  button:hover { border-color: #4b6588; }
  button.active { background: #164e63; border-color: var(--blue); color: #e0f2fe; }
  button.primary { background: #065f46; border-color: var(--green); color: #ecfdf5; }
  .kpis { display: grid; grid-template-columns: repeat(3, 1fr); gap: 14px; margin-bottom: 14px; }
  @media (max-width: 820px) { .kpis { grid-template-columns: 1fr; } }
  .card {
    background: var(--panel); border: 1px solid var(--border);
    border-radius: 12px; padding: 16px;
  }
  .kpi-title { font-size: 12px; font-weight: 700; text-transform: uppercase; letter-spacing: 0.04em; margin-bottom: 8px; display: flex; justify-content: space-between; align-items: center; }
  .kpi-big { font-size: 30px; font-weight: 800; font-family: ui-monospace, SFMono-Regular, Menlo, monospace; }
  .kpi-sub { font-size: 12px; color: var(--muted); font-family: ui-monospace, SFMono-Regular, Menlo, monospace; margin-top: 6px; }
  .pill {
    font-size: 11px; padding: 2px 8px; border-radius: 999px; font-weight: 700;
    background: #052e16; border: 1px solid #14532d; color: var(--green);
  }
  .two-col { display: grid; grid-template-columns: 1fr 1fr; gap: 14px; }
  @media (max-width: 860px) { .two-col { grid-template-columns: 1fr; } }
  .card h2 { margin: 0 0 10px 0; font-size: 15px; color: #cbd5e1; display: flex; justify-content: space-between; align-items: center; flex-wrap: wrap; gap: 8px; }
  canvas { width: 100%; height: 260px; display: block; background: #090d14; border-radius: 8px; border: 1px solid #1c2638; }
  .legend { display: flex; gap: 14px; font-size: 12px; color: var(--muted); }
  .dot { display: inline-block; width: 10px; height: 10px; border-radius: 50%; margin-right: 5px; vertical-align: middle; }
</style>
</head>
<body>
<div class="wrap">
  <header>
    <div>
      <h1>Demo 4 — Local Microsecond IPC (64-ch Closed-Loop BCI)</h1>
      <div class="sub">Two separate OS processes racing the exact same outlet: Standard LSL TCP loopback vs <code>labstream shm/1</code> lock-free shared-memory ring</div>
    </div>
    <div class="toolbar">
      <button data-rate="500">500 Hz</button>
      <button data-rate="1000" class="active">1000 Hz</button>
      <button data-rate="2000">2000 Hz</button>
      <button id="resetBtn">Reset Stats</button>
      <button id="pauseBtn">Pause</button>
      <button id="exportBtn" class="primary">Export PNG</button>
    </div>
  </header>

  <div class="kpis">
    <div class="card">
      <div class="kpi-title" style="color:var(--red)">
        <span>Standard LSL (TCP Loopback)</span>
        <span style="color:var(--muted)">Process A</span>
      </div>
      <div class="kpi-big" style="color:var(--red)" id="tcpP50">-- µs</div>
      <div class="kpi-sub" id="tcpSub">p95: -- µs | p99: -- µs | max: -- µs</div>
    </div>
    <div class="card">
      <div class="kpi-title" style="color:var(--green)">
        <span>labstream shm/1 (Shared Memory)</span>
        <span class="pill" id="shmPill">SHM RING</span>
      </div>
      <div class="kpi-big" style="color:var(--green)" id="shmP50">-- µs</div>
      <div class="kpi-sub" id="shmSub">p95: -- µs | p99: -- µs | max: -- µs</div>
    </div>
    <div class="card">
      <div class="kpi-title" style="color:var(--gold)">
        <span>Cross-Process Speedup</span>
        <span style="color:var(--muted)" id="sampleCnt">0 samples</span>
      </div>
      <div class="kpi-big" style="color:var(--gold)" id="speedup">--×</div>
      <div class="kpi-sub" id="speedupSub">p50 &amp; p99 tail latency reduction</div>
    </div>
  </div>

  <div class="two-col">
    <div class="card">
      <h2>
        <span>Latency Distribution (Log Scale: 1 µs – 2000 µs)</span>
        <span class="legend">
          <span><i class="dot" style="background:var(--red)"></i>TCP Loopback</span>
          <span><i class="dot" style="background:var(--green)"></i>shm/1 Ring</span>
        </span>
      </h2>
      <canvas id="histCanvas" width="550" height="260"></canvas>
    </div>
    <div class="card">
      <h2>
        <span>Live p50 &amp; p99 Latency Timeline (last 10 s)</span>
        <span class="legend">
          <span><i class="dot" style="background:var(--red)"></i>TCP p50/p99</span>
          <span><i class="dot" style="background:var(--green)"></i>shm/1 p50/p99</span>
        </span>
      </h2>
      <canvas id="timeCanvas" width="550" height="260"></canvas>
    </div>
  </div>
</div>

<script>
let paused = false;
let lastState = null;
const LOG_MIN = 0.0;
const LOG_MAX = Math.log10(2000.0);

document.querySelectorAll('[data-rate]').forEach(b => {
  b.onclick = () => {
    const hz = +b.dataset.rate;
    document.querySelectorAll('[data-rate]').forEach(x => x.classList.toggle('active', +x.dataset.rate === hz));
    fetch(`/api/rate/${hz}`, { method: 'POST' });
  };
});
document.getElementById('resetBtn').onclick = () => fetch('/api/reset', { method: 'POST' });
document.getElementById('pauseBtn').onclick = () => {
  paused = !paused;
  document.getElementById('pauseBtn').textContent = paused ? 'Resume' : 'Pause';
};

function drawHist(st) {
  const c = document.getElementById('histCanvas');
  const ctx = c.getContext('2d');
  const W = c.width, H = c.height;
  ctx.clearRect(0, 0, W, H);
  const padB = 26, padT = 16, plotH = H - padB - padT;

  const ticks = [1, 2, 5, 10, 20, 50, 100, 200, 500, 1000, 2000];
  ctx.strokeStyle = '#1b2638'; ctx.lineWidth = 1;
  ctx.fillStyle = '#64748b'; ctx.font = '10px monospace';
  for (const t of ticks) {
    const x = ((Math.log10(t) - LOG_MIN) / (LOG_MAX - LOG_MIN)) * (W - 20) + 10;
    ctx.beginPath(); ctx.moveTo(x, padT); ctx.lineTo(x, H - padB); ctx.stroke();
    const lbl = t >= 1000 ? (t/1000) + 'ms' : t + 'µs';
    ctx.fillText(lbl, x - 12, H - 8);
  }

  const th = st.tcp.hist || [], sh = st.shm.hist || [];
  const maxTcp = Math.max(1, ...th);
  const maxShm = Math.max(1, ...sh);
  const bw = (W - 20) / th.length;

  // Normalize each distribution to its own peak so both shapes are crystal clear
  for (let i = 0; i < th.length; i++) {
    const x = 10 + i * bw;
    if (th[i] > 0) {
      const h = (th[i] / maxTcp) * plotH;
      ctx.fillStyle = 'rgba(248, 113, 113, 0.55)';
      ctx.fillRect(x, H - padB - h, bw - 1, h);
    }
    if (sh[i] > 0) {
      const h = (sh[i] / maxShm) * plotH;
      ctx.fillStyle = 'rgba(52, 211, 153, 0.65)';
      ctx.fillRect(x, H - padB - h, bw - 1, h);
    }
  }

  // p50 vertical markers
  const mark = (us, color, label) => {
    if (us <= 0) return;
    const x = ((Math.log10(Math.max(1, us)) - LOG_MIN) / (LOG_MAX - LOG_MIN)) * (W - 20) + 10;
    ctx.strokeStyle = color; ctx.lineWidth = 2; ctx.setLineDash([4, 3]);
    ctx.beginPath(); ctx.moveTo(x, padT); ctx.lineTo(x, H - padB); ctx.stroke();
    ctx.setLineDash([]);
    ctx.fillStyle = color; ctx.font = 'bold 11px monospace';
    ctx.fillText(label, Math.min(W - 80, x + 4), padT + 12);
  };
  mark(st.shm.p50, '#34d399', `shm p50 ${st.shm.p50.toFixed(1)}µs`);
  mark(st.tcp.p50, '#f87171', `tcp p50 ${st.tcp.p50.toFixed(1)}µs`);
}

function drawTimeline(st) {
  const c = document.getElementById('timeCanvas');
  const ctx = c.getContext('2d');
  const W = c.width, H = c.height;
  ctx.clearRect(0, 0, W, H);
  const padB = 20, padT = 14, plotH = H - padB - padT;

  const all = [...(st.tcp.p99_ring || []), ...(st.shm.p99_ring || []), 50];
  const yMax = Math.max(50, Math.ceil(Math.max(...all) / 50) * 50);

  ctx.strokeStyle = '#1b2638'; ctx.lineWidth = 1;
  ctx.fillStyle = '#64748b'; ctx.font = '10px monospace';
  for (let i = 0; i <= 4; i++) {
    const v = (yMax * i) / 4;
    const y = H - padB - (i / 4) * plotH;
    ctx.beginPath(); ctx.moveTo(0, y); ctx.lineTo(W, y); ctx.stroke();
    ctx.fillText(v.toFixed(0) + ' µs', 6, y - 3);
  }

  const drawLine = (arr, color, width, dash) => {
    if (!arr || arr.length < 2) return;
    ctx.strokeStyle = color; ctx.lineWidth = width; ctx.setLineDash(dash);
    ctx.beginPath();
    arr.forEach((v, i) => {
      const x = (i / (arr.length - 1)) * (W - 16) + 8;
      const y = H - padB - (Math.min(v, yMax) / yMax) * plotH;
      if (i === 0) ctx.moveTo(x, y); else ctx.lineTo(x, y);
    });
    ctx.stroke();
    ctx.setLineDash([]);
  };

  drawLine(st.tcp.p99_ring, 'rgba(248,113,113,0.45)', 1.5, [4, 3]);
  drawLine(st.tcp.p50_ring, '#f87171', 2.4, []);
  drawLine(st.shm.p99_ring, 'rgba(52,211,153,0.45)', 1.5, [4, 3]);
  drawLine(st.shm.p50_ring, '#34d399', 2.6, []);
}

function updateUI(st) {
  lastState = st;
  document.getElementById('tcpP50').textContent = `${st.tcp.p50.toFixed(1)} µs`;
  document.getElementById('tcpSub').textContent =
    `p95: ${st.tcp.p95.toFixed(1)} µs | p99: ${st.tcp.p99.toFixed(1)} µs | max: ${st.tcp.max.toFixed(0)} µs`;
  document.getElementById('shmP50').textContent = `${st.shm.p50.toFixed(1)} µs`;
  document.getElementById('shmSub').textContent =
    `p95: ${st.shm.p95.toFixed(1)} µs | p99: ${st.shm.p99.toFixed(1)} µs | max: ${st.shm.max.toFixed(0)} µs`;
  document.getElementById('shmPill').textContent = st.shm.on_shm ? '✓ SHM RING ACTIVE' : 'CONNECTING...';
  document.getElementById('speedup').textContent = `${st.speedup_p50.toFixed(1)}× faster`;
  document.getElementById('speedupSub').textContent =
    `p50: ${st.speedup_p50.toFixed(1)}× faster | p99: ${st.speedup_p99.toFixed(1)}× faster`;
  document.getElementById('sampleCnt').textContent =
    `${st.shm.samples.toLocaleString('en-US')} samples @ ${st.rate_hz} Hz`;

  drawHist(st);
  drawTimeline(st);
}

document.getElementById('exportBtn').onclick = () => {
  if (!lastState) return;
  const st = lastState;
  const out = document.createElement('canvas');
  out.width = 1140; out.height = 460;
  const ctx = out.getContext('2d');
  ctx.fillStyle = '#0b0f17'; ctx.fillRect(0, 0, out.width, out.height);

  ctx.fillStyle = '#e6edf7'; ctx.font = 'bold 22px sans-serif';
  ctx.fillText(`labstream v0.2.0 — Cross-Process Local IPC (64-ch @ ${st.rate_hz} Hz)`, 28, 38);

  ctx.fillStyle = '#f87171'; ctx.font = 'bold 15px monospace';
  ctx.fillText(`Standard LSL TCP:  p50 ${st.tcp.p50.toFixed(1)} µs  |  p99 ${st.tcp.p99.toFixed(1)} µs  |  max ${st.tcp.max.toFixed(0)} µs`, 28, 72);
  ctx.fillStyle = '#34d399';
  ctx.fillText(`labstream shm/1:   p50 ${st.shm.p50.toFixed(1)} µs  |  p99 ${st.shm.p99.toFixed(1)} µs  |  max ${st.shm.max.toFixed(0)} µs`, 28, 98);
  ctx.fillStyle = '#fbbf24';
  ctx.fillText(`Speedup: ${st.speedup_p50.toFixed(1)}× lower p50  |  ${st.speedup_p99.toFixed(1)}× lower p99  (${st.shm.samples.toLocaleString('en-US')} samples)`, 28, 124);

  ctx.drawImage(document.getElementById('histCanvas'), 28, 150, 530, 280);
  ctx.drawImage(document.getElementById('timeCanvas'), 582, 150, 530, 280);

  const a = document.createElement('a');
  a.download = 'labstream_demo4_shm_ipc.png';
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
  setTimeout(poll, 150);
}
poll();
</script>
</body>
</html>
"#;
