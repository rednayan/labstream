//! The description of a stream, and the builder that makes one.

use crate::Error;
use lsl_wire::Format;

/// One channel of a stream.
///
/// A device declares these in the description tree. Most declare nothing, so
/// every field can be empty.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Channel {
    /// The name of the electrode or axis, such as `Fp1` or `ax`.
    pub label: String,
    /// The unit of the value, such as `microvolts`.
    pub unit: String,
    /// What the channel measures, such as `EEG`.
    pub kind: String,
}

impl Channel {
    /// A channel with a label and no other declaration.
    pub fn new(label: &str) -> Channel {
        Channel {
            label: label.to_string(),
            ..Default::default()
        }
    }

    /// The same channel, with a unit.
    pub fn unit(mut self, unit: &str) -> Channel {
        self.unit = unit.to_string();
        self
    }

    /// The same channel, with a measurement type.
    pub fn kind(mut self, kind: &str) -> Channel {
        self.kind = kind.to_string();
        self
    }
}

/// The description of one stream.
#[derive(Debug, Clone)]
pub struct StreamInfo {
    pub(crate) inner: lsl_net::StreamInfo,
}

impl StreamInfo {
    /// Start a description.
    ///
    /// The builder sets the channel count from the channels. A count that does
    /// not agree with the description tree is therefore not possible.
    pub fn builder(name: &str, stream_type: &str, format: Format) -> Builder {
        Builder {
            name: name.to_string(),
            stream_type: stream_type.to_string(),
            format,
            rate: 0.0,
            source_id: String::new(),
            channels: Vec::new(),
            count: None,
        }
    }

    /// The name that the source published.
    pub fn name(&self) -> &str {
        &self.inner.name
    }

    /// The content type, such as `EEG`.
    pub fn stream_type(&self) -> &str {
        &self.inner.stream_type
    }

    /// The identifier that stays the same when the source restarts.
    pub fn source_id(&self) -> &str {
        &self.inner.source_id
    }

    /// The host that the source runs on.
    pub fn hostname(&self) -> &str {
        &self.inner.hostname
    }

    /// The format of every value.
    pub fn format(&self) -> Format {
        self.inner.format
    }

    /// How many channels one sample holds.
    pub fn channel_count(&self) -> usize {
        self.inner.channel_count as usize
    }

    /// The nominal rate in hertz. A rate of zero means an irregular stream.
    pub fn rate(&self) -> f64 {
        self.inner.nominal_srate
    }

    /// True when the stream has a nominal rate.
    ///
    /// A regular stream carries a timeline. An irregular stream, such as a
    /// marker stream, carries events.
    pub fn is_regular(&self) -> bool {
        self.inner.nominal_srate > 0.0
    }

    /// The channels that the stream declares.
    ///
    /// The list always holds [`StreamInfo::channel_count`] entries. A channel
    /// that the stream does not name gets the name `Ch1` to `ChN`.
    ///
    /// The description tree does not arrive with discovery. It arrives on the
    /// data port. A description from [`crate::resolve_all`] therefore reports no
    /// channels. Call [`StreamInfo::fetch`] first, or read this from
    /// [`crate::Inlet::info`].
    ///
    /// Two conventions exist for the name. LSL documents `<label>`, and many
    /// sources write `<name>`. This reads `<label>` first and `<name>` second,
    /// because a source that writes only `<name>` is common.
    pub fn channels(&self) -> Vec<Channel> {
        let n = self.channel_count();
        let mut out: Vec<Channel> = Vec::with_capacity(n);
        if let Some(list) = self.inner.desc.child("channels") {
            for node in list.children.iter().filter(|c| c.name == "channel").take(n) {
                let mut label = node.child_value_of("label").trim();
                if label.is_empty() {
                    label = node.child_value_of("name").trim();
                }
                out.push(Channel {
                    label: label.to_string(),
                    unit: node.child_value_of("unit").trim().to_string(),
                    kind: node.child_value_of("type").trim().to_string(),
                });
            }
        }
        while out.len() < n {
            out.push(Channel::new(&format!("Ch{}", out.len() + 1)));
        }
        for (i, c) in out.iter_mut().enumerate() {
            if c.label.is_empty() {
                c.label = format!("Ch{}", i + 1);
            }
        }
        out
    }

    /// Read the whole description from the source, with the description tree.
    ///
    /// This opens its own short connection. It does not open a feed, so a browser
    /// can read the channels of a stream that it does not read data from.
    pub fn fetch(&self, timeout: std::time::Duration) -> Result<StreamInfo, Error> {
        let inner = lsl_net::read_fullinfo(&self.inner, timeout)?;
        Ok(StreamInfo { inner })
    }

    /// The description tree, for a program that reads a field this type does not
    /// give.
    pub fn desc(&self) -> &lsl_net::desc::Node {
        &self.inner.desc
    }
}

/// The builder for [`StreamInfo`].
///
/// The build fails when the description is not consistent. This removes a class
/// of fault that liblsl accepts: a channel count of 8 with 6 channels in the
/// description tree.
#[derive(Debug, Clone)]
pub struct Builder {
    name: String,
    stream_type: String,
    format: Format,
    rate: f64,
    source_id: String,
    channels: Vec<Channel>,
    count: Option<usize>,
}

impl Builder {
    /// Set the nominal rate in hertz.
    pub fn rate(mut self, hz: f64) -> Builder {
        self.rate = hz;
        self
    }

    /// Mark the stream irregular. Marker streams are irregular.
    pub fn irregular(mut self) -> Builder {
        self.rate = 0.0;
        self
    }

    /// Set the identifier that stays the same when the source restarts.
    ///
    /// Set this whenever the source can identify itself. An inlet finds a stream
    /// again by this value.
    pub fn source_id(mut self, id: &str) -> Builder {
        self.source_id = id.to_string();
        self
    }

    /// Set how many channels one sample holds, with no channel names.
    pub fn channel_count(mut self, n: usize) -> Builder {
        self.count = Some(n);
        self
    }

    /// Name every channel. This also sets the channel count.
    pub fn channel_labels<I, S>(mut self, labels: I) -> Builder
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.channels = labels
            .into_iter()
            .map(|l| Channel::new(l.as_ref()))
            .collect();
        self
    }

    /// Declare every channel with its unit and type. This also sets the count.
    pub fn channels<I>(mut self, channels: I) -> Builder
    where
        I: IntoIterator<Item = Channel>,
    {
        self.channels = channels.into_iter().collect();
        self
    }

    /// Build the description.
    ///
    /// # Errors
    /// The build fails when the name is empty, when the rate is negative, or when
    /// a channel count and a channel list disagree.
    pub fn build(self) -> Result<StreamInfo, Error> {
        if self.name.is_empty() {
            return Err(Error::Invalid("a stream needs a name"));
        }
        if self.rate < 0.0 {
            return Err(Error::Invalid("a rate cannot be negative"));
        }
        let n = match (self.count, self.channels.len()) {
            (Some(a), 0) => a,
            (Some(a), b) if a != b => {
                return Err(Error::Invalid(
                    "the channel count and the channel list disagree",
                ))
            }
            (_, b) => b,
        };
        if n == 0 {
            return Err(Error::Invalid("a stream needs at least one channel"));
        }

        let mut inner = lsl_net::StreamInfo::new(
            &self.name,
            &self.stream_type,
            n as u32,
            self.format,
            self.rate,
        );
        inner.source_id = self.source_id;
        if !self.channels.is_empty() {
            let list = inner.desc.append_child("channels");
            for c in &self.channels {
                let node = list.append_child("channel");
                node.append_child_value("label", &c.label);
                if !c.unit.is_empty() {
                    node.append_child_value("unit", &c.unit);
                }
                if !c.kind.is_empty() {
                    node.append_child_value("type", &c.kind);
                }
            }
        }
        Ok(StreamInfo { inner })
    }
}
