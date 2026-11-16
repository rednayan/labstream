//! The sequence numbers of the `seq/1` feature. `docs/extensions.md`.
//!
//! A numbering record can stand between two samples. It gives the number of
//! the next sample. The outlet writes one record at the start of each
//! connection, and one more before each sample whose number is not one more
//! than the number of the sample before it. The inlet counts the numbers
//! that never arrived.

/// The first byte of a numbering record.
///
/// The sample tags are 1 and 2 (`src/sample.h:19-20`, SPEC.md 7.1). This
/// value is neither, so the first byte tells a record from a sample.
pub const TAG: u8 = 0xFE;

/// The length of a numbering record in bytes.
pub const RECORD_LEN: usize = 9;

/// Append a record that gives `number` to the next sample.
///
/// The number is always little-endian. The byte order of the connection does
/// not apply to it.
pub fn write_record(out: &mut Vec<u8>, number: u64) {
    out.push(TAG);
    out.extend_from_slice(&number.to_le_bytes());
}

/// What starts at a position in the sample stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    /// A complete record, with its number.
    Record(u64),
    /// A record that is cut. The caller reads more bytes.
    Incomplete,
    /// No record. The bytes start a sample, or nothing.
    Sample,
}

/// Look at the start of `buf` for a record.
pub fn peek(buf: &[u8]) -> Next {
    match buf.first() {
        Some(&TAG) if buf.len() >= RECORD_LEN => {
            let mut b = [0u8; 8];
            b.copy_from_slice(&buf[1..RECORD_LEN]);
            Next::Record(u64::from_le_bytes(b))
        }
        Some(&TAG) => Next::Incomplete,
        _ => Next::Sample,
    }
}

/// The count of lost samples on the inlet side.
///
/// The counter keeps the number that it expects next. A record above that
/// number adds the difference to the loss. Each sample moves the expected
/// number by one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Counter {
    expected: Option<u64>,
    lost: u64,
    backwards: u64,
}

impl Counter {
    /// A counter with no expected number yet.
    pub fn new() -> Self {
        Counter::default()
    }

    /// Read a record.
    ///
    /// The first record sets the expected number and counts no loss. A number
    /// below the expected number counts no loss: it adds one to
    /// [`Counter::backwards`], and the counter takes the new number.
    pub fn record(&mut self, number: u64) {
        if let Some(e) = self.expected {
            if number > e {
                self.lost = self.lost.saturating_add(number - e);
            } else if number < e {
                self.backwards = self.backwards.saturating_add(1);
            }
        }
        self.expected = Some(number);
    }

    /// Count one sample.
    pub fn sample(&mut self) {
        if let Some(e) = self.expected {
            self.expected = Some(e.saturating_add(1));
        }
    }

    /// Forget the expected number. A new outlet has a new counter.
    pub fn restart(&mut self) {
        self.expected = None;
    }

    /// The samples that never arrived, since the counter was made.
    pub fn lost(&self) -> u64 {
        self.lost
    }

    /// How many records gave a number below the expected one.
    pub fn backwards(&self) -> u64 {
        self.backwards
    }

    /// The number that the counter expects next, if it has one.
    pub fn expected(&self) -> Option<u64> {
        self.expected
    }
}

/// The record decision on the outlet side, for one connection.
///
/// The writer calls [`Numbering::before`] with the number of each sample that
/// it is about to write. The result tells whether a record goes first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Numbering {
    next: Option<u64>,
}

impl Numbering {
    /// A new connection, which starts with a record.
    pub fn new() -> Self {
        Numbering::default()
    }

    /// Write the record that opens a connection. `number` is the number
    /// that the next sample will get.
    ///
    /// The record goes out at once, before any sample. A gap that happened
    /// before this connection is then visible to the inlet at once, also on
    /// a stream that sends no more samples for a long time.
    pub fn start(&mut self, number: u64, out: &mut Vec<u8>) {
        write_record(out, number);
        self.next = Some(number);
    }

    /// Write a record into `out` if the sample with `number` needs one, then
    /// expect the number after it.
    pub fn before(&mut self, number: u64, out: &mut Vec<u8>) {
        if self.next != Some(number) {
            write_record(out, number);
        }
        self.next = Some(number.wrapping_add(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_reads_back() {
        for n in [0u64, 1, 255, 1 << 40, u64::MAX] {
            let mut out = Vec::new();
            write_record(&mut out, n);
            assert_eq!(out.len(), RECORD_LEN);
            assert_eq!(peek(&out), Next::Record(n));
        }
    }

    #[test]
    fn the_number_is_little_endian() {
        let mut out = Vec::new();
        write_record(&mut out, 0x0102_0304_0506_0708);
        assert_eq!(out, [0xFE, 8, 7, 6, 5, 4, 3, 2, 1]);
    }

    #[test]
    fn a_cut_record_asks_for_more_bytes() {
        let mut out = Vec::new();
        write_record(&mut out, 42);
        for cut in 1..RECORD_LEN {
            assert_eq!(peek(&out[..cut]), Next::Incomplete);
        }
        assert_eq!(peek(&[]), Next::Sample);
        assert_eq!(peek(&[1, 0, 0]), Next::Sample);
        assert_eq!(peek(&[2]), Next::Sample);
    }

    #[test]
    fn the_first_record_counts_no_loss() {
        let mut c = Counter::new();
        c.record(1000);
        assert_eq!(c.lost(), 0);
        assert_eq!(c.expected(), Some(1000));
    }

    #[test]
    fn a_gap_counts_the_missing_numbers() {
        let mut c = Counter::new();
        c.record(10);
        c.sample(); // 10
        c.sample(); // 11
        c.record(15); // 12, 13, 14 are lost
        c.sample();
        assert_eq!(c.lost(), 3);
        assert_eq!(c.expected(), Some(16));
    }

    #[test]
    fn a_number_that_goes_back_counts_no_loss_and_does_not_underflow() {
        let mut c = Counter::new();
        c.record(100);
        c.sample();
        c.record(5);
        assert_eq!(c.lost(), 0);
        assert_eq!(c.backwards(), 1);
        assert_eq!(c.expected(), Some(5));
    }

    #[test]
    fn a_restart_takes_a_new_baseline() {
        let mut c = Counter::new();
        c.record(50);
        c.restart();
        c.record(0);
        c.record(3);
        assert_eq!(c.lost(), 3);
        assert_eq!(c.backwards(), 0);
    }

    #[test]
    fn a_started_connection_needs_no_second_record_for_the_next_number() {
        let mut w = Numbering::new();
        let mut out = Vec::new();
        w.start(30, &mut out);
        assert_eq!(peek(&out), Next::Record(30));
        let mut more = Vec::new();
        w.before(30, &mut more);
        assert!(more.is_empty(), "the next sample follows the record");
        w.before(35, &mut more);
        assert_eq!(peek(&more), Next::Record(35));
    }

    #[test]
    fn the_writer_and_the_counter_agree() {
        // The numbers that a writer sends, with gaps from dropped samples.
        let numbers = [7u64, 8, 9, 12, 13, 20, 21, 22];
        let mut w = Numbering::new();
        let mut c = Counter::new();
        let mut records = 0;
        for n in numbers {
            let mut out = Vec::new();
            w.before(n, &mut out);
            if let Next::Record(r) = peek(&out) {
                records += 1;
                c.record(r);
            }
            c.sample();
        }
        assert_eq!(records, 3, "one at the start and one at each gap");
        assert_eq!(c.lost(), 2 + 6);
        assert_eq!(c.expected(), Some(23));
    }
}
