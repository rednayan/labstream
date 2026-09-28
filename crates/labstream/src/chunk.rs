//! The buffer that a chunk read fills.

use crate::Scalar;

/// A block of samples that the caller keeps and reuses.
///
/// The values sit end to end, one sample after another. The buffer therefore
/// needs one allocation for its whole life. A program that reads a fast stream
/// makes one of these at startup and passes it to every
/// [`crate::Inlet::pull_chunk`] call.
#[derive(Debug, Clone)]
pub struct Chunk<T> {
    data: Vec<T>,
    stamps: Vec<f64>,
    channels: usize,
    capacity: usize,
}

impl<T: Scalar> Chunk<T> {
    /// A buffer for this many channels and this many samples.
    ///
    /// # Panics
    /// A chunk of no channel has no meaning, and every read of one would give a
    /// wrong answer quietly. A count of zero is a fault in the program, so this
    /// call reports it where it happens.
    pub fn new(channels: usize, capacity_samples: usize) -> Chunk<T> {
        assert!(channels > 0, "a chunk needs at least one channel");
        let capacity = capacity_samples.max(1);
        Chunk {
            data: Vec::with_capacity(channels * capacity),
            stamps: Vec::with_capacity(capacity),
            channels,
            capacity,
        }
    }

    /// How many samples the buffer holds now.
    pub fn len(&self) -> usize {
        self.stamps.len()
    }

    /// True when the buffer holds no sample.
    pub fn is_empty(&self) -> bool {
        self.stamps.is_empty()
    }

    /// How many channels one sample holds.
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// How many samples the buffer can hold.
    pub fn capacity_samples(&self) -> usize {
        self.capacity
    }

    /// One sample, as one value for each channel.
    pub fn sample(&self, i: usize) -> &[T] {
        &self.data[i * self.channels..(i + 1) * self.channels]
    }

    /// The timestamp of every sample, in order.
    pub fn timestamps(&self) -> &[f64] {
        &self.stamps
    }

    /// Every sample with its timestamp.
    pub fn iter(&self) -> impl Iterator<Item = (f64, &[T])> + '_ {
        (0..self.len()).map(move |i| (self.stamps[i], self.sample(i)))
    }

    /// Every value of one channel, in order.
    ///
    /// A viewer of signals holds one buffer for each channel, and the samples
    /// arrive one after another. Without this call, each program writes the same
    /// loop. `src/io/lsl.rs` of the g-signals viewer held that loop.
    ///
    /// Fill a buffer that the program keeps with the two calls of the standard
    /// library. `clear` then `extend` replaces what the buffer held; `extend`
    /// alone adds to it. Which one happens is then visible where it is decided.
    ///
    /// ```
    /// # use labstream::Chunk;
    /// # let chunk = Chunk::<f32>::new(2, 8);
    /// let mut channel_0 = Vec::new();
    /// channel_0.clear();
    /// channel_0.extend(chunk.channel(0));
    /// ```
    ///
    /// # Panics
    /// If `c` is not a channel of this chunk. A slice panics for an index that
    /// is out of range, and this call does the same. Without the check it would
    /// stride into the samples that follow and give values that look real.
    pub fn channel(&self, c: usize) -> impl Iterator<Item = T> + '_ {
        assert!(
            c < self.channels,
            "channel {c} of a chunk that has {}",
            self.channels
        );
        self.data.iter().skip(c).step_by(self.channels).copied()
    }

    /// Drop every sample. The allocation stays.
    pub fn clear(&mut self) {
        self.data.clear();
        self.stamps.clear();
    }

    /// Add one sample. The inlet calls this.
    #[allow(dead_code)] // the caller is `Inlet::pull_chunk`
    pub(crate) fn push(&mut self, timestamp: f64, values: impl Iterator<Item = T>) {
        let before = self.data.len();
        self.data.extend(values.take(self.channels));
        self.data.resize(before + self.channels, T::default());
        self.stamps.push(timestamp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn two_by_three() -> Chunk<f32> {
        // 2 channels, 3 samples: channel 0 is 0,10,20 and channel 1 is 1,11,21.
        let mut c = Chunk::<f32>::new(2, 8);
        for (i, t) in [1.0, 2.0, 3.0].into_iter().enumerate() {
            c.push(t, [i as f32 * 10.0, i as f32 * 10.0 + 1.0].into_iter());
        }
        c
    }

    #[test]
    fn a_channel_reads_every_value_of_that_channel() {
        let c = two_by_three();
        assert_eq!(c.len(), 3);
        assert_eq!(c.channel(0).collect::<Vec<_>>(), vec![0.0, 10.0, 20.0]);
        assert_eq!(c.channel(1).collect::<Vec<_>>(), vec![1.0, 11.0, 21.0]);
        assert_eq!(c.timestamps(), &[1.0, 2.0, 3.0]);
    }

    #[test]
    fn a_buffer_that_the_program_keeps_is_cleared_by_the_program() {
        // This crate held a method that appended and read as though it replaced.
        // The g-signals viewer called it in a loop over one buffer and sent every
        // earlier sample again on each pass. The method is gone: `extend` says
        // that it adds, and a `clear` beside it says that the buffer starts over.
        let c = two_by_three();
        let mut dst = vec![99.0];

        dst.extend(c.channel(0));
        assert_eq!(dst, vec![99.0, 0.0, 10.0, 20.0], "extend adds");

        dst.clear();
        dst.extend(c.channel(0));
        assert_eq!(dst, vec![0.0, 10.0, 20.0], "clear first replaces");
    }

    #[test]
    #[should_panic(expected = "channel 2 of a chunk that has 2")]
    fn a_channel_that_does_not_exist_panics() {
        // Without the check this strides into the samples that follow and gives
        // values that look real.
        let _ = two_by_three().channel(2).count();
    }

    #[test]
    #[should_panic(expected = "at least one channel")]
    fn a_chunk_of_no_channel_panics() {
        let _ = Chunk::<f32>::new(0, 8);
    }

    #[test]
    fn clear_keeps_the_allocation_and_drops_the_samples() {
        let mut c = two_by_three();
        c.clear();
        assert!(c.is_empty());
        assert_eq!(c.len(), 0);
        assert_eq!(c.timestamps(), &[] as &[f64]);
        assert_eq!(c.channel(0).count(), 0);
    }
}
