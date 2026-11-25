//! Live tests for step 14 (`enc/1` opt-in negotiated transport encryption).

use labstream_net::enc::{set_runtime_policy, EncryptionPolicy};
use labstream_net::{Inlet, Outlet, StreamInfo};
use labstream_wire::{Format, Sample, Value};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct ResetPolicyOnDrop;
impl Drop for ResetPolicyOnDrop {
    fn drop(&mut self) {
        set_runtime_policy(None);
    }
}

/// Start a transparent loopback TCP proxy that captures every byte sent from
/// the outlet to the inlet so tests can verify no plaintext sample bytes
/// appear on the wire.
fn start_capturing_proxy(target_port: u16) -> (u16, Arc<Mutex<Vec<u8>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("proxy bind");
    let port = listener.local_addr().unwrap().port();
    let captured = Arc::new(Mutex::new(Vec::<u8>::new()));
    let cap_clone = Arc::clone(&captured);
    std::thread::spawn(move || {
        if let Ok((mut client, _)) = listener.accept() {
            if let Ok(mut server) = TcpStream::connect(("127.0.0.1", target_port)) {
                let mut c_read = client.try_clone().unwrap();
                let mut s_write = server.try_clone().unwrap();
                let fwd_up = std::thread::spawn(move || {
                    let mut buf = [0u8; 4096];
                    while let Ok(n) = c_read.read(&mut buf) {
                        if n == 0 || s_write.write_all(&buf[..n]).is_err() {
                            break;
                        }
                    }
                });
                let mut buf = [0u8; 4096];
                while let Ok(n) = server.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    cap_clone.lock().unwrap().extend_from_slice(&buf[..n]);
                    if client.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
                let _ = fwd_up.join();
            }
        }
    });
    (port, captured)
}

#[test]
fn enc_off_by_default_and_encrypts_wire_bytes_when_enabled() {
    let _reset = ResetPolicyOnDrop;

    // 1. Default policy: encryption is OFF.
    set_runtime_policy(None);
    let mut info0 = StreamInfo::new("EncDefaultOff", "Markers", 1, Format::String, 0.0);
    info0.source_id = format!("enc-off-{}", std::process::id());
    let outlet0 = Outlet::new(info0).expect("outlet0");
    let inlet0 = Inlet::open(outlet0.info(), Duration::from_secs(2)).expect("inlet0");
    assert!(
        !inlet0.is_encrypted(),
        "enc/1 must be off by default unless enabled"
    );
    drop(inlet0);
    drop(outlet0);

    // 2. Opt-in encryption with PSK: wire capture holds ZERO plaintext sample bytes.
    set_runtime_policy(Some(EncryptionPolicy::opportunistic(Some(
        "clinical-lab-key-2026".to_string(),
    ))));
    let mut info = StreamInfo::new("EncMarkers", "Markers", 1, Format::String, 0.0);
    info.source_id = format!("enc-on-{}", std::process::id());
    let outlet = Outlet::new(info).expect("outlet");
    let mut proxied = outlet.info().clone();
    let (proxy_port, captured) = start_capturing_proxy(proxied.v4data_port);
    proxied.v4address = "127.0.0.1".to_string();
    proxied.v4data_port = proxy_port;

    let mut inlet = Inlet::open(&proxied, Duration::from_secs(2)).expect("encrypted inlet");
    assert!(outlet.wait_for_consumers(Duration::from_secs(2)));
    assert!(inlet.is_encrypted(), "inlet must negotiate enc/1");
    assert!(inlet.health().encrypted);

    let secret_marker = b"PATIENT_ID_99887766_SEIZURE_ONSET";
    outlet.push(&Sample {
        timestamp: 1234.5,
        values: vec![Value::Str(secret_marker.to_vec())],
    });
    let got = inlet
        .pull(Duration::from_secs(2))
        .unwrap()
        .expect("decrypted marker");
    assert_eq!(got.timestamp.to_bits(), 1234.5f64.to_bits());
    assert_eq!(got.values[0], Value::Str(secret_marker.to_vec()));

    // Criterion 1: no plaintext sample bytes appear anywhere in the wire capture.
    let wire = captured.lock().unwrap();
    assert!(wire.len() > secret_marker.len());
    assert!(
        !wire
            .windows(secret_marker.len())
            .any(|w| w == secret_marker),
        "plaintext sample bytes leaked onto the TCP wire!"
    );

    // 3. Criterion 3: with RequireEncryption = true, connecting to a plain
    // outlet fails with a clear error message.
    drop(inlet);
    drop(outlet);

    // Start an outlet while policy is OFF (simulating a plain/liblsl outlet).
    set_runtime_policy(Some(EncryptionPolicy::off()));
    let mut plain_info = StreamInfo::new("PlainPeer", "EEG", 2, Format::Float32, 250.0);
    plain_info.source_id = format!("enc-plain-{}", std::process::id());
    let plain_outlet = Outlet::new(plain_info).expect("plain outlet");
    // Wait briefly so outlet is accepting, then flip inlet to required via a
    // raw TCP check? Note: outlet reads `effective_policy()` on accept, so to
    // test a plain peer against a required inlet we simulate a plain LSL server
    // response on loopback (just like liblsl!).
    let fake_lsl = TcpListener::bind("127.0.0.1:0").unwrap();
    let fake_port = fake_lsl.local_addr().unwrap().port();
    let uid = plain_outlet.info().uid.clone();
    std::thread::spawn(move || {
        if let Ok((mut s, _)) = fake_lsl.accept() {
            let mut req = [0u8; 2048];
            let _ = s.read(&mut req);
            // Plain liblsl answer: no Labstream-Features header.
            let resp = format!(
                "LSL/110 200 OK\r\nUID: {uid}\r\nByte-Order: 1234\r\nSuppress-Subnormals: 0\r\nData-Protocol-Version: 110\r\n\r\n"
            );
            let _ = s.write_all(resp.as_bytes());
            let _ = s.flush();
            std::thread::sleep(Duration::from_millis(200));
        }
    });
    let mut target = plain_outlet.info().clone();
    target.v4address = "127.0.0.1".to_string();
    target.v4data_port = fake_port;

    set_runtime_policy(Some(EncryptionPolicy::required(None)));
    let err = match Inlet::open(&target, Duration::from_secs(2)) {
        Ok(_) => panic!("required encryption must refuse plain peer"),
        Err(e) => e,
    };
    let msg = err.to_string().to_ascii_lowercase();
    assert!(
        msg.contains("encryption"),
        "error message must clearly mention encryption, got: {msg}"
    );

    // 4. Verify numeric EEG stream + comp/1 + meta/1 compose cleanly inside enc/1.
    set_runtime_policy(Some(EncryptionPolicy::required(Some(
        "eeg-psk-123".to_string(),
    ))));
    let mut eeg_info = StreamInfo::new("EncEeg", "EEG", 8, Format::Float32, 512.0);
    eeg_info.source_id = format!("enc-eeg-{}", std::process::id());
    let eeg_out = Outlet::new(eeg_info).expect("eeg out");
    let mut eeg_in =
        Inlet::open_with_buffer(eeg_out.info(), Duration::from_secs(2), 4096).expect("eeg in");
    assert!(eeg_out.wait_for_consumers(Duration::from_secs(2)));
    assert!(eeg_in.is_encrypted());
    assert!(!eeg_in.on_shm());
    assert!(!eeg_in.on_udp());

    eeg_out.push_metadata_at("impedance/Fp1", "3.2 kOhm", 50.0);
    let vals: Vec<f32> = (0..16 * 8).map(|i| i as f32 * 0.125).collect();
    eeg_out.push_chunk_f32(&vals, 50.03125);

    let mut got_vals = Vec::new();
    let mut got_ts = Vec::new();
    let end = Instant::now() + Duration::from_secs(2);
    while got_ts.len() < 16 && Instant::now() < end {
        let rem = 16 - got_ts.len();
        let _ = eeg_in
            .pull_chunk_f32(&mut got_vals, &mut got_ts, rem, Duration::from_millis(50))
            .unwrap();
    }
    assert_eq!(got_vals, vals);
    let mut metas = Vec::new();
    while metas.is_empty() && Instant::now() < end {
        metas.extend(eeg_in.pull_metadata());
        if metas.is_empty() {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    assert_eq!(metas.len(), 1);
    assert_eq!(metas[0].value, "3.2 kOhm");
}
