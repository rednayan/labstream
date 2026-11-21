//! The outlet side of the `dgram/1` feature. `docs/extensions.md`, section
//! `dgram`.
//!
//! The session first runs as with `resume/1`. When a hello with the correct
//! cookie arrives, a path record on TCP moves the samples to UDP. The TCP
//! connection then carries resends, samples too large for a datagram, and
//! the path records. A thread reads the requests of the inlet from TCP.

use crate::queue::{Fanout, SampleQueue};
use labstream_proto::dgram::{self, Packer, Packet, Path, Record};
use labstream_proto::seq::Numbering;
use labstream_wire::{Codec, Format, Sample};
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, UdpSocket};
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
}

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

/// Wait for the first hello with the correct cookie from the address of the
/// TCP peer. The result holds the source of that hello, once it arrives.
fn wait_hello(socket: &UdpSocket, cookie: u64, peer: IpAddr) -> Arc<Mutex<Option<SocketAddr>>> {
    let found = Arc::new(Mutex::new(None));
    let Ok(sock) = socket.try_clone() else {
        return found;
    };
    let out = Arc::clone(&found);
    std::thread::spawn(move || {
        let end = Instant::now() + HELLO_WAIT;
        let mut buf = [0u8; 64];
        while let Some(left) = end.checked_duration_since(Instant::now()) {
            let _ = sock.set_read_timeout(Some(left.max(Duration::from_millis(1))));
            let Ok((n, from)) = sock.recv_from(&mut buf) else {
                continue;
            };
            if from.ip() != peer {
                continue;
            }
            if dgram::parse(&buf[..n]) == Ok(dgram::Datagram::Hello { cookie }) {
                *out.lock().unwrap() = Some(from);
                return;
            }
        }
    });
    found
}

/// The live part of a `dgram/1` session.
pub(crate) struct Live<'a, W: Write> {
    pub queue: &'a SampleQueue,
    pub fanout: &'a Fanout,
    pub codec: &'a Codec,
    pub out: &'a mut W,
    pub numbering: Numbering,
    /// The number of the last sample on TCP, for the timestamp rule.
    pub last: Option<u64>,
    pub setup: Setup,
    pub trace: bool,
}

impl<W: Write> Live<'_, W> {
    /// Send the queue until `stop`, or until a write fails. The result is the
    /// count of samples sent.
    pub fn run(mut self, stop: &dyn Fn() -> bool) -> usize {
        let hello = wait_hello(&self.setup.socket, self.setup.cookie, self.setup.peer);
        let mut target: Option<SocketAddr> = None;
        let mut path = Path::Tcp;
        let mut packer = Packer::new(self.setup.cookie);
        let mut fallback_at: Option<Instant> = None;
        let mut last_data = Instant::now();
        let mut repeated = true;
        let mut last_idle = Instant::now();
        let mut batch: Vec<Sample> = Vec::new();
        let mut full: Vec<f64> = Vec::new();
        let mut pending: Vec<u8> = Vec::new();
        let mut sent = 0usize;
        // The number after the last sample that left the queue.
        let mut next = self
            .queue
            .pop_many_numbered(&mut Vec::new(), 0, Duration::ZERO)
            .1;

        loop {
            if stop() || self.queue.is_closed() {
                break;
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
                Path::Udp => IDLE_EVERY.saturating_sub(last_idle.elapsed()),
                Path::Tcp if target.is_none() => Duration::from_millis(10),
                Path::Tcp => Duration::from_millis(200),
            }
            .max(Duration::from_millis(1));
            batch.clear();
            full.clear();
            let (taken, first) = self
                .queue
                .pop_many_full(&mut batch, &mut full, usize::MAX, wait);

            if taken == 0 {
                if path == Path::Udp {
                    let to = target.expect("UDP needs a target");
                    if !repeated && last_data.elapsed() >= REPEAT_AFTER {
                        let d = packer
                            .repeat()
                            .unwrap_or_else(|| dgram::idle(self.setup.cookie, next).to_vec());
                        let _ = self.setup.socket.send_to(&d, to);
                        repeated = true;
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
                    for (i, s) in batch.iter_mut().enumerate() {
                        let number = first.wrapping_add(i as u64);
                        self.numbering.before(number, &mut pending);
                        if self.last.map(|l| l.wrapping_add(1)) != Some(number) {
                            s.timestamp = full[i];
                        }
                        self.last = Some(number);
                        if self.codec.encode(s, &mut pending).is_err() {
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
                    let mut encoded = Vec::with_capacity(batch.len());
                    for (i, s) in batch.iter_mut().enumerate() {
                        // A datagram can arrive out of order, so each sample
                        // carries its full timestamp.
                        s.timestamp = full[i];
                        let mut e = Vec::new();
                        if self.codec.encode(s, &mut e).is_err() {
                            return sent;
                        }
                        encoded.push(e);
                    }
                    let mut packets = Vec::new();
                    packer.push(first, &encoded, &mut packets);
                    let mut failed = false;
                    for p in packets {
                        match p {
                            Packet::Datagram(d) => {
                                if !failed && self.setup.socket.send_to(&d, to).is_err() {
                                    failed = true;
                                }
                            }
                            Packet::Tcp(n) => {
                                let i = (n - first) as usize;
                                self.numbering.before(n, &mut pending);
                                pending.extend_from_slice(&encoded[i]);
                                self.last = Some(n);
                            }
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
                .read_history(at, until, crate::outlet::RESEND_BLOCK, &mut history)
        {
            for (i, s) in history.iter().enumerate() {
                let number = first.wrapping_add(i as u64);
                self.numbering.before(number, pending);
                if self.codec.encode(s, pending).is_err() {
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
