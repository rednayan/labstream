//! Network diagnostics for LSL discovery (`labstream doctor`).
//!
//! `docs/discovery.md` gives each check and the cause that it names. Both the
//! `labstream` CLI and Rust or C applications such as Phasic can run
//! [`diagnose`] and read the structured [`Report`].

use crate::config::{self, FallbackDiscovery};
use crate::info::StreamInfo;
use crate::inlet::{probe_peers_tcp, QueryWave};
use crate::netif::{self, Ipv4Interface};
use labstream_proto::discovery;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

/// Options for [`diagnose`].
#[derive(Debug, Clone)]
pub struct Options {
    /// Optional peer IP addresses or hostnames to probe directly.
    pub peers: Vec<String>,
    /// How long each live discovery phase waits for answers.
    pub timeout: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            peers: Vec::new(),
            timeout: Duration::from_millis(1200),
        }
    }
}

/// Outcome of one diagnostic check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// The check passed.
    Ok,
    /// The check found a degraded path or a warning.
    Warn,
    /// The check failed.
    Fail,
}

impl Status {
    /// Short lowercase label for text and JSON output.
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Warn => "warn",
            Status::Fail => "fail",
        }
    }
}

/// One diagnostic check and its cause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    /// Machine-readable check name.
    pub name: String,
    /// Result status.
    pub status: Status,
    /// Short summary of what happened.
    pub summary: String,
    /// Cause of a warning or failure, when known.
    pub cause: Option<String>,
    /// Concrete step to resolve the warning or failure.
    pub fix: Option<String>,
}

/// One stream seen during the live discovery check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundStream {
    /// Stream name.
    pub name: String,
    /// Stream type.
    pub stream_type: String,
    /// Source hostname.
    pub hostname: String,
    /// Resolved IP address of the outlet.
    pub address: String,
    /// TCP data port of the outlet.
    pub data_port: u16,
    /// Which discovery path found this stream.
    pub via: String,
}

/// Full diagnostic report returned by [`diagnose`].
#[derive(Debug, Clone)]
pub struct Report {
    /// Where the active configuration came from.
    pub config_source: String,
    /// Active `lab.SessionID`.
    pub session_id: String,
    /// Active `labstream.Features`.
    pub features: Vec<String>,
    /// Active `labstream.FallbackDiscovery` mode.
    pub fallback_discovery: FallbackDiscovery,
    /// Configured `lab.KnownPeers`.
    pub known_peers: Vec<String>,
    /// Configured `ports.MulticastPort`.
    pub multicast_port: u16,
    /// Configured `ports.BasePort`.
    pub base_port: u16,
    /// Configured `ports.PortRange`.
    pub port_range: u16,
    /// Detected IPv4 interfaces.
    pub interfaces: Vec<Ipv4Interface>,
    /// Streams found during live discovery and peer probes.
    pub streams: Vec<FoundStream>,
    /// Individual check outcomes.
    pub checks: Vec<Check>,
}

impl Report {
    /// True when at least one check has status [`Status::Fail`].
    pub fn has_failures(&self) -> bool {
        self.checks.iter().any(|c| c.status == Status::Fail)
    }

    /// Format the report for terminal output.
    pub fn format_text(&self) -> String {
        let mut out = String::new();
        out.push_str("labstream doctor\n");
        out.push_str(&format!("  config:    {}\n", self.config_source));
        out.push_str(&format!("  session:   {}\n", self.session_id));
        out.push_str(&format!("  features:  {{{}}}\n", self.features.join(", ")));
        out.push_str(&format!(
            "  fallback:  {}\n",
            self.fallback_discovery.as_str()
        ));
        out.push_str(&format!(
            "  ports:     multicast {}, unicast {}..{}\n",
            self.multicast_port,
            self.base_port,
            self.base_port + self.port_range
        ));
        if !self.known_peers.is_empty() {
            out.push_str(&format!("  peers:     {}\n", self.known_peers.join(", ")));
        }
        out.push_str("\nInterfaces:\n");
        if self.interfaces.is_empty() {
            out.push_str("  (none detected)\n");
        } else {
            for iface in &self.interfaces {
                let mut flags = Vec::new();
                if iface.is_up {
                    flags.push("UP");
                }
                if iface.is_loopback {
                    flags.push("LOOPBACK");
                }
                if iface.supports_multicast {
                    flags.push("MULTICAST");
                }
                let bcast = iface
                    .broadcast
                    .map(|b| format!(" bcast {b}"))
                    .unwrap_or_default();
                out.push_str(&format!(
                    "  {:<12} {}/{}{} [{}]\n",
                    iface.name,
                    iface.addr,
                    iface.prefix_len,
                    bcast,
                    flags.join(",")
                ));
            }
        }
        out.push_str("\nChecks:\n");
        for c in &self.checks {
            let tag = match c.status {
                Status::Ok => "OK  ",
                Status::Warn => "WARN",
                Status::Fail => "FAIL",
            };
            out.push_str(&format!("  [{tag}] {}: {}\n", c.name, c.summary));
            if let Some(cause) = &c.cause {
                out.push_str(&format!("         cause: {cause}\n"));
            }
            if let Some(fix) = &c.fix {
                out.push_str(&format!("         fix:   {fix}\n"));
            }
        }
        if !self.streams.is_empty() {
            out.push_str("\nStreams found:\n");
            for s in &self.streams {
                out.push_str(&format!(
                    "  {} ({}) on {} ({}:{}) via {}\n",
                    s.name, s.stream_type, s.hostname, s.address, s.data_port, s.via
                ));
            }
        }
        out
    }

    /// Encode the report as JSON with no external serializer dependency.
    pub fn to_json(&self) -> String {
        let mut s = String::new();
        s.push('{');
        push_kv_str(&mut s, "config_source", &self.config_source, true);
        push_kv_str(&mut s, "session_id", &self.session_id, false);
        s.push_str(",\"features\":[");
        for (i, f) in self.features.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            push_json_str(&mut s, f);
        }
        s.push(']');
        push_kv_str(
            &mut s,
            "fallback_discovery",
            self.fallback_discovery.as_str(),
            false,
        );
        s.push_str(",\"known_peers\":[");
        for (i, p) in self.known_peers.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            push_json_str(&mut s, p);
        }
        s.push(']');
        s.push_str(&format!(
            ",\"multicast_port\":{},\"base_port\":{},\"port_range\":{}",
            self.multicast_port, self.base_port, self.port_range
        ));
        s.push_str(",\"interfaces\":[");
        for (i, iface) in self.interfaces.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push('{');
            push_kv_str(&mut s, "name", &iface.name, true);
            push_kv_str(&mut s, "addr", &iface.addr.to_string(), false);
            s.push_str(&format!(",\"prefix_len\":{}", iface.prefix_len));
            match iface.broadcast {
                Some(b) => push_kv_str(&mut s, "broadcast", &b.to_string(), false),
                None => s.push_str(",\"broadcast\":null"),
            }
            s.push_str(&format!(
                ",\"ifindex\":{},\"is_up\":{},\"is_loopback\":{},\"supports_multicast\":{}}}",
                iface.ifindex, iface.is_up, iface.is_loopback, iface.supports_multicast
            ));
        }
        s.push(']');
        s.push_str(",\"streams\":[");
        for (i, st) in self.streams.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push('{');
            push_kv_str(&mut s, "name", &st.name, true);
            push_kv_str(&mut s, "type", &st.stream_type, false);
            push_kv_str(&mut s, "hostname", &st.hostname, false);
            push_kv_str(&mut s, "address", &st.address, false);
            s.push_str(&format!(",\"data_port\":{}", st.data_port));
            push_kv_str(&mut s, "via", &st.via, false);
            s.push('}');
        }
        s.push(']');
        s.push_str(",\"checks\":[");
        for (i, c) in self.checks.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push('{');
            push_kv_str(&mut s, "name", &c.name, true);
            push_kv_str(&mut s, "status", c.status.as_str(), false);
            push_kv_str(&mut s, "summary", &c.summary, false);
            match &c.cause {
                Some(v) => push_kv_str(&mut s, "cause", v, false),
                None => s.push_str(",\"cause\":null"),
            }
            match &c.fix {
                Some(v) => push_kv_str(&mut s, "fix", v, false),
                None => s.push_str(",\"fix\":null"),
            }
            s.push('}');
        }
        s.push_str("]}");
        s
    }
}

fn push_kv_str(out: &mut String, key: &str, val: &str, first: bool) {
    if !first {
        out.push(',');
    }
    push_json_str(out, key);
    out.push(':');
    push_json_str(out, val);
}

fn push_json_str(out: &mut String, text: &str) {
    out.push('"');
    for ch in text.chars() {
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
    out.push('"');
}

/// Run the full diagnostic suite and return a [`Report`].
pub fn diagnose(options: &Options) -> Report {
    let cfg = config::get();
    let interfaces = netif::local_ipv4_interfaces();
    let mut checks = Vec::new();
    let mut streams = Vec::new();

    // 1. Interfaces check.
    let lan_ifaces: Vec<&Ipv4Interface> = interfaces
        .iter()
        .filter(|i| i.is_up && !i.is_loopback)
        .collect();
    let mcast_lan: Vec<&Ipv4Interface> = lan_ifaces
        .iter()
        .copied()
        .filter(|i| i.supports_multicast)
        .collect();
    if lan_ifaces.is_empty() {
        checks.push(Check {
            name: "interfaces".to_string(),
            status: Status::Warn,
            summary: "no active non-loopback IPv4 interface found".to_string(),
            cause: Some("only the loopback interface is up".to_string()),
            fix: Some("connect to Wi-Fi or Ethernet for multi-machine streams".to_string()),
        });
    } else if mcast_lan.is_empty() {
        checks.push(Check {
            name: "interfaces".to_string(),
            status: Status::Warn,
            summary: format!("{} LAN interface(s), none with multicast", lan_ifaces.len()),
            cause: Some("no active LAN interface has the MULTICAST flag".to_string()),
            fix: Some("use fallback discovery or set lab.KnownPeers".to_string()),
        });
    } else if lan_ifaces.len() > 1 {
        let names: Vec<&str> = lan_ifaces.iter().map(|i| i.name.as_str()).collect();
        checks.push(Check {
            name: "interfaces".to_string(),
            status: Status::Ok,
            summary: format!(
                "{} active LAN interfaces ({})",
                lan_ifaces.len(),
                names.join(", ")
            ),
            cause: None,
            fix: None,
        });
    } else {
        checks.push(Check {
            name: "interfaces".to_string(),
            status: Status::Ok,
            summary: format!(
                "1 active LAN interface ({} {}/{})",
                lan_ifaces[0].name, lan_ifaces[0].addr, lan_ifaces[0].prefix_len
            ),
            cause: None,
            fix: None,
        });
    }

    // 2. Multicast port bind + 3. Multicast self-test + 4. Broadcast self-test.
    let (port_check, mcast_check, bcast_check) = check_local_multicast_and_broadcast(&interfaces);
    checks.push(port_check);
    checks.push(mcast_check);
    checks.push(bcast_check);

    // 5. Live discovery check: compare multicast against fallback.
    let query = format!("session_id='{}'", cfg.session_id);
    let half = (options.timeout / 2).max(Duration::from_millis(400));
    let (mcast_found, fb_found, kp_udp_found, kp_tcp_found) =
        probe_live_discovery(&query, half, &cfg.known_peers);

    for info in &mcast_found {
        push_unique_stream(&mut streams, info, "multicast");
    }
    for info in &fb_found {
        push_unique_stream(&mut streams, info, "fallback");
    }
    for info in &kp_udp_found {
        push_unique_stream(&mut streams, info, "known_peers_udp");
    }
    for info in &kp_tcp_found {
        push_unique_stream(&mut streams, info, "known_peers_tcp");
    }

    let fb_only: Vec<&StreamInfo> = fb_found
        .iter()
        .filter(|s| !mcast_found.iter().any(|m| m.uid == s.uid))
        .collect();
    let tcp_only: Vec<&StreamInfo> = kp_tcp_found
        .iter()
        .filter(|s| {
            !mcast_found.iter().any(|m| m.uid == s.uid)
                && !fb_found.iter().any(|f| f.uid == s.uid)
                && !kp_udp_found.iter().any(|u| u.uid == s.uid)
        })
        .collect();

    if !mcast_found.is_empty() && fb_only.is_empty() && tcp_only.is_empty() {
        checks.push(Check {
            name: "discovery".to_string(),
            status: Status::Ok,
            summary: format!("multicast found {} stream(s)", mcast_found.len()),
            cause: None,
            fix: None,
        });
    } else if !fb_only.is_empty() {
        let total = streams.len();
        checks.push(Check {
            name: "discovery".to_string(),
            status: Status::Warn,
            summary: format!(
                "multicast found {}, fallback found {} more ({} total)",
                mcast_found.len(),
                fb_only.len(),
                total
            ),
            cause: Some("multicast is blocked on this network".to_string()),
            fix: Some(
                "labstream fallback discovery works on /24 networks. For larger subnets or liblsl peers, add the outlet IP to lab.KnownPeers."
                    .to_string(),
            ),
        });
    } else if !tcp_only.is_empty() {
        checks.push(Check {
            name: "discovery".to_string(),
            status: Status::Warn,
            summary: format!(
                "UDP found 0 streams, TCP KnownPeers found {}",
                tcp_only.len()
            ),
            cause: Some("UDP discovery ports are blocked, while TCP works".to_string()),
            fix: Some(
                "open UDP ports 16571-16603 on the firewall, or keep lab.KnownPeers set"
                    .to_string(),
            ),
        });
    } else if !kp_udp_found.is_empty() {
        checks.push(Check {
            name: "discovery".to_string(),
            status: Status::Warn,
            summary: format!(
                "multicast found 0 streams, KnownPeers UDP found {}",
                kp_udp_found.len()
            ),
            cause: Some("multicast is blocked on this network".to_string()),
            fix: Some("keep lab.KnownPeers set or allow multicast on UDP 16571".to_string()),
        });
    } else {
        checks.push(Check {
            name: "discovery".to_string(),
            status: Status::Ok,
            summary: format!("no streams active in session '{}'", cfg.session_id),
            cause: None,
            fix: Some(
                "start an outlet, make sure both sides use the same SessionID, or pass --peer <IP>"
                    .to_string(),
            ),
        });
    }

    // 6. Explicit remote peer checks.
    for peer in &options.peers {
        let (check, peer_streams) = check_peer(peer, &query, half);
        for (info, via) in peer_streams {
            push_unique_stream(&mut streams, &info, &via);
        }
        checks.push(check);
    }

    Report {
        config_source: cfg.source.clone(),
        session_id: cfg.session_id.clone(),
        features: cfg.features.clone(),
        fallback_discovery: cfg.fallback_discovery,
        known_peers: cfg.known_peers.clone(),
        multicast_port: cfg.multicast_port,
        base_port: cfg.base_port,
        port_range: cfg.port_range,
        interfaces,
        streams,
        checks,
    }
}

fn push_unique_stream(list: &mut Vec<FoundStream>, info: &StreamInfo, via: &str) {
    let address = if !info.v4address.is_empty() {
        info.v4address.clone()
    } else {
        info.v6address.clone()
    };
    let data_port = if info.v4data_port != 0 {
        info.v4data_port
    } else {
        info.v6data_port
    };
    if list
        .iter()
        .any(|s| s.name == info.name && s.address == address && s.data_port == data_port)
    {
        return;
    }
    list.push(FoundStream {
        name: info.name.clone(),
        stream_type: info.stream_type.clone(),
        hostname: info.hostname.clone(),
        address,
        data_port,
        via: via.to_string(),
    });
}

fn check_local_multicast_and_broadcast(interfaces: &[Ipv4Interface]) -> (Check, Check, Check) {
    let cfg = config::get();
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), cfg.multicast_port);
    let sock = match bind_shared_udp4(addr) {
        Ok(s) => s,
        Err(e) => {
            let port_check = Check {
                name: "multicast_port".to_string(),
                status: Status::Fail,
                summary: format!("cannot bind UDP port {}: {e}", cfg.multicast_port),
                cause: Some(
                    "another process holds the multicast port without SO_REUSEADDR".to_string(),
                ),
                fix: Some("stop the other program or change ports.MulticastPort".to_string()),
            };
            let skip_mc = Check {
                name: "multicast_self_test".to_string(),
                status: Status::Warn,
                summary: "skipped because the multicast port did not bind".to_string(),
                cause: None,
                fix: None,
            };
            let skip_bc = Check {
                name: "broadcast_self_test".to_string(),
                status: Status::Warn,
                summary: "skipped because the multicast port did not bind".to_string(),
                cause: None,
                fix: None,
            };
            return (port_check, skip_mc, skip_bc);
        }
    };
    let _ = sock.set_broadcast(true);
    let _ = sock.set_read_timeout(Some(Duration::from_millis(120)));

    let group = cfg
        .multicast_addresses
        .iter()
        .filter_map(|a| a.parse::<Ipv4Addr>().ok())
        .find(|a| a.is_multicast() && !a.is_broadcast())
        .unwrap_or(Ipv4Addr::new(224, 0, 0, 183));

    let mut joined = 0usize;
    let mut join_addrs: Vec<Ipv4Addr> = interfaces
        .iter()
        .filter(|i| i.is_up)
        .map(|i| i.addr)
        .collect();
    if !join_addrs.contains(&Ipv4Addr::UNSPECIFIED) {
        join_addrs.push(Ipv4Addr::UNSPECIFIED);
    }
    for iface in &join_addrs {
        if sock.join_multicast_v4(&group, iface).is_ok() {
            joined += 1;
        }
    }

    let port_check = Check {
        name: "multicast_port".to_string(),
        status: Status::Ok,
        summary: format!(
            "UDP port {} bound with reuse ({} group join(s))",
            cfg.multicast_port, joined
        ),
        cause: None,
        fix: None,
    };

    // Self-test multicast packet delivery.
    let token = format!("__doctor_mc_{}__", crate::make_query_id());
    let mc_ok = self_test_udp(&sock, &token, |tx| {
        let _ = tx.set_multicast_loop_v4(true);
        let _ = tx.set_multicast_ttl_v4(cfg.multicast_ttl);
        let _ = tx.send_to(token.as_bytes(), (group, cfg.multicast_port));
    });
    let mcast_check = if mc_ok {
        Check {
            name: "multicast_self_test".to_string(),
            status: Status::Ok,
            summary: format!(
                "local multicast loopback to {group}:{} passed",
                cfg.multicast_port
            ),
            cause: None,
            fix: None,
        }
    } else {
        Check {
            name: "multicast_self_test".to_string(),
            status: Status::Warn,
            summary: format!(
                "no local multicast packet arrived on {group}:{}",
                cfg.multicast_port
            ),
            cause: Some(
                "local multicast route is missing or a local firewall blocks multicast".to_string(),
            ),
            fix: Some(
                "allow UDP 16571 on the local firewall or rely on fallback discovery".to_string(),
            ),
        }
    };

    // Self-test broadcast delivery (limited or directed).
    let btoken = format!("__doctor_bc_{}__", crate::make_query_id());
    let bcasts: Vec<Ipv4Addr> = interfaces.iter().filter_map(|i| i.broadcast).collect();
    let bc_ok = self_test_udp(&sock, &btoken, |tx| {
        let _ = tx.set_broadcast(true);
        let _ = tx.send_to(btoken.as_bytes(), (Ipv4Addr::BROADCAST, cfg.multicast_port));
        for b in &bcasts {
            let _ = tx.send_to(btoken.as_bytes(), (*b, cfg.multicast_port));
        }
    });
    let bcast_check = if bc_ok {
        Check {
            name: "broadcast_self_test".to_string(),
            status: Status::Ok,
            summary: format!("local UDP broadcast to port {} passed", cfg.multicast_port),
            cause: None,
            fix: None,
        }
    } else {
        Check {
            name: "broadcast_self_test".to_string(),
            status: Status::Warn,
            summary: format!(
                "no local broadcast packet arrived on port {}",
                cfg.multicast_port
            ),
            cause: Some("local firewall or OS blocked UDP broadcast".to_string()),
            fix: Some(
                "unicast fallback and lab.KnownPeers still work without broadcast".to_string(),
            ),
        }
    };

    (port_check, mcast_check, bcast_check)
}

fn self_test_udp(rx: &UdpSocket, token: &str, send: impl FnOnce(&UdpSocket)) -> bool {
    let Ok(tx) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) else {
        return false;
    };
    send(&tx);
    let mut buf = [0u8; 512];
    let end = Instant::now() + Duration::from_millis(200);
    while Instant::now() < end {
        match rx.recv_from(&mut buf) {
            Ok((n, _)) if &buf[..n] == token.as_bytes() => return true,
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    false
}

fn bind_shared_udp4(addr: SocketAddr) -> std::io::Result<UdpSocket> {
    let sock = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )?;
    sock.set_reuse_address(true)?;
    sock.bind(&addr.into())?;
    Ok(sock.into())
}

fn probe_live_discovery(
    query: &str,
    window: Duration,
    known_peers: &[String],
) -> (
    Vec<StreamInfo>,
    Vec<StreamInfo>,
    Vec<StreamInfo>,
    Vec<StreamInfo>,
) {
    let Ok(mut wave) = QueryWave::open() else {
        return (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    };
    // Phase A: multicast and 255.255.255.255 only.
    wave.send_multicast_only(query);
    let mcast = wave.collect(window, |_| false);

    // Phase B: fallback (directed broadcast, ARP neighbors, /24 unicast).
    let Ok(mut fb_wave) = QueryWave::open() else {
        return (mcast, Vec::new(), Vec::new(), Vec::new());
    };
    fb_wave.send_fallback(query, 0);
    let fb = fb_wave.collect(window, |_| false);

    // Phase C: KnownPeers UDP and TCP.
    let kp_udp = if known_peers.is_empty() {
        Vec::new()
    } else if let Ok(mut kp_wave) = QueryWave::open() {
        kp_wave.send_peers_only(query, known_peers);
        kp_wave.collect(window, |_| false)
    } else {
        Vec::new()
    };
    let kp_tcp = if known_peers.is_empty() {
        Vec::new()
    } else {
        probe_peers_tcp(known_peers, query, window)
    };
    (mcast, fb, kp_udp, kp_tcp)
}

fn check_peer(peer: &str, query: &str, window: Duration) -> (Check, Vec<(StreamInfo, String)>) {
    let cfg = config::get();
    let peers = vec![peer.to_string()];

    // Path 1: UDP unicast from the receive socket (same port).
    let udp_same = if let Ok(mut w) = QueryWave::open() {
        w.send_peers_only(query, &peers);
        w.collect(window, |_| false)
    } else {
        Vec::new()
    };

    // Path 2: UDP unicast from a separate socket (liblsl behavior).
    let udp_split = probe_peer_split_udp(
        peer,
        query,
        cfg.multicast_port,
        cfg.base_port,
        cfg.port_range,
        window,
    );

    // Path 3: TCP LSL:shortinfo.
    let tcp = probe_peers_tcp(&peers, query, window);

    let mut found = Vec::new();
    for s in &udp_same {
        found.push((s.clone(), format!("peer_udp({peer})")));
    }
    for s in &tcp {
        if !udp_same.iter().any(|u| u.uid == s.uid) {
            found.push((s.clone(), format!("peer_tcp({peer})")));
        }
    }

    let check = if !udp_same.is_empty() && !udp_split.is_empty() {
        Check {
            name: format!("peer:{peer}"),
            status: Status::Ok,
            summary: format!("{} stream(s) answered on UDP and TCP", found.len()),
            cause: None,
            fix: None,
        }
    } else if !udp_same.is_empty() && udp_split.is_empty() {
        Check {
            name: format!("peer:{peer}"),
            status: Status::Warn,
            summary: format!(
                "{} stream(s) answered on same-port UDP, 0 on split-port UDP",
                udp_same.len()
            ),
            cause: Some(
                "stateful firewall drops replies to a separate UDP return port".to_string(),
            ),
            fix: Some(
                "labstream handles this automatically. liblsl inlets need an inbound firewall rule."
                    .to_string(),
            ),
        }
    } else if !tcp.is_empty() {
        Check {
            name: format!("peer:{peer}"),
            status: Status::Warn,
            summary: format!(
                "UDP timed out, TCP LSL:shortinfo found {} stream(s)",
                tcp.len()
            ),
            cause: Some(format!(
                "UDP ports {}-{} are blocked to {peer}, while TCP works",
                cfg.multicast_port,
                cfg.base_port + cfg.port_range - 1
            )),
            fix: Some(format!(
                "add {peer} to lab.KnownPeers in lsl_api.cfg, or open UDP ports {}-{}",
                cfg.multicast_port,
                cfg.base_port + cfg.port_range - 1
            )),
        }
    } else {
        Check {
            name: format!("peer:{peer}"),
            status: Status::Fail,
            summary: format!("no stream answered on {peer} over UDP or TCP"),
            cause: Some(format!(
                "{peer} is unreachable, a firewall blocks ports {}-{}, no outlet runs, or SessionID differs",
                cfg.multicast_port,
                cfg.base_port + cfg.port_range - 1
            )),
            fix: Some(format!(
                "make sure that {peer} answers ping and allows TCP/UDP ports {}-{}",
                cfg.multicast_port,
                cfg.base_port + cfg.port_range - 1
            )),
        }
    };
    (check, found)
}

fn probe_peer_split_udp(
    peer: &str,
    query: &str,
    multicast_port: u16,
    base_port: u16,
    port_range: u16,
    window: Duration,
) -> Vec<StreamInfo> {
    let Ok(rx) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) else {
        return Vec::new();
    };
    let Ok(tx) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) else {
        return Vec::new();
    };
    let Ok(local) = rx.local_addr() else {
        return Vec::new();
    };
    let _ = rx.set_read_timeout(Some(Duration::from_millis(40)));
    let qid = crate::make_query_id();
    let msg = discovery::build_query(query, local.port(), &qid);
    let _ = tx.send_to(&msg, (peer, multicast_port));
    for p in base_port..base_port + port_range {
        let _ = tx.send_to(&msg, (peer, p));
    }
    let mut out = Vec::new();
    let mut buf = [0u8; 65536];
    let end = Instant::now() + window;
    while Instant::now() < end {
        match rx.recv_from(&mut buf) {
            Ok((n, _)) => {
                if let Some(a) = discovery::parse_answer(&buf[..n]) {
                    if a.query_id == qid {
                        if let Some(info) = StreamInfo::from_shortinfo_xml(&a.xml) {
                            if !out.iter().any(|s: &StreamInfo| s.uid == info.uid) {
                                out.push(info);
                            }
                        }
                    }
                }
            }
            Err(_) => {
                if !out.is_empty() {
                    break;
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_formats_text_and_valid_json() {
        let report = Report {
            config_source: "defaults".to_string(),
            session_id: "default".to_string(),
            features: vec!["seq".to_string(), "resume".to_string()],
            fallback_discovery: FallbackDiscovery::Auto,
            known_peers: vec!["10.0.0.2".to_string()],
            multicast_port: 16571,
            base_port: 16572,
            port_range: 32,
            interfaces: vec![Ipv4Interface {
                name: "wlan0".to_string(),
                addr: Ipv4Addr::new(192, 168, 2, 10),
                prefix_len: 24,
                broadcast: Some(Ipv4Addr::new(192, 168, 2, 255)),
                ifindex: 3,
                is_up: true,
                is_loopback: false,
                supports_multicast: true,
            }],
            streams: vec![FoundStream {
                name: "EEG \"1\"".to_string(),
                stream_type: "EEG".to_string(),
                hostname: "lab-pc".to_string(),
                address: "192.168.2.208".to_string(),
                data_port: 16572,
                via: "fallback".to_string(),
            }],
            checks: vec![Check {
                name: "discovery".to_string(),
                status: Status::Warn,
                summary: "multicast found 0, fallback found 1".to_string(),
                cause: Some("multicast is blocked on this network".to_string()),
                fix: Some("use fallback".to_string()),
            }],
        };
        let text = report.format_text();
        assert!(text.contains("labstream doctor"));
        assert!(text.contains("multicast is blocked on this network"));
        assert!(text.contains("via fallback"));
        let json = report.to_json();
        assert!(json.starts_with('{') && json.ends_with('}'));
        assert!(json.contains("\"fallback_discovery\":\"auto\""));
        assert!(json.contains("EEG \\\"1\\\""));
        assert!(!report.has_failures());
    }
}
