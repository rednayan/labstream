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
    scratch: Vec<labstream_wire::Sample>,
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
}

impl Inlet {
    /// Start an inlet for a stream that discovery found.
    pub fn builder(info: &StreamInfo) -> Builder<'_> {
        Builder {
            info,
            buffer: Buffer::DEFAULT,
            recover: true,
            post: Post::NONE,
        }
    }

    /// The description of the connected stream, with its description tree.
    pub fn info(&self) -> &StreamInfo {
        &self.info
    }

    /// Read one sample, as one value for each channel.
    ///
    /// The call gives `None` when the timeout passes with no sample. A timeout of
    /// zero never blocks.
    pub fn pull<T: Scalar>(&mut self, timeout: Duration) -> Result<Option<(f64, Vec<T>)>, Error> {
        match self.inner.pull(timeout)? {
            Some(s) => Ok(Some((
                s.timestamp,
                s.values.iter().map(T::from_value).collect(),
            ))),
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
        let got = self.inner.pull_chunk(&mut self.scratch, room, timeout)?;
        for s in &self.scratch {
            chunk.push(s.timestamp, s.values.iter().map(T::from_value));
        }
        Ok(got)
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

    /// Open the feed.
    ///
    /// # Errors
    /// The call fails when the source does not answer inside the timeout, or when
    /// the source is gone.
    pub fn open(self, timeout: Duration) -> Result<Inlet, Error> {
        let samples = self.buffer.samples(self.info.rate());
        let buffered = samples.min(i32::MAX as usize) as i32;
        let mut inner = if self.recover {
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
