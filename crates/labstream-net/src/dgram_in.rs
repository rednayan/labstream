//! The inlet side of the `dgram/1` feature. `docs/extensions.md`, section
//! `dgram`.
//!
//! The samples of a connection arrive on two paths: UDP, and TCP for the
//! resends of large gaps and for the samples too large for a datagram. The
//! small gaps get their resends on UDP: the inlet asks for them in feedback
//! datagrams. A [`Reorder`]
//! buffer puts them back in the order of their numbers. The reader of TCP
//! and a thread for UDP both put samples into it, and both move the samples
//! that are in order to the queue of the application, under one lock.

use crate::health::{GapTracker, LatencyProbe};
use crate::queue::SampleQueue;
use labstream_proto::dgram::{self, Action, Datagram, LossMeter, Path, Reorder};
use labstream_proto::features::DgramParam;
use labstream_wire::{Codec, PackedSample};
use std::io::Write;
use std::net::{IpAddr, SocketAddr, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The UDP read timeout when a gap is open.
const GAP_TIMEOUT: Duration = Duration::from_millis(2);
/// The UDP read timeout while hellos go out.
const HELLO_TIMEOUT: Duration = Duration::from_millis(20);
/// The UDP read timeout on a clean link with no open gap.
const IDLE_TIMEOUT: Duration = Duration::from_millis(50);
/// The time between two hellos, and the time that the inlet sends them.
const HELLO_EVERY: Duration = Duration::from_millis(100);
const HELLO_FOR: Duration = Duration::from_secs(1);
/// The shortest wait for a copy before a resend request.
const MIN_WAIT: f64 = 0.010;
/// The time after a resend request on TCP before the inlet gives up the gap.
const GIVE_UP: f64 = 0.500;
/// The largest small gap, which gets its resend on UDP.
const SMALL_GAP: u64 = 64;
/// The time after which the inlet gives up a small gap at `E`.
const SMALL_GIVE_UP: f64 = 1.0;
/// The time between two feedback datagrams while datagrams arrive.
const FEEDBACK_EVERY: Duration = Duration::from_millis(100);
/// The loss after the copies and the resends that counts a window of 1 s as
/// bad.
const BAD_LOSS: f64 = 0.05;
/// The count of bad windows in a row that asks for TCP.
const BAD_WINDOWS: u32 = 3;
/// The time with no datagram that asks for TCP, while TCP works.
const DEAD_UDP: Duration = Duration::from_secs(3);

/// Minimum interval between `LatencyProbe` updates on clean high-rate streams.
const LATENCY_UPDATE_EVERY: Duration = Duration::from_millis(10);
/// Minimum interval between clean-link housekeeping checks (`poll`, `nacks`, `check_path`).
const CLEAN_POLL_EVERY: Duration = Duration::from_millis(20);

/// The state of one connection, under one lock.
struct State {
    reorder: Reorder<PackedSample>,
    /// The loss count of `seq` when the connection started. The reorder
    /// buffer counts the loss after that point.
    base_lost: u64,
    started: bool,
    /// The resend requests with no answer yet.
    requests: Vec<(u64, u32)>,
    meter: LossMeter,
    /// True once a datagram of the outlet arrived.
    heard: bool,
    on_udp: bool,
    asked_tcp: bool,
    tcp: TcpStream,
    last_datagram: Instant,
    /// The mean time between two datagrams, in seconds.
    period: f64,
    window_start: Instant,
    window_datagrams: u32,
    /// The samples given up in the window.
    window_lost: u64,
    window_first: Option<u64>,
    bad_windows: u32,
    /// Set when the connection ends. No sample goes to the queue after it.
    closed: bool,
    /// The time base of the reorder buffer.
    begin: Instant,
    /// `N` of the `resume` answer: the TCP resend gives the samples below it.
    resend_until: Option<u64>,
    /// Reorder loss count already forwarded into `gaps`.
    recorded_lost: u64,
    /// Last `lost` total stored in the shared atomic, to avoid redundant stores.
    stored_lost: u64,
    /// Loss delta waiting to attach to the next delivered sample batch for
    /// loss-aware dejitter (`docs/roadmap.md`, step 2).
    pending_skip: u64,
    /// Last delivered sample timestamp cached under `State` so clean streams
    /// avoid locking `gaps` on every datagram.
    last_sample_ts: Option<f64>,
    /// True while `gaps` has a pending loss waiting for the next delivered sample.
    has_pending_gap: bool,
    /// Last instant `latency.record` was called.
    last_latency_update: Option<Instant>,
    /// Shared inlet gap ring for `GapKind::Lost`.
    gaps: Arc<Mutex<GapTracker>>,
    /// Lock-free latency probe updated periodically per delivered batch.
    latency: Arc<LatencyProbe>,
}

/// The `dgram/1` part of one inlet connection.
pub(crate) struct DgramIn {
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
}

impl DgramIn {
    /// Open the UDP socket, send the hellos, and start the thread for UDP.
    /// `peer` is the address of the TCP connection.
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        param: DgramParam,
        tcp: &TcpStream,
        codec: Codec,
        queue: Arc<SampleQueue>,
        lost: Arc<std::sync::atomic::AtomicU64>,
        gaps: Arc<Mutex<GapTracker>>,
        latency: Arc<LatencyProbe>,
        resend_until: Option<u64>,
    ) -> Option<DgramIn> {
        let peer = tcp.peer_addr().ok()?;
        let local: IpAddr = if peer.is_ipv6() {
            std::net::Ipv6Addr::UNSPECIFIED.into()
        } else {
            std::net::Ipv4Addr::UNSPECIFIED.into()
        };
        let socket = UdpSocket::bind((local, 0)).ok()?;
        socket.set_read_timeout(Some(HELLO_TIMEOUT)).ok()?;
        let outlet = SocketAddr::new(peer.ip(), param.port);
        let state = Arc::new(Mutex::new(State {
            reorder: {
                let mut r = Reorder::new();
                r.set_small_gaps(SMALL_GAP, SMALL_GIVE_UP);
                r
            },
            base_lost: 0,
            started: false,
            requests: Vec::new(),
            meter: LossMeter::new(),
            heard: false,
            on_udp: false,
            asked_tcp: false,
            tcp: tcp.try_clone().ok()?,
            last_datagram: Instant::now(),
            period: 0.0,
            window_start: Instant::now(),
            window_datagrams: 0,
            window_lost: 0,
            window_first: None,
            bad_windows: 0,
            closed: false,
            begin: Instant::now(),
            resend_until,
            recorded_lost: 0,
            stored_lost: 0,
            pending_skip: 0,
            last_sample_ts: None,
            has_pending_gap: false,
            last_latency_update: None,
            gaps,
            latency,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        {
            let (state, stop) = (Arc::clone(&state), Arc::clone(&stop));
            std::thread::spawn(move || {
                udp_loop(
                    socket,
                    outlet,
                    param.cookie,
                    codec,
                    &state,
                    &stop,
                    &queue,
                    &lost,
                )
            });
        }
        Some(DgramIn { state, stop })
    }

    /// A `seq` record on TCP. The first one starts the buffer (forwarding any
    /// reconnect gap `reconnect_delta` that occurred before `dgram` started).
    /// A later record that answers a resend request, with a number above the
    /// request, shows samples that the history no longer holds.
    pub fn seq_record(&self, number: u64, counter_lost: u64, reconnect_delta: u64) {
        let mut s = self.state.lock().unwrap();
        if !s.started {
            s.started = true;
            s.base_lost = counter_lost;
            if reconnect_delta > 0 {
                s.pending_skip = s.pending_skip.saturating_add(reconnect_delta);
                let last_ts = s.last_sample_ts;
                {
                    let mut g = s.gaps.lock().unwrap();
                    if let Some(ts) = last_ts {
                        g.on_sample(ts);
                    }
                    g.record_lost(reconnect_delta);
                }
                s.has_pending_gap = true;
            }
            s.reorder.start(number);
            // The `resume` resend is on the way on TCP. Asking for it again
            // only adds a second copy to the link.
            if let Some(until) = s.resend_until {
                let now = s.begin.elapsed().as_secs_f64();
                s.reorder.assume_requested(number, until, now);
            }
            return;
        }
        let e = s.reorder.expected();
        if let Some(i) = s
            .requests
            .iter()
            .position(|&(from, count)| number > from && number <= from + count as u64)
        {
            let (from, _) = s.requests.remove(i);
            if e == Some(from) {
                if trace() {
                    eprintln!("TRACE dgram: the history starts at {number}, not {from}");
                }
                s.reorder.lost_below(number);
                sync_lost_to_tracker(&mut s);
            }
        }
    }

    /// A path record on TCP.
    pub fn path(&self, path: Path) {
        let mut s = self.state.lock().unwrap();
        s.on_udp = path == Path::Udp;
        // The datagrams show that the outlet is alive. `docs/extensions.md`,
        // section `dgram`, keepalive on UDP.
        if s.on_udp {
            crate::keepalive::relax(&s.tcp);
        } else {
            crate::keepalive::enable(&s.tcp);
        }
        if s.on_udp {
            s.asked_tcp = false;
            s.last_datagram = Instant::now();
            s.bad_windows = 0;
        }
    }

    /// A sample from TCP, with its number.
    pub fn tcp_sample(&self, number: u64, sample: PackedSample) {
        let mut s = self.state.lock().unwrap();
        let now = s.begin.elapsed().as_secs_f64();
        s.reorder.progress(now);
        s.reorder.insert(number, sample);
    }

    /// Move the samples that are in order to the queue.
    pub fn deliver(&self, queue: &SampleQueue, lost: &std::sync::atomic::AtomicU64) {
        let mut s = self.state.lock().unwrap();
        deliver(&mut s, queue, lost, Instant::now());
    }

    /// End this part when its connection ends. No sample goes to the queue
    /// after this call. The result is the number that the inlet expects next,
    /// for the `resume` request of the next connection.
    pub fn close(&self) -> Option<u64> {
        let mut s = self.state.lock().unwrap();
        s.closed = true;
        if let Some(ts) = s.last_sample_ts {
            s.gaps.lock().unwrap().on_sample(ts);
        }
        self.stop.store(true, Ordering::SeqCst);
        s.reorder.expected()
    }

    /// True while the samples of the connection arrive on UDP.
    pub fn on_udp(&self) -> bool {
        self.state.lock().unwrap().on_udp
    }

    /// True if a datagram of the outlet arrived in the last `time`. The
    /// watchdog of the inlet counts it as data: on UDP, no byte arrives on
    /// TCP for a long time.
    pub fn heard_within(&self, time: Duration) -> bool {
        let s = self.state.lock().unwrap();
        s.heard && s.last_datagram.elapsed() < time
    }
}

impl Drop for DgramIn {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn deliver(s: &mut State, queue: &SampleQueue, lost: &std::sync::atomic::AtomicU64, now: Instant) {
    let mut out = Vec::new();
    deliver_into(s, &mut out, queue, lost, now);
}

fn sync_lost_to_tracker(s: &mut State) {
    let cur = s.reorder.lost();
    if cur > s.recorded_lost {
        let delta = cur - s.recorded_lost;
        s.recorded_lost = cur;
        s.pending_skip = s.pending_skip.saturating_add(delta);
        let last_ts = s.last_sample_ts;
        {
            let mut g = s.gaps.lock().unwrap();
            if let Some(ts) = last_ts {
                g.on_sample(ts);
            }
            g.record_lost(delta);
        }
        s.has_pending_gap = true;
    }
}

fn deliver_into(
    s: &mut State,
    out: &mut Vec<PackedSample>,
    queue: &SampleQueue,
    lost: &std::sync::atomic::AtomicU64,
    now: Instant,
) {
    if s.closed {
        out.clear();
        return;
    }
    sync_lost_to_tracker(s);
    s.reorder.drain(out);
    if !out.is_empty() {
        let first_ts = out[0].timestamp;
        let last_ts = out[out.len() - 1].timestamp;
        if s.has_pending_gap {
            s.gaps.lock().unwrap().on_batch(first_ts, last_ts);
            s.has_pending_gap = false;
        }
        s.last_sample_ts = Some(last_ts);
        let update_latency = s.last_latency_update.map_or(true, |prev| {
            now.duration_since(prev) >= LATENCY_UPDATE_EVERY
        });
        if update_latency {
            s.latency.record(last_ts);
            s.last_latency_update = Some(now);
        }
        let skip = s.pending_skip;
        s.pending_skip = 0;
        if skip > 0 {
            queue.push_owned_packed_with_skips(out, &[(0, skip)]);
        } else {
            queue.push_owned_packed(out);
        }
    }
    let total_lost = s.base_lost + s.reorder.lost();
    if total_lost != s.stored_lost {
        s.stored_lost = total_lost;
        lost.store(total_lost, Ordering::SeqCst);
    }
}

#[allow(clippy::too_many_arguments)]
fn udp_loop(
    socket: UdpSocket,
    outlet: SocketAddr,
    cookie: u64,
    codec: Codec,
    state: &Mutex<State>,
    stop: &AtomicBool,
    queue: &SampleQueue,
    lost: &std::sync::atomic::AtomicU64,
) {
    let begin = state.lock().unwrap().begin;
    let mut hello_at: Option<Instant> = None;
    let mut heard = false;
    let mut feedback_at = Instant::now();
    let mut last_poll = Instant::now();
    let mut cur_timeout = HELLO_TIMEOUT;
    let mut buf = vec![0u8; 2048];
    let mut out: Vec<PackedSample> = Vec::new();
    while !stop.load(Ordering::Relaxed) && !queue.is_closed() {
        // The hellos, until the first datagram arrives, or for 1 s.
        if !heard
            && begin.elapsed() < HELLO_FOR
            && hello_at.map_or(true, |t| t.elapsed() >= HELLO_EVERY)
        {
            let _ = socket.send_to(&dgram::hello(cookie), outlet);
            hello_at = Some(Instant::now());
        }

        let got = match socket.recv_from(&mut buf) {
            Ok((n, from)) if from == outlet => Some(n),
            _ => None,
        };
        let mut s = state.lock().unwrap();
        let now = Instant::now();
        let t = now.saturating_duration_since(begin).as_secs_f64();
        out.clear();
        if let Some(n) = got {
            match dgram::parse(&buf[..n]) {
                Ok(Datagram::Data {
                    cookie: c,
                    first,
                    count,
                    serial,
                    payload,
                }) if c == cookie => {
                    s.meter.record(serial, t);
                    s.reorder.udp_heard(t);
                    let mut at = 0;
                    for k in 0..count as u64 {
                        match codec.decode_packed(&payload[at..]) {
                            Ok((sample, used)) => {
                                at += used;
                                s.reorder
                                    .insert_or_emit(first.wrapping_add(k), sample, &mut out);
                            }
                            Err(_) => break,
                        }
                    }
                    heard_datagram(&mut s, now);
                    heard = true;
                    s.heard = true;
                }
                Ok(Datagram::Idle { cookie: c, next }) if c == cookie => {
                    s.reorder.udp_heard(t);
                    s.reorder.idle(next);
                    heard_datagram(&mut s, now);
                    heard = true;
                    s.heard = true;
                }
                _ => {}
            }
        }
        deliver_into(&mut s, &mut out, queue, lost, now);

        let has_gaps = s.reorder.has_gaps();
        let due_poll = got.is_none() || now.duration_since(last_poll) >= CLEAN_POLL_EVERY;
        if has_gaps || due_poll {
            last_poll = now;
            // A gap at the expected number: a request, then a loss.
            let wait = MIN_WAIT.max(2.0 * s.period);
            match s.reorder.poll(t, wait, GIVE_UP) {
                Some(Action::Request { from, count }) => {
                    let mut r = Vec::new();
                    dgram::write_resend(&mut r, from, count);
                    if s.tcp.write_all(&r).is_ok() {
                        if trace() {
                            eprintln!("TRACE dgram: asked for {count} samples from {from}");
                        }
                        s.requests.push((from, count));
                    }
                }
                Some(Action::GaveUp { from, count }) => {
                    if trace() {
                        eprintln!("TRACE dgram: gave up {count} samples from {from}");
                    }
                    s.window_lost += count;
                    deliver_into(&mut s, &mut out, queue, lost, now);
                }
                None => {}
            }

            // The small gaps, and the measured loss, in a feedback datagram.
            // `docs/extensions.md`, section `dgram`, the feedback datagram.
            let gaps = if has_gaps {
                s.reorder.nacks(t, wait)
            } else {
                Vec::new()
            };
            if heard
                && (!gaps.is_empty()
                    || (s.on_udp && now.duration_since(feedback_at) >= FEEDBACK_EVERY))
            {
                let d = dgram::feedback(cookie, s.meter.loss_ppm(t), &gaps);
                if socket.send_to(&d, outlet).is_ok() {
                    feedback_at = now;
                    if trace() && !gaps.is_empty() {
                        eprintln!(
                            "TRACE dgram: asked for {} gaps on UDP from {}",
                            gaps.len(),
                            gaps[0].0
                        );
                    }
                }
            }
            check_path(&mut s, now);
        }
        let want_timeout = if has_gaps {
            GAP_TIMEOUT
        } else if !heard {
            HELLO_TIMEOUT
        } else {
            IDLE_TIMEOUT
        };
        drop(s);
        if want_timeout != cur_timeout {
            let _ = socket.set_read_timeout(Some(want_timeout));
            cur_timeout = want_timeout;
        }
    }
}

/// True when `LSL_TRACE_DGRAM` is set.
fn trace() -> bool {
    std::env::var_os("LSL_TRACE_DGRAM").is_some()
}

fn heard_datagram(s: &mut State, now: Instant) {
    let gap = now.duration_since(s.last_datagram).as_secs_f64();
    s.period = if s.period == 0.0 {
        gap
    } else {
        0.9 * s.period + 0.1 * gap
    };
    s.last_datagram = now;
    s.window_datagrams += 1;
}

/// The fallback rules of the inlet: heavy loss after the copies and the
/// resends, or a UDP path that stops while TCP works. `docs/extensions.md`,
/// section `dgram`, the fallback to TCP, rules 1 and 2.
fn check_path(s: &mut State, now: Instant) {
    if !s.on_udp || s.asked_tcp {
        return;
    }
    let dead = now.duration_since(s.last_datagram) >= DEAD_UDP;
    if now.duration_since(s.window_start) >= Duration::from_secs(1) {
        let e = s.reorder.expected().unwrap_or(0);
        let samples = e.saturating_sub(s.window_first.unwrap_or(e)).max(1);
        if s.window_datagrams >= 10 {
            let bad = s.window_lost as f64 / samples as f64 > BAD_LOSS;
            s.bad_windows = if bad { s.bad_windows + 1 } else { 0 };
        }
        s.window_start = now;
        s.window_datagrams = 0;
        s.window_lost = 0;
        s.window_first = Some(e);
    }
    if dead || s.bad_windows >= BAD_WINDOWS {
        if let Some(from) = s.reorder.expected() {
            let mut r = Vec::new();
            dgram::write_path_request(&mut r, Path::Tcp, from);
            crate::keepalive::enable(&s.tcp);
            if s.tcp.write_all(&r).is_ok() {
                s.asked_tcp = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use labstream_wire::{ByteOrder, Format};
    use std::net::TcpListener;

    #[test]
    fn on_udp_the_tcp_connection_takes_the_long_keepalive_timers() {
        // `docs/extensions.md`, section `dgram`, keepalive on UDP.
        let Ok(listener) = TcpListener::bind("127.0.0.1:0") else {
            eprintln!("no local network; skipping");
            return;
        };
        let stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        crate::keepalive::enable(&stream);
        let param = DgramParam { port: 9, cookie: 1 };
        let codec = Codec::new(Format::Float32, 1, ByteOrder::native());
        let queue = Arc::new(SampleQueue::new(16));
        let lost = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let d = DgramIn::start(
            param,
            &stream,
            codec,
            queue,
            lost,
            Arc::new(Mutex::new(GapTracker::new())),
            Arc::new(LatencyProbe::default()),
            None,
        )
        .expect("a UDP socket");
        let sock = socket2::SockRef::from(&stream);
        d.path(Path::Udp);
        // The timers read back only on the systems where the getters exist.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        assert_eq!(
            sock.tcp_keepalive_time().unwrap(),
            crate::keepalive::UDP_IDLE,
            "a quiet TCP connection on UDP is not ended by keepalive"
        );
        d.path(Path::Tcp);
        #[cfg(any(target_os = "linux", target_os = "android"))]
        assert_eq!(sock.tcp_keepalive_time().unwrap(), crate::keepalive::IDLE);
        assert!(sock.keepalive().unwrap());
        d.close();
    }
}
