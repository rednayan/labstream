//! Demo 6: Hospital Wi-Fi Security (`enc/1` negotiated transport encryption vs
//! standard plaintext LSL TCP) with a live transparent TCP wire sniffer.

use labstream_net::enc::{set_runtime_policy, EncryptionPolicy};
use labstream_net::{Inlet, Outlet, StreamInfo};
use labstream_wire::{Format, PackedSample, Value};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const WIRE_BUF_CAP: usize = 512;
const CLINICAL_TEMPLATES: [&str; 6] = [
    "PATIENT=JANE_DOE|MRN=849201|WARD=ICU-4B|EVENT=SEIZURE_ONSET",
    "PATIENT=JANE_DOE|MRN=849201|VITALS=HR:118_BP:148/92|SpO2=94%",
    "PATIENT=JANE_DOE|MRN=849201|MED=LORAZEPAM_2MG_IV|STAT=GIVEN",
    "PATIENT=JANE_DOE|MRN=849201|EEG_ALERT=LEFT_TEMPORAL_SPIKE",
    "PATIENT=JANE_DOE|MRN=849201|CLINICIAN=DR_PATEL|NOTE=STABILIZING",
    "PATIENT=JANE_DOE|MRN=849201|VITALS=HR:84_BP:124/78|SpO2=99%",
];

struct SideState {
    encrypted: bool,
    total_wire_bytes: u64,
    wire_tail: VecDeque<u8>,
    leaked_hits: u64,
    decoded_markers: VecDeque<(f64, String)>,
    decoded_count: u64,
}

impl SideState {
    fn new(encrypted: bool) -> Self {
        Self {
            encrypted,
            total_wire_bytes: 0,
            wire_tail: VecDeque::with_capacity(WIRE_BUF_CAP + 64),
            leaked_hits: 0,
            decoded_markers: VecDeque::with_capacity(16),
            decoded_count: 0,
        }
    }

    fn push_wire(&mut self, bytes: &[u8]) {
        self.total_wire_bytes += bytes.len() as u64;
        for &b in bytes {
            self.wire_tail.push_back(b);
            while self.wire_tail.len() > WIRE_BUF_CAP {
                self.wire_tail.pop_front();
            }
        }
        // Check if sensitive clinical tokens appear in plaintext on the wire.
        if bytes.windows(7).any(|w| w == b"PATIENT" || w == b"MRN=849") {
            self.leaked_hits += 1;
        }
    }

    fn entropy(&self) -> f64 {
        if self.wire_tail.is_empty() {
            return 0.0;
        }
        let mut counts = [0u32; 256];
        for &b in &self.wire_tail {
            counts[b as usize] += 1;
        }
        let n = self.wire_tail.len() as f64;
        let mut h = 0.0;
        for &c in &counts {
            if c > 0 {
                let p = c as f64 / n;
                h -= p * p.log2();
            }
        }
        h
    }
}

struct SharedState {
    plain: SideState,
    enc: SideState,
    enc_outlet_port: u16,
    enc_outlet_uid: String,
    last_probe_result: String,
    custom_queue: VecDeque<String>,
}

/// Transparent loopback TCP wire tap that forwards bytes untouched while
/// recording the exact server-to-client TCP payload for the live sniffer.
fn start_wire_tap(target_port: u16, state: Arc<Mutex<SharedState>>, is_enc: bool) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind wire tap");
    let proxy_port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        while let Ok((mut client, _)) = listener.accept() {
            let st = Arc::clone(&state);
            thread::spawn(move || {
                let Ok(mut server) = TcpStream::connect(("127.0.0.1", target_port)) else {
                    return;
                };
                let _ = client.set_nodelay(true);
                let _ = server.set_nodelay(true);
                let mut c_read = client.try_clone().unwrap();
                let mut s_write = server.try_clone().unwrap();
                let up = thread::spawn(move || {
                    let mut buf = [0u8; 4096];
                    while let Ok(n) = c_read.read(&mut buf) {
                        if n == 0 || s_write.write_all(&buf[..n]).is_err() {
                            break;
                        }
                    }
                });
                let mut buf = [0u8; 4096];
                let mut past_headers = false;
                let mut header_acc = Vec::<u8>::new();
                while let Ok(n) = server.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    let slice = &buf[..n];
                    // Skip the initial LSL/110 handshake header so the sniffer focuses on
                    // the live data stream bytes (sample frames after the handshake).
                    if past_headers {
                        let mut guard = st.lock().unwrap();
                        if is_enc {
                            guard.enc.push_wire(slice);
                        } else {
                            guard.plain.push_wire(slice);
                        }
                    } else {
                        header_acc.extend_from_slice(slice);
                        if let Some(pos) = header_acc.windows(4).position(|w| w == b"\r\n\r\n") {
                            past_headers = true;
                            let rest = &header_acc[pos + 4..];
                            if !rest.is_empty() {
                                let mut guard = st.lock().unwrap();
                                if is_enc {
                                    guard.enc.push_wire(rest);
                                } else {
                                    guard.plain.push_wire(rest);
                                }
                            }
                            header_acc.clear();
                        }
                    }
                    if client.write_all(slice).is_err() {
                        break;
                    }
                }
                let _ = up.join();
            });
        }
    });
    proxy_port
}

fn probe_unauthorized_plaintext(port: u16, uid: &str) -> String {
    let Ok(mut sock) = TcpStream::connect(("127.0.0.1", port)) else {
        return "ERROR: could not connect".to_string();
    };
    let _ = sock.set_read_timeout(Some(Duration::from_secs(2)));
    let req = format!(
        "LSL:streamfeed/110 {uid}\r\nNative-Byte-Order: 1234\r\nEndian-Performance: 1000\r\nHas-IEEE754-Floats: 1\r\nSupports-Subnormals: 1\r\nValue-Size: 1\r\nData-Protocol-Version: 110\r\nMax-Buffer-Length: 360\r\nMax-Chunk-Length: 0\r\nHostname: rogue-sniffer\r\nSource-Id: rogue\r\nSession-Id: default\r\nUID: rogue-uid\r\n\r\n"
    );
    if sock.write_all(req.as_bytes()).is_err() {
        return "ERROR: write failed".to_string();
    }
    let mut resp = [0u8; 512];
    match sock.read(&mut resp) {
        Ok(n) if n > 0 => {
            let txt = String::from_utf8_lossy(&resp[..n]);
            txt.lines().next().unwrap_or("closed").trim().to_string()
        }
        _ => "Connection closed by encrypted outlet".to_string(),
    }
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => {}
            c => out.push(c),
        }
    }
    out
}

fn format_hex_ascii(bytes: &VecDeque<u8>) -> (String, String) {
    let slice: Vec<u8> = bytes.iter().copied().collect();
    let tail = if slice.len() > 256 {
        &slice[slice.len() - 256..]
    } else {
        &slice[..]
    };
    let mut hex = String::with_capacity(tail.len() * 3);
    let mut asc = String::with_capacity(tail.len());
    for (i, &b) in tail.iter().enumerate() {
        if i > 0 && i % 16 == 0 {
            hex.push('\n');
            asc.push('\n');
        } else if i > 0 {
            hex.push(' ');
        }
        use std::fmt::Write as _;
        let _ = write!(hex, "{b:02x}");
        asc.push(if b.is_ascii_graphic() || b == b' ' {
            b as char
        } else {
            '.'
        });
    }
    (hex, asc)
}

fn write_side_json(out: &mut String, key: &str, s: &SideState) {
    use std::fmt::Write as _;
    let (hex, asc) = format_hex_ascii(&s.wire_tail);
    let _ = write!(
        out,
        "\"{key}\":{{\"encrypted\":{},\"wire_bytes\":{},\"entropy\":{:.2},\"leaked_hits\":{},\"decoded_count\":{},\"hex\":\"{}\",\"ascii\":\"{}\",\"markers\":[",
        s.encrypted,
        s.total_wire_bytes,
        s.entropy(),
        s.leaked_hits,
        s.decoded_count,
        json_escape(&hex),
        json_escape(&asc)
    );
    for (i, (ts, val)) in s.decoded_markers.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "{{\"ts\":{ts:.2},\"value\":\"{}\"}}", json_escape(val));
    }
    out.push_str("]}");
}

fn state_json(st: &SharedState) -> String {
    let mut out = String::with_capacity(4096);
    out.push('{');
    write_side_json(&mut out, "plain", &st.plain);
    out.push(',');
    write_side_json(&mut out, "enc", &st.enc);
    out.push_str(",\"last_probe\":\"");
    out.push_str(&json_escape(&st.last_probe_result));
    out.push_str("\"}");
    out
}

fn handle_http(mut stream: TcpStream, state: &Arc<Mutex<SharedState>>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let mut buf = [0u8; 4096];
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

    let (status, ct, body) = match (method, path) {
        ("GET", "/") => ("200 OK", "text/html; charset=utf-8", INDEX_HTML.to_string()),
        ("GET", "/api/state") => {
            let st = state.lock().unwrap();
            ("200 OK", "application/json", state_json(&st))
        }
        ("POST", "/api/inject") => {
            let mut msg = String::from("PATIENT=CUSTOM_SECRET|MRN=999999|CONFIDENTIAL_TEST");
            if let Some((_, raw)) = query.split_once("msg=") {
                let decoded = raw
                    .replace("%20", " ")
                    .replace('+', " ")
                    .replace("%7C", "|")
                    .replace("%3D", "=");
                if !decoded.trim().is_empty() {
                    msg = decoded.trim().chars().take(96).collect();
                }
            }
            state.lock().unwrap().custom_queue.push_back(msg);
            ("200 OK", "application/json", "{\"ok\":true}".to_string())
        }
        ("POST", "/api/probe") => {
            let (port, uid) = {
                let g = state.lock().unwrap();
                (g.enc_outlet_port, g.enc_outlet_uid.clone())
            };
            let res = probe_unauthorized_plaintext(port, &uid);
            state.lock().unwrap().last_probe_result = res.clone();
            (
                "200 OK",
                "application/json",
                format!("{{\"result\":\"{}\"}}", json_escape(&res)),
            )
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
    let mut port = 8785u16;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        if a == "--port" {
            if let Some(p) = it.next().and_then(|s| s.parse().ok()) {
                port = p;
            }
        }
    }

    // Use TCP (disable shm/dgram) so both streams traverse the live TCP wire sniffer.
    labstream_net::config::set_content("[labstream]\nFeatures = {seq, resume}\n");

    let state = Arc::new(Mutex::new(SharedState {
        plain: SideState::new(false),
        enc: SideState::new(true),
        enc_outlet_port: 0,
        enc_outlet_uid: String::new(),
        last_probe_result: String::new(),
        custom_queue: VecDeque::new(),
    }));

    // 1. Start Standard LSL plaintext outlet + wire tap + inlet (enc/1 OFF).
    set_runtime_policy(None);
    let mut info_plain = StreamInfo::new("Demo6Plaintext", "Markers", 1, Format::String, 0.0);
    info_plain.source_id = "demo6-plain".to_string();
    let outlet_plain = Outlet::new(info_plain).expect("plain outlet");
    let mut proxied_plain = outlet_plain.info().clone();
    let tap_plain_port = start_wire_tap(proxied_plain.v4data_port, Arc::clone(&state), false);
    proxied_plain.v4address = "127.0.0.1".to_string();
    proxied_plain.v4data_port = tap_plain_port;
    proxied_plain.v6data_port = 0;
    let mut inlet_plain = Inlet::open(&proxied_plain, Duration::from_secs(3)).expect("plain inlet");
    assert!(!inlet_plain.is_encrypted());

    // 2. Enable mandatory enc/1 (X25519 + HKDF-SHA256 + ChaCha20-Poly1305) with PSK.
    set_runtime_policy(Some(EncryptionPolicy::required(Some(
        "hospital-icu-psk-2026".to_string(),
    ))));
    let mut info_enc = StreamInfo::new("Demo6Encrypted", "Markers", 1, Format::String, 0.0);
    info_enc.source_id = "demo6-enc".to_string();
    let outlet_enc = Outlet::new(info_enc).expect("enc outlet");
    let enc_real_port = outlet_enc.info().v4data_port;
    {
        let mut g = state.lock().unwrap();
        g.enc_outlet_port = enc_real_port;
        g.enc_outlet_uid = outlet_enc.info().uid.clone();
    }
    let mut proxied_enc = outlet_enc.info().clone();
    let tap_enc_port = start_wire_tap(enc_real_port, Arc::clone(&state), true);
    proxied_enc.v4address = "127.0.0.1".to_string();
    proxied_enc.v4data_port = tap_enc_port;
    proxied_enc.v6data_port = 0;
    let mut inlet_enc = Inlet::open(&proxied_enc, Duration::from_secs(3)).expect("enc inlet");
    assert!(inlet_enc.is_encrypted());

    let stop = Arc::new(AtomicBool::new(false));

    // Reader thread: Plaintext inlet
    {
        let st = Arc::clone(&state);
        let sp = Arc::clone(&stop);
        thread::spawn(move || {
            while !sp.load(Ordering::Relaxed) {
                if let Ok(Some(s)) = inlet_plain.pull(Duration::from_millis(100)) {
                    let txt = match s.values.first() {
                        Some(Value::Str(v)) => String::from_utf8_lossy(v).into_owned(),
                        _ => String::new(),
                    };
                    let mut g = st.lock().unwrap();
                    g.plain.decoded_count += 1;
                    g.plain.decoded_markers.push_front((s.timestamp, txt));
                    while g.plain.decoded_markers.len() > 8 {
                        g.plain.decoded_markers.pop_back();
                    }
                }
            }
        });
    }

    // Reader thread: Encrypted inlet
    {
        let st = Arc::clone(&state);
        let sp = Arc::clone(&stop);
        thread::spawn(move || {
            while !sp.load(Ordering::Relaxed) {
                if let Ok(Some(s)) = inlet_enc.pull(Duration::from_millis(100)) {
                    let txt = match s.values.first() {
                        Some(Value::Str(v)) => String::from_utf8_lossy(v).into_owned(),
                        _ => String::new(),
                    };
                    let mut g = st.lock().unwrap();
                    g.enc.decoded_count += 1;
                    g.enc.decoded_markers.push_front((s.timestamp, txt));
                    while g.enc.decoded_markers.len() > 8 {
                        g.enc.decoded_markers.pop_back();
                    }
                }
            }
        });
    }

    // Sender thread: pushes identical clinical markers into both outlets.
    {
        let st = Arc::clone(&state);
        let sp = Arc::clone(&stop);
        thread::spawn(move || {
            let mut idx = 0usize;
            while !sp.load(Ordering::Relaxed) {
                let custom = st.lock().unwrap().custom_queue.pop_front();
                let payload = custom.unwrap_or_else(|| {
                    let s = CLINICAL_TEMPLATES[idx % CLINICAL_TEMPLATES.len()];
                    idx += 1;
                    format!("{s}|SEQ={idx:04}")
                });
                let ts = labstream_net::clock();
                let bytes = payload.into_bytes();
                outlet_plain.push_packed(PackedSample::from_strings(ts, vec![bytes.clone()]));
                outlet_enc.push_packed(PackedSample::from_strings(ts, vec![bytes]));
                thread::sleep(Duration::from_millis(900));
            }
        });
    }

    let addr = format!("127.0.0.1:{port}");
    let listener = TcpListener::bind(&addr).expect("bind HTTP server");
    println!("============================================================");
    println!("  labstream Demo 6 — Hospital Wi-Fi Security (enc/1 Sniffer)");
    println!("  Open in browser:  http://{addr}/");
    println!("============================================================");

    for conn in listener.incoming().flatten() {
        let st = Arc::clone(&state);
        thread::spawn(move || handle_http(conn, &st));
    }
}

const INDEX_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>labstream Demo 6 — Hospital Wi-Fi Security (enc/1)</title>
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
    margin: 0; padding: 18px; background: var(--bg); color: var(--text);
    font-family: ui-sans-serif, system-ui, -apple-system, Segoe UI, Roboto, sans-serif;
  }
  .wrap { max-width: 1220px; margin: 0 auto; }
  header {
    display: flex; flex-wrap: wrap; justify-content: space-between; align-items: center;
    gap: 12px; margin-bottom: 14px;
  }
  h1 { margin: 0; font-size: 20px; }
  .sub { color: var(--muted); font-size: 13px; margin-top: 3px; }
  .toolbar { display: flex; gap: 8px; flex-wrap: wrap; align-items: center; }
  input {
    background: #0d131f; color: var(--text); border: 1px solid var(--border);
    border-radius: 7px; padding: 7px 10px; font-size: 12px; width: 250px;
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  }
  button {
    background: #1b2536; color: var(--text); border: 1px solid var(--border);
    border-radius: 7px; padding: 7px 12px; font-size: 12px; cursor: pointer; font-weight: 600;
  }
  button:hover { border-color: #4b6588; }
  button.warn { background: #450a0a; border-color: var(--red); color: #fecaca; }
  button.ok { background: #065f46; border-color: var(--green); color: #ecfdf5; }
  .two-col { display: grid; grid-template-columns: 1fr 1fr; gap: 14px; }
  @media (max-width: 900px) { .two-col { grid-template-columns: 1fr; } }
  .card {
    background: var(--panel); border: 1px solid var(--border);
    border-radius: 12px; padding: 14px;
  }
  .card h2 {
    margin: 0 0 10px 0; font-size: 15px; display: flex;
    justify-content: space-between; align-items: center; flex-wrap: wrap; gap: 8px;
  }
  .badge {
    padding: 3px 9px; border-radius: 999px; font-size: 11px; font-weight: 700;
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  }
  .badge.bad { background: #450a0a; border: 1px solid #991b1b; color: var(--red); }
  .badge.good { background: #052e16; border: 1px solid #14532d; color: var(--green); }
  .stats {
    display: flex; gap: 14px; flex-wrap: wrap; font-size: 12px;
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
    color: var(--muted); margin-bottom: 10px;
  }
  .wire-box {
    display: grid; grid-template-columns: 1.65fr 1fr; gap: 10px;
    background: #080c13; border: 1px solid #1c2638; border-radius: 8px; padding: 10px;
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 11px;
    line-height: 1.45; min-height: 240px; overflow-x: auto;
  }
  pre { margin: 0; white-space: pre; }
  .leak { background: rgba(248, 113, 113, 0.28); color: #fecaca; font-weight: 700; border-radius: 2px; }
  .section-lbl { font-size: 11px; font-weight: 700; color: var(--muted); text-transform: uppercase; margin: 12px 0 6px 0; }
  .rx-list {
    background: #080c13; border: 1px solid #1c2638; border-radius: 8px; padding: 8px;
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 11px;
    min-height: 110px;
  }
  .rx-row { padding: 3px 0; border-bottom: 1px solid #141d2e; color: #cbd5e1; }
  .probe-bar {
    margin-top: 14px; padding: 12px 14px; background: var(--panel);
    border: 1px solid var(--border); border-radius: 10px;
    display: flex; justify-content: space-between; align-items: center; flex-wrap: wrap; gap: 10px;
  }
  .probe-out { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 12px; color: var(--gold); }
</style>
</head>
<body>
<div class="wrap">
  <header>
    <div>
      <h1>Demo 6 — Hospital Wi-Fi Security (<code>enc/1</code> Live Wire Sniffer)</h1>
      <div class="sub">Real TCP socket tap comparing Standard LSL plaintext vs <code>labstream enc/1</code> (X25519 + HKDF-SHA256 + ChaCha20-Poly1305)</div>
    </div>
    <div class="toolbar">
      <input id="customMsg" value="PATIENT=VIP_TEST|MRN=777123|SECRET" />
      <button id="injectBtn">Inject Secret Marker</button>
      <button id="pauseBtn">Pause</button>
      <button id="exportBtn" class="ok">Export PNG</button>
    </div>
  </header>

  <div class="two-col">
    <div class="card">
      <h2>
        <span style="color:var(--red)">Standard LSL (Plaintext TCP)</span>
        <span id="plainBadge" class="badge bad">EXPOSED ON WIRE</span>
      </h2>
      <div class="stats" id="plainStats">Wire: 0 B | Entropy: -- bits/B | Leaks: 0</div>
      <div class="wire-box">
        <pre id="plainHex" style="color:#94a3b8"></pre>
        <pre id="plainAsc" style="color:#fca5a5"></pre>
      </div>
      <div class="section-lbl">Inlet Received Markers</div>
      <div class="rx-list" id="plainRx"></div>
    </div>

    <div class="card">
      <h2>
        <span style="color:var(--green)">labstream enc/1 (ChaCha20-Poly1305)</span>
        <span id="encBadge" class="badge good">✓ ENCRYPTED (0 LEAKS)</span>
      </h2>
      <div class="stats" id="encStats">Wire: 0 B | Entropy: -- bits/B | Leaks: 0</div>
      <div class="wire-box">
        <pre id="encHex" style="color:#6ee7b7"></pre>
        <pre id="encAsc" style="color:#34d399"></pre>
      </div>
      <div class="section-lbl">Authorized Inlet Decrypted Markers</div>
      <div class="rx-list" id="encRx"></div>
    </div>
  </div>

  <div class="probe-bar">
    <div>
      <strong>Unauthorized Eavesdropper Test:</strong>
      <span style="color:var(--muted);font-size:13px"> Send a real plaintext <code>LSL:feed/110</code> handshake to the encrypted outlet</span>
    </div>
    <div style="display:flex;align-items:center;gap:12px;flex-wrap:wrap">
      <span id="probeOut" class="probe-out">Click button to test →</span>
      <button id="probeBtn" class="warn">Test Unauthorized Client</button>
    </div>
  </div>
</div>

<script>
let paused = false;
let lastState = null;

document.getElementById('pauseBtn').onclick = () => {
  paused = !paused;
  document.getElementById('pauseBtn').textContent = paused ? 'Resume' : 'Pause';
};
document.getElementById('injectBtn').onclick = () => {
  const v = encodeURIComponent(document.getElementById('customMsg').value);
  fetch(`/api/inject?msg=${v}`, { method: 'POST' });
};
document.getElementById('probeBtn').onclick = async () => {
  const r = await fetch('/api/probe', { method: 'POST' });
  const d = await r.json();
  document.getElementById('probeOut').textContent = `Outlet response: "${d.result}"`;
};

function escHtml(s) {
  return s.replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;');
}

function highlightLeaks(asc) {
  return escHtml(asc).replace(/(PATIENT|MRN=\d+|JANE_DOE|SEIZURE_ONSET|LORAZEPAM|VIP_TEST|SECRET)/g, '<span class="leak">$1</span>');
}

function updateUI(st) {
  lastState = st;
  const p = st.plain, e = st.enc;

  document.getElementById('plainStats').textContent =
    `Wire: ${p.wire_bytes.toLocaleString('en-US')} B | Shannon Entropy: ${p.entropy.toFixed(2)} / 8.0 bits/B | Plaintext Leaks: ${p.leaked_hits}`;
  document.getElementById('plainBadge').textContent = `✗ EXPOSED (${p.leaked_hits} LEAKS)`;
  document.getElementById('plainHex').textContent = p.hex;
  document.getElementById('plainAsc').innerHTML = highlightLeaks(p.ascii);
  document.getElementById('plainRx').innerHTML = (p.markers || []).slice(0, 5)
    .map(m => `<div class="rx-row">${escHtml(m.value)}</div>`).join('');

  document.getElementById('encStats').textContent =
    `Wire: ${e.wire_bytes.toLocaleString('en-US')} B | Shannon Entropy: ${e.entropy.toFixed(2)} / 8.0 bits/B | Plaintext Leaks: ${e.leaked_hits}`;
  document.getElementById('encBadge').textContent =
    e.leaked_hits === 0 ? `✓ ENCRYPTED (0 LEAKS)` : `✗ LEAK DETECTED`;
  document.getElementById('encHex').textContent = e.hex;
  document.getElementById('encAsc').textContent = e.ascii;
  document.getElementById('encRx').innerHTML = (e.markers || []).slice(0, 5)
    .map(m => `<div class="rx-row" style="color:#a7f3d0">✓ ${escHtml(m.value)}</div>`).join('');

  if (st.last_probe) {
    document.getElementById('probeOut').textContent = `Outlet response: "${st.last_probe}"`;
  }
}

document.getElementById('exportBtn').onclick = () => {
  if (!lastState) return;
  const st = lastState;
  const out = document.createElement('canvas');
  out.width = 1180; out.height = 500;
  const ctx = out.getContext('2d');
  ctx.fillStyle = '#0b0f17'; ctx.fillRect(0, 0, out.width, out.height);

  ctx.fillStyle = '#e6edf7'; ctx.font = 'bold 21px sans-serif';
  ctx.fillText('labstream v0.2.0 — Hospital Wi-Fi Security (enc/1 Wire Sniffer)', 24, 36);

  const drawPanel = (x, title, color, side) => {
    ctx.fillStyle = '#131a26'; ctx.fillRect(x, 56, 554, 418);
    ctx.strokeStyle = '#233046'; ctx.strokeRect(x, 56, 554, 418);
    ctx.fillStyle = color; ctx.font = 'bold 15px sans-serif';
    ctx.fillText(title, x + 14, 82);
    ctx.fillStyle = '#94a3b8'; ctx.font = '12px monospace';
    ctx.fillText(`Entropy: ${side.entropy.toFixed(2)} bits/B  |  Plaintext leaks: ${side.leaked_hits}  |  Decoded: ${side.decoded_count}`, x + 14, 104);
    ctx.fillStyle = '#080c13'; ctx.fillRect(x + 14, 116, 526, 232);
    ctx.fillStyle = color; ctx.font = '11px monospace';
    const lines = (side.ascii || '').split('\n').slice(0, 14);
    const hlines = (side.hex || '').split('\n').slice(0, 14);
    lines.forEach((l, i) => {
      ctx.fillStyle = '#64748b';
      ctx.fillText((hlines[i] || '').slice(0, 47), x + 22, 134 + i * 15);
      ctx.fillStyle = color;
      ctx.fillText(l, x + 390, 134 + i * 15);
    });
    ctx.fillStyle = '#cbd5e1'; ctx.font = '11px monospace';
    (side.markers || []).slice(0, 4).forEach((m, i) => {
      ctx.fillText((side.encrypted ? '✓ ' : '') + m.value.slice(0, 68), x + 14, 372 + i * 18);
    });
  };
  drawPanel(24, 'Standard LSL (Plaintext TCP — EXPOSED)', '#f87171', st.plain);
  drawPanel(602, 'labstream enc/1 (ChaCha20-Poly1305 — 0 LEAKS)', '#34d399', st.enc);

  const a = document.createElement('a');
  a.download = 'labstream_demo6_encryption.png';
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
  setTimeout(poll, 300);
}
poll();
</script>
</body>
</html>
"#;
