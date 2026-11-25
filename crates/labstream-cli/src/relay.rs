//! `labstream relay`: forward local LSL streams (such as liblsl device
//! drivers) through labstream outlets carrying `seq/1`, `resume/1`, `dgram/1`,
//! and optional disk store-and-forward (`docs/roadmap.md`, step 4).

use labstream_net::relay::{is_relayed, RelayOptions, StreamRelay, DEFAULT_SUFFIX};
use labstream_net::ws::{WsBridge, WsOptions};
use labstream_net::{config, ContinuousResolver, StreamInfo};
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

/// Parsed options for `labstream relay`.
#[derive(Debug, Clone)]
pub struct RelayArgs {
    pub names: Vec<String>,
    pub types: Vec<String>,
    pub query: Option<String>,
    pub name_suffix: String,
    pub session_id: Option<String>,
    pub out_session_id: Option<String>,
    pub disk_mib: Option<f64>,
    pub disk_dir: Option<PathBuf>,
    pub ram_mib: Option<f64>,
    pub local_only: bool,
    pub duration: Option<Duration>,
    /// Opt-in WebSocket bind address (off by default, `docs/roadmap.md`, step 13).
    pub ws_bind: Option<SocketAddr>,
    /// Additional peer IP addresses allowed to connect to the WebSocket endpoint.
    pub ws_allow: Vec<IpAddr>,
    /// Allow any IP address to connect to the WebSocket endpoint (`--ws-allow *`).
    pub ws_allow_any: bool,
    /// Default marker stream name for markers sent from browsers.
    pub ws_marker_stream: String,
    pub help: bool,
}

impl Default for RelayArgs {
    fn default() -> Self {
        RelayArgs {
            names: Vec::new(),
            types: Vec::new(),
            query: None,
            name_suffix: DEFAULT_SUFFIX.to_string(),
            session_id: None,
            out_session_id: None,
            disk_mib: None,
            disk_dir: None,
            ram_mib: None,
            local_only: false,
            duration: None,
            ws_bind: None,
            ws_allow: Vec::new(),
            ws_allow_any: false,
            ws_marker_stream: "BrowserMarkers".to_string(),
            help: false,
        }
    }
}

pub fn relay_usage() -> &'static str {
    "Usage:
  labstream relay [OPTIONS]

Options:
  --name NAME          Relay only streams matching NAME. Repeat or comma-separate.
  --type TYPE          Relay only streams matching TYPE. Repeat or comma-separate.
  --query PRED         Custom LSL discovery predicate (default: all in session).
  --name-suffix TEXT   Suffix added to relayed stream names (default: -relay).
  --session ID         Source session identifier to discover (default: active).
  --out-session ID     Session identifier for relayed outlets (default: source).
  --disk-mb MIB        Enable disk store-and-forward up to MIB MiB (default: 0).
  --disk-dir DIR       Directory for disk spool files (default: temp dir).
  --ram-mb MIB         RAM limit for resume history in MiB (default: 64).
  --local-only         Relay only streams hosted on this machine.
  --ws-port PORT       Enable WebSocket bridge on 127.0.0.1:PORT (default: off).
  --ws-bind ADDR       Enable WebSocket bridge on ADDR (for example 127.0.0.1:8765).
  --ws-allow IP        Allow non-loopback peer IP (repeat, comma-separate, or *).
  --ws-marker-stream N Default LSL marker stream for browser markers (BrowserMarkers).
  --duration SECONDS   Stop after SECONDS seconds (default: run until stopped).
  -h, --help           Show this help text."
}

pub fn parse_relay_args(args: &[String]) -> Result<RelayArgs, String> {
    let mut out = RelayArgs::default();
    let mut i = 0usize;
    while i < args.len() {
        let arg = args[i].as_str();
        let next_val = |idx: &mut usize, flag: &str| -> Result<String, String> {
            *idx += 1;
            args.get(*idx)
                .cloned()
                .ok_or_else(|| format!("missing value after {flag}"))
        };
        let split_into = |val: &str, dst: &mut Vec<String>| {
            for item in val.split(',') {
                let t = item.trim();
                if !t.is_empty() {
                    dst.push(t.to_string());
                }
            }
        };
        match arg {
            "-h" | "--help" => {
                out.help = true;
                return Ok(out);
            }
            "--local-only" => out.local_only = true,
            "--name" => {
                let v = next_val(&mut i, "--name")?;
                split_into(&v, &mut out.names);
            }
            "--type" => {
                let v = next_val(&mut i, "--type")?;
                split_into(&v, &mut out.types);
            }
            "--query" => out.query = Some(next_val(&mut i, "--query")?),
            "--name-suffix" => out.name_suffix = next_val(&mut i, "--name-suffix")?,
            "--session" => out.session_id = Some(next_val(&mut i, "--session")?),
            "--out-session" => out.out_session_id = Some(next_val(&mut i, "--out-session")?),
            "--disk-mb" => {
                let v = next_val(&mut i, "--disk-mb")?;
                let mb: f64 = v
                    .parse()
                    .map_err(|_| format!("invalid --disk-mb value: {v}"))?;
                if !mb.is_finite() || mb < 0.0 {
                    return Err(format!("--disk-mb must be zero or positive: {v}"));
                }
                out.disk_mib = Some(mb);
            }
            "--disk-dir" => out.disk_dir = Some(PathBuf::from(next_val(&mut i, "--disk-dir")?)),
            "--ram-mb" => {
                let v = next_val(&mut i, "--ram-mb")?;
                let mb: f64 = v
                    .parse()
                    .map_err(|_| format!("invalid --ram-mb value: {v}"))?;
                if !mb.is_finite() || mb <= 0.0 {
                    return Err(format!("--ram-mb must be positive: {v}"));
                }
                out.ram_mib = Some(mb);
            }
            "--duration" => {
                let v = next_val(&mut i, "--duration")?;
                let secs: f64 = v
                    .parse()
                    .map_err(|_| format!("invalid --duration value: {v}"))?;
                if !secs.is_finite() || secs <= 0.0 {
                    return Err(format!("--duration must be positive: {v}"));
                }
                out.duration = Some(Duration::from_secs_f64(secs));
            }
            "--ws-port" => {
                let v = next_val(&mut i, "--ws-port")?;
                let port: u16 = v
                    .parse()
                    .map_err(|_| format!("invalid --ws-port value: {v}"))?;
                out.ws_bind = Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port));
            }
            "--ws-bind" => {
                let v = next_val(&mut i, "--ws-bind")?;
                let addr: SocketAddr = v
                    .parse()
                    .map_err(|_| format!("invalid --ws-bind address: {v}"))?;
                out.ws_bind = Some(addr);
            }
            "--ws-allow" => {
                let v = next_val(&mut i, "--ws-allow")?;
                parse_ws_allow(&v, &mut out)?;
            }
            "--ws-marker-stream" => {
                out.ws_marker_stream = next_val(&mut i, "--ws-marker-stream")?;
            }
            other => {
                if let Some(v) = other.strip_prefix("--name=") {
                    split_into(v, &mut out.names);
                } else if let Some(v) = other.strip_prefix("--type=") {
                    split_into(v, &mut out.types);
                } else if let Some(v) = other.strip_prefix("--query=") {
                    out.query = Some(v.to_string());
                } else if let Some(v) = other.strip_prefix("--name-suffix=") {
                    out.name_suffix = v.to_string();
                } else if let Some(v) = other.strip_prefix("--session=") {
                    out.session_id = Some(v.to_string());
                } else if let Some(v) = other.strip_prefix("--out-session=") {
                    out.out_session_id = Some(v.to_string());
                } else if let Some(v) = other.strip_prefix("--disk-mb=") {
                    let mb: f64 = v
                        .parse()
                        .map_err(|_| format!("invalid --disk-mb value: {v}"))?;
                    if !mb.is_finite() || mb < 0.0 {
                        return Err(format!("--disk-mb must be zero or positive: {v}"));
                    }
                    out.disk_mib = Some(mb);
                } else if let Some(v) = other.strip_prefix("--disk-dir=") {
                    out.disk_dir = Some(PathBuf::from(v));
                } else if let Some(v) = other.strip_prefix("--ram-mb=") {
                    let mb: f64 = v
                        .parse()
                        .map_err(|_| format!("invalid --ram-mb value: {v}"))?;
                    if !mb.is_finite() || mb <= 0.0 {
                        return Err(format!("--ram-mb must be positive: {v}"));
                    }
                    out.ram_mib = Some(mb);
                } else if let Some(v) = other.strip_prefix("--duration=") {
                    let secs: f64 = v
                        .parse()
                        .map_err(|_| format!("invalid --duration value: {v}"))?;
                    if !secs.is_finite() || secs <= 0.0 {
                        return Err(format!("--duration must be positive: {v}"));
                    }
                    out.duration = Some(Duration::from_secs_f64(secs));
                } else if let Some(v) = other.strip_prefix("--ws-port=") {
                    let port: u16 = v
                        .parse()
                        .map_err(|_| format!("invalid --ws-port value: {v}"))?;
                    out.ws_bind = Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port));
                } else if let Some(v) = other.strip_prefix("--ws-bind=") {
                    let addr: SocketAddr = v
                        .parse()
                        .map_err(|_| format!("invalid --ws-bind address: {v}"))?;
                    out.ws_bind = Some(addr);
                } else if let Some(v) = other.strip_prefix("--ws-allow=") {
                    parse_ws_allow(v, &mut out)?;
                } else if let Some(v) = other.strip_prefix("--ws-marker-stream=") {
                    out.ws_marker_stream = v.to_string();
                } else {
                    return Err(format!("unknown argument: {other}"));
                }
            }
        }
        i += 1;
    }
    Ok(out)
}

fn parse_ws_allow(val: &str, out: &mut RelayArgs) -> Result<(), String> {
    for item in val.split(',') {
        let t = item.trim();
        if t.is_empty() {
            continue;
        }
        if t == "*" {
            out.ws_allow_any = true;
        } else {
            let ip: IpAddr = t
                .parse()
                .map_err(|_| format!("invalid --ws-allow IP address: {t}"))?;
            out.ws_allow.push(ip);
        }
    }
    Ok(())
}

/// Discovery-driven manager that attaches [`StreamRelay`] instances to matching
/// streams while ignoring its own and other relayed streams.
pub struct RelayService {
    args: RelayArgs,
    resolver: ContinuousResolver,
    local_hostname: String,
    active: HashMap<String, StreamRelay>,
    own_uids: HashSet<String>,
}

impl RelayService {
    pub fn new(args: RelayArgs) -> Self {
        let session = args
            .session_id
            .clone()
            .unwrap_or_else(|| config::get().session_id.clone());
        let query = match &args.query {
            Some(q) => q.clone(),
            None => format!("session_id='{session}'"),
        };
        let resolver = ContinuousResolver::new(&query, 5.0);
        RelayService {
            args,
            resolver,
            local_hostname: labstream_net::info::hostname(),
            active: HashMap::new(),
            own_uids: HashSet::new(),
        }
    }

    fn matches(&self, info: &StreamInfo) -> bool {
        if self.own_uids.contains(&info.uid) || is_relayed(info) {
            return false;
        }
        if self.args.local_only && info.hostname != self.local_hostname {
            return false;
        }
        if !self.args.names.is_empty() && !self.args.names.iter().any(|n| n == &info.name) {
            return false;
        }
        if !self.args.types.is_empty() && !self.args.types.iter().any(|t| t == &info.stream_type) {
            return false;
        }
        true
    }

    fn build_options(&self) -> RelayOptions {
        let mut limits = config::get().history_limits();
        if let Some(mb) = self.args.ram_mib {
            limits.bytes = (mb * (1u64 << 20) as f64) as usize;
        }
        if let Some(mb) = self.args.disk_mib {
            limits.disk_bytes = (mb * (1u64 << 20) as f64) as u64;
            if limits.disk_bytes > 0 {
                limits.seconds = f64::INFINITY;
            }
        }
        if let Some(dir) = &self.args.disk_dir {
            limits.disk_dir = Some(dir.clone());
        }
        let history_limits = if self.args.ram_mib.is_some()
            || self.args.disk_mib.is_some()
            || self.args.disk_dir.is_some()
        {
            Some(limits)
        } else {
            None
        };
        RelayOptions {
            name_suffix: self.args.name_suffix.clone(),
            session_id: self.args.out_session_id.clone(),
            history_limits,
            ..RelayOptions::default()
        }
    }

    /// Poll discovery, attach new matching streams, and drop finished relays.
    /// Returns descriptions of newly attached relays.
    pub fn poll(&mut self) -> Vec<(String, StreamInfo)> {
        self.active.retain(|_, r| r.is_running());
        let found = self.resolver.results(usize::MAX);
        let mut started = Vec::new();
        for info in found {
            if self.active.contains_key(&info.uid) || !self.matches(&info) {
                continue;
            }
            if let Ok(relay) = StreamRelay::start(&info, self.build_options()) {
                let published = relay.info().clone();
                let src_name = info.name.clone();
                self.own_uids.insert(published.uid.clone());
                self.active.insert(info.uid.clone(), relay);
                started.push((src_name, published));
            }
        }
        started
    }

    #[cfg(test)]
    pub fn active_count(&self) -> usize {
        self.active.len()
    }

    #[cfg(test)]
    pub fn total_forwarded(&self) -> u64 {
        self.active.values().map(StreamRelay::forwarded).sum()
    }
}

pub fn run_relay(args: &[String]) -> ExitCode {
    let parsed = match parse_relay_args(args) {
        Ok(a) => a,
        Err(err) => {
            eprintln!("error: {err}\n\n{}", relay_usage());
            return ExitCode::from(2);
        }
    };
    if parsed.help {
        println!("{}", relay_usage());
        return ExitCode::SUCCESS;
    }

    let deadline = parsed.duration.map(|d| Instant::now() + d);
    let _ws_bridge = if let Some(bind) = parsed.ws_bind {
        let ws_opts = WsOptions {
            bind,
            allowed_peers: parsed.ws_allow.clone(),
            allow_any: parsed.ws_allow_any,
            session_id: parsed
                .out_session_id
                .clone()
                .or_else(|| parsed.session_id.clone()),
            marker_stream_name: parsed.ws_marker_stream.clone(),
        };
        match WsBridge::start(ws_opts) {
            Ok(b) => {
                eprintln!(
                    "labstream relay: WebSocket bridge listening on ws://{}/ws",
                    b.local_addr()
                );
                Some(b)
            }
            Err(e) => {
                eprintln!("error: failed to start WebSocket bridge on {bind}: {e}");
                return ExitCode::from(1);
            }
        }
    } else {
        None
    };
    let mut service = RelayService::new(parsed);
    eprintln!("labstream relay: watching for matching LSL streams...");

    loop {
        for (src_name, published) in service.poll() {
            println!(
                "relaying '{}' -> '{}' (type={}, ch={}, rate={} Hz, source_id={})",
                src_name,
                published.name,
                published.stream_type,
                published.channel_count,
                published.nominal_srate,
                published.source_id,
            );
        }
        if let Some(end) = deadline {
            if Instant::now() >= end {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use labstream_net::{Inlet, Outlet};
    use labstream_wire::Format;

    #[test]
    fn parse_relay_accepts_flags_and_rejects_invalid_values() {
        let args: Vec<String> = [
            "--name",
            "BioSemi,Markers",
            "--type=EEG",
            "--name-suffix",
            "-fwd",
            "--disk-mb",
            "256",
            "--ram-mb=32",
            "--local-only",
            "--duration",
            "5",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let parsed = parse_relay_args(&args).unwrap();
        assert_eq!(parsed.names, vec!["BioSemi", "Markers"]);
        assert_eq!(parsed.types, vec!["EEG"]);
        assert_eq!(parsed.name_suffix, "-fwd");
        assert_eq!(parsed.disk_mib, Some(256.0));
        assert_eq!(parsed.ram_mib, Some(32.0));
        assert!(parsed.local_only);
        assert_eq!(parsed.duration, Some(Duration::from_secs(5)));
        assert!(parsed.ws_bind.is_none(), "WebSocket bridge must be off by default");

        let ws_args: Vec<String> = [
            "--ws-port",
            "9876",
            "--ws-allow",
            "192.168.1.50",
            "--ws-marker-stream",
            "WebEvents",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let ws_parsed = parse_relay_args(&ws_args).unwrap();
        assert_eq!(
            ws_parsed.ws_bind,
            Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9876))
        );
        assert_eq!(
            ws_parsed.ws_allow,
            vec![IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50))]
        );
        assert_eq!(ws_parsed.ws_marker_stream, "WebEvents");

        assert!(parse_relay_args(&["--disk-mb".to_string(), "-1".to_string()]).is_err());
        assert!(parse_relay_args(&["--ram-mb".to_string(), "0".to_string()]).is_err());
        assert!(parse_relay_args(&["--ws-allow".to_string(), "not-an-ip".to_string()]).is_err());
        assert!(parse_relay_args(&["--bogus".to_string()]).is_err());
    }

    #[test]
    fn relay_service_discovers_forwards_and_never_relays_its_own_output() {
        let session = format!("relay-cli-{}", std::process::id());
        let mut info = StreamInfo::new("CliRelaySrc", "EEG", 2, Format::Float32, 100.0);
        info.session_id = session.clone();
        info.source_id = "cli-relay-src".to_string();
        let outlet = Outlet::new(info).expect("source outlet");

        let args = RelayArgs {
            names: vec!["CliRelaySrc".to_string()],
            session_id: Some(session.clone()),
            out_session_id: Some(session.clone()),
            ..RelayArgs::default()
        };
        let mut service = RelayService::new(args);
        let mut relayed_info = None;
        let deadline = Instant::now() + Duration::from_secs(4);
        while relayed_info.is_none() && Instant::now() < deadline {
            let started = service.poll();
            if let Some((_, pub_info)) = started.into_iter().next() {
                relayed_info = Some(pub_info);
            } else {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        let pub_info = relayed_info.expect("relay attached");
        assert_eq!(pub_info.name, "CliRelaySrc-relay");
        assert!(is_relayed(&pub_info));

        // Poll again: the service must not attach a second relay to its own outlet.
        std::thread::sleep(Duration::from_millis(200));
        assert!(service.poll().is_empty());
        assert_eq!(service.active_count(), 1);

        let mut inlet = Inlet::open_with_buffer(&pub_info, Duration::from_secs(2), 1024).unwrap();
        let wait_c = Instant::now() + Duration::from_secs(2);
        while outlet.consumer_count() == 0 && Instant::now() < wait_c {
            std::thread::sleep(Duration::from_millis(10));
        }

        outlet.push_chunk_f32(&[1.5, -2.5, 3.25, 4.75], 50.0);
        let mut got = Vec::new();
        let mut ts = Vec::new();
        let read_end = Instant::now() + Duration::from_secs(2);
        while ts.len() < 2 && Instant::now() < read_end {
            let rem = 2 - ts.len();
            let _ = inlet
                .pull_chunk_f32(&mut got, &mut ts, rem, Duration::from_millis(100))
                .unwrap();
        }
        assert_eq!(got, vec![1.5, -2.5, 3.25, 4.75]);
        assert!((ts[1] - 50.0).abs() < 1e-9);
        assert!(service.total_forwarded() >= 2);
    }
}
