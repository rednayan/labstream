//! The outlet: a discovery responder and a TCP feed server.

use crate::info::StreamInfo;
use crate::queue::{Fanout, SampleQueue};
use crate::{clock, config};
use labstream_proto::discovery;
use labstream_proto::header::parse_block;
use labstream_proto::negotiate::{negotiate, FeedRequest, Outcome, ServerStream, LITTLE};
use labstream_proto::timesync;
use labstream_wire::{Codec, Sample, TEST_PATTERN_OFFSETS};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How fast this side converts a byte order.
///
/// liblsl measures this with a benchmark. A fixed value keeps the negotiation
/// deterministic, and the value never travels as a claim about speed. It only
/// decides which side converts. SPEC.md 5.2.
const ENDIAN_PERFORMANCE: f64 = 1000.0;

struct Shared {
    info: Mutex<StreamInfo>,
    /// One queue per connected consumer. SPEC.md 8.8.
    fanout: Fanout,
    /// The consumers of a blocking outlet, which holds no queue at all.
    sync: Mutex<Vec<SyncConsumer>>,
    /// True when this outlet writes each sample before the push returns.
    sync_mode: bool,
    stop: Mutex<bool>,
}

/// One consumer of a blocking outlet.
///
/// A blocking outlet holds the socket rather than a queue. Each consumer keeps
/// its own codec, because two consumers can agree on different byte orders.
struct SyncConsumer {
    stream: TcpStream,
    codec: Codec,
}

/// A stream outlet.
pub struct Outlet {
    shared: Arc<Shared>,
    info: StreamInfo,
    /// True when the outlet holds the port that carries a multicast query.
    ///
    /// Another process on this machine can already hold it. The outlet then
    /// answers a unicast query and a broadcast query, and never sees a
    /// multicast query. A consumer on another machine can therefore miss the
    /// stream. SPEC.md 1.1.
    multicast_bound: bool,
}

impl Outlet {
    /// Publish a stream.
    ///
    /// The call binds a TCP port for the data and a UDP port for discovery and
    /// time probes, then starts one thread for each.
    pub fn new(info: StreamInfo) -> std::io::Result<Self> {
        Outlet::with_flags(info, false)
    }

    /// Publish a stream that writes each sample before the push returns.
    ///
    /// This is `transp_sync_blocking` (`include/lsl/common.h:172`). The outlet
    /// holds no queue, so a consumer that cannot keep up holds up the push
    /// instead of losing a sample.
    ///
    /// **The wire is the same.** liblsl hands the socket to the blocking writer
    /// only after the headers and the test pattern have gone out
    /// (`src/tcp_server.cpp:733`), and the layout of a sample does not change
    /// (`src/sync_serialization.h`). A consumer cannot tell the two modes
    /// apart.
    ///
    /// A stream of strings has no fixed width, and liblsl refuses it here
    /// (`src/stream_outlet_impl.cpp:34-38`).
    pub fn new_blocking(info: StreamInfo) -> std::io::Result<Self> {
        if info.format == labstream_wire::Format::String {
            return Err(std::io::Error::other(
                "Synchronous (zero-copy) mode is not supported for string-format streams",
            ));
        }
        Outlet::with_flags(info, true)
    }

    fn with_flags(mut info: StreamInfo, sync_mode: bool) -> std::io::Result<Self> {
        let cfg = config::get();
        // One stack for each protocol that the configuration allows, the way
        // `stream_outlet_impl::instantiate_stack` builds them.
        let tcp4 = if cfg.allow_ipv4 {
            bind_tcp_in_range(V4).ok()
        } else {
            None
        };
        let udp4 = if cfg.allow_ipv4 {
            bind_udp_in_range(V4).ok()
        } else {
            None
        };
        let tcp6 = if cfg.allow_ipv6 {
            bind_tcp_in_range(V6).ok()
        } else {
            None
        };
        let udp6 = if cfg.allow_ipv6 {
            bind_udp_in_range(V6).ok()
        } else {
            None
        };
        if tcp4.is_none() && tcp6.is_none() {
            return Err(std::io::Error::other(
                "neither the IPv4 nor the IPv6 stack could be built",
            ));
        }
        if let Some(t) = &tcp4 {
            info.v4data_port = t.local_addr()?.port();
        }
        if let Some(u) = &udp4 {
            info.v4service_port = u.local_addr()?.port();
        }
        if let Some(t) = &tcp6 {
            info.v6data_port = t.local_addr()?.port();
        }
        if let Some(u) = &udp6 {
            info.v6service_port = u.local_addr()?.port();
        }
        if info.uid.is_empty() {
            info.uid = crate::make_uid();
        }
        info.created_at = clock();
        // The server sets the session identifier as it starts.
        // `src/tcp_server.cpp:328`.
        if info.session_id.is_empty() {
            info.session_id = config::get().session_id.clone();
        }

        let shared = Arc::new(Shared {
            info: Mutex::new(info.clone()),
            fanout: Fanout::new(),
            sync: Mutex::new(Vec::new()),
            sync_mode,
            stop: Mutex::new(false),
        });

        for sock in [udp4, udp6].into_iter().flatten() {
            let s = Arc::clone(&shared);
            std::thread::spawn(move || udp_loop(sock, s));
        }
        // A multicast or broadcast query arrives on its own port, outside the
        // unicast range (`src/api_config.cpp:169`). An outlet that listens only
        // on the range never answers one.
        let mut multicast_bound = false;
        for family in [V4, V6] {
            if (family == V4 && !cfg.allow_ipv4) || (family == V6 && !cfg.allow_ipv6) {
                continue;
            }
            if let Ok(mc) = bind_multicast_port(family) {
                multicast_bound = true;
                let s = Arc::clone(&shared);
                std::thread::spawn(move || udp_loop(mc, s));
            }
        }
        for listener in [tcp4, tcp6].into_iter().flatten() {
            let s = Arc::clone(&shared);
            std::thread::spawn(move || tcp_loop(listener, s));
        }

        Ok(Outlet {
            shared,
            info,
            multicast_bound,
        })
    }

    /// The description that this outlet publishes.
    pub fn info(&self) -> &StreamInfo {
        &self.info
    }

    /// True when the outlet holds the port that carries a multicast query.
    ///
    /// A false value is not an error on its own. It means that another process
    /// on this machine holds the port, and that only a unicast query or a
    /// broadcast query reaches this outlet.
    pub fn holds_multicast_port(&self) -> bool {
        self.multicast_bound
    }

    /// The number of connected consumers.
    pub fn consumer_count(&self) -> usize {
        if self.shared.sync_mode {
            // A blocking outlet registers its consumers with the writer rather
            // than with the queue. `src/stream_outlet_impl.cpp:176-179`.
            return self.shared.sync.lock().unwrap().len();
        }
        self.shared.fanout.count()
    }

    /// Wait until at least one consumer is connected.
    pub fn wait_for_consumers(&self, timeout: Duration) -> bool {
        let end = Instant::now() + timeout;
        while Instant::now() < end {
            if self.consumer_count() > 0 {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        self.consumer_count() > 0
    }

    /// Send one sample to every connected consumer.
    ///
    /// This never waits. A consumer whose queue is full loses its oldest
    /// sample, and every other consumer is unaffected. SPEC.md 8.7 and 8.8.
    pub fn push(&self, s: &Sample) {
        // Two rules decide the timestamp, and both belong here.
        //
        // A configuration can order every timestamp to be the current clock
        // (`src/stream_outlet_impl.cpp:162`). A pushed timestamp of zero also
        // means the current clock (`src/stream_outlet_impl.cpp:170`, SPEC.md
        // 8.3).
        //
        // The second rule has to live in the outlet and not at the C boundary.
        // A consumer reads a timestamp of zero as "no sample", so an outlet
        // that sends one delivers values that no application can use.
        //
        // The deduced tag is -1.0, so it passes through untouched.
        let timestamp = if config::get().force_default_timestamps || s.timestamp == 0.0 {
            clock()
        } else {
            s.timestamp
        };
        let owned;
        let sample = if timestamp == s.timestamp {
            s
        } else {
            owned = Sample {
                timestamp,
                values: s.values.clone(),
            };
            &owned
        };
        if self.shared.sync_mode {
            write_blocking(&self.shared, sample);
            return;
        }
        self.shared.fanout.push(sample);
    }
}

/// Write one sample to every consumer, and wait for each write to finish.
///
/// A consumer whose socket fails is dropped. Nothing is queued, so a consumer
/// that reads slowly holds up the caller. That is the whole point of the mode.
fn write_blocking(shared: &Shared, sample: &Sample) {
    let mut out = Vec::new();
    let mut consumers = shared.sync.lock().unwrap();
    consumers.retain_mut(|c| {
        out.clear();
        if c.codec.encode(sample, &mut out).is_err() {
            return false;
        }
        c.stream.write_all(&out).is_ok() && c.stream.flush().is_ok()
    });
}

/// Write several samples to every consumer, with one write for each consumer.
fn write_blocking_many(shared: &Shared, samples: &[Sample]) {
    let mut out = Vec::new();
    let mut consumers = shared.sync.lock().unwrap();
    consumers.retain_mut(|c| {
        out.clear();
        for s in samples {
            if c.codec.encode(s, &mut out).is_err() {
                return false;
            }
        }
        c.stream.write_all(&out).is_ok() && c.stream.flush().is_ok()
    });
}

impl Drop for Outlet {
    fn drop(&mut self) {
        *self.shared.stop.lock().unwrap() = true;
        self.shared.fanout.close();
    }
}

/// Which protocol a socket speaks.
pub(crate) const V4: bool = false;
pub(crate) const V6: bool = true;

/// The address that a listening socket of this family binds.
fn any_address(v6: bool) -> std::net::IpAddr {
    if v6 {
        std::net::Ipv6Addr::UNSPECIFIED.into()
    } else {
        std::net::Ipv4Addr::UNSPECIFIED.into()
    }
}

/// Bind the first free port of the range, for one protocol.
///
/// liblsl walks the same range and then falls back to a port that the
/// operating system picks, when `ports.AllowRandomPorts` allows it
/// (`src/socket_utils.cpp:6-25`).
fn bind_tcp_in_range(v6: bool) -> std::io::Result<TcpListener> {
    let cfg = config::get();
    let host = any_address(v6);
    let mut last = None;
    for p in cfg.base_port..cfg.base_port + cfg.port_range {
        match TcpListener::bind((host, p)) {
            Ok(l) => return Ok(l),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("no free TCP port in the range")))
}

fn bind_udp_in_range(v6: bool) -> std::io::Result<UdpSocket> {
    let cfg = config::get();
    let host = any_address(v6);
    let mut last = None;
    for p in cfg.base_port..cfg.base_port + cfg.port_range {
        match UdpSocket::bind((host, p)) {
            Ok(s) => {
                // Joining with the unspecified address lets the kernel pick one
                // interface. It can pick the loopback, which carries no
                // multicast, and the outlet is then invisible.
                join_groups(&s, v6);
                if !v6 {
                    let _ = s.set_broadcast(true);
                }
                return Ok(s);
            }
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("no free UDP port in the range")))
}

fn bind_multicast_port(v6: bool) -> std::io::Result<UdpSocket> {
    // The port has to be shared. `src/udp_server.cpp:60` sets `reuse_address`
    // before it binds, so several outlets in one process, and several
    // programs on one machine, all receive the same multicast query.
    //
    // Without this, only the first outlet answers a multicast query. A program
    // that publishes a data stream and a marker stream would then have one of
    // the two invisible to another machine.
    //
    // The standard library has no call for this option.
    let addr = SocketAddr::new(any_address(v6), config::get().multicast_port);
    let sock = socket2::Socket::new(
        if v6 {
            socket2::Domain::IPV6
        } else {
            socket2::Domain::IPV4
        },
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )?;
    sock.set_reuse_address(true)?;
    if v6 {
        // A socket that also accepts IPv4 would take the port from the IPv4
        // responder, and one of the two families would go unanswered.
        sock.set_only_v6(true)?;
    }
    sock.bind(&addr.into())?;
    let s: UdpSocket = sock.into();
    join_groups(&s, v6);
    if !v6 {
        let _ = s.set_broadcast(true);
    }
    Ok(s)
}

/// Join every discovery group of one family on every local interface.
///
/// An IPv6 join names an interface by its index, not by its address
/// (`src/udp_server.cpp:78`). Index zero lets the operating system pick, and
/// every real index is tried as well.
fn join_groups(s: &UdpSocket, v6: bool) {
    if v6 {
        for group in &config::get().multicast_addresses {
            if let Ok(addr) = group.parse::<std::net::Ipv6Addr>() {
                if !addr.is_multicast() {
                    continue;
                }
                for index in crate::local_interface_indexes() {
                    let _ = s.join_multicast_v6(&addr, index);
                }
            }
        }
        return;
    }
    let mut ifaces = crate::local_ipv4_addresses();
    ifaces.push(std::net::Ipv4Addr::UNSPECIFIED);
    for group in &config::get().multicast_addresses {
        if let Ok(addr) = group.parse::<std::net::Ipv4Addr>() {
            // A broadcast address is not a group, so no join is possible.
            if addr.is_broadcast() || !addr.is_multicast() {
                continue;
            }
            for iface in &ifaces {
                let _ = s.join_multicast_v4(&addr, iface);
            }
        }
    }
}

/// Answer discovery queries and time probes. SPEC.md 2 and 3.
fn udp_loop(sock: UdpSocket, shared: Arc<Shared>) {
    let _ = sock.set_read_timeout(Some(Duration::from_millis(200)));
    let mut buf = [0u8; 65536];
    loop {
        if *shared.stop.lock().unwrap() {
            return;
        }
        let (n, from) = match sock.recv_from(&mut buf) {
            Ok(x) => x,
            Err(_) => continue,
        };
        // The arrival time belongs to a time probe, so read the clock first.
        let t1 = clock();
        let msg = &buf[..n];
        if std::env::var_os("LSL_TRACE_UDP").is_some() {
            eprintln!(
                "TRACE udp {} bytes from {}: {:?}",
                n,
                from,
                String::from_utf8_lossy(&msg[..n.min(80)])
            );
        }

        if let Some(q) = discovery::parse_query(msg) {
            let info = shared.info.lock().unwrap().clone();
            // A query that does not match gets no answer at all. SPEC.md 2.2.
            if info.matches(&q.query) {
                let xml = info.to_shortinfo_xml();
                let reply = discovery::build_answer(&q.query_id, &xml);
                let to = std::net::SocketAddr::new(from.ip(), q.return_port);
                let _ = sock.send_to(&reply, to);
            }
            continue;
        }

        if let Some(p) = timesync::parse_probe(msg) {
            let t2 = clock();
            let reply = timesync::build_answer(p.wave_id, p.t0, t1, t2);
            let _ = sock.send_to(&reply, from);
            continue;
        }
        // An unknown method gets no answer. SPEC.md 10.
    }
}

fn tcp_loop(listener: TcpListener, shared: Arc<Shared>) {
    let _ = listener.set_nonblocking(true);
    loop {
        if *shared.stop.lock().unwrap() {
            return;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                let s = Arc::clone(&shared);
                let _ = stream.set_nonblocking(false);
                std::thread::spawn(move || {
                    let _ = serve_client(stream, s);
                });
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return,
        }
    }
}

/// Run one feed connection. SPEC.md 4, 5, and 6.
fn serve_client(mut stream: TcpStream, shared: Arc<Shared>) -> std::io::Result<()> {
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    crate::keepalive::enable(&stream);

    // Read the request line first. Only a feed request carries a header block,
    // so a read that waits for the empty line would hang on the other two.
    // `src/tcp_server.cpp:499` reads the same way.
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let nl = loop {
        if let Some(p) = buf.iter().position(|&c| c == b'\n') {
            break p;
        }
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > 1 << 20 {
            return Ok(());
        }
    };
    let request_line = String::from_utf8_lossy(&buf[..nl])
        .trim_end_matches('\r')
        .to_string();

    let info = shared.info.lock().unwrap().clone();

    // A request for the whole description. The answer is the document, and the
    // close of the connection ends it. `src/tcp_server.cpp:508`.
    if request_line == "LSL:fullinfo" {
        stream.write_all(info.to_fullinfo_xml().as_bytes())?;
        return stream.flush();
    }

    // A request for the short description, with the query on the next line.
    // A stream that does not match the query gets no answer at all.
    // `src/tcp_server.cpp:537`.
    if request_line == "LSL:shortinfo" {
        let query = loop {
            let rest = &buf[nl + 1..];
            if let Some(p) = rest.iter().position(|&c| c == b'\n') {
                break String::from_utf8_lossy(&rest[..p]).trim().to_string();
            }
            let n = stream.read(&mut tmp)?;
            if n == 0 {
                return Ok(());
            }
            buf.extend_from_slice(&tmp[..n]);
        };
        if info.matches(&query) {
            stream.write_all(info.to_shortinfo_xml().as_bytes())?;
            return stream.flush();
        }
        return Ok(());
    }

    // A feed request carries a header block that ends with an empty line. Wait
    // for that line before the block is read.
    while find_double_crlf(&buf).is_none() {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > 1 << 20 {
            return Ok(());
        }
    }

    // Only the feed request is served here. SPEC.md 4.1 names the others.
    let (version, uid) = match request_line.strip_prefix("LSL:streamfeed/") {
        Some(rest) => {
            let mut it = rest.splitn(2, ' ');
            let v: i32 = it.next().unwrap_or("0").trim().parse().unwrap_or(0);
            (v, it.next().unwrap_or("").trim().to_string())
        }
        None => {
            if request_line.starts_with("LSL:streamfeed") {
                (100, String::new())
            } else {
                return Ok(());
            }
        }
    };

    let (headers, _) = parse_block(&buf[nl + 1..]).unwrap_or_default();
    let mut srv = ServerStream::new(&info.uid, info.format, LITTLE, ENDIAN_PERFORMANCE);
    if info.format == labstream_wire::Format::String {
        srv.channel_bytes = 0;
    }
    let req = FeedRequest::from_headers(version, &uid, &headers, srv.channel_bytes);

    let trace = std::env::var_os("LSL_TRACE_TCP").is_some();
    if trace {
        eprintln!("TRACE request {request_line:?}");
        eprintln!("TRACE headers {headers:?}");
    }

    let accepted = match negotiate(&req, &srv) {
        Outcome::Refuse(s) => {
            if trace {
                eprintln!("TRACE refused {} {}", s.code, s.message);
            }
            stream.write_all(s.to_wire().as_bytes())?;
            return Ok(());
        }
        Outcome::Accept(a) => a,
    };

    // Protocol 1.00 carries every sample in a Boost archive
    // (`src/sample.cpp:330`), which this implementation does not write.
    //
    // liblsl answers 505 only when the **major** version differs
    // (`src/tcp_server.cpp:576`), so 505 is the wrong word here: a request for
    // 1.00 against a 1.10 server is one that liblsl serves. There is no code
    // in this protocol for "the version is one I cannot write".
    //
    // The connection therefore closes with nothing written. A peer reads the
    // end of the stream, which is a state every client already handles, rather
    // than a status line that a 1.00 client never looks for.
    if accepted.data_protocol_version < 110 {
        if trace {
            eprintln!(
                "TRACE closing: the peer agreed to protocol {}, which this side does not write",
                accepted.data_protocol_version
            );
        }
        return Ok(());
    }

    stream.write_all(
        accepted
            .to_wire(srv.configured_version, &info.uid)
            .as_bytes(),
    )?;

    let order = match accepted.codec_order() {
        Some(o) => o,
        None => return Ok(()),
    };
    let codec = Codec::new(info.format, info.channel_count as usize, order)
        .with_suppress_subnormals(accepted.suppress_subnormals);

    // Two test pattern samples open every feed. SPEC.md 6.
    let mut out = Vec::new();
    for off in TEST_PATTERN_OFFSETS {
        let s = labstream_wire::test_pattern(info.format, info.channel_count as usize, off);
        codec
            .encode(&s, &mut out)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
    }
    if trace {
        eprintln!(
            "TRACE accepted version={} buffer={} chunk={} order={:?} test-pattern={} bytes",
            accepted.data_protocol_version,
            accepted.max_buffer_length,
            accepted.max_chunk_length,
            accepted.codec_order(),
            out.len()
        );
    }
    stream.write_all(&out)?;
    stream.flush()?;

    // A buffer length of zero or less sends nothing more. SPEC.md 9.
    if accepted.max_buffer_length <= 0 {
        return Ok(());
    }

    // A blocking outlet keeps the socket instead of a queue. The handover
    // happens here, after the headers and the test pattern, which is where
    // liblsl does it (`src/tcp_server.cpp:733`).
    if shared.sync_mode {
        if trace {
            eprintln!("TRACE handing the socket to the blocking writer");
        }
        shared
            .sync
            .lock()
            .unwrap()
            .push(SyncConsumer { stream, codec });
        return Ok(());
    }

    // The header carries a count of samples, already converted from whatever
    // unit the peer's API used. SPEC.md 8.6.
    let queue = shared
        .fanout
        .add(accepted.max_buffer_length.max(1) as usize);

    // A chunk decides when one write happens. It is not a wire unit. SPEC.md 9.
    // A value of zero means no limit (`src/tcp_server.cpp:749-753`). This
    // outlet has no chunk size of its own.
    let max_chunk = if accepted.max_chunk_length > 0 {
        accepted.max_chunk_length as usize
    } else {
        usize::MAX
    };

    let stop = || *shared.stop.lock().unwrap();
    let sent = feed(&queue, &codec, &mut stream, max_chunk, &stop, trace);
    if trace {
        eprintln!("TRACE session over after {sent} samples");
    }
    shared.fanout.remove(&queue);
    Ok(())
}

/// Copy the samples of one consumer queue to its socket.
///
/// liblsl writes when a sample carries the push-through flag, or when the
/// count reaches the chunk size (`src/tcp_server.cpp:782`). Each push call of
/// liblsl sets the flag on its last sample by default
/// (`src/stream_outlet_impl.h:74`, `src/stream_outlet_impl.h:208-209`).
///
/// Here, the samples of one push call go into the queue with one lock. The
/// writer takes all samples that wait, and it writes when the count reaches
/// the chunk size or when the queue is empty. An empty queue therefore acts
/// as the push-through flag. When the writer is slower than the source, one
/// write can carry more than one push call. No sample waits for a later push.
///
/// The result is the number of samples written.
fn feed<W: Write>(
    queue: &SampleQueue,
    codec: &Codec,
    out: &mut W,
    max_chunk: usize,
    stop: &dyn Fn() -> bool,
    trace: bool,
) -> usize {
    let mut batch: Vec<Sample> = Vec::new();
    let mut pending: Vec<u8> = Vec::new();
    let mut count = 0usize;
    let mut sent = 0usize;
    let mut write = |pending: &mut Vec<u8>, count: &mut usize, sent: &mut usize| -> bool {
        if trace && *sent == 0 {
            eprintln!("TRACE first data write, {} bytes", pending.len());
        }
        if let Err(e) = out.write_all(pending) {
            if trace {
                eprintln!("TRACE write failed after {sent} samples: {e}");
            }
            return false;
        }
        let _ = out.flush();
        *sent += *count;
        pending.clear();
        *count = 0;
        true
    };
    loop {
        if stop() || queue.is_closed() {
            break;
        }
        batch.clear();
        if queue.pop_many(&mut batch, usize::MAX, Duration::from_millis(200)) == 0 {
            continue;
        }
        for s in &batch {
            if codec.encode(s, &mut pending).is_err() {
                return sent;
            }
            count += 1;
            if count >= max_chunk && !write(&mut pending, &mut count, &mut sent) {
                return sent;
            }
        }
        // The queue was empty when the batch came out, so nothing more is on
        // the way. Write what is left.
        if !pending.is_empty() && !write(&mut pending, &mut count, &mut sent) {
            return sent;
        }
    }
    sent
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

impl Outlet {
    /// Send several samples that share one timestamp, with one lock.
    ///
    /// This is the same call as [`Outlet::push_chunk`]. It stays for the
    /// programs that use it.
    pub fn push_chunk_fast(&self, samples: &[Sample], timestamp: f64) {
        self.push_chunk(samples, timestamp);
    }

    /// Send several samples, each with its own timestamp, with one lock.
    ///
    /// The timestamp rules of [`Outlet::push`] apply to each sample. A
    /// consumer receives the samples of one call together, so a consumer
    /// connection writes them with one socket write. SPEC.md 9.
    ///
    /// A blocking outlet writes the samples to each consumer with one write,
    /// before the call returns.
    pub fn push_samples(&self, mut samples: Vec<Sample>) {
        if samples.is_empty() {
            return;
        }
        for s in &mut samples {
            s.timestamp = self.stamp(s.timestamp);
        }
        if self.shared.sync_mode {
            write_blocking_many(&self.shared, &samples);
            return;
        }
        self.shared.fanout.push_many(&samples);
    }

    /// The timestamp of each sample of a chunk. SPEC.md 9.2.
    fn chunk_timestamps<'a>(
        &self,
        samples: &'a [Sample],
        timestamp: f64,
    ) -> impl Iterator<Item = (f64, &'a Sample)> {
        let mut first = if timestamp == 0.0 {
            crate::clock()
        } else {
            timestamp
        };
        if self.info.nominal_srate != 0.0 {
            first -= (samples.len() - 1) as f64 / self.info.nominal_srate;
        }
        samples.iter().enumerate().map(move |(k, s)| {
            let t = if k == 0 {
                first
            } else {
                labstream_wire::DEDUCED_TIMESTAMP
            };
            (t, s)
        })
    }

    /// Apply the two timestamp rules of [`Outlet::push`].
    fn stamp(&self, t: f64) -> f64 {
        if config::get().force_default_timestamps || t == 0.0 {
            clock()
        } else {
            t
        }
    }

    /// Send several samples that share one timestamp. SPEC.md 9.2.
    ///
    /// The timestamp names the **last** sample. A stream with a rate counts
    /// backward from it, so the first sample is dated
    /// `timestamp - (count - 1) / rate`. Only that first sample carries a
    /// timestamp on the wire, and the rest carry the deduced tag.
    ///
    /// An application that acquires a block and stamps it on arrival therefore
    /// gets the right time for every sample in the block.
    ///
    /// The samples go to each consumer with one lock, and a consumer
    /// connection writes them with one socket write. SPEC.md 9.
    pub fn push_chunk(&self, samples: &[Sample], timestamp: f64) {
        if samples.is_empty() {
            return;
        }
        let block: Vec<Sample> = self
            .chunk_timestamps(samples, timestamp)
            .map(|(t, s)| Sample {
                timestamp: t,
                values: s.values.clone(),
            })
            .collect();
        self.push_samples(block);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use labstream_wire::{ByteOrder, Format, Value};
    use std::sync::atomic::{AtomicBool, Ordering};

    /// A writer that counts its calls and keeps the bytes.
    #[derive(Default)]
    struct Counting {
        writes: usize,
        bytes: Vec<u8>,
    }

    impl Write for Counting {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.writes += 1;
            self.bytes.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn samples(n: usize) -> Vec<Sample> {
        (0..n)
            .map(|k| Sample {
                timestamp: k as f64,
                values: vec![Value::F32(k as f32), Value::F32(0.5)],
            })
            .collect()
    }

    /// Feed the queue that the caller filled, then stop.
    fn run(queue: &SampleQueue, max_chunk: usize) -> (usize, Counting) {
        let codec = Codec::new(Format::Float32, 2, ByteOrder::native());
        let mut out = Counting::default();
        let once = AtomicBool::new(false);
        // The first call to `stop` lets one round run. The second one stops.
        let stop = || once.swap(true, Ordering::SeqCst);
        let sent = feed(queue, &codec, &mut out, max_chunk, &stop, false);
        (sent, out)
    }

    #[test]
    fn a_chunk_with_no_limit_goes_out_in_one_write() {
        let q = SampleQueue::new(1000);
        q.push_many(&samples(100));
        let (sent, out) = run(&q, usize::MAX);
        assert_eq!(sent, 100);
        assert_eq!(out.writes, 1, "one write for the whole chunk");
    }

    #[test]
    fn a_chunk_limit_splits_the_writes() {
        let q = SampleQueue::new(1000);
        q.push_many(&samples(100));
        let (sent, out) = run(&q, 16);
        assert_eq!(sent, 100);
        // Six full chunks of 16, then the last 4 samples.
        assert_eq!(out.writes, 7);
    }

    #[test]
    fn a_single_sample_is_not_held_back() {
        // A limit that one sample does not reach. The sample still goes out
        // at once, as the push-through flag of liblsl does.
        let q = SampleQueue::new(1000);
        q.push_many(&samples(1));
        let (sent, out) = run(&q, 16);
        assert_eq!(sent, 1);
        assert_eq!(out.writes, 1);
    }

    #[test]
    fn the_bytes_decode_to_the_same_samples() {
        let q = SampleQueue::new(1000);
        let sent = samples(50);
        q.push_many(&sent);
        let (_, out) = run(&q, 7);
        let codec = Codec::new(Format::Float32, 2, ByteOrder::native());
        let (got, used) = codec.decode_all(&out.bytes).unwrap();
        assert_eq!(used, out.bytes.len());
        assert_eq!(got, sent);
    }
}
