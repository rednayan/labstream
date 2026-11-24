//! The inlet: read one stream.

use crate::{Chunk, Error, Scalar, StreamInfo};
use std::time::Duration;

/// The post-processing stages that an inlet applies to a timestamp.
///
/// The stages run in a fixed order: clock sync, then jitter removal, then the
/// clamp. A stage that is not selected does not run.
///
/// The constants of liblsl live in a header that a Rust program cannot read
/// (`include/lsl/common.h:103`). Each stage below holds the value of that
/// header.
///
/// # One flag of liblsl is absent
///
/// liblsl holds a fifth flag, `proc_threadsafe`, and its `proc_ALL` includes
/// it. That flag guards the state of the filter with a lock, because a C
/// program can read one inlet from two threads. Its own header says that it
/// "uses somewhat more CPU".
///
/// [`Inlet::pull`](crate::Inlet::pull) takes `&mut self`, so two threads
/// cannot read one inlet here. The lock would guard nothing and cost time, so
/// this crate does not offer the flag. [`Post::ALL`] is therefore every stage
/// that changes a timestamp, and it is not the same number as `proc_ALL`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Post(u32);

impl Post {
    /// Apply no stage. A timestamp arrives as the source wrote it.
    pub const NONE: Post = Post(0);
    /// Add the measured offset between the two clocks.
    pub const CLOCK_SYNC: Post = Post(1);
    /// Remove jitter with a least squares fit of the nominal rate.
    pub const DEJITTER: Post = Post(2);
    /// Never let a timestamp go backward.
    pub const MONOTONIZE: Post = Post(4);
    /// Every stage that changes a timestamp. This is what a viewer of live
    /// signals wants.
    ///
    /// This is not the value of `proc_ALL` in liblsl. The note on [`Post`]
    /// gives the flag that is absent and the reason.
    pub const ALL: Post = Post(1 | 2 | 4);

    /// The bits, for a call that needs the raw value.
    pub fn bits(self) -> u32 {
        self.0
    }
}

impl std::ops::BitOr for Post {
    type Output = Post;
    fn bitor(self, rhs: Post) -> Post {
        Post(self.0 | rhs.0)
    }
}

/// How much data an inlet holds for the program.
///
/// liblsl counts this in seconds for a regular stream and in samples for an
/// irregular one, through one `int32` parameter
/// (`include/lsl_cpp.h:914`). The two units at one parameter cause silent data
/// loss: a program that means 8 seconds and gets 8 samples drops nearly
/// everything. This type names the unit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Buffer {
    /// Hold this many seconds of a regular stream.
    Seconds(f64),
    /// Hold this many samples.
    Samples(usize),
}

impl Buffer {
    /// What liblsl holds by default: 360 seconds.
    pub const DEFAULT: Buffer = Buffer::Seconds(360.0);

    /// The size in samples for a stream at this rate.
    ///
    /// A rate of zero gives a count in samples, because seconds have no meaning
    /// for an irregular stream.
    pub fn samples(self, rate: f64) -> usize {
        match self {
            Buffer::Samples(n) => n.max(1),
            Buffer::Seconds(s) if rate > 0.0 => ((s * rate).ceil() as usize).max(1),
            Buffer::Seconds(_) => 360,
        }
    }
}

/// Read samples from one stream.
///
/// Open one with [`Inlet::builder`].
pub struct Inlet {
    inner: labstream_net::Inlet,
    info: StreamInfo,
    /// The block that `labstream-net` fills. It is kept so that a read allocates
    /// nothing after the first one.
    scratch: Vec<labstream_wire::PackedSample>,
}

/// The builder for [`Inlet`].
///
/// liblsl takes four positional parameters, and three of them have defaults
/// (`include/lsl_cpp.h:914`). A reader of the call cannot see which is which.
/// Each option here has a name.
pub struct Builder<'a> {
    info: &'a StreamInfo,
    buffer: Buffer,
    recover: bool,
    post: Post,
    channels: Vec<u16>,
    decimate: u32,
    target_rate: Option<f64>,
}

impl Inlet {
    /// Start an inlet for a stream that discovery found.
    pub fn builder(info: &StreamInfo) -> Builder<'_> {
        Builder {
            info,
            buffer: Buffer::DEFAULT,
            recover: true,
            post: Post::NONE,
            channels: Vec::new(),
            decimate: 1,
            target_rate: None,
        }
    }

    /// The description of the connected stream, with its description tree.
    pub fn info(&self) -> &StreamInfo {
        &self.info
    }

    /// The active `select/1` channel and rate selection, or `None` when the
    /// full stream is delivered.
    pub fn selection(&self) -> Option<labstream_net::SelectParam> {
        self.inner.selection()
    }

    /// Read one sample, as one value for each channel.
    ///
    /// The call gives `None` when the timeout passes with no sample. A timeout of
    /// zero never blocks.
    pub fn pull<T: Scalar>(&mut self, timeout: Duration) -> Result<Option<(f64, Vec<T>)>, Error> {
        match self.inner.pull_packed(timeout)? {
            Some(s) => {
                let mut values = Vec::with_capacity(s.channel_count());
                T::unpack_into(&s, &mut values);
                Ok(Some((s.timestamp, values)))
            }
            None => Ok(None),
        }
    }

    /// Read one sample from a stream that carries text.
    ///
    /// A marker stream carries text. [`Inlet::pull`] cannot read one, because its
    /// value type is [`Scalar`] and a `String` is not `Copy`. This call gives the
    /// text of each channel instead. A marker stream has one channel, so the
    /// vector holds one string.
    ///
    /// A number that arrives on a text stream gives its decimal form, so a stream
    /// of mixed values still reads.
    ///
    /// ```no_run
    /// # use labstream::{Inlet, Query};
    /// # use std::time::Duration;
    /// # fn main() -> Result<(), labstream::Error> {
    /// # let info = labstream::resolve_first(&Query::stream_type("Markers"), Duration::from_secs(5))?.unwrap();
    /// # let mut inlet = Inlet::builder(&info).open(Duration::from_secs(5))?;
    /// while let Some((at, values)) = inlet.pull_text(Duration::from_millis(20))? {
    ///     println!("{at} {}", values.join(" "));
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn pull_text(&mut self, timeout: Duration) -> Result<Option<(f64, Vec<String>)>, Error> {
        match self.inner.pull(timeout)? {
            Some(s) => Ok(Some((
                s.timestamp,
                s.values.iter().map(crate::as_text).collect(),
            ))),
            None => Ok(None),
        }
    }

    /// Read every sample that waits, into a buffer that the caller keeps.
    ///
    /// This is the call for a program that reads a fast stream. The buffer holds
    /// the samples end to end, so one pull does one lock and no allocation. A
    /// program that reads 8 channels at 1000 Hz with [`Inlet::pull`] instead does
    /// 1000 locks and 1000 allocations each second.
    ///
    /// The call gives how many samples it wrote. It writes no more than
    /// `chunk.capacity_samples()`, and it leaves the rest for the next call.
    ///
    /// If the timeout is not zero, the call waits for the first sample only. It
    /// never waits for the buffer to fill, because a viewer must show what
    /// arrived.
    pub fn pull_chunk<T: Scalar>(
        &mut self,
        chunk: &mut Chunk<T>,
        timeout: Duration,
    ) -> Result<usize, Error> {
        let room = chunk.capacity_samples() - chunk.len();
        if room == 0 {
            return Ok(0);
        }
        self.scratch.clear();
        let got = self
            .inner
            .pull_chunk_packed(&mut self.scratch, room, timeout)?;
        for s in &self.scratch {
            chunk.push_packed(s);
        }
        Ok(got)
    }

    /// Read up to `max_samples` into flat `f32` and timestamp buffers.
    pub fn pull_chunk_f32(
        &mut self,
        data: &mut Vec<f32>,
        timestamps: &mut Vec<f64>,
        max_samples: usize,
        timeout: Duration,
    ) -> Result<usize, Error> {
        Ok(self
            .inner
            .pull_chunk_f32(data, timestamps, max_samples, timeout)?)
    }

    /// Read up to `max_samples` into flat `f64` and timestamp buffers.
    pub fn pull_chunk_f64(
        &mut self,
        data: &mut Vec<f64>,
        timestamps: &mut Vec<f64>,
        max_samples: usize,
        timeout: Duration,
    ) -> Result<usize, Error> {
        Ok(self
            .inner
            .pull_chunk_f64(data, timestamps, max_samples, timeout)?)
    }

    /// Read up to `max_samples` into flat `i32` and timestamp buffers.
    pub fn pull_chunk_i32(
        &mut self,
        data: &mut Vec<i32>,
        timestamps: &mut Vec<f64>,
        max_samples: usize,
        timeout: Duration,
    ) -> Result<usize, Error> {
        Ok(self
            .inner
            .pull_chunk_i32(data, timestamps, max_samples, timeout)?)
    }

    /// Read up to `max_samples` into flat `i16` and timestamp buffers.
    pub fn pull_chunk_i16(
        &mut self,
        data: &mut Vec<i16>,
        timestamps: &mut Vec<f64>,
        max_samples: usize,
        timeout: Duration,
    ) -> Result<usize, Error> {
        Ok(self
            .inner
            .pull_chunk_i16(data, timestamps, max_samples, timeout)?)
    }

    /// Read up to `max_samples` into flat `i8` and timestamp buffers.
    pub fn pull_chunk_i8(
        &mut self,
        data: &mut Vec<i8>,
        timestamps: &mut Vec<f64>,
        max_samples: usize,
        timeout: Duration,
    ) -> Result<usize, Error> {
        Ok(self
            .inner
            .pull_chunk_i8(data, timestamps, max_samples, timeout)?)
    }

    /// Read up to `max_samples` into flat `i64` and timestamp buffers.
    pub fn pull_chunk_i64(
        &mut self,
        data: &mut Vec<i64>,
        timestamps: &mut Vec<f64>,
        max_samples: usize,
        timeout: Duration,
    ) -> Result<usize, Error> {
        Ok(self
            .inner
            .pull_chunk_i64(data, timestamps, max_samples, timeout)?)
    }

    /// The measured offset between the clock of the source and the local clock.
    ///
    /// Add this value to a timestamp to move it onto the local clock. The
    /// `CLOCK_SYNC` stage does this on its own. The value is `None` before the
    /// first measurement completes.
    pub fn time_correction(&self) -> Option<f64> {
        self.inner.time_correction()
    }

    /// Wait for the first offset measurement, then give it.
    pub fn wait_for_time_correction(&self, timeout: Duration) -> Option<f64> {
        self.inner.wait_for_time_correction(timeout)
    }

    /// How many samples wait for the program.
    pub fn available(&self) -> usize {
        self.inner.samples_available()
    }

    /// How many samples the buffer dropped because the program was too slow.
    ///
    /// A viewer that shows live signals must report this number. A gap that
    /// nobody reports looks like data.
    pub fn dropped(&self) -> u64 {
        self.inner.samples_dropped()
    }

    /// How many samples never reached this inlet, since it opened.
    ///
    /// Two labstream peers number their samples (the `seq/1` feature of
    /// `docs/extensions.md`). The count shows a sample that the outlet
    /// dropped because this inlet read too slowly, and the samples that went
    /// out while the connection was lost. [`Inlet::dropped`] counts the
    /// samples that this inlet dropped itself. The total loss is the sum.
    ///
    /// The value is `None` when the source does not number its samples, for
    /// example a liblsl outlet. `None` means unknown, not zero.
    pub fn lost(&self) -> Option<u64> {
        self.inner.samples_lost()
    }

    /// How many samples came again from the history of the source, since
    /// this inlet opened.
    ///
    /// After a lost connection, a labstream source sends the samples of the
    /// dropout again (the `resume/1` feature of `docs/extensions.md`). They
    /// arrive late, with their original timestamps, and in order.
    /// [`Inlet::lost`] counts only the samples that the history did not hold.
    ///
    /// The value is `None` when the source does not keep a history, for
    /// example a liblsl outlet. `None` means unknown, not zero.
    pub fn resent(&self) -> Option<u64> {
        self.inner.samples_resent()
    }

    /// The round-trip time of the latest clock offset estimate, in seconds.
    pub fn time_uncertainty(&self) -> Option<f64> {
        self.inner.time_uncertainty()
    }

    /// How many times the connection has been rebuilt after a source restart
    /// or network dropout.
    pub fn recoveries(&self) -> u64 {
        self.inner.recoveries()
    }

    /// True while samples arrive over UDP (`dgram/1`).
    pub fn on_udp(&self) -> bool {
        self.inner.on_udp()
    }

    /// True while samples arrive through shared memory (`shm/1`).
    pub fn on_shm(&self) -> bool {
        self.inner.on_shm()
    }

    /// Snapshot the health of this inlet in one call.
    pub fn health(&self) -> crate::InletHealth {
        self.inner.health()
    }

    /// Snapshot every gap recorded on this inlet, sorted by start timestamp.
    pub fn gaps(&self) -> Vec<crate::Gap> {
        self.inner.gaps()
    }

    /// Drain and return every finalized gap recorded since the last call,
    /// sorted by start timestamp.
    pub fn take_gaps(&self) -> Vec<crate::Gap> {
        self.inner.take_gaps()
    }

    /// True when the source ended and the program read every sample.
    ///
    /// An inlet that recovers reconnects on its own, so this stays false while
    /// the source restarts.
    pub fn is_finished(&self) -> bool {
        self.inner.is_finished()
    }
}

impl<'a> Builder<'a> {
    /// Set how much data the inlet holds. The default is 360 seconds.
    pub fn buffer(mut self, buffer: Buffer) -> Builder<'a> {
        self.buffer = buffer;
        self
    }

    /// Choose whether the inlet finds the stream again after the source
    /// restarts. The default is true, as in liblsl (`include/lsl_cpp.h:915`).
    pub fn recover(mut self, recover: bool) -> Builder<'a> {
        self.recover = recover;
        self
    }

    /// Choose the post-processing stages. The default applies none.
    pub fn postprocess(mut self, post: Post) -> Builder<'a> {
        self.post = post;
        self
    }

    /// Ask the outlet to send only these 0-based channels (`select/1`,
    /// `docs/roadmap.md`, step 6).
    ///
    /// A `liblsl` outlet ignores the offer and sends the full stream.
    pub fn channels<I>(mut self, indices: I) -> Builder<'a>
    where
        I: IntoIterator<Item = usize>,
    {
        self.channels = indices
            .into_iter()
            .filter_map(|i| u16::try_from(i).ok())
            .collect();
        self
    }

    /// Ask the outlet to decimate the stream by an integer `factor >= 1`
    /// with a Hamming-windowed sinc low-pass FIR filter (`select/1`).
    pub fn decimate(mut self, factor: u32) -> Builder<'a> {
        self.decimate = factor.max(1);
        self.target_rate = None;
        self
    }

    /// Ask the outlet to decimate the stream down to `target_hz` (`select/1`).
    ///
    /// Converts `target_hz` to the nearest integer decimation factor
    /// `round(source_rate / target_hz).max(1)`.
    pub fn rate(mut self, target_hz: f64) -> Builder<'a> {
        if target_hz.is_finite() && target_hz > 0.0 {
            self.target_rate = Some(target_hz);
        }
        self
    }

    /// Open the feed.
    ///
    /// # Errors
    /// The call fails when the source does not answer inside the timeout, or when
    /// the source is gone.
    pub fn open(self, timeout: Duration) -> Result<Inlet, Error> {
        let decimate = match self.target_rate {
            Some(target) if self.info.rate() > 0.0 => {
                (self.info.rate() / target).round().max(1.0) as u32
            }
            _ => self.decimate.max(1),
        };
        let sel = labstream_net::SelectParam {
            decimate,
            channels: self.channels,
        };
        let effective_rate = sel.output_srate(self.info.rate());
        let samples = self.buffer.samples(effective_rate);
        let buffered = samples.min(i32::MAX as usize) as i32;
        let mut inner = if sel.is_active() {
            labstream_net::Inlet::open_selected(
                &self.info.inner,
                timeout,
                buffered,
                self.recover,
                sel,
            )?
        } else if self.recover {
            labstream_net::Inlet::open_recovering(&self.info.inner, timeout, buffered)?
        } else {
            labstream_net::Inlet::open_with_buffer(&self.info.inner, timeout, buffered)?
        };
        inner.set_postprocessing(self.post.bits());
        let info = StreamInfo {
            inner: inner.info().clone(),
        };
        Ok(Inlet {
            inner,
            info,
            scratch: Vec::new(),
        })
    }
}
