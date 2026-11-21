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
}
