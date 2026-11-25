//! The outlet side of the `dgram/1` feature. `docs/extensions.md`, section
//! `dgram`.
//!
//! The session first runs as with `resume/1`. When a hello with the correct
//! cookie arrives, a path record on TCP moves the samples to UDP. The TCP
//! connection then carries the resends of large gaps, samples too large for
//! a datagram, and the path records. A thread reads the requests of the
//! inlet from TCP. A second thread reads the hello and the feedback
//! datagrams of the inlet from UDP.

use crate::queue::{Fanout, SampleQueue};
use labstream_proto::comp::{CompState, DgramCompEncoder};
use labstream_proto::dgram::{self, CopyControl, Packer, Path, Record};
use labstream_proto::seq::Numbering;
use labstream_wire::{Codec, Format, PackedSample};
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The time with no data datagram after which the outlet sends a repeat
/// datagram. `docs/extensions.md`, the copies, rule 3.
const REPEAT_AFTER: Duration = Duration::from_millis(5);
/// The time between two idle datagrams. `docs/extensions.md`, the idle
/// datagram.
const IDLE_EVERY: Duration = Duration::from_secs(1);
/// The time that the outlet waits for a hello.
const HELLO_WAIT: Duration = Duration::from_secs(1);
/// The time after a fallback before the outlet tries UDP again.
const RETRY_UDP: Duration = Duration::from_secs(30);
/// The idle datagrams after the last data datagram. `docs/extensions.md`,
/// the idle datagram, rule 1.
const TAIL: [Duration; 3] = [
    Duration::from_millis(25),
    Duration::from_millis(100),
    Duration::from_millis(250),
];
/// The most samples that the outlet sends for one feedback datagram.
const RESEND_CAP: usize = 256;
/// The shortest time before the outlet sends one sample again on UDP.
const RESEND_AGAIN: Duration = Duration::from_millis(10);
/// The time with no feedback datagram after which the outlet uses the short
/// keepalive timers again. `docs/extensions.md`, keepalive on UDP, rule 3.
const QUIET_FEEDBACK: Duration = Duration::from_secs(3);

/// True when the samples of this stream are small enough for `dgram/1`.
pub(crate) fn sample_fits(format: Format, channels: usize) -> bool {
    match format.width() {
        // A string sample that is too large goes on TCP, one by one.
        None => true,
        Some(w) => 1 + 8 + w * channels <= dgram::MAX_SAMPLE_BYTES,
    }
}

/// The UDP socket of one connection, bound in the LSL port range, in the
/// address family of the TCP peer.
pub(crate) fn bind(peer: SocketAddr) -> Option<UdpSocket> {
    let cfg = crate::config::get();
    let host: IpAddr = if peer.is_ipv6() {
        std::net::Ipv6Addr::UNSPECIFIED.into()
    } else {
        std::net::Ipv4Addr::UNSPECIFIED.into()
    };
    (cfg.base_port..cfg.base_port.saturating_add(cfg.port_range))
        .find_map(|p| UdpSocket::bind((host, p)).ok())
}

/// A random cookie.
pub(crate) fn cookie() -> u64 {
    let b = crate::random_bytes();
    u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

/// What the session needs for `dgram/1`.
pub(crate) struct Setup {
    pub socket: UdpSocket,
    pub cookie: u64,
    pub peer: IpAddr,
    /// The requests of the inlet, read from TCP.
    pub requests: Receiver<Record>,
    /// The queue of the session, to wake it for a feedback datagram.
    pub queue: Arc<SampleQueue>,
    /// The TCP connection, for its keepalive timers.
    pub tcp: TcpStream,
}

/// One feedback datagram of the inlet: the loss in parts per million, and
/// the gaps.
type Feedback = (u32, Vec<(u64, u32)>);

/// Read the requests of the inlet from TCP. The thread ends with the
/// connection.
pub(crate) fn read_requests(
    mut stream: TcpStream,
    queue: std::sync::Arc<SampleQueue>,
) -> Receiver<Record> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = stream.set_read_timeout(None);
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            match stream.read(&mut tmp) {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
            }
            while !buf.is_empty() {
                let r = dgram::peek(&buf);
                match r {
                    Record::Resend { .. } | Record::PathRequest { .. } => {
                        buf.drain(..r.len());
                        if tx.send(r).is_err() {
                            return;
                        }
                        // The session can wait on the queue for up to 1 s.
                        queue.wake();
                    }
                    Record::Incomplete => break,
                    // Bytes that are no request end the reader. The inlet
                    // sends nothing else.
                    _ => return,
                }
            }
        }
    });
    rx
}

/// Read the UDP socket of the session until `stop`. The first hello with
/// the correct cookie from the address of the TCP peer, in 1 s, sets the
/// target. After it, each feedback datagram from the target with the
/// correct cookie goes to the result channel, and wakes the session.
fn read_udp(
    socket: &UdpSocket,
    cookie: u64,
    peer: IpAddr,
    queue: Arc<SampleQueue>,
    stop: Arc<AtomicBool>,
) -> (Arc<Mutex<Option<SocketAddr>>>, Receiver<Feedback>) {
    let found = Arc::new(Mutex::new(None));
    let (tx, rx) = std::sync::mpsc::channel();
    let Ok(sock) = socket.try_clone() else {
        return (found, rx);
    };
    let out = Arc::clone(&found);
    std::thread::spawn(move || {
        let _ = sock.set_read_timeout(Some(Duration::from_millis(100)));
        let begin = Instant::now();
        let mut target: Option<SocketAddr> = None;
        let mut buf = [0u8; 2048];
        while !stop.load(Ordering::SeqCst) {
            if target.is_none() && begin.elapsed() >= HELLO_WAIT {
                return;
            }
            let Ok((n, from)) = sock.recv_from(&mut buf) else {
                continue;
            };
            match (target, dgram::parse(&buf[..n])) {
                (None, Ok(dgram::Datagram::Hello { cookie: c }))
                    if c == cookie && from.ip() == peer =>
                {
                    target = Some(from);
                    *out.lock().unwrap() = Some(from);
                }
                (
                    Some(t),
                    Ok(dgram::Datagram::Feedback {
                        cookie: c,
                        loss,
                        gaps,
                    }),
                ) if c == cookie && from == t => {
                    if tx.send((loss, gaps.iter().collect())).is_err() {
                        return;
                    }
                    queue.wake();
                }
                _ => {}
            }
        }
    });
    (found, rx)
}

/// Sets a flag when it goes out of scope.
struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// The live part of a `dgram/1` session.
pub(crate) struct Live<'a, W: Write> {
    pub queue: &'a SampleQueue,
    pub fanout: &'a Fanout,
    pub codec: &'a Codec,
    pub channels: Option<Vec<u16>>,
    pub compressed: bool,
    pub meta_enabled: bool,
    pub meta_cursor: u64,
    pub out: &'a mut W,
    pub numbering: Numbering,
    /// The number of the last sample on TCP, for the timestamp rule.
    pub last: Option<u64>,
    pub tcp_comp: Option<CompState>,
    pub setup: Setup,
    pub trace: bool,
}

fn encode_selected(
    codec: &Codec,
    channels: Option<&[u16]>,
    s: &PackedSample,
    ts: Option<f64>,
    out: &mut Vec<u8>,
) -> Result<(), labstream_wire::Error> {
    match (channels, ts) {
        (Some(chs), Some(t)) => {
            let p = labstream_proto::select::project_packed(s, chs);
            codec.encode_packed_at(&p, t, out)
        }
        (Some(chs), None) => {
            let p = labstream_proto::select::project_packed(s, chs);
            codec.encode_packed(&p, out)
        }
        (None, Some(t)) => codec.encode_packed_at(s, t, out),
        (None, None) => codec.encode_packed(s, out),
    }
}

fn encode_tcp_selected(
    codec: &Codec,
    channels: Option<&[u16]>,
    comp: Option<&mut CompState>,
    s: &PackedSample,
    ts: f64,
    out: &mut Vec<u8>,
) -> Result<(), labstream_wire::Error> {
    match (channels, comp) {
        (Some(chs), Some(c)) => {
            let p = labstream_proto::select::project_packed(s, chs);
            c.encode_tcp_sample(codec, &p, ts, out)
        }
        (None, Some(c)) => c.encode_tcp_sample(codec, s, ts, out),
        (Some(chs), None) => {
            let p = labstream_proto::select::project_packed(s, chs);
            codec.encode_packed_at(&p, ts, out)
        }
        (None, None) => codec.encode_packed_at(s, ts, out),
    }
}

/// Self-contained compressed UDP datagram packer (`dgram/1` + `comp/1`).
///
/// Packs consecutive samples into `DATA_COMP` (type 5) datagrams up to
/// [`dgram::MAX_DATAGRAM`] bytes, falling back to plain `DATA` (type 2)
/// whenever plain framing is smaller or equal.
struct CompPacker {
    cookie: u64,
    codec: Codec,
    enc: DgramCompEncoder,
    recent: VecDeque<(u64, PackedSample, f64, u8)>,
    copies: u8,
    serial: u32,
}

impl CompPacker {
    fn new(cookie: u64, codec: Codec) -> Self {
        let enc = DgramCompEncoder::new(codec.format, codec.channels);
        CompPacker {
            cookie,
            codec,
            enc,
            recent: VecDeque::new(),
            copies: 0,
            serial: 0,
        }
    }

    fn copies(&self) -> u8 {
        self.copies
    }

    fn serial(&self) -> u32 {
        self.serial
    }

    fn set_copies(&mut self, copies: u8) {
        self.copies = copies.min(dgram::MAX_COPIES);
        if self.copies == 0 {
            self.recent.clear();
        }
    }

    fn next_serial(&mut self) -> u32 {
        let s = self.serial;
        self.serial = self.serial.wrapping_add(1);
        s
    }

    fn remember(&mut self, number: u64, sample: PackedSample, ts: f64) {
        if let Some(last) = self.recent.back() {
            if last.0.wrapping_add(1) != number {
                self.recent.clear();
            }
        }
        self.recent.push_back((number, sample, ts, 0));
        if self.recent.len() > 128 {
            self.recent.pop_front();
        }
    }

    fn plain_sample_len(&self) -> usize {
        9 + self.codec.format.width().unwrap_or(0) * self.codec.channels
    }

    fn emit_datagram(
        &mut self,
        first: u64,
        slice: &[(PackedSample, f64)],
        gram: &mut Vec<u8>,
    ) {
        let count = slice.len();
        let serial = self.next_serial();
        let plain_len = count * self.plain_sample_len();
        let comp_payload = self.enc.finish();
        let room = dgram::MAX_DATAGRAM - dgram::DATA_HEADER;
        gram.clear();
        if comp_payload.len() < plain_len || plain_len > room {
            dgram::data_comp_header(gram, self.cookie, first, count as u16, serial);
            gram.extend_from_slice(comp_payload);
        } else {
            dgram::data_header(gram, self.cookie, first, count as u16, serial);
            for (s, ts) in slice {
                let _ = self.codec.encode_packed_at(s, *ts, gram);
            }
        }
        debug_assert!(gram.len() <= dgram::MAX_DATAGRAM);
    }

    fn push_batch(
        &mut self,
        first: u64,
        samples: &[(PackedSample, f64)],
        gram: &mut Vec<u8>,
        mut on_datagram: impl FnMut(&[u8]),
    ) {
        let room = dgram::MAX_DATAGRAM - dgram::DATA_HEADER;
        let mut i = 0;
        let mut combined: Vec<(PackedSample, f64)> = Vec::new();
        while i < samples.len() {
            let n = first.wrapping_add(i as u64);
            // Pack as many new samples as fit in one compressed datagram.
            self.enc.reset();
            let mut j = i;
            while j < samples.len() && self.enc.try_push(&samples[j].0, samples[j].1, room) {
                j += 1;
            }
            if j == i {
                // Should never happen for streams passing `sample_fits`.
                i += 1;
                continue;
            }
            let new_slice = &samples[i..j];

            // When copies are enabled, try prepending eligible recent samples
            // right before `n` while everything fits in `room`.
            let mut used_copies = 0usize;
            if self.copies > 0 && !self.recent.is_empty() {
                let mut eligible = 0usize;
                for (num, _, _, sent) in self.recent.iter().rev() {
                    if *num != n.wrapping_sub(1 + eligible as u64) || *sent >= self.copies {
                        break;
                    }
                    eligible += 1;
                }
                if eligible > 0 {
                    // Try the largest copy count that fits alongside `new_slice`.
                    let mut try_k = eligible.min(16);
                    while try_k > 0 {
                        self.enc.reset();
                        let start_idx = self.recent.len() - try_k;
                        let mut ok = true;
                        for (_, rs, rts, _) in self.recent.range(start_idx..) {
                            if !self.enc.try_push(rs, *rts, room) {
                                ok = false;
                                break;
                            }
                        }
                        if ok {
                            for (ns, nts) in new_slice {
                                if !self.enc.try_push(ns, *nts, room) {
                                    ok = false;
                                    break;
                                }
                            }
                        }
                        if ok {
                            used_copies = try_k;
                            break;
                        }
                        try_k -= 1;
                    }
                    if used_copies == 0 {
                        // Restore encoder state to `new_slice` alone.
                        self.enc.reset();
                        for (ns, nts) in new_slice {
                            let _ = self.enc.try_push(ns, *nts, room);
                        }
                    }
                }
            }

            if used_copies > 0 {
                let start_idx = self.recent.len() - used_copies;
                combined.clear();
                for (_, rs, rts, sent) in self.recent.range_mut(start_idx..) {
                    *sent += 1;
                    combined.push((rs.clone(), *rts));
                }
                combined.extend_from_slice(new_slice);
                self.emit_datagram(n.wrapping_sub(used_copies as u64), &combined, gram);
            } else {
                self.emit_datagram(n, new_slice, gram);
            }
            on_datagram(gram);

            if self.copies > 0 {
                for (k, (ns, nts)) in new_slice.iter().enumerate() {
                    self.remember(n.wrapping_add(k as u64), ns.clone(), *nts);
                }
            }
            i = j;
        }
    }

    fn repeat(&mut self, gram: &mut Vec<u8>) -> bool {
        let Some(last_num) = self.recent.back().map(|x| x.0) else {
            return false;
        };
        let next = last_num.wrapping_add(1);
        let mut eligible = 0usize;
        for (num, _, _, sent) in self.recent.iter().rev() {
            if *num != next.wrapping_sub(1 + eligible as u64) || *sent >= self.copies {
                break;
            }
            eligible += 1;
        }
        if eligible == 0 {
            return false;
        }
        let room = dgram::MAX_DATAGRAM - dgram::DATA_HEADER;
        let mut try_k = eligible.min(32);
        while try_k > 0 {
            self.enc.reset();
            let start_idx = self.recent.len() - try_k;
            let mut ok = true;
            for (_, rs, rts, _) in self.recent.range(start_idx..) {
                if !self.enc.try_push(rs, *rts, room) {
                    ok = false;
                    break;
                }
            }
            if ok {
                let mut slice = Vec::with_capacity(try_k);
                for (_, rs, rts, sent) in self.recent.range_mut(start_idx..) {
                    *sent += 1;
                    slice.push((rs.clone(), *rts));
                }
                self.emit_datagram(next.wrapping_sub(try_k as u64), &slice, gram);
                return true;
            }
            try_k -= 1;
        }
        false
    }

    fn resend(&mut self, first: u64, samples: &[(PackedSample, f64)], out: &mut Vec<Vec<u8>>) {
        let room = dgram::MAX_DATAGRAM - dgram::DATA_HEADER;
        let mut i = 0;
        let mut gram = Vec::with_capacity(dgram::MAX_DATAGRAM);
        while i < samples.len() {
            self.enc.reset();
            let mut j = i;
            while j < samples.len() && self.enc.try_push(&samples[j].0, samples[j].1, room) {
                j += 1;
            }
            if j == i {
                i += 1;
                continue;
            }
            self.emit_datagram(first.wrapping_add(i as u64), &samples[i..j], &mut gram);
            out.push(gram.clone());
            i = j;
        }
    }
}

impl<W: Write> Live<'_, W> {
    /// Send the queue until `stop`, or until a write fails. The result is the
    /// count of samples sent.
    pub fn run(mut self, stop: &dyn Fn() -> bool) -> usize {
        let stop_udp = Arc::new(AtomicBool::new(false));
        let _stop_udp = StopOnDrop(Arc::clone(&stop_udp));
        let (hello, feedback) = read_udp(
            &self.setup.socket,
            self.setup.cookie,
            self.setup.peer,
            Arc::clone(&self.setup.queue),
            stop_udp,
        );
        let begin = Instant::now();
        let mut target: Option<SocketAddr> = None;
        let mut path = Path::Tcp;
        let mut packer = Packer::new(self.setup.cookie);
        let mut comp_packer = self
            .compressed
            .then(|| CompPacker::new(self.setup.cookie, *self.codec));
        let mut control = CopyControl::new();
        let mut resent_at: HashMap<u64, Instant> = HashMap::new();
        let mut tail = TAIL.len();
        let mut last_feedback = Instant::now();
        let mut relaxed = false;
        let mut fallback_at: Option<Instant> = None;
        let mut last_data = Instant::now();
        let mut repeated = true;
        let mut last_idle = Instant::now();
        let mut batch: Vec<PackedSample> = Vec::new();
        let mut full: Vec<f64> = Vec::new();
        let mut comp_batch: Vec<(PackedSample, f64)> = Vec::new();
        let mut pending: Vec<u8> = Vec::new();
        let mut slab: Vec<u8> = Vec::new();
        let mut ranges: Vec<std::ops::Range<usize>> = Vec::new();
        let mut gram: Vec<u8> = Vec::with_capacity(dgram::MAX_DATAGRAM);
        let mut meta_buf: Vec<labstream_proto::meta::MetadataUpdate> = Vec::new();
        let mut sent = 0usize;
        // The number after the last sample that left the queue.
        let mut next = self
            .queue
            .pop_many_numbered_packed(&mut Vec::new(), 0, Duration::ZERO)
            .1;

        loop {
            if stop() || self.queue.is_closed() {
                break;
            }
            if self.meta_enabled {
                meta_buf.clear();
                self.fanout
                    .read_metadata_since(&mut self.meta_cursor, &mut meta_buf);
                if !meta_buf.is_empty() {
                    for u in &meta_buf {
                        labstream_proto::meta::write_record(&mut pending, u);
                    }
                    if !self.flush(&mut pending) {
                        return sent;
                    }
                }
            }
            if target.is_none() {
                target = *hello.lock().unwrap();
            }

            // The requests of the inlet.
            loop {
                match self.setup.requests.try_recv() {
                    Ok(Record::Resend { from, count }) => {
                        let until = from.saturating_add(count as u64).min(next);
                        if !self.resend(from, until, &mut pending) {
                            return sent;
                        }
                    }
                    Ok(Record::PathRequest {
                        path: Path::Tcp,
                        from,
                    }) => {
                        if path == Path::Udp {
                            if self.trace {
                                eprintln!("TRACE dgram: the inlet asks for TCP from {from}");
                            }
                            path = Path::Tcp;
                            fallback_at = Some(Instant::now());
                            dgram::write_path(&mut pending, Path::Tcp, from);
                            if !self.resend(from, next, &mut pending) {
                                return sent;
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return sent,
                }
            }

            // The feedback of the inlet: the count of copies, and the small
            // gaps, sent again on UDP.
            while let Ok((loss, gaps)) = feedback.try_recv() {
                last_feedback = Instant::now();
                if path == Path::Udp && !relaxed {
                    crate::keepalive::relax(&self.setup.tcp);
                    relaxed = true;
                }
                let t = begin.elapsed().as_secs_f64();
                let serial = comp_packer
                    .as_ref()
                    .map_or_else(|| packer.serial(), CompPacker::serial);
                let cur_copies = comp_packer
                    .as_ref()
                    .map_or_else(|| packer.copies(), CompPacker::copies);
                let copies = control.report(loss, t, serial);
                if self.trace && copies != cur_copies {
                    eprintln!("TRACE dgram: {copies} copies at a loss of {loss} ppm");
                }
                if let Some(cp) = comp_packer.as_mut() {
                    cp.set_copies(copies);
                } else {
                    packer.set_copies(copies);
                }
                if let (Path::Udp, Some(to)) = (path, target) {
                    self.resend_udp(
                        &gaps,
                        next,
                        &mut packer,
                        comp_packer.as_mut(),
                        &mut resent_at,
                        to,
                    );
                }
            }

            // Keepalive: the long timers only while the feedback shows that
            // the inlet is alive. `docs/extensions.md`, keepalive on UDP.
            if relaxed && (path == Path::Tcp || last_feedback.elapsed() >= QUIET_FEEDBACK) {
                crate::keepalive::enable(&self.setup.tcp);
                relaxed = false;
            }

            // The change to UDP, once the hello arrived, and again after a
            // fallback.
            let may_retry = fallback_at.map_or(true, |t| t.elapsed() >= RETRY_UDP);
            if path == Path::Tcp && target.is_some() && may_retry {
                if self.trace {
                    eprintln!("TRACE dgram: samples from {next} go on UDP");
                }
                dgram::write_path(&mut pending, Path::Udp, next);
                if !self.flush(&mut pending) {
                    return sent;
                }
                path = Path::Udp;
                last_idle = Instant::now();
            }

            let wait = match path {
                Path::Udp if !repeated => REPEAT_AFTER.saturating_sub(last_data.elapsed()),
                Path::Udp if tail < TAIL.len() => TAIL[tail].saturating_sub(last_data.elapsed()),
                Path::Udp => IDLE_EVERY.saturating_sub(last_idle.elapsed()),
                Path::Tcp if target.is_none() => Duration::from_millis(10),
                Path::Tcp => Duration::from_millis(200),
            }
            .max(Duration::from_millis(1));
            batch.clear();
            full.clear();
            let (taken, first) =
                self.queue
                    .pop_many_full_packed(&mut batch, &mut full, usize::MAX, wait);

            if taken == 0 {
                if path == Path::Udp {
                    let to = target.expect("UDP needs a target");
                    if !repeated && last_data.elapsed() >= REPEAT_AFTER {
                        if let Some(cp) = comp_packer.as_mut() {
                            if cp.repeat(&mut gram) {
                                let _ = self.setup.socket.send_to(&gram, to);
                            } else {
                                let _ = self
                                    .setup
                                    .socket
                                    .send_to(&dgram::idle(self.setup.cookie, next), to);
                            }
                        } else {
                            let d = packer
                                .repeat()
                                .unwrap_or_else(|| dgram::idle(self.setup.cookie, next).to_vec());
                            let _ = self.setup.socket.send_to(&d, to);
                        }
                        repeated = true;
                        last_idle = Instant::now();
                    } else if tail < TAIL.len() && last_data.elapsed() >= TAIL[tail] {
                        let _ = self
                            .setup
                            .socket
                            .send_to(&dgram::idle(self.setup.cookie, next), to);
                        tail += 1;
                        last_idle = Instant::now();
                    } else if last_idle.elapsed() >= IDLE_EVERY {
                        let _ = self
                            .setup
                            .socket
                            .send_to(&dgram::idle(self.setup.cookie, next), to);
                        last_idle = Instant::now();
                    }
                }
                continue;
            }
            next = first.wrapping_add(taken as u64);

            match path {
                Path::Tcp => {
                    for (i, s) in batch.iter().enumerate() {
                        let number = first.wrapping_add(i as u64);
                        self.numbering.before(number, &mut pending);
                        let ts = if self.last.map(|l| l.wrapping_add(1)) != Some(number) {
                            full[i]
                        } else {
                            s.timestamp
                        };
                        self.last = Some(number);
                        if encode_tcp_selected(
                            self.codec,
                            self.channels.as_deref(),
                            self.tcp_comp.as_mut(),
                            s,
                            ts,
                            &mut pending,
                        )
                        .is_err()
                        {
                            return sent;
                        }
                        sent += 1;
                    }
                    if !self.flush(&mut pending) {
                        return sent;
                    }
                }
                Path::Udp => {
                    let to = target.expect("UDP needs a target");
                    let mut failed = false;
                    let socket = &self.setup.socket;
                    if let Some(cp) = comp_packer.as_mut() {
                        comp_batch.clear();
                        for (i, s) in batch.iter().enumerate() {
                            let projected = match self.channels.as_deref() {
                                Some(chs) => labstream_proto::select::project_packed(s, chs),
                                None => s.clone(),
                            };
                            comp_batch.push((projected, full[i]));
                        }
                        cp.push_batch(first, &comp_batch, &mut gram, |d| {
                            if !failed && socket.send_to(d, to).is_err() {
                                failed = true;
                            }
                        });
                    } else {
                        slab.clear();
                        ranges.clear();
                        for (i, s) in batch.iter().enumerate() {
                            // A datagram can arrive out of order, so each sample
                            // carries its full timestamp.
                            let start = slab.len();
                            if encode_selected(
                                self.codec,
                                self.channels.as_deref(),
                                s,
                                Some(full[i]),
                                &mut slab,
                            )
                            .is_err()
                            {
                                return sent;
                            }
                            ranges.push(start..slab.len());
                        }
                        let numbering = &mut self.numbering;
                        let last = &mut self.last;
                        if ranges.len() == 1 {
                            let single = [&slab[ranges[0].clone()]];
                            packer.push_slices(
                                first,
                                &single,
                                &mut gram,
                                |d| {
                                    if !failed && socket.send_to(d, to).is_err() {
                                        failed = true;
                                    }
                                },
                                |n| {
                                    numbering.before(n, &mut pending);
                                    pending.extend_from_slice(&slab[ranges[0].clone()]);
                                    *last = Some(n);
                                },
                            );
                        } else {
                            let slices: Vec<&[u8]> =
                                ranges.iter().map(|r| &slab[r.clone()]).collect();
                            packer.push_slices(
                                first,
                                &slices,
                                &mut gram,
                                |d| {
                                    if !failed && socket.send_to(d, to).is_err() {
                                        failed = true;
                                    }
                                },
                                |n| {
                                    let i = (n - first) as usize;
                                    numbering.before(n, &mut pending);
                                    pending.extend_from_slice(&slab[ranges[i].clone()]);
                                    *last = Some(n);
                                },
                            );
                        }
                    }
                    sent += taken;
                    if failed {
                        // An error of the socket: back to TCP, with the
                        // samples of this batch. `docs/extensions.md`, the
                        // fallback, rule 3.
                        if self.trace {
                            eprintln!("TRACE dgram: the socket failed, back to TCP");
                        }
                        path = Path::Tcp;
                        fallback_at = Some(Instant::now());
                        dgram::write_path(&mut pending, Path::Tcp, first);
                        if !self.resend(first, next, &mut pending) {
                            return sent;
                        }
                    }
                    if !self.flush(&mut pending) {
                        return sent;
                    }
                    last_data = Instant::now();
                    repeated = false;
                    tail = 0;
                }
            }
        }
        sent
    }

    /// Send the samples from `from` up to `until - 1` again, from the
    /// history, on TCP. If the history no longer holds the first ones, the
    /// `seq` record of the first sample that it holds shows the gap.
    fn resend(&mut self, from: u64, until: u64, pending: &mut Vec<u8>) -> bool {
        let mut history = Vec::new();
        let mut at = from;
        while let Some((first, taken)) =
            self.fanout
                .read_history_packed(at, until, crate::outlet::RESEND_BLOCK, &mut history)
        {
            for (i, s) in history.iter().enumerate() {
                let number = first.wrapping_add(i as u64);
                self.numbering.before(number, pending);
                if encode_tcp_selected(
                    self.codec,
                    self.channels.as_deref(),
                    self.tcp_comp.as_mut(),
                    s,
                    s.timestamp,
                    pending,
                )
                .is_err()
                {
                    return false;
                }
                self.last = Some(number);
            }
            history.clear();
            if !self.flush(pending) {
                return false;
            }
            at = first.wrapping_add(taken as u64);
        }
        self.flush(pending)
    }

    /// Send the samples of small gaps again on UDP, from the history.
    /// `docs/extensions.md`, section `dgram`, the resend, rule 5.
    fn resend_udp(
        &mut self,
        gaps: &[(u64, u32)],
        next: u64,
        packer: &mut Packer,
        mut comp_packer: Option<&mut CompPacker>,
        resent_at: &mut HashMap<u64, Instant>,
        to: SocketAddr,
    ) {
        if resent_at.len() > 4096 {
            resent_at.retain(|_, t| t.elapsed() < Duration::from_secs(1));
        }
        let cur_copies = comp_packer
            .as_deref()
            .map_or_else(|| packer.copies(), CompPacker::copies);
        let times = if cur_copies >= 3 { 2 } else { 1 };
        let mut budget = RESEND_CAP;
        let mut history = Vec::new();
        let mut grams = Vec::new();
        for &(from, count) in gaps {
            let until = from.saturating_add(count as u64).min(next);
            let mut at = from;
            while at < until && budget > 0 {
                // A run of numbers that did not go out in the last 10 ms.
                while at < until
                    && resent_at
                        .get(&at)
                        .is_some_and(|t| t.elapsed() < RESEND_AGAIN)
                {
                    at += 1;
                }
                let mut end = at;
                while end < until
                    && (end - at) < budget as u64
                    && !resent_at
                        .get(&end)
                        .is_some_and(|t| t.elapsed() < RESEND_AGAIN)
                {
                    end += 1;
                }
                if end == at {
                    break;
                }
                history.clear();
                let Some((first, taken)) =
                    self.fanout
                        .read_history_packed(at, end, (end - at) as usize, &mut history)
                else {
                    break;
                };
                grams.clear();
                if let Some(cp) = comp_packer.as_deref_mut() {
                    let mut pair_buf = Vec::with_capacity(taken);
                    for s in &history {
                        let projected = match self.channels.as_deref() {
                            Some(chs) => labstream_proto::select::project_packed(s, chs),
                            None => s.clone(),
                        };
                        pair_buf.push((projected, s.timestamp));
                    }
                    cp.resend(first, &pair_buf, &mut grams);
                } else {
                    let mut encoded = Vec::with_capacity(taken);
                    for s in &history {
                        let mut e = Vec::new();
                        if encode_selected(self.codec, self.channels.as_deref(), s, None, &mut e)
                            .is_err()
                        {
                            return;
                        }
                        encoded.push(e);
                    }
                    packer.resend(first, &encoded, &mut grams);
                }
                for d in &grams {
                    for _ in 0..times {
                        let _ = self.setup.socket.send_to(d, to);
                    }
                }
                let now = Instant::now();
                for n in first..first.wrapping_add(taken as u64) {
                    resent_at.insert(n, now);
                }
                budget = budget.saturating_sub(taken);
                if self.trace {
                    eprintln!("TRACE dgram: resent {taken} samples from {first} on UDP");
                }
                at = end.max(first.wrapping_add(taken as u64));
            }
        }
    }

    /// Write what waits on TCP. A resend block and a batch each go out with
    /// one write. `Max-Chunk-Length` does not apply on this path.
    fn flush(&mut self, pending: &mut Vec<u8>) -> bool {
        if pending.is_empty() {
            return true;
        }
        let ok = self.out.write_all(pending).is_ok() && self.out.flush().is_ok();
        pending.clear();
        ok
    }
}
