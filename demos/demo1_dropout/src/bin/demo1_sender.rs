//! Machine A (Sender) for Demo 1: "Pull the Wi-Fi Cable".
//!
//! Runs on Windows, Linux, or macOS. Publishes:
//! - `Demo1EEG`: 512 Hz x 2 ch Float32 (ch 0 = synthetic EEG waveform,
//!   ch 1 = exact monotonic sample counter so both liblsl and labstream
//!   measure exact sample loss).
//! - `Demo1Markers`: 1 Hz String event markers (`STIM_0001`, `STIM_0002`, ...).
//!
//! Uses a 60-second `resume/1` history so physical Wi-Fi toggles or cable pulls
//! up to 30+ seconds recover 100 % of samples and markers on labstream while
//! liblsl permanently loses them.

use labstream_net::history::HistoryLimits;
use labstream_net::{local_ipv4_addresses, Outlet, StreamInfo};
use labstream_wire::{Format, Sample, Value};
use std::time::{Duration, Instant};

const SRATE: f64 = 512.0;
const CHUNK: usize = 16; // 31.25 ms per push

fn main() {
    let mut eeg_info = StreamInfo::new("Demo1EEG", "EEG", 2, Format::Float32, SRATE);
    eeg_info.source_id = "demo1-eeg-01".to_string();

    let mut marker_info = StreamInfo::new("Demo1Markers", "Markers", 1, Format::String, 0.0);
    marker_info.source_id = "demo1-markers-01".to_string();

    let limits = HistoryLimits {
        seconds: 60.0,
        bytes: 64 * 1024 * 1024,
        disk_bytes: 0,
        disk_dir: None,
    };

    let eeg_out = Outlet::new_with_limits(eeg_info, limits.clone()).expect("open Demo1EEG outlet");
    let marker_out =
        Outlet::new_with_limits(marker_info, limits).expect("open Demo1Markers outlet");

    println!("============================================================");
    println!("  labstream Demo 1 — Sender (Machine A)");
    println!("============================================================");
    println!("Streams published:");
    println!("  - Demo1EEG     (512 Hz, 2 ch, Float32, 60s resume history)");
    println!("  - Demo1Markers (1 Hz, 1 ch, String, 60s resume history)");
    let ips = local_ipv4_addresses();
    if !ips.is_empty() {
        println!("\nLocal IPv4 addresses (pass one to --peer on Machine B if multicast is blocked):");
        for ip in ips {
            if !ip.is_loopback() {
                println!("  -> {ip}");
            }
        }
    }
    println!("\nTo run the dropout demo:");
    println!("  1. Start run_viewer.sh on Machine B and open http://127.0.0.1:8780");
    println!("  2. Wait until both liblsl and labstream show LIVE.");
    println!("  3. Turn off Wi-Fi (or unplug Ethernet) on THIS machine for 5-10 seconds.");
    println!("  4. Turn Wi-Fi back on and watch liblsl lose data while labstream recovers 100%!\n");

    let t_start = Instant::now();
    let mut seq: u64 = 0;
    let mut marker_id: u64 = 0;
    let mut last_print = Instant::now();
    let mut buf = Vec::with_capacity(CHUNK * 2);

    loop {
        let target_secs = (seq + CHUNK as u64) as f64 / SRATE;
        let elapsed = t_start.elapsed().as_secs_f64();
        if target_secs > elapsed {
            std::thread::sleep(Duration::from_secs_f64(target_secs - elapsed));
        }

        buf.clear();
        for i in 0..CHUNK {
            let k = seq + i as u64;
            let t = k as f64 / SRATE;
            // Realistic multi-band EEG-like signal (10 Hz alpha + 22 Hz beta + 1.5 Hz slow wave).
            let eeg = (2.0 * std::f64::consts::PI * 10.0 * t).sin() * 0.65
                + (2.0 * std::f64::consts::PI * 22.0 * t).sin() * 0.25
                + (2.0 * std::f64::consts::PI * 1.5 * t).sin() * 0.35;
            buf.push(eeg as f32);
            buf.push(k as f32);
        }
        eeg_out.push_chunk_f32(&buf, 0.0);
        seq += CHUNK as u64;

        // Emit one event marker every 512 samples (1.0 s).
        if seq / 512 > marker_id {
            marker_id = seq / 512;
            let text = format!("STIM_{marker_id:04}");
            marker_out.push(&Sample {
                timestamp: 0.0,
                values: vec![Value::Str(text.into_bytes())],
            });
        }

        if last_print.elapsed() >= Duration::from_secs(1) {
            last_print = Instant::now();
            println!(
                "sent: {seq:>8} samples | {marker_id:>4} markers | EEG consumers: {} | Marker consumers: {}",
                eeg_out.consumer_count(),
                marker_out.consumer_count(),
            );
        }
    }
}
