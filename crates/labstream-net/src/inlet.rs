//! The inlet: a resolver and a TCP feed client.

use crate::clock::{ClockState, LiveSource};
use crate::config;
use crate::info::StreamInfo;
use crate::queue::SampleQueue;
use labstream_proto::discovery;
use labstream_proto::feed::{FeedClient, FeedParams, State, TestPatternError};
use labstream_time::{flags, PostProcessor};
use labstream_wire::Sample;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Send a discovery query and collect the answers. SPEC.md 2.
///
/// The query identifier is an opaque token. This function builds one, sends
/// it, and drops any answer that carries a different value. It never derives
/// the value of another implementation. SPEC.md 2.3.
pub fn resolve(query: &str, minimum: usize, timeout: Duration) -> std::io::Result<Vec<StreamInfo>> {
    let mut wave = QueryWave::open()?;
    let end = Instant::now() + timeout;
    let mut found: Vec<StreamInfo> = Vec::new();

    while Instant::now() < end {
        wave.send(query);
        let seen: Vec<String> = found.iter().map(|f| f.uid.clone()).collect();
        for info in wave.collect(Duration::from_millis(400), |uid| {
            seen.iter().any(|u| u == uid)
        }) {
            if !found.iter().any(|f| f.uid == info.uid) {
                found.push(info);
            }
        }
        if found.len() >= minimum {
            return Ok(found);
        }
    }
    Ok(found)
}

/// One set of sockets that sends a query and reads the answers.
///
/// A one-shot resolve and a background resolver send the same wave, so the
/// sockets live here and both callers borrow them.
///
/// # One leg for each protocol
///
/// liblsl runs a separate attempt per protocol. Its receiving socket has one
/// protocol, and it sends only to the groups of that protocol
/// (`src/resolve_attempt_udp.cpp:174`). The same shape is used here, because
/// the query carries the port that the answer must return to, and that port
/// belongs to one socket.
pub(crate) struct QueryWave {
    legs: Vec<Leg>,
    /// The token that this wave carries. An answer with a different token
    /// belongs to another resolver. SPEC.md 2.3.
    query_id: String,
    buf: Vec<u8>,
}

struct Leg {
    /// The socket that answers arrive on.
    rx: UdpSocket,
    /// One socket for each local address, plus one bound to any address.
    senders: Vec<UdpSocket>,
    return_port: u16,
    targets: Vec<String>,
}

impl QueryWave {
    pub(crate) fn open() -> std::io::Result<QueryWave> {
        let cfg = config::get();
        let mut legs = Vec::new();
        if cfg.allow_ipv4 {
            if let Ok(leg) = Leg::open(false) {
                legs.push(leg);
            }
        }
        if cfg.allow_ipv6 {
            if let Ok(leg) = Leg::open(true) {
                legs.push(leg);
            }
        }
        if legs.is_empty() {
            return Err(std::io::Error::other(
                "no socket could be opened for a query",
            ));
        }
        Ok(QueryWave {
            legs,
            query_id: crate::make_query_id(),
            buf: vec![0u8; 65536],
        })
    }

    /// Send one wave of queries, on every protocol.
    pub(crate) fn send(&self, query: &str) {
        let cfg = config::get();
        for leg in &self.legs {
            let msg = discovery::build_query(query, leg.return_port, &self.query_id);
            for s in &leg.senders {
                // A multicast or broadcast query goes to the multicast port.
                // SPEC.md 1.1.
                for addr in &leg.targets {
                    let _ = s.send_to(&msg, (addr.as_str(), cfg.multicast_port));
                }
                // A unicast query walks the port range, and reaches every
                // address that the configuration names as a peer.
                for p in cfg.base_port..cfg.base_port + cfg.port_range {
                    for addr in &leg.targets {
                        let _ = s.send_to(&msg, (addr.as_str(), p));
                    }
                    for addr in &cfg.known_peers {
                        let _ = s.send_to(&msg, (addr.as_str(), p));
                    }
                }
            }
        }
    }

    /// Read answers until the time runs out.
    ///
    /// `skip` drops an answer whose identifier the caller already holds.
    pub(crate) fn collect(
        &mut self,
        window: Duration,
        skip: impl Fn(&str) -> bool,
    ) -> Vec<StreamInfo> {
        let mut out: Vec<StreamInfo> = Vec::new();
        let end = Instant::now() + window;
        // Each socket is read in turn, with a short wait, so one quiet
        // protocol does not hold up the other.
        while Instant::now() < end {
            let mut got_any = false;
            for leg in &self.legs {
                while let Ok((n, from)) = leg.rx.recv_from(&mut self.buf) {
                    got_any = true;
                    let answer = match discovery::parse_answer(&self.buf[..n]) {
                        Some(a) => a,
                        None => continue,
                    };
                    // An answer to another query is dropped.
                    if answer.query_id != self.query_id {
                        continue;
                    }
                    if let Some(mut info) = StreamInfo::from_shortinfo_xml(&answer.xml) {
                        stamp_address(&mut info, from);
                        if !skip(&info.uid) && !out.iter().any(|f| f.uid == info.uid) {
                            out.push(info);
                        }
                    }
                }
            }
            if !got_any {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        out
    }
}

/// Write the address that an answer came from into the description.
///
/// An outlet leaves the field empty, and the resolver of the peer fills it in
/// (`src/resolve_attempt_udp.cpp:135-140`). Without this an inlet has no
/// address to connect to.
fn stamp_address(info: &mut StreamInfo, from: SocketAddr) {
    match from {
        SocketAddr::V4(a) => {
            if info.v4address.is_empty() {
                info.v4address = a.ip().to_string();
            }
        }
        SocketAddr::V6(a) => {
            if info.v6address.is_empty() {
                // A link-local address only names a machine together with the
                // interface it arrived on. Without that number a connection
                // fails with an invalid argument. asio writes the same form,
                // and liblsl stores it as it stands
                // (`src/resolve_attempt_udp.cpp:140`).
                info.v6address = if a.scope_id() != 0 {
                    format!("{}%{}", a.ip(), a.scope_id())
                } else {
                    a.ip().to_string()
                };
            }
        }
    }
}

impl Leg {
    fn open(v6: bool) -> std::io::Result<Leg> {
        let cfg = config::get();
        let any: std::net::IpAddr = if v6 {
            std::net::Ipv6Addr::UNSPECIFIED.into()
        } else {
            std::net::Ipv4Addr::UNSPECIFIED.into()
        };
        let rx = UdpSocket::bind((any, 0))?;
        if !v6 {
            rx.set_broadcast(true)?;
        }
        rx.set_read_timeout(Some(Duration::from_millis(40)))?;
        let return_port = rx.local_addr()?.port();

        // A query must leave by every interface that carries multicast,
        // because a peer joins the group on one of them. liblsl walks the same
        // list (`src/resolve_attempt_udp.cpp`, send_next_query).
        //
        // The standard library has no call that selects the outgoing interface
        // for a multicast send. Binding a socket to a local address does the
        // same thing, so this opens one socket for each address.
        let mut senders: Vec<UdpSocket> = Vec::new();
        if !v6 {
            for a in crate::local_ipv4_addresses() {
                if let Ok(s) = UdpSocket::bind((a, 0)) {
                    let _ = s.set_broadcast(true);
                    let _ = s.set_multicast_ttl_v4(cfg.multicast_ttl);
                    let _ = s.set_multicast_loop_v4(true);
                    senders.push(s);
                }
            }
        }
        // The query reaches as far as the scope of the configuration. liblsl
        // sets the same value (`src/resolve_attempt_udp.cpp:45`).
        let one = UdpSocket::bind((any, 0))?;
        if v6 {
            let _ = one.set_multicast_loop_v6(true);
            let _ = socket2::SockRef::from(&one).set_multicast_hops_v6(cfg.multicast_ttl);
        } else {
            one.set_broadcast(true)?;
            let _ = one.set_multicast_ttl_v4(cfg.multicast_ttl);
            let _ = one.set_multicast_loop_v4(true);
        }
        senders.push(one);

        // Only the groups of this protocol.
        let mut targets: Vec<String> = cfg
            .multicast_addresses
            .iter()
            .filter(|a| match a.parse::<std::net::IpAddr>() {
                Ok(std::net::IpAddr::V6(_)) => v6,
                Ok(std::net::IpAddr::V4(_)) => !v6,
                Err(_) => false,
            })
            .cloned()
            .collect();
        if v6 && !targets.iter().any(|t| t == "::1") {
            // The loopback carries no group, so a stream on this machine needs
            // its address named directly.
            targets.push("::1".to_string());
        }

        Ok(Leg {
            rx,
            senders,
            return_port,
            targets,
        })
    }
}

/// Where the data of a stream lives.
///
/// A description that discovery returned carries the address that the answer
/// came from. A description that a program built by hand carries none, and the
/// stream can then only be on this machine.
///
/// liblsl takes the IPv4 endpoint when the description holds one, and the IPv6
/// endpoint otherwise (`src/inlet_connection.cpp:115-125`).
pub(crate) fn data_endpoint(info: &StreamInfo) -> std::io::Result<SocketAddr> {
    endpoint(
        &info.v4address,
        info.v4data_port,
        &info.v6address,
        info.v6data_port,
    )
}

/// The host and port that answer a time probe.
pub(crate) fn service_endpoint(info: &StreamInfo) -> Option<SocketAddr> {
    if !info.v4address.is_empty() && info.v4service_port != 0 {
        return format!("{}:{}", info.v4address, info.v4service_port)
            .parse()
            .ok();
    }
    if !info.v6address.is_empty() && info.v6service_port != 0 {
        return v6_endpoint(&info.v6address, info.v6service_port).ok();
    }
    format!("127.0.0.1:{}", info.v4service_port).parse().ok()
}

fn endpoint(v4: &str, v4port: u16, v6: &str, v6port: u16) -> std::io::Result<SocketAddr> {
    if !v4.is_empty() && v4port != 0 {
        return format!("{v4}:{v4port}")
            .parse()
            .map_err(|_| std::io::Error::other(format!("cannot read the address {v4}")));
    }
    if !v6.is_empty() && v6port != 0 {
        return v6_endpoint(v6, v6port);
    }
    // No address arrived, so the stream can only be on this machine.
    format!("127.0.0.1:{v4port}")
        .parse()
        .map_err(|_| std::io::Error::other("cannot read the loopback address"))
}

/// Build an IPv6 endpoint, with the interface number when the text carries one.
///
/// A link-local address names a machine only together with the interface that
/// reaches it. The text holds that number after a `%`, and the standard
/// library reads neither the `%` nor the number, so both parts are taken
/// apart here.
pub(crate) fn v6_endpoint(text: &str, port: u16) -> std::io::Result<SocketAddr> {
    let (host, scope) = match text.split_once('%') {
        Some((h, s)) => (h, s.parse::<u32>().unwrap_or(0)),
        None => (text, 0),
    };
    let addr: std::net::Ipv6Addr = host
        .parse()
        .map_err(|_| std::io::Error::other(format!("cannot read the address {text}")))?;
    Ok(SocketAddr::V6(std::net::SocketAddrV6::new(
        addr, port, 0, scope,
    )))
}

/// Build the query that liblsl builds for a property.
///
/// `src/resolver_impl.cpp:66-73`.
pub fn build_property_query(session_id: &str, prop: &str, value: &str) -> String {
    format!("session_id='{session_id}' and {prop}='{value}'")
}

/// A connected inlet.
///
/// A background thread reads the socket and decodes samples into a bounded
/// queue. The application pulls from that queue.
///
/// The split matters. A reader that decoded inside `pull` would leave the
/// socket unread while the application worked, so a loss would land in the
/// outlet's queue instead of this one. liblsl runs the same background thread
/// (`src/data_receiver.h`).
pub struct Inlet {
    queue: Arc<SampleQueue>,
    info: StreamInfo,
    clock: Arc<ClockState>,
    source: LiveSource,
    post: PostProcessor,
    /// Set once the reader thread stops.
    reader_done: Arc<AtomicBool>,
    conn: Arc<ConnState>,
}

/// What the reader thread and the application share about the connection.
struct ConnState {
    /// True when the inlet finds the stream again after its source restarts.
    recover: bool,
    /// How many times the connection has been rebuilt.
    recoveries: AtomicU64,
    /// False while the reader is between connections.
    connected: AtomicBool,
    /// The description of the stream that the reader is attached to. A
    /// recovery replaces it, and the instance identifier changes.
    info: Mutex<StreamInfo>,
}

/// How long without data before the reader treats the source as gone.
///
/// liblsl checks on the same period and uses the same threshold
/// (`src/api_config.cpp:302-303`).
fn watchdog_threshold() -> Duration {
    Duration::from_secs_f64(config::get().watchdog_time_threshold)
}

impl Inlet {
    /// Open a feed to a resolved stream. SPEC.md 4 and 6.
    ///
    /// The call runs the whole handshake, including the two test pattern
    /// samples that open every feed.
    pub fn open(info: &StreamInfo, timeout: Duration) -> std::io::Result<Self> {
        Inlet::open_with_buffer(info, timeout, 3600)
    }

    /// Open a feed and choose the size of the queue, in samples.
    ///
    /// The value also goes out in `Max-Buffer-Length`, which carries a count of
    /// samples. A caller that thinks in seconds converts first. SPEC.md 8.6.
    pub fn open_with_buffer(
        info: &StreamInfo,
        timeout: Duration,
        buffer_samples: i32,
    ) -> std::io::Result<Self> {
        Inlet::open_full(info, timeout, buffer_samples, false)
    }

    /// Open a feed and choose whether the inlet finds the stream again after a
    /// restart of its source. SPEC.md 10.1.
    ///
    /// liblsl enables recovery by default (`include/lsl_cpp.h:915`).
    pub fn open_recovering(
        info: &StreamInfo,
        timeout: Duration,
        buffer_samples: i32,
    ) -> std::io::Result<Self> {
        Inlet::open_full(info, timeout, buffer_samples, true)
    }

    fn open_full(
        info: &StreamInfo,
        timeout: Duration,
        buffer_samples: i32,
        recover: bool,
    ) -> std::io::Result<Self> {
        let (stream, client, buf) = handshake(info, timeout, buffer_samples)?;
        debug_assert_eq!(client.state(), State::Streaming);

        // The queue holds what the application has not pulled yet. Its size is
        // the value that went out in `Max-Buffer-Length`, which is a count of
        // samples. SPEC.md 8.6.
        let queue = Arc::new(SampleQueue::new(buffer_samples.max(1) as usize));
        let reader_done = Arc::new(AtomicBool::new(false));

        // The offset thread runs for the life of the inlet. It answers on the
        // service port, which is separate from the data port. SPEC.md 3.
        let clock = Arc::new(ClockState::default());
        let seed = Arc::new(Mutex::new(
            std::process::id() as u64 ^ 0x9E37_79B9_7F4A_7C15,
        ));
        if let Some(target) = service_endpoint(info) {
            crate::clock::start(target, Arc::clone(&clock), seed);
        }

        let conn = Arc::new(ConnState {
            recover,
            recoveries: AtomicU64::new(0),
            connected: AtomicBool::new(true),
            info: Mutex::new(info.clone()),
        });

        // One thread owns the connection for its whole life: it reads, and when
        // the source goes away it finds the stream again and starts over.
        {
            let q = Arc::clone(&queue);
            let done = Arc::clone(&reader_done);
            let clk = Arc::clone(&clock);
            let state = Arc::clone(&conn);
            let codec = *client.codec().expect("the handshake set the codec");
            std::thread::spawn(move || {
                read_and_recover(stream, buf, codec, q, done, clk, state, buffer_samples);
            });
        }

        Ok(Inlet {
            queue,
            reader_done,
            conn,
            info: info.clone(),
            source: LiveSource::new(Arc::clone(&clock), info.nominal_srate),
            clock,
            // No stage runs until a caller asks for one. liblsl uses the same
            // default (`include/lsl/common.h:103`).
            post: PostProcessor::new(),
        })
    }

    /// Choose the post-processing stages. SPEC.md 8.5.
    ///
    /// The default applies none, so a timestamp arrives as the sender wrote it.
    pub fn set_postprocessing(&mut self, options: u32) {
        self.post.set_options(options);
    }

    /// Apply every stage: clock sync, then jitter removal, then the clamp.
    pub fn set_postprocessing_all(&mut self) {
        self.post
            .set_options(flags::CLOCKSYNC | flags::DEJITTER | flags::MONOTONIZE);
    }

    /// The measured offset between the two clocks, or `None` before the first
    /// burst finishes.
    ///
    /// Add this value to a timestamp to move it onto the local clock. The
    /// `CLOCKSYNC` stage does that on its own.
    pub fn time_correction(&self) -> Option<f64> {
        self.clock.offset()
    }

    /// Wait for a first offset measurement.
    pub fn wait_for_time_correction(&self, timeout: Duration) -> Option<f64> {
        self.clock.wait(timeout)
    }

    /// The round-trip time of the estimate that the offset came from.
    pub fn time_uncertainty(&self) -> Option<f64> {
        self.clock.uncertainty()
    }

    /// The description of the connected stream.
    ///
    /// This is the short description that discovery returned. It carries an
    /// empty description tree. Call [`Inlet::fullinfo`] for the tree.
    pub fn info(&self) -> &StreamInfo {
        &self.info
    }

    /// Read the whole description, with the description tree.
    ///
    /// The short description that a discovery answer carries has no tree
    /// (`src/stream_info_impl.cpp:160`). The tree travels on the data port.
    /// This opens its own connection, the way `info_receiver` does
    /// (`src/info_receiver.cpp:60`), and leaves the feed connection alone.
    pub fn fullinfo(&self, timeout: Duration) -> std::io::Result<StreamInfo> {
        read_fullinfo(&self.info, timeout)
    }

    /// Read one sample.
    ///
    /// Returns `Ok(None)` when the timeout passes with no whole sample.
    ///
    /// The timestamp passes through the post-processing stages that
    /// [`Inlet::set_postprocessing`] selected. With none selected the value is
    /// the one that the sender wrote.
    ///
    /// With `CLOCKSYNC` selected, the first call waits for the first offset
    /// measurement, for up to 5 s. The timeout of the call does not limit that
    /// wait. liblsl waits the same time (`src/stream_inlet_impl.h:44`).
    pub fn pull(&mut self, timeout: Duration) -> std::io::Result<Option<Sample>> {
        match self.queue.pop(timeout) {
            Some(mut s) => {
                // Every stage runs here, in the fixed order. SPEC.md 8.5.
                s.timestamp = self.post.process(s.timestamp, &mut self.source);
                Ok(Some(s))
            }
            None => Ok(None),
        }
    }

    /// Read every sample that waits, up to `max`, with one lock.
    ///
    /// The samples go to the end of `out`. The call gives how many it added.
    /// Every timestamp passes through the stages that
    /// [`Inlet::set_postprocessing`] selected, in the order of one
    /// [`Inlet::pull`] for each sample.
    ///
    /// The timeout applies to the first sample only. The call never waits for
    /// `max` samples, because a program that shows live signals must show what
    /// arrived.
    ///
    /// With `CLOCKSYNC` selected, the first call can wait up to 5 s for the
    /// first offset measurement, as [`Inlet::pull`] does.
    ///
    /// This is the call for a fast stream. [`Inlet::pull`] takes the queue lock
    /// and reads the clock for each sample. A stream of 8 channels at 1000 Hz
    /// therefore costs 1000 locks each second with `pull`, and one lock for each
    /// block here.
    pub fn pull_chunk(
        &mut self,
        out: &mut Vec<Sample>,
        max: usize,
        timeout: Duration,
    ) -> std::io::Result<usize> {
        let first = out.len();
        let got = self.queue.pop_many(out, max, timeout);
        // Every stage runs here, in the fixed order, one sample after another.
        // SPEC.md 8.5. The state of the filter carries across the block, so a
        // block gives the timestamps that the same samples get one at a time.
        for s in &mut out[first..] {
            s.timestamp = self.post.process(s.timestamp, &mut self.source);
        }
        Ok(got)
    }

    /// How many samples wait for the application.
    pub fn samples_available(&self) -> usize {
        self.queue.len()
    }

    /// How many samples the queue has dropped.
    ///
    /// Nothing on the wire carries this number, so a peer cannot learn it.
    /// SPEC.md 8.7. This exists for a local report only.
    pub fn samples_dropped(&self) -> u64 {
        self.queue.dropped()
    }

    /// The size of the queue, in samples.
    pub fn buffer_capacity(&self) -> usize {
        self.queue.capacity()
    }

    /// True once the connection ended and every sample has been pulled.
    pub fn is_finished(&self) -> bool {
        self.reader_done.load(Ordering::SeqCst) && self.queue.is_empty()
    }
}

impl Drop for Inlet {
    fn drop(&mut self) {
        self.clock.stop();
        self.queue.close();
    }
}

/// Read samples, and rebuild the connection when the source goes away.
///
/// SPEC.md 10.1 records what liblsl does: the inlet finds the stream again by
/// name, type, source identifier, channel count, and channel format. The
/// instance identifier changes across a restart, and the clock state clears
/// because the old fit describes a process that no longer exists.
#[allow(clippy::too_many_arguments)]
fn read_and_recover(
    mut sock: TcpStream,
    mut carry: Vec<u8>,
    mut codec: labstream_wire::Codec,
    queue: Arc<SampleQueue>,
    done: Arc<AtomicBool>,
    clock: Arc<ClockState>,
    conn: Arc<ConnState>,
    buffer_samples: i32,
) {
    let mut tmp = vec![0u8; 65536];
    let mut batch: Vec<Sample> = Vec::new();
    let _ = sock.set_read_timeout(Some(Duration::from_millis(200)));
    let mut last_data = Instant::now();

    // Most samples of a chunk carry no timestamp. The reader rebuilds each one
    // from the position in the stream, and the accumulator runs for the life of
    // the connection. SPEC.md 8.1.
    let srate = conn.info.lock().unwrap().nominal_srate;
    let mut deducer = labstream_wire::TimestampDeducer::new(srate);

    loop {
        if queue.is_closed() {
            break;
        }

        // Decode everything already buffered before reading more. The whole
        // read goes into the queue with one lock.
        let decoded = decode_buffered(&codec, &mut carry, &mut deducer, &mut batch);
        if !batch.is_empty() {
            last_data = Instant::now();
            queue.push_owned(&mut batch);
        }
        if decoded.is_err() {
            // A stream that cannot be decoded is not worth recovering.
            // The bytes are wrong, not missing.
            done.store(true, Ordering::SeqCst);
            clock.stop();
            queue.close();
            return;
        }

        let ended = match sock.read(&mut tmp) {
            Ok(0) => Some(Ending::Closed),
            Ok(n) => {
                carry.extend_from_slice(&tmp[..n]);
                last_data = Instant::now();
                None
            }
            Err(ref e) if read_timed_out(e) => {
                // A source that stopped without closing looks exactly like a
                // quiet one. The watchdog is what tells them apart.
                if last_data.elapsed() > watchdog_threshold() {
                    Some(Ending::Quiet)
                } else {
                    None
                }
            }
            Err(_) => Some(Ending::Closed),
        };

        let ending = match ended {
            Some(e) => e,
            None => continue,
        };
        let trace = std::env::var_os("LSL_TRACE_RECOVER").is_some();
        if trace {
            eprintln!("TRACE recover: {ending:?}, recover={}", conn.recover);
        }
        if !conn.recover {
            break;
        }
        let known = conn.info.lock().unwrap().clone();
        if trace {
            eprintln!(
                "TRACE recover: query {:?} recoverable={}",
                known.recovery_query(),
                known.is_recoverable()
            );
        }
        if !known.is_recoverable() {
            // Without a source identifier the query can match another stream,
            // so liblsl refuses to recover. SPEC.md 10.1.
            break;
        }
        if ending == Ending::Closed {
            conn.connected.store(false, Ordering::SeqCst);
        }

        // Try until a connection exists again, or the inlet closes.
        loop {
            if queue.is_closed() {
                conn.connected.store(false, Ordering::SeqCst);
                done.store(true, Ordering::SeqCst);
                clock.stop();
                return;
            }

            // A closed connection to an outlet that still runs is the usual
            // case after a network fault. The handshake sends the old
            // instance identifier, and the outlet answers with its own, so a
            // different outlet on the same port cannot attach. liblsl
            // connects again to the same endpoint after an error
            // (`src/data_receiver.cpp:331-345`).
            if ending == Ending::Closed {
                if let Ok((s, c, rest)) = handshake(&known, Duration::from_secs(1), buffer_samples)
                {
                    if trace {
                        eprintln!("TRACE recover: connected again to {}", known.uid);
                    }
                    let _ = s.set_read_timeout(Some(Duration::from_millis(200)));
                    sock = s;
                    carry = rest;
                    codec = *c.codec().expect("the handshake set the codec");
                    conn.recoveries.fetch_add(1, Ordering::SeqCst);
                    conn.connected.store(true, Ordering::SeqCst);
                    last_data = Instant::now();
                    break;
                }
            }

            let found = find_again(&known);
            if trace {
                eprintln!("TRACE recover: found {found:?}");
            }
            match found {
                Found::Moved(fresh) => {
                    match handshake(&fresh, Duration::from_secs(5), buffer_samples) {
                        Ok((s, c, rest)) => {
                            let _ = s.set_read_timeout(Some(Duration::from_millis(200)));
                            sock = s;
                            carry = rest;
                            codec = *c.codec().expect("the handshake set the codec");
                            // The old offset, the old fit, and the timestamp
                            // accumulator all describe a process that has
                            // gone. The probes go to the new service port
                            // (`src/time_receiver.cpp:26-33`).
                            // The new target goes in first, so that a burst to
                            // the old target cannot publish after the reset.
                            if let Some(target) = service_endpoint(&fresh) {
                                clock.retarget(target);
                            }
                            clock.reset_offset();
                            deducer = labstream_wire::TimestampDeducer::new(fresh.nominal_srate);
                            *conn.info.lock().unwrap() = *fresh;
                            conn.recoveries.fetch_add(1, Ordering::SeqCst);
                            conn.connected.store(true, Ordering::SeqCst);
                            last_data = Instant::now();
                            break;
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(500)),
                    }
                }
                Found::Same | Found::Nothing if ending == Ending::Quiet => {
                    // The socket is still open. liblsl does nothing in this
                    // case and continues to read (`src/inlet_connection.cpp:187-188`).
                    // The next check comes after one more watchdog period.
                    last_data = Instant::now();
                    break;
                }
                Found::Same | Found::Nothing => {
                    // Wait a little so that the outlet does not see a flood of
                    // connections. liblsl waits the same time
                    // (`src/data_receiver.cpp:345`).
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        }
    }

    conn.connected.store(false, Ordering::SeqCst);
    done.store(true, Ordering::SeqCst);
    // No new offset can matter now. A stop also ends a wait for the first one.
    clock.stop();
    queue.close();
}

/// Decode every whole sample in `carry` and add it to `out`.
///
/// The bytes of the decoded samples leave `carry` with one move at the end.
/// A move after each sample costs time in proportion to the square of the
/// buffer size, and a burst of data fills the buffer.
///
/// The bytes of a sample that is not complete stay in `carry`.
fn decode_buffered(
    codec: &labstream_wire::Codec,
    carry: &mut Vec<u8>,
    deducer: &mut labstream_wire::TimestampDeducer,
    out: &mut Vec<Sample>,
) -> Result<(), labstream_wire::Error> {
    let mut pos = 0;
    let result = loop {
        match codec.decode(&carry[pos..]) {
            Ok((mut sample, used)) => {
                pos += used;
                sample.timestamp = deducer.apply(sample.timestamp);
                out.push(sample);
            }
            Err(labstream_wire::Error::Truncated) => break Ok(()),
            Err(e) => break Err(e),
        }
    };
    carry.drain(..pos);
    result
}

/// Why the reader stopped reading the current connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ending {
    /// The outlet closed the connection, or the socket reported an error.
    Closed,
    /// No data arrived for one watchdog period. The socket is still open.
    Quiet,
}

/// The result of a search for a lost stream.
#[derive(Debug)]
enum Found {
    /// The stream with the old instance identifier is still visible.
    Same,
    /// Exactly one stream matches, and it has a new instance identifier.
    Moved(Box<StreamInfo>),
    /// No stream matches, or more than one does.
    Nothing,
}

/// Find the stream again after its source restarted.
///
/// The query comes from [`StreamInfo::recovery_query`]. liblsl accepts the
/// result only when exactly one stream matches, because a source identifier
/// that is not unique would otherwise attach the inlet to the wrong stream
/// (`src/inlet_connection.cpp:188-192`).
fn find_again(known: &StreamInfo) -> Found {
    let found = match resolve(&known.recovery_query(), 1, Duration::from_secs(2)) {
        Ok(f) => f,
        Err(_) => return Found::Nothing,
    };
    // A result that still carries the old identifier means that the source
    // did not go away (`src/inlet_connection.cpp:186-188`).
    if found.iter().any(|f| f.uid == known.uid) {
        return Found::Same;
    }
    if found.len() != 1 {
        return Found::Nothing;
    }
    Found::Moved(Box::new(found.into_iter().next().unwrap()))
}

/// True when a read ended because its read timeout ran out.
///
/// On Unix, a read timeout gives `EAGAIN` (`WouldBlock`). `ETIMEDOUT`
/// (`TimedOut`) there means that the kernel ended the connection, for example
/// after keepalive probes got no answer. That is a lost connection, not a
/// quiet one. Windows gives `WSAETIMEDOUT` for a read timeout, so on Windows
/// both kinds mean a quiet connection.
fn read_timed_out(e: &std::io::Error) -> bool {
    match e.kind() {
        std::io::ErrorKind::WouldBlock => true,
        std::io::ErrorKind::TimedOut => cfg!(not(unix)),
        _ => false,
    }
}

/// Ask a data port for the whole description.
///
/// The exchange is one line out and a document back. The sender closes the
/// connection when the document is complete, so the reader reads to the end of
/// the stream (`src/info_receiver.cpp:60-67`).
///
/// A description that arrives with a created time of zero is not usable, and
/// liblsl retries. This returns an error instead, and the caller decides.
pub fn read_fullinfo(known: &StreamInfo, timeout: Duration) -> std::io::Result<StreamInfo> {
    let addr = data_endpoint(known)?;
    let mut stream = TcpStream::connect_timeout(&addr, timeout)?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.write_all(b"LSL:fullinfo\r\n")?;
    stream.flush()?;

    let mut text = String::new();
    stream.read_to_string(&mut text)?;
    StreamInfo::from_fullinfo_xml(&text).map_err(std::io::Error::other)
}

/// Connect and run the whole handshake. SPEC.md 4 and 6.
///
/// The reader thread calls this again after a recovery, so it lives outside
/// [`Inlet`] and holds no state of its own.
fn handshake(
    info: &StreamInfo,
    timeout: Duration,
    buffer_samples: i32,
) -> std::io::Result<(TcpStream, FeedClient, Vec<u8>)> {
    let addr = data_endpoint(info)?;
    let mut stream = TcpStream::connect_timeout(&addr, timeout)?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(timeout))?;
    crate::keepalive::enable(&stream);

    let mut params = FeedParams::new(&info.uid, info.format);
    params.hostname = crate::info::hostname();
    params.source_id = info.source_id.clone();
    params.session_id = info.session_id.clone();
    params.max_buffer_length = buffer_samples.max(1);

    let (mut client, request) = FeedClient::start(params, info.format, info.channel_count as usize);
    stream.write_all(&request)?;
    stream.flush()?;

    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];

    // Read the answer headers.
    loop {
        match client.on_headers(&buf) {
            Ok(used) => {
                buf.drain(..used);
                break;
            }
            Err(labstream_proto::header::Error::Incomplete) => {}
            Err(e) => return Err(std::io::Error::other(e.to_string())),
        }
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            return Err(std::io::Error::other(
                "the outlet closed during the handshake",
            ));
        }
        buf.extend_from_slice(&tmp[..n]);
    }

    // Read the two test pattern samples and compare them.
    loop {
        match client.on_test_pattern(&buf) {
            Ok(used) => {
                buf.drain(..used);
                break;
            }
            Err(TestPatternError::Incomplete) => {}
            Err(e) => {
                return Err(std::io::Error::other(format!(
                    "the test pattern did not match: {e:?}"
                )))
            }
        }
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            return Err(std::io::Error::other(
                "the outlet closed before the test pattern",
            ));
        }
        buf.extend_from_slice(&tmp[..n]);
    }

    debug_assert_eq!(client.state(), State::Streaming);
    Ok((stream, client, buf))
}

impl Inlet {
    /// True while the reader is attached to a source.
    ///
    /// A recovery sets this to false until the stream is found again.
    pub fn is_connected(&self) -> bool {
        self.conn.connected.load(Ordering::SeqCst)
    }

    /// How many times the connection has been rebuilt. SPEC.md 10.1.
    pub fn recoveries(&self) -> u64 {
        self.conn.recoveries.load(Ordering::SeqCst)
    }

    /// The stream that the reader is attached to now.
    ///
    /// A recovery replaces this, and the instance identifier changes while the
    /// name and the source identifier stay.
    pub fn current_info(&self) -> StreamInfo {
        self.conn.info.lock().unwrap().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use labstream_wire::{ByteOrder, Codec, Format, TimestampDeducer, Value, DEDUCED_TIMESTAMP};

    fn stream(codec: &Codec, n: usize) -> (Vec<Sample>, Vec<u8>) {
        let mut bytes = Vec::new();
        let samples: Vec<Sample> = (0..n)
            .map(|k| Sample {
                timestamp: if k % 5 == 0 {
                    100.0 + k as f64
                } else {
                    DEDUCED_TIMESTAMP
                },
                values: vec![Value::F32(k as f32), Value::F32(-1.0), Value::F32(2.5)],
            })
            .collect();
        for s in &samples {
            codec.encode(s, &mut bytes).unwrap();
        }
        (samples, bytes)
    }

    /// Decode the bytes that arrive in reads of the given sizes.
    fn decode_in_reads(codec: &Codec, bytes: &[u8], cuts: &[usize]) -> Vec<Sample> {
        let mut carry = Vec::new();
        let mut deducer = TimestampDeducer::new(100.0);
        let mut out = Vec::new();
        let mut from = 0;
        for &to in cuts.iter().chain(std::iter::once(&bytes.len())) {
            carry.extend_from_slice(&bytes[from..to]);
            decode_buffered(codec, &mut carry, &mut deducer, &mut out).unwrap();
            from = to;
        }
        assert!(carry.is_empty(), "no byte stays behind at the end");
        out
    }

    #[test]
    fn a_cut_at_any_byte_gives_the_same_samples() {
        let codec = Codec::new(Format::Float32, 3, ByteOrder::native());
        let (_, bytes) = stream(&codec, 40);
        let whole = decode_in_reads(&codec, &bytes, &[]);
        assert_eq!(whole.len(), 40);
        for cut in 1..bytes.len() {
            assert_eq!(
                decode_in_reads(&codec, &bytes, &[cut]),
                whole,
                "a cut at {cut}"
            );
        }
        // Many small reads, three bytes each.
        let cuts: Vec<usize> = (3..bytes.len()).step_by(3).collect();
        assert_eq!(decode_in_reads(&codec, &bytes, &cuts), whole);
    }

    #[test]
    fn the_deduced_timestamps_follow_the_rate() {
        let codec = Codec::new(Format::Float32, 3, ByteOrder::native());
        let (_, bytes) = stream(&codec, 6);
        let got = decode_in_reads(&codec, &bytes, &[]);
        assert_eq!(got[0].timestamp, 100.0);
        assert!((got[1].timestamp - 100.01).abs() < 1e-9);
        assert_eq!(got[5].timestamp, 105.0);
    }

    #[test]
    fn a_bad_tag_is_an_error_and_keeps_the_good_samples() {
        let codec = Codec::new(Format::Float32, 3, ByteOrder::native());
        let (_, mut bytes) = stream(&codec, 2);
        bytes.push(0xEE);
        let mut carry = bytes;
        let mut deducer = TimestampDeducer::new(100.0);
        let mut out = Vec::new();
        assert!(decode_buffered(&codec, &mut carry, &mut deducer, &mut out).is_err());
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn a_kernel_timeout_is_a_lost_connection_on_unix() {
        use std::io::{Error, ErrorKind};
        assert!(read_timed_out(&Error::from(ErrorKind::WouldBlock)));
        assert_eq!(
            read_timed_out(&Error::from(ErrorKind::TimedOut)),
            cfg!(not(unix))
        );
        assert!(!read_timed_out(&Error::from(ErrorKind::ConnectionReset)));
    }

    #[test]
    fn a_query_socket_takes_the_multicast_ttl_of_the_configuration() {
        let leg = match Leg::open(false) {
            Ok(l) => l,
            Err(_) => {
                eprintln!("no local network; skipping");
                return;
            }
        };
        let want = config::get().multicast_ttl;
        for s in &leg.senders {
            assert_eq!(s.multicast_ttl_v4().unwrap(), want);
        }
    }
}
