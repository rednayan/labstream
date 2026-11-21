//! The wire formats of the `dgram/1` feature. `docs/extensions.md`, section
//! `dgram`.
//!
//! Four datagrams go between the two UDP sockets: the hello and the feedback
//! of the inlet, and the data and idle datagrams of the outlet. Three records
//! go on the TCP
//! connection: the path record of the outlet, and the resend request and
//! path request of the inlet. Each number is little-endian, whatever the
//! byte order of the connection.

/// The largest datagram, the header included. This size passes most links
/// with no fragmentation of IP.
pub const MAX_DATAGRAM: usize = 1200;

/// The length of the header of a data datagram.
pub const DATA_HEADER: usize = 1 + 8 + 8 + 2 + 4;

/// The largest sample of a stream that can use `dgram/1`. A datagram then
/// holds at least 3 samples, so the copies can work.
pub const MAX_SAMPLE_BYTES: usize = 400;

/// The type byte of a hello datagram.
pub const HELLO: u8 = 1;
/// The type byte of a data datagram.
pub const DATA: u8 = 2;
/// The type byte of an idle datagram.
pub const IDLE: u8 = 3;
/// The type byte of a feedback datagram.
pub const FEEDBACK: u8 = 4;
/// The length of the header of a feedback datagram.
pub const FEEDBACK_HEADER: usize = 1 + 8 + 4 + 1;
/// The most gaps in one feedback datagram.
pub const MAX_GAPS: usize = 64;
/// The value of `loss` in a feedback datagram when the inlet has no measure.
pub const NO_MEASURE: u32 = u32::MAX;

/// The first byte of a path record, on TCP from the outlet.
pub const PATH_TAG: u8 = 0xFD;
/// The length of a path record.
pub const PATH_LEN: usize = 1 + 1 + 8;
/// The first byte of a resend request, on TCP from the inlet.
pub const RESEND_TAG: u8 = 0xFC;
/// The length of a resend request.
pub const RESEND_LEN: usize = 1 + 8 + 4;
/// The first byte of a path request, on TCP from the inlet.
pub const PATH_REQUEST_TAG: u8 = 0xFB;
/// The length of a path request.
pub const PATH_REQUEST_LEN: usize = 1 + 1 + 8;

/// The path of the samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Path {
    /// The TCP connection.
    Tcp,
    /// The UDP socket.
    Udp,
}

impl Path {
    fn byte(self) -> u8 {
        match self {
            Path::Tcp => 0,
            Path::Udp => 1,
        }
    }

    fn from_byte(b: u8) -> Option<Path> {
        match b {
            0 => Some(Path::Tcp),
            1 => Some(Path::Udp),
            _ => None,
        }
    }
}

/// One datagram, as the receiver reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Datagram<'a> {
    /// The first datagram of the inlet.
    Hello {
        /// The cookie of the answer.
        cookie: u64,
    },
    /// Samples with consecutive numbers.
    Data {
        /// The cookie of the answer.
        cookie: u64,
        /// The number of the first sample.
        first: u64,
        /// The count of samples.
        count: u16,
        /// The number of the datagram.
        serial: u32,
        /// The samples, in the encoding of SPEC.md 7.
        payload: &'a [u8],
    },
    /// No sample follows.
    Idle {
        /// The cookie of the answer.
        cookie: u64,
        /// The number of the next sample.
        next: u64,
    },
    /// The measured loss and the small gaps of the inlet.
    Feedback {
        /// The cookie of the answer.
        cookie: u64,
        /// The loss of datagrams in parts per million, or [`NO_MEASURE`].
        loss: u32,
        /// The gaps that the inlet asks for.
        gaps: Gaps<'a>,
    },
}

/// The gaps of a feedback datagram, as `(from, count)` pairs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gaps<'a>(&'a [u8]);

impl Gaps<'_> {
    /// Each gap, in the order of the datagram.
    pub fn iter(&self) -> impl Iterator<Item = (u64, u32)> + '_ {
        self.0
            .chunks_exact(12)
            .map(|g| (u64_at(g, 0), u32::from_le_bytes([g[8], g[9], g[10], g[11]])))
    }
}

/// Why a datagram could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The datagram is shorter than its header.
    Truncated,
    /// The type byte is unknown.
    BadType(u8),
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    let mut x = [0u8; 8];
    x.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(x)
}

/// Write a hello datagram.
pub fn hello(cookie: u64) -> [u8; 9] {
    let mut out = [0u8; 9];
    out[0] = HELLO;
    out[1..].copy_from_slice(&cookie.to_le_bytes());
    out
}

/// Write the header of a data datagram. The samples follow it.
pub fn data_header(out: &mut Vec<u8>, cookie: u64, first: u64, count: u16, serial: u32) {
    out.push(DATA);
    out.extend_from_slice(&cookie.to_le_bytes());
    out.extend_from_slice(&first.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&serial.to_le_bytes());
}

/// Write a feedback datagram. Only the first [`MAX_GAPS`] gaps go in.
pub fn feedback(cookie: u64, loss: u32, gaps: &[(u64, u32)]) -> Vec<u8> {
    let gaps = &gaps[..gaps.len().min(MAX_GAPS)];
    let mut out = Vec::with_capacity(FEEDBACK_HEADER + 12 * gaps.len());
    out.push(FEEDBACK);
    out.extend_from_slice(&cookie.to_le_bytes());
    out.extend_from_slice(&loss.to_le_bytes());
    out.push(gaps.len() as u8);
    for &(from, count) in gaps {
        out.extend_from_slice(&from.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
    }
    out
}

/// Write an idle datagram.
pub fn idle(cookie: u64, next: u64) -> [u8; 17] {
    let mut out = [0u8; 17];
    out[0] = IDLE;
    out[1..9].copy_from_slice(&cookie.to_le_bytes());
    out[9..].copy_from_slice(&next.to_le_bytes());
    out
}

/// Read one datagram.
pub fn parse(buf: &[u8]) -> Result<Datagram<'_>, Error> {
    let kind = *buf.first().ok_or(Error::Truncated)?;
    match kind {
        HELLO if buf.len() >= 9 => Ok(Datagram::Hello {
            cookie: u64_at(buf, 1),
        }),
        DATA if buf.len() >= DATA_HEADER => Ok(Datagram::Data {
            cookie: u64_at(buf, 1),
            first: u64_at(buf, 9),
            count: u16::from_le_bytes([buf[17], buf[18]]),
            serial: u32::from_le_bytes([buf[19], buf[20], buf[21], buf[22]]),
            payload: &buf[DATA_HEADER..],
        }),
        IDLE if buf.len() >= 17 => Ok(Datagram::Idle {
            cookie: u64_at(buf, 1),
            next: u64_at(buf, 9),
        }),
        FEEDBACK if buf.len() >= FEEDBACK_HEADER => {
            let n = buf[13] as usize;
            let end = FEEDBACK_HEADER + 12 * n;
            if buf.len() < end {
                return Err(Error::Truncated);
            }
            Ok(Datagram::Feedback {
                cookie: u64_at(buf, 1),
                loss: u32::from_le_bytes([buf[9], buf[10], buf[11], buf[12]]),
                gaps: Gaps(&buf[FEEDBACK_HEADER..end]),
            })
        }
        HELLO | DATA | IDLE | FEEDBACK => Err(Error::Truncated),
        other => Err(Error::BadType(other)),
    }
}

/// Append a path record: the samples from `number` go on `path`.
pub fn write_path(out: &mut Vec<u8>, path: Path, number: u64) {
    out.push(PATH_TAG);
    out.push(path.byte());
    out.extend_from_slice(&number.to_le_bytes());
}

/// Append a resend request for `count` samples from `from`.
pub fn write_resend(out: &mut Vec<u8>, from: u64, count: u32) {
    out.push(RESEND_TAG);
    out.extend_from_slice(&from.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
}

/// Append a path request: the inlet asks for `path` from the number `from`.
pub fn write_path_request(out: &mut Vec<u8>, path: Path, from: u64) {
    out.push(PATH_REQUEST_TAG);
    out.push(path.byte());
    out.extend_from_slice(&from.to_le_bytes());
}

/// A record at the start of a byte stream on TCP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Record {
    /// A path record, from the outlet.
    Path {
        /// The new path.
        path: Path,
        /// The number of the first sample on the new path.
        number: u64,
    },
    /// A resend request, from the inlet.
    Resend {
        /// The first missing number.
        from: u64,
        /// The count of missing samples.
        count: u32,
    },
    /// A path request, from the inlet.
    PathRequest {
        /// The path that the inlet asks for.
        path: Path,
        /// The number that the inlet expects next.
        from: u64,
    },
    /// A record that is cut. The caller reads more bytes.
    Incomplete,
    /// No record of this module.
    None,
    /// A record with a path byte that is not 0 or 1.
    Bad,
}

impl Record {
    /// The length of the record in bytes, for a complete record.
    pub fn len(&self) -> usize {
        match self {
            Record::Path { .. } => PATH_LEN,
            Record::Resend { .. } => RESEND_LEN,
            Record::PathRequest { .. } => PATH_REQUEST_LEN,
            _ => 0,
        }
    }

    /// True for a record with no bytes.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Look at the start of `buf` for a record of this module.
pub fn peek(buf: &[u8]) -> Record {
    let Some(&tag) = buf.first() else {
        return Record::None;
    };
    let need = match tag {
        PATH_TAG => PATH_LEN,
        RESEND_TAG => RESEND_LEN,
        PATH_REQUEST_TAG => PATH_REQUEST_LEN,
        _ => return Record::None,
    };
    if buf.len() < need {
        return Record::Incomplete;
    }
    match tag {
        PATH_TAG => match Path::from_byte(buf[1]) {
            Some(path) => Record::Path {
                path,
                number: u64_at(buf, 2),
            },
            None => Record::Bad,
        },
        RESEND_TAG => Record::Resend {
            from: u64_at(buf, 1),
            count: u32::from_le_bytes([buf[9], buf[10], buf[11], buf[12]]),
        },
        _ => match Path::from_byte(buf[1]) {
            Some(path) => Record::PathRequest {
                path,
                from: u64_at(buf, 2),
            },
            None => Record::Bad,
        },
    }
}

/// The most later datagrams that carry a copy of a sample.
pub const MAX_COPIES: u8 = 8;

/// The count of copies for a measured loss of datagrams, in parts per
/// million. `docs/extensions.md`, section `dgram`, the copies, rule 6.
pub fn copies_for(loss_ppm: u32) -> u8 {
    match loss_ppm {
        0 => 0,
        1..=20_000 => 2,
        20_001..=100_000 => 3,
        100_001..=250_000 => 4,
        250_001..=400_000 => 6,
        _ => MAX_COPIES,
    }
}

/// The time that each report must give a lower count of copies before
/// that count applies.
pub const LOWER_AFTER: f64 = 5.0;
/// The count of datagrams that the outlet must send while each report
/// gives a lower count of copies, before that count applies. At 1 % loss,
/// 1000 datagrams in a row with no loss occur less than once in 20000. A
/// stream with few datagrams, for example chunks of 16, thus keeps its
/// copies.
pub const LOWER_DATAGRAMS: u32 = 1000;

/// The count of copies of the outlet, from the reports of the inlet. A
/// higher count applies at once. A lower count applies only after
/// [`LOWER_AFTER`] seconds and [`LOWER_DATAGRAMS`] datagrams. Time is a
/// value in seconds that the caller gives.
#[derive(Debug, Default)]
pub struct CopyControl {
    current: u8,
    /// The start of the time with lower reports (the time and the `serial`
    /// of the next datagram), and the highest count that they gave.
    lower: Option<(f64, u32, u8)>,
}

impl CopyControl {
    /// A control that starts with no copies.
    pub fn new() -> CopyControl {
        CopyControl::default()
    }

    /// The count of copies now.
    pub fn copies(&self) -> u8 {
        self.current
    }

    /// Take one report. `loss` is in parts per million, or [`NO_MEASURE`].
    /// `serial` is the `serial` of the next datagram of the outlet. The
    /// result is the count of copies after the report.
    pub fn report(&mut self, loss: u32, now: f64, serial: u32) -> u8 {
        if loss == NO_MEASURE {
            return self.current;
        }
        let target = copies_for(loss);
        if target >= self.current {
            self.current = target;
            self.lower = None;
            return self.current;
        }
        let (since, from, high) = *self.lower.get_or_insert((now, serial, target));
        let high = high.max(target);
        if now - since >= LOWER_AFTER && serial.wrapping_sub(from) >= LOWER_DATAGRAMS {
            self.current = high;
            // This report starts the next time with lower reports.
            self.lower = (target < high).then_some((now, serial, target));
        } else {
            self.lower = Some((since, from, high));
        }
        self.current
    }
}

/// The length of one bucket of the loss meter, in seconds.
const BUCKET: f64 = 0.1;
/// The count of buckets that the loss meter adds.
const BUCKETS: usize = 10;
/// A pause in the datagrams of at least this time is a cut. The jump in
/// `serial` across it is no loss.
const CUT: f64 = 1.0;

/// The loss of datagrams at the inlet, from their `serial`.
/// `docs/extensions.md`, section `dgram`, the feedback datagram, rule 1.
///
/// Each jump in `serial` counts as loss, except a jump across a pause of
/// 1 s or more. A datagram that arrives after a later one takes back one
/// loss. The meter adds the last 10 buckets of 100 ms.
#[derive(Debug, Default)]
pub struct LossMeter {
    /// (start, datagrams, lost), oldest first.
    buckets: std::collections::VecDeque<(f64, u64, u64)>,
    /// The highest `serial`, and the time of the last datagram.
    last: Option<(u32, f64)>,
}

impl LossMeter {
    /// An empty meter.
    pub fn new() -> LossMeter {
        LossMeter::default()
    }

    /// One data datagram arrived at `now`.
    pub fn record(&mut self, serial: u32, now: f64) {
        let (mut lost, mut back) = (0u64, false);
        match self.last {
            Some((high, at)) if serial > high => {
                if now - at < CUT {
                    lost = (serial - high - 1) as u64;
                }
                self.last = Some((serial, now));
            }
            Some((high, _)) => {
                back = serial < high;
                self.last = Some((high, now));
            }
            None => self.last = Some((serial, now)),
        }
        match self.buckets.back_mut() {
            Some(b) if now - b.0 < BUCKET => {
                b.1 += 1;
                b.2 += lost;
            }
            _ => {
                self.buckets.push_back((now, 1, lost));
                if self.buckets.len() > BUCKETS {
                    self.buckets.pop_front();
                }
            }
        }
        if back {
            // A late datagram: its loss counted when the later one came.
            if let Some(b) = self.buckets.iter_mut().rev().find(|b| b.2 > 0) {
                b.2 -= 1;
            }
        }
    }

    /// The loss in parts per million over the last second, or
    /// [`NO_MEASURE`] if fewer than 10 datagrams arrived in it.
    pub fn loss_ppm(&self, now: f64) -> u32 {
        let (mut got, mut lost) = (0u64, 0u64);
        for b in &self.buckets {
            if now - b.0 < BUCKET * BUCKETS as f64 {
                got += b.1;
                lost += b.2;
            }
        }
        if got < 10 {
            return NO_MEASURE;
        }
        (lost * 1_000_000 / (got + lost)) as u32
    }
}

/// How many sent samples the packer keeps for copies.
const RECENT: usize = 128;

/// What the outlet sends for a part of a push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Packet {
    /// One data datagram, ready to send.
    Datagram(Vec<u8>),
    /// The sample with this number does not fit in one datagram. It goes on
    /// TCP, with its `seq` record.
    Tcp(u64),
}

/// Puts samples into data datagrams, with copies of the samples before
/// them. `docs/extensions.md`, section `dgram`, the copies.
///
/// The packer works on encoded samples, so it needs no codec. Each sample
/// must carry its full timestamp.
#[derive(Debug)]
pub struct Packer {
    cookie: u64,
    /// The last samples, with their numbers and the count of copies that
    /// went out, oldest first. The numbers are consecutive.
    recent: std::collections::VecDeque<(u64, Vec<u8>, u8)>,
    /// The count of copies of each sample.
    copies: u8,
    /// The `serial` of the next data datagram.
    serial: u32,
}

impl Packer {
    /// A packer for the connection with this cookie. It starts with no
    /// copies.
    pub fn new(cookie: u64) -> Packer {
        Packer {
            cookie,
            recent: std::collections::VecDeque::new(),
            copies: 0,
            serial: 0,
        }
    }

    /// The count of copies of each sample.
    pub fn copies(&self) -> u8 {
        self.copies
    }

    /// The `serial` of the next data datagram.
    pub fn serial(&self) -> u32 {
        self.serial
    }

    /// Set the count of copies of each sample, at most [`MAX_COPIES`].
    pub fn set_copies(&mut self, copies: u8) {
        self.copies = copies.min(MAX_COPIES);
    }

    /// Put samples of a resend into data datagrams with no copies.
    /// `samples` holds encoded samples with consecutive numbers from
    /// `first`. A sample too large for a datagram is left out.
    pub fn resend(&mut self, first: u64, samples: &[Vec<u8>], out: &mut Vec<Vec<u8>>) {
        let room = MAX_DATAGRAM - DATA_HEADER;
        let mut i = 0;
        while i < samples.len() {
            if samples[i].len() > room {
                i += 1;
                continue;
            }
            let mut j = i;
            let mut used = 0;
            while j < samples.len() && used + samples[j].len() <= room {
                used += samples[j].len();
                j += 1;
            }
            let mut d = Vec::with_capacity(DATA_HEADER + used);
            let serial = self.next_serial();
            data_header(
                &mut d,
                self.cookie,
                first.wrapping_add(i as u64),
                (j - i) as u16,
                serial,
            );
            for bytes in &samples[i..j] {
                d.extend_from_slice(bytes);
            }
            out.push(d);
            i = j;
        }
    }

    fn next_serial(&mut self) -> u32 {
        let s = self.serial;
        self.serial = self.serial.wrapping_add(1);
        s
    }

    /// Put the samples of one push into packets. `samples` holds encoded
    /// samples with consecutive numbers from `first`.
    ///
    /// The new samples fill each datagram first. The free space then takes
    /// copies of the samples just before them, newest first, so that the
    /// samples of a datagram stay consecutive. Each sample goes into at most
    /// `1 + copies` datagrams.
    pub fn push(&mut self, first: u64, samples: &[Vec<u8>], out: &mut Vec<Packet>) {
        let room = MAX_DATAGRAM - DATA_HEADER;
        let mut i = 0;
        while i < samples.len() {
            let n = first.wrapping_add(i as u64);
            if samples[i].len() > room {
                out.push(Packet::Tcp(n));
                // A copy must stand just before the new samples, so the
                // samples before a gap in the run are of no further use.
                self.recent.clear();
                i += 1;
                continue;
            }
            let mut j = i;
            let mut used = 0;
            while j < samples.len() && used + samples[j].len() <= room {
                used += samples[j].len();
                j += 1;
            }
            let copies = self.take_copies(n, room - used);
            out.push(Packet::Datagram(self.datagram(n, copies, &samples[i..j])));
            for (k, sample) in samples[i..j].iter().enumerate() {
                self.remember(n.wrapping_add(k as u64), sample.clone());
            }
            i = j;
        }
    }

    /// A datagram with copies of the last samples and no new sample, or
    /// `None` when each recent sample has all its copies. The outlet sends
    /// it when no data datagram follows for 5 ms.
    pub fn repeat(&mut self) -> Option<Vec<u8>> {
        let next = self.recent.back()?.0.wrapping_add(1);
        let copies = self.take_copies(next, MAX_DATAGRAM - DATA_HEADER);
        if copies == 0 {
            return None;
        }
        Some(self.datagram(next, copies, &[]))
    }

    /// Mark the copies of the samples just before `next`, newest first,
    /// that fit in `room` bytes. The result is their count.
    fn take_copies(&mut self, next: u64, mut room: usize) -> usize {
        let mut count = 0;
        let copies = self.copies;
        for (number, bytes, sent) in self.recent.iter_mut().rev() {
            if *number != next.wrapping_sub(1 + count as u64)
                || *sent >= copies
                || bytes.len() > room
            {
                break;
            }
            room -= bytes.len();
            *sent += 1;
            count += 1;
        }
        count
    }

    /// Build a datagram from the last `copies` recent samples, then the new
    /// samples, which start at the number `next`.
    fn datagram(&mut self, next: u64, copies: usize, new: &[Vec<u8>]) -> Vec<u8> {
        let count = copies + new.len();
        let mut d = Vec::with_capacity(MAX_DATAGRAM);
        let serial = self.next_serial();
        data_header(
            &mut d,
            self.cookie,
            next.wrapping_sub(copies as u64),
            count as u16,
            serial,
        );
        let start = self.recent.len() - copies;
        for (_, bytes, _) in self.recent.range(start..) {
            d.extend_from_slice(bytes);
        }
        for bytes in new {
            d.extend_from_slice(bytes);
        }
        debug_assert!(d.len() <= MAX_DATAGRAM);
        d
    }

    fn remember(&mut self, number: u64, bytes: Vec<u8>) {
        if let Some(last) = self.recent.back() {
            if last.0.wrapping_add(1) != number {
                self.recent.clear();
            }
        }
        self.recent.push_back((number, bytes, 0));
        if self.recent.len() > RECENT {
            self.recent.pop_front();
        }
    }
}

/// What the inlet must do for a gap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Ask the outlet on TCP for `count` samples from `from`.
    Request {
        /// The first missing number.
        from: u64,
        /// The count of missing samples.
        count: u32,
    },
    /// The inlet gave up the gap. The samples count as lost.
    GaveUp {
        /// The first lost number.
        from: u64,
        /// The count of lost samples.
        count: u64,
    },
}

/// Puts the samples of two paths back in the order of their numbers.
/// `docs/extensions.md`, section `dgram`, the resend and the count on two
/// paths.
///
/// The buffer gives each number once, in order. A gap at the expected
/// number waits for a copy, then for a resend, then counts as lost. Time is
/// a value in seconds that the caller gives, so the buffer needs no clock.
#[derive(Debug)]
pub struct Reorder<T> {
    expected: Option<u64>,
    pending: std::collections::BTreeMap<u64, T>,
    /// The number after the highest number that the inlet knows of.
    horizon: Option<u64>,
    gap_since: Option<f64>,
    /// The open request: the numbers `from..until`, and the time of the
    /// request or of the last TCP data.
    requested: Option<(u64, u64, f64)>,
    lost: u64,
    duplicates: u64,
    /// The largest small gap, or 0 when the small gaps go on TCP too.
    small: u64,
    /// The time after which the inlet gives up a small gap at the expected
    /// number.
    small_give_up: f64,
    /// The small gaps, in the order of their numbers.
    holes: Vec<Hole>,
    /// The round trip of a resend on UDP, in seconds.
    rtt: Option<f64>,
    /// The expected number when a small gap stood there first, and the time.
    head: Option<(u64, f64)>,
}

/// One small gap. `docs/extensions.md`, section `dgram`, the resend.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Hole {
    from: u64,
    until: u64,
    /// The time when the gap first showed.
    since: f64,
    /// The time of the last request.
    asked: Option<f64>,
    /// The time of the first request, while it is the only one.
    first_ask: Option<f64>,
}

/// The round trip of a resend before the first measure, in seconds.
const FIRST_RTT: f64 = 0.050;
/// The shortest time between two requests for one small gap, in seconds.
const MIN_REPEAT: f64 = 0.010;

impl<T> Default for Reorder<T> {
    fn default() -> Self {
        Reorder {
            expected: None,
            pending: std::collections::BTreeMap::new(),
            horizon: None,
            gap_since: None,
            requested: None,
            lost: 0,
            duplicates: 0,
            small: 0,
            small_give_up: 0.0,
            holes: Vec::new(),
            rtt: None,
            head: None,
        }
    }
}

impl<T> Reorder<T> {
    /// An empty buffer with no expected number.
    pub fn new() -> Self {
        Reorder::default()
    }

    /// Set the expected number, if the buffer has none yet.
    pub fn start(&mut self, number: u64) {
        if self.expected.is_none() {
            self.expected = Some(number);
        }
    }

    /// Repair each gap of at most `max` samples on UDP: [`Reorder::nacks`]
    /// gives them, [`Reorder::poll`] asks for none of them on TCP, and gives
    /// one up at the expected number `give_up` seconds after it showed.
    pub fn set_small_gaps(&mut self, max: u64, give_up: f64) {
        self.small = max;
        self.small_give_up = give_up;
    }

    /// The round trip of a resend on UDP, once measured.
    pub fn rtt(&self) -> Option<f64> {
        self.rtt
    }

    /// The gaps below the highest known number, as `from..until`, in order.
    fn gaps(&self) -> Vec<(u64, u64)> {
        let mut out = Vec::new();
        let Some(mut at) = self.expected else {
            return out;
        };
        for &n in self.pending.keys() {
            if n > at {
                out.push((at, n));
            }
            at = n.wrapping_add(1);
        }
        if let Some(h) = self.horizon {
            if h > at {
                out.push((at, h));
            }
        }
        out
    }

    /// Find the small gaps again, and keep the times of the old ones. A gap
    /// that the open TCP request covers is not a small gap.
    fn update_holes(&mut self, now: f64) {
        let covered = self.requested.map(|(f, u, _)| (f, u));
        let old = std::mem::take(&mut self.holes);
        let gaps = self.gaps();
        for &(from, until) in &gaps {
            if until - from > self.small {
                continue;
            }
            if covered.is_some_and(|(f, u)| from >= f && until <= u) {
                continue;
            }
            let mut hole = Hole {
                from,
                until,
                since: now,
                asked: None,
                first_ask: None,
            };
            let mut overlaps = 0;
            for o in old.iter().filter(|o| o.from < until && from < o.until) {
                overlaps += 1;
                hole.since = hole.since.min(o.since);
                hole.asked = match (hole.asked, o.asked) {
                    (Some(a), Some(b)) => Some(a.max(b)),
                    (a, b) => a.or(b),
                };
                // The answer to the first request filled a part of the gap.
                if o.first_ask.is_some() && (o.from, o.until) == (from, until) {
                    hole.first_ask = o.first_ask;
                }
            }
            if overlaps > 1 {
                hole.first_ask = None;
            }
            self.holes.push(hole);
        }
        // A gap that got smaller or closed after its first request gives a
        // measure of the round trip.
        for o in &old {
            let Some(t) = o.first_ask else { continue };
            let still: u64 = gaps
                .iter()
                .map(|&(f, u)| u.min(o.until).saturating_sub(f.max(o.from)))
                .sum();
            if still < o.until - o.from {
                let sample = now - t;
                self.rtt = Some(match self.rtt {
                    None => sample,
                    Some(r) => 0.875 * r + 0.125 * sample,
                });
            }
        }
    }

    /// The small gaps to ask for on UDP now: each gap that stayed for
    /// `wait` seconds, again each `max(10 ms, 1.5 x round trip)` until it
    /// closes. At most [`MAX_GAPS`] gaps, from the lowest.
    pub fn nacks(&mut self, now: f64, wait: f64) -> Vec<(u64, u32)> {
        if self.small == 0 {
            return Vec::new();
        }
        self.update_holes(now);
        let every = MIN_REPEAT.max(1.5 * self.rtt.unwrap_or(FIRST_RTT));
        let mut out = Vec::new();
        for h in self.holes.iter_mut() {
            if out.len() == MAX_GAPS {
                break;
            }
            if now - h.since < wait {
                continue;
            }
            if h.asked.is_some_and(|t| now - t < every) {
                continue;
            }
            h.first_ask = if h.asked.is_none() { Some(now) } else { None };
            h.asked = Some(now);
            out.push((h.from, (h.until - h.from) as u32));
        }
        out
    }

    /// The number that the buffer gives next.
    pub fn expected(&self) -> Option<u64> {
        self.expected
    }

    /// The count of numbers that the buffer gave up.
    pub fn lost(&self) -> u64 {
        self.lost
    }

    /// The count of samples that arrived again, or too late.
    pub fn duplicates(&self) -> u64 {
        self.duplicates
    }

    /// Take one sample from either path. The result is false for a sample
    /// that the buffer already has, or that it gave up.
    pub fn insert(&mut self, number: u64, item: T) -> bool {
        let below = self.expected.is_some_and(|e| number < e);
        if below || self.pending.contains_key(&number) {
            self.duplicates += 1;
            return false;
        }
        self.pending.insert(number, item);
        let after = number.wrapping_add(1);
        self.horizon = Some(self.horizon.map_or(after, |h| h.max(after)));
        true
    }

    /// An idle datagram: the next sample of the outlet gets `next`.
    pub fn idle(&mut self, next: u64) {
        self.horizon = Some(self.horizon.map_or(next, |h| h.max(next)));
    }

    /// The outlet has no sample below `number` for this inlet: a resend
    /// started at `number`. The numbers from the expected number up to it
    /// count as lost. `docs/extensions.md`, resend rule 5.
    pub fn lost_below(&mut self, number: u64) {
        if let Some(e) = self.expected {
            if number > e {
                let given_up = (e..number)
                    .filter(|n| !self.pending.contains_key(n))
                    .count();
                self.lost += given_up as u64;
                self.pending = self.pending.split_off(&number);
                self.expected = Some(number);
            }
        }
    }

    /// Data arrived on TCP at `now`. A resend can be slow on a slow link, so
    /// the time limit of an open request counts again from this point.
    /// `docs/extensions.md`, section `dgram`, resend rule 6.
    pub fn progress(&mut self, now: f64) {
        if let Some((from, until, _)) = self.requested {
            self.requested = Some((from, until, now));
        }
    }

    /// The numbers `from..until` are on the way on TCP, as at the start of a
    /// connection with a `resume` resend. The buffer asks for none of them.
    pub fn assume_requested(&mut self, from: u64, until: u64, now: f64) {
        if until > from {
            self.requested = Some((from, until, now));
        }
    }

    /// Move each sample that follows the expected number to `out`, in
    /// order. The result is their count.
    pub fn drain(&mut self, out: &mut Vec<T>) -> usize {
        let Some(mut e) = self.expected else {
            return 0;
        };
        let mut count = 0;
        while let Some(item) = self.pending.remove(&e) {
            out.push(item);
            e = e.wrapping_add(1);
            count += 1;
        }
        self.expected = Some(e);
        count
    }

    /// The end of the gap at the expected number, or `None` with no gap.
    fn gap_end(&self) -> Option<u64> {
        let e = self.expected?;
        match self.pending.keys().next() {
            Some(&first) if first > e => Some(first),
            Some(_) => None,
            None => self.horizon.filter(|&h| h > e),
        }
    }

    /// Check the gap at the expected number. Call it after each
    /// [`Reorder::drain`].
    ///
    /// A gap that stays for `wait` seconds gives a request. A request with no
    /// answer for `give_up` seconds gives up the gap: its numbers count as
    /// lost, and the samples after it can go.
    pub fn poll(&mut self, now: f64, wait: f64, give_up: f64) -> Option<Action> {
        // A request that the expected number passed is complete.
        if let (Some((_, until, _)), Some(e)) = (self.requested, self.expected) {
            if until <= e {
                self.requested = None;
                self.gap_since = None;
            }
        }
        let (Some(e), Some(end)) = (self.expected, self.gap_end()) else {
            self.gap_since = None;
            return None;
        };
        // A small gap at the expected number is repaired on UDP. Its time
        // limit counts from when it reached the expected number: a gap
        // behind a TCP resend waits for that resend first. The limit is at
        // least 4 round trips, for a link with a long queue.
        let in_request = self.requested.is_some_and(|(f, u, _)| e >= f && e < u);
        if self.small > 0 && end - e <= self.small && !in_request {
            self.gap_since = None;
            if self.head.map(|h| h.0) != Some(e) {
                self.head = Some((e, now));
            }
            let since = self.head.map_or(now, |h| h.1);
            let limit = self.small_give_up.max(4.0 * self.rtt.unwrap_or(0.0));
            if now - since >= limit {
                self.lost += end - e;
                self.expected = Some(end);
                self.holes.retain(|h| h.from != e);
                self.head = None;
                return Some(Action::GaveUp {
                    from: e,
                    count: end - e,
                });
            }
            return None;
        }
        match self.requested {
            // The open request covers the start of the gap. No new request
            // for that part: the answer can be on the way. A part beyond the
            // request gets its own request after the wait.
            Some((from, until, at)) => {
                if now - at >= give_up {
                    let stop = end.min(until);
                    self.lost += stop - e;
                    self.expected = Some(stop);
                    self.requested = None;
                    self.gap_since = None;
                    return Some(Action::GaveUp {
                        from: e,
                        count: stop - e,
                    });
                }
                if end > until {
                    let since = *self.gap_since.get_or_insert(now);
                    if now - since >= wait {
                        self.requested = Some((from, end, now));
                        self.gap_since = None;
                        return Some(Action::Request {
                            from: until,
                            count: (end - until).min(u32::MAX as u64) as u32,
                        });
                    }
                }
                None
            }
            None => {
                let since = *self.gap_since.get_or_insert(now);
                if now - since >= wait {
                    self.requested = Some((e, end, now));
                    self.gap_since = None;
                    Some(Action::Request {
                        from: e,
                        count: (end - e).min(u32::MAX as u64) as u32,
                    })
                } else {
                    None
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_datagram_reads_back() {
        assert_eq!(
            parse(&hello(0x0102_0304_0506_0708)),
            Ok(Datagram::Hello {
                cookie: 0x0102_0304_0506_0708
            })
        );
        assert_eq!(
            parse(&idle(7, 99)),
            Ok(Datagram::Idle {
                cookie: 7,
                next: 99
            })
        );
        let mut d = Vec::new();
        data_header(&mut d, 7, 1000, 2, 9);
        d.extend_from_slice(&[1, 2, 3]);
        assert_eq!(d.len(), DATA_HEADER + 3);
        assert_eq!(
            parse(&d),
            Ok(Datagram::Data {
                cookie: 7,
                first: 1000,
                count: 2,
                serial: 9,
                payload: &[1, 2, 3]
            })
        );
    }

    #[test]
    fn the_numbers_are_little_endian() {
        assert_eq!(hello(1)[1..], [1, 0, 0, 0, 0, 0, 0, 0]);
        let mut d = Vec::new();
        data_header(&mut d, 0, 0x0102, 0x0304, 0x0506_0708);
        assert_eq!(d[9..19], [2, 1, 0, 0, 0, 0, 0, 0, 4, 3]);
        assert_eq!(d[19..23], [8, 7, 6, 5]);
    }

    #[test]
    fn a_cut_or_unknown_datagram_is_an_error() {
        assert_eq!(parse(&[]), Err(Error::Truncated));
        assert_eq!(parse(&hello(5)[..8]), Err(Error::Truncated));
        assert_eq!(parse(&idle(5, 6)[..16]), Err(Error::Truncated));
        let mut d = Vec::new();
        data_header(&mut d, 1, 2, 3, 4);
        assert_eq!(parse(&d[..DATA_HEADER - 1]), Err(Error::Truncated));
        assert_eq!(parse(&[9, 0, 0]), Err(Error::BadType(9)));
    }

    #[test]
    fn each_record_reads_back() {
        let mut b = Vec::new();
        write_path(&mut b, Path::Udp, 42);
        write_resend(&mut b, 100, 7);
        write_path_request(&mut b, Path::Tcp, 200);
        let first = peek(&b);
        assert_eq!(
            first,
            Record::Path {
                path: Path::Udp,
                number: 42
            }
        );
        let rest = &b[first.len()..];
        let second = peek(rest);
        assert_eq!(
            second,
            Record::Resend {
                from: 100,
                count: 7
            }
        );
        let rest = &rest[second.len()..];
        let third = peek(rest);
        assert_eq!(
            third,
            Record::PathRequest {
                path: Path::Tcp,
                from: 200
            }
        );
        assert_eq!(rest.len(), third.len());
    }

    #[test]
    fn a_cut_record_asks_for_more_bytes() {
        let mut b = Vec::new();
        write_resend(&mut b, 1, 2);
        for cut in 1..RESEND_LEN {
            assert_eq!(peek(&b[..cut]), Record::Incomplete);
        }
        assert_eq!(peek(&[]), Record::None);
        // A sample tag and the `seq` tag are not records of this module.
        assert_eq!(peek(&[1, 0]), Record::None);
        assert_eq!(peek(&[0xFE, 0]), Record::None);
        assert_eq!(peek(&[PATH_TAG, 7, 0, 0, 0, 0, 0, 0, 0, 0]), Record::Bad);
    }

    #[test]
    fn the_tags_differ_from_the_sample_tags_and_the_seq_tag() {
        for tag in [PATH_TAG, RESEND_TAG, PATH_REQUEST_TAG] {
            assert!(tag != 1 && tag != 2 && tag != crate::seq::TAG);
        }
    }

    // The packer.

    /// A sample of `len` bytes whose bytes all hold the low byte of `n`.
    fn sample(n: u64, len: usize) -> Vec<u8> {
        vec![n as u8; len]
    }

    /// The numbers in each datagram of `out`, for samples of `len` bytes.
    fn numbers(out: &[Packet], len: usize) -> Vec<Vec<u64>> {
        out.iter()
            .filter_map(|p| match p {
                Packet::Datagram(d) => Some(d),
                Packet::Tcp(_) => None,
            })
            .map(|d| {
                assert!(d.len() <= MAX_DATAGRAM);
                match parse(d).unwrap() {
                    Datagram::Data {
                        cookie,
                        first,
                        count,
                        payload,
                        ..
                    } => {
                        assert_eq!(cookie, 77);
                        assert_eq!(payload.len(), count as usize * len);
                        let ns: Vec<u64> = (0..count as u64).map(|k| first + k).collect();
                        // Each sample holds the low byte of its number.
                        for (k, n) in ns.iter().enumerate() {
                            assert_eq!(payload[k * len], *n as u8, "sample {n}");
                        }
                        ns
                    }
                    other => panic!("not a data datagram: {other:?}"),
                }
            })
            .collect()
    }

    /// Push `total` samples of `len` bytes in chunks of `chunk`. With
    /// `repeat`, a repeat datagram follows each push.
    fn run(total: u64, chunk: u64, len: usize, repeat: bool) -> Vec<Vec<u64>> {
        let mut p = Packer::new(77);
        p.set_copies(2);
        let mut out = Vec::new();
        let mut n = 0;
        while n < total {
            let block: Vec<Vec<u8>> = (n..(n + chunk).min(total))
                .map(|k| sample(k, len))
                .collect();
            p.push(n, &block, &mut out);
            if repeat {
                if let Some(d) = p.repeat() {
                    out.push(Packet::Datagram(d));
                }
            }
            n += chunk;
        }
        numbers(&out, len)
    }

    #[test]
    fn each_sample_goes_into_at_most_three_datagrams() {
        for (chunk, len, repeat) in [
            (1, 41, false),
            (16, 41, true),
            (32, 265, true),
            (5, 300, true),
        ] {
            let grams = run(2000, chunk, len, repeat);
            let mut seen = std::collections::HashMap::new();
            for g in &grams {
                for n in g {
                    *seen.entry(*n).or_insert(0) += 1;
                }
            }
            assert_eq!(seen.len(), 2000, "each sample goes out");
            assert!(seen.values().all(|&c| c <= 3), "chunk {chunk}, len {len}");
        }
    }

    /// The numbers that no datagram carries when the datagram `lost` is
    /// lost. The last two samples have no later datagram, so they do not
    /// count.
    fn missing_without(grams: &[Vec<u64>], lost: usize, total: u64) -> Vec<u64> {
        let mut have = std::collections::HashSet::new();
        for (k, g) in grams.iter().enumerate() {
            if k != lost {
                have.extend(g.iter().copied());
            }
        }
        (0..total - 2).filter(|n| !have.contains(n)).collect()
    }

    #[test]
    fn one_lost_datagram_costs_no_sample_at_512_hz_x_8_channels() {
        // One sample of 41 bytes in each push.
        let grams = run(300, 1, 41, false);
        for lost in 0..grams.len() {
            assert_eq!(
                missing_without(&grams, lost, 300),
                Vec::<u64>::new(),
                "lost {lost}"
            );
        }
    }

    #[test]
    fn a_repeat_datagram_covers_a_lost_chunk() {
        // Chunks of 16 samples of 41 bytes, and a repeat after each push.
        let grams = run(320, 16, 41, true);
        for lost in 0..grams.len() {
            assert_eq!(
                missing_without(&grams, lost, 320),
                Vec::<u64>::new(),
                "lost {lost}"
            );
        }
    }

    #[test]
    fn large_samples_fill_a_datagram_with_four() {
        let grams = run(64, 32, 265, false);
        // 32 new samples of 265 bytes need 8 datagrams of 4 samples.
        assert_eq!(grams.len(), 16);
        assert!(grams.iter().all(|g| g.len() <= 4));
    }

    #[test]
    fn a_sample_too_large_for_a_datagram_goes_on_tcp() {
        let mut p = Packer::new(77);
        p.set_copies(2);
        let mut out = Vec::new();
        let block = vec![sample(0, 41), sample(1, 41), sample(2, 2000), sample(3, 41)];
        p.push(0, &block, &mut out);
        assert_eq!(out.len(), 3);
        assert_eq!(out[1], Packet::Tcp(2));
        let grams = numbers(&out, 41);
        assert_eq!(
            grams,
            vec![vec![0, 1], vec![3]],
            "no copy crosses the TCP sample"
        );
    }

    #[test]
    fn a_repeat_with_nothing_to_copy_is_none() {
        let mut p = Packer::new(77);
        p.set_copies(2);
        assert_eq!(p.repeat(), None);
        let mut out = Vec::new();
        p.push(0, &[sample(0, 41)], &mut out);
        assert!(p.repeat().is_some());
        assert!(p.repeat().is_some());
        assert_eq!(p.repeat(), None, "each sample has its two copies");
    }

    // The reorder buffer.

    fn take(r: &mut Reorder<u64>) -> Vec<u64> {
        let mut out = Vec::new();
        r.drain(&mut out);
        out
    }

    #[test]
    fn the_buffer_gives_each_number_once_in_order() {
        let mut r = Reorder::new();
        r.start(10);
        assert!(r.insert(12, 12));
        assert!(r.insert(10, 10));
        assert_eq!(take(&mut r), vec![10]);
        assert!(r.insert(11, 11));
        assert!(!r.insert(11, 11), "a copy goes away");
        assert!(!r.insert(9, 9), "a number below the expected one goes away");
        assert_eq!(take(&mut r), vec![11, 12]);
        assert_eq!(r.duplicates(), 2);
        assert_eq!(r.expected(), Some(13));
    }

    #[test]
    fn a_gap_asks_after_the_wait_and_a_resend_fills_it() {
        let mut r = Reorder::new();
        r.start(0);
        r.insert(0, 0);
        r.insert(2, 2);
        assert_eq!(take(&mut r), vec![0]);
        assert_eq!(r.poll(0.000, 0.010, 0.5), None);
        assert_eq!(r.poll(0.005, 0.010, 0.5), None, "a copy can still come");
        assert_eq!(
            r.poll(0.011, 0.010, 0.5),
            Some(Action::Request { from: 1, count: 1 })
        );
        assert_eq!(r.poll(0.020, 0.010, 0.5), None, "one request for a gap");
        // The resend arrives on TCP.
        r.insert(1, 1);
        assert_eq!(take(&mut r), vec![1, 2]);
        assert_eq!(r.poll(0.030, 0.010, 0.5), None);
        assert_eq!(r.lost(), 0);
    }

    #[test]
    fn a_gap_with_no_answer_counts_as_lost() {
        let mut r = Reorder::new();
        r.start(0);
        r.insert(3, 3);
        assert_eq!(r.poll(0.0, 0.01, 0.5), None);
        assert_eq!(
            r.poll(0.02, 0.01, 0.5),
            Some(Action::Request { from: 0, count: 3 })
        );
        assert_eq!(r.poll(0.30, 0.01, 0.5), None);
        assert_eq!(
            r.poll(0.53, 0.01, 0.5),
            Some(Action::GaveUp { from: 0, count: 3 })
        );
        assert_eq!(take(&mut r), vec![3]);
        assert_eq!(r.lost(), 3);
        assert!(!r.insert(1, 1), "a late sample of the gap goes away");
    }

    #[test]
    fn data_on_tcp_holds_off_the_give_up() {
        let mut r = Reorder::new();
        r.start(0);
        r.insert(3, 3);
        assert_eq!(r.poll(0.0, 0.01, 0.5), None);
        assert_eq!(
            r.poll(0.02, 0.01, 0.5),
            Some(Action::Request { from: 0, count: 3 })
        );
        // The resend is slow: other TCP data arrives at 0.4 and 0.8 s.
        r.progress(0.4);
        assert_eq!(r.poll(0.6, 0.01, 0.5), None);
        r.progress(0.8);
        assert_eq!(r.poll(1.2, 0.01, 0.5), None);
        // The gap fills at 1.25 s.
        for n in 0..3 {
            r.insert(n, n);
        }
        assert_eq!(take(&mut r), vec![0, 1, 2, 3]);
        assert_eq!(r.lost(), 0);
        // With no data, the limit still acts.
        r.insert(6, 6);
        assert!(r.poll(2.0, 0.01, 0.5).is_none());
        assert!(r.poll(2.02, 0.01, 0.5).is_some());
        assert_eq!(
            r.poll(2.53, 0.01, 0.5),
            Some(Action::GaveUp { from: 4, count: 2 })
        );
    }

    #[test]
    fn an_idle_datagram_shows_a_lost_last_sample() {
        let mut r = Reorder::new();
        r.start(0);
        r.insert(0, 0);
        assert_eq!(take(&mut r), vec![0]);
        assert_eq!(r.poll(0.0, 0.01, 0.5), None, "no gap is known");
        r.idle(2);
        assert_eq!(r.poll(0.1, 0.01, 0.5), None);
        assert_eq!(
            r.poll(0.2, 0.01, 0.5),
            Some(Action::Request { from: 1, count: 1 })
        );
    }

    #[test]
    fn a_history_that_holds_less_counts_the_rest_as_lost() {
        let mut r = Reorder::new();
        r.start(0);
        r.insert(5, 5);
        r.insert(8, 8);
        // The outlet resends from 4: 0 to 3 are gone.
        r.lost_below(4);
        r.insert(4, 4);
        assert_eq!(take(&mut r), vec![4, 5]);
        assert_eq!(r.lost(), 4);
    }

    #[test]
    fn random_loss_and_order_on_two_paths_give_each_number_once() {
        // A small random source, so the test needs no crate.
        let mut x: u64 = 0x2545_f491_4f6c_dd1d;
        let mut rnd = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for _ in 0..50 {
            let total = 500u64;
            let mut r = Reorder::new();
            r.start(0);
            let mut out = Vec::new();
            let mut now = 0.0;
            let mut tcp_holds_all = true;
            for n in 0..total {
                // 20 % of the samples come late, 10 % never on UDP, 5 % twice.
                let roll = rnd() % 100;
                if roll >= 10 {
                    r.insert(n, n);
                }
                if roll < 5 {
                    r.insert(n, n);
                }
                if roll < 20 && n >= 3 {
                    r.insert(n - 3, n - 3);
                }
                now += 0.002;
                r.drain(&mut out);
                match r.poll(now, 0.01, 0.5) {
                    Some(Action::Request { from, count }) => {
                        // The TCP resend gives the samples that half of the
                        // time, and nothing the other half.
                        if rnd() % 2 == 0 {
                            for m in from..from + count as u64 {
                                r.insert(m, m);
                            }
                        } else {
                            tcp_holds_all = false;
                        }
                    }
                    Some(Action::GaveUp { .. }) | None => {}
                }
                r.drain(&mut out);
            }
            // Let each open gap end.
            for _ in 0..1000 {
                now += 0.01;
                r.drain(&mut out);
                let _ = r.poll(now, 0.01, 0.5);
            }
            r.drain(&mut out);
            assert!(out.windows(2).all(|w| w[1] > w[0]), "in order, each once");
            let given: std::collections::HashSet<u64> = out.iter().copied().collect();
            let last = *out.last().unwrap();
            let never = (0..=last).filter(|n| !given.contains(n)).count() as u64;
            assert_eq!(r.lost(), never, "lost counts the numbers never given");
            if tcp_holds_all {
                assert_eq!(r.lost(), 0);
            }
        }
    }

    #[test]
    fn a_shrinking_gap_gets_no_new_request() {
        let mut r = Reorder::new();
        r.start(0);
        r.insert(100, 100);
        assert_eq!(r.poll(0.0, 0.01, 0.5), None);
        assert_eq!(
            r.poll(0.02, 0.01, 0.5),
            Some(Action::Request {
                from: 0,
                count: 100
            })
        );
        // The answer arrives in parts. The gap shrinks, and stays covered.
        for n in 0..40 {
            r.insert(n, n);
        }
        take(&mut r);
        r.progress(0.05);
        for t in [0.10, 0.20, 0.40] {
            assert_eq!(r.poll(t, 0.01, 0.5), None, "at {t}");
        }
        // The rest of the answer, then a new gap after it.
        for n in 40..100 {
            r.insert(n, n);
        }
        r.insert(150, 150);
        take(&mut r);
        assert_eq!(r.poll(0.41, 0.01, 0.5), None);
        assert_eq!(
            r.poll(0.43, 0.01, 0.5),
            Some(Action::Request {
                from: 101,
                count: 49
            })
        );
        assert_eq!(r.lost(), 0);
    }

    #[test]
    fn the_range_of_a_resume_resend_gets_no_request() {
        let mut r = Reorder::new();
        r.start(10);
        r.assume_requested(10, 500, 0.0);
        // UDP gives 500 onward while TCP still sends the resend.
        r.insert(500, 500);
        for t in [0.01, 0.1, 0.3] {
            assert_eq!(r.poll(t, 0.01, 0.5), None);
            r.progress(t);
        }
        for n in 10..500 {
            r.insert(n, n);
        }
        assert_eq!(take(&mut r).len(), 491);
        assert_eq!(r.poll(0.4, 0.01, 0.5), None);
        assert_eq!(r.lost(), 0);
    }

    // The repair on UDP and the count of copies.

    #[test]
    fn a_feedback_datagram_reads_back() {
        let d = feedback(5, 1234, &[(10, 2), (40, 1)]);
        assert_eq!(d.len(), FEEDBACK_HEADER + 24);
        match parse(&d) {
            Ok(Datagram::Feedback { cookie, loss, gaps }) => {
                assert_eq!((cookie, loss), (5, 1234));
                assert_eq!(gaps.iter().collect::<Vec<_>>(), vec![(10, 2), (40, 1)]);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(parse(&d[..d.len() - 1]), Err(Error::Truncated));
        let many: Vec<(u64, u32)> = (0..100).map(|k| (k * 10, 1)).collect();
        let d = feedback(5, 0, &many);
        assert!(d.len() <= MAX_DATAGRAM);
        match parse(&d) {
            Ok(Datagram::Feedback { gaps, .. }) => assert_eq!(gaps.iter().count(), MAX_GAPS),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_datagrams_get_consecutive_serials() {
        let mut p = Packer::new(77);
        p.set_copies(2);
        let mut out = Vec::new();
        p.push(0, &[sample(0, 41)], &mut out);
        p.push(1, &[sample(1, 41)], &mut out);
        let mut grams: Vec<Vec<u8>> = out
            .into_iter()
            .map(|p| match p {
                Packet::Datagram(d) => d,
                Packet::Tcp(_) => panic!("no TCP sample"),
            })
            .collect();
        grams.push(p.repeat().unwrap());
        p.resend(0, &[sample(0, 41)], &mut grams);
        let serials: Vec<u32> = grams
            .iter()
            .map(|d| match parse(d) {
                Ok(Datagram::Data { serial, .. }) => serial,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(serials, vec![0, 1, 2, 3]);
    }

    #[test]
    fn with_no_copies_each_sample_goes_once_and_a_repeat_is_none() {
        let grams = {
            let mut p = Packer::new(77);
            let mut out = Vec::new();
            for n in 0..10 {
                p.push(n, &[sample(n, 41)], &mut out);
                assert_eq!(p.repeat(), None);
            }
            numbers(&out, 41)
        };
        assert_eq!(grams, (0..10).map(|n| vec![n]).collect::<Vec<_>>());
    }

    #[test]
    fn a_resend_packs_the_samples_with_no_copies() {
        let mut p = Packer::new(77);
        p.set_copies(8);
        let block: Vec<Vec<u8>> = (100..140).map(|k| sample(k, 41)).collect();
        let mut grams = Vec::new();
        p.resend(100, &block, &mut grams);
        let out: Vec<Packet> = grams.into_iter().map(Packet::Datagram).collect();
        let got: Vec<u64> = numbers(&out, 41).concat();
        assert_eq!(got, (100..140).collect::<Vec<_>>());
    }

    #[test]
    fn the_copies_follow_the_loss_up_at_once_and_down_after_5_s() {
        // A report at `t` seconds, with 1000 datagrams each second.
        let at = |c: &mut CopyControl, loss: u32, t: f64| c.report(loss, t, (t * 1000.0) as u32);
        let mut c = CopyControl::new();
        assert_eq!(c.copies(), 0, "a clean link sends no copy");
        assert_eq!(at(&mut c, NO_MEASURE, 0.0), 0);
        assert_eq!(at(&mut c, 0, 0.1), 0);
        assert_eq!(at(&mut c, 10_000, 0.2), 2, "1 % loss");
        assert_eq!(at(&mut c, 300_000, 0.3), 6, "30 % loss, at once");
        assert_eq!(at(&mut c, 500_000, 0.4), 8, "50 % loss, at once");
        assert_eq!(at(&mut c, 0, 0.5), 8);
        assert_eq!(at(&mut c, 200_000, 3.0), 8, "a lower count waits");
        assert_eq!(at(&mut c, 0, 5.4), 8);
        assert_eq!(
            at(&mut c, 0, 5.5),
            4,
            "after 5 s, the highest count of the lower reports"
        );
        assert_eq!(at(&mut c, 0, 9.0), 4);
        assert_eq!(at(&mut c, 0, 10.6), 0);
        assert_eq!(copies_for(20_000), 2);
        assert_eq!(copies_for(20_001), 3);
        assert_eq!(copies_for(400_001), MAX_COPIES);
    }

    #[test]
    fn with_few_datagrams_the_copies_stay_longer_than_5_s() {
        // 64 datagrams each second, as with chunks of 16 at 512 Hz.
        let at = |c: &mut CopyControl, loss: u32, t: f64| c.report(loss, t, (t * 64.0) as u32);
        let mut c = CopyControl::new();
        assert_eq!(at(&mut c, 10_000, 0.0), 2);
        let mut t = 0.1;
        while t < 15.0 {
            assert_eq!(
                at(&mut c, 0, t),
                2,
                "at {t} s, {} datagrams",
                (t * 64.0) as u32
            );
            t += 0.1;
        }
        assert_eq!(at(&mut c, 0, 15.8), 0, "after 1000 datagrams");
    }

    #[test]
    fn the_loss_meter_counts_the_gaps_in_serial_but_not_a_cut() {
        let mut m = LossMeter::new();
        assert_eq!(m.loss_ppm(0.0), NO_MEASURE);
        // 100 datagrams in 0.2 s, each 5th one lost, also at the border of
        // two buckets.
        for k in 0..100u32 {
            if k % 5 != 3 {
                m.record(k, k as f64 * 0.002);
            }
        }
        let loss = m.loss_ppm(0.2);
        assert_eq!(loss, 200_000);
        // A late datagram takes back its loss.
        let mut m = LossMeter::new();
        for (i, k) in [0u32, 1, 3, 2, 4, 5, 6, 7, 8, 9, 10]
            .into_iter()
            .enumerate()
        {
            m.record(k, 0.01 * i as f64);
        }
        assert_eq!(m.loss_ppm(0.2), 0);
        // A cut of 2 s, then a clean link: the old buckets leave the window,
        // and the jump in serial counts as no loss.
        let mut m = LossMeter::new();
        for k in 0..50u32 {
            m.record(k, k as f64 * 0.002);
        }
        for k in 0..50u32 {
            m.record(5000 + k, 2.0 + k as f64 * 0.002);
        }
        assert_eq!(m.loss_ppm(2.1), 0);
    }

    #[test]
    fn the_small_gaps_are_asked_for_on_udp_in_parallel() {
        let mut r = Reorder::new();
        r.set_small_gaps(64, 1.0);
        r.start(0);
        for n in [0, 2, 3, 5, 6, 7, 100] {
            r.insert(n, n);
        }
        assert_eq!(take(&mut r), vec![0]);
        assert_eq!(r.nacks(0.000, 0.010), vec![], "a copy can still come");
        assert_eq!(r.poll(0.000, 0.010, 0.5), None);
        assert_eq!(
            r.nacks(0.011, 0.010),
            vec![(1, 1), (4, 1)],
            "each small gap at once; the large gap 8..100 is not one"
        );
        assert_eq!(r.poll(0.011, 0.010, 0.5), None, "no request on TCP");
        assert_eq!(r.nacks(0.030, 0.010), vec![], "too soon to ask again");
        assert_eq!(
            r.nacks(0.090, 0.010),
            vec![(1, 1), (4, 1)],
            "again after 1.5 x 50 ms"
        );
        // The answer to the second request gives no measure (only the first
        // request of a gap counts).
        r.insert(1, 1);
        let _ = r.nacks(0.100, 0.010);
        assert_eq!(r.rtt(), None);
        assert_eq!(take(&mut r), vec![1, 2, 3]);
        // A gap that stays for 1 s at the expected number is lost.
        assert_eq!(r.poll(0.5, 0.010, 0.5), None, "4 is now at E");
        assert_eq!(r.poll(1.4, 0.010, 0.5), None);
        assert_eq!(
            r.poll(1.5, 0.010, 0.5),
            Some(Action::GaveUp { from: 4, count: 1 })
        );
        assert_eq!(take(&mut r), vec![5, 6, 7]);
        assert_eq!(r.lost(), 1);
    }

    #[test]
    fn the_first_answer_measures_the_round_trip() {
        let mut r = Reorder::new();
        r.set_small_gaps(64, 1.0);
        r.start(0);
        r.insert(0, 0);
        r.insert(2, 2);
        take(&mut r);
        assert_eq!(r.nacks(0.0, 0.010), vec![]);
        assert_eq!(r.nacks(0.02, 0.010), vec![(1, 1)]);
        r.insert(1, 1);
        assert_eq!(r.nacks(0.06, 0.010), vec![]);
        let rtt = r.rtt().expect("a measure");
        assert!((rtt - 0.04).abs() < 1e-9, "{rtt}");
        // The next gap is asked again after 1.5 x 40 ms.
        r.insert(4, 4);
        take(&mut r);
        assert_eq!(r.nacks(0.09, 0.010), vec![]);
        assert_eq!(r.nacks(0.10, 0.010), vec![(3, 1)]);
        assert_eq!(r.nacks(0.159, 0.010), vec![]);
        assert_eq!(r.nacks(0.161, 0.010), vec![(3, 1)]);
    }

    #[test]
    fn a_small_gap_behind_a_tcp_resend_gets_its_full_time_at_the_expected_number() {
        let mut r = Reorder::new();
        r.set_small_gaps(64, 1.0);
        r.start(0);
        // The `resume` resend 0..100 is on the way on TCP, and the live
        // samples on UDP have a small gap at 150.
        r.assume_requested(0, 100, 0.0);
        for n in (100..150).chain(151..200) {
            r.insert(n, n);
        }
        assert_eq!(r.nacks(0.0, 0.010), vec![]);
        assert_eq!(r.nacks(0.02, 0.010), vec![(150, 1)], "asked for at once");
        // The TCP resend ends after 1.5 s on a slow link. Its data holds
        // off the give-up of the TCP part.
        for t in [0.4, 0.8, 1.2] {
            r.progress(t);
            assert_eq!(r.poll(t, 0.010, 0.5), None);
        }
        for n in 0..100 {
            r.progress(1.5);
            r.insert(n, n);
        }
        assert_eq!(take(&mut r).len(), 150);
        assert_eq!(r.poll(1.5, 0.010, 0.5), None, "the gap just reached E");
        assert_eq!(r.poll(2.4, 0.010, 0.5), None);
        assert_eq!(
            r.poll(2.5, 0.010, 0.5),
            Some(Action::GaveUp {
                from: 150,
                count: 1
            })
        );
    }

    #[test]
    fn on_a_link_with_a_long_queue_a_small_gap_waits_4_round_trips() {
        let mut r = Reorder::new();
        r.set_small_gaps(64, 1.0);
        r.start(0);
        r.insert(0, 0);
        r.insert(2, 2);
        take(&mut r);
        assert_eq!(r.nacks(0.0, 0.010), vec![]);
        assert_eq!(r.nacks(0.02, 0.010), vec![(1, 1)]);
        // The answer comes after 0.5 s.
        r.insert(1, 1);
        let _ = r.nacks(0.52, 0.010);
        assert!((r.rtt().unwrap() - 0.5).abs() < 1e-9);
        take(&mut r);
        r.insert(4, 4);
        assert_eq!(r.poll(1.0, 0.010, 0.5), None);
        assert_eq!(r.poll(2.9, 0.010, 0.5), None, "4 x 0.5 s, not 1 s");
        assert_eq!(
            r.poll(3.0, 0.010, 0.5),
            Some(Action::GaveUp { from: 3, count: 1 })
        );
    }

    #[test]
    fn a_gap_in_the_resume_resend_is_not_a_small_gap() {
        let mut r = Reorder::new();
        r.set_small_gaps(64, 1.0);
        r.start(0);
        r.assume_requested(0, 50, 0.0);
        r.insert(10, 10);
        assert_eq!(r.nacks(0.1, 0.010), vec![]);
        assert_eq!(r.poll(0.1, 0.010, 0.5), None);
    }

    /// A small random number source, so the simulation repeats.
    struct Rng(u64);

    impl Rng {
        fn unit(&mut self) -> f64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 11) as f64 / (1u64 << 53) as f64
        }
    }

    /// The result of one simulated run.
    #[derive(Debug)]
    struct Sim {
        lost: u64,
        p99: f64,
        bytes: usize,
    }

    /// Simulate 10 s of a 512 Hz x 8 channel stream in chunks of `chunk`, with `loss`
    /// in each direction and 20 ms delay. With `repair`, the inlet asks for
    /// the small gaps on UDP and the outlet follows the measured loss. With
    /// no `repair`, the outlet sends 2 copies and a request on TCP gets no
    /// answer, as with TCP at heavy loss.
    fn simulate(loss: f64, repair: bool, seed: u64, chunk: u64) -> Sim {
        const DELAY: f64 = 0.020;
        const RATE: f64 = 512.0;
        const LEN: usize = 41;
        let total = (RATE * 10.0) as u64;
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let mut packer = Packer::new(1);
        let mut control = CopyControl::new();
        if !repair {
            packer.set_copies(2);
        }
        let mut reorder: Reorder<u64> = Reorder::new();
        if repair {
            reorder.set_small_gaps(64, 1.0);
        }
        let mut meter = LossMeter::new();
        let mut forward: Vec<(f64, Vec<u8>)> = Vec::new();
        let mut back: Vec<(f64, Vec<u8>)> = Vec::new();
        let mut resent_at = std::collections::HashMap::new();
        let mut delays = Vec::new();
        let (mut next, mut last_data, mut repeated, mut bytes) = (0u64, 0.0, true, 0usize);
        let mut idles = 0usize;
        let mut last_feedback = 0.0;
        let mut started = false;
        let send = |d: Vec<u8>, t: f64, rng: &mut Rng, to: &mut Vec<(f64, Vec<u8>)>| {
            if rng.unit() >= loss {
                to.push((t + DELAY, d));
            }
        };
        // The mean time between two datagrams at the inlet, as in the inlet.
        let (mut period, mut last_arrival) = (0.0f64, 0.0f64);
        let mut pushed = vec![0.0f64; total as usize];
        for tick in 0..13_000u64 {
            let t = tick as f64 * 0.001;
            // The outlet: new samples, the repeat, and the idle datagrams.
            while next < total && (next + chunk.min(total - next)) as f64 / RATE <= t {
                let n = chunk.min(total - next);
                let block: Vec<Vec<u8>> = (0..n).map(|_| vec![0u8; LEN]).collect();
                let mut out = Vec::new();
                packer.push(next, &block, &mut out);
                for p in out {
                    if let Packet::Datagram(d) = p {
                        bytes += d.len();
                        send(d, t, &mut rng, &mut forward);
                    }
                }
                for k in next..next + n {
                    pushed[k as usize] = t;
                }
                next += n;
                last_data = t;
                repeated = false;
                idles = 0;
            }
            if !repeated && t - last_data >= 0.005 {
                let d = packer.repeat().unwrap_or_else(|| idle(1, next).to_vec());
                send(d, t, &mut rng, &mut forward);
                repeated = true;
            }
            let tail = [0.025, 0.100, 0.250];
            if idles < tail.len() && t - last_data >= tail[idles] {
                send(idle(1, next).to_vec(), t, &mut rng, &mut forward);
                idles += 1;
            }
            // The outlet: the feedback of the inlet.
            let (due, rest): (Vec<_>, Vec<_>) = back.drain(..).partition(|(at, _)| *at <= t);
            back = rest;
            for (_, d) in due {
                let Ok(Datagram::Feedback { loss: l, gaps, .. }) = parse(&d) else {
                    panic!("a feedback datagram");
                };
                packer.set_copies(control.report(l, t, packer.serial()));
                let mut budget = 256;
                for (from, count) in gaps.iter() {
                    let numbers: Vec<u64> = (from..from + count as u64)
                        .filter(|n| *n < next)
                        .filter(|n| resent_at.get(n).map_or(true, |&a: &f64| t - a >= 0.010))
                        .take(budget)
                        .collect();
                    budget -= numbers.len();
                    for &n in &numbers {
                        resent_at.insert(n, t);
                        let mut grams = Vec::new();
                        packer.resend(n, &[vec![0u8; LEN]], &mut grams);
                        for g in grams {
                            let times = if control.copies() >= 3 { 2 } else { 1 };
                            for _ in 0..times {
                                send(g.clone(), t, &mut rng, &mut forward);
                            }
                        }
                    }
                }
            }
            // The inlet.
            let (due, rest): (Vec<_>, Vec<_>) = forward.drain(..).partition(|(at, _)| *at <= t);
            forward = rest;
            for (_, d) in due {
                let gap = t - last_arrival;
                period = if period == 0.0 {
                    gap
                } else {
                    0.9 * period + 0.1 * gap
                };
                last_arrival = t;
                match parse(&d) {
                    Ok(Datagram::Data {
                        first,
                        count,
                        serial,
                        ..
                    }) => {
                        if !started {
                            reorder.start(first);
                            started = true;
                        }
                        meter.record(serial, t);
                        for k in 0..count as u64 {
                            reorder.insert(first + k, first + k);
                        }
                    }
                    Ok(Datagram::Idle { next, .. }) => reorder.idle(next),
                    other => panic!("{other:?}"),
                }
            }
            let wait = MIN_REPEAT.max(2.0 * period);
            let mut out = Vec::new();
            reorder.drain(&mut out);
            let _ = reorder.poll(t, wait, 0.5);
            reorder.drain(&mut out);
            for n in out {
                if n as f64 / RATE >= 1.0 {
                    delays.push(t - pushed[n as usize]);
                    if std::env::var_os("SIMDBG").is_some() && t - pushed[n as usize] > 0.035 {
                        eprintln!(
                            "late {n} pushed {:.3} delay {:.3} copies {}",
                            pushed[n as usize],
                            t - pushed[n as usize],
                            packer.copies()
                        );
                    }
                }
            }
            if repair {
                let gaps = reorder.nacks(t, wait);
                if !gaps.is_empty() || t - last_feedback >= 0.1 {
                    send(
                        feedback(1, meter.loss_ppm(t), &gaps),
                        t,
                        &mut rng,
                        &mut back,
                    );
                    last_feedback = t;
                }
            }
        }
        let delivered = delays.len() as u64 + RATE as u64;
        delays.sort_by(|a, b| a.partial_cmp(b).unwrap());
        Sim {
            lost: total.saturating_sub(delivered),
            p99: delays[delays.len() * 99 / 100],
            bytes,
        }
    }

    #[test]
    fn at_30_percent_loss_the_repair_on_udp_loses_no_sample() {
        for seed in 1..=4 {
            let s = simulate(0.30, true, seed, 1);
            assert_eq!(s.lost, 0, "seed {seed}: {s:?}");
        }
    }

    #[test]
    fn at_50_percent_loss_the_repair_on_udp_loses_at_most_0_1_percent() {
        for seed in 1..=4 {
            let s = simulate(0.50, true, seed, 1);
            assert!(s.lost <= 5, "seed {seed}: {s:?}");
        }
    }

    #[test]
    fn without_the_repair_on_udp_30_percent_loss_loses_samples() {
        let s = simulate(0.30, false, 1, 1);
        assert!(s.lost > 0, "{s:?}");
    }

    #[test]
    fn at_1_percent_loss_the_p99_stays_near_the_delay() {
        for seed in 1..=4 {
            let s = simulate(0.01, true, seed, 1);
            assert_eq!(s.lost, 0, "seed {seed}: {s:?}");
            assert!(s.p99 < 0.035, "seed {seed}: {s:?}");
        }
    }

    #[test]
    fn at_1_percent_loss_with_chunks_of_16_the_p99_stays_near_the_delay() {
        // Chunks of 16 give few datagrams, so a window of 1 s often shows no
        // loss. The copies must not go away on such thin evidence.
        for seed in 1..=8 {
            let s = simulate(0.01, true, seed, 16);
            assert_eq!(s.lost, 0, "seed {seed}: {s:?}");
            assert!(s.p99 < 0.035, "seed {seed}: {s:?}");
        }
    }

    #[test]
    fn a_clean_link_sends_no_copy() {
        let clean = simulate(0.0, true, 1, 1);
        let fixed = simulate(0.0, false, 1, 1);
        assert_eq!(clean.lost, 0);
        assert_eq!(clean.bytes, 5120 * (DATA_HEADER + 41), "{clean:?}");
        assert!(fixed.bytes > 2 * clean.bytes, "{fixed:?}");
    }
}
