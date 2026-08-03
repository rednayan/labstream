//! The Lab Streaming Layer, for Rust programs.
//!
//! This crate is the API that a Rust program uses. It holds no protocol code.
//! The protocol lives in `lsl-net`, `lsl-proto`, `lsl-wire`, and `lsl-time`, and
//! those crates stay exact to the C++ library at `sccn/liblsl`. A program that
//! needs a protocol detail can still call them.
//!
//! # Read a stream
//!
//! ```no_run
//! use lsl::{Buffer, Chunk, Inlet, Post, Query};
//! use std::time::Duration;
//!
//! # fn main() -> Result<(), lsl::Error> {
//! let info = lsl::resolve_first(&Query::stream_type("EEG"), Duration::from_secs(5))?
//!     .expect("an EEG stream");
//! let mut inlet = Inlet::builder(&info)
//!     .buffer(Buffer::Seconds(8.0))
//!     .postprocess(Post::ALL)
//!     .open(Duration::from_secs(5))?;
//!
//! for c in inlet.info().channels() {
//!     println!("{} in {}", c.label, c.unit);
//! }
//!
//! let mut chunk = Chunk::<f32>::new(info.channel_count(), 4096);
//! loop {
//!     chunk.clear();
//!     inlet.pull_chunk(&mut chunk, Duration::from_millis(20))?;
//!     for (t, sample) in chunk.iter() {
//!         println!("{t} {sample:?}");
//!     }
//! }
//! # }
//! ```
//!
//! # Publish a stream
//!
//! ```no_run
//! use lsl::{Format, Outlet, StreamInfo};
//!
//! # fn main() -> Result<(), lsl::Error> {
//! let info = StreamInfo::builder("MyDevice", "EEG", Format::Float32)
//!     .rate(250.0)
//!     .source_id("device-0001")
//!     .channel_labels(["Fp1", "Fp2", "C3", "C4"])
//!     .build()?;
//! let outlet = Outlet::new(info)?;
//! outlet.push(&[1.0f32, 2.0, 3.0, 4.0])?;
//! # Ok(())
//! # }
//! ```

#![warn(missing_docs)]

mod chunk;
mod info;
mod inlet;
mod outlet;
mod query;
mod scalar;

pub use chunk::Chunk;
pub use info::{Builder as StreamInfoBuilder, Channel, StreamInfo};
pub use inlet::{Buffer, Builder as InletBuilder, Inlet, Post};
pub use outlet::Outlet;
pub use query::Query;
pub use scalar::{as_f64, as_text, Scalar};

/// The sample format of a stream.
pub use lsl_wire::Format;

use std::time::Duration;

/// What can go wrong.
///
/// liblsl separates a lost stream from a timeout (`include/lsl_cpp.h:60-80`).
/// `std::io::Error` does not, so a program cannot tell "the source is gone" from
/// "the socket failed". This type keeps them apart.
#[derive(Debug)]
pub enum Error {
    /// The call did not complete inside its timeout.
    Timeout,
    /// The source is gone and it did not come back.
    Lost,
    /// A slice held the wrong number of values.
    ChannelCount {
        /// How many values the stream needs.
        want: usize,
        /// How many values the call got.
        got: usize,
    },
    /// A description or an argument is not consistent.
    Invalid(&'static str),
    /// The network or the operating system reported a fault.
    Io(std::io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Timeout => write!(f, "the call timed out"),
            Error::Lost => write!(f, "the source is gone"),
            Error::ChannelCount { want, got } => {
                write!(f, "the stream has {want} channels and the call gave {got}")
            }
            Error::Invalid(m) => write!(f, "{m}"),
            Error::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Error {
        match e.kind() {
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => Error::Timeout,
            std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::NotConnected => Error::Lost,
            _ => Error::Io(e),
        }
    }
}

/// Find every stream that matches, for the whole timeout.
///
/// This is the call for a browser. It waits the whole time, because a slow host
/// answers late and a browser must show it.
pub fn resolve_all(query: &Query, timeout: Duration) -> Result<Vec<StreamInfo>, Error> {
    let found = lsl_net::resolve(query.as_str(), usize::MAX, timeout)?;
    Ok(found
        .into_iter()
        .map(|inner| StreamInfo { inner })
        .collect())
}

/// Find one stream that matches, and stop at the first answer.
///
/// This is the call for a program that knows the stream it wants. It returns as
/// soon as that stream answers, so it does not wait the whole time.
pub fn resolve_first(query: &Query, timeout: Duration) -> Result<Option<StreamInfo>, Error> {
    let found = lsl_net::resolve(query.as_str(), 1, timeout)?;
    Ok(found.into_iter().next().map(|inner| StreamInfo { inner }))
}

/// Find at least this many streams, then stop.
///
/// The call gives fewer than `count` streams when the timeout passes first.
pub fn resolve_at_least(
    query: &Query,
    count: usize,
    timeout: Duration,
) -> Result<Vec<StreamInfo>, Error> {
    let found = lsl_net::resolve(query.as_str(), count, timeout)?;
    Ok(found
        .into_iter()
        .map(|inner| StreamInfo { inner })
        .collect())
}

/// The local clock, in seconds.
///
/// Every timestamp of this library uses this clock. The value counts from an
/// arbitrary moment, so only a difference of two values has meaning.
pub fn clock() -> f64 {
    lsl_net::clock()
}

/// Watch the network and keep a current list of streams.
///
/// A browser that calls [`resolve_all`] every few seconds pays the whole timeout
/// each time. This watcher sends its queries on its own thread, so a read of the
/// list never waits.
///
/// The thread stops when the watcher is dropped.
///
/// ```no_run
/// # use lsl::{Query, Watcher};
/// # use std::time::Duration;
/// let watcher = Watcher::new(&Query::all(), Duration::from_secs(5));
/// for info in watcher.streams() {
///     println!("{} at {}", info.name(), info.hostname());
/// }
/// ```
pub struct Watcher {
    inner: lsl_net::ContinuousResolver,
}

impl Watcher {
    /// Start to watch for the streams that match.
    ///
    /// A stream that stops answering leaves the list after `forget_after`.
    /// liblsl forgets after 5 seconds by default (`include/lsl_cpp.h:1641`).
    pub fn new(query: &Query, forget_after: Duration) -> Watcher {
        Watcher {
            inner: lsl_net::ContinuousResolver::new(query.as_str(), forget_after.as_secs_f64()),
        }
    }

    /// The streams that answered most recently. This never waits.
    pub fn streams(&self) -> Vec<StreamInfo> {
        self.inner
            .results(usize::MAX)
            .into_iter()
            .map(|inner| StreamInfo { inner })
            .collect()
    }

    /// How many streams the list holds now.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// True when the list holds no stream.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}
