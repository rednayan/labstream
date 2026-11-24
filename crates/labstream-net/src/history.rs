//! The history of an outlet for the `resume` feature (`docs/extensions.md`,
//! section `resume`) and disk-backed store-and-forward (`docs/roadmap.md`,
//! step 3).
//!
//! The history holds the last samples of the outlet, with full timestamps,
//! in the order of their `seq` numbers. A consumer that connects again after
//! a dropout gets the samples that it did not receive from here.
//!
//! When `disk_bytes > 0` (`labstream.ResumeDisk`), samples evicted from RAM
//! spill into bounded immutable segment files on disk. Reading a segment
//! during a resend happens outside the `Fanout` lock so live pushes and
//! other consumers do not wait on disk I/O. Every spool file is removed when
//! the segment ages out or when the outlet closes.
//!
//! liblsl has no history. Its outlet makes an empty queue for each new
//! connection (`src/tcp_server.cpp:746`, `src/consumer_queue.cpp:10-20`).

use labstream_wire::{Format, PackedSample, PackedValues, Sample, Value, IRREGULAR_RATE};
use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// The bytes that the history counts for each sample: the timestamp and the
/// number.
const SAMPLE_BYTES: usize = 16;

/// The bytes that the history counts for one value in the legacy [`Sample`]
/// representation, retained for [`sample_bytes`].
const VALUE_BYTES: usize = std::mem::size_of::<Value>();

/// Sparse byte-offset index stride inside one disk segment, in samples.
const INDEX_STRIDE: usize = 256;

/// On-disk per-record header size: `seq (8) + ts (8) + fmt (1) + len (4)`.
const RECORD_HEADER_BYTES: usize = 21;

/// The limits of a history. The history removes its oldest samples when it
/// reaches them.
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryLimits {
    /// The time of samples at the nominal rate. `labstream.ResumeSeconds`.
    /// The value 0 turns the history off.
    pub seconds: f64,
    /// The memory limit in bytes. `labstream.ResumeMemory`, which is in MiB.
    pub bytes: usize,
    /// The disk spool limit in bytes. `labstream.ResumeDisk`, which is in MiB.
    /// Zero keeps the history in memory only (the default).
    pub disk_bytes: u64,
    /// Optional directory for disk spool files (`labstream.ResumeDiskPath`).
    /// `None` uses the system temporary directory.
    pub disk_dir: Option<PathBuf>,
}

impl HistoryLimits {
    /// The defaults of `docs/extensions.md`: 30 s, 64 MiB RAM, no disk spool.
    ///
    /// A cut of 10 s, plus the time to connect again, gave a gap of about
    /// 11.8 s in the lossy-link benchmark (`docs/roadmap.md`, step 0). A
    /// default of 10 s lost the oldest samples of that gap.
    pub const DEFAULT: HistoryLimits = HistoryLimits {
        seconds: 30.0,
        bytes: 64 << 20,
        disk_bytes: 0,
        disk_dir: None,
    };

    /// A history with these limits holds no sample.
    pub const OFF: HistoryLimits = HistoryLimits {
        seconds: 0.0,
        bytes: 0,
        disk_bytes: 0,
        disk_dir: None,
    };

    /// True when these limits let a history hold a sample.
    pub fn allow_history(&self) -> bool {
        self.seconds > 0.0 && (self.bytes > 0 || self.disk_bytes > 0)
    }

    /// The count of samples that the time limit allows at a nominal rate.
    ///
    /// A stream with an irregular rate, or an infinite time limit, has no
    /// count limit and uses the memory and disk limits only.
    fn max_samples(&self, srate: f64) -> usize {
        if srate == IRREGULAR_RATE || !srate.is_finite() || srate < 0.0 || !self.seconds.is_finite()
        {
            return usize::MAX;
        }
        let n = (self.seconds * srate).ceil();
        if n >= usize::MAX as f64 {
            usize::MAX
        } else {
            (n as usize).max(1)
        }
    }
}

impl Default for HistoryLimits {
    fn default() -> Self {
        HistoryLimits::DEFAULT
    }
}

/// The memory that the history counts for one sample.
///
/// The count is 16 bytes, plus the size of a `Value` for each channel, plus
/// the bytes of each string value. `docs/extensions.md`, history rule 5.
pub fn sample_bytes(s: &Sample) -> usize {
    let strings: usize = s
        .values
        .iter()
        .map(|v| match v {
            Value::Str(b) => b.len(),
            _ => 0,
        })
        .sum();
    SAMPLE_BYTES + VALUE_BYTES * s.values.len() + strings
}

/// The memory that the history counts for one [`PackedSample`].
///
/// For numeric streams, this is `16 + channels * format_width` (272 bytes
/// for 64 `f32` channels instead of 2064 bytes with `Vec<Value>`).
pub fn packed_sample_bytes(s: &PackedSample) -> usize {
    SAMPLE_BYTES + s.payload_bytes()
}

/// Owns a per-outlet temporary spool directory and deletes it on drop.
///
/// Each [`SegmentFile`] holds an `Arc<SpoolDir>` so the directory disappears
/// only after the last segment file closes and unlinks cleanly on Linux,
/// Windows, and macOS.
#[derive(Debug)]
struct SpoolDir {
    path: PathBuf,
}

impl Drop for SpoolDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Owns one on-disk segment file and removes it once no reader holds it.
#[derive(Debug)]
struct SegmentFile {
    path: PathBuf,
    _dir: Arc<SpoolDir>,
}

impl Drop for SegmentFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Metadata for one contiguous range of numbered samples in a segment file.
#[derive(Debug)]
struct Segment {
    file: Arc<SegmentFile>,
    first: u64,
    len: usize,
    bytes: u64,
    /// Sparse `(sample_seq, file_byte_offset)` entries every `INDEX_STRIDE`.
    index: Vec<(u64, u64)>,
}

impl Segment {
    fn next_seq(&self) -> u64 {
        self.first.wrapping_add(self.len as u64)
    }

    fn seek_offset_for(&self, seq: u64) -> u64 {
        let target = seq.wrapping_sub(self.first);
        let pos = self
            .index
            .partition_point(|&(s, _)| s.wrapping_sub(self.first) <= target);
        if pos == 0 {
            0
        } else {
            self.index[pos - 1].1
        }
    }
}

#[derive(Debug)]
struct ActiveSegment {
    seg: Segment,
    writer: BufWriter<File>,
}

/// Bounded ring of segment files on disk.
#[derive(Debug)]
struct DiskSpool {
    dir: Arc<SpoolDir>,
    next_seg_id: u64,
    sealed: VecDeque<Segment>,
    active: Option<ActiveSegment>,
    total_bytes: u64,
    total_samples: usize,
    max_bytes: u64,
    segment_target_bytes: u64,
}

impl DiskSpool {
    fn create(base_dir: Option<&Path>, max_bytes: u64) -> std::io::Result<Self> {
        let root = match base_dir {
            Some(p) => p.to_path_buf(),
            None => std::env::temp_dir(),
        };
        let r = crate::random_bytes();
        let tag = u64::from_le_bytes([r[0], r[1], r[2], r[3], r[4], r[5], r[6], r[7]]);
        let path = root.join(format!("labstream-spool-{}-{tag:016x}", std::process::id()));
        fs::create_dir_all(&path)?;
        // Aim for ~8 segments so eviction drops small chunks without creating
        // thousands of files. Clamp between 32 KiB and 16 MiB, never above max_bytes.
        let target = (max_bytes / 8)
            .clamp(32 * 1024, 16 * 1024 * 1024)
            .min(max_bytes.max(1));
        Ok(DiskSpool {
            dir: Arc::new(SpoolDir { path }),
            next_seg_id: 0,
            sealed: VecDeque::new(),
            active: None,
            total_bytes: 0,
            total_samples: 0,
            max_bytes,
            segment_target_bytes: target,
        })
    }

    fn oldest(&self) -> Option<u64> {
        if let Some(s) = self.sealed.front() {
            Some(s.first)
        } else {
            self.active.as_ref().map(|a| a.seg.first)
        }
    }

    fn seal_active(&mut self) {
        if let Some(mut act) = self.active.take() {
            let _ = act.writer.flush();
            drop(act.writer);
            if act.seg.len > 0 {
                self.sealed.push_back(act.seg);
            }
        }
    }

    fn append(&mut self, seq: u64, s: &PackedSample) -> std::io::Result<()> {
        let rec_bytes = encoded_record_len(s) as u64;
        if rec_bytes > self.max_bytes {
            self.clear();
            return Ok(());
        }
        // Seal current segment if adding this record crosses the segment target.
        if let Some(act) = &self.active {
            if act.seg.len > 0 && act.seg.bytes + rec_bytes > self.segment_target_bytes {
                self.seal_active();
            }
        }
        if self.active.is_none() {
            let id = self.next_seg_id;
            self.next_seg_id += 1;
            let path = self.dir.path.join(format!("seg-{id:08}.bin"));
            let file = File::create(&path)?;
            let seg = Segment {
                file: Arc::new(SegmentFile {
                    path,
                    _dir: Arc::clone(&self.dir),
                }),
                first: seq,
                len: 0,
                bytes: 0,
                index: Vec::new(),
            };
            self.active = Some(ActiveSegment {
                seg,
                writer: BufWriter::with_capacity(64 * 1024, file),
            });
        }
        let act = self.active.as_mut().unwrap();
        if act.seg.len % INDEX_STRIDE == 0 {
            act.seg.index.push((seq, act.seg.bytes));
        }
        let written = write_record(&mut act.writer, seq, s)? as u64;
        act.seg.len += 1;
        act.seg.bytes += written;
        self.total_bytes += written;
        self.total_samples += 1;
        self.trim_to_limits(usize::MAX);
        Ok(())
    }

    fn trim_to_limits(&mut self, max_disk_samples: usize) {
        while let Some(front) = self.sealed.front() {
            let over_bytes = self.total_bytes > self.max_bytes;
            let over_samples = self.total_samples > max_disk_samples;
            if !over_bytes && !over_samples {
                break;
            }
            let drop_bytes = front.bytes;
            let drop_len = front.len;
            self.sealed.pop_front();
            self.total_bytes -= drop_bytes;
            self.total_samples -= drop_len;
        }
        // If even with no sealed segments the active segment exceeds max_bytes,
        // drop it so total disk bytes never exceed the limit.
        if self.total_bytes > self.max_bytes {
            self.clear();
        }
    }

    fn clear(&mut self) {
        self.active = None;
        self.sealed.clear();
        self.total_bytes = 0;
        self.total_samples = 0;
    }

    /// Seal any active segment if needed and build a lock-free [`DiskReadPlan`]
    /// starting at `from` (clamped up to `oldest`) and stopping below `until`.
    fn plan_read(&mut self, from: u64, until: u64, max: usize) -> Option<DiskReadPlan> {
        let oldest = self.oldest()?;
        // Seal active segment if the requested start falls inside it so that
        // the file is flushed and immutable for concurrent readers.
        if let Some(act) = &self.active {
            let d_from = from.wrapping_sub(act.seg.first);
            if d_from < act.seg.len as u64 {
                self.seal_active();
            }
        }
        // Find the first sealed segment whose range overlaps `[from, until)`.
        let d_until = until.wrapping_sub(oldest);
        if d_until == 0 || d_until > u64::MAX / 2 || max == 0 {
            return None;
        }
        let target_from = if from.wrapping_sub(oldest) > u64::MAX / 2 {
            oldest
        } else {
            from
        };
        let seg = self.sealed.iter().find(|seg| {
            let d = target_from.wrapping_sub(seg.first);
            d < seg.len as u64
        })?;
        let start_seq = target_from;
        let seg_rem = seg.next_seq().wrapping_sub(start_seq);
        let req_rem = until.wrapping_sub(start_seq);
        if req_rem == 0 || req_rem > u64::MAX / 2 {
            return None;
        }
        let count = (seg_rem.min(req_rem) as usize).min(max);
        if count == 0 {
            return None;
        }
        Some(DiskReadPlan {
            file: Arc::clone(&seg.file),
            start_offset: seg.seek_offset_for(start_seq),
            start_seq,
            count,
        })
    }
}

/// A self-contained disk read that executes outside the `Fanout` lock so
/// resending old samples from disk never delays live consumers.
#[derive(Debug)]
pub(crate) struct DiskReadPlan {
    file: Arc<SegmentFile>,
    start_offset: u64,
    start_seq: u64,
    count: usize,
}

impl DiskReadPlan {
    /// Read the planned samples from disk into `out`.
    pub(crate) fn execute(self, out: &mut Vec<PackedSample>) -> Option<(u64, usize)> {
        let mut f = File::open(&self.file.path).ok()?;
        f.seek(SeekFrom::Start(self.start_offset)).ok()?;
        let mut reader = BufReader::with_capacity(64 * 1024, f);
        let mut read = 0usize;
        while read < self.count {
            let (seq, sample) = read_record(&mut reader).ok()?;
            let dist = seq.wrapping_sub(self.start_seq);
            if dist > u64::MAX / 2 {
                // Still before `start_seq` within the index stride.
                continue;
            }
            if dist >= self.count as u64 {
                break;
            }
            out.push(sample);
            read += 1;
        }
        if read == 0 {
            None
        } else {
            Some((self.start_seq, read))
        }
    }
}

/// Result of planning a history read under the `Fanout` lock.
#[derive(Debug)]
pub(crate) enum HistoryRead {
    /// No sample available in `[from, until)`.
    Empty,
    /// Samples were copied from RAM directly into `out`.
    Ready {
        /// Sequence number of the first sample appended to `out`.
        first: u64,
        /// Count of samples appended to `out`.
        count: usize,
    },
    /// Samples live on disk; execute this plan after releasing the lock.
    Disk(DiskReadPlan),
}

fn encoded_record_len(s: &PackedSample) -> usize {
    let payload = match &s.values {
        PackedValues::Numeric { bytes, .. } => bytes.len(),
        PackedValues::Strings(list) => 4 + list.iter().map(|b| 4 + b.len()).sum::<usize>(),
    };
    RECORD_HEADER_BYTES + payload
}

fn write_record<W: Write>(w: &mut W, seq: u64, s: &PackedSample) -> std::io::Result<usize> {
    match &s.values {
        PackedValues::Numeric { format, bytes } => {
            let payload_len = bytes.len() as u32;
            let mut hdr = [0u8; RECORD_HEADER_BYTES];
            hdr[0..8].copy_from_slice(&seq.to_le_bytes());
            hdr[8..16].copy_from_slice(&s.timestamp.to_le_bytes());
            hdr[16] = *format as u8;
            hdr[17..21].copy_from_slice(&payload_len.to_le_bytes());
            w.write_all(&hdr)?;
            w.write_all(bytes)?;
            Ok(RECORD_HEADER_BYTES + bytes.len())
        }
        PackedValues::Strings(list) => {
            let payload_len: usize = 4 + list.iter().map(|b| 4 + b.len()).sum::<usize>();
            let mut hdr = [0u8; RECORD_HEADER_BYTES];
            hdr[0..8].copy_from_slice(&seq.to_le_bytes());
            hdr[8..16].copy_from_slice(&s.timestamp.to_le_bytes());
            hdr[16] = Format::String as u8;
            hdr[17..21].copy_from_slice(&(payload_len as u32).to_le_bytes());
            w.write_all(&hdr)?;
            w.write_all(&(list.len() as u32).to_le_bytes())?;
            for b in list.iter() {
                w.write_all(&(b.len() as u32).to_le_bytes())?;
                w.write_all(b)?;
            }
            Ok(RECORD_HEADER_BYTES + payload_len)
        }
    }
}

fn read_record<R: Read>(r: &mut R) -> std::io::Result<(u64, PackedSample)> {
    let mut hdr = [0u8; RECORD_HEADER_BYTES];
    r.read_exact(&mut hdr)?;
    let seq = u64::from_le_bytes(hdr[0..8].try_into().unwrap());
    let timestamp = f64::from_le_bytes(hdr[8..16].try_into().unwrap());
    let format = Format::from_u8(hdr[16])
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "bad format byte"))?;
    let payload_len = u32::from_le_bytes(hdr[17..21].try_into().unwrap()) as usize;
    if format == Format::String {
        let mut buf = vec![0u8; payload_len];
        r.read_exact(&mut buf)?;
        if buf.len() < 4 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "truncated string sample",
            ));
        }
        let count = u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize;
        let mut pos = 4usize;
        let mut strings = Vec::with_capacity(count);
        for _ in 0..count {
            if pos + 4 > buf.len() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "truncated string length",
                ));
            }
            let len = u32::from_le_bytes(buf[pos..pos + 4].try_into().unwrap()) as usize;
            pos += 4;
            if pos + len > buf.len() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "truncated string bytes",
                ));
            }
            strings.push(buf[pos..pos + len].to_vec());
            pos += len;
        }
        Ok((seq, PackedSample::from_strings(timestamp, strings)))
    } else {
        let mut buf = vec![0u8; payload_len];
        r.read_exact(&mut buf)?;
        Ok((
            seq,
            PackedSample::from_raw_bytes(timestamp, format, Arc::from(buf)),
        ))
    }
}

/// The last samples of an outlet, with their numbers.
///
/// The samples have the numbers `back - len .. back`, with no gap. The
/// owner gives the number of each sample at the push, and the history
/// removes samples only from the front.
#[derive(Debug)]
pub struct History {
    samples: VecDeque<PackedSample>,
    back: u64,
    bytes: usize,
    max_samples: usize,
    max_bytes: usize,
    disk_max_bytes: u64,
    disk_dir: Option<PathBuf>,
    /// Disk spool created lazily when RAM fills and `disk_max_bytes > 0`.
    /// Wrapped in `Mutex` so `read_packed(&self)` can seal an active segment
    /// when a consumer asks for disk-spilled samples, while `push_packed(&mut self)`
    /// uses `get_mut()` with zero lock overhead.
    spool: Option<Mutex<DiskSpool>>,
}

impl History {
    /// Build an empty history. The next sample gets the number `next`.
    pub fn new(limits: HistoryLimits, srate: f64, next: u64) -> History {
        History {
            samples: VecDeque::new(),
            back: next,
            bytes: 0,
            max_samples: limits.max_samples(srate),
            max_bytes: limits.bytes,
            disk_max_bytes: limits.disk_bytes,
            disk_dir: limits.disk_dir,
            spool: None,
        }
    }

    /// How many samples the history holds across RAM and disk.
    pub fn len(&self) -> usize {
        self.samples.len() + self.disk_samples()
    }

    /// True when the history holds no sample in RAM or on disk.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The memory that the history holds in RAM now, in bytes.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// The bytes stored in disk spool segments now. Zero when disk spooling is off
    /// or has not spilled yet.
    pub fn disk_bytes(&self) -> u64 {
        self.spool
            .as_ref()
            .map_or(0, |s| s.lock().unwrap().total_bytes)
    }

    /// Path of the active spool directory, if disk spilling has started.
    pub fn spool_dir(&self) -> Option<PathBuf> {
        self.spool
            .as_ref()
            .map(|s| s.lock().unwrap().dir.path.clone())
    }

    fn disk_samples(&self) -> usize {
        self.spool
            .as_ref()
            .map_or(0, |s| s.lock().unwrap().total_samples)
    }

    fn ram_oldest(&self) -> u64 {
        self.back.wrapping_sub(self.samples.len() as u64)
    }

    /// The number of the oldest sample across disk and RAM. With no sample,
    /// the number that the next sample gets.
    pub fn oldest(&self) -> u64 {
        self.back.wrapping_sub(self.len() as u64)
    }

    /// The number that the next sample gets.
    pub fn next(&self) -> u64 {
        self.back
    }

    /// Add one sample. Its timestamp must be a full timestamp, not the
    /// deduced tag.
    ///
    /// The history then removes its oldest samples (or spills them to disk
    /// when `disk_bytes > 0`) until it is in its limits.
    pub fn push(&mut self, s: Sample) {
        self.push_packed(PackedSample::from(s));
    }

    /// Add one [`PackedSample`] without copying its `Arc` payload.
    pub fn push_packed(&mut self, s: PackedSample) {
        debug_assert!(s.timestamp != labstream_wire::DEDUCED_TIMESTAMP);
        let seq = self.back;
        self.back = self.back.wrapping_add(1);
        let size = packed_sample_bytes(&s);
        if self.max_samples == 0 {
            self.clear_all();
            return;
        }
        if self.disk_max_bytes == 0 {
            if size > self.max_bytes {
                self.samples.clear();
                self.bytes = 0;
                return;
            }
            self.bytes += size;
            self.samples.push_back(s);
            while self.samples.len() > self.max_samples || self.bytes > self.max_bytes {
                match self.samples.pop_front() {
                    Some(old) => self.bytes -= packed_sample_bytes(&old),
                    None => break,
                }
            }
            return;
        }

        // Disk-backed mode: keep newest samples in RAM up to `max_bytes`,
        // spilling older samples into bounded segment files on disk.
        if size > self.max_bytes {
            // Spill all buffered RAM first so sequence order on disk stays contiguous,
            // then spill this oversized sample directly to disk.
            self.spill_from_ram(self.samples.len());
            self.append_to_spool(seq, &s);
            self.enforce_total_sample_limit();
            return;
        }
        self.bytes += size;
        self.samples.push_back(s);
        if self.bytes > self.max_bytes {
            // Spill enough oldest samples to bring RAM down to ~75% of max_bytes
            // so disk appends happen in amortized batches while RAM stays <= max_bytes.
            let target = self.max_bytes.saturating_mul(3) / 4;
            let mut to_spill = 0usize;
            let mut freed = 0usize;
            for item in &self.samples {
                if self.bytes.saturating_sub(freed) <= target && to_spill > 0 {
                    break;
                }
                freed += packed_sample_bytes(item);
                to_spill += 1;
                if self.bytes.saturating_sub(freed) <= self.max_bytes
                    && to_spill == self.samples.len()
                {
                    break;
                }
            }
            self.spill_from_ram(to_spill.max(1));
        }
        self.enforce_total_sample_limit();
    }

    fn clear_all(&mut self) {
        self.samples.clear();
        self.bytes = 0;
        if let Some(sp) = self.spool.as_mut() {
            sp.get_mut().unwrap().clear();
        }
    }

    fn ensure_spool(&mut self) -> Option<&mut DiskSpool> {
        if self.spool.is_none() {
            match DiskSpool::create(self.disk_dir.as_deref(), self.disk_max_bytes) {
                Ok(sp) => self.spool = Some(Mutex::new(sp)),
                Err(_) => return None,
            }
        }
        Some(self.spool.as_mut().unwrap().get_mut().unwrap())
    }

    fn append_to_spool(&mut self, seq: u64, sample: &PackedSample) {
        if let Some(sp) = self.ensure_spool() {
            if sp.append(seq, sample).is_err() {
                sp.clear();
            }
        }
    }

    fn spill_from_ram(&mut self, count: usize) {
        let count = count.min(self.samples.len());
        if count == 0 {
            return;
        }
        let mut next_seq = self.ram_oldest();
        let mut evicted = Vec::with_capacity(count);
        for _ in 0..count {
            if let Some(old) = self.samples.pop_front() {
                self.bytes -= packed_sample_bytes(&old);
                evicted.push((next_seq, old));
                next_seq = next_seq.wrapping_add(1);
            }
        }
        if let Some(sp) = self.ensure_spool() {
            for (seq, sample) in &evicted {
                if sp.append(*seq, sample).is_err() {
                    sp.clear();
                    break;
                }
            }
        }
    }

    fn enforce_total_sample_limit(&mut self) {
        if self.max_samples == usize::MAX {
            return;
        }
        let ram_len = self.samples.len();
        if let Some(sp) = self.spool.as_mut() {
            let max_disk = self.max_samples.saturating_sub(ram_len);
            sp.get_mut().unwrap().trim_to_limits(max_disk);
        }
        while self.samples.len() > self.max_samples {
            if let Some(old) = self.samples.pop_front() {
                self.bytes -= packed_sample_bytes(&old);
            }
            if let Some(sp) = self.spool.as_mut() {
                sp.get_mut().unwrap().clear();
            }
        }
    }

    /// Copy at most `max` samples to the end of `out`, from the number
    /// `from`, and below the number `until`, in order.
    ///
    /// `until` is the first number that a consumer queue holds. The history
    /// also holds the samples from that number, so without the bound a
    /// resend gives a sample twice. `docs/extensions.md`, resend rule 5.
    ///
    /// If `from` is below the oldest number, the copy starts at the oldest
    /// number. The result is the number of the first copied sample, and the
    /// count of copied samples. With no copied sample, the result is `None`.
    pub fn read(
        &self,
        from: u64,
        until: u64,
        max: usize,
        out: &mut Vec<Sample>,
    ) -> Option<(u64, usize)> {
        let mut packed = Vec::new();
        let res = self.read_packed(from, until, max, &mut packed)?;
        out.extend(packed.iter().map(PackedSample::to_sample));
        Some(res)
    }

    /// Plan a read of at most `max` packed samples from `from` below `until`.
    ///
    /// Samples in RAM are cloned into `out` immediately. Samples on disk
    /// return a [`DiskReadPlan`] that the caller runs after releasing the
    /// `Fanout` lock.
    pub(crate) fn plan_read_packed(
        &self,
        from: u64,
        until: u64,
        max: usize,
        out: &mut Vec<PackedSample>,
    ) -> HistoryRead {
        if max == 0 {
            return HistoryRead::Empty;
        }
        let oldest = self.oldest();
        let total_len = self.len() as u64;
        if total_len == 0 {
            return HistoryRead::Empty;
        }
        let position = |n: u64| -> u64 {
            let d = n.wrapping_sub(oldest);
            if d > u64::MAX / 2 {
                0
            } else {
                d.min(total_len)
            }
        };
        let start_rel = position(from);
        let end_rel = position(until);
        if start_rel >= end_rel {
            return HistoryRead::Empty;
        }
        let start_seq = oldest.wrapping_add(start_rel);
        let ram_oldest = self.ram_oldest();
        let disk_len = ram_oldest.wrapping_sub(oldest);

        // If `start_rel` is in the disk portion, serve one contiguous block
        // from the matching disk segment outside the Fanout lock.
        if start_rel < disk_len {
            if let Some(sp) = self.spool.as_ref() {
                let disk_until = oldest.wrapping_add(end_rel.min(disk_len));
                if let Some(plan) = sp.lock().unwrap().plan_read(start_seq, disk_until, max) {
                    return HistoryRead::Disk(plan);
                }
            }
            // Fall through to RAM if the disk segment could not be planned.
        }

        // Serve from RAM.
        let ram_len = self.samples.len() as u64;
        let ram_pos = |n: u64| -> u64 {
            let d = n.wrapping_sub(ram_oldest);
            if d > u64::MAX / 2 {
                0
            } else {
                d.min(ram_len)
            }
        };
        let r_start = ram_pos(start_seq);
        let r_end = ram_pos(until);
        if r_start >= r_end {
            return HistoryRead::Empty;
        }
        let count = ((r_end - r_start) as usize).min(max);
        out.extend(
            self.samples
                .range(r_start as usize..r_start as usize + count)
                .cloned(),
        );
        HistoryRead::Ready {
            first: ram_oldest.wrapping_add(r_start),
            count,
        }
    }

    /// Share at most `max` packed samples to the end of `out` (`Arc` clone
    /// for RAM samples, or direct read from disk segment), from `from` and
    /// below `until`.
    pub fn read_packed(
        &self,
        from: u64,
        until: u64,
        max: usize,
        out: &mut Vec<PackedSample>,
    ) -> Option<(u64, usize)> {
        match self.plan_read_packed(from, until, max, out) {
            HistoryRead::Empty => None,
            HistoryRead::Ready { first, count } => Some((first, count)),
            HistoryRead::Disk(plan) => plan.execute(out),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(i: u64) -> Sample {
        Sample {
            timestamp: i as f64,
            values: vec![Value::F32(i as f32)],
        }
    }

    fn numbers(out: &[Sample]) -> Vec<u64> {
        out.iter().map(|s| s.timestamp as u64).collect()
    }

    #[test]
    fn the_time_limit_keeps_the_newest_samples() {
        let limits = HistoryLimits {
            seconds: 1.0,
            bytes: usize::MAX,
            ..HistoryLimits::DEFAULT
        };
        let mut h = History::new(limits, 100.0, 0);
        for i in 0..250 {
            h.push(sample(i));
        }
        assert_eq!(h.len(), 100);
        assert_eq!(h.oldest(), 150);
        assert_eq!(h.next(), 250);
    }

    #[test]
    fn the_memory_limit_keeps_the_newest_samples() {
        let one = packed_sample_bytes(&PackedSample::from(sample(0)));
        assert_eq!(one, 16 + 4);
        let limits = HistoryLimits {
            seconds: 1000.0,
            bytes: 10 * one,
            ..HistoryLimits::DEFAULT
        };
        let mut h = History::new(limits, 100.0, 0);
        for i in 0..25 {
            h.push(sample(i));
        }
        assert_eq!(h.len(), 10);
        assert_eq!(h.bytes(), 10 * one);
        assert_eq!(h.oldest(), 15);
    }

    #[test]
    fn an_irregular_stream_uses_the_memory_limit_only() {
        let one = packed_sample_bytes(&PackedSample::from(sample(0)));
        let limits = HistoryLimits {
            seconds: 0.001,
            bytes: 50 * one,
            ..HistoryLimits::DEFAULT
        };
        let mut h = History::new(limits, IRREGULAR_RATE, 0);
        for i in 0..80 {
            h.push(sample(i));
        }
        assert_eq!(h.len(), 50);
    }

    #[test]
    fn a_string_counts_its_bytes() {
        let marker = |text: &str| Sample {
            timestamp: 1.0,
            values: vec![Value::Str(text.as_bytes().to_vec())],
        };
        assert_eq!(sample_bytes(&marker("")), 16 + VALUE_BYTES);
        assert_eq!(sample_bytes(&marker("stimulus")), 16 + VALUE_BYTES + 8);
        let packed_long = |text: &str| packed_sample_bytes(&PackedSample::from(marker(text)));

        // Long markers reach the memory limit first.
        let long = "x".repeat(1000);
        let limits = HistoryLimits {
            seconds: 10.0,
            bytes: 3 * packed_long(&long),
            ..HistoryLimits::DEFAULT
        };
        let mut h = History::new(limits, IRREGULAR_RATE, 0);
        for _ in 0..5 {
            h.push(marker(&long));
        }
        assert_eq!(h.len(), 3);
    }

    #[test]
    fn packed_history_uses_a_fraction_of_value_memory_for_64_channels() {
        let wide = PackedSample::from_f32(1.0, &[0.0f32; 64]);
        // 16 bytes metadata + 64 * 4 bytes payload = 272 bytes (vs 2064 with Vec<Value>).
        assert_eq!(packed_sample_bytes(&wide), 272);
    }

    #[test]
    fn a_sample_larger_than_the_memory_limit_empties_the_history() {
        let limits = HistoryLimits {
            seconds: 10.0,
            bytes: 100,
            ..HistoryLimits::DEFAULT
        };
        let mut h = History::new(limits, IRREGULAR_RATE, 0);
        h.push(sample(0));
        assert_eq!(h.len(), 1);
        h.push(Sample {
            timestamp: 1.0,
            values: vec![Value::Str(vec![b'x'; 200])],
        });
        // The history cannot hold sample 1, and it must not hold sample 0
        // alone, because then the numbers would have a gap.
        assert!(h.is_empty());
        assert_eq!(h.oldest(), 2);
        h.push(sample(2));
        assert_eq!(h.oldest(), 2);
    }

    #[test]
    fn a_read_gives_the_samples_from_the_asked_number() {
        let mut h = History::new(HistoryLimits::DEFAULT, 100.0, 1000);
        for i in 1000..1100 {
            h.push(sample(i));
        }
        let mut out = Vec::new();
        assert_eq!(h.read(1040, 1100, 5, &mut out), Some((1040, 5)));
        assert_eq!(numbers(&out), vec![1040, 1041, 1042, 1043, 1044]);

        // The last block is shorter than the maximum.
        out.clear();
        assert_eq!(h.read(1098, 1100, 256, &mut out), Some((1098, 2)));
        assert_eq!(numbers(&out), vec![1098, 1099]);
    }

    #[test]
    fn a_read_below_the_history_starts_at_the_oldest_sample() {
        let limits = HistoryLimits {
            seconds: 1.0,
            bytes: usize::MAX,
            ..HistoryLimits::DEFAULT
        };
        let mut h = History::new(limits, 10.0, 0);
        for i in 0..30 {
            h.push(sample(i));
        }
        let mut out = Vec::new();
        assert_eq!(h.read(3, 30, 256, &mut out), Some((20, 10)));
        assert_eq!(numbers(&out), (20..30).collect::<Vec<_>>());
    }

    #[test]
    fn a_read_at_or_above_the_next_number_gives_nothing() {
        let mut h = History::new(HistoryLimits::DEFAULT, 100.0, 0);
        let mut out = Vec::new();
        assert_eq!(h.read(0, 10, 256, &mut out), None);
        for i in 0..10 {
            h.push(sample(i));
        }
        assert_eq!(h.read(10, 20, 256, &mut out), None);
        assert_eq!(h.read(5000, 6000, 256, &mut out), None);
        assert_eq!(h.read(5, 10, 0, &mut out), None);
        assert!(out.is_empty());
    }

    #[test]
    fn a_read_stops_below_the_bound() {
        let mut h = History::new(HistoryLimits::DEFAULT, 100.0, 0);
        for i in 0..50 {
            h.push(sample(i));
        }
        let mut out = Vec::new();
        assert_eq!(h.read(10, 20, 256, &mut out), Some((10, 10)));
        assert_eq!(numbers(&out), (10..20).collect::<Vec<_>>());
        // A bound at or below the start gives nothing.
        assert_eq!(h.read(20, 20, 256, &mut Vec::new()), None);
        assert_eq!(h.read(30, 20, 256, &mut Vec::new()), None);
        // A bound below the oldest number gives nothing.
        let limits = HistoryLimits {
            seconds: 0.1,
            bytes: usize::MAX,
            ..HistoryLimits::DEFAULT
        };
        let mut small = History::new(limits, 100.0, 0);
        for i in 0..50 {
            small.push(sample(i));
        }
        assert_eq!(small.oldest(), 40);
        assert_eq!(small.read(0, 30, 256, &mut Vec::new()), None);
    }

    #[test]
    fn the_numbers_can_wrap() {
        let start = u64::MAX - 4;
        let mut h = History::new(HistoryLimits::DEFAULT, 100.0, start);
        for i in 0..10u64 {
            h.push(sample(i));
        }
        assert_eq!(h.oldest(), start);
        assert_eq!(h.next(), 5);
        let mut out = Vec::new();
        assert_eq!(h.read(1, 5, 256, &mut out), Some((1, 4)));
        assert_eq!(numbers(&out), vec![6, 7, 8, 9]);
        out.clear();
        assert_eq!(h.read(start - 3, 5, 2, &mut out), Some((start, 2)));
        assert_eq!(numbers(&out), vec![0, 1]);
    }

    #[test]
    fn the_default_history_holds_30_s() {
        assert_eq!(HistoryLimits::DEFAULT.seconds, 30.0);
        assert_eq!(HistoryLimits::DEFAULT.bytes, 64 << 20);
        assert_eq!(HistoryLimits::DEFAULT.disk_bytes, 0);
        // At 512 Hz x 8 channels, the time limit acts first.
        let mut h = History::new(HistoryLimits::DEFAULT, 512.0, 0);
        let s = Sample {
            timestamp: 1.0,
            values: vec![Value::F32(0.0); 8],
        };
        for _ in 0..40 * 512 {
            h.push(s.clone());
        }
        assert_eq!(h.len(), 30 * 512);
    }

    #[test]
    fn the_limits_can_turn_the_history_off() {
        assert!(HistoryLimits::DEFAULT.allow_history());
        assert!(!HistoryLimits::OFF.allow_history());
        let no_time = HistoryLimits {
            seconds: 0.0,
            bytes: 1 << 20,
            ..HistoryLimits::DEFAULT
        };
        assert!(!no_time.allow_history());
    }

    #[test]
    fn disk_spool_spills_when_ram_fills_reads_exact_and_cleans_up_on_drop() {
        let limits = HistoryLimits {
            seconds: f64::INFINITY,
            bytes: 8 * 1024,             // 8 KiB RAM cap
            disk_bytes: 2 * 1024 * 1024, // 2 MiB disk cap
            disk_dir: None,
        };
        let spool_path;
        {
            let mut h = History::new(limits, 512.0, 100);
            let total = 2000u64;
            for i in 0..total {
                let ch: Vec<f32> = (0..16).map(|c| (i as f32) + (c as f32) * 0.1).collect();
                h.push_packed(PackedSample::from_f32(10.0 + i as f64 / 512.0, &ch));
                assert!(
                    h.bytes() <= 8 * 1024,
                    "RAM {} exceeded 8 KiB at sample {i}",
                    h.bytes()
                );
            }
            assert_eq!(h.len(), total as usize);
            assert_eq!(h.oldest(), 100);
            assert!(h.disk_bytes() > 0, "expected samples to spill to disk");
            spool_path = h.spool_dir().expect("spool directory created");
            assert!(spool_path.exists());

            // Read every sample across disk + RAM boundary in 256-sample blocks.
            let mut got = Vec::new();
            let mut at = 100u64;
            let until = 100 + total;
            while let Some((first, taken)) = h.read_packed(at, until, 256, &mut got) {
                assert_eq!(first, at);
                at = first + taken as u64;
            }
            assert_eq!(got.len(), total as usize);
            for (idx, s) in got.iter().enumerate() {
                let want_t = 10.0 + idx as f64 / 512.0;
                assert_eq!(s.timestamp.to_bits(), want_t.to_bits());
                let mut vals = Vec::new();
                s.push_into_f32(&mut vals);
                assert_eq!(vals.len(), 16);
                assert_eq!(vals[0], idx as f32);
                assert_eq!(vals[15], (idx as f32) + 1.5);
            }
        }
        assert!(
            !spool_path.exists(),
            "spool directory must be removed when History drops"
        );
    }

    #[test]
    fn disk_spool_enforces_max_disk_bytes_and_handles_strings() {
        let limits = HistoryLimits {
            seconds: f64::INFINITY,
            bytes: 2 * 1024,
            disk_bytes: 128 * 1024, // 128 KiB disk cap
            disk_dir: None,
        };
        let mut h = History::new(limits, IRREGULAR_RATE, 0);
        for i in 0..1000u64 {
            let payload = format!("marker-{i:04}-{}", "a".repeat(200)).into_bytes();
            h.push_packed(PackedSample::from_strings(i as f64, vec![payload]));
            assert!(h.bytes() <= 2 * 1024);
            assert!(
                h.disk_bytes() <= 128 * 1024,
                "disk {} exceeded 128 KiB",
                h.disk_bytes()
            );
        }
        assert!(h.oldest() > 0, "oldest segments must evict under disk cap");
        let oldest = h.oldest();
        let mut out = Vec::new();
        let mut at = 0u64;
        while let Some((first, taken)) = h.read_packed(at, 1000, 64, &mut out) {
            if at == 0 {
                assert_eq!(first, oldest);
            } else {
                assert_eq!(first, at);
            }
            at = first + taken as u64;
        }
        assert_eq!(out.len(), (1000 - oldest) as usize);
        let first_s = out[0].to_sample();
        let expected_prefix = format!("marker-{oldest:04}-");
        match &first_s.values[0] {
            Value::Str(b) => assert!(b.starts_with(expected_prefix.as_bytes())),
            _ => panic!("expected string sample"),
        }
    }
}
