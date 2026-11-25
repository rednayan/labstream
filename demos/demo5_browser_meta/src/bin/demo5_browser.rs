//! Demo 5: Browser-Native WASM Decoder (`labstream::ws::WsBridge` + `labstream-wasm`)
//! and Live `meta/1` Electrode Impedance + Bidirectional Browser-to-LSL Markers.

use labstream::ws::{WsBridge, WsOptions};
use labstream::{Format, Inlet, Outlet, Query, StreamInfo};
use std::collections::VecDeque;
use std::f64::consts::TAU;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const STREAM_NAME: &str = "Demo5BrowserEEG";
const MARKER_STREAM: &str = "BrowserMarkers";
const CHANNELS: usize = 8;
const SRATE: f64 = 250.0;
const CHUNK_SAMPLES: usize = 10; // 25 Hz push cadence (40 ms chunks)
const LABELS: [&str; CHANNELS] = ["Fp1", "Fp2", "C3", "C4", "P3", "P4", "O1", "O2"];
const LABSTREAM_JS: &str = include_str!("../../../../web/labstream.js");

struct SimControl {
    impedances: [f64; CHANNELS],
    battery_pct: f64,
    dirty_meta: bool,
    received_markers: VecDeque<(f64, String)>,
}

impl Default for SimControl {
    fn default() -> Self {
        Self {
            impedances: [4.2, 5.1, 3.8, 4.6, 5.5, 4.9, 6.2, 5.8],
            battery_pct: 94.0,
            dirty_meta: true,
            received_markers: VecDeque::with_capacity(32),
        }
    }
}

struct Args {
    http_port: u16,
    ws_port: u16,
}

fn parse_args() -> Args {
    let mut http_port = 8784u16;
    let mut ws_port = 8765u16;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--port" => {
                if let Some(v) = it.next().and_then(|s| s.parse().ok()) {
                    http_port = v;
                }
            }
            "--ws-port" => {
                if let Some(v) = it.next().and_then(|s| s.parse().ok()) {
                    ws_port = v;
                }
            }
            _ => {}
        }
    }
    Args { http_port, ws_port }
}

fn find_wasm_bytes() -> Option<Vec<u8>> {
    let candidates = [
        PathBuf::from("target/wasm32-unknown-unknown/release/labstream_wasm.wasm"),
        PathBuf::from("../../target/wasm32-unknown-unknown/release/labstream_wasm.wasm"),
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/wasm32-unknown-unknown/release/labstream_wasm.wasm"),
    ];
    for p in &candidates {
        if let Ok(bytes) = std::fs::read(p) {
            return Some(bytes);
        }
    }
    None
}

fn push_all_metadata(outlet: &Outlet, sim: &SimControl) {
    for (i, &lbl) in LABELS.iter().enumerate() {
        let _ = outlet.push_metadata(&format!("imp.{lbl}"), &format!("{:.1}", sim.impedances[i]));
    }
    let _ = outlet.push_metadata("battery_pct", &format!("{:.0}", sim.battery_pct));
    let worst = sim
        .impedances
        .iter()
        .copied()
        .fold(0.0f64, f64::max);
    let status = if worst > 25.0 {
        "POOR_CONTACT"
    } else if worst > 10.0 {
        "MARGINAL"
    } else {
        "GOOD"
    };
    let _ = outlet.push_metadata("cap_status", status);
}

fn run_sender(ctrl: Arc<Mutex<SimControl>>, stop: Arc<AtomicBool>) {
    let info = StreamInfo::builder(STREAM_NAME, "EEG", Format::Float32)
        .rate(SRATE)
        .source_id("demo5-browser-eeg-8ch")
        .channel_labels(LABELS)
        .build()
        .expect("build Demo5 StreamInfo");

    let outlet = Outlet::new(info).expect("create Demo5 Outlet");
    eprintln!("[demo5-sender] publishing {STREAM_NAME} ({CHANNELS} ch @ {SRATE} Hz) with live meta/1");

    let mut seq: u64 = 0;
    let mut flat = vec![0.0f32; CHUNK_SAMPLES * CHANNELS];
    let t0 = Instant::now();
    let mut last_meta = Instant::now() - Duration::from_secs(2);

    while !stop.load(Ordering::Relaxed) {
        let imps = {
            let mut c = ctrl.lock().unwrap();
            let need_meta = c.dirty_meta || last_meta.elapsed() >= Duration::from_secs(1);
            if need_meta {
                // Gentle realistic electrode drift (+/- 0.2 kOhm)
                for (i, imp) in c.impedances.iter_mut().enumerate() {
                    let jitter = (((seq / 250 + i as u64) % 5) as f64 - 2.0) * 0.1;
                    *imp = (*imp + jitter).clamp(2.5, 65.0);
                }
                push_all_metadata(&outlet, &c);
                c.dirty_meta = false;
                last_meta = Instant::now();
            }
            c.impedances
        };

        for k in 0..CHUNK_SAMPLES {
            let s = seq + k as u64;
            let t = s as f64 / SRATE;
            let row = &mut flat[k * CHANNELS..(k + 1) * CHANNELS];
            for (ch, slot) in row.iter_mut().enumerate() {
                let ph = ch as f64 * 0.55;
                let alpha = 18.0 * (TAU * 10.0 * t + ph).sin();
                let theta = 8.0 * (TAU * 6.0 * t + ph * 1.3).sin();
                let beta = 4.5 * (TAU * 21.0 * t + ph * 0.7).sin();
                // Poor electrode contact (> 20 kOhm) picks up 50 Hz mains hum.
                let hum = if imps[ch] > 20.0 {
                    ((imps[ch] - 18.0) * 1.4) * (TAU * 50.0 * t).sin()
                } else {
                    0.0
                };
                // Periodic gentle eye blink on Fp1/Fp2 every 4 seconds.
                let blink_t = (t % 4.0) - 2.0;
                let blink = if ch < 2 && blink_t.abs() < 0.15 {
                    55.0 * (-blink_t * blink_t / 0.004).exp()
                } else {
                    0.0
                };
                *slot = (alpha + theta + beta + hum + blink) as f32;
            }
        }
        let end_ts = labstream_net::clock();
        let _ = outlet.push_chunk(&flat, end_ts);
        seq += CHUNK_SAMPLES as u64;

        let target = t0 + Duration::from_secs_f64(seq as f64 / SRATE);
        let now = Instant::now();
        if target > now {
            thread::sleep(target - now);
        }
    }
}

/// Native Rust LSL inlet listening to `BrowserMarkers` (created by `WsBridge` when
/// the browser sends a marker) to confirm true bidirectional browser -> LSL delivery.
fn run_marker_listener(ctrl: Arc<Mutex<SimControl>>, stop: Arc<AtomicBool>) {
    let q = Query::name(MARKER_STREAM);
    while !stop.load(Ordering::Relaxed) {
        let info = match labstream::resolve_first(&q, Duration::from_millis(500)) {
            Ok(Some(i)) => i,
            _ => continue,
        };
        let mut inlet = match Inlet::builder(&info).open(Duration::from_secs(3)) {
            Ok(i) => i,
            Err(_) => continue,
        };
        eprintln!("[demo5-markers] native LSL inlet attached to {MARKER_STREAM}");
        while !stop.load(Ordering::Relaxed) {
            match inlet.pull_text(Duration::from_millis(200)) {
                Ok(Some((ts, vals))) => {
                    let text = vals.join(" ");
                    let mut c = ctrl.lock().unwrap();
                    c.received_markers.push_front((ts, text));
                    while c.received_markers.len() > 12 {
                        c.received_markers.pop_back();
                    }
                }
                Ok(None) => {}
                Err(_) => break,
            }
        }
    }
}

fn markers_json(ctrl: &SimControl) -> String {
    use std::fmt::Write as _;
    let mut out = String::from("{\"markers\":[");
    for (i, (ts, text)) in ctrl.received_markers.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let esc = text.replace('\\', "\\\\").replace('"', "\\\"");
        let _ = write!(out, "{{\"ts\":{ts:.3},\"value\":\"{esc}\"}}");
    }
    out.push_str("]}");
    out
}

fn handle_http(
    mut stream: TcpStream,
    ctrl: &Arc<Mutex<SimControl>>,
    wasm: &Arc<Option<Vec<u8>>>,
    ws_port: u16,
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

    match (method, path) {
        ("GET", "/") => {
            let html = INDEX_HTML.replace("__WS_PORT__", &ws_port.to_string());
            write_resp(&mut stream, "200 OK", "text/html; charset=utf-8", html.as_bytes());
        }
        ("GET", "/labstream.js") => {
            write_resp(
                &mut stream,
                "200 OK",
                "application/javascript; charset=utf-8",
                LABSTREAM_JS.as_bytes(),
            );
        }
        ("GET", "/labstream_wasm.wasm") => {
            if let Some(bytes) = wasm.as_ref() {
                write_resp(&mut stream, "200 OK", "application/wasm", bytes);
            } else {
                write_resp(
                    &mut stream,
                    "404 Not Found",
                    "text/plain",
                    b"labstream_wasm.wasm not built; run: cargo build --release --target wasm32-unknown-unknown -p labstream-wasm",
                );
            }
        }
        ("GET", "/api/markers") => {
            let c = ctrl.lock().unwrap();
            let body = markers_json(&c);
            write_resp(&mut stream, "200 OK", "application/json", body.as_bytes());
        }
        ("POST", "/api/action/loosen_c3") => {
            let mut c = ctrl.lock().unwrap();
            c.impedances[2] = 48.5; // C3
            c.impedances[0] = 16.4; // Fp1 marginal
            c.dirty_meta = true;
            write_resp(&mut stream, "200 OK", "application/json", b"{\"ok\":true}");
        }
        ("POST", "/api/action/apply_gel") => {
            let mut c = ctrl.lock().unwrap();
            c.impedances = [4.0, 4.6, 3.5, 4.2, 4.8, 4.4, 5.1, 4.9];
            c.dirty_meta = true;
            write_resp(&mut stream, "200 OK", "application/json", b"{\"ok\":true}");
        }
        ("POST", "/api/action/low_battery") => {
            let mut c = ctrl.lock().unwrap();
            c.battery_pct = if c.battery_pct > 25.0 { 14.0 } else { 94.0 };
            c.dirty_meta = true;
            write_resp(&mut stream, "200 OK", "application/json", b"{\"ok\":true}");
        }
        _ => {
            write_resp(&mut stream, "404 Not Found", "text/plain", b"not found");
        }
    }
}

fn write_resp(stream: &mut TcpStream, status: &str, ct: &str, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
}

fn main() {
    let args = parse_args();
    labstream_net::config::set_content("[labstream]\nFeatures = {seq, resume, meta}\n");

    let wasm = Arc::new(find_wasm_bytes());
    if wasm.is_none() {
        eprintln!("[demo5] warning: labstream_wasm.wasm not found; build with:");
        eprintln!("        cargo build --release --target wasm32-unknown-unknown -p labstream-wasm");
    }

    let stop = Arc::new(AtomicBool::new(false));
    let ctrl = Arc::new(Mutex::new(SimControl::default()));

    // Start the real LSL outlet with live `meta/1` updates.
    {
        let c = Arc::clone(&ctrl);
        let s = Arc::clone(&stop);
        thread::spawn(move || run_sender(c, s));
    }

    // Start the real RFC 6455 WebSocket bridge (`labstream::ws::WsBridge`).
    let ws_opts = WsOptions {
        bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), args.ws_port),
        marker_stream_name: MARKER_STREAM.to_string(),
        ..Default::default()
    };
    let _bridge = WsBridge::start(ws_opts).expect("start WsBridge");

    // Start the native LSL inlet that confirms browser-injected markers.
    {
        let c = Arc::clone(&ctrl);
        let s = Arc::clone(&stop);
        thread::spawn(move || run_marker_listener(c, s));
    }

    let addr = format!("127.0.0.1:{}", args.http_port);
    let listener = TcpListener::bind(&addr).expect("bind HTTP server");
    println!("============================================================");
    println!("  labstream Demo 5 — Browser WASM + Live meta/1 Impedance");
    println!("  WebSocket Bridge: ws://127.0.0.1:{}/", args.ws_port);
    println!("  Open in browser:  http://{addr}/");
    println!("============================================================");

    for conn in listener.incoming().flatten() {
        let c = Arc::clone(&ctrl);
        let w = Arc::clone(&wasm);
        let wp = args.ws_port;
        thread::spawn(move || handle_http(conn, &c, &w, wp));
    }
}

const INDEX_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>labstream Demo 5 — Browser WASM + Live meta/1 Impedance</title>
<style>
  :root {
    --bg: #0b0f17;
    --panel: #131a26;
    --border: #233046;
    --text: #e6edf7;
    --muted: #8ea0b8;
    --green: #34d399;
    --gold: #fbbf24;
    --red: #f87171;
    --blue: #38bdf8;
    --purple: #c084fc;
  }
  * { box-sizing: border-box; }
  body {
    margin: 0; padding: 18px; background: var(--bg); color: var(--text);
    font-family: ui-sans-serif, system-ui, -apple-system, Segoe UI, Roboto, sans-serif;
  }
  .wrap { max-width: 1240px; margin: 0 auto; }
  header {
    display: flex; flex-wrap: wrap; justify-content: space-between; align-items: center;
    gap: 12px; margin-bottom: 14px;
  }
  h1 { margin: 0; font-size: 20px; }
  .sub { color: var(--muted); font-size: 13px; margin-top: 3px; }
  .toolbar { display: flex; gap: 8px; flex-wrap: wrap; align-items: center; }
  button {
    background: #1b2536; color: var(--text); border: 1px solid var(--border);
    border-radius: 7px; padding: 7px 11px; font-size: 12px; cursor: pointer; font-weight: 600;
  }
  button:hover { border-color: #4b6588; }
  button.warn { background: #450a0a; border-color: var(--red); color: #fecaca; }
  button.ok { background: #065f46; border-color: var(--green); color: #ecfdf5; }
  button.accent { background: #3b0764; border-color: var(--purple); color: #f3e8ff; }
  .pill {
    padding: 4px 10px; border-radius: 999px; font-size: 12px; font-weight: 700;
    background: #052e16; border: 1px solid #14532d; color: var(--green);
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  }
  .main-grid { display: grid; grid-template-columns: 1.75fr 1fr; gap: 14px; }
  @media (max-width: 920px) { .main-grid { grid-template-columns: 1fr; } }
  .card {
    background: var(--panel); border: 1px solid var(--border);
    border-radius: 12px; padding: 14px;
  }
  .card h2 {
    margin: 0 0 10px 0; font-size: 14px; color: #cbd5e1;
    display: flex; justify-content: space-between; align-items: center; gap: 8px; flex-wrap: wrap;
  }
  canvas { display: block; width: 100%; background: #090d14; border-radius: 8px; border: 1px solid #1c2638; }
  .meta-list {
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 11px;
    max-height: 120px; overflow-y: auto; background: #090d14;
    border: 1px solid #1c2638; border-radius: 8px; padding: 8px; margin-top: 10px;
  }
  .meta-row { display: flex; justify-content: space-between; padding: 2px 0; border-bottom: 1px solid #141d2e; }
  .marker-bar { display: flex; gap: 6px; flex-wrap: wrap; margin-top: 8px; }
</style>
</head>
<body>
<div class="wrap">
  <header>
    <div>
      <h1>Demo 5 — Zero-Install Browser Lab (WASM + Live <code>meta/1</code>)</h1>
      <div class="sub">27 KB <code>labstream-wasm</code> decoding binary LSL WebSocket frames + live <code>meta/1</code> electrode impedance + Browser &rarr; LSL markers</div>
    </div>
    <div class="toolbar">
      <span id="statusPill" class="pill">CONNECTING...</span>
      <button id="loosenBtn" class="warn">Loosen C3 Electrode</button>
      <button id="gelBtn" class="ok">Apply Gel (Fix All)</button>
      <button id="battBtn">Toggle Low Battery</button>
      <button id="pauseBtn">Pause</button>
      <button id="exportBtn" class="ok">Export PNG</button>
    </div>
  </header>

  <div class="main-grid">
    <div class="card">
      <h2>
        <span>8-Channel Live EEG (Decoded in Browser via WebAssembly)</span>
        <span id="wasmStats" style="font-family:monospace;font-size:12px;color:var(--blue)">0 samples/s</span>
      </h2>
      <canvas id="eegCanvas" width="760" height="440"></canvas>
      <div class="marker-bar">
        <span style="font-size:12px;color:var(--muted);align-self:center;font-weight:700">Browser &rarr; LSL Marker:</span>
        <button class="accent" data-mk="EYES_OPEN">EYES_OPEN</button>
        <button class="accent" data-mk="EYES_CLOSED">EYES_CLOSED</button>
        <button class="accent" data-mk="STIM_FLASH">STIM_FLASH</button>
        <button class="accent" data-mk="ARTIFACT_MARK">ARTIFACT_MARK</button>
      </div>
    </div>

    <div class="card">
      <h2>
        <span>10-20 Head Cap (Live <code>meta/1</code>)</span>
        <span id="capBadge" style="font-family:monospace;font-size:12px;color:var(--green)">GOOD | BATT --%</span>
      </h2>
      <canvas id="capCanvas" width="380" height="260"></canvas>
      <div class="meta-list" id="metaLog"><div style="color:var(--muted)">Waiting for live meta/1 frames...</div></div>
      <div class="meta-list" id="markerLog" style="margin-top:8px;max-height:92px"><div style="color:var(--muted)">Click a marker button to inject Browser &rarr; LSL...</div></div>
    </div>
  </div>
</div>

<script type="module">
import { loadLabstreamWasm, LabstreamBrowserClient } from '/labstream.js';

const LABELS = ['Fp1','Fp2','C3','C4','P3','P4','O1','O2'];
const POS = {
  Fp1: [0.36, 0.22], Fp2: [0.64, 0.22],
  C3:  [0.28, 0.50], C4:  [0.72, 0.50],
  P3:  [0.34, 0.72], P4:  [0.66, 0.72],
  O1:  [0.41, 0.86], O2:  [0.59, 0.86],
};
const BUF_LEN = 750; // 3 s at 250 Hz
const rings = LABELS.map(() => new Float32Array(BUF_LEN));
let writeIdx = 0;
let totalSamples = 0;
let metaFrames = 0;
let paused = false;
let lastDecodeUs = 0;
let rateSamples = 0;
let rateT0 = performance.now();
let currentSps = 0;

const impedances = { Fp1: 4.2, Fp2: 5.1, C3: 3.8, C4: 4.6, P3: 5.5, P4: 4.9, O1: 6.2, O2: 5.8 };
let batteryPct = '94';
let capStatus = 'GOOD';
const metaHistory = [];
const visualMarkers = [];

document.getElementById('loosenBtn').onclick = () => fetch('/api/action/loosen_c3', { method: 'POST' });
document.getElementById('gelBtn').onclick = () => fetch('/api/action/apply_gel', { method: 'POST' });
document.getElementById('battBtn').onclick = () => fetch('/api/action/low_battery', { method: 'POST' });
document.getElementById('pauseBtn').onclick = () => {
  paused = !paused;
  document.getElementById('pauseBtn').textContent = paused ? 'Resume' : 'Pause';
};

function impColor(v) {
  if (v > 25) return '#f87171';
  if (v > 10) return '#fbbf24';
  return '#34d399';
}

function drawCap() {
  const c = document.getElementById('capCanvas');
  const ctx = c.getContext('2d');
  const W = c.width, H = c.height;
  ctx.clearRect(0, 0, W, H);
  const cx = W / 2, cy = H / 2 + 6, r = 104;

  // Head outline + nose + ears
  ctx.strokeStyle = '#334155'; ctx.lineWidth = 2.5;
  ctx.beginPath(); ctx.arc(cx, cy, r, 0, Math.PI * 2); ctx.stroke();
  ctx.beginPath(); ctx.moveTo(cx - 12, cy - r); ctx.lineTo(cx, cy - r - 16); ctx.lineTo(cx + 12, cy - r); ctx.stroke();
  ctx.beginPath(); ctx.ellipse(cx - r - 6, cy, 6, 18, 0, 0, Math.PI * 2); ctx.stroke();
  ctx.beginPath(); ctx.ellipse(cx + r + 6, cy, 6, 18, 0, 0, Math.PI * 2); ctx.stroke();

  for (const lbl of LABELS) {
    const [nx, ny] = POS[lbl];
    const x = cx + (nx - 0.5) * r * 2.1;
    const y = cy + (ny - 0.5) * r * 2.1;
    const val = impedances[lbl] ?? 5.0;
    const col = impColor(val);

    ctx.beginPath(); ctx.arc(x, y, 18, 0, Math.PI * 2);
    ctx.fillStyle = col + '28'; ctx.fill();
    ctx.lineWidth = 2.5; ctx.strokeStyle = col; ctx.stroke();

    ctx.fillStyle = '#f8fafc'; ctx.font = 'bold 11px sans-serif'; ctx.textAlign = 'center';
    ctx.fillText(lbl, x, y - 1);
    ctx.fillStyle = col; ctx.font = 'bold 10px monospace';
    ctx.fillText(val.toFixed(1) + 'k', x, y + 11);
  }
  ctx.textAlign = 'left';
}

function drawEeg() {
  const c = document.getElementById('eegCanvas');
  const ctx = c.getContext('2d');
  const W = c.width, H = c.height;
  ctx.clearRect(0, 0, W, H);
  const rowH = H / CHANNELS;

  for (let ch = 0; ch < CHANNELS; ch++) {
    const midY = (ch + 0.5) * rowH;
    const lbl = LABELS[ch];
    const col = impColor(impedances[lbl] ?? 5.0);

    ctx.strokeStyle = '#141e2e'; ctx.lineWidth = 1;
    ctx.beginPath(); ctx.moveTo(52, midY); ctx.lineTo(W, midY); ctx.stroke();

    ctx.fillStyle = col; ctx.font = 'bold 12px monospace';
    ctx.fillText(lbl, 8, midY + 4);

    ctx.strokeStyle = impedances[lbl] > 25 ? '#f87171' : '#38bdf8';
    ctx.lineWidth = 1.5;
    ctx.beginPath();
    const ring = rings[ch];
    for (let i = 0; i < BUF_LEN; i++) {
      const idx = (writeIdx + i) % BUF_LEN;
      const x = 52 + (i / (BUF_LEN - 1)) * (W - 60);
      const y = midY - (ring[idx] / 75.0) * (rowH * 0.44);
      if (i === 0) ctx.moveTo(x, y); else ctx.lineTo(x, y);
    }
    ctx.stroke();
  }

  // Draw recent browser-injected markers
  for (const mk of visualMarkers) {
    const age = totalSamples - mk.sampleAt;
    if (age < 0 || age > BUF_LEN) continue;
    const x = 52 + ((BUF_LEN - age) / (BUF_LEN - 1)) * (W - 60);
    ctx.strokeStyle = '#c084fc'; ctx.lineWidth = 1.5; ctx.setLineDash([4, 3]);
    ctx.beginPath(); ctx.moveTo(x, 0); ctx.lineTo(x, H); ctx.stroke();
    ctx.setLineDash([]);
    ctx.fillStyle = '#c084fc'; ctx.font = 'bold 11px monospace';
    ctx.fillText(mk.label, Math.min(W - 90, x + 4), 14);
  }
}

function renderMetaLog() {
  const el = document.getElementById('metaLog');
  el.innerHTML = metaHistory.slice(0, 8).map(m =>
    `<div class="meta-row"><span style="color:#94a3b8">#${m.seq} ${m.key}</span><span style="color:#34d399;font-weight:700">${m.value}</span></div>`
  ).join('');
  const bColor = Number(batteryPct) < 25 ? '#f87171' : (capStatus === 'GOOD' ? '#34d399' : '#fbbf24');
  const cb = document.getElementById('capBadge');
  cb.style.color = bColor;
  cb.textContent = `${capStatus} | BATT ${batteryPct}% | ${metaFrames} meta/1`;
}

async function pollNativeMarkers() {
  try {
    const r = await fetch('/api/markers');
    if (r.ok) {
      const d = await r.json();
      if (d.markers.length > 0) {
        document.getElementById('markerLog').innerHTML = d.markers.slice(0, 5).map(m =>
          `<div class="meta-row"><span style="color:#c084fc;font-weight:700">✓ LSL RX: ${m.value}</span><span style="color:#64748b">t=${m.ts.toFixed(2)}s</span></div>`
        ).join('');
      }
    }
  } catch (_) {}
  setTimeout(pollNativeMarkers, 400);
}

const CHANNELS = 8;

async function start() {
  const pill = document.getElementById('statusPill');
  const wasm = await loadLabstreamWasm('/labstream_wasm.wasm');
  const client = new LabstreamBrowserClient(`ws://127.0.0.1:__WS_PORT__/`, wasm);

  client.onSamples = (decoded) => {
    const t0 = performance.now();
    if (!paused) {
      for (let i = 0; i < decoded.count; i++) {
        for (let ch = 0; ch < CHANNELS; ch++) {
          rings[ch][writeIdx] = decoded.values[i * CHANNELS + ch];
        }
        writeIdx = (writeIdx + 1) % BUF_LEN;
        totalSamples++;
      }
    }
    rateSamples += decoded.count;
    lastDecodeUs = (performance.now() - t0) * 1000;
    const elapsed = (performance.now() - rateT0) / 1000;
    if (elapsed >= 0.5) {
      currentSps = rateSamples / elapsed;
      rateSamples = 0;
      rateT0 = performance.now();
      document.getElementById('wasmStats').textContent =
        `${currentSps.toFixed(0)} SPS | WASM decode ${lastDecodeUs.toFixed(0)} µs/frame | ${totalSamples.toLocaleString('en-US')} samples`;
    }
  };

  client.onMetadata = (msg) => {
    metaFrames++;
    if (msg.key.startsWith('imp.')) {
      const ch = msg.key.slice(4);
      impedances[ch] = parseFloat(msg.value);
    } else if (msg.key === 'battery_pct') {
      batteryPct = msg.value;
    } else if (msg.key === 'cap_status') {
      capStatus = msg.value;
    }
    metaHistory.unshift(msg);
    if (metaHistory.length > 20) metaHistory.pop();
    renderMetaLog();
  };

  await client.connect();
  client.subscribe('Demo5BrowserEEG', 0);
  pill.textContent = '✓ WASM + WS LIVE';

  document.querySelectorAll('[data-mk]').forEach(b => {
    b.onclick = () => {
      const val = b.dataset.mk;
      client.sendMarker(val, 'BrowserMarkers', 0.0);
      visualMarkers.push({ label: val, sampleAt: totalSamples });
      if (visualMarkers.length > 12) visualMarkers.shift();
    };
  });

  function frame() {
    drawEeg();
    drawCap();
    requestAnimationFrame(frame);
  }
  frame();
  pollNativeMarkers();
}

document.getElementById('exportBtn').onclick = () => {
  const out = document.createElement('canvas');
  out.width = 1180; out.height = 520;
  const ctx = out.getContext('2d');
  ctx.fillStyle = '#0b0f17'; ctx.fillRect(0, 0, out.width, out.height);
  ctx.fillStyle = '#e6edf7'; ctx.font = 'bold 21px sans-serif';
  ctx.fillText('labstream v0.2.0 — Browser WASM (27 KB) + Live meta/1 Impedance', 24, 36);
  ctx.fillStyle = '#34d399'; ctx.font = 'bold 13px monospace';
  ctx.fillText(`WASM: ${currentSps.toFixed(0)} SPS | meta/1 frames: ${metaFrames} | Cap: ${capStatus} | Battery: ${batteryPct}%`, 24, 58);
  ctx.drawImage(document.getElementById('eegCanvas'), 24, 74, 740, 420);
  ctx.drawImage(document.getElementById('capCanvas'), 784, 74, 372, 260);
  const a = document.createElement('a');
  a.download = 'labstream_demo5_browser_meta.png';
  a.href = out.toDataURL('image/png');
  a.click();
};

start().catch(err => {
  const pill = document.getElementById('statusPill');
  pill.textContent = 'ERROR: ' + err.message;
  pill.style.color = '#f87171';
});
</script>
</body>
</html>
"#;
