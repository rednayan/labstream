//! The outlet: a discovery responder and a TCP feed server.

use crate::health::{ConsumerHealth, OutletHealth};
use crate::info::StreamInfo;
use crate::queue::{Fanout, SampleQueue};
use crate::{clock, config};
use labstream_proto::discovery;
use labstream_proto::features;
use labstream_proto::header::parse_block;
use labstream_proto::negotiate::{negotiate, FeedRequest, Outcome, ServerStream, LITTLE};
use labstream_proto::seq::Numbering;
use labstream_proto::timesync;
use labstream_wire::{Codec, PackedSample, Sample, TEST_PATTERN_OFFSETS};
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
    sync: Mutex<SyncSet>,
    /// True when this outlet writes each sample before the push returns.
    sync_mode: bool,
    stop: Mutex<bool>,
    /// The random value that identifies this run of the outlet for the
    /// `resume` feature. `docs/extensions.md`, negotiation rule 2.
    epoch: u64,
}

/// One consumer of a blocking outlet.
///
/// A blocking outlet holds the socket rather than a queue. Each consumer keeps
/// its own codec, because two consumers can agree on different byte orders.
struct SyncConsumer {
    id: u64,
    peer: String,
    stream: TcpStream,
    codec: Codec,
    /// The record state, when the consumer agreed to `seq/1`.
    numbering: Option<Numbering>,
}

/// The consumers of a blocking outlet, and the number of the next sample.
///
/// One lock holds both, so the numbers follow the order of the writes.
#[derive(Default)]
struct SyncSet {
    list: Vec<SyncConsumer>,
    next_consumer_id: u64,
    next: u64,
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
        Outlet::with_flags(info, false, config::get().history_limits())
    }

    /// Publish a stream with explicit `resume` history and disk-spool limits
    /// (`docs/roadmap.md`, step 3).
    pub fn new_with_limits(
        info: StreamInfo,
        limits: crate::history::HistoryLimits,
    ) -> std::io::Result<Self> {
        Outlet::with_flags(info, false, limits)
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
        Outlet::with_flags(info, true, config::get().history_limits())
    }

    fn with_flags(
        mut info: StreamInfo,
        sync_mode: bool,
        limits: crate::history::HistoryLimits,
    ) -> std::io::Result<Self> {
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
            fanout: Fanout::with_rate(info.nominal_srate, limits),
            sync: Mutex::new(SyncSet::default()),
            sync_mode,
            stop: Mutex::new(false),
            epoch: {
                let b = crate::random_bytes();
                u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
            },
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

    /// How many samples the `resume` history holds across RAM and disk. Zero
    /// when the history is off, which is the case until a consumer agrees to
    /// `resume/1`. `docs/extensions.md`, section `resume`.
    pub fn history_len(&self) -> usize {
        self.shared.fanout.history_len()
    }

    /// Bytes currently held by the `resume` history in RAM.
    pub fn history_ram_bytes(&self) -> usize {
        self.shared.fanout.history_ram_bytes()
    }

    /// Bytes currently held by the `resume` history in disk spool segments.
    pub fn history_disk_bytes(&self) -> u64 {
        self.shared.fanout.history_disk_bytes()
    }

    /// Path of the active disk spool directory, if disk spilling has started.
    pub fn history_spool_dir(&self) -> Option<std::path::PathBuf> {
        self.shared.fanout.history_spool_dir()
    }

    /// The number of connected consumers.
    pub fn consumer_count(&self) -> usize {
        if self.shared.sync_mode {
            // A blocking outlet registers its consumers with the writer rather
            // than with the queue. `src/stream_outlet_impl.cpp:176-179`.
            return self.shared.sync.lock().unwrap().list.len();
        }
        self.shared.fanout.count()
    }

    /// Snapshot the fill and drop state of every connected consumer queue.
    ///
    /// For a blocking outlet (`Outlet::new_blocking`), each consumer has no
    /// queue and reports zero fill, zero capacity, and zero drops.
    pub fn health(&self) -> OutletHealth {
        if self.shared.sync_mode {
            let set = self.shared.sync.lock().unwrap();
            let consumers = set
                .list
                .iter()
                .map(|c| ConsumerHealth {
                    id: c.id,
                    peer: c.peer.clone(),
                    fill: 0,
                    capacity: 0,
                    dropped: 0,
                })
                .collect();
            return OutletHealth {
                consumers,
                history_len: 0,
                pushed: set.next,
            };
        }
        self.shared.fanout.health()
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
        self.push_packed(PackedSample::from_sample(s));
    }

    /// Emit a live metadata update stamped with the current local clock
    /// (`docs/roadmap.md`, step 16).
    ///
    /// Every connected `labstream` consumer that negotiated `meta/1` receives
    /// the update in order with the sample stream. `liblsl` inlets see no
    /// change.
    pub fn push_metadata(&self, key: &str, value: &str) -> labstream_proto::meta::MetadataUpdate {
        self.push_metadata_at(key, value, 0.0)
    }

    /// Emit a live metadata update with an explicit timestamp (`0.0` uses the
    /// current local clock).
    pub fn push_metadata_at(
        &self,
        key: &str,
        value: &str,
        timestamp: f64,
    ) -> labstream_proto::meta::MetadataUpdate {
        let ts = if timestamp == 0.0 || config::get().force_default_timestamps {
            crate::clock()
        } else {
            timestamp
        };
        self.shared
            .fanout
            .push_metadata(key.to_string(), value.to_string(), ts)
    }

    /// Snapshot the latest live metadata value emitted for each key.
    pub fn metadata(
        &self,
    ) -> std::collections::BTreeMap<String, labstream_proto::meta::MetadataUpdate> {
        self.shared.fanout.metadata_snapshot().1
    }

    /// Send one [`PackedSample`] to every connected consumer without copying
    /// its channel payload across consumer queues or the `resume` history.
    pub fn push_packed(&self, mut s: PackedSample) {
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
        s.timestamp = self.stamp(s.timestamp);
        if self.shared.sync_mode {
            write_blocking_packed(&self.shared, &s);
            return;
        }
        self.shared.fanout.push_packed(s);
    }

    /// Send one numeric sample directly from raw native-endian bytes.
    ///
    /// When only `shm/1` consumers are attached and `resume` history is off,
    /// this avoids heap allocation completely.
    pub fn push_numeric_raw(&self, timestamp: f64, format: labstream_wire::Format, bytes: &[u8]) {
        let ts = self.stamp(timestamp);
        if self.shared.sync_mode {
            let s = PackedSample::from_raw_bytes(ts, format, Arc::from(bytes));
            write_blocking_packed(&self.shared, &s);
            return;
        }
        self.shared.fanout.push_numeric_raw(ts, format, bytes);
    }

    /// Send a flat contiguous numeric chunk with per-sample timestamps,
    /// writing directly into `shm/1` consumer rings before allocating any
    /// queued samples.
    pub fn push_chunk_numeric_raw(
        &self,
        format: labstream_wire::Format,
        raw: &[u8],
        row_bytes: usize,
        timestamps: &[f64],
    ) {
        if timestamps.is_empty() || row_bytes == 0 {
            return;
        }
        let stamped: Vec<f64> = timestamps.iter().map(|&t| self.stamp(t)).collect();
        if self.shared.sync_mode {
            let block: Vec<PackedSample> = (0..stamped.len())
                .map(|k| {
                    PackedSample::from_raw_bytes(
                        stamped[k],
                        format,
                        Arc::from(&raw[k * row_bytes..(k + 1) * row_bytes]),
                    )
                })
                .collect();
            write_blocking_many_packed(&self.shared, &block);
            return;
        }
        self.shared
            .fanout
            .push_chunk_numeric_raw(format, raw, row_bytes, &stamped);
    }
}

/// Write one packed sample to every consumer, and wait for each write to finish.
///
/// A consumer whose socket fails is dropped. Nothing is queued, so a consumer
/// that reads slowly holds up the caller. That is the whole point of the mode.
fn write_blocking_packed(shared: &Shared, sample: &PackedSample) {
    write_blocking_many_packed(shared, std::slice::from_ref(sample));
}

/// Write several packed samples to every consumer, with one write for each consumer.
fn write_blocking_many_packed(shared: &Shared, samples: &[PackedSample]) {
    let mut out = Vec::new();
    let mut set = shared.sync.lock().unwrap();
    let first = set.next;
    set.next = first.wrapping_add(samples.len() as u64);
    set.list.retain_mut(|c| {
        out.clear();
        for (i, s) in samples.iter().enumerate() {
            if let Some(n) = c.numbering.as_mut() {
                n.before(first.wrapping_add(i as u64), &mut out);
            }
            if c.codec.encode_packed(s, &mut out).is_err() {
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

    // A labstream inlet offers its features in one more header. An inlet
    // that sent none gets the plain answer. `docs/extensions.md`.
    let allowed = &config::get().features;
    let offered = headers.get(features::HEADER_KEY).map(features::parse);
    let mut answered = offered.as_deref().map(|o| features::answer(o, allowed));
    let numbered = answered
        .as_deref()
        .is_some_and(|a| features::has(a, features::SEQ, 1));

    // Negotiate `enc/1` (off by default, `docs/roadmap.md`, step 14).
    let enc_policy = crate::enc::effective_policy();
    let enc_key: Option<[u8; 32]> = if enc_policy.enabled {
        let offered_enc = offered.as_deref().and_then(features::enc_request);
        match (offered_enc, answered.as_mut()) {
            (Some(peer_enc), Some(a)) => {
                let kp = crate::enc::EphemeralKey::generate()?;
                let pub_bytes = kp.public_bytes();
                let key = kp.derive_feed_key(peer_enc.pubkey, true, enc_policy.psk.as_deref())?;
                a.push(features::EncParam { pubkey: pub_bytes }.token());
                if trace {
                    eprintln!("TRACE enc: enabled (X25519 + ChaCha20-Poly1305)");
                }
                Some(key)
            }
            _ if enc_policy.required => {
                let _ = stream.write_all(b"LSL/110 403 Encryption required\r\n\r\n");
                let _ = stream.flush();
                return Ok(());
            }
            _ => None,
        }
    } else {
        None
    };

    // Negotiate `select/1` before `resume/1` and `dgram/1` because decimation
    // changes the sample timeline and channel projection shrinks datagrams.
    let selected: Option<features::SelectParam> = if let (false, Some(o), Some(a)) =
        (shared.sync_mode, offered.as_deref(), answered.as_mut())
    {
        if let Some(sel) = features::select_request(
            o,
            allowed,
            info.channel_count as usize,
            info.nominal_srate,
            info.format == labstream_wire::Format::String,
        ) {
            a.push(sel.token());
            if trace {
                eprintln!(
                    "TRACE select: decimate={} channels={:?}",
                    sel.decimate, sel.channels
                );
            }
            Some(sel)
        } else {
            None
        }
    } else {
        None
    };
    let decimating = selected.as_ref().is_some_and(|s| s.decimate > 1);
    let out_channels = selected.as_ref().map_or(info.channel_count as usize, |s| {
        s.output_channels(info.channel_count as usize)
    });

    let peer_label = stream
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_default();

    // A `resume` consumer gets its queue before the answer, because the
    // answer holds the number of the first sample of the queue. liblsl makes
    // the queue later (`src/tcp_server.cpp:746`), which a peer cannot see.
    // `docs/extensions.md`, resend rule 1.
    let mut resumed: Option<(Registered, Resend)> = None;
    let mut resume_fresh_for_shm = false;
    let queue_follows =
        !shared.sync_mode && accepted.max_buffer_length > 0 && accepted.codec_order().is_some();
    let allow_resume = queue_follows && !decimating;

    // Check `shm/1` eligibility up front so fresh connections can negotiate
    // `shm/1` cleanly alongside `resume/1` without allocating a redundant queue.
    // When `enc/1` is active, keep samples on the authenticated encrypted TCP stream.
    let wants_shm = queue_follows
        && enc_key.is_none()
        && info.format != labstream_wire::Format::String
        && !decimating
        && crate::shm::supported()
        && offered
            .as_deref()
            .is_some_and(|o| features::shm_request(o, allowed));

    if let (true, Some(o), Some(a)) = (allow_resume, offered.as_deref(), answered.as_mut()) {
        if let Some(request) = features::resume_request(o, a, allowed) {
            if shared.fanout.start_history() {
                let from = match request {
                    features::ResumeRequest::From(p) if p.epoch == shared.epoch => Some(p.number),
                    _ => None,
                };
                if wants_shm && from.is_none() {
                    let n = shared.fanout.next_number();
                    a.push(
                        features::ResumeParam {
                            epoch: shared.epoch,
                            number: n,
                        }
                        .token(),
                    );
                    resume_fresh_for_shm = true;
                } else {
                    let (queue, n) = shared.fanout.add_resumable_with_peer(
                        accepted.max_buffer_length.max(1) as usize,
                        peer_label.clone(),
                    );
                    a.push(
                        features::ResumeParam {
                            epoch: shared.epoch,
                            number: n,
                        }
                        .token(),
                    );
                    if trace {
                        eprintln!("TRACE resume: from {from:?}, first live sample {n}");
                    }
                    resumed = Some((
                        Registered {
                            fanout: &shared.fanout,
                            queue,
                        },
                        Resend { from, until: n },
                    ));
                }
            }
        }
    }

    // Negotiate `comp/1` on numeric non-blocking sessions (`docs/extensions.md`,
    // section `comp`). If `shm/1` activates after the headers, shared memory
    // transfers raw bytes directly and `comp/1` is unused.
    let compressed = if let (false, Some(o), Some(a)) =
        (shared.sync_mode, offered.as_deref(), answered.as_mut())
    {
        let is_str = info.format == labstream_wire::Format::String;
        if features::comp_request(o, allowed, is_str) {
            a.push(features::Token::new(features::COMP, 1));
            if trace {
                eprintln!("TRACE comp: enabled");
            }
            true
        } else {
            false
        }
    } else {
        false
    };

    // Negotiate `shm/1` on fresh numeric non-decimated sessions
    // (`docs/extensions.md`, section `shm`).
    let mut shm_writer: Option<Arc<crate::shm::ShmWriter>> = None;
    if let (true, true, Some(a)) = (wants_shm, resumed.is_none(), answered.as_mut()) {
        if let Ok(writer) = crate::shm::ShmWriter::create(
            info.format,
            out_channels,
            accepted.max_buffer_length.max(1) as usize,
        ) {
            a.push(features::ShmParam { id: writer.id() }.token());
            if trace {
                eprintln!("TRACE shm: offered segment {:016x}", writer.id());
            }
            shm_writer = Some(Arc::new(writer));
        }
    }

    // A `dgram` consumer gets a UDP socket of its own, and the answer gives
    // its port and a cookie. `docs/extensions.md`, section `dgram`.
    // When `shm/1` is offered on loopback, skip `dgram/1` so a container
    // fallback uses pure TCP (`docs/roadmap.md`, step 9, criterion 3).
    let peer_is_loopback = stream.peer_addr().is_ok_and(|p| p.ip().is_loopback());
    let can_dgram = enc_key.is_none()
        && (resumed.is_some() || resume_fresh_for_shm)
        && !(shm_writer.is_some() && peer_is_loopback);
    let mut dgram_socket: Option<(std::net::UdpSocket, u64, std::net::IpAddr)> = None;
    if let (true, Some(o), Some(a)) = (can_dgram, offered.as_deref(), answered.as_mut()) {
        let fits = crate::dgram_out::sample_fits(info.format, out_channels);
        if fits && features::dgram_request(o, a, allowed) {
            let peer = stream.peer_addr()?;
            if let Some(sock) = crate::dgram_out::bind(peer) {
                let cookie = crate::dgram_out::cookie();
                let port = sock.local_addr()?.port();
                a.push(features::DgramParam { port, cookie }.token());
                if trace {
                    eprintln!("TRACE dgram: UDP port {port}");
                }
                dgram_socket = Some((sock, cookie, peer.ip()));
            }
        }
    }

    stream.write_all(
        accepted
            .to_wire_with(srv.configured_version, &info.uid, answered.as_deref())
            .as_bytes(),
    )?;

    let order = match accepted.codec_order() {
        Some(o) => o,
        None => return Ok(()),
    };
    let codec = Codec::new(info.format, info.channel_count as usize, order)
        .with_suppress_subnormals(accepted.suppress_subnormals);
    let data_codec = Codec::new(info.format, out_channels, order)
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
        let mut set = shared.sync.lock().unwrap();
        // A numbered consumer gets its first record at once, under the lock
        // that numbers the writes. `docs/extensions.md`.
        let numbering = if numbered {
            let mut n = Numbering::new();
            let mut record = Vec::new();
            n.start(set.next, &mut record);
            stream.write_all(&record)?;
            stream.flush()?;
            Some(n)
        } else {
            None
        };
        set.next_consumer_id = set.next_consumer_id.wrapping_add(1);
        let id = set.next_consumer_id;
        set.list.push(SyncConsumer {
            id,
            peer: peer_label,
            stream,
            codec,
            numbering,
        });
        return Ok(());
    }

    let meta_enabled = answered
        .as_deref()
        .is_some_and(|a| features::has(a, features::META, 1));
    // Snapshot live metadata before registering this consumer in the fanout so
    // updates emitted right after `wait_for_consumers()` are never coalesced
    // into the initial snapshot.
    let initial_meta = meta_enabled.then(|| shared.fanout.metadata_snapshot());

    // Step 5 of `docs/extensions.md` (`shm`): wait up to 1 s for `shm-ok <token>`.
    // If the token matches, register the SHM ring directly in the fanout and
    // keep the TCP socket open to detect end-of-stream and recovery.
    // Otherwise unlink the segment and fall back cleanly to TCP/dgram.
    if let Some(writer) = shm_writer {
        if crate::shm::read_shm_reply(&mut stream, writer.token(), Duration::from_secs(1)) {
            if trace {
                eprintln!("TRACE shm: active on segment {:016x}", writer.id());
            }
            writer.unlink_name();
            drop(dgram_socket);
            let proj_channels = selected
                .as_ref()
                .and_then(|s| (!s.channels.is_empty()).then(|| s.channels.clone()));
            let _first_seq =
                shared
                    .fanout
                    .add_shm_with_peer(Arc::clone(&writer), peer_label, proj_channels);
            let _guard = RegisteredShm {
                fanout: &shared.fanout,
                writer: Arc::clone(&writer),
            };
            let mut meta_cursor = 0u64;
            let mut meta_buf = Vec::new();
            let mut meta_wire = Vec::new();
            if let Some((gen, snap)) = initial_meta {
                meta_cursor = gen;
                if !snap.is_empty() {
                    let mut vals: Vec<_> = snap.into_values().collect();
                    vals.sort_by_key(|u| u.seq);
                    for u in &vals {
                        labstream_proto::meta::write_record(&mut meta_wire, u);
                    }
                    if stream.write_all(&meta_wire).is_err() || stream.flush().is_err() {
                        let _ = stream.shutdown(std::net::Shutdown::Both);
                        return Ok(());
                    }
                    meta_wire.clear();
                }
            }
            let poll_timeout = if meta_enabled {
                Duration::from_millis(25)
            } else {
                Duration::from_millis(200)
            };
            let _ = stream.set_read_timeout(Some(poll_timeout));
            let mut probe = [0u8; 64];
            loop {
                if *shared.stop.lock().unwrap() || writer.is_closed() {
                    break;
                }
                if meta_enabled {
                    meta_buf.clear();
                    shared
                        .fanout
                        .read_metadata_since(&mut meta_cursor, &mut meta_buf);
                    if !meta_buf.is_empty() {
                        meta_wire.clear();
                        for u in &meta_buf {
                            labstream_proto::meta::write_record(&mut meta_wire, u);
                        }
                        if stream.write_all(&meta_wire).is_err() || stream.flush().is_err() {
                            break;
                        }
                    }
                }
                match stream.read(&mut probe) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(ref e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut => {}
                    Err(_) => break,
                }
            }
            let _ = stream.shutdown(std::net::Shutdown::Both);
            return Ok(());
        } else if trace {
            eprintln!(
                "TRACE shm: fallback to TCP for segment {:016x}",
                writer.id()
            );
        }
        // `writer` drops here, unlinking the unused SHM segment immediately.
    }

    // The header carries a count of samples, already converted from whatever
    // unit the peer's API used. SPEC.md 8.6.
    let (consumer, resend) = match resumed {
        Some((c, r)) => (c, Some(r)),
        None if resume_fresh_for_shm => {
            let (queue, n) = shared
                .fanout
                .add_resumable_with_peer(accepted.max_buffer_length.max(1) as usize, peer_label);
            (
                Registered {
                    fanout: &shared.fanout,
                    queue,
                },
                Some(Resend {
                    from: None,
                    until: n,
                }),
            )
        }
        None if decimating => (
            Registered {
                fanout: &shared.fanout,
                queue: shared
                    .fanout
                    .add_resumable_with_peer(accepted.max_buffer_length.max(1) as usize, peer_label)
                    .0,
            },
            None,
        ),
        None => (
            Registered {
                fanout: &shared.fanout,
                queue: shared
                    .fanout
                    .add_with_peer(accepted.max_buffer_length.max(1) as usize, peer_label),
            },
            None,
        ),
    };

    // A chunk decides when one write happens. It is not a wire unit. SPEC.md 9.
    // A value of zero means no limit (`src/tcp_server.cpp:749-753`). This
    // outlet has no chunk size of its own.
    let max_chunk = if accepted.max_chunk_length > 0 {
        accepted.max_chunk_length as usize
    } else {
        usize::MAX
    };

    // The inlet writes its requests on TCP. A thread reads them.
    let dgram = match dgram_socket {
        Some((socket, cookie, peer)) => Some(crate::dgram_out::Setup {
            socket,
            cookie,
            peer,
            requests: crate::dgram_out::read_requests(
                stream.try_clone()?,
                Arc::clone(&consumer.queue),
            ),
            queue: Arc::clone(&consumer.queue),
            tcp: stream.try_clone()?,
        }),
        None => None,
    };

    let selector = selected.map(|sel| {
        labstream_proto::select::StreamSelector::new(sel, info.format, info.channel_count as usize)
    });

    let meta_feed = initial_meta.map(|(gen, snap)| (&shared.fanout, gen, snap));
    let stop = || *shared.stop.lock().unwrap();
    let sent = if let Some(key) = enc_key {
        let mut enc_stream = crate::enc::EncWriter::new(&mut stream, &key)?;
        // Send an empty authenticated frame immediately so mismatched PSKs are
        // caught at connect time even on a quiet stream.
        enc_stream.write_frame(&[])?;
        enc_stream.flush()?;
        feed_with(
            &consumer.queue,
            &data_codec,
            &mut enc_stream,
            max_chunk,
            numbered,
            compressed,
            meta_feed,
            resend.as_ref().map(|r| (&shared.fanout, r)),
            dgram,
            selector,
            &stop,
            trace,
        )
    } else {
        feed_with(
            &consumer.queue,
            &data_codec,
            &mut stream,
            max_chunk,
            numbered,
            compressed,
            meta_feed,
            resend.as_ref().map(|r| (&shared.fanout, r)),
            dgram,
            selector,
            &stop,
            trace,
        )
    };
    // Shut down the shared TCP socket so the `dgram` request reader exits
    // immediately and the peer receives FIN without waiting for a watchdog.
    let _ = stream.shutdown(std::net::Shutdown::Both);
    if trace {
        eprintln!("TRACE session over after {sent} samples");
    }
    Ok(())
}

/// A consumer queue in the fanout. The drop removes it, also when a write
/// fails before the session starts.
struct Registered<'a> {
    fanout: &'a Fanout,
    queue: Arc<SampleQueue>,
}

impl Drop for Registered<'_> {
    fn drop(&mut self) {
        self.fanout.remove(&self.queue);
    }
}

/// An active `shm/1` consumer ring in the fanout. Removing it on drop closes
/// the segment and unregisters it from future pushes.
struct RegisteredShm<'a> {
    fanout: &'a Fanout,
    writer: Arc<crate::shm::ShmWriter>,
}

impl Drop for RegisteredShm<'_> {
    fn drop(&mut self) {
        self.writer.close();
        self.fanout.remove_shm(&self.writer);
    }
}

/// What a `resume` connection sends from the history.
#[derive(Debug, Clone, Copy)]
struct Resend {
    /// The number `R` from the inlet, or `None` when nothing goes again.
    from: Option<u64>,
    /// `N`: the first number of the queue. The history sends only numbers
    /// below it.
    until: u64,
}

/// The largest count of samples that one read of the history copies. A push
/// waits for at most one such copy. `docs/extensions.md`, resend rule 6.
pub(crate) const RESEND_BLOCK: usize = 256;

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
/// When `numbered` is true, the consumer agreed to `seq/1`. A numbering
/// record then goes before the first sample, and before each sample whose
/// number does not follow the one before it. A record does not count toward
/// the chunk size. `docs/extensions.md`.
///
/// The result is the number of samples written.
#[cfg(test)]
fn feed<W: Write>(
    queue: &SampleQueue,
    codec: &Codec,
    out: &mut W,
    max_chunk: usize,
    numbered: bool,
    stop: &dyn Fn() -> bool,
    trace: bool,
) -> usize {
    feed_with(
        queue, codec, out, max_chunk, numbered, false, None, None, None, None, stop, trace,
    )
}

/// Copy the samples of one consumer queue to its socket, with an optional
/// resend for the `resume` feature and optional channel/rate selection
/// (`select/1`) and lossless compression (`comp/1`).
///
/// With a resend, the connection first sends the history samples from `R`
/// up to `N - 1`, in blocks, each sample with its full timestamp. Then it
/// sends the queue. The first sample of the connection, and each sample
/// whose number does not follow the number before it, also carry the full
/// timestamp. `docs/extensions.md`, section `resume`.
#[allow(clippy::too_many_arguments)]
fn feed_with<W: Write>(
    queue: &SampleQueue,
    codec: &Codec,
    out: &mut W,
    max_chunk: usize,
    numbered: bool,
    compressed: bool,
    meta_init: Option<(
        &Fanout,
        u64,
        std::collections::BTreeMap<String, labstream_proto::meta::MetadataUpdate>,
    )>,
    resend: Option<(&Fanout, &Resend)>,
    dgram: Option<crate::dgram_out::Setup>,
    mut selector: Option<labstream_proto::select::StreamSelector>,
    stop: &dyn Fn() -> bool,
    trace: bool,
) -> usize {
    let decimating = selector.as_ref().is_some_and(|s| s.param().decimate > 1);
    let mut numbering = numbered.then(Numbering::new);
    let mut tcp_comp = (compressed && labstream_proto::comp::CompState::supports(codec.format))
        .then(|| labstream_proto::comp::CompState::new(codec.format, codec.channels));
    // The first history block decides the number of the first record.
    let mut history: Vec<PackedSample> = Vec::new();
    let mut block = match resend {
        Some((fanout, r)) => r
            .from
            .and_then(|from| fanout.read_history_packed(from, r.until, RESEND_BLOCK, &mut history)),
        None => None,
    };
    // The first record goes out at once, before any sample. A gap before
    // this connection is then visible on a quiet stream too.
    if let Some(n) = numbering.as_mut() {
        let next = if decimating {
            0
        } else {
            match block {
                Some((first, _)) => first,
                None => {
                    queue
                        .pop_many_numbered_packed(&mut Vec::new(), 0, Duration::ZERO)
                        .1
                }
            }
        };
        let mut record = Vec::new();
        n.start(next, &mut record);
        if out.write_all(&record).is_err() {
            return 0;
        }
        let _ = out.flush();
    }
    let mut meta_cursor: u64 = 0;
    let mut meta_queue: std::collections::VecDeque<labstream_proto::meta::MetadataUpdate> =
        std::collections::VecDeque::new();
    let mut meta_scratch: Vec<labstream_proto::meta::MetadataUpdate> = Vec::new();
    let meta_fanout = meta_init.as_ref().map(|(f, _, _)| *f);
    if let Some((_, gen, snap)) = meta_init {
        meta_cursor = gen;
        if !snap.is_empty() {
            let mut vals: Vec<_> = snap.into_values().collect();
            vals.sort_by_key(|u| u.seq);
            let mut init_meta = Vec::new();
            for u in &vals {
                labstream_proto::meta::write_record(&mut init_meta, u);
            }
            if out.write_all(&init_meta).is_err() {
                return 0;
            }
            let _ = out.flush();
        }
    }
    let mut batch: Vec<PackedSample> = Vec::new();
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

    let proj_channels: Option<Vec<u16>> = selector
        .as_ref()
        .and_then(|s| (!s.param().channels.is_empty()).then(|| s.param().channels.clone()));

    // The resend. Each history sample has its full timestamp.
    let mut last: Option<u64> = None;
    while let Some((first, taken)) = block {
        if stop() {
            return sent;
        }
        for (i, s) in history.iter().enumerate() {
            let number = first.wrapping_add(i as u64);
            if let Some(n) = numbering.as_mut() {
                n.before(number, &mut pending);
            }
            let enc = match (proj_channels.as_deref(), tcp_comp.as_mut()) {
                (Some(chs), Some(c)) => {
                    let p = labstream_proto::select::project_packed(s, chs);
                    c.encode_tcp_sample(codec, &p, p.timestamp, &mut pending)
                }
                (None, Some(c)) => c.encode_tcp_sample(codec, s, s.timestamp, &mut pending),
                (Some(chs), None) => {
                    let p = labstream_proto::select::project_packed(s, chs);
                    codec.encode_packed(&p, &mut pending)
                }
                (None, None) => codec.encode_packed(s, &mut pending),
            };
            if enc.is_err() {
                return sent;
            }
            last = Some(number);
            count += 1;
            if count >= max_chunk && !write(&mut pending, &mut count, &mut sent) {
                return sent;
            }
        }
        // Each block goes out at once. The resend thus holds at most one
        // block of bytes, and a dead socket ends it after one block.
        if !pending.is_empty() && !write(&mut pending, &mut count, &mut sent) {
            return sent;
        }
        history.clear();
        let (fanout, r) = resend.expect("a block comes only from a resend");
        let next = first.wrapping_add(taken as u64);
        block = fanout.read_history_packed(next, r.until, RESEND_BLOCK, &mut history);
    }
    if !pending.is_empty() && !write(&mut pending, &mut count, &mut sent) {
        return sent;
    }
    if trace && last.is_some() {
        eprintln!("TRACE resume: resent {sent} samples");
    }

    // A `dgram` connection runs its own live part. It needs `seq` and
    // `resume`, so the numbering and the fanout exist.
    // The numbering moves to that part only when it runs.
    if let (Some(setup), true, Some((fanout, _))) = (dgram, numbering.is_some(), resend) {
        let numbering = numbering.take().expect("checked above");
        return sent
            + crate::dgram_out::Live {
                queue,
                fanout,
                codec,
                channels: proj_channels,
                compressed: tcp_comp.is_some(),
                meta_enabled: meta_fanout.is_some(),
                meta_cursor,
                out,
                numbering,
                last,
                tcp_comp,
                setup,
                trace,
            }
            .run(stop);
    }

    let mut full = Vec::new();
    let mut prev_raw: Option<u64> = None;
    let mut decimated_seq: u64 = 0;
    loop {
        if stop() || queue.is_closed() {
            break;
        }
        batch.clear();
        full.clear();
        let (taken, first) = if resend.is_some() || decimating {
            queue.pop_many_full_packed(
                &mut batch,
                &mut full,
                usize::MAX,
                Duration::from_millis(200),
            )
        } else {
            queue.pop_many_numbered_packed(&mut batch, usize::MAX, Duration::from_millis(200))
        };
        if taken == 0 {
            if let Some(f) = meta_fanout {
                meta_scratch.clear();
                f.read_metadata_since(&mut meta_cursor, &mut meta_scratch);
                meta_queue.extend(meta_scratch.drain(..));
                if !meta_queue.is_empty() {
                    while let Some(u) = meta_queue.pop_front() {
                        labstream_proto::meta::write_record(&mut pending, &u);
                    }
                    if !write(&mut pending, &mut count, &mut sent) {
                        return sent;
                    }
                }
            }
            continue;
        }
        if let Some(f) = meta_fanout {
            meta_scratch.clear();
            f.read_metadata_since(&mut meta_cursor, &mut meta_scratch);
            meta_queue.extend(meta_scratch.drain(..));
        }
        if decimating {
            let sel = selector.as_mut().expect("decimating requires selector");
            for (i, s) in batch.iter().enumerate() {
                let raw_num = first.wrapping_add(i as u64);
                while let Some(front) = meta_queue.front() {
                    if front.seq <= raw_num {
                        let u = meta_queue.pop_front().unwrap();
                        labstream_proto::meta::write_record(&mut pending, &u);
                    } else {
                        break;
                    }
                }
                if let Some(prev) = prev_raw {
                    let expected = prev.wrapping_add(1);
                    if raw_num > expected {
                        let raw_lost = raw_num - expected;
                        decimated_seq =
                            decimated_seq.wrapping_add(raw_lost / sel.param().decimate as u64);
                    }
                }
                prev_raw = Some(raw_num);
                if let Some(emitted) = sel.push(s, full[i]) {
                    if let Some(n) = numbering.as_mut() {
                        n.before(decimated_seq, &mut pending);
                    }
                    decimated_seq = decimated_seq.wrapping_add(1);
                    let enc = match tcp_comp.as_mut() {
                        Some(c) => {
                            c.encode_tcp_sample(codec, &emitted, emitted.timestamp, &mut pending)
                        }
                        None => codec.encode_packed(&emitted, &mut pending),
                    };
                    if enc.is_err() {
                        return sent;
                    }
                    count += 1;
                    if count >= max_chunk && !write(&mut pending, &mut count, &mut sent) {
                        return sent;
                    }
                }
            }
        } else {
            for (i, s) in batch.iter().enumerate() {
                let number = first.wrapping_add(i as u64);
                while let Some(front) = meta_queue.front() {
                    if front.seq <= number {
                        let u = meta_queue.pop_front().unwrap();
                        labstream_proto::meta::write_record(&mut pending, &u);
                    } else {
                        break;
                    }
                }
                if let Some(n) = numbering.as_mut() {
                    n.before(number, &mut pending);
                }
                // After a gap, a deduced timestamp has no correct sample before
                // it. `docs/extensions.md`, rule 5 of the resend timestamps.
                let ts = if resend.is_some() {
                    let use_ts = if last.map(|l| l.wrapping_add(1)) != Some(number) {
                        full[i]
                    } else {
                        s.timestamp
                    };
                    last = Some(number);
                    use_ts
                } else {
                    s.timestamp
                };
                let enc = match (proj_channels.as_deref(), tcp_comp.as_mut()) {
                    (Some(chs), Some(c)) => {
                        let p = labstream_proto::select::project_packed(s, chs);
                        c.encode_tcp_sample(codec, &p, ts, &mut pending)
                    }
                    (None, Some(c)) => c.encode_tcp_sample(codec, s, ts, &mut pending),
                    (Some(chs), None) => {
                        let p = labstream_proto::select::project_packed(s, chs);
                        codec.encode_packed_at(&p, ts, &mut pending)
                    }
                    (None, None) => codec.encode_packed_at(s, ts, &mut pending),
                };
                if enc.is_err() {
                    return sent;
                }
                count += 1;
                if count >= max_chunk && !write(&mut pending, &mut count, &mut sent) {
                    return sent;
                }
            }
        }
        // Flush any metadata updates emitted after the last sample in this batch.
        while let Some(u) = meta_queue.pop_front() {
            labstream_proto::meta::write_record(&mut pending, &u);
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
    pub fn push_samples(&self, samples: Vec<Sample>) {
        if samples.is_empty() {
            return;
        }
        let packed: Vec<PackedSample> = samples.into_iter().map(PackedSample::from).collect();
        self.push_samples_packed(packed);
    }

    /// Send several [`PackedSample`]s, each with its own timestamp, with one lock.
    pub fn push_samples_packed(&self, mut samples: Vec<PackedSample>) {
        if samples.is_empty() {
            return;
        }
        for s in &mut samples {
            s.timestamp = self.stamp(s.timestamp);
        }
        if self.shared.sync_mode {
            write_blocking_many_packed(&self.shared, &samples);
            return;
        }
        self.shared.fanout.push_many_packed(&samples);
    }

    /// Compute the timestamp of the first sample in a chunk of `count` samples.
    fn chunk_first_timestamp(&self, count: usize, timestamp: f64) -> f64 {
        let mut first = if timestamp == 0.0 {
            crate::clock()
        } else {
            timestamp
        };
        if self.info.nominal_srate != 0.0 && count > 1 {
            first -= (count - 1) as f64 / self.info.nominal_srate;
        }
        first
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
        let first = self.chunk_first_timestamp(samples.len(), timestamp);
        let block: Vec<PackedSample> = samples
            .iter()
            .enumerate()
            .map(|(k, s)| {
                let t = if k == 0 {
                    first
                } else {
                    labstream_wire::DEDUCED_TIMESTAMP
                };
                PackedSample::from_sample(s).with_timestamp(t)
            })
            .collect();
        self.push_samples_packed(block);
    }

    /// Send several [`PackedSample`]s that share one end timestamp.
    pub fn push_chunk_packed(&self, samples: &[PackedSample], timestamp: f64) {
        if samples.is_empty() {
            return;
        }
        let first = self.chunk_first_timestamp(samples.len(), timestamp);
        let block: Vec<PackedSample> = samples
            .iter()
            .enumerate()
            .map(|(k, s)| {
                let t = if k == 0 {
                    first
                } else {
                    labstream_wire::DEDUCED_TIMESTAMP
                };
                s.with_timestamp(t)
            })
            .collect();
        self.push_samples_packed(block);
    }

    /// Send a flat interleaved chunk of `f32` channel values sharing `timestamp`.
    pub fn push_chunk_f32(&self, data: &[f32], timestamp: f64) {
        self.push_chunk_flat(data, timestamp, PackedSample::from_f32);
    }

    /// Send a flat interleaved chunk of `f64` channel values sharing `timestamp`.
    pub fn push_chunk_f64(&self, data: &[f64], timestamp: f64) {
        self.push_chunk_flat(data, timestamp, PackedSample::from_f64);
    }

    /// Send a flat interleaved chunk of `i32` channel values sharing `timestamp`.
    pub fn push_chunk_i32(&self, data: &[i32], timestamp: f64) {
        self.push_chunk_flat(data, timestamp, PackedSample::from_i32);
    }

    /// Send a flat interleaved chunk of `i16` channel values sharing `timestamp`.
    pub fn push_chunk_i16(&self, data: &[i16], timestamp: f64) {
        self.push_chunk_flat(data, timestamp, PackedSample::from_i16);
    }

    /// Send a flat interleaved chunk of `i8` channel values sharing `timestamp`.
    pub fn push_chunk_i8(&self, data: &[i8], timestamp: f64) {
        self.push_chunk_flat(data, timestamp, PackedSample::from_i8);
    }

    /// Send a flat interleaved chunk of `i64` channel values sharing `timestamp`.
    pub fn push_chunk_i64(&self, data: &[i64], timestamp: f64) {
        self.push_chunk_flat(data, timestamp, PackedSample::from_i64);
    }

    fn push_chunk_flat<T>(&self, data: &[T], timestamp: f64, pack: fn(f64, &[T]) -> PackedSample) {
        let chans = self.info.channel_count as usize;
        if chans == 0 || data.is_empty() || data.len() % chans != 0 {
            return;
        }
        let count = data.len() / chans;
        let first = self.chunk_first_timestamp(count, timestamp);
        let block: Vec<PackedSample> = data
            .chunks_exact(chans)
            .enumerate()
            .map(|(k, row)| {
                let t = if k == 0 {
                    first
                } else {
                    labstream_wire::DEDUCED_TIMESTAMP
                };
                pack(t, row)
            })
            .collect();
        self.push_samples_packed(block);
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
        let sent = feed(queue, &codec, &mut out, max_chunk, false, &stop, false);
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

    /// Feed a numbered queue once, and read the records and samples back.
    fn run_numbered(
        queue: &SampleQueue,
        max_chunk: usize,
        order: ByteOrder,
    ) -> (Vec<Sample>, Vec<u64>, Counting) {
        use labstream_proto::seq::{peek, Next, RECORD_LEN};
        let codec = Codec::new(Format::Float32, 2, order);
        let mut out = Counting::default();
        let once = AtomicBool::new(false);
        let stop = || once.swap(true, Ordering::SeqCst);
        feed(queue, &codec, &mut out, max_chunk, true, &stop, false);
        let mut pos = 0;
        let (mut got, mut records) = (Vec::new(), Vec::new());
        while pos < out.bytes.len() {
            if let Next::Record(n) = peek(&out.bytes[pos..]) {
                records.push(n);
                pos += RECORD_LEN;
                continue;
            }
            let (s, used) = codec.decode(&out.bytes[pos..]).unwrap();
            got.push(s);
            pos += used;
        }
        (got, records, out)
    }

    #[test]
    fn a_numbered_feed_starts_with_a_record() {
        for order in [ByteOrder::Little, ByteOrder::Big] {
            let q = SampleQueue::with_first_number(1000, 500);
            let sent = samples(20);
            q.push_many(&sent);
            let (got, records, _) = run_numbered(&q, usize::MAX, order);
            assert_eq!(got, sent);
            assert_eq!(records, vec![500], "one record, at the start");
        }
    }

    #[test]
    fn a_full_queue_shows_as_a_gap_in_the_numbers() {
        // A queue of 4 that gets 10 samples keeps numbers 106 to 109.
        let q = SampleQueue::with_first_number(4, 100);
        q.push_many(&samples(10));
        let (got, records, _) = run_numbered(&q, usize::MAX, ByteOrder::native());
        assert_eq!(got.len(), 4);
        assert_eq!(records, vec![106]);
    }

    #[test]
    fn a_record_does_not_count_toward_the_chunk_size() {
        let q = SampleQueue::with_first_number(1000, 7);
        q.push_many(&samples(32));
        let (got, records, out) = run_numbered(&q, 16, ByteOrder::native());
        assert_eq!(got.len(), 32);
        assert_eq!(records, vec![7]);
        // The opening record is a write of its own, at the start of the
        // connection. Then 16 samples go in each write.
        assert_eq!(out.writes, 3, "the record, then two chunks of 16");
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

    // The `resume` resend. `docs/extensions.md`, section `resume`.

    const RATE: f64 = 300.0;

    /// Chunks of `len` samples, as `Outlet::push_chunk` makes them: a
    /// timestamp on the first sample, and the deduced tag on the others.
    fn push_chunks(f: &Fanout, from: i32, to: i32, len: i32) {
        let mut k = from;
        while k < to {
            let end = (k + len).min(to);
            let block: Vec<Sample> = (k..end)
                .map(|i| Sample {
                    timestamp: if i == k {
                        1000.0 + i as f64 / RATE + 0.000_123
                    } else {
                        labstream_wire::DEDUCED_TIMESTAMP
                    },
                    values: vec![Value::I32(i)],
                })
                .collect();
            f.push_many(&block);
            k = end;
        }
    }

    /// A plain queue that gets every push, for a reference.
    fn witness(f: &Fanout) -> Arc<SampleQueue> {
        f.add(usize::MAX)
    }

    /// The full timestamp of each sample that the witness got, as an inlet
    /// with no loss calculates it. The index is the value of the sample.
    fn reference(witness: &SampleQueue) -> Vec<u64> {
        let mut out = Vec::new();
        witness.pop_many(&mut out, usize::MAX, Duration::ZERO);
        let mut d = labstream_wire::TimestampDeducer::new(RATE);
        out.iter().map(|s| d.apply(s.timestamp).to_bits()).collect()
    }

    struct Received {
        /// The value, the wire timestamp, and the full timestamp of each
        /// sample, as an inlet reads them.
        samples: Vec<(i32, f64, u64)>,
        records: Vec<u64>,
    }

    /// Run one resume connection once, and read it back as an inlet does.
    fn run_resume(f: &Fanout, queue: &SampleQueue, resend: Resend) -> Received {
        // The resend asks once, and must go on. Then the live part stops
        // when the queue is empty.
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let stop = || calls.fetch_add(1, Ordering::SeqCst) >= 1 && queue.is_empty();
        run_resume_with(f, queue, resend, &stop).0
    }

    /// [`run_resume`] with the stop function of the caller. The result also
    /// gives the count of writes.
    fn run_resume_with(
        f: &Fanout,
        queue: &SampleQueue,
        resend: Resend,
        stop: &dyn Fn() -> bool,
    ) -> (Received, usize) {
        use labstream_proto::seq::{peek, Next, RECORD_LEN};
        let codec = Codec::new(Format::Int32, 1, ByteOrder::native());
        let mut out = Counting::default();
        feed_with(
            queue,
            &codec,
            &mut out,
            usize::MAX,
            true,
            false,
            None,
            Some((f, &resend)),
            None,
            None,
            stop,
            false,
        );
        let mut deducer = labstream_wire::TimestampDeducer::new(RATE);
        let mut got = Received {
            samples: Vec::new(),
            records: Vec::new(),
        };
        let mut pos = 0;
        while pos < out.bytes.len() {
            if let Next::Record(n) = peek(&out.bytes[pos..]) {
                got.records.push(n);
                pos += RECORD_LEN;
                continue;
            }
            let (s, used) = codec.decode(&out.bytes[pos..]).unwrap();
            let v = match s.values[0] {
                Value::I32(v) => v,
                _ => unreachable!(),
            };
            let full = deducer.apply(s.timestamp).to_bits();
            got.samples.push((v, s.timestamp, full));
            pos += used;
        }
        (got, out.writes)
    }

    #[test]
    fn a_resend_gives_each_missing_sample_once_with_its_timestamp() {
        let f = Fanout::with_rate(RATE, crate::history::HistoryLimits::DEFAULT);
        f.start_history();
        let all = witness(&f);
        push_chunks(&f, 0, 30, 9);
        let (queue, n) = f.add_resumable(1000);
        assert_eq!(n, 30);
        push_chunks(&f, 30, 45, 9);

        // Sample 10 is in the middle of a chunk.
        let got = run_resume(
            &f,
            &queue,
            Resend {
                from: Some(10),
                until: n,
            },
        );
        assert_eq!(got.records, vec![10], "the numbers have no gap");
        let values: Vec<i32> = got.samples.iter().map(|s| s.0).collect();
        assert_eq!(values, (10..45).collect::<Vec<_>>());

        let want = reference(&all);
        for &(v, wire, full) in &got.samples {
            if v < 30 {
                assert_ne!(wire, labstream_wire::DEDUCED_TIMESTAMP, "resent sample {v}");
            }
            assert_eq!(full, want[v as usize], "sample {v}");
        }
        // The live samples keep the tags of their push. Sample 30 starts a
        // chunk, and sample 31 does not.
        let tag = |v: i32| got.samples.iter().find(|s| s.0 == v).unwrap().1;
        assert_ne!(tag(30), labstream_wire::DEDUCED_TIMESTAMP);
        assert_eq!(tag(31), labstream_wire::DEDUCED_TIMESTAMP);
    }

    #[test]
    fn a_resend_starts_at_the_oldest_sample_when_the_history_is_too_short() {
        let limits = crate::history::HistoryLimits {
            seconds: 20.0 / RATE,
            bytes: usize::MAX,
            ..crate::history::HistoryLimits::DEFAULT
        };
        let f = Fanout::with_rate(RATE, limits);
        f.start_history();
        let all = witness(&f);
        push_chunks(&f, 0, 50, 7);
        let (queue, n) = f.add_resumable(1000);
        let got = run_resume(
            &f,
            &queue,
            Resend {
                from: Some(5),
                until: n,
            },
        );
        // The inlet counts 30 - 5 lost samples from this record.
        assert_eq!(got.records, vec![30]);
        assert_eq!(got.samples.first().map(|s| s.0), Some(30));
        assert_eq!(got.samples.len(), 20);
        let want = reference(&all);
        for &(v, _, full) in &got.samples {
            assert_eq!(full, want[v as usize], "sample {v}");
        }
    }

    #[test]
    fn a_connection_with_nothing_to_resend_starts_at_the_queue() {
        // No number, a number equal to N, and a number above N.
        for from in [None, Some(20), Some(500)] {
            let f = Fanout::with_rate(RATE, crate::history::HistoryLimits::DEFAULT);
            f.start_history();
            let all = witness(&f);
            push_chunks(&f, 0, 20, 9);
            let (queue, n) = f.add_resumable(1000);
            assert_eq!(n, 20);
            // Sample 20 is in the middle of a chunk that starts at 18.
            push_chunks(&f, 20, 30, 9);
            let got = run_resume(&f, &queue, Resend { from, until: n });
            assert_eq!(got.records, vec![20], "from {from:?}");
            let values: Vec<i32> = got.samples.iter().map(|s| s.0).collect();
            assert_eq!(values, (20..30).collect::<Vec<_>>(), "from {from:?}");
            // The first sample of the connection carries its full timestamp.
            assert_ne!(got.samples[0].1, labstream_wire::DEDUCED_TIMESTAMP);
            let want = reference(&all);
            for &(v, _, full) in &got.samples {
                assert_eq!(full, want[v as usize], "from {from:?}, sample {v}");
            }
        }
    }

    #[test]
    fn a_drop_in_the_queue_gives_a_full_timestamp_after_the_gap() {
        let f = Fanout::with_rate(RATE, crate::history::HistoryLimits::DEFAULT);
        f.start_history();
        let all = witness(&f);
        let (queue, n) = f.add_resumable(5);
        assert_eq!(n, 0);
        // One chunk of 12 into a queue of 5. The queue keeps 7 to 11, and the
        // only explicit timestamp, on sample 0, is gone.
        push_chunks(&f, 0, 12, 12);
        let got = run_resume(
            &f,
            &queue,
            Resend {
                from: None,
                until: n,
            },
        );
        // The queue dropped 0 to 6 before the first record.
        assert_eq!(got.records, vec![7]);
        let values: Vec<i32> = got.samples.iter().map(|s| s.0).collect();
        assert_eq!(values, vec![7, 8, 9, 10, 11]);
        assert_ne!(got.samples[0].1, labstream_wire::DEDUCED_TIMESTAMP);
        let want = reference(&all);
        for &(v, _, full) in &got.samples {
            assert_eq!(full, want[v as usize], "sample {v}");
        }
    }

    #[test]
    fn a_long_resend_goes_out_block_by_block() {
        let f = Fanout::with_rate(RATE, crate::history::HistoryLimits::DEFAULT);
        f.start_history();
        let all = witness(&f);
        push_chunks(&f, 0, 1000, 9);
        let (queue, n) = f.add_resumable(5000);
        push_chunks(&f, 1000, 1010, 9);
        let calls = std::sync::atomic::AtomicUsize::new(0);
        // Four blocks of the resend ask first. Then the live part stops when
        // the queue is empty.
        let stop = || calls.fetch_add(1, Ordering::SeqCst) >= 4 && queue.is_empty();
        let (got, writes) = run_resume_with(
            &f,
            &queue,
            Resend {
                from: Some(0),
                until: n,
            },
            &stop,
        );
        assert_eq!(got.records, vec![0]);
        let values: Vec<i32> = got.samples.iter().map(|s| s.0).collect();
        assert_eq!(values, (0..1010).collect::<Vec<_>>());
        // The record, four history blocks, and the live samples.
        assert_eq!(writes, 6);
        let want = reference(&all);
        for &(v, _, full) in &got.samples {
            assert_eq!(full, want[v as usize], "sample {v}");
        }
    }

    #[test]
    fn samples_that_leave_the_history_during_a_resend_show_as_gaps() {
        // A history of 600 samples.
        let limits = crate::history::HistoryLimits {
            seconds: 2.0,
            bytes: usize::MAX,
            ..crate::history::HistoryLimits::DEFAULT
        };
        let f = Fanout::with_rate(RATE, limits);
        f.start_history();
        let all = witness(&f);
        push_chunks(&f, 0, 600, 9);
        let (queue, n) = f.add_resumable(5000);
        assert_eq!(n, 600);
        // Before the first two blocks go out, the outlet pushes 300 more
        // samples each time. The history then loses its oldest samples.
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let stop = || {
            let c = calls.fetch_add(1, Ordering::SeqCst);
            if c < 2 {
                let from = 600 + 300 * c as i32;
                push_chunks(&f, from, from + 300, 9);
                false
            } else {
                queue.is_empty()
            }
        };
        let (got, _) = run_resume_with(
            &f,
            &queue,
            Resend {
                from: Some(0),
                until: n,
            },
            &stop,
        );
        // Block 0 was read before the first removal. The second read starts
        // at the oldest sample, 300. The third read finds nothing below 600.
        assert_eq!(got.records, vec![0, 300, 600]);
        let mut want_values: Vec<i32> = (0..256).collect();
        want_values.extend(300..556);
        want_values.extend(600..1200);
        let values: Vec<i32> = got.samples.iter().map(|s| s.0).collect();
        assert_eq!(values, want_values);
        // The first sample after each gap carries its full timestamp, so each
        // timestamp is correct.
        let want = reference(&all);
        for &(v, wire, full) in &got.samples {
            assert_eq!(full, want[v as usize], "sample {v}");
            if v == 600 {
                assert_ne!(wire, labstream_wire::DEDUCED_TIMESTAMP);
            }
        }
    }
}
