//! Live tests for step 3: disk-backed store-and-forward (`docs/roadmap.md`).

mod resume_common;

use labstream_net::history::HistoryLimits;
use labstream_net::{Inlet, Outlet, StreamInfo};
use labstream_wire::Format;
use resume_common::Proxy;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// Push `from..to` (32-channel `f32`, 512 Hz) in chunks of 64 samples, encoding
/// the sample index in channel 0 and channel index in channels 1..32.
fn push_range(outlet: &Outlet, from: usize, to: usize, channels: usize, rate: f64) {
    let chunk = 64usize;
    let mut buf = Vec::with_capacity(chunk * channels);
    let mut k = from;
    while k < to {
        let end = (k + chunk).min(to);
        let n = end - k;
        buf.clear();
        for i in k..end {
            buf.push(i as f32);
            for ch in 1..channels {
                buf.push(ch as f32 * 0.25);
            }
        }
        let end_ts = 1000.0 + (end - 1) as f64 / rate;
        outlet.push_chunk_f32(&buf, end_ts);
        let _ = n;
        k = end;
    }
}

#[test]
fn five_minute_equivalent_cut_at_512hz_32ch_spills_to_disk_with_zero_loss_and_cleans_up() {
    let channels = 32usize;
    let rate = 512.0;
    let ram_cap = 512 * 1024; // 512 KiB RAM cap
    let disk_cap = 64u64 << 20; // 64 MiB disk cap
    let limits = HistoryLimits {
        seconds: f64::INFINITY,
        bytes: ram_cap,
        disk_bytes: disk_cap,
        disk_dir: None,
    };

    let mut info = StreamInfo::new(
        "StoreForward5Min",
        "EEG",
        channels as u32,
        Format::Float32,
        rate,
    );
    info.source_id = "store-forward-5min".to_string();

    let spool_dir;
    {
        let outlet = Outlet::new_with_limits(info, limits).expect("outlet binds");
        let proxy = Proxy::start(outlet.info().v4data_port).expect("proxy binds");
        let via_proxy = proxy.info(outlet.info());

        // Consumer A goes through the cuttable proxy; consumer B stays live directly.
        let mut inlet_a =
            Inlet::open_recovering(&via_proxy, Duration::from_secs(5), 200_000).expect("inlet A");
        let mut inlet_b = Inlet::open_with_buffer(outlet.info(), Duration::from_secs(5), 200_000)
            .expect("inlet B");
        assert!(outlet.wait_for_consumers(Duration::from_secs(2)));

        // Warmup so both consumers attach and `resume/1` history starts.
        let warmup = 512usize;
        push_range(&outlet, 0, warmup, channels, rate);

        let mut data_a = Vec::new();
        let mut ts_a = Vec::new();
        let mut data_b = Vec::new();
        let mut ts_b = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while (ts_a.len() < warmup || ts_b.len() < warmup) && Instant::now() < deadline {
            if ts_a.len() < warmup {
                let rem = warmup - ts_a.len();
                let _ = inlet_a
                    .pull_chunk_f32(&mut data_a, &mut ts_a, rem, Duration::from_millis(50))
                    .unwrap();
            }
            if ts_b.len() < warmup {
                let rem = warmup - ts_b.len();
                let _ = inlet_b
                    .pull_chunk_f32(&mut data_b, &mut ts_b, rem, Duration::from_millis(50))
                    .unwrap();
            }
        }
        assert_eq!(ts_a.len(), warmup);
        assert_eq!(ts_b.len(), warmup);

        // Cut consumer A and push 5 minutes' worth of 512 Hz x 32 ch = 153,600 samples.
        proxy.paused.store(true, Ordering::SeqCst);
        proxy.cut();
        // Wait briefly so A sees the closed connection before the big burst.
        let cut_wait = Instant::now() + Duration::from_secs(2);
        while inlet_a.is_connected() && Instant::now() < cut_wait {
            std::thread::sleep(Duration::from_millis(10));
        }

        let cut_samples = 300 * 512; // 5 minutes at 512 Hz = 153,600 samples (~23 MiB on disk)
        let after_cut = warmup + cut_samples;
        // Drain consumer B in a background thread so B stays live and unblocked.
        let mut b_inlet = inlet_b;
        let tail_samples = 512usize;
        let total = after_cut + tail_samples;
        let b_thread = std::thread::spawn(move || {
            let mut db = Vec::new();
            let mut tb = Vec::new();
            let mut count = warmup;
            let end = Instant::now() + Duration::from_secs(20);
            while count < total && Instant::now() < end {
                db.clear();
                tb.clear();
                let got = b_inlet
                    .pull_chunk_f32(&mut db, &mut tb, 4096, Duration::from_millis(100))
                    .unwrap();
                count += got;
            }
            (count, b_inlet.samples_lost(), b_inlet.samples_dropped())
        });

        let step = 256usize;
        let mut k = warmup;
        while k < after_cut {
            let end = (k + step).min(after_cut);
            push_range(&outlet, k, end, channels, rate);
            // Pace bursts to ~35 KiB/ms so loopback UDP for live consumer B
            // stays within the OS socket receive buffer during the 23 MiB burst.
            std::thread::sleep(Duration::from_millis(1));
            // Criterion 2: RAM stays strictly within ResumeMemory during the cut.
            assert!(
                outlet.history_ram_bytes() <= ram_cap,
                "RAM {} exceeded cap {ram_cap} at sample {end}",
                outlet.history_ram_bytes()
            );
            assert!(
                outlet.history_disk_bytes() <= disk_cap,
                "disk {} exceeded cap {disk_cap}",
                outlet.history_disk_bytes()
            );
            k = end;
        }

        // Verify disk spilling actually occurred (~22+ MiB on disk, <= 512 KiB in RAM).
        assert!(
            outlet.history_disk_bytes() > 15_000_000,
            "expected > 15 MB on disk, got {}",
            outlet.history_disk_bytes()
        );
        assert!(outlet.history_ram_bytes() <= ram_cap);
        spool_dir = outlet.history_spool_dir().expect("spool dir exists");
        assert!(spool_dir.exists());

        // Reconnect consumer A: resend 153,600 samples from disk + live tail!
        proxy.paused.store(false, Ordering::SeqCst);

        // Wait briefly so consumer A starts its disk resend, then push live tail
        // and verify live consumer B receives it promptly (Criterion 3).
        std::thread::sleep(Duration::from_millis(20));
        let live_push_start = Instant::now();
        push_range(&outlet, after_cut, total, channels, rate);
        let live_push_elapsed = live_push_start.elapsed();
        assert!(
            live_push_elapsed < Duration::from_secs(2),
            "live push must not block on disk resend (took {live_push_elapsed:?})"
        );
        let (b_count, b_lost, b_dropped) = b_thread.join().unwrap();
        let b_done_elapsed = live_push_start.elapsed();
        assert_eq!(b_count, total, "live consumer B count");
        assert_eq!(b_lost, Some(0), "live consumer B lost");
        assert_eq!(b_dropped, 0, "live consumer B dropped");
        assert!(
            b_done_elapsed < Duration::from_secs(2),
            "Criterion 3: disk resend must not delay live consumer B (took {b_done_elapsed:?})"
        );

        // Read every sample on consumer A and verify zero loss + exact order.
        let mut got_a = warmup;
        let mut expected_idx = warmup;
        let mut chunk_data = Vec::new();
        let mut chunk_ts = Vec::new();
        let recv_deadline = Instant::now() + Duration::from_secs(30);
        while got_a < total && Instant::now() < recv_deadline {
            chunk_data.clear();
            chunk_ts.clear();
            let n = inlet_a
                .pull_chunk_f32(
                    &mut chunk_data,
                    &mut chunk_ts,
                    4096,
                    Duration::from_millis(200),
                )
                .unwrap();
            for s in 0..n {
                let idx_val = chunk_data[s * channels] as usize;
                assert_eq!(
                    idx_val, expected_idx,
                    "sample order/value mismatch at {expected_idx}"
                );
                assert_eq!(
                    chunk_data[s * channels + channels - 1],
                    (channels - 1) as f32 * 0.25
                );
                expected_idx += 1;
            }
            got_a += n;
        }
        assert_eq!(got_a, total, "consumer A must receive all {total} samples");
        assert_eq!(
            inlet_a.samples_lost(),
            Some(0),
            "Criterion 1: 0 lost samples after 5-min equivalent cut"
        );
        assert!(
            inlet_a.samples_resent().unwrap_or(0) >= cut_samples as u64,
            "expected >= {cut_samples} resent samples, got {:?}",
            inlet_a.samples_resent()
        );

        drop(inlet_a);
    }

    // Criterion 4: the outlet removes the spool directory and all segment files on close.
    // Give session threads a brief moment to drop their Arc<SegmentFile> if still winding down.
    let clean_deadline = Instant::now() + Duration::from_secs(2);
    while spool_dir.exists() && Instant::now() < clean_deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !spool_dir.exists(),
        "spool directory {} must be removed when outlet closes",
        spool_dir.display()
    );
}
