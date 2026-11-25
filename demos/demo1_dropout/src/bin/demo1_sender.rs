//! Machine A (Sender) for Demo 1: "Pull the Wi-Fi Cable".
//!
//! Runs on Windows, Linux, or macOS. Publishes:
//! - `Demo1EEG`: 512 Hz x 2 ch Float32 (ch 0 = synthetic EEG waveform + live
//!   user-injected spikes, ch 1 = exact monotonic sample counter).
//! - `Demo1Markers`:
//!   - 1 Hz CRC32-verified block markers (`STIM_0001|<crc32hex>`) proving
//!     bit-exact recovery of every 512-sample second.
//!   - Instant user-triggered markers (`EVENT_<label>@<seq>`) when you press
//!     Enter in this terminal — press Enter WHILE Wi-Fi IS OFF to prove that
//!     unpredictable offline events arrive intact on labstream!

use labstream_net::history::HistoryLimits;
use labstream_net::{local_ipv4_addresses, Outlet, StreamInfo};
use labstream_wire::{Format, Sample, Value};
use std::io::BufRead;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const SRATE: f64 = 512.0;
const CHUNK: usize = 16; // 31.25 ms per push

fn crc32_update(mut crc: u32, bytes: &[u8]) -> u32 {
    for &b in bytes {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    crc
}

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

    // Stdin thread: pressing Enter (optionally with a custom label) injects a
    // sharp unpredictable biphasic EEG spike + EVENT marker.
    let pending_events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let ev = Arc::clone(&pending_events);
        std::thread::spawn(move || {
            let stdin = std::io::stdin();
            let mut count = 0u64;
            for line in stdin.lock().lines().map_while(Result::ok) {
                count += 1;
                let trimmed = line.trim();
                let label = if trimmed.is_empty() {
                    format!("CLICK_{count}")
                } else {
                    trimmed.replace(['|', '@', '"', '\\'], "_")
                };
                ev.lock().unwrap().push(label);
            }
        });
    }

    println!("============================================================");
    println!("  labstream Demo 1 — Sender (Machine A)");
    println!("============================================================");
    println!("Streams published:");
    println!("  - Demo1EEG     (512 Hz, 2 ch, Float32, 60s resume history)");
    println!("  - Demo1Markers (1 Hz CRC32 checksums + live Enter-key events)");
    let ips = local_ipv4_addresses();
    if !ips.is_empty() {
        println!(
            "\nLocal IPv4 addresses (pass one to --peer on Machine B if multicast is blocked):"
        );
        for ip in ips {
            if !ip.is_loopback() {
                println!("  -> {ip}");
            }
        }
    }
    println!("\nTo run the dropout + bit-exact proof demo:");
    println!("  1. Start run_viewer.sh on Machine B and open http://127.0.0.1:8780");
    println!("  2. Turn Wi-Fi OFF on THIS machine for 5-10 seconds.");
    println!("  3. WHILE OFFLINE, press [Enter] (or type a word + Enter) to inject an");
    println!("     unpredictable spike + marker!");
    println!("  4. Turn Wi-Fi back ON: watch your offline event + 100% CRC32 match");
    println!("     appear inside labstream's gold recovered window!\n");

    let t_start = Instant::now();
    let mut seq: u64 = 0;
    let mut marker_id: u64 = 0;
    let mut block_crc: u32 = 0xFFFF_FFFF;
    let mut spike_rem: i32 = 0;
    let mut last_print = Instant::now();
    let mut buf = Vec::with_capacity(CHUNK * 2);

    loop {
        let target_secs = (seq + CHUNK as u64) as f64 / SRATE;
        let elapsed = t_start.elapsed().as_secs_f64();
        if target_secs > elapsed {
            std::thread::sleep(Duration::from_secs_f64(target_secs - elapsed));
        }

        // Check for user-triggered offline events from stdin.
        let triggered: Vec<String> = {
            let mut q = pending_events.lock().unwrap();
            std::mem::take(&mut *q)
        };
        for label in triggered {
            spike_rem = 20; // sharp 20-sample (~39 ms) biphasic pulse
            let text = format!("EVENT_{label}@{seq}");
            marker_out.push(&Sample {
                timestamp: 0.0,
                values: vec![Value::Str(text.into_bytes())],
            });
            println!("  >>> INJECTED LIVE EVENT: {label} at sample #{seq} (with EEG spike!)");
        }

        buf.clear();
        for i in 0..CHUNK {
            let k = seq + i as u64;
            let t = k as f64 / SRATE;
            let mut eeg = ((2.0 * std::f64::consts::PI * 10.0 * t).sin() * 0.55
                + (2.0 * std::f64::consts::PI * 22.0 * t).sin() * 0.22
                + (2.0 * std::f64::consts::PI * 1.5 * t).sin() * 0.30) as f32;
            if spike_rem > 0 {
                // Unmistakable sharp biphasic spike (+1.9 then -1.6)
                eeg += if spike_rem > 10 { 1.9 } else { -1.6 };
                spike_rem -= 1;
            }
            block_crc = crc32_update(block_crc, &eeg.to_le_bytes());
            buf.push(eeg);
            buf.push(k as f32);

            // End of each 512-sample (1 s) block: emit CRC32-signed marker.
            if (k + 1) % 512 == 0 {
                marker_id = (k + 1) / 512;
                let final_crc = !block_crc;
                let text = format!("STIM_{marker_id:04}|{final_crc:08x}");
                marker_out.push(&Sample {
                    timestamp: 0.0,
                    values: vec![Value::Str(text.into_bytes())],
                });
                block_crc = 0xFFFF_FFFF;
            }
        }
        eeg_out.push_chunk_f32(&buf, 0.0);
        seq += CHUNK as u64;

        if last_print.elapsed() >= Duration::from_secs(1) {
            last_print = Instant::now();
            println!(
                "sent: {seq:>8} samples | {marker_id:>4} CRC blocks | EEG consumers: {} | Marker consumers: {}  (press Enter anytime to inject event)",
                eeg_out.consumer_count(),
                marker_out.consumer_count(),
            );
        }
    }
}
