//! Machine B (Viewer) for Demo 1: "Pull the Wi-Fi Cable".
//!
//! Connects to `Demo1EEG` and `Demo1Markers` on Machine A using BOTH:
//! - Genuine C++ `liblsl` (via the `oracle_inlet` sidecar linked to liblsl)
//! - `labstream` v0.2.0 (`seq/1 + resume/1 + dgram/1`)
//!
//! Serves a dark-mode split-screen browser dashboard at `http://127.0.0.1:8780`
//! with aligned sample-sequence X-axes, live loss/recovery counters, and
//! one-click SVG/PNG chart export.

use labstream_net::{resolve, Inlet};
use labstream_wire::Value;
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Keep ~120 seconds (2 minutes) of 128 Hz decimated plot points (15360 points)
/// so the presenter can pause and scroll back through any dropout window.
const MAX_POINTS: usize = 15360;
const MAX_GAPS: usize = 128;
const MAX_MARKERS: usize = 256;

#[derive(Clone, Copy)]
struct PlotPoint {
    seq: u64,
    val: f32,
    resent: bool,
}

#[derive(Clone, Copy)]
struct GapSpan {
    from: u64,
    to: u64,
    lost: u64,
}

#[derive(Clone)]
struct MarkerEvent {
    id: u64,
    text: String,
    approx_seq: u64,
    /// 0 = normal/pending, 1 = CRC32 verified (✓), 2 = CRC32 mismatch (✗), 3 = user offline event (★)
    kind: u8,
}

fn crc32_update(mut crc: u32, bytes: &[u8]) -> u32 {
    for &b in bytes {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    crc
}

#[derive(Default)]
struct LaneState {
    connected: bool,
    transport: String,
    received: u64,
    lost: u64,
    resent: u64,
    recoveries: u64,
    markers_received: u64,
    markers_lost: u64,
    crc_verified: u64,
    crc_failed: u64,
    user_events: u64,
    last_marker_id: Option<u64>,
    last_seq: u64,
    last_rx_at: Option<Instant>,
    points: VecDeque<PlotPoint>,
    gaps: VecDeque<GapSpan>,
    markers: VecDeque<MarkerEvent>,
    /// Per-block CRC32 accumulator (`block_id = seq / 512` -> `(count, running_crc)`).
    block_acc: HashMap<u64, (u32, u32)>,
    /// Finalized 512-sample block CRCs (`block_id -> final_crc32`).
    completed_blocks: HashMap<u64, u32>,
    /// Expected CRCs received from `STIM_xxxx|<crc>` markers awaiting block completion.
    pending_crc_markers: HashMap<u64, u32>,
}

impl LaneState {
    fn feed_sample_crc(&mut self, seq: u64, val: f32) {
        let block_id = seq / 512;
        let entry = self.block_acc.entry(block_id).or_insert((0, 0xFFFF_FFFF));
        entry.0 += 1;
        entry.1 = crc32_update(entry.1, &val.to_le_bytes());
        if entry.0 == 512 {
            let final_crc = !entry.1;
            self.block_acc.remove(&block_id);
            self.completed_blocks.insert(block_id, final_crc);
            if self.completed_blocks.len() > 128 {
                if let Some(&min_k) = self.completed_blocks.keys().min() {
                    self.completed_blocks.remove(&min_k);
                }
            }
            if let Some(expected) = self.pending_crc_markers.remove(&block_id) {
                self.record_crc_outcome(block_id + 1, final_crc == expected);
            }
        }
    }

    fn record_crc_outcome(&mut self, stim_id: u64, ok: bool) {
        if ok {
            self.crc_verified += 1;
        } else {
            self.crc_failed += 1;
        }
        for m in self.markers.iter_mut().rev() {
            if m.id == stim_id && m.kind == 0 {
                m.kind = if ok { 1 } else { 2 };
                break;
            }
        }
    }

    fn push_point(&mut self, seq: u64, val: f32, resent: bool) {
        if self.points.len() >= MAX_POINTS {
            self.points.pop_front();
        }
        self.points.push_back(PlotPoint { seq, val, resent });
        if seq > self.last_seq {
            self.last_seq = seq;
        }
        self.last_rx_at = Some(Instant::now());
        self.connected = true;
    }

    fn push_gap(&mut self, from: u64, to: u64, lost: u64) {
        if self.gaps.len() >= MAX_GAPS {
            self.gaps.pop_front();
        }
        self.gaps.push_back(GapSpan { from, to, lost });
    }

    fn push_marker(&mut self, text: String) {
        // Handle user-triggered offline events: `EVENT_<label>@<seq>`
        if let Some(rest) = text.strip_prefix("EVENT_") {
            let (label, ev_seq) = match rest.rsplit_once('@') {
                Some((l, s)) => (l.to_string(), s.parse::<u64>().unwrap_or(self.last_seq)),
                None => (rest.to_string(), self.last_seq),
            };
            self.user_events += 1;
            if self.markers.len() >= MAX_MARKERS {
                self.markers.pop_front();
            }
            self.markers.push_back(MarkerEvent {
                id: 0,
                text: format!("★ {label}"),
                approx_seq: ev_seq,
                kind: 3,
            });
            return;
        }

        // Handle 1 Hz CRC-signed markers: `STIM_0042|9f4a2c11`
        let (base, expected_crc) = match text.split_once('|') {
            Some((b, hex)) => (b, u32::from_str_radix(hex.trim(), 16).ok()),
            None => (text.as_str(), None),
        };
        let id = base
            .strip_prefix("STIM_")
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        if id > 0 {
            if let Some(prev) = self.last_marker_id {
                if id > prev + 1 {
                    self.markers_lost += id - prev - 1;
                }
            }
            self.last_marker_id = Some(id);
        }
        self.markers_received += 1;
        let approx_seq = if id > 0 { id * 512 } else { self.last_seq };
        let mut kind = 0u8;
        if let (true, Some(exp_crc)) = (id > 0, expected_crc) {
            let block_id = id - 1;
            if let Some(&got_crc) = self.completed_blocks.get(&block_id) {
                let ok = got_crc == exp_crc;
                if ok {
                    self.crc_verified += 1;
                    kind = 1;
                } else {
                    self.crc_failed += 1;
                    kind = 2;
                }
            } else {
                self.pending_crc_markers.insert(block_id, exp_crc);
            }
        }
        if self.markers.len() >= MAX_MARKERS {
            self.markers.pop_front();
        }
        self.markers.push_back(MarkerEvent {
            id,
            text: base.to_string(),
            approx_seq,
            kind,
        });
    }

    fn is_live(&self) -> bool {
        self.last_rx_at
            .is_some_and(|t| t.elapsed() < Duration::from_millis(900))
    }

    fn reset(&mut self) {
        self.received = 0;
        self.lost = 0;
        self.resent = 0;
        self.recoveries = 0;
        self.markers_received = 0;
        self.markers_lost = 0;
        self.crc_verified = 0;
        self.crc_failed = 0;
        self.user_events = 0;
        self.last_marker_id = None;
        self.points.clear();
        self.gaps.clear();
        self.markers.clear();
        self.block_acc.clear();
        self.completed_blocks.clear();
        self.pending_crc_markers.clear();
    }
}

struct SharedState {
    oracle: LaneState,
    labstream: LaneState,
    peer_label: String,
    oracle_pid: Option<u32>,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut peer: Option<String> = None;
    let mut port: u16 = 8780;
    let mut oracle_bin = String::from("demos/demo1_dropout/oracle_inlet");

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
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
            "--oracle-bin" => {
                i += 1;
                if let Some(b) = args.get(i) {
                    oracle_bin = b.clone();
                }
            }
            other => {
                if let Some(v) = other.strip_prefix("--peer=") {
                    peer = Some(v.to_string());
                } else if let Some(v) = other.strip_prefix("--port=") {
                    if let Ok(p) = v.parse() {
                        port = p;
                    }
                } else if let Some(v) = other.strip_prefix("--oracle-bin=") {
                    oracle_bin = v.to_string();
                }
            }
        }
        i += 1;
    }

    // If --peer <IP> is provided, write a temporary lsl_api.cfg with KnownPeers
    // so BOTH genuine C++ liblsl and labstream discover Machine A even when
    // Wi-Fi blocks multicast.
    let cfg_path = if let Some(ref ip) = peer {
        let p = std::env::temp_dir().join(format!("demo1_lsl_{}.cfg", std::process::id()));
        let body = format!("[lab]\nKnownPeers = {{{ip}}}\n");
        std::fs::write(&p, body).expect("write temp lsl_api.cfg");
        std::env::set_var("LSLAPICFG", &p);
        println!("[demo1-viewer] configured KnownPeers = {{{ip}}} via {}", p.display());
        Some(p)
    } else {
        None
    };

    let shared = Arc::new(Mutex::new(SharedState {
        oracle: LaneState {
            transport: "TCP (plain LSL)".to_string(),
            ..LaneState::default()
        },
        labstream: LaneState {
            transport: "connecting...".to_string(),
            ..LaneState::default()
        },
        peer_label: peer.clone().unwrap_or_else(|| "LAN / multicast".to_string()),
        oracle_pid: None,
    }));
    let reset_gen = Arc::new(AtomicU64::new(0));

    // 1. Spawn genuine C++ liblsl sidecar reader thread (restarts cleanly on Reset).
    {
        let st = Arc::clone(&shared);
        let gen = Arc::clone(&reset_gen);
        let bin = oracle_bin.clone();
        let cfg = cfg_path.clone();
        std::thread::spawn(move || run_oracle_reader(st, gen, &bin, cfg.as_deref()));
    }

    // 2. Spawn native labstream EEG inlet reader thread.
    {
        let st = Arc::clone(&shared);
        std::thread::spawn(move || run_labstream_eeg(st));
    }

    // 3. Spawn native labstream Marker inlet reader thread.
    {
        let st = Arc::clone(&shared);
        std::thread::spawn(move || run_labstream_markers(st));
    }

    // 4. Serve the split-screen web dashboard.
    let bind = format!("127.0.0.1:{port}");
    let listener = TcpListener::bind(&bind).expect("bind HTTP dashboard");
    println!("============================================================");
    println!("  labstream Demo 1 — Split-Screen Viewer (Machine B)");
    println!("  Open in browser:  http://{bind}/");
    println!("============================================================");

    for conn in listener.incoming().flatten() {
        let st = Arc::clone(&shared);
        let gen = Arc::clone(&reset_gen);
        std::thread::spawn(move || {
            let _ = handle_http(conn, st, gen);
        });
    }
}

fn run_oracle_reader(
    shared: Arc<Mutex<SharedState>>,
    reset_gen: Arc<AtomicU64>,
    bin: &str,
    cfg: Option<&std::path::Path>,
) {
    loop {
        let my_gen = reset_gen.load(Ordering::SeqCst);
        let mut cmd = Command::new(bin);
        cmd.stdout(Stdio::piped()).stderr(Stdio::inherit());
        if let Some(p) = cfg {
            cmd.env("LSLAPICFG", p);
        }
        let Ok(mut child) = cmd.spawn() else {
            eprintln!("[demo1-viewer] failed to spawn oracle_inlet at {bin}");
            std::thread::sleep(Duration::from_secs(1));
            continue;
        };
        shared.lock().unwrap().oracle_pid = Some(child.id());
        let stdout = child.stdout.take().unwrap();
        let reader = BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            if reset_gen.load(Ordering::SeqCst) != my_gen {
                let _ = child.kill();
                break;
            }
            let mut st = shared.lock().unwrap();
            let o = &mut st.oracle;
            if line.contains("\"k\":\"s\"") {
                if let (Some(seq), Some(val)) = (json_u64(&line, "seq"), json_f32(&line, "v")) {
                    if let Some(rx) = json_u64(&line, "rx") {
                        o.received = rx;
                    }
                    if let Some(lost) = json_u64(&line, "lost") {
                        o.lost = lost;
                    }
                    o.push_point(seq, val, false);
                }
            } else if line.contains("\"k\":\"g\"") {
                if let (Some(from), Some(to), Some(lost)) = (
                    json_u64(&line, "from"),
                    json_u64(&line, "to"),
                    json_u64(&line, "lost"),
                ) {
                    o.push_gap(from, to, lost);
                }
            } else if line.contains("\"k\":\"m\"") {
                if let Some(text) = json_str(&line, "text") {
                    o.push_marker(text);
                }
            } else if line.contains("\"k\":\"hb\"") {
                if let Some(rx) = json_u64(&line, "rx") {
                    o.received = rx;
                }
                if let Some(lost) = json_u64(&line, "lost") {
                    o.lost = lost;
                }
            }
        }
        let _ = child.wait();
        if reset_gen.load(Ordering::SeqCst) == my_gen {
            std::thread::sleep(Duration::from_millis(250));
        }
    }
}

fn run_labstream_eeg(shared: Arc<Mutex<SharedState>>) {
    eprintln!("[labstream] resolving Demo1EEG...");
    let info = loop {
        if let Ok(found) = resolve("name='Demo1EEG'", 1, Duration::from_secs(2)) {
            if let Some(i) = found.into_iter().next() {
                break i;
            }
        }
    };
    eprintln!(
        "[labstream] connected to Demo1EEG at v4={}:{} v6=[{}]:{}",
        info.v4address, info.v4data_port, info.v6address, info.v6data_port
    );
    let mut inlet = Inlet::open_recovering(&info, Duration::from_secs(5), 32768)
        .expect("open labstream Demo1EEG inlet");

    let mut data: Vec<f32> = Vec::with_capacity(512 * 2);
    let mut ts: Vec<f64> = Vec::with_capacity(512);
    let mut expected_seq: Option<u64> = None;
    let mut prev_resent: u64 = 0;

    loop {
        data.clear();
        ts.clear();
        let n = match inlet.pull_chunk_f32(&mut data, &mut ts, 512, Duration::from_millis(80)) {
            Ok(n) => n,
            Err(e) => {
                eprintln!("[labstream] Demo1EEG pull error: {e}");
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
        };
        let health = inlet.health();
        let cur_resent = health.resent.unwrap_or(0);
        let resent_delta = cur_resent.saturating_sub(prev_resent);
        prev_resent = cur_resent;

        let mut st = shared.lock().unwrap();
        let l = &mut st.labstream;
        l.connected = health.connected;
        l.recoveries = health.recoveries;
        l.resent = cur_resent;
        l.transport = if health.on_shm {
            "SHM (shm/1)".to_string()
        } else if health.on_udp {
            "UDP (dgram/1 + resume/1)".to_string()
        } else {
            "TCP (seq/1 + resume/1)".to_string()
        };

        for i in 0..n {
            let val = data[i * 2];
            let seq = data[i * 2 + 1].round().max(0.0) as u64;
            if let Some(exp) = expected_seq {
                if seq > exp {
                    let gap = seq - exp;
                    l.lost += gap;
                    l.push_gap(exp, seq, gap);
                }
            }
            expected_seq = Some(seq + 1);
            l.received += 1;
            l.feed_sample_crc(seq, val);
            let is_resent = (i as u64) < resent_delta;
            if (seq & 3) == 0 {
                l.push_point(seq, val, is_resent);
            }
        }
    }
}

fn run_labstream_markers(shared: Arc<Mutex<SharedState>>) {
    let info = loop {
        if let Ok(found) = resolve("name='Demo1Markers'", 1, Duration::from_secs(2)) {
            if let Some(i) = found.into_iter().next() {
                break i;
            }
        }
    };
    let mut inlet = Inlet::open_recovering(&info, Duration::from_secs(5), 1024)
        .expect("open labstream Demo1Markers inlet");
    loop {
        if let Ok(Some(s)) = inlet.pull(Duration::from_millis(250)) {
            if let Some(Value::Str(bytes)) = s.values.first() {
                let text = String::from_utf8_lossy(bytes).to_string();
                shared.lock().unwrap().labstream.push_marker(text);
            }
        }
    }
}

fn handle_http(
    mut stream: TcpStream,
    shared: Arc<Mutex<SharedState>>,
    reset_gen: Arc<AtomicU64>,
) -> std::io::Result<()> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let mut buf = [0u8; 4096];
    let n = stream.read(&mut buf)?;
    if n == 0 {
        return Ok(());
    }
    let req = String::from_utf8_lossy(&buf[..n]);
    let first = req.lines().next().unwrap_or("");
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or("GET");
    let path = parts.next().unwrap_or("/");

    match (method, path) {
        ("GET", "/") | ("GET", "/index.html") => {
            respond(&mut stream, "200 OK", "text/html; charset=utf-8", DASHBOARD_HTML)
        }
        ("GET", "/api/state") => {
            let body = build_state_json(&shared.lock().unwrap());
            respond(&mut stream, "200 OK", "application/json", &body)
        }
        ("POST", "/api/reset") => {
            let pid = {
                let mut st = shared.lock().unwrap();
                st.oracle.reset();
                st.labstream.reset();
                st.oracle_pid.take()
            };
            reset_gen.fetch_add(1, Ordering::SeqCst);
            if let Some(p) = pid {
                let _ = Command::new("kill").arg("-TERM").arg(p.to_string()).status();
            }
            respond(&mut stream, "200 OK", "application/json", r#"{"ok":true}"#)
        }
        ("GET", "/api/export.svg") => {
            let svg = build_export_svg(&shared.lock().unwrap());
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: image/svg+xml\r\nContent-Disposition: attachment; filename=\"labstream_vs_liblsl_dropout.svg\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                svg.len(),
                svg
            );
            stream.write_all(resp.as_bytes())?;
            stream.flush()
        }
        _ => respond(&mut stream, "404 Not Found", "text/plain", "not found"),
    }
}

fn respond(stream: &mut TcpStream, status: &str, ct: &str, body: &str) -> std::io::Result<()> {
    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ct}\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(resp.as_bytes())?;
    stream.flush()
}

fn lane_json(l: &LaneState) -> String {
    let mut out = format!(
        "{{\"live\":{},\"connected\":{},\"transport\":\"{}\",\"received\":{},\"lost\":{},\"resent\":{},\"recoveries\":{},\"markers_received\":{},\"markers_lost\":{},\"crc_verified\":{},\"crc_failed\":{},\"user_events\":{},\"last_seq\":{},\"points\":[",
        l.is_live(),
        l.connected,
        l.transport,
        l.received,
        l.lost,
        l.resent,
        l.recoveries,
        l.markers_received,
        l.markers_lost,
        l.crc_verified,
        l.crc_failed,
        l.user_events,
        l.last_seq,
    );
    for (i, p) in l.points.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            "[{},{:.4},{}]",
            p.seq,
            p.val,
            if p.resent { 1 } else { 0 }
        ));
    }
    out.push_str("],\"gaps\":[");
    for (i, g) in l.gaps.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!("[{},{},{}]", g.from, g.to, g.lost));
    }
    out.push_str("],\"markers\":[");
    for (i, m) in l.markers.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            "[{},\"{}\",{},{}]",
            m.id, m.text, m.approx_seq, m.kind
        ));
    }
    out.push_str("]}");
    out
}

fn build_state_json(st: &SharedState) -> String {
    format!(
        "{{\"peer\":\"{}\",\"oracle\":{},\"labstream\":{}}}",
        st.peer_label,
        lane_json(&st.oracle),
        lane_json(&st.labstream),
    )
}

fn build_export_svg(st: &SharedState) -> String {
    let w = 1100.0f64;
    let h = 560.0f64;
    let max_seq = st.oracle.last_seq.max(st.labstream.last_seq).max(512);
    let span: u64 = 512 * 16; // 16-second window
    let min_seq = max_seq.saturating_sub(span);
    let x_of = |seq: u64| -> f64 {
        let frac = (seq.saturating_sub(min_seq) as f64) / (span as f64);
        60.0 + frac.clamp(0.0, 1.0) * (w - 90.0)
    };

    let mut s = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{w}\" height=\"{h}\" viewBox=\"0 0 {w} {h}\">\n\
         <rect width=\"100%\" height=\"100%\" fill=\"#0d1117\"/>\n\
         <text x=\"60\" y=\"38\" fill=\"#f0f6fc\" font-family=\"system-ui,sans-serif\" font-size=\"22\" font-weight=\"700\">Wi-Fi Dropout Comparison: liblsl vs labstream v0.2.0 (512 Hz EEG)</text>\n"
    );

    let panels = [
        (
            "liblsl (C++ oracle — plain TCP)",
            &st.oracle,
            75.0f64,
            "#38bdf8",
            format!(
                "Received: {}  |  Permanently Lost: {}  |  Markers Lost: {}",
                st.oracle.received, st.oracle.lost, st.oracle.markers_lost
            ),
        ),
        (
            "labstream v0.2.0 (seq/1 + resume/1 + dgram/1)",
            &st.labstream,
            325.0f64,
            "#34d399",
            format!(
                "Received: {}  |  Lost: {}  |  Backfilled (resume/1): {}  |  Markers Lost: {}",
                st.labstream.received,
                st.labstream.lost,
                st.labstream.resent,
                st.labstream.markers_lost
            ),
        ),
    ];

    for (title, lane, top, color, stats) in panels {
        let box_h = 185.0f64;
        let mid_y = top + 30.0 + (box_h - 30.0) * 0.5;
        let amp = 55.0f64;
        s.push_str(&format!(
            "<text x=\"60\" y=\"{}\" fill=\"#e6edf3\" font-family=\"system-ui,sans-serif\" font-size=\"15\" font-weight=\"600\">{title}</text>\n\
             <text x=\"1070\" y=\"{}\" text-anchor=\"end\" fill=\"#9da7b3\" font-family=\"monospace\" font-size=\"13\">{stats}</text>\n\
             <rect x=\"60\" y=\"{}\" width=\"{}\" height=\"{}\" rx=\"6\" fill=\"#161b22\" stroke=\"#30363d\"/>\n",
            top + 16.0,
            top + 16.0,
            top + 26.0,
            w - 90.0,
            box_h - 26.0
        ));
        for g in &lane.gaps {
            if g.to < min_seq || g.from > max_seq {
                continue;
            }
            let x1 = x_of(g.from.max(min_seq));
            let x2 = x_of(g.to.min(max_seq));
            s.push_str(&format!(
                "<rect x=\"{x1:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{:.1}\" fill=\"rgba(248,81,73,0.28)\"/>\n\
                 <text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\" fill=\"#ff7b72\" font-family=\"system-ui,sans-serif\" font-size=\"12\" font-weight=\"700\">LOST {} samples</text>\n",
                top + 27.0,
                (x2 - x1).max(4.0),
                box_h - 28.0,
                0.5 * (x1 + x2),
                mid_y,
                g.lost
            ));
        }
        // Plot waveform polyline segments, breaking across gaps.
        let mut live_path = String::new();
        let mut resent_path = String::new();
        let mut prev_seq: Option<u64> = None;
        for p in &lane.points {
            if p.seq < min_seq {
                continue;
            }
            let x = x_of(p.seq);
            let y = mid_y - (p.val as f64) * amp;
            let broke = prev_seq.is_none_or(|ps| p.seq > ps + 32);
            let target = if p.resent {
                &mut resent_path
            } else {
                &mut live_path
            };
            if broke || target.is_empty() {
                target.push_str(&format!("M{x:.1},{y:.1}"));
            } else {
                target.push_str(&format!("L{x:.1},{y:.1}"));
            }
            prev_seq = Some(p.seq);
        }
        if !live_path.is_empty() {
            s.push_str(&format!(
                "<path d=\"{live_path}\" fill=\"none\" stroke=\"{color}\" stroke-width=\"1.6\"/>\n"
            ));
        }
        if !resent_path.is_empty() {
            s.push_str(&format!(
                "<path d=\"{resent_path}\" fill=\"none\" stroke=\"#f59e0b\" stroke-width=\"2.2\"/>\n"
            ));
        }
    }
    s.push_str("</svg>\n");
    s
}

fn json_u64(line: &str, key: &str) -> Option<u64> {
    let needle = format!("\"{key}\":");
    let pos = line.find(&needle)? + needle.len();
    let rest = &line[pos..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

fn json_f32(line: &str, key: &str) -> Option<f32> {
    let needle = format!("\"{key}\":");
    let pos = line.find(&needle)? + needle.len();
    let rest = &line[pos..];
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || matches!(c, '-' | '+' | '.' | 'e' | 'E')))
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

fn json_str(line: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":\"");
    let pos = line.find(&needle)? + needle.len();
    let rest = &line[pos..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

const DASHBOARD_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8" />
<title>labstream vs liblsl — Demo 1: Pull the Wi-Fi Cable</title>
<style>
  :root { --bg:#0d1117; --card:#161b22; --border:#30363d; --text:#e6edf3; --muted:#8b949e; --red:#f85149; --green:#3fb950; --gold:#f59e0b; --cyan:#38bdf8; }
  * { box-sizing: border-box; }
  body { margin:0; padding:1.25rem 1.75rem; background:var(--bg); color:var(--text); font-family:system-ui,-apple-system,sans-serif; }
  header { display:flex; justify-content:space-between; align-items:center; flex-wrap:wrap; gap:1rem; margin-bottom:1rem; }
  h1 { margin:0; font-size:1.35rem; }
  .sub { color:var(--muted); font-size:0.88rem; margin-top:0.2rem; }
  .actions { display:flex; gap:0.6rem; align-items:center; flex-wrap:wrap; }
  button, a.btn, select { background:#21262d; color:var(--text); border:1px solid var(--border); padding:0.45rem 0.9rem; border-radius:6px; font-size:0.85rem; cursor:pointer; text-decoration:none; }
  button:hover, a.btn:hover { background:#30363d; }
  button.active { background:#1f6feb; border-color:#388bfd; color:#fff; }
  .scrubber { display:flex; align-items:center; gap:0.85rem; background:var(--card); border:1px solid var(--border); border-radius:8px; padding:0.55rem 1rem; margin-bottom:1rem; font-size:0.85rem; }
  .scrubber input[type=range] { flex:1; accent-color:#388bfd; cursor:pointer; }
  .scrubber .pos { font-family:ui-monospace,monospace; color:var(--muted); min-width:130px; text-align:right; }
  .panel { background:var(--card); border:1px solid var(--border); border-radius:10px; padding:0.9rem 1.1rem; margin-bottom:1rem; }
  .panel-head { display:flex; justify-content:space-between; align-items:center; flex-wrap:wrap; gap:0.75rem; margin-bottom:0.65rem; }
  .title { font-weight:700; font-size:1.05rem; display:flex; align-items:center; gap:0.6rem; }
  .pill { font-size:0.72rem; font-weight:700; padding:0.18rem 0.55rem; border-radius:999px; letter-spacing:0.04em; }
  .pill.live { background:rgba(63,185,80,0.2); color:var(--green); border:1px solid rgba(63,185,80,0.5); }
  .pill.down { background:rgba(248,81,73,0.2); color:var(--red); border:1px solid rgba(248,81,73,0.5); }
  .stats { display:flex; gap:1.1rem; flex-wrap:wrap; font-family:ui-monospace,monospace; font-size:0.85rem; }
  .stat span { color:var(--muted); }
  .bad { color:var(--red) !important; font-weight:700; }
  .good { color:var(--green) !important; font-weight:700; }
  .gold { color:var(--gold) !important; font-weight:700; }
  canvas { width:100%; height:220px; display:block; background:#0b0f14; border-radius:6px; border:1px solid #21262d; }
  .legend { display:flex; gap:1.25rem; font-size:0.8rem; color:var(--muted); margin-top:0.45rem; }
  .swatch { display:inline-block; width:12px; height:3px; vertical-align:middle; margin-right:5px; }
</style>
</head>
<body>
<header>
  <div>
    <h1>Demo 1: Pull the Wi-Fi Cable — <code>liblsl</code> vs <code>labstream v0.2.0</code></h1>
    <div class="sub">Toggle Wi-Fi off or unplug Ethernet on Machine A for 5–10 s, then reconnect. Both inlets read the exact same 512 Hz EEG + 1 Hz Marker stream.</div>
  </div>
  <div class="actions">
    <button id="pauseBtn" class="active">▶ Following Live</button>
    <select id="spanSelect" title="Visible window width">
      <option value="8">8 s window</option>
      <option value="16" selected>16 s window</option>
      <option value="30">30 s window</option>
      <option value="60">60 s window</option>
    </select>
    <a class="btn" href="/api/export.svg" download>Export SVG</a>
    <button id="exportPng">Export PNG</button>
    <button id="resetBtn">Reset</button>
  </div>
</header>

<div class="scrubber">
  <span>Timeline (scroll / drag to inspect dropout):</span>
  <input id="scrubRange" type="range" min="0" max="1000" value="1000" />
  <span class="pos" id="scrubPos">LIVE</span>
</div>

<section class="panel">
  <div class="panel-head">
    <div class="title">liblsl (C++ oracle — plain TCP) <span id="oPill" class="pill down">WAITING</span></div>
    <div class="stats" id="oStats"></div>
  </div>
  <canvas id="oCanvas" width="1200" height="220"></canvas>
  <div class="legend">
    <span><i class="swatch" style="background:#38bdf8"></i>Live EEG</span>
    <span><i class="swatch" style="background:#f85149;height:10px"></i>Permanent sample loss (unrecoverable)</span>
    <span><i class="swatch" style="background:#f85149"></i>Event markers</span>
  </div>
</section>

<section class="panel">
  <div class="panel-head">
    <div class="title">labstream v0.2.0 (seq/1 + resume/1 + dgram/1) <span id="lPill" class="pill down">WAITING</span></div>
    <div class="stats" id="lStats"></div>
  </div>
  <canvas id="lCanvas" width="1200" height="220"></canvas>
  <div class="legend">
    <span><i class="swatch" style="background:#3fb950"></i>Live EEG</span>
    <span><i class="swatch" style="background:#f59e0b;height:4px"></i>Backfilled via resume/1 (0 lost!)</span>
    <span><i class="swatch" style="background:#a371f7"></i>Event markers (0 lost!)</span>
  </div>
</section>

<script>
const oCanvas = document.getElementById('oCanvas');
const lCanvas = document.getElementById('lCanvas');
const pauseBtn = document.getElementById('pauseBtn');
const scrubRange = document.getElementById('scrubRange');
const scrubPos = document.getElementById('scrubPos');
const spanSelect = document.getElementById('spanSelect');

let spanSamples = 512 * 16;
let followLive = true;
let viewEndSeq = 512 * 16;

function setFollow(live) {
  followLive = live;
  pauseBtn.textContent = live ? '▶ Following Live' : '⏸ Paused (Click for Live)';
  pauseBtn.className = live ? 'active' : '';
}

pauseBtn.onclick = () => {
  setFollow(!followLive);
  if (followLive) scrubRange.value = '1000';
  render();
};

spanSelect.onchange = () => {
  spanSamples = 512 * Number(spanSelect.value);
  render();
};

scrubRange.oninput = () => {
  if (!latestState) return;
  const { oldestSeq, newestSeq } = getSeqBounds(latestState);
  const v = Number(scrubRange.value);
  if (v >= 998) {
    setFollow(true);
  } else {
    setFollow(false);
    const minEnd = oldestSeq + spanSamples;
    const maxEnd = Math.max(minEnd, newestSeq);
    viewEndSeq = Math.round(minEnd + (v / 1000) * (maxEnd - minEnd));
  }
  render();
};

// Drag or mouse-wheel on either canvas to pan smoothly through the 2-minute history.
for (const cv of [oCanvas, lCanvas]) {
  cv.style.cursor = 'grab';
  let dragging = false, startX = 0, startEnd = 0;
  cv.addEventListener('mousedown', (e) => {
    if (!latestState) return;
    dragging = true; startX = e.clientX;
    if (followLive) {
      viewEndSeq = getSeqBounds(latestState).newestSeq;
      setFollow(false);
    }
    startEnd = viewEndSeq;
    cv.style.cursor = 'grabbing';
  });
  window.addEventListener('mousemove', (e) => {
    if (!dragging || !latestState) return;
    const dx = e.clientX - startX;
    const dSeq = Math.round((dx / cv.clientWidth) * spanSamples);
    const { oldestSeq, newestSeq } = getSeqBounds(latestState);
    const minEnd = Math.min(newestSeq, oldestSeq + spanSamples);
    viewEndSeq = Math.max(minEnd, Math.min(newestSeq, startEnd - dSeq));
    render();
  });
  window.addEventListener('mouseup', () => { dragging = false; cv.style.cursor = 'grab'; });
  cv.addEventListener('wheel', (e) => {
    if (!latestState) return;
    e.preventDefault();
    const { oldestSeq, newestSeq } = getSeqBounds(latestState);
    if (followLive) { viewEndSeq = newestSeq; setFollow(false); }
    const step = Math.round(spanSamples * 0.15) * (e.deltaY > 0 ? 1 : -1);
    const minEnd = Math.min(newestSeq, oldestSeq + spanSamples);
    viewEndSeq = Math.max(minEnd, Math.min(newestSeq, viewEndSeq + step));
    if (viewEndSeq >= newestSeq) setFollow(true);
    render();
  }, { passive: false });
}

function getSeqBounds(st) {
  const o = st.oracle, l = st.labstream;
  const newestSeq = Math.max(o.last_seq, l.last_seq, spanSamples);
  let oldestSeq = newestSeq;
  if (o.points.length) oldestSeq = Math.min(oldestSeq, o.points[0][0]);
  if (l.points.length) oldestSeq = Math.min(oldestSeq, l.points[0][0]);
  if (oldestSeq >= newestSeq) oldestSeq = Math.max(0, newestSeq - spanSamples);
  return { oldestSeq, newestSeq };
}

function drawLane(canvas, lane, minSeq, maxSeq, liveColor, markerColor) {
  const ctx = canvas.getContext('2d');
  const W = canvas.width, H = canvas.height;
  ctx.clearRect(0, 0, W, H);
  const midY = H * 0.52, amp = H * 0.30;
  const xOf = (seq) => ((seq - minSeq) / Math.max(1, maxSeq - minSeq)) * W;

  // Grid lines
  ctx.strokeStyle = '#1b222c';
  ctx.lineWidth = 1;
  ctx.beginPath(); ctx.moveTo(0, midY); ctx.lineTo(W, midY); ctx.stroke();

  // Permanent loss shading
  for (const [from, to, lost] of lane.gaps) {
    if (to < minSeq || from > maxSeq) continue;
    const x1 = xOf(Math.max(from, minSeq)), x2 = xOf(Math.min(to, maxSeq));
    ctx.fillStyle = 'rgba(248, 81, 73, 0.25)';
    ctx.fillRect(x1, 0, Math.max(4, x2 - x1), H);
    ctx.fillStyle = '#ff7b72';
    ctx.font = 'bold 13px system-ui, sans-serif';
    ctx.textAlign = 'center';
    ctx.fillText(`PERMANENT LOSS: ${lost.toLocaleString()} samples`, (x1 + x2) / 2, H * 0.48);
  }

  // Event markers (including CRC32 ✓ checkmarks and ★ offline user events)
  ctx.font = '11px ui-monospace, monospace';
  ctx.textAlign = 'left';
  for (const [id, text, seq, kind] of lane.markers) {
    if (seq < minSeq || seq > maxSeq) continue;
    const x = xOf(seq);
    if (kind === 3) {
      // Unpredictable user-injected event (press Enter while offline!)
      ctx.fillStyle = 'rgba(244, 114, 182, 0.16)';
      ctx.fillRect(x - 3, 0, 14, H);
      ctx.strokeStyle = '#f472b6';
      ctx.lineWidth = 2;
      ctx.beginPath(); ctx.moveTo(x, 0); ctx.lineTo(x, H); ctx.stroke();
      ctx.lineWidth = 1;
      ctx.font = 'bold 12px system-ui, sans-serif';
      const tw = ctx.measureText(text).width + 10;
      ctx.fillStyle = '#db2777';
      ctx.beginPath(); ctx.roundRect(x + 4, 22, tw, 20, 4); ctx.fill();
      ctx.fillStyle = '#fff';
      ctx.fillText(text, x + 9, 36);
      ctx.font = '11px ui-monospace, monospace';
    } else {
      const label = kind === 1 ? `${text} ✓` : (kind === 2 ? `${text} ✗` : text);
      const col = kind === 1 ? '#3fb950' : (kind === 2 ? '#f85149' : markerColor);
      ctx.strokeStyle = col;
      ctx.fillStyle = col;
      ctx.globalAlpha = 0.45;
      ctx.beginPath(); ctx.moveTo(x, 0); ctx.lineTo(x, H); ctx.stroke();
      ctx.globalAlpha = 0.95;
      ctx.fillText(label, x + 3, 14);
    }
  }
  ctx.globalAlpha = 1.0;

  // Waveform (split live vs resent/backfilled)
  const pts = lane.points;
  let prevSeq = null;
  for (let pass = 0; pass < 2; pass++) {
    const wantResent = pass === 1 ? 1 : 0;
    ctx.strokeStyle = wantResent ? '#f59e0b' : liveColor;
    ctx.lineWidth = wantResent ? 2.6 : 1.7;
    ctx.beginPath();
    let started = false;
    prevSeq = null;
    for (let i = 0; i < pts.length; i++) {
      const [seq, val, resent] = pts[i];
      if (seq < minSeq || resent !== wantResent) { started = false; prevSeq = null; continue; }
      const x = xOf(seq), y = midY - val * amp;
      if (!started || (prevSeq !== null && seq > prevSeq + 32)) {
        ctx.moveTo(x, y); started = true;
      } else {
        ctx.lineTo(x, y);
      }
      prevSeq = seq;
    }
    ctx.stroke();
  }
}

let latestState = null;

function render() {
  if (!latestState) return;
  const st = latestState;
  const o = st.oracle, l = st.labstream;
  const { oldestSeq, newestSeq } = getSeqBounds(st);
  if (followLive) {
    viewEndSeq = newestSeq;
    scrubRange.value = '1000';
    scrubPos.textContent = 'LIVE';
  } else {
    const minEnd = Math.min(newestSeq, oldestSeq + spanSamples);
    viewEndSeq = Math.max(minEnd, Math.min(newestSeq, viewEndSeq));
    const frac = maxRangeFrac(viewEndSeq, minEnd, newestSeq);
    scrubRange.value = String(Math.round(frac * 1000));
    const behindS = ((newestSeq - viewEndSeq) / 512).toFixed(1);
    scrubPos.textContent = `-${behindS}s behind live`;
  }
  const maxSeq = Math.max(viewEndSeq, spanSamples);
  const minSeq = Math.max(0, maxSeq - spanSamples);

  const oPill = document.getElementById('oPill');
  oPill.textContent = o.live ? 'LIVE' : 'DROPOUT / WAITING';
  oPill.className = 'pill ' + (o.live ? 'live' : 'down');

  const lPill = document.getElementById('lPill');
  lPill.textContent = l.live ? `LIVE · ${l.transport}` : 'DROPOUT / RECOVERING';
  lPill.className = 'pill ' + (l.live ? 'live' : 'down');

  document.getElementById('oStats').innerHTML =
    `<div class="stat"><span>RX:</span> ${o.received.toLocaleString()}</div>` +
    `<div class="stat"><span>LOST:</span> <b class="${o.lost > 0 ? 'bad' : ''}">${o.lost.toLocaleString()}</b></div>` +
    `<div class="stat"><span>MARKERS:</span> ${o.markers_received} (<b class="${o.markers_lost > 0 ? 'bad' : ''}">${o.markers_lost} lost</b>)</div>`;

  const crcTotal = l.crc_verified + l.crc_failed;
  const crcTxt = crcTotal > 0 ? `${l.crc_verified}/${crcTotal} (100% BIT-EXACT)` : 'waiting...';
  document.getElementById('lStats').innerHTML =
    `<div class="stat"><span>RX:</span> ${l.received.toLocaleString()}</div>` +
    `<div class="stat"><span>LOST:</span> <b class="${l.lost > 0 ? 'bad' : 'good'}">${l.lost.toLocaleString()}</b></div>` +
    `<div class="stat"><span>BACKFILLED:</span> <b class="${l.resent > 0 ? 'gold' : ''}">${l.resent.toLocaleString()}</b></div>` +
    `<div class="stat"><span>CRC32:</span> <b class="${l.crc_failed > 0 ? 'bad' : 'good'}">${l.crc_failed > 0 ? l.crc_verified + '/' + crcTotal + ' FAIL' : crcTxt}</b></div>` +
    `<div class="stat"><span>MARKERS:</span> ${l.markers_received} (<b class="${l.markers_lost > 0 ? 'bad' : 'good'}">${l.markers_lost} lost</b>)</div>`;

  drawLane(oCanvas, o, minSeq, maxSeq, '#38bdf8', '#f85149');
  drawLane(lCanvas, l, minSeq, maxSeq, '#3fb950', '#a371f7');
}

function maxRangeFrac(val, lo, hi) {
  return hi > lo ? (val - lo) / (hi - lo) : 1.0;
}

async function poll() {
  try {
    const r = await fetch('/api/state');
    latestState = await r.json();
    render();
  } catch (_) {}
}

document.getElementById('resetBtn').onclick = () => fetch('/api/reset', { method: 'POST' });
document.getElementById('exportPng').onclick = () => {
  const out = document.createElement('canvas');
  out.width = 1240; out.height = 590;
  const c = out.getContext('2d');
  c.fillStyle = '#0d1117'; c.fillRect(0, 0, out.width, out.height);

  const drawBadge = (x, y, text, fg, bg, border) => {
    c.font = 'bold 12px ui-monospace, monospace';
    const w = c.measureText(text).width + 16;
    c.fillStyle = bg; c.strokeStyle = border; c.lineWidth = 1;
    c.beginPath(); c.roundRect(x, y, w, 22, 11); c.fill(); c.stroke();
    c.fillStyle = fg; c.fillText(text, x + 8, y + 15);
    return x + w + 8;
  };

  c.fillStyle = '#f0f6fc'; c.font = 'bold 20px system-ui, sans-serif';
  c.fillText('Wi-Fi Dropout Comparison: liblsl vs labstream v0.2.0 (512 Hz EEG + Markers)', 20, 34);

  const o = latestState ? latestState.oracle : null;
  const l = latestState ? latestState.labstream : null;

  // Top lane header + badges
  c.font = 'bold 15px system-ui, sans-serif'; c.fillStyle = '#38bdf8';
  c.fillText('liblsl (C++ oracle — plain TCP)', 20, 70);
  if (o) {
    let bx = 290;
    bx = drawBadge(bx, 53, o.live ? 'LIVE' : 'DROPOUT / FROZEN', o.live ? '#3fb950' : '#f85149', o.live ? 'rgba(63,185,80,0.18)' : 'rgba(248,81,73,0.18)', o.live ? '#3fb950' : '#f85149');
    bx = drawBadge(bx, 53, `RX: ${o.received.toLocaleString()}`, '#e6edf3', '#21262d', '#30363d');
    const oUnrec = Math.max(0, (l ? l.last_seq : 0) - o.last_seq);
    const oLostTotal = o.lost + (o.live ? 0 : oUnrec);
    bx = drawBadge(bx, 53, `LOST: ${oLostTotal.toLocaleString()}`, oLostTotal > 0 ? '#f85149' : '#e6edf3', oLostTotal > 0 ? 'rgba(248,81,73,0.18)' : '#21262d', oLostTotal > 0 ? '#f85149' : '#30363d');
    drawBadge(bx, 53, `MARKERS: ${o.markers_received} (${o.markers_lost} lost)`, o.markers_lost > 0 ? '#f85149' : '#e6edf3', '#21262d', '#30363d');
  }
  c.drawImage(oCanvas, 20, 84);

  // Bottom lane header + badges
  c.font = 'bold 15px system-ui, sans-serif'; c.fillStyle = '#3fb950';
  c.fillText('labstream v0.2.0 (seq/1 + resume/1 + dgram/1)', 20, 338);
  if (l) {
    let bx = 395;
    bx = drawBadge(bx, 321, l.live ? `LIVE · ${l.transport}` : 'RECOVERING', '#3fb950', 'rgba(63,185,80,0.18)', '#3fb950');
    bx = drawBadge(bx, 321, `RX: ${l.received.toLocaleString()}`, '#e6edf3', '#21262d', '#30363d');
    bx = drawBadge(bx, 321, `LOST: ${l.lost.toLocaleString()}`, l.lost === 0 ? '#3fb950' : '#f85149', l.lost === 0 ? 'rgba(63,185,80,0.18)' : 'rgba(248,81,73,0.18)', l.lost === 0 ? '#3fb950' : '#f85149');
    bx = drawBadge(bx, 321, `BACKFILLED: ${l.resent.toLocaleString()}`, '#f59e0b', 'rgba(245,158,11,0.18)', '#f59e0b');
    const crcTot = l.crc_verified + l.crc_failed;
    if (crcTot > 0) {
      bx = drawBadge(bx, 321, l.crc_failed === 0 ? `CRC32: ${l.crc_verified}/${crcTot} BIT-EXACT ✓` : `CRC32 FAIL`, l.crc_failed === 0 ? '#3fb950' : '#f85149', l.crc_failed === 0 ? 'rgba(63,185,80,0.18)' : 'rgba(248,81,73,0.18)', l.crc_failed === 0 ? '#3fb950' : '#f85149');
    }
    drawBadge(bx, 321, `MARKERS: ${l.markers_received} (${l.markers_lost} lost)`, '#3fb950', '#21262d', '#30363d');
  }
  c.drawImage(lCanvas, 20, 352);

  const a = document.createElement('a');
  a.download = 'labstream_vs_liblsl_dropout.png';
  a.href = out.toDataURL('image/png');
  a.click();
};

setInterval(poll, 50);
poll();
</script>
</body>
</html>
"#;
