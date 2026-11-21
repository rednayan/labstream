//! The wire formats of the `dgram/1` feature. `docs/extensions.md`, section
//! `dgram`.
//!
//! Three datagrams go between the two UDP sockets: the hello of the inlet,
//! and the data and idle datagrams of the outlet. Three records go on the TCP
//! connection: the path record of the outlet, and the resend request and
//! path request of the inlet. Each number is little-endian, whatever the
//! byte order of the connection.

/// The largest datagram, the header included. This size passes most links
/// with no fragmentation of IP.
pub const MAX_DATAGRAM: usize = 1200;

/// The length of the header of a data datagram.
pub const DATA_HEADER: usize = 1 + 8 + 8 + 2;

/// The largest sample of a stream that can use `dgram/1`. A datagram then
/// holds at least 3 samples, so the copies can work.
pub const MAX_SAMPLE_BYTES: usize = 400;

/// The type byte of a hello datagram.
pub const HELLO: u8 = 1;
/// The type byte of a data datagram.
pub const DATA: u8 = 2;
/// The type byte of an idle datagram.
pub const IDLE: u8 = 3;

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
pub fn data_header(out: &mut Vec<u8>, cookie: u64, first: u64, count: u16) {
    out.push(DATA);
    out.extend_from_slice(&cookie.to_le_bytes());
    out.extend_from_slice(&first.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
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
            payload: &buf[DATA_HEADER..],
        }),
        IDLE if buf.len() >= 17 => Ok(Datagram::Idle {
            cookie: u64_at(buf, 1),
            next: u64_at(buf, 9),
        }),
        HELLO | DATA | IDLE => Err(Error::Truncated),
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

/// How many later datagrams carry a copy of a sample.
pub const COPIES: u8 = 2;

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
}

impl Packer {
    /// A packer for the connection with this cookie.
    pub fn new(cookie: u64) -> Packer {
        Packer {
            cookie,
            recent: std::collections::VecDeque::new(),
        }
    }

    /// Put the samples of one push into packets. `samples` holds encoded
    /// samples with consecutive numbers from `first`.
    ///
    /// The new samples fill each datagram first. The free space then takes
    /// copies of the samples just before them, newest first, so that the
    /// samples of a datagram stay consecutive. Each sample goes into at most
    /// `1 + COPIES` datagrams.
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
        for (number, bytes, sent) in self.recent.iter_mut().rev() {
            if *number != next.wrapping_sub(1 + count as u64)
                || *sent >= COPIES
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
    fn datagram(&self, next: u64, copies: usize, new: &[Vec<u8>]) -> Vec<u8> {
        let count = copies + new.len();
        let mut d = Vec::with_capacity(MAX_DATAGRAM);
        data_header(
            &mut d,
            self.cookie,
            next.wrapping_sub(copies as u64),
            count as u16,
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
    requested: Option<(u64, f64)>,
    lost: u64,
    duplicates: u64,
}

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
        let (Some(e), Some(end)) = (self.expected, self.gap_end()) else {
            self.gap_since = None;
            self.requested = None;
            return None;
        };
        // A request for a different number belongs to an older gap.
        if self.requested.is_some_and(|(from, _)| from != e) {
            self.requested = None;
            self.gap_since = Some(now);
        }
        let since = *self.gap_since.get_or_insert(now);
        match self.requested {
            None if now - since >= wait => {
                self.requested = Some((e, now));
                Some(Action::Request {
                    from: e,
                    count: (end - e).min(u32::MAX as u64) as u32,
                })
            }
            Some((_, at)) if now - at >= give_up => {
                self.lost += end - e;
                self.expected = Some(end);
                self.gap_since = None;
                self.requested = None;
                Some(Action::GaveUp {
                    from: e,
                    count: end - e,
                })
            }
            _ => None,
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
        data_header(&mut d, 7, 1000, 2);
        d.extend_from_slice(&[1, 2, 3]);
        assert_eq!(d.len(), DATA_HEADER + 3);
        assert_eq!(
            parse(&d),
            Ok(Datagram::Data {
                cookie: 7,
                first: 1000,
                count: 2,
                payload: &[1, 2, 3]
            })
        );
    }

    #[test]
    fn the_numbers_are_little_endian() {
        assert_eq!(hello(1)[1..], [1, 0, 0, 0, 0, 0, 0, 0]);
        let mut d = Vec::new();
        data_header(&mut d, 0, 0x0102, 0x0304);
        assert_eq!(d[9..19], [2, 1, 0, 0, 0, 0, 0, 0, 4, 3]);
    }

    #[test]
    fn a_cut_or_unknown_datagram_is_an_error() {
        assert_eq!(parse(&[]), Err(Error::Truncated));
        assert_eq!(parse(&hello(5)[..8]), Err(Error::Truncated));
        assert_eq!(parse(&idle(5, 6)[..16]), Err(Error::Truncated));
        let mut d = Vec::new();
        data_header(&mut d, 1, 2, 3);
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
}
