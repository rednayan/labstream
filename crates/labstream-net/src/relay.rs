//! Local-to-network LSL stream relay (`docs/roadmap.md`, step 4).
//!
//! A `liblsl` outlet does not speak `seq/1`, `resume/1`, `dgram/1`, or
//! store-and-forward. [`StreamRelay`] attaches to an existing LSL outlet (for
//! example a `liblsl` device driver on the same machine) and republishes its
//! description, sample bits, and timestamps through a `labstream` [`Outlet`].
//! Downstream `labstream` inlets then cross the bad link with all negotiated
//! features active.

use crate::desc::{Kind, Node};
use crate::history::HistoryLimits;
use crate::{read_fullinfo, Inlet, Outlet, StreamInfo};
use labstream_wire::PackedSample;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

/// XML tag appended to `<desc>` of a relayed stream so an inlet can tell a
/// relayed stream from its source stream even when the names match.
pub const RELAY_DESC_TAG: &str = "labstream_relay";

/// Default suffix appended to relayed stream names and `source_id` values.
pub const DEFAULT_SUFFIX: &str = "-relay";

/// Options for [`StreamRelay::start`].
#[derive(Debug, Clone)]
pub struct RelayOptions {
    /// Suffix appended to the published stream name (default `"-relay"`).
    /// Pass an empty string to keep the original stream name.
    pub name_suffix: String,
    /// Optional session identifier override for the relayed outlet.
    /// `None` keeps the source stream session (or active configuration).
    pub session_id: Option<String>,
    /// Optional `resume` and disk store-and-forward limits for the relayed
    /// outlet. `None` uses the active configuration (`Config::history_limits`).
    pub history_limits: Option<HistoryLimits>,
    /// Input buffer capacity on the relay inlet, in samples.
    /// `None` picks 360 s for regular streams and 3600 samples for irregular
    /// streams.
    pub buffer_samples: Option<i32>,
    /// Timeout for initial connection and `LSL:fullinfo` fetch.
    pub open_timeout: Duration,
}

impl Default for RelayOptions {
    fn default() -> Self {
        RelayOptions {
            name_suffix: DEFAULT_SUFFIX.to_string(),
            session_id: None,
            history_limits: None,
            buffer_samples: None,
            open_timeout: Duration::from_secs(5),
        }
    }
}

/// True when `info` describes a stream published by a `labstream` relay.
///
/// Checks both the `<labstream_relay>` element in `<desc>` (present on
/// `LSL:fullinfo`) and the `"-relay"` suffix on `source_id` (present on
/// short discovery answers too).
pub fn is_relayed(info: &StreamInfo) -> bool {
    let has_tag = info
        .desc
        .children
        .iter()
        .any(|c| c.kind == Kind::Element && c.name == RELAY_DESC_TAG);
    has_tag || info.source_id.ends_with(DEFAULT_SUFFIX)
}

/// Build the relayed [`StreamInfo`] from a full source description.
///
/// Preserves `stream_type`, `channel_count`, `format`, `nominal_srate`, and
/// every existing `<desc>` child node, clears `uid` so the relay outlet gets
/// its own instance identifier, updates `name` and `source_id`, and appends a
/// `<labstream_relay>` metadata element to `<desc>`.
pub fn build_relayed_info(source: &StreamInfo, opts: &RelayOptions) -> StreamInfo {
    let mut out = source.clone();
    out.name = format!("{}{}", source.name, opts.name_suffix);
    out.uid.clear();
    out.v4address.clear();
    out.v4data_port = 0;
    out.v4service_port = 0;
    out.v6address.clear();
    out.v6data_port = 0;
    out.v6service_port = 0;
    out.created_at = 0.0;
    out.hostname = crate::info::hostname();
    if let Some(sess) = &opts.session_id {
        out.session_id = sess.clone();
    }
    out.source_id = if source.source_id.is_empty() {
        format!("{}{}", source.uid, DEFAULT_SUFFIX)
    } else if source.source_id.ends_with(DEFAULT_SUFFIX) {
        source.source_id.clone()
    } else {
        format!("{}{}", source.source_id, DEFAULT_SUFFIX)
    };

    // Append `<labstream_relay>` marker without altering any existing `<desc>` children.
    let mut marker = Node::element(RELAY_DESC_TAG);
    for (tag, val) in [
        ("source_uid", source.uid.as_str()),
        ("source_name", source.name.as_str()),
        ("source_id", source.source_id.as_str()),
        ("source_hostname", source.hostname.as_str()),
    ] {
        let mut child = Node::element(tag);
        if !val.is_empty() {
            child.children.push(Node::text(val));
        }
        marker.children.push(child);
    }
    if out.desc.name.is_empty() {
        out.desc.name = "desc".to_string();
    }
    out.desc.children.push(marker);
    out
}

/// A live relay forwarding one source LSL stream into a `labstream` [`Outlet`].
pub struct StreamRelay {
    source_uid: String,
    source_name: String,
    outlet_info: StreamInfo,
    outlet: Arc<Outlet>,
    forwarded: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl StreamRelay {
    /// Attach to `source`, fetch its full `<desc>`, open a `labstream`
    /// [`Outlet`], and start forwarding packed samples with exact timestamps.
    pub fn start(source: &StreamInfo, opts: RelayOptions) -> std::io::Result<StreamRelay> {
        let full = read_fullinfo(source, opts.open_timeout).unwrap_or_else(|_| source.clone());
        let buffer_samples = opts.buffer_samples.unwrap_or_else(|| {
            if full.nominal_srate > 0.0 {
                ((360.0 * full.nominal_srate).ceil() as i32).max(1)
            } else {
                3600
            }
        });
        let mut inlet = Inlet::open_recovering(source, opts.open_timeout, buffer_samples)?;
        // Never alter source timestamps: pass raw values bit-for-bit.
        inlet.set_postprocessing(0);

        let relayed_desc = build_relayed_info(&full, &opts);
        let limits = opts
            .history_limits
            .unwrap_or_else(|| crate::config::get().history_limits());
        let outlet = Arc::new(Outlet::new_with_limits(relayed_desc, limits)?);
        let outlet_info = outlet.info().clone();

        let forwarded = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let running = Arc::new(AtomicBool::new(true));

        let out_ref = Arc::clone(&outlet);
        let fwd_ref = Arc::clone(&forwarded);
        let stop_ref = Arc::clone(&stop);
        let run_ref = Arc::clone(&running);

        let worker = std::thread::spawn(move || {
            let mut batch: Vec<PackedSample> = Vec::with_capacity(256);
            while !stop_ref.load(Ordering::Relaxed) {
                batch.clear();
                match inlet.pull_chunk_packed(&mut batch, 1024, Duration::from_millis(50)) {
                    Ok(0) => {
                        if inlet.is_finished() {
                            break;
                        }
                    }
                    Ok(n) => {
                        let to_push = std::mem::take(&mut batch);
                        out_ref.push_samples_packed(to_push);
                        fwd_ref.fetch_add(n as u64, Ordering::Relaxed);
                    }
                    Err(_) => break,
                }
            }
            run_ref.store(false, Ordering::SeqCst);
        });

        Ok(StreamRelay {
            source_uid: source.uid.clone(),
            source_name: source.name.clone(),
            outlet_info,
            outlet,
            forwarded,
            stop,
            running,
            worker: Some(worker),
        })
    }

    /// Description published by the relay outlet.
    pub fn info(&self) -> &StreamInfo {
        &self.outlet_info
    }

    /// Instance identifier (`uid`) of the source stream being relayed.
    pub fn source_uid(&self) -> &str {
        &self.source_uid
    }

    /// Name of the source stream being relayed.
    pub fn source_name(&self) -> &str {
        &self.source_name
    }

    /// Total number of samples forwarded so far.
    pub fn forwarded(&self) -> u64 {
        self.forwarded.load(Ordering::Relaxed)
    }

    /// True while the relay forwarding thread is running.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Number of downstream consumers connected to the relay outlet.
    pub fn consumer_count(&self) -> usize {
        self.outlet.consumer_count()
    }

    /// Bytes held by the relay outlet's `resume` history in RAM.
    pub fn history_ram_bytes(&self) -> usize {
        self.outlet.history_ram_bytes()
    }

    /// Bytes held by the relay outlet's `resume` history on disk.
    pub fn history_disk_bytes(&self) -> u64 {
        self.outlet.history_disk_bytes()
    }

    /// Signal the relay to stop and join its worker thread.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
    }
}

impl Drop for StreamRelay {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use labstream_wire::Format;

    #[test]
    fn build_relayed_info_preserves_desc_and_marks_stream_as_relayed() {
        let mut src = StreamInfo::new("BioSemi", "EEG", 8, Format::Float32, 512.0);
        src.uid = "orig-uid-123".to_string();
        src.source_id = "biosemi-01".to_string();
        let mut chs = Node::element("channels");
        let mut ch0 = Node::element("channel");
        let mut lbl = Node::element("label");
        lbl.children.push(Node::text("Fp1"));
        ch0.children.push(lbl);
        chs.children.push(ch0);
        src.desc.name = "desc".to_string();
        src.desc.children.push(chs);

        assert!(!is_relayed(&src));

        let relayed = build_relayed_info(&src, &RelayOptions::default());
        assert!(is_relayed(&relayed));
        assert_eq!(relayed.name, "BioSemi-relay");
        assert_eq!(relayed.source_id, "biosemi-01-relay");
        assert!(relayed.uid.is_empty(), "outlet generates fresh uid");
        assert_eq!(relayed.stream_type, "EEG");
        assert_eq!(relayed.channel_count, 8);
        assert_eq!(relayed.format, Format::Float32);
        assert_eq!(relayed.nominal_srate, 512.0);
        assert!(relayed.desc.children.iter().any(|c| c.name == "channels"));
        assert!(relayed
            .desc
            .children
            .iter()
            .any(|c| c.name == RELAY_DESC_TAG));
    }
}
