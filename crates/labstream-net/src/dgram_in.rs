//! The inlet side of the `dgram/1` feature. `docs/extensions.md`, section
//! `dgram`.
//!
//! The samples of a connection arrive on two paths: UDP, and TCP for the
//! resends and for the samples too large for a datagram. A [`Reorder`]
//! buffer puts them back in the order of their numbers. The reader of TCP
//! and a thread for UDP both put samples into it, and both move the samples
//! that are in order to the queue of the application, under one lock.

use crate::queue::SampleQueue;
use labstream_proto::dgram::{self, Action, Datagram, Path, Reorder};
use labstream_proto::features::DgramParam;
use labstream_wire::{Codec, Sample};
use std::io::Write;
use std::net::{IpAddr, SocketAddr, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The time between two hellos, and the time that the inlet sends them.
const HELLO_EVERY: Duration = Duration::from_millis(100);
const HELLO_FOR: Duration = Duration::from_secs(1);
/// The shortest wait for a copy before a resend request.
const MIN_WAIT: f64 = 0.010;
/// The time after a resend request before the inlet gives up the gap.
const GIVE_UP: f64 = 0.500;
/// The loss after the copies that counts a window of 1 s as bad.
const BAD_LOSS: f64 = 0.05;
/// The count of bad windows in a row that asks for TCP.
const BAD_WINDOWS: u32 = 3;
/// The time with no datagram that asks for TCP, while TCP works.
const DEAD_UDP: Duration = Duration::from_secs(3);

/// The state of one connection, under one lock.
struct State {
    reorder: Reorder<Sample>,
    /// The loss count of `seq` when the connection started. The reorder
    /// buffer counts the loss after that point.
    base_lost: u64,
    started: bool,
    /// The resend requests with no answer yet.
    requests: Vec<(u64, u32)>,
    on_udp: bool,
    asked_tcp: bool,
    tcp: TcpStream,
    last_datagram: Instant,
    /// The mean time between two datagrams, in seconds.
    period: f64,
    window_start: Instant,
    window_datagrams: u32,
    window_asked: u64,
    window_first: Option<u64>,
    bad_windows: u32,
    /// Set when the connection ends. No sample goes to the queue after it.
    closed: bool,
}

/// The `dgram/1` part of one inlet connection.
pub(crate) struct DgramIn {
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
}

impl DgramIn {
    /// Open the UDP socket, send the hellos, and start the thread for UDP.
    /// `peer` is the address of the TCP connection.
    pub fn start(
        param: DgramParam,
        tcp: &TcpStream,
        codec: Codec,
        queue: Arc<SampleQueue>,
        lost: Arc<std::sync::atomic::AtomicU64>,
    ) -> Option<DgramIn> {
        let peer = tcp.peer_addr().ok()?;
        let local: IpAddr = if peer.is_ipv6() {
            std::net::Ipv6Addr::UNSPECIFIED.into()
        } else {
            std::net::Ipv4Addr::UNSPECIFIED.into()
        };
        let socket = UdpSocket::bind((local, 0)).ok()?;
        socket
            .set_read_timeout(Some(Duration::from_millis(2)))
            .ok()?;
        let outlet = SocketAddr::new(peer.ip(), param.port);
        let state = Arc::new(Mutex::new(State {
            reorder: Reorder::new(),
            base_lost: 0,
            started: false,
            requests: Vec::new(),
            on_udp: false,
            asked_tcp: false,
            tcp: tcp.try_clone().ok()?,
            last_datagram: Instant::now(),
            period: 0.0,
            window_start: Instant::now(),
            window_datagrams: 0,
            window_asked: 0,
            window_first: None,
            bad_windows: 0,
            closed: false,
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

    /// A `seq` record on TCP. The first one starts the buffer. A record
    /// that answers a resend request, with a number above the request,
    /// shows samples that the history no longer holds.
    pub fn seq_record(&self, number: u64, counter_lost: u64) {
        let mut s = self.state.lock().unwrap();
        if !s.started {
            s.started = true;
            s.base_lost = counter_lost;
            s.reorder.start(number);
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
                s.reorder.lost_below(number);
            }
        }
    }

    /// A path record on TCP.
    pub fn path(&self, path: Path) {
        let mut s = self.state.lock().unwrap();
        s.on_udp = path == Path::Udp;
        if s.on_udp {
            s.asked_tcp = false;
            s.last_datagram = Instant::now();
            s.bad_windows = 0;
        }
    }

    /// A sample from TCP, with its number.
    pub fn tcp_sample(&self, number: u64, sample: Sample) {
        self.state.lock().unwrap().reorder.insert(number, sample);
    }

    /// Move the samples that are in order to the queue.
    pub fn deliver(&self, queue: &SampleQueue, lost: &std::sync::atomic::AtomicU64) {
        let mut s = self.state.lock().unwrap();
        deliver(&mut s, queue, lost);
    }

    /// End this part when its connection ends. No sample goes to the queue
    /// after this call. The result is the number that the inlet expects next,
    /// for the `resume` request of the next connection.
    pub fn close(&self) -> Option<u64> {
        let mut s = self.state.lock().unwrap();
        s.closed = true;
        self.stop.store(true, Ordering::SeqCst);
        s.reorder.expected()
    }

    /// True while the samples of the connection arrive on UDP.
    pub fn on_udp(&self) -> bool {
        self.state.lock().unwrap().on_udp
    }
}

impl Drop for DgramIn {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn deliver(s: &mut State, queue: &SampleQueue, lost: &std::sync::atomic::AtomicU64) {
    if s.closed {
        return;
    }
    let mut out = Vec::new();
    s.reorder.drain(&mut out);
    if !out.is_empty() {
        queue.push_owned(&mut out);
    }
    lost.store(s.base_lost + s.reorder.lost(), Ordering::SeqCst);
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
    let begin = Instant::now();
    let mut hello_at: Option<Instant> = None;
    let mut heard = false;
    let mut buf = vec![0u8; 2048];
    while !stop.load(Ordering::SeqCst) && !queue.is_closed() {
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
        if let Some(n) = got {
            match dgram::parse(&buf[..n]) {
                Ok(Datagram::Data {
                    cookie: c,
                    first,
                    count,
                    payload,
                }) if c == cookie => {
                    let mut at = 0;
                    for k in 0..count as u64 {
                        match codec.decode(&payload[at..]) {
                            Ok((sample, used)) => {
                                at += used;
                                s.reorder.insert(first.wrapping_add(k), sample);
                            }
                            Err(_) => break,
                        }
                    }
                    heard_datagram(&mut s, now);
                    heard = true;
                }
                Ok(Datagram::Idle { cookie: c, next }) if c == cookie => {
                    s.reorder.idle(next);
                    heard_datagram(&mut s, now);
                    heard = true;
                }
                _ => {}
            }
        }
        deliver(&mut s, queue, lost);

        // A gap at the expected number: a request, then a loss.
        let wait = MIN_WAIT.max(2.0 * s.period);
        let t = begin.elapsed().as_secs_f64();
        match s.reorder.poll(t, wait, GIVE_UP) {
            Some(Action::Request { from, count }) => {
                let mut r = Vec::new();
                dgram::write_resend(&mut r, from, count);
                if s.tcp.write_all(&r).is_ok() {
                    s.requests.push((from, count));
                    s.window_asked += count as u64;
                }
            }
            Some(Action::GaveUp { .. }) => deliver(&mut s, queue, lost),
            None => {}
        }
        check_path(&mut s, now);
    }
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

/// The fallback rules of the inlet: heavy loss after the copies, or a UDP
/// path that stops while TCP works. `docs/extensions.md`, section `dgram`,
/// the fallback to TCP, rules 1 and 2.
fn check_path(s: &mut State, now: Instant) {
    if !s.on_udp || s.asked_tcp {
        return;
    }
    let dead = now.duration_since(s.last_datagram) >= DEAD_UDP;
    if now.duration_since(s.window_start) >= Duration::from_secs(1) {
        let e = s.reorder.expected().unwrap_or(0);
        let samples = e.saturating_sub(s.window_first.unwrap_or(e)).max(1);
        if s.window_datagrams >= 10 {
            let bad = s.window_asked as f64 / samples as f64 > BAD_LOSS;
            s.bad_windows = if bad { s.bad_windows + 1 } else { 0 };
        }
        s.window_start = now;
        s.window_datagrams = 0;
        s.window_asked = 0;
        s.window_first = Some(e);
    }
    if dead || s.bad_windows >= BAD_WINDOWS {
        if let Some(from) = s.reorder.expected() {
            let mut r = Vec::new();
            dgram::write_path_request(&mut r, Path::Tcp, from);
            if s.tcp.write_all(&r).is_ok() {
                s.asked_tcp = true;
            }
        }
    }
}
