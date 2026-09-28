//! The outlet: publish one stream.

use crate::{Error, Scalar, StreamInfo};
use std::time::Duration;

/// Publish samples for other programs to read.
pub struct Outlet {
    inner: labstream_net::Outlet,
    info: StreamInfo,
    channels: usize,
}

impl Outlet {
    /// Publish a stream.
    ///
    /// The stream is on the network when this call returns.
    pub fn new(info: StreamInfo) -> Result<Outlet, Error> {
        let channels = info.channel_count();
        let inner = labstream_net::Outlet::new(info.inner.clone())?;
        Ok(Outlet {
            inner,
            info,
            channels,
        })
    }

    /// The description that this outlet published.
    pub fn info(&self) -> &StreamInfo {
        &self.info
    }

    /// Write one sample, with the current clock as its timestamp.
    ///
    /// The slice holds one value for each channel.
    ///
    /// # Errors
    /// The call fails when the slice length does not match the channel count.
    /// liblsl reads past the end of a short buffer instead
    /// (`include/lsl_cpp.h:1094`).
    pub fn push<T: Scalar>(&self, sample: &[T]) -> Result<(), Error> {
        self.push_at(sample, labstream_net::clock())
    }

    /// Write one sample with a timestamp that the caller measured.
    ///
    /// A device that stamps its own samples must use this call. A stamp from the
    /// device is more correct than the moment the program got the data.
    pub fn push_at<T: Scalar>(&self, sample: &[T], timestamp: f64) -> Result<(), Error> {
        if sample.len() != self.channels {
            return Err(Error::ChannelCount {
                want: self.channels,
                got: sample.len(),
            });
        }
        self.inner.push(&labstream_wire::Sample {
            timestamp,
            values: sample.iter().map(|v| v.into_value()).collect(),
        });
        Ok(())
    }

    /// Write a block of samples with one call.
    ///
    /// `data` holds the samples end to end, one sample after another.
    ///
    /// `end_timestamp` names the **last** sample. A stream with a rate counts
    /// backward from it, so only the first sample carries a stamp on the wire and
    /// the rest carry the deduced tag. A device that acquires a block and stamps
    /// it on arrival therefore gets the right time for every sample.
    ///
    /// This is the call for a fast stream. It takes one lock for the block. A
    /// loop of [`Outlet::push_at`] takes one for each sample.
    ///
    /// # Errors
    /// The call fails when the length of `data` is not a whole number of samples.
    pub fn push_chunk<T: Scalar>(&self, data: &[T], end_timestamp: f64) -> Result<(), Error> {
        if self.channels == 0 || data.len() % self.channels != 0 {
            return Err(Error::ChannelCount {
                want: self.channels,
                got: data.len(),
            });
        }
        let block: Vec<labstream_wire::Sample> = data
            .chunks_exact(self.channels)
            .map(|row| labstream_wire::Sample {
                timestamp: 0.0, // the block call dates every sample
                values: row.iter().map(|v| v.into_value()).collect(),
            })
            .collect();
        self.inner.push_chunk_fast(&block, end_timestamp);
        Ok(())
    }

    /// Write one text marker, with the current clock as its timestamp.
    ///
    /// A marker stream has one channel and the format `String`.
    pub fn push_text(&self, marker: &str) -> Result<(), Error> {
        self.push_text_at(marker, labstream_net::clock())
    }

    /// Write one text marker with a timestamp that the caller measured.
    pub fn push_text_at(&self, marker: &str, timestamp: f64) -> Result<(), Error> {
        if self.channels != 1 {
            return Err(Error::ChannelCount {
                want: 1,
                got: self.channels,
            });
        }
        self.inner.push(&labstream_wire::Sample {
            timestamp,
            values: vec![labstream_wire::Value::Str(marker.as_bytes().to_vec())],
        });
        Ok(())
    }

    /// How many inlets read this stream now.
    pub fn consumer_count(&self) -> usize {
        self.inner.consumer_count()
    }

    /// True when at least one inlet reads this stream.
    ///
    /// A source that measures power can stop the measurement while this is false.
    pub fn has_consumers(&self) -> bool {
        self.inner.consumer_count() > 0
    }

    /// Wait for the first inlet. The call gives false when the timeout passes.
    pub fn wait_for_consumers(&self, timeout: Duration) -> bool {
        self.inner.wait_for_consumers(timeout)
    }
}
