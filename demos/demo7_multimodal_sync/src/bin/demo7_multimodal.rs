//! Demo 7: Multi-Modal Heterogeneous-Rate Fusion (`labstream::SyncGroup`, step 18).
//! Fuses EEG (500 Hz) + ECG (250 Hz, 1.7 ms offset) + GSR (120 Hz) onto one unified
//! 250 Hz time grid with explicit `gap_mask` handling on sensor dropouts.

use labstream_net::{Inlet, Outlet, StreamInfo, SyncConfig, SyncGroup};
use labstream_wire::{Format, PackedSample};
use std::collections::VecDeque;
use std::f64::consts::TAU;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const OUT_RATE: f64 = 250.0;
const RING_LEN: usize = 1000; // 4.0 seconds at 250 Hz

/// Synthetic physiological waveforms phase-locked to the same 1.0 Hz cardiac cycle.
fn synth_eeg(t: f64) -> f32 {
    // 10 Hz alpha + 22 Hz beta + evoked cortical pulse 120 ms after each R-peak
    let beat = t.rem_euclid(1.0);
    let evoked = 28.0 * (-((beat - 0.12) / 0.025).powi(2)).exp();
    let alpha = 16.0 * (TAU * 10.0 * t).sin();
    let beta = 6.0 * (TAU * 22.0 * t).sin();
    (alpha + beta + evoked) as f32
}

fn synth_ecg(t: f64) -> f32 {
    // Synthetic P-QRS-T complex every 1.0 s (R-peak at beat = 0.00 s)
    let beat = t.rem_euclid(1.0);
    let dt_r = if beat > 0.5 { beat - 1.0 } else { beat };
    let p = 0.18 * (-((dt_r + 0.16) / 0.025).powi(2)).exp();
    let q = -0.15 * (-((dt_r + 0.028) / 0.008).powi(2)).exp();
    let r = 1.25 * (-(dt_r / 0.011).powi(2)).exp();
    let s = -0.28 * (-((dt_r - 0.026) / 0.009).powi(2)).exp();
    let tw = 0.34 * (-((dt_r - 0.24) / 0.045).powi(2)).exp();
    (p + q + r + s + tw) as f32
}

fn synth_gsr(t: f64) -> f32 {
    // Slow skin conductance + respiratory sinus modulation (0.25 Hz + 1.0 Hz harmonic)
    let resp = 0.65 * (TAU * 0.25 * t).sin();
    let pulse = 0.25 * (TAU * 1.0 * t - 0.6).sin();
    (resp + pulse) as f32
}

#[derive(Clone, Copy)]
struct FusedPoint {
    t: f64,
    eeg: f64,
    ecg: f64,
    gsr: f64,
    gap_mask: u64,
}

struct SharedState {
    aligned_total: u64,
    gap_samples_total: u64,
    gap_counts: [u64; 3],
    lookahead_ms: f64,
    last_gap_mask: u64,
    ring: VecDeque<FusedPoint>,
}

impl Default for SharedState {
    fn default() -> Self {
        Self {
            aligned_total: 0,
            gap_samples_total: 0,
            gap_counts: [0; 3],
            lookahead_ms: 0.0,
            last_gap_mask: 0,
            ring: VecDeque::with_capacity(RING_LEN + 32),
        }
    }
}

struct DropFlags {
    /// Epoch milliseconds until which each sensor (0=EEG, 1=ECG, 2=GSR) skips pushes.
    until_ms: [AtomicU64; 3],
    t0: Instant,
}

impl DropFlags {
    fn new() -> Self {
        Self {
            until_ms: [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)],
            t0: Instant::now(),
        }
    }

    fn trigger(&self, idx: usize, duration_ms: u64) {
        let now_ms = self.t0.elapsed().as_millis() as u64;
        self.until_ms[idx].store(now_ms + duration_ms, Ordering::Relaxed);
    }

    fn is_dropped(&self, idx: usize) -> bool {
        let now_ms = self.t0.elapsed().as_millis() as u64;
        now_ms < self.until_ms[idx].load(Ordering::Relaxed)
    }
}

fn start_sensor_thread(
    name: &'static str,
    srate: f64,
    offset_sec: f64,
    drop_idx: usize,
    drops: Arc<DropFlags>,
    stop: Arc<AtomicBool>,
    synth: fn(f64) -> f32,
) -> StreamInfo {
    let mut info = StreamInfo::new(name, "Sensor", 1, Format::Float32, srate);
    info.source_id = format!("demo7-{name}");
    let outlet = Outlet::new(info).expect("create outlet");
    let pub_info = outlet.info().clone();

    thread::spawn(move || {
        let chunk_n = ((srate * 0.02).round() as usize).max(1); // ~20 ms chunks
        let t0 = Instant::now();
        let base_clock = labstream_net::clock();
        let mut seq: u64 = 0;

        while !stop.load(Ordering::Relaxed) {
            let dropped = drops.is_dropped(drop_idx);
            if !dropped {
                let mut block = Vec::with_capacity(chunk_n);
                for k in 0..chunk_n {
                    let s = seq + k as u64;
                    let t_rel = (s as f64) / srate + offset_sec;
                    let ts = base_clock + t_rel;
                    let val = synth(t_rel);
                    block.push(PackedSample::from_f32(ts, &[val]));
                }
                let end_ts = base_clock + ((seq + chunk_n as u64 - 1) as f64) / srate + offset_sec;
                outlet.push_chunk_packed(&block, end_ts);
            }
            seq += chunk_n as u64;
            let target = t0 + Duration::from_secs_f64(seq as f64 / srate);
            let now = Instant::now();
            if target > now {
                thread::sleep(target - now);
            }
        }
    });

    pub_info
}

fn fmt_opt(v: f64) -> String {
    if v.is_finite() {
        format!("{v:.3}")
    } else {
        "null".to_string()
    }
}

fn state_json(st: &SharedState) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(16384);
    let _ = write!(
        out,
        "{{\"out_rate\":{OUT_RATE:.1},\"lookahead_ms\":{:.1},\"aligned_total\":{},\"gap_samples_total\":{},\"gap_counts\":[{},{},{}],\"last_gap_mask\":{},\"samples\":[",
        st.lookahead_ms,
        st.aligned_total,
        st.gap_samples_total,
        st.gap_counts[0],
        st.gap_counts[1],
        st.gap_counts[2],
        st.last_gap_mask
    );
    // Emit every 2nd sample (500 points = 4 s @ 125 pts/s) for crisp, fast browser rendering.
    let mut first = true;
    for (i, p) in st.ring.iter().enumerate() {
        if i % 2 != 0 && p.gap_mask == 0 {
            continue;
        }
        if !first {
            out.push(',');
        }
        first = false;
        let _ = write!(
            out,
            "[{:.4},{},{},{},{}]",
            p.t,
            fmt_opt(p.eeg),
            fmt_opt(p.ecg),
            fmt_opt(p.gsr),
            p.gap_mask
        );
    }
    out.push_str("]}");
    out
}

fn handle_http(mut stream: TcpStream, state: &Arc<Mutex<SharedState>>, drops: &Arc<DropFlags>) {
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
        ("POST", "/api/drop/eeg") => {
            drops.trigger(0, 800);
            ("200 OK", "application/json", "{\"ok\":true}".to_string())
        }
        ("POST", "/api/drop/ecg") => {
            drops.trigger(1, 800);
            ("200 OK", "application/json", "{\"ok\":true}".to_string())
        }
        ("POST", "/api/drop/gsr") => {
            drops.trigger(2, 800);
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
    let mut port = 8786u16;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        if a == "--port" {
            if let Some(p) = it.next().and_then(|s| s.parse().ok()) {
                port = p;
            }
        }
    }

    let stop = Arc::new(AtomicBool::new(false));
    let drops = Arc::new(DropFlags::new());
    let state = Arc::new(Mutex::new(SharedState::default()));

    // Start 3 heterogeneous-rate streams: EEG @ 500 Hz, ECG @ 250 Hz (+1.7 ms offset), GSR @ 120 Hz.
    let info_eeg = start_sensor_thread(
        "Demo7_EEG",
        500.0,
        0.0,
        0,
        Arc::clone(&drops),
        Arc::clone(&stop),
        synth_eeg,
    );
    let info_ecg = start_sensor_thread(
        "Demo7_ECG",
        250.0,
        0.0017,
        1,
        Arc::clone(&drops),
        Arc::clone(&stop),
        synth_ecg,
    );
    let info_gsr = start_sensor_thread(
        "Demo7_GSR",
        120.0,
        0.0031,
        2,
        Arc::clone(&drops),
        Arc::clone(&stop),
        synth_gsr,
    );

    let in_eeg = Inlet::open(&info_eeg, Duration::from_secs(3)).expect("open EEG inlet");
    let in_ecg = Inlet::open(&info_ecg, Duration::from_secs(3)).expect("open ECG inlet");
    let in_gsr = Inlet::open(&info_gsr, Duration::from_secs(3)).expect("open GSR inlet");

    let mut cfg = SyncConfig::new(OUT_RATE);
    cfg.auto_postprocess = false; // same local monotonic clock; pure resampler alignment
    let mut group = SyncGroup::new(vec![in_eeg, in_ecg, in_gsr], cfg).expect("create SyncGroup");
    let lookahead_ms = group.lookahead_seconds() * 1000.0;
    state.lock().unwrap().lookahead_ms = lookahead_ms;
    eprintln!(
        "[demo7] SyncGroup active: 500 Hz + 250 Hz + 120 Hz -> {:.1} Hz (lookahead {:.1} ms)",
        group.output_rate(),
        lookahead_ms
    );

    {
        let st = Arc::clone(&state);
        let sp = Arc::clone(&stop);
        thread::spawn(move || {
            let mut batch = Vec::with_capacity(64);
            while !sp.load(Ordering::Relaxed) {
                batch.clear();
                let n = match group.pull(&mut batch, 64, Duration::from_millis(25)) {
                    Ok(n) => n,
                    Err(_) => continue,
                };
                if n == 0 {
                    continue;
                }
                let mut g = st.lock().unwrap();
                for s in &batch {
                    g.aligned_total += 1;
                    g.last_gap_mask = s.gap_mask;
                    if s.gap_mask != 0 {
                        g.gap_samples_total += 1;
                        for b in 0..3 {
                            if (s.gap_mask & (1 << b)) != 0 {
                                g.gap_counts[b] += 1;
                            }
                        }
                    }
                    g.ring.push_back(FusedPoint {
                        t: s.timestamp,
                        eeg: s.values[0],
                        ecg: s.values[1],
                        gsr: s.values[2],
                        gap_mask: s.gap_mask,
                    });
                    while g.ring.len() > RING_LEN {
                        g.ring.pop_front();
                    }
                }
            }
        });
    }

    let addr = format!("127.0.0.1:{port}");
    let listener = TcpListener::bind(&addr).expect("bind HTTP server");
    println!("============================================================");
    println!("  labstream Demo 7 — Multi-Modal Sensor Fusion (SyncGroup)");
    println!("  500 Hz EEG + 250 Hz ECG + 120 Hz GSR -> 250 Hz Unified Grid");
    println!("  Open in browser:  http://{addr}/");
    println!("============================================================");

    for conn in listener.incoming().flatten() {
        let st = Arc::clone(&state);
        let dr = Arc::clone(&drops);
        thread::spawn(move || handle_http(conn, &st, &dr));
    }
}

const INDEX_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>labstream Demo 7 — Multi-Modal Sensor Fusion (SyncGroup)</title>
<style>
  :root {
    --bg: #0b0f17;
    --panel: #131a26;
    --border: #233046;
    --text: #e6edf7;
    --muted: #8ea0b8;
    --blue: #38bdf8;
    --green: #34d399;
    --gold: #fbbf24;
    --red: #f87171;
    --purple: #c084fc;
  }
  * { box-sizing: border-box; }
  body {
    margin: 0; padding: 18px; background: var(--bg); color: var(--text);
    font-family: ui-sans-serif, system-ui, -apple-system, Segoe UI, Roboto, sans-serif;
  }
  .wrap { max-width: 1200px; margin: 0 auto; }
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
  .card {
    background: var(--panel); border: 1px solid var(--border);
    border-radius: 12px; padding: 14px; margin-bottom: 14px;
  }
  .stats-row {
    display: flex; flex-wrap: wrap; gap: 18px; font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
    font-size: 12px; color: var(--muted); margin-bottom: 10px;
  }
  .stats-row b { color: var(--text); }
  canvas { display: block; width: 100%; background: #090d14; border-radius: 8px; border: 1px solid #1c2638; }
</style>
</head>
<body>
<div class="wrap">
  <header>
    <div>
      <h1>Demo 7 — Multi-Modal Sensor Fusion (<code>labstream::SyncGroup</code>)</h1>
      <div class="sub">Real-time Blackman-sinc resampling: <code>EEG 500 Hz</code> + <code>ECG 250 Hz</code> + <code>GSR 120 Hz</code> &rarr; unified <code>250 Hz</code> matrix with explicit <code>gap_mask</code></div>
    </div>
    <div class="toolbar">
      <button class="warn" id="dropEeg">Drop EEG (0.8 s)</button>
      <button class="warn" id="dropEcg">Drop ECG (0.8 s)</button>
      <button class="warn" id="dropGsr">Drop GSR (0.8 s)</button>
      <button id="pauseBtn">Pause</button>
      <button id="exportBtn" class="ok">Export PNG</button>
    </div>
  </header>

  <div class="card">
    <div class="stats-row" id="statsBar">Connecting to SyncGroup...</div>
    <canvas id="syncCanvas" width="1160" height="440"></canvas>
  </div>
</div>

<script>
let paused = false;
let lastState = null;

document.getElementById('dropEeg').onclick = () => fetch('/api/drop/eeg', { method: 'POST' });
document.getElementById('dropEcg').onclick = () => fetch('/api/drop/ecg', { method: 'POST' });
document.getElementById('dropGsr').onclick = () => fetch('/api/drop/gsr', { method: 'POST' });
document.getElementById('pauseBtn').onclick = () => {
  paused = !paused;
  document.getElementById('pauseBtn').textContent = paused ? 'Resume' : 'Pause';
};

const TRACKS = [
  { name: 'EEG  (500 Hz → 250 Hz, 2× down)', idx: 1, bit: 1, color: '#38bdf8', scale: 55.0 },
  { name: 'ECG  (250 Hz → 250 Hz, +1.7 ms)', idx: 2, bit: 2, color: '#34d399', scale: 1.45 },
  { name: 'GSR  (120 Hz → 250 Hz, 2.08× up)', idx: 3, bit: 4, color: '#c084fc', scale: 1.25 },
];

function draw(st) {
  const c = document.getElementById('syncCanvas');
  const ctx = c.getContext('2d');
  const W = c.width, H = c.height;
  ctx.clearRect(0, 0, W, H);

  const pts = st.samples || [];
  if (pts.length < 2) return;

  const tMax = pts[pts.length - 1][0];
  const tMin = tMax - 4.0;
  const toX = t => 14 + ((t - tMin) / 4.0) * (W - 28);
  const rowH = H / 3;

  // Vertical 1-second grid lines (aligned across all 3 modalities!)
  const firstSec = Math.ceil(tMin);
  ctx.strokeStyle = 'rgba(251, 191, 36, 0.22)'; ctx.lineWidth = 1; ctx.setLineDash([4, 4]);
  for (let s = firstSec; s <= tMax; s += 1.0) {
    const x = toX(s);
    ctx.beginPath(); ctx.moveTo(x, 0); ctx.lineTo(x, H); ctx.stroke();
  }
  ctx.setLineDash([]);

  TRACKS.forEach((tr, ti) => {
    const top = ti * rowH;
    const midY = top + rowH * 0.56;

    // Divider & zero line
    if (ti > 0) {
      ctx.strokeStyle = '#1e293b'; ctx.lineWidth = 1;
      ctx.beginPath(); ctx.moveTo(0, top); ctx.lineTo(W, top); ctx.stroke();
    }
    ctx.strokeStyle = '#141e2e';
    ctx.beginPath(); ctx.moveTo(14, midY); ctx.lineTo(W - 14, midY); ctx.stroke();

    // Shade gap_mask bands in translucent red
    for (let i = 0; i < pts.length; i++) {
      if ((pts[i][4] & tr.bit) !== 0) {
        const x1 = toX(pts[i][0]);
        const x2 = i + 1 < pts.length ? toX(pts[i + 1][0]) : x1 + 4;
        ctx.fillStyle = 'rgba(248, 113, 113, 0.22)';
        ctx.fillRect(x1, top + 24, Math.max(3, x2 - x1), rowH - 30);
      }
    }

    // Draw signal segments (breaking cleanly on NaN / gap_mask)
    ctx.strokeStyle = tr.color; ctx.lineWidth = 2.0;
    ctx.beginPath();
    let penDown = false;
    for (const p of pts) {
      const val = p[tr.idx];
      if (val === null || (p[4] & tr.bit) !== 0) {
        penDown = false;
        continue;
      }
      const x = toX(p[0]);
      const y = midY - (val / tr.scale) * (rowH * 0.38);
      if (!penDown) { ctx.moveTo(x, y); penDown = true; } else ctx.lineTo(x, y);
    }
    ctx.stroke();

    // Track header label
    ctx.fillStyle = tr.color; ctx.font = 'bold 13px monospace';
    ctx.fillText(tr.name, 16, top + 19);
    const gc = (st.gap_counts || [0,0,0])[ti];
    if (gc > 0) {
      ctx.fillStyle = '#f87171'; ctx.font = 'bold 12px monospace';
      ctx.fillText(`GAP_MASK (bit ${ti}): ${gc} flagged samples`, W - 290, top + 19);
    }
  });
}

function updateUI(st) {
  lastState = st;
  const maskBin = '0b' + st.last_gap_mask.toString(2).padStart(3, '0');
  document.getElementById('statsBar').innerHTML =
    `<span>Unified Grid: <b>${st.out_rate.toFixed(1)} Hz</b></span>` +
    `<span>Lookahead: <b>${st.lookahead_ms.toFixed(1)} ms</b></span>` +
    `<span>Aligned Matrix Rows: <b>${st.aligned_total.toLocaleString('en-US')}</b></span>` +
    `<span>Explicitly Flagged Gaps: <b style="color:${st.gap_samples_total > 0 ? '#f87171' : '#34d399'}">${st.gap_samples_total}</b></span>` +
    `<span>Latest gap_mask: <b>${maskBin}</b></span>`;
  draw(st);
}

document.getElementById('exportBtn').onclick = () => {
  if (!lastState) return;
  const st = lastState;
  const out = document.createElement('canvas');
  out.width = 1200; out.height = 520;
  const ctx = out.getContext('2d');
  ctx.fillStyle = '#0b0f17'; ctx.fillRect(0, 0, out.width, out.height);
  ctx.fillStyle = '#e6edf7'; ctx.font = 'bold 21px sans-serif';
  ctx.fillText('labstream v0.2.0 — SyncGroup Multi-Modal Fusion (500 + 250 + 120 Hz → 250 Hz)', 20, 34);
  ctx.fillStyle = '#34d399'; ctx.font = 'bold 13px monospace';
  ctx.fillText(`Aligned rows: ${st.aligned_total.toLocaleString('en-US')} | Lookahead: ${st.lookahead_ms.toFixed(1)} ms | Flagged gap samples: ${st.gap_samples_total}`, 20, 56);
  ctx.drawImage(document.getElementById('syncCanvas'), 20, 68, 1160, 440);
  const a = document.createElement('a');
  a.download = 'labstream_demo7_multimodal_sync.png';
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
  setTimeout(poll, 80);
}
poll();
</script>
</body>
</html>
"#;
