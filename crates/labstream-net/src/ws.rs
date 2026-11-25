//! Zero-dependency RFC 6455 WebSocket bridge for browser reach
//! (`docs/roadmap.md`, step 13).
//!
//! Browsers cannot open the raw UDP/TCP sockets of LSL. [`WsBridge`] adds an
//! opt-in WebSocket endpoint (off by default, bound to `127.0.0.1` and
//! accepting only loopback peers unless configured otherwise) that:
//!
//! 1. Lists discovered LSL streams (`{"op":"list"}` or `GET /streams`).
//! 2. Subscribes a browser to any LSL stream (`{"op":"subscribe",...}`) and
//!    forwards exact LSL wire-encoded samples in binary WebSocket frames
//!    (`[u32 stream_id LE][u32 count LE][LSL wire samples...]`) for decoding by
//!    `labstream-wasm`, plus live `meta/1` metadata updates as JSON text
//!    frames.
//! 3. Publishes browser markers (`{"op":"marker",...}`) onto a live LSL string
//!    marker outlet.

use crate::info::StreamInfo;
use crate::{config, resolve, ContinuousResolver, Inlet, Outlet};
use labstream_wire::{ByteOrder, Codec, Format, PackedSample, Sample, Value};
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
#[cfg(test)]
use std::time::Instant;

const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const MAX_WS_FRAME: usize = 8 * 1024 * 1024;
const DEFAULT_MARKER_STREAM: &str = "BrowserMarkers";

/// Configuration for [`WsBridge::start`].
#[derive(Debug, Clone)]
pub struct WsOptions {
    /// Local address and port to bind (default `127.0.0.1:8765`).
    pub bind: SocketAddr,
    /// Additional non-loopback peer IP addresses allowed to connect.
    /// Loopback (`127.0.0.1`, `::1`) is always allowed.
    pub allowed_peers: Vec<IpAddr>,
    /// When true, accept connections from any IP address. Default `false`.
    pub allow_any: bool,
    /// Optional LSL session identifier to discover and publish markers under.
    /// `None` uses the active configuration (`lab.SessionID`).
    pub session_id: Option<String>,
    /// Default LSL outlet name for markers sent by browser pages without an
    /// explicit `stream` field.
    pub marker_stream_name: String,
}

impl Default for WsOptions {
    fn default() -> Self {
        WsOptions {
            bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8765),
            allowed_peers: Vec::new(),
            allow_any: false,
            session_id: None,
            marker_stream_name: DEFAULT_MARKER_STREAM.to_string(),
        }
    }
}

impl WsOptions {
    /// True when `ip` is permitted to connect under this policy.
    pub fn is_peer_allowed(&self, ip: IpAddr) -> bool {
        if self.allow_any || ip.is_loopback() {
            return true;
        }
        self.allowed_peers.contains(&ip)
    }
}

struct Shared {
    opts: WsOptions,
    session_id: String,
    resolver: ContinuousResolver,
    markers: Mutex<HashMap<String, Arc<Outlet>>>,
    stop: AtomicBool,
}

impl Shared {
    fn list_streams(&self) -> Vec<StreamInfo> {
        self.resolver.results(usize::MAX)
    }

    fn find_stream(&self, target: &str) -> Option<StreamInfo> {
        let cached = self.resolver.results(usize::MAX);
        if let Some(found) = cached
            .into_iter()
            .find(|s| s.uid == target || s.name == target)
        {
            return Some(found);
        }
        let query = format!("session_id='{}'", self.session_id);
        resolve(&query, usize::MAX, Duration::from_secs(1))
            .ok()?
            .into_iter()
            .find(|s| s.uid == target || s.name == target)
    }

    fn push_marker(&self, stream_name: &str, value: &str, timestamp: f64) -> io::Result<f64> {
        let name = if stream_name.is_empty() {
            self.opts.marker_stream_name.as_str()
        } else {
            stream_name
        };
        let outlet = {
            let mut map = self.markers.lock().unwrap();
            if let Some(o) = map.get(name) {
                Arc::clone(o)
            } else {
                let mut info = StreamInfo::new(name, "Markers", 1, Format::String, 0.0);
                info.session_id = self.session_id.clone();
                info.source_id = format!("labstream-ws-marker-{name}");
                let created = Arc::new(Outlet::new(info)?);
                map.insert(name.to_string(), Arc::clone(&created));
                created
            }
        };
        let ts = if timestamp == 0.0 || !timestamp.is_finite() {
            crate::clock()
        } else {
            timestamp
        };
        outlet.push(&Sample {
            timestamp: ts,
            values: vec![Value::Str(value.as_bytes().to_vec())],
        });
        Ok(ts)
    }
}

/// Running WebSocket bridge serving browser viewers and marker publishers.
pub struct WsBridge {
    addr: SocketAddr,
    shared: Arc<Shared>,
    accept_thread: Option<JoinHandle<()>>,
}

impl WsBridge {
    /// Start the WebSocket bridge on `opts.bind`.
    pub fn start(opts: WsOptions) -> io::Result<WsBridge> {
        let listener = TcpListener::bind(opts.bind)?;
        let addr = listener.local_addr()?;
        listener.set_nonblocking(true)?;

        let session_id = opts
            .session_id
            .clone()
            .unwrap_or_else(|| config::get().session_id.clone());
        let query = format!("session_id='{session_id}'");
        let resolver = ContinuousResolver::new(&query, 5.0);

        let shared = Arc::new(Shared {
            opts,
            session_id,
            resolver,
            markers: Mutex::new(HashMap::new()),
            stop: AtomicBool::new(false),
        });

        let sh = Arc::clone(&shared);
        let accept_thread = std::thread::spawn(move || {
            while !sh.stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, peer)) => {
                        let _ = stream.set_nonblocking(false);
                        let _ = stream.set_nodelay(true);
                        if !sh.opts.is_peer_allowed(peer.ip()) {
                            let _ = stream.write_all(
                                b"HTTP/1.1 403 Forbidden\r\nContent-Length: 9\r\nConnection: close\r\n\r\nForbidden",
                            );
                            let _ = stream.flush();
                            continue;
                        }
                        let conn_shared = Arc::clone(&sh);
                        std::thread::spawn(move || {
                            let _ = handle_connection(stream, conn_shared);
                        });
                    }
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(WsBridge {
            addr,
            shared,
            accept_thread: Some(accept_thread),
        })
    }

    /// Bound local socket address (useful when binding port `0`).
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Stop the accept loop and close any active sessions.
    pub fn stop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.accept_thread.take() {
            let _ = h.join();
        }
    }
}

impl Drop for WsBridge {
    fn drop(&mut self) {
        self.stop();
    }
}

fn handle_connection(mut stream: TcpStream, shared: Arc<Shared>) -> io::Result<()> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    let header_end = loop {
        if buf.len() > 16384 {
            let _ = stream.write_all(b"HTTP/1.1 431 Request Header Fields Too Large\r\n\r\n");
            return Ok(());
        }
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };

    let req_text = String::from_utf8_lossy(&buf[..header_end]);
    let mut lines = req_text.lines();
    let first = lines.next().unwrap_or("");
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("/");

    let mut headers: HashMap<String, String> = HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }

    if method != "GET" {
        let _ = stream.write_all(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\n\r\n");
        return Ok(());
    }

    let is_upgrade = headers
        .get("upgrade")
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
    if is_upgrade {
        let Some(key) = headers.get("sec-websocket-key") else {
            let _ = stream.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n");
            return Ok(());
        };
        let accept = ws_accept_key(key);
        let resp = format!(
            "HTTP/1.1 101 Switching Protocols\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Accept: {accept}\r\n\r\n"
        );
        stream.write_all(resp.as_bytes())?;
        stream.flush()?;
        return run_ws_session(stream, shared);
    }

    if path == "/streams" {
        let body = streams_list_json(&shared.list_streams());
        let resp = format!(
            "HTTP/1.1 200 OK\r\n\
             Content-Type: application/json\r\n\
             Access-Control-Allow-Origin: *\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(resp.as_bytes())?;
        stream.flush()?;
        return Ok(());
    }

    if path == "/" || path == "/index.html" {
        let body = BUILTIN_HTML;
        let resp = format!(
            "HTTP/1.1 200 OK\r\n\
             Content-Type: text/html; charset=utf-8\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(resp.as_bytes())?;
        stream.flush()?;
        return Ok(());
    }

    let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
    Ok(())
}

struct SubHandle {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Drop for SubHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
    }
}

fn run_ws_session(mut reader: TcpStream, shared: Arc<Shared>) -> io::Result<()> {
    let writer = Arc::new(Mutex::new(reader.try_clone()?));
    let _ = reader.set_read_timeout(Some(Duration::from_millis(100)));
    let next_sub_id = AtomicU32::new(1);
    let mut subs: HashMap<u32, SubHandle> = HashMap::new();

    while !shared.stop.load(Ordering::Relaxed) {
        let frame = match read_ws_frame(&mut reader, &writer) {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(ref e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(_) => break,
        };
        if frame.opcode != 0x1 {
            continue;
        }
        let Ok(text) = std::str::from_utf8(&frame.payload) else {
            continue;
        };
        let fields = parse_flat_json(text);
        let op = fields.get("op").map(String::as_str).unwrap_or("");
        match op {
            "list" => {
                let json = streams_list_json(&shared.list_streams());
                send_ws_text(&writer, &json)?;
            }
            "subscribe" => {
                let target = fields.get("stream").map(String::as_str).unwrap_or("");
                let postprocess: u32 = fields
                    .get("postprocess")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                let Some(info) = shared.find_stream(target) else {
                    let msg = format!(
                        "{{\"op\":\"error\",\"message\":\"stream not found: {}\"}}",
                        json_escape(target)
                    );
                    send_ws_text(&writer, &msg)?;
                    continue;
                };
                let mut inlet = match Inlet::open_recovering(&info, Duration::from_secs(3), 4096) {
                    Ok(i) => i,
                    Err(e) => {
                        let msg = format!(
                            "{{\"op\":\"error\",\"message\":\"{}\"}}",
                            json_escape(&e.to_string())
                        );
                        send_ws_text(&writer, &msg)?;
                        continue;
                    }
                };
                inlet.set_postprocessing(postprocess);
                let sub_id = next_sub_id.fetch_add(1, Ordering::Relaxed);
                let info_json = stream_info_json(&info);
                let ack = format!(
                    "{{\"op\":\"subscribed\",\"id\":{sub_id},\"info\":{info_json},\
                     \"format\":{},\"channel_count\":{},\"nominal_srate\":{},\"byte_order\":\"le\"}}",
                    info.format as u8, info.channel_count, info.nominal_srate
                );
                send_ws_text(&writer, &ack)?;

                let sub_stop = Arc::new(AtomicBool::new(false));
                let sub_stop_w = Arc::clone(&sub_stop);
                let bridge_stop = Arc::clone(&shared);
                let w = Arc::clone(&writer);
                let fmt = info.format;
                let chs = info.channel_count as usize;
                let worker = std::thread::spawn(move || {
                    let codec = Codec::new(fmt, chs, ByteOrder::Little);
                    let mut batch: Vec<PackedSample> = Vec::with_capacity(256);
                    let mut frame_buf: Vec<u8> = Vec::with_capacity(4096);
                    while !sub_stop_w.load(Ordering::Relaxed)
                        && !bridge_stop.stop.load(Ordering::Relaxed)
                    {
                        // Forward any live `meta/1` updates.
                        for upd in inlet.pull_metadata() {
                            let msg = format!(
                                "{{\"op\":\"meta\",\"id\":{sub_id},\"seq\":{},\"timestamp\":{:.9},\"key\":\"{}\",\"value\":\"{}\"}}",
                                upd.seq,
                                upd.timestamp,
                                json_escape(&upd.key),
                                json_escape(&upd.value)
                            );
                            if send_ws_text(&w, &msg).is_err() {
                                return;
                            }
                        }

                        batch.clear();
                        match inlet.pull_chunk_packed(&mut batch, 256, Duration::from_millis(20)) {
                            Ok(0) => {
                                if inlet.is_finished() {
                                    break;
                                }
                            }
                            Ok(n) => {
                                frame_buf.clear();
                                frame_buf.extend_from_slice(&sub_id.to_le_bytes());
                                frame_buf.extend_from_slice(&(n as u32).to_le_bytes());
                                for s in &batch {
                                    if codec.encode_packed_at(s, s.timestamp, &mut frame_buf).is_err() {
                                        return;
                                    }
                                }
                                if send_ws_binary(&w, &frame_buf).is_err() {
                                    return;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                });
                subs.insert(
                    sub_id,
                    SubHandle {
                        stop: sub_stop,
                        worker: Some(worker),
                    },
                );
            }
            "unsubscribe" => {
                if let Some(id) = fields.get("id").and_then(|s| s.parse::<u32>().ok()) {
                    subs.remove(&id);
                    let ack = format!("{{\"op\":\"unsubscribed\",\"id\":{id}}}");
                    send_ws_text(&writer, &ack)?;
                }
            }
            "marker" => {
                let stream_name = fields.get("stream").map(String::as_str).unwrap_or("");
                let value = fields.get("value").map(String::as_str).unwrap_or("");
                let ts: f64 = fields
                    .get("timestamp")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0.0);
                match shared.push_marker(stream_name, value, ts) {
                    Ok(actual_ts) => {
                        let used = if stream_name.is_empty() {
                            shared.opts.marker_stream_name.as_str()
                        } else {
                            stream_name
                        };
                        let ack = format!(
                            "{{\"op\":\"marker_ok\",\"stream\":\"{}\",\"timestamp\":{actual_ts:.9}}}",
                            json_escape(used)
                        );
                        send_ws_text(&writer, &ack)?;
                    }
                    Err(e) => {
                        let msg = format!(
                            "{{\"op\":\"error\",\"message\":\"{}\"}}",
                            json_escape(&e.to_string())
                        );
                        send_ws_text(&writer, &msg)?;
                    }
                }
            }
            _ => {
                let msg = format!("{{\"op\":\"error\",\"message\":\"unknown op: {}\"}}", json_escape(op));
                send_ws_text(&writer, &msg)?;
            }
        }
    }

    // Drop all active subscriptions cleanly before closing the socket.
    drop(subs);
    Ok(())
}

fn send_ws_text(writer: &Mutex<TcpStream>, text: &str) -> io::Result<()> {
    let mut w = writer.lock().unwrap();
    write_ws_frame(&mut *w, 0x1, text.as_bytes())
}

fn send_ws_binary(writer: &Mutex<TcpStream>, payload: &[u8]) -> io::Result<()> {
    let mut w = writer.lock().unwrap();
    write_ws_frame(&mut *w, 0x2, payload)
}

/// Decoded RFC 6455 WebSocket message frame.
struct WsFrame {
    opcode: u8,
    payload: Vec<u8>,
}

fn write_ws_frame<W: Write>(out: &mut W, opcode: u8, payload: &[u8]) -> io::Result<()> {
    let mut header = Vec::with_capacity(10);
    header.push(0x80 | (opcode & 0x0F));
    let len = payload.len();
    if len < 126 {
        header.push(len as u8);
    } else if len <= 0xFFFF {
        header.push(126);
        header.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        header.push(127);
        header.extend_from_slice(&(len as u64).to_be_bytes());
    }
    out.write_all(&header)?;
    out.write_all(payload)?;
    out.flush()
}

fn read_ws_frame(reader: &mut TcpStream, writer: &Mutex<TcpStream>) -> io::Result<Option<WsFrame>> {
    let mut msg_opcode: Option<u8> = None;
    let mut msg_payload = Vec::new();
    loop {
        let mut h = [0u8; 2];
        reader.read_exact(&mut h)?;
        let fin = (h[0] & 0x80) != 0;
        let opcode = h[0] & 0x0F;
        let masked = (h[1] & 0x80) != 0;
        let mut len = (h[1] & 0x7F) as u64;
        if len == 126 {
            let mut ext = [0u8; 2];
            reader.read_exact(&mut ext)?;
            len = u16::from_be_bytes(ext) as u64;
        } else if len == 127 {
            let mut ext = [0u8; 8];
            reader.read_exact(&mut ext)?;
            len = u64::from_be_bytes(ext);
        }
        if len as usize > MAX_WS_FRAME || msg_payload.len() + len as usize > MAX_WS_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "WebSocket frame too large",
            ));
        }
        let mask_key = if masked {
            let mut m = [0u8; 4];
            reader.read_exact(&mut m)?;
            Some(m)
        } else {
            None
        };
        let mut data = vec![0u8; len as usize];
        if !data.is_empty() {
            reader.read_exact(&mut data)?;
        }
        if let Some(m) = mask_key {
            for (i, b) in data.iter_mut().enumerate() {
                *b ^= m[i & 3];
            }
        }
        match opcode {
            0x8 => {
                // Close frame: echo Close and end session.
                let _ = write_ws_frame(&mut *writer.lock().unwrap(), 0x8, &[]);
                return Ok(None);
            }
            0x9 => {
                // Ping: reply with Pong carrying the same payload.
                let _ = write_ws_frame(&mut *writer.lock().unwrap(), 0xA, &data);
                continue;
            }
            0xA => continue, // Pong
            0x0 => {
                msg_payload.extend_from_slice(&data);
                if fin {
                    return Ok(Some(WsFrame {
                        opcode: msg_opcode.unwrap_or(0x1),
                        payload: msg_payload,
                    }));
                }
            }
            other => {
                msg_opcode = Some(other);
                msg_payload.extend_from_slice(&data);
                if fin {
                    return Ok(Some(WsFrame {
                        opcode: other,
                        payload: msg_payload,
                    }));
                }
            }
        }
    }
}

fn stream_info_json(s: &StreamInfo) -> String {
    format!(
        "{{\"uid\":\"{}\",\"name\":\"{}\",\"type\":\"{}\",\"channel_count\":{},\"format\":{},\"nominal_srate\":{},\"source_id\":\"{}\",\"hostname\":\"{}\"}}",
        json_escape(&s.uid),
        json_escape(&s.name),
        json_escape(&s.stream_type),
        s.channel_count,
        s.format as u8,
        s.nominal_srate,
        json_escape(&s.source_id),
        json_escape(&s.hostname),
    )
}

fn streams_list_json(streams: &[StreamInfo]) -> String {
    let mut out = String::from("{\"op\":\"streams\",\"streams\":[");
    for (i, s) in streams.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&stream_info_json(s));
    }
    out.push_str("]}");
    out
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
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
    out
}

/// Minimal parser for flat JSON objects (`{"k":"v","n":123}`).
fn parse_flat_json(input: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let bytes = input.as_bytes();
    let mut i = 0;
    let skip_ws = |idx: &mut usize| {
        while *idx < bytes.len() && bytes[*idx].is_ascii_whitespace() {
            *idx += 1;
        }
    };
    let parse_str = |idx: &mut usize| -> Option<String> {
        skip_ws(idx);
        if *idx >= bytes.len() || bytes[*idx] != b'"' {
            return None;
        }
        *idx += 1;
        let mut s = String::new();
        while *idx < bytes.len() {
            let b = bytes[*idx];
            *idx += 1;
            if b == b'"' {
                return Some(s);
            }
            if b == b'\\' && *idx < bytes.len() {
                let esc = bytes[*idx];
                *idx += 1;
                match esc {
                    b'"' => s.push('"'),
                    b'\\' => s.push('\\'),
                    b'/' => s.push('/'),
                    b'n' => s.push('\n'),
                    b'r' => s.push('\r'),
                    b't' => s.push('\t'),
                    _ => s.push(esc as char),
                }
            } else {
                s.push(b as char);
            }
        }
        None
    };

    skip_ws(&mut i);
    if i >= bytes.len() || bytes[i] != b'{' {
        return map;
    }
    i += 1;
    loop {
        skip_ws(&mut i);
        if i >= bytes.len() || bytes[i] == b'}' {
            break;
        }
        let Some(key) = parse_str(&mut i) else {
            break;
        };
        skip_ws(&mut i);
        if i >= bytes.len() || bytes[i] != b':' {
            break;
        }
        i += 1;
        skip_ws(&mut i);
        if i >= bytes.len() {
            break;
        }
        let val = if bytes[i] == b'"' {
            match parse_str(&mut i) {
                Some(v) => v,
                None => break,
            }
        } else {
            let start = i;
            while i < bytes.len()
                && !matches!(bytes[i], b',' | b'}' | b' ' | b'\t' | b'\r' | b'\n')
            {
                i += 1;
            }
            String::from_utf8_lossy(&bytes[start..i]).to_string()
        };
        map.insert(key, val);
        skip_ws(&mut i);
        if i < bytes.len() && bytes[i] == b',' {
            i += 1;
        } else {
            break;
        }
    }
    map
}

/// Compute `Sec-WebSocket-Accept = Base64(SHA-1(key + WS_GUID))` per RFC 6455.
pub(crate) fn ws_accept_key(key: &str) -> String {
    let mut input = Vec::with_capacity(key.len() + WS_GUID.len());
    input.extend_from_slice(key.trim().as_bytes());
    input.extend_from_slice(WS_GUID.as_bytes());
    let digest = sha1(&input);
    base64_encode(&digest)
}

/// Standard FIPS 180-4 SHA-1 (used solely for the RFC 6455 WebSocket handshake).
fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h0: u32 = 0x6745_2301;
    let mut h1: u32 = 0xEFCD_AB89;
    let mut h2: u32 = 0x98BA_DCFE;
    let mut h3: u32 = 0x1032_5476;
    let mut h4: u32 = 0xC3D2_E1F0;

    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while (msg.len() % 64) != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    let mut w = [0u32; 80];
    for chunk in msg.chunks_exact(64) {
        for (i, word) in chunk.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h0, h1, h2, h3, h4);
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1u32),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        h0 = h0.wrapping_add(a);
        h1 = h1.wrapping_add(b);
        h2 = h2.wrapping_add(c);
        h3 = h3.wrapping_add(d);
        h4 = h4.wrapping_add(e);
    }

    let mut out = [0u8; 20];
    out[0..4].copy_from_slice(&h0.to_be_bytes());
    out[4..8].copy_from_slice(&h1.to_be_bytes());
    out[8..12].copy_from_slice(&h2.to_be_bytes());
    out[12..16].copy_from_slice(&h3.to_be_bytes());
    out[16..20].copy_from_slice(&h4.to_be_bytes());
    out
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Minimal built-in status and marker page served at `GET /`.
const BUILTIN_HTML: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>labstream WebSocket bridge</title></head>
<body style="font-family:system-ui,sans-serif;max-width:720px;margin:2rem auto">
<h1>labstream WebSocket bridge</h1>
<p>Connect to <code>ws://&lt;host&gt;/ws</code> or load <code>web/index.html</code> with <code>labstream_wasm.wasm</code>.</p>
<p><a href="/streams">GET /streams</a></p>
</body></html>
"#;

/// Wait briefly for a condition in tests.
#[cfg(test)]
fn wait_until(timeout: Duration, mut pred: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + timeout;
    while Instant::now() < end {
        if pred() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    pred()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc6455_accept_key_matches_spec_vector() {
        assert_eq!(
            ws_accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn default_policy_allows_only_loopback() {
        let opts = WsOptions::default();
        assert!(opts.is_peer_allowed(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert!(opts.is_peer_allowed(IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)));
        assert!(!opts.is_peer_allowed(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10))));

        let mut custom = opts.clone();
        custom
            .allowed_peers
            .push(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10)));
        assert!(custom.is_peer_allowed(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10))));
        assert!(!custom.is_peer_allowed(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5))));
    }

    fn ws_client_connect(addr: SocketAddr) -> TcpStream {
        let mut s = TcpStream::connect(addr).expect("connect");
        let req = format!(
            "GET /ws HTTP/1.1\r\nHost: {addr}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
        );
        s.write_all(req.as_bytes()).unwrap();
        let mut resp = Vec::new();
        let mut b = [0u8; 1];
        while !resp.ends_with(b"\r\n\r\n") {
            s.read_exact(&mut b).unwrap();
            resp.push(b[0]);
        }
        let text = String::from_utf8_lossy(&resp);
        assert!(text.contains("101 Switching Protocols"), "got: {text}");
        assert!(text.contains("s3pPLMBiTxaQ9kYGzzhZRbK+xOo="));
        s
    }

    fn ws_client_send_text(s: &mut TcpStream, text: &str) {
        let payload = text.as_bytes();
        let mask = [0x12u8, 0x34, 0x56, 0x78];
        let mut out = vec![0x81u8];
        if payload.len() < 126 {
            out.push(0x80 | payload.len() as u8);
        } else {
            out.push(0x80 | 126);
            out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        }
        out.extend_from_slice(&mask);
        for (i, &b) in payload.iter().enumerate() {
            out.push(b ^ mask[i & 3]);
        }
        s.write_all(&out).unwrap();
        s.flush().unwrap();
    }

    fn ws_client_read_frame(s: &mut TcpStream) -> (u8, Vec<u8>) {
        let mut h = [0u8; 2];
        s.read_exact(&mut h).unwrap();
        let opcode = h[0] & 0x0F;
        let mut len = (h[1] & 0x7F) as usize;
        if len == 126 {
            let mut ext = [0u8; 2];
            s.read_exact(&mut ext).unwrap();
            len = u16::from_be_bytes(ext) as usize;
        } else if len == 127 {
            let mut ext = [0u8; 8];
            s.read_exact(&mut ext).unwrap();
            len = u64::from_be_bytes(ext) as usize;
        }
        let mut data = vec![0u8; len];
        s.read_exact(&mut data).unwrap();
        (opcode, data)
    }

    #[test]
    fn ws_bridge_streams_exact_samples_and_publishes_markers() {
        let session = format!("ws-test-{}", std::process::id());
        let mut info = StreamInfo::new("WsEeg", "EEG", 8, Format::Float32, 512.0);
        info.session_id = session.clone();
        info.source_id = "ws-eeg-01".to_string();
        let outlet = Outlet::new(info).expect("outlet");

        let mut bridge = WsBridge::start(WsOptions {
            bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            session_id: Some(session.clone()),
            marker_stream_name: "WsMarkers".to_string(),
            ..WsOptions::default()
        })
        .expect("ws bridge");

        let mut ws = ws_client_connect(bridge.local_addr());
        let _ = ws.set_read_timeout(Some(Duration::from_secs(3)));

        // Wait until resolver sees WsEeg, then subscribe.
        assert!(wait_until(Duration::from_secs(3), || {
            ws_client_send_text(&mut ws, r#"{"op":"list"}"#);
            let (op, body) = ws_client_read_frame(&mut ws);
            op == 1 && String::from_utf8_lossy(&body).contains("WsEeg")
        }));

        ws_client_send_text(&mut ws, r#"{"op":"subscribe","stream":"WsEeg"}"#);
        let (op, ack) = ws_client_read_frame(&mut ws);
        assert_eq!(op, 1);
        let ack_text = String::from_utf8(ack).unwrap();
        assert!(ack_text.contains("\"op\":\"subscribed\""), "ack: {ack_text}");

        // Also open a native inlet to verify bit-exact equality.
        let mut native =
            Inlet::open_with_buffer(outlet.info(), Duration::from_secs(2), 4096).unwrap();
        assert!(wait_until(Duration::from_secs(2), || outlet.consumer_count() >= 2));

        let sample_vals: Vec<f32> = (0..8).map(|c| c as f32 * 1.25 - 3.5).collect();
        outlet.push_chunk_f32(&sample_vals, 42.125);

        let mut nat_vals = Vec::new();
        let mut nat_ts = Vec::new();
        assert!(wait_until(Duration::from_secs(2), || {
            let _ = native.pull_chunk_f32(&mut nat_vals, &mut nat_ts, 1, Duration::from_millis(50));
            !nat_ts.is_empty()
        }));

        let (bin_op, bin) = ws_client_read_frame(&mut ws);
        assert_eq!(bin_op, 2, "expected binary sample frame");
        let stream_id = u32::from_le_bytes(bin[0..4].try_into().unwrap());
        let count = u32::from_le_bytes(bin[4..8].try_into().unwrap());
        assert_eq!(stream_id, 1);
        assert_eq!(count, 1);

        let codec = Codec::new(Format::Float32, 8, ByteOrder::Little);
        let (decoded, _) = codec.decode(&bin[8..]).unwrap();
        assert_eq!(decoded.timestamp.to_bits(), nat_ts[0].to_bits());
        for (c, v) in decoded.values.iter().enumerate() {
            match v {
                Value::F32(x) => assert_eq!(x.to_bits(), nat_vals[c].to_bits()),
                other => panic!("unexpected {other:?}"),
            }
        }

        // Send a browser marker and verify it arrives on an LSL marker inlet.
        ws_client_send_text(
            &mut ws,
            r#"{"op":"marker","stream":"WsMarkers","value":"stimulus_on","timestamp":99.5}"#,
        );
        let (op, m_ack) = ws_client_read_frame(&mut ws);
        assert_eq!(op, 1);
        assert!(String::from_utf8_lossy(&m_ack).contains("marker_ok"));

        let found = resolve(
            &format!("name='WsMarkers' and session_id='{session}'"),
            1,
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(found.len(), 1);
        let mut m_inlet = Inlet::open(&found[0], Duration::from_secs(2)).unwrap();
        // Push a second marker now that the inlet is attached.
        ws_client_send_text(
            &mut ws,
            r#"{"op":"marker","stream":"WsMarkers","value":"response_1","timestamp":100.25}"#,
        );
        let _ = ws_client_read_frame(&mut ws);
        let marker_sample = m_inlet
            .pull(Duration::from_secs(2))
            .unwrap()
            .expect("marker sample");
        assert_eq!(marker_sample.timestamp.to_bits(), 100.25f64.to_bits());
        assert_eq!(
            marker_sample.values[0],
            Value::Str(b"response_1".to_vec())
        );

        bridge.stop();
    }
}
