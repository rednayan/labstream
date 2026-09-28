//! The native implementation under test.
//!
//! This program implements the same command line as `oracle/peer.cpp`, which
//! wraps real liblsl. The interop harness drives both and knows neither.
//!
//! ```text
//! lsl-peer outlet --name N --format F --channels C --srate S --input FILE
//! lsl-peer inlet  --name N --format F --channels C --count K --output FILE
//! ```

use labstream_net::{build_property_query, resolve, Inlet, Outlet, StreamInfo};
use labstream_wire::{Format, Sample, Value};
use std::io::{BufRead, Write};
use std::time::Duration;

fn die(msg: &str) -> ! {
    eprintln!("ERROR {msg}");
    std::process::exit(1);
}

struct Args {
    mode: String,
    name: String,
    format: Format,
    channels: usize,
    srate: f64,
    input: String,
    output: String,
    count: usize,
    timeout: f64,
    postproc: u32,
    buflen: i32,
    pull_delay_ms: u64,
    push_delay_ms: u64,
    source_id: String,
    stream_type: String,
    recover: bool,
    chunk: usize,
    desc: bool,
    desc_output: String,
    zero_stamps: bool,
    sync: bool,
}

fn parse_args() -> Args {
    let raw: Vec<String> = std::env::args().collect();
    if raw.len() < 2 {
        die("no mode");
    }
    let mut a = Args {
        mode: raw[1].clone(),
        name: String::new(),
        format: Format::Float32,
        channels: 1,
        srate: 100.0,
        input: String::new(),
        output: String::new(),
        count: 0,
        timeout: 20.0,
        postproc: 0,
        buflen: 3600,
        pull_delay_ms: 0,
        push_delay_ms: 4,
        source_id: String::new(),
        stream_type: "Interop".to_string(),
        recover: false,
        chunk: 0,
        desc: false,
        desc_output: String::new(),
        zero_stamps: false,
        sync: false,
    };
    let mut i = 2;
    while i + 1 < raw.len() {
        let (k, v) = (raw[i].as_str(), raw[i + 1].as_str());
        match k {
            "--name" => a.name = v.to_string(),
            "--format" => {
                a.format = labstream_net::info::format_from_name(v)
                    .unwrap_or_else(|| die(&format!("unknown format {v}")))
            }
            "--channels" => a.channels = v.parse().unwrap_or_else(|_| die("bad channels")),
            "--srate" => a.srate = v.parse().unwrap_or_else(|_| die("bad srate")),
            "--input" => a.input = v.to_string(),
            "--output" => a.output = v.to_string(),
            "--count" => a.count = v.parse().unwrap_or_else(|_| die("bad count")),
            "--timeout" => a.timeout = v.parse().unwrap_or_else(|_| die("bad timeout")),
            // The harness turns the stages on to compare a corrected timestamp.
            "--postproc" => a.postproc = v.parse().unwrap_or_else(|_| die("bad postproc")),
            "--buflen" => a.buflen = v.parse().unwrap_or_else(|_| die("bad buflen")),
            "--pull-delay-ms" => a.pull_delay_ms = v.parse().unwrap_or_else(|_| die("bad delay")),
            "--push-delay-ms" => a.push_delay_ms = v.parse().unwrap_or_else(|_| die("bad delay")),
            "--source-id" => a.source_id = v.to_string(),
            "--type" => a.stream_type = v.to_string(),
            // Accepted and ignored, so the harness can pass one command line to
            // either implementation.
            "--recover" => a.recover = v == "1",
            "--chunk" => a.chunk = v.parse().unwrap_or_else(|_| die("bad chunk")),
            // Push every sample with a timestamp of zero, which means the
            // current clock. SPEC.md 8.3.
            "--zero-stamps" => a.zero_stamps = v == "1",
            // Write each sample before the push returns, with no queue.
            "--sync" => a.sync = v == "1",
            "--desc" => a.desc = v == "1",
            "--desc-output" => a.desc_output = v.to_string(),
            // Accepted and ignored, so one command line drives either peer.
            "--chunklen" | "--consumers" | "--pushthrough" => {}
            other => die(&format!("unknown option {other}")),
        }
        i += 2;
    }
    a
}

// === the sample file ===
//
// One line per sample: a timestamp as the bits of an f64, then one token per
// channel. A numeric token is a bit pattern. A string token is hex bytes, and
// `-` stands for an empty value, because an empty field would vanish when the
// reader splits a line.

fn unhex_u64(s: &str) -> u64 {
    u64::from_str_radix(s, 16).unwrap_or_else(|_| die(&format!("bad hex {s}")))
}

fn unhex_bytes(s: &str) -> Vec<u8> {
    if s == "-" {
        return Vec::new();
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap_or_else(|_| die("bad hex bytes")))
        .collect()
}

fn hex_bytes(b: &[u8]) -> String {
    if b.is_empty() {
        return "-".to_string();
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn value_from_token(format: Format, tok: &str) -> Value {
    match format {
        Format::Float32 => Value::F32(f32::from_bits(unhex_u64(tok) as u32)),
        Format::Double64 => Value::F64(f64::from_bits(unhex_u64(tok))),
        Format::Int32 => Value::I32(unhex_u64(tok) as i32),
        Format::Int16 => Value::I16(unhex_u64(tok) as i16),
        Format::Int8 => Value::I8(unhex_u64(tok) as i8),
        Format::Int64 => Value::I64(unhex_u64(tok) as i64),
        Format::String => Value::Str(unhex_bytes(tok)),
        Format::Undefined => die("undefined format"),
    }
}

fn token_from_value(v: &Value) -> String {
    match v {
        Value::F32(x) => format!("{:x}", x.to_bits()),
        Value::F64(x) => format!("{:x}", x.to_bits()),
        Value::I32(x) => format!("{:x}", *x as i64 as u64),
        Value::I16(x) => format!("{:x}", *x as i64 as u64),
        Value::I8(x) => format!("{:x}", *x as i64 as u64),
        Value::I64(x) => format!("{:x}", *x as u64),
        Value::Str(b) => hex_bytes(b),
    }
}

fn read_samples(path: &str, format: Format) -> Vec<Sample> {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| die(&format!("cannot read: {e}")));
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let mut it = l.split_whitespace();
            let t = f64::from_bits(unhex_u64(it.next().unwrap_or_else(|| die("no timestamp"))));
            Sample {
                timestamp: t,
                values: it.map(|tok| value_from_token(format, tok)).collect(),
            }
        })
        .collect()
}

// === the description tree ===

/// The tree that the harness expects. `oracle/peer.cpp` builds the same one.
///
/// `blank` and `hollow` are here on purpose. A tag with an empty text child
/// writes as `<blank></blank>` and a tag with no child at all writes as
/// `<hollow />`. An implementation that keeps only a text field cannot tell
/// them apart.
fn build_desc(info: &mut StreamInfo, channels: usize) {
    {
        let chns = info.desc.append_child("channels");
        for i in 0..channels {
            let c = chns.append_child("channel");
            c.append_child_value("label", &format!("Ch{i}"));
            c.append_child_value("unit", "microvolts");
            c.append_child_value("type", "EEG");
        }
    }
    info.desc
        .append_child_value("manufacturer", "Acme & Co <test>");
    info.desc.append_child_value("blank", "");
    info.desc.append_child("hollow");
}

/// Write the `<desc>` part of a document, at the depth it sits at.
fn desc_block(info: &StreamInfo) -> String {
    let mut node = info.desc.clone();
    node.name = "desc".to_string();
    let mut out = String::new();
    node.write(&mut out, 1);
    out
}

// === outlet ===

fn run_outlet(a: &Args) {
    let samples = read_samples(&a.input, a.format);

    let mut info = StreamInfo::new(
        &a.name,
        &a.stream_type,
        a.channels as u32,
        a.format,
        a.srate,
    );
    info.source_id = if a.source_id.is_empty() {
        format!("{}_src", a.name)
    } else {
        a.source_id.clone()
    };
    if a.desc {
        build_desc(&mut info, a.channels);
    }
    let outlet = if a.sync {
        Outlet::new_blocking(info)
    } else {
        Outlet::new(info)
    }
    .unwrap_or_else(|e| die(&format!("cannot publish: {e}")));

    eprintln!("READY {}", outlet.info().uid);

    // Wait for the harness to say that the inlet is connected.
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line).is_err() {
        die("no start signal");
    }

    if !outlet.wait_for_consumers(Duration::from_secs_f64(a.timeout)) {
        die("no consumer");
    }
    std::thread::sleep(Duration::from_millis(200));

    if a.chunk > 0 {
        // The timestamp names the last sample of the chunk. SPEC.md 9.2.
        for block in samples.chunks(a.chunk) {
            let stamp = block[block.len() - 1].timestamp;
            outlet.push_chunk(block, stamp);
            if a.push_delay_ms > 0 {
                std::thread::sleep(Duration::from_millis(a.push_delay_ms));
            }
        }
    } else {
        for s in &samples {
            let s = &Sample {
                timestamp: if a.zero_stamps { 0.0 } else { s.timestamp },
                values: s.values.clone(),
            };
            outlet.push(s);
            if a.push_delay_ms > 0 {
                std::thread::sleep(Duration::from_millis(a.push_delay_ms));
            }
        }
    }

    eprintln!("PUSHED {}", samples.len());
    std::thread::sleep(Duration::from_secs(30));
}

// === inlet ===

fn run_inlet(a: &Args) {
    let query = build_property_query(&labstream_net::config::get().session_id, "name", &a.name);
    let found = resolve(&query, 1, Duration::from_secs_f64(a.timeout))
        .unwrap_or_else(|e| die(&format!("resolve failed: {e}")));
    let info = match found.into_iter().find(|i| i.name == a.name) {
        Some(i) => i,
        None => die(&format!("could not resolve {}", a.name)),
    };

    // Convert the request the way the C boundary of liblsl does. SPEC.md 8.6.
    let buffer_samples = info.buffer_samples(a.buflen, labstream_net::info::transport::DEFAULT);
    let open = if a.recover {
        Inlet::open_recovering
    } else {
        Inlet::open_with_buffer
    };
    let mut inlet = open(&info, Duration::from_secs_f64(a.timeout), buffer_samples)
        .unwrap_or_else(|e| die(&format!("cannot open: {e}")));
    if a.postproc != 0 {
        inlet.set_postprocessing(a.postproc);
        // A corrected timestamp needs a measurement first. Without one the
        // clock sync stage adds zero and the comparison sees raw values.
        inlet.wait_for_time_correction(Duration::from_secs(6));
    }

    // The tree does not travel with a discovery answer. Ask the data port.
    if a.desc && !a.desc_output.is_empty() {
        match inlet.fullinfo(Duration::from_secs_f64(a.timeout)) {
            Ok(full) => {
                let _ = std::fs::write(&a.desc_output, desc_block(&full));
            }
            Err(e) => die(&format!("cannot read the description: {e}")),
        }
    }

    eprintln!("READY {}", info.uid);

    let mut out =
        std::fs::File::create(&a.output).unwrap_or_else(|e| die(&format!("cannot write: {e}")));

    for _ in 0..a.count {
        match inlet.pull(Duration::from_secs_f64(a.timeout)) {
            Ok(Some(s)) => {
                let toks: Vec<String> = s.values.iter().map(token_from_value).collect();
                let line = format!("{:x} {}\n", s.timestamp.to_bits(), toks.join(" "));
                if out.write_all(line.as_bytes()).is_err() {
                    die("cannot write a sample");
                }
            }
            Ok(None) => break,
            Err(e) => die(&format!("pull failed: {e}")),
        }
        if a.pull_delay_ms > 0 {
            std::thread::sleep(Duration::from_millis(a.pull_delay_ms));
        }
    }
    let _ = out.flush();
    eprintln!("DONE");
}

fn main() {
    let a = parse_args();
    match a.mode.as_str() {
        "outlet" => run_outlet(&a),
        "inlet" => run_inlet(&a),
        other => die(&format!("unknown mode {other}")),
    }
}
