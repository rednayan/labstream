//! Demo 2: Hyperscanning Phase-Lock (`liblsl` nearest-sample vs `labstream SyncGroup`).
//!
//! Simulates two asynchronous 250 SPS ADCs sampling the exact same physical
//! sine wave (50 Hz, 60 Hz, or 100 Hz) with a worst-case half-sample (2.0 ms)
//! clock phase offset — producing 72.0° phase error at 100 Hz under naive
//! nearest-sample pairing, and < 0.2° under `labstream::SyncGroup`!
//!
//! Modes:
//! - `--mode both` (default): runs both ADCs + live split-screen viewer on one PC.
//! - `--mode sender`: runs only the two asynchronous ADC outlets (e.g. Windows).
//! - `--mode viewer --peer <IP>`: connects to remote ADCs and serves dashboard.

use labstream_net::{local_ipv4_addresses, resolve, Inlet, Outlet, StreamInfo, SyncConfig, SyncGroup};
use labstream_wire::Format;
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const FS: f64 = 250.0;
const DT: f64 = 1.0 / FS; // 4.0 ms
const HALF_SAMPLE: f64 = 0.5 * DT; // 2.0 ms worst-case offset -> 72.0 deg at 100 Hz!
const CHUNK: usize = 8;
const MAX_SCOPE: usize = 256; // ~1 second at 250 SPS (UI zooms to 60 ms / 15 samples)

struct ScopeData {
    freq_hz: f64,
    /// Naive nearest-sample pairs: `(t, adc_a, adc_b)`
    naive: VecDeque<(f64, f64, f64)>,
    /// SyncGroup aligned pairs: `(t, adc_a, adc_b)`
    synced: VecDeque<(f64, f64, f64)>,
    naive_phase_deg: f64,
    synced_phase_deg: f64,
    naive_rms_pct: f64,
    synced_rms_pct: f64,
    samples_synced: u64,
}

impl ScopeData {
    fn new(freq_hz: f64) -> Self {
        ScopeData {
            freq_hz,
            naive: VecDeque::with_capacity(MAX_SCOPE),
            synced: VecDeque::with_capacity(MAX_SCOPE),
            naive_phase_deg: 0.0,
            synced_phase_deg: 0.0,
            naive_rms_pct: 0.0,
            synced_rms_pct: 0.0,
            samples_synced: 0,
        }
    }

    fn recompute_metrics(&mut self) {
        let calc = |buf: &VecDeque<(f64, f64, f64)>| -> (f64, f64) {
            if buf.len() < 40 {
                return (0.0, 0.0);
            }
            let skip = buf.len().saturating_sub(150);
            let mut err2 = 0.0;
            let mut ref2 = 0.0;
            for &(_, a, b) in buf.iter().skip(skip) {
                let d = a - b;
                err2 += d * d;
                ref2 += a * a;
            }
            if ref2 < 1e-12 {
                return (0.0, 0.0);
            }
            let rel = (err2 / ref2).sqrt();
            let deg = 2.0 * (rel / 2.0).min(1.0).asin() * 180.0 / std::f64::consts::PI;
            (deg, rel * 100.0)
        };
        let (nd, nr) = calc(&self.naive);
        let (sd, sr) = calc(&self.synced);
        self.naive_phase_deg = nd;
        self.naive_rms_pct = nr;
        self.synced_phase_deg = sd;
        self.synced_rms_pct = sr;
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut mode = String::from("both");
    let mut peer: Option<String> = None;
    let mut port: u16 = 8781;
    let mut freq_hz: u32 = 100;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--mode" => {
                i += 1;
                if let Some(m) = args.get(i) {
                    mode = m.clone();
                }
            }
            "--peer" => {
                i += 1;
                peer = args.get(i).cloned();
            }
            "--port" => {
                i += 1;
                if let Some(p) = args.get(i).and_then(|s| s.parse().ok()) {
                    port = p;
                }
            }
            "--freq" => {
                i += 1;
                if let Some(f) = args.get(i).and_then(|s| s.parse().ok()) {
                    freq_hz = f;
                }
            }
            _ => {}
        }
        i += 1;
    }

    if let Some(ref ip) = peer {
        let p = std::env::temp_dir().join(format!("demo2_lsl_{}.cfg", std::process::id()));
        std::fs::write(&p, format!("[lab]\nKnownPeers = {{{ip}}}\n")).expect("write cfg");
        std::env::set_var("LSLAPICFG", &p);
    }

    let freq_atomic = Arc::new(AtomicU32::new(freq_hz));

    if mode == "sender" {
        run_sender(freq_atomic);
        return;
    }

    if mode == "both" {
        let f = Arc::clone(&freq_atomic);
        std::thread::spawn(move || run_sender(f));
        std::thread::sleep(Duration::from_millis(200));
    }

    let shared = Arc::new(Mutex::new(ScopeData::new(freq_hz as f64)));

    // Spawn live naive + SyncGroup consumer threads.
    {
        let st = Arc::clone(&shared);
        std::thread::spawn(move || run_consumers(st));
    }

    let bind = format!("127.0.0.1:{port}");
    let listener = TcpListener::bind(&bind).expect("bind HTTP");
    println!("============================================================");
    println!("  labstream Demo 2 — Hyperscanning Phase-Lock");
    println!("  250 SPS async ADCs (2.0 ms half-sample offset)");
    println!("  Open in browser:  http://{bind}/");
    println!("============================================================");

    for conn in listener.incoming().flatten() {
        let st = Arc::clone(&shared);
        let fa = Arc::clone(&freq_atomic);
        std::thread::spawn(move || {
            let _ = handle_http(conn, st, fa);
        });
    }
}

fn run_sender(freq_hz: Arc<AtomicU32>) {
    let mut info_a = StreamInfo::new("HyperADC_A", "EEG", 1, Format::Float32, FS);
    info_a.source_id = "demo2-adc-a".to_string();

    let mut info_b = StreamInfo::new("HyperADC_B", "EEG", 1, Format::Float32, FS);
    info_b.source_id = "demo2-adc-b".to_string();

    let out_a = Outlet::new(info_a).expect("HyperADC_A outlet");
    let out_b = Outlet::new(info_b).expect("HyperADC_B outlet");

    println!("[demo2-sender] publishing HyperADC_A and HyperADC_B at 250 SPS (2.0 ms offset)");
    for ip in local_ipv4_addresses() {
        if !ip.is_loopback() {
            println!("  -> {ip}");
        }
    }

    let t_start = Instant::now();
    let t0 = 1000.0f64;
    let mut k: u64 = 0;
    let mut va = Vec::with_capacity(CHUNK);
    let mut vb = Vec::with_capacity(CHUNK);

    loop {
        let target = (k + CHUNK as u64) as f64 * DT;
        let elapsed = t_start.elapsed().as_secs_f64();
        if target > elapsed {
            std::thread::sleep(Duration::from_secs_f64(target - elapsed));
        }

        let f = freq_hz.load(Ordering::Relaxed) as f64;
        va.clear();
        vb.clear();
        for i in 0..CHUNK {
            let idx = k + i as u64;
            let ta = t0 + idx as f64 * DT;
            let tb = ta + HALF_SAMPLE; // +2.0 ms half-sample asynchronous ADC offset
            va.push((2.0 * std::f64::consts::PI * f * ta).sin() as f32);
            vb.push((2.0 * std::f64::consts::PI * f * tb).sin() as f32);
        }
        let last_a = t0 + (k + CHUNK as u64 - 1) as f64 * DT;
        let last_b = last_a + HALF_SAMPLE;
        out_a.push_chunk_f32(&va, last_a);
        out_b.push_chunk_f32(&vb, last_b);
        k += CHUNK as u64;
    }
}

fn resolve_named(name: &str) -> StreamInfo {
    let q = format!("name='{name}'");
    loop {
        if let Ok(found) = resolve(&q, 1, Duration::from_secs(2)) {
            if let Some(info) = found.into_iter().next() {
                return info;
            }
        }
    }
}

fn run_consumers(shared: Arc<Mutex<ScopeData>>) {
    let info_a = resolve_named("HyperADC_A");
    let info_b = resolve_named("HyperADC_B");
    eprintln!("[demo2] resolved HyperADC_A and HyperADC_B");

    // 1. Naive nearest-sample pair (simulates what standard liblsl apps do).
    let st_naive = Arc::clone(&shared);
    let ia_n = info_a.clone();
    let ib_n = info_b.clone();
    std::thread::spawn(move || {
        let mut in_a = Inlet::open_with_buffer(&ia_n, Duration::from_secs(5), 4096).unwrap();
        let mut in_b = Inlet::open_with_buffer(&ib_n, Duration::from_secs(5), 4096).unwrap();
        let mut da = Vec::new();
        let mut ta = Vec::new();
        let mut db = Vec::new();
        let mut tb = Vec::new();
        let mut qa: VecDeque<(f64, f64)> = VecDeque::new();
        let mut qb: VecDeque<(f64, f64)> = VecDeque::new();
        loop {
            da.clear();
            ta.clear();
            db.clear();
            tb.clear();
            let _ = in_a.pull_chunk_f32(&mut da, &mut ta, 64, Duration::from_millis(20));
            let _ = in_b.pull_chunk_f32(&mut db, &mut tb, 64, Duration::from_millis(20));
            for i in 0..ta.len() {
                qa.push_back((ta[i], da[i] as f64));
            }
            for i in 0..tb.len() {
                qb.push_back((tb[i], db[i] as f64));
            }
            let mut st = st_naive.lock().unwrap();
            while let (Some(&(tsa, va)), Some(&(tsb, vb))) = (qa.front(), qb.front()) {
                // Pair nearest samples by index/timestamp without sub-sample resampling.
                if (tsa - tsb).abs() > DT * 1.5 {
                    if tsa < tsb {
                        qa.pop_front();
                    } else {
                        qb.pop_front();
                    }
                    continue;
                }
                qa.pop_front();
                qb.pop_front();
                if st.naive.len() >= MAX_SCOPE {
                    st.naive.pop_front();
                }
                st.naive.push_back((tsa, va, vb));
            }
            st.recompute_metrics();
        }
    });

    // 2. labstream v0.2.0 SyncGroup (fractional-delay windowed-sinc resampler).
    let in_a = Inlet::open_with_buffer(&info_a, Duration::from_secs(5), 4096).unwrap();
    let in_b = Inlet::open_with_buffer(&info_b, Duration::from_secs(5), 4096).unwrap();
    let mut cfg = SyncConfig::new(FS);
    cfg.auto_postprocess = false; // preserve exact synthetic timestamps for clean phase demo
    let mut group = SyncGroup::new(vec![in_a, in_b], cfg).expect("SyncGroup");
    let mut aligned = Vec::new();
    loop {
        aligned.clear();
        if let Ok(n) = group.pull(&mut aligned, 64, Duration::from_millis(40)) {
            if n > 0 {
                let mut st = shared.lock().unwrap();
                for s in &aligned {
                    if st.synced.len() >= MAX_SCOPE {
                        st.synced.pop_front();
                    }
                    st.synced.push_back((s.timestamp, s.values[0], s.values[1]));
                    st.samples_synced += 1;
                }
                st.recompute_metrics();
            }
        }
    }
}

fn handle_http(
    mut stream: TcpStream,
    shared: Arc<Mutex<ScopeData>>,
    freq_atomic: Arc<AtomicU32>,
) -> std::io::Result<()> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let mut buf = [0u8; 2048];
    let n = stream.read(&mut buf)?;
    if n == 0 {
        return Ok(());
    }
    let req = String::from_utf8_lossy(&buf[..n]);
    let first = req.lines().next().unwrap_or("");
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or("GET");
    let path = parts.next().unwrap_or("/");

    if method == "GET" && (path == "/" || path == "/index.html") {
        return respond(&mut stream, "200 OK", "text/html; charset=utf-8", HTML);
    }
    if method == "GET" && path == "/api/state" {
        let body = state_json(&shared.lock().unwrap());
        return respond(&mut stream, "200 OK", "application/json", &body);
    }
    if method == "POST" {
        if let Some(f_str) = path.strip_prefix("/api/freq/") {
            if let Ok(f) = f_str.parse::<u32>() {
                if matches!(f, 50 | 60 | 100) {
                    freq_atomic.store(f, Ordering::Relaxed);
                    let mut st = shared.lock().unwrap();
                    st.freq_hz = f as f64;
                    st.naive.clear();
                    st.synced.clear();
                    return respond(&mut stream, "200 OK", "application/json", r#"{"ok":true}"#);
                }
            }
        }
    }
    respond(&mut stream, "404 Not Found", "text/plain", "not found")
}

fn respond(stream: &mut TcpStream, status: &str, ct: &str, body: &str) -> std::io::Result<()> {
    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ct}\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(resp.as_bytes())?;
    stream.flush()
}

fn state_json(st: &ScopeData) -> String {
    let fmt_pts = |q: &VecDeque<(f64, f64, f64)>| -> String {
        let mut s = String::from("[");
        let skip = q.len().saturating_sub(40); // last 160 ms (40 samples @ 250 SPS)
        for (i, &(t, a, b)) in q.iter().skip(skip).enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&format!("[{t:.6},{a:.5},{b:.5}]"));
        }
        s.push(']');
        s
    };
    format!(
        "{{\"freq_hz\":{},\"naive_phase_deg\":{:.2},\"synced_phase_deg\":{:.3},\"naive_rms_pct\":{:.1},\"synced_rms_pct\":{:.2},\"samples_synced\":{},\"naive\":{},\"synced\":{}}}",
        st.freq_hz,
        st.naive_phase_deg,
        st.synced_phase_deg,
        st.naive_rms_pct,
        st.synced_rms_pct,
        st.samples_synced,
        fmt_pts(&st.naive),
        fmt_pts(&st.synced),
    )
}

const HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8" />
<title>Demo 2: Hyperscanning Phase-Lock — liblsl vs labstream SyncGroup</title>
<style>
  :root { --bg:#0d1117; --card:#161b22; --border:#30363d; --text:#e6edf3; --muted:#8b949e; --red:#f85149; --green:#3fb950; --cyan:#38bdf8; --orange:#fb923c; }
  * { box-sizing:border-box; }
  body { margin:0; padding:1.25rem 1.75rem; background:var(--bg); color:var(--text); font-family:system-ui,-apple-system,sans-serif; }
  header { display:flex; justify-content:space-between; align-items:center; flex-wrap:wrap; gap:1rem; margin-bottom:1rem; }
  h1 { margin:0; font-size:1.35rem; }
  .sub { color:var(--muted); font-size:0.88rem; margin-top:0.2rem; }
  .actions { display:flex; gap:0.5rem; align-items:center; }
  button { background:#21262d; color:var(--text); border:1px solid var(--border); padding:0.45rem 0.85rem; border-radius:6px; font-size:0.85rem; cursor:pointer; }
  button:hover { background:#30363d; }
  button.active { background:#1f6feb; border-color:#388bfd; color:#fff; }
  .panel { background:var(--card); border:1px solid var(--border); border-radius:10px; padding:0.9rem 1.1rem; margin-bottom:1rem; }
  .head { display:flex; justify-content:space-between; align-items:center; flex-wrap:wrap; gap:0.75rem; margin-bottom:0.6rem; }
  .title { font-weight:700; font-size:1.05rem; }
  .badges { display:flex; gap:0.6rem; flex-wrap:wrap; font-family:ui-monospace,monospace; font-size:0.83rem; }
  .badge { padding:0.2rem 0.6rem; border-radius:999px; border:1px solid var(--border); background:#21262d; }
  .badge.bad { color:var(--red); border-color:var(--red); background:rgba(248,81,73,0.16); font-weight:700; }
  .badge.good { color:var(--green); border-color:var(--green); background:rgba(63,185,80,0.16); font-weight:700; }
  canvas { width:100%; height:200px; display:block; background:#0b0f14; border-radius:6px; border:1px solid #21262d; }
  canvas.resid { height:90px; }
  .legend { display:flex; gap:1.25rem; font-size:0.8rem; color:var(--muted); margin-top:0.4rem; }
  .sw { display:inline-block; width:14px; height:3px; vertical-align:middle; margin-right:5px; }
</style>
</head>
<body>
<header>
  <div>
    <h1>Demo 2: Hyperscanning Phase-Lock — Two Asynchronous 250 SPS ADCs (2.0 ms offset)</h1>
    <div class="sub">Worst-case half-sample clock offset between two independent headsets: 72° phase shift at 100 Hz under nearest-sample pairing vs &lt; 0.2° with <code>labstream SyncGroup</code>.</div>
  </div>
  <div class="actions">
    <button data-f="50">50 Hz (36°)</button>
    <button data-f="60">60 Hz (43.2°)</button>
    <button data-f="100" class="active">100 Hz (72°)</button>
    <button id="pauseBtn">⏸ Pause</button>
    <button id="exportPng">Export PNG</button>
  </div>
</header>

<section class="panel">
  <div class="head">
    <div class="title" style="color:#fb923c">Standard LSL / Nearest-Sample Pairing (asynchronous ADCs)</div>
    <div class="badges" id="naiveBadges"></div>
  </div>
  <canvas id="cNaive" width="1200" height="200"></canvas>
  <div class="legend">
    <span><i class="sw" style="background:#38bdf8"></i>ADC A (reference)</span>
    <span><i class="sw" style="background:#fb923c"></i>ADC B (+2.0 ms offset — visibly out of phase!)</span>
  </div>
</section>

<section class="panel">
  <div class="head">
    <div class="title" style="color:#3fb950">labstream v0.2.0 SyncGroup (Blackman-Sinc Fractional-Delay Resampler)</div>
    <div class="badges" id="syncBadges"></div>
  </div>
  <canvas id="cSync" width="1200" height="200"></canvas>
  <div class="legend">
    <span><i class="sw" style="background:#38bdf8"></i>ADC A (aligned grid)</span>
    <span><i class="sw" style="background:#3fb950"></i>ADC B (resampled — locked on top of ADC A!)</span>
  </div>
</section>

<section class="panel">
  <div class="head">
    <div class="title">Residual Phase Error (ADC A − ADC B)</div>
    <div class="badges" id="residBadges"></div>
  </div>
  <canvas id="cResid" class="resid" width="1200" height="90"></canvas>
  <div class="legend">
    <span><i class="sw" style="background:#f85149"></i>Standard LSL residual (huge error)</span>
    <span><i class="sw" style="background:#3fb950"></i>labstream SyncGroup residual (flat zero!)</span>
  </div>
</section>

<script>
const cNaive = document.getElementById('cNaive');
const cSync = document.getElementById('cSync');
const cResid = document.getElementById('cResid');
let paused = false, lastState = null;

function drawPair(cv, pts, colA, colB) {
  const ctx = cv.getContext('2d'), W = cv.width, H = cv.height;
  ctx.clearRect(0, 0, W, H);
  const mid = H * 0.5, amp = H * 0.38;
  ctx.strokeStyle = '#1b222c'; ctx.lineWidth = 1;
  ctx.beginPath(); ctx.moveTo(0, mid); ctx.lineTo(W, mid); ctx.stroke();
  if (pts.length < 2) return;
  const drawOne = (idx, col, width, dash) => {
    ctx.strokeStyle = col; ctx.lineWidth = width; ctx.setLineDash(dash);
    ctx.beginPath();
    for (let i = 0; i < pts.length; i++) {
      const x = (i / (pts.length - 1)) * W;
      const y = mid - pts[i][idx] * amp;
      if (i === 0) ctx.moveTo(x, y); else ctx.lineTo(x, y);
    }
    ctx.stroke();
    ctx.setLineDash([]);
  };
  drawOne(1, colA, 2.4, []);
  drawOne(2, colB, 2.2, [6, 4]);
}

function drawResid(cv, naive, synced) {
  const ctx = cv.getContext('2d'), W = cv.width, H = cv.height;
  ctx.clearRect(0, 0, W, H);
  const mid = H * 0.5, amp = H * 0.34;
  ctx.strokeStyle = '#1b222c'; ctx.beginPath(); ctx.moveTo(0, mid); ctx.lineTo(W, mid); ctx.stroke();
  const plot = (pts, col, w) => {
    if (pts.length < 2) return;
    ctx.strokeStyle = col; ctx.lineWidth = w; ctx.beginPath();
    for (let i = 0; i < pts.length; i++) {
      const x = (i / (pts.length - 1)) * W;
      const y = mid - (pts[i][1] - pts[i][2]) * amp;
      if (i === 0) ctx.moveTo(x, y); else ctx.lineTo(x, y);
    }
    ctx.stroke();
  };
  plot(naive, '#f85149', 1.8);
  plot(synced, '#3fb950', 2.4);
}

async function poll() {
  if (paused) return;
  try {
    const r = await fetch('/api/state');
    const st = await r.json();
    lastState = st;
    const ratio = st.synced_phase_deg > 0.001 ? Math.round(st.naive_phase_deg / st.synced_phase_deg) : 999;
    document.getElementById('naiveBadges').innerHTML =
      `<span class="badge bad">PHASE ERROR: ${st.naive_phase_deg.toFixed(2)}°</span>` +
      `<span class="badge bad">RESIDUAL RMS: ${st.naive_rms_pct.toFixed(1)}%</span>`;
    document.getElementById('syncBadges').innerHTML =
      `<span class="badge good">PHASE ERROR: ${st.synced_phase_deg.toFixed(3)}°</span>` +
      `<span class="badge good">RESIDUAL RMS: ${st.synced_rms_pct.toFixed(2)}%</span>` +
      `<span class="badge good">${ratio}× BETTER</span>`;
    document.getElementById('residBadges').innerHTML =
      `<span class="badge">Signal: ${st.freq_hz} Hz @ 250 SPS</span>` +
      `<span class="badge">Aligned samples: ${st.samples_synced.toLocaleString()}</span>`;
    drawPair(cNaive, st.naive, '#38bdf8', '#fb923c');
    drawPair(cSync, st.synced, '#38bdf8', '#3fb950');
    drawResid(cResid, st.naive, st.synced);
  } catch (_) {}
}

document.querySelectorAll('button[data-f]').forEach(btn => {
  btn.onclick = async () => {
    document.querySelectorAll('button[data-f]').forEach(b => b.classList.remove('active'));
    btn.classList.add('active');
    await fetch('/api/freq/' + btn.dataset.f, { method: 'POST' });
  };
});
document.getElementById('pauseBtn').onclick = (e) => {
  paused = !paused;
  e.target.textContent = paused ? '▶ Resume' : '⏸ Pause';
  e.target.className = paused ? 'active' : '';
};
document.getElementById('exportPng').onclick = () => {
  const out = document.createElement('canvas');
  out.width = 1240; out.height = 660;
  const c = out.getContext('2d');
  c.fillStyle = '#0d1117'; c.fillRect(0, 0, out.width, out.height);
  c.fillStyle = '#f0f6fc'; c.font = 'bold 20px system-ui, sans-serif';
  const f = lastState ? lastState.freq_hz : 100;
  c.fillText(`Hyperscanning Phase-Lock (${f} Hz @ 250 SPS, 2.0 ms async offset): Standard LSL vs labstream SyncGroup`, 20, 34);
  c.font = 'bold 14px system-ui, sans-serif';
  c.fillStyle = '#fb923c';
  c.fillText(`Standard LSL Nearest-Sample — Phase Error: ${lastState ? lastState.naive_phase_deg.toFixed(2) : '?'}°  (RMS: ${lastState ? lastState.naive_rms_pct.toFixed(1) : '?'}%)`, 20, 68);
  c.drawImage(cNaive, 20, 78);
  c.fillStyle = '#3fb950';
  c.fillText(`labstream v0.2.0 SyncGroup — Phase Error: ${lastState ? lastState.synced_phase_deg.toFixed(3) : '?'}°  (RMS: ${lastState ? lastState.synced_rms_pct.toFixed(2) : '?'}%)`, 20, 308);
  c.drawImage(cSync, 20, 318);
  c.fillStyle = '#e6edf3';
  c.fillText('Residual Difference (ADC A − ADC B): Red = Standard LSL, Green = labstream SyncGroup', 20, 546);
  c.drawImage(cResid, 20, 556);
  const a = document.createElement('a');
  a.download = `labstream_hypersync_${f}hz.png`;
  a.href = out.toDataURL('image/png');
  a.click();
};

setInterval(poll, 80);
poll();
</script>
</body>
</html>
"#;
