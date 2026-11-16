//! Recovery after a lost connection, measured against a live outlet.
//!
//! These tests open real sockets on the loopback interface, so they belong to
//! the reporting tier and not to the merge gate. A failure here can come from
//! the machine.
//!
//! A proxy sits between the inlet and the outlet. The test closes the proxied
//! connection while the outlet continues to run. This is what a network fault
//! does to an inlet on another machine. liblsl connects again to the same
//! endpoint (`src/data_receiver.cpp:331-345`).

use labstream_net::{Inlet, Outlet, StreamInfo};
use labstream_wire::{Format, Sample, Value};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A TCP proxy that forwards each connection to `target`.
///
/// The proxy keeps a copy of each accepted socket, so the test can close it.
struct Proxy {
    port: u16,
    accepted: Arc<Mutex<Vec<TcpStream>>>,
}

impl Proxy {
    fn start(target: u16) -> Option<Proxy> {
        let listener = TcpListener::bind("127.0.0.1:0").ok()?;
        let port = listener.local_addr().ok()?.port();
        let accepted = Arc::new(Mutex::new(Vec::new()));
        let list = Arc::clone(&accepted);
        std::thread::spawn(move || {
            for client in listener.incoming() {
                let client = match client {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                let server = match TcpStream::connect(("127.0.0.1", target)) {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                list.lock().unwrap().push(client.try_clone().unwrap());
                pipe(client.try_clone().unwrap(), server.try_clone().unwrap());
                pipe(server, client);
            }
        });
        Some(Proxy { port, accepted })
    }

    /// Close every connection that the proxy accepted until now.
    fn cut(&self) {
        for s in self.accepted.lock().unwrap().drain(..) {
            let _ = s.shutdown(Shutdown::Both);
        }
    }

    fn connections(&self) -> usize {
        self.accepted.lock().unwrap().len()
    }
}

/// Copy bytes from one socket to the other until one side closes.
fn pipe(mut from: TcpStream, mut to: TcpStream) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 16384];
        loop {
            match from.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if to.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = to.shutdown(Shutdown::Both);
        let _ = from.shutdown(Shutdown::Both);
    });
}

fn sample(k: usize) -> Sample {
    Sample {
        timestamp: 0.0,
        values: vec![Value::F32(k as f32)],
    }
}

fn value(s: &Sample) -> f32 {
    match s.values[0] {
        Value::F32(v) => v,
        _ => panic!("wrong format"),
    }
}

#[test]
fn a_cut_connection_to_a_running_outlet_connects_again() {
    let mut info = StreamInfo::new("RecoverLive1", "RecoverTest", 1, Format::Float32, 100.0);
    info.source_id = "RecoverLive1_src".to_string();
    let outlet = match Outlet::new(info) {
        Ok(o) => o,
        Err(_) => {
            eprintln!("no local network; skipping");
            return;
        }
    };
    let real = outlet.info().clone();
    let proxy = match Proxy::start(real.v4data_port) {
        Some(p) => p,
        None => {
            eprintln!("no local network; skipping");
            return;
        }
    };

    // The inlet knows the outlet only through the proxy port.
    let mut via_proxy = real.clone();
    via_proxy.v4address = "127.0.0.1".to_string();
    via_proxy.v4data_port = proxy.port;
    let mut inlet = Inlet::open_recovering(&via_proxy, Duration::from_secs(5), 4096)
        .expect("the inlet connects through the proxy");
    assert!(outlet.wait_for_consumers(Duration::from_secs(5)));

    // Samples before the cut.
    let mut k = 0usize;
    let mut got = Vec::new();
    let end = Instant::now() + Duration::from_secs(5);
    while got.is_empty() && Instant::now() < end {
        outlet.push(&sample(k));
        k += 1;
        let _ = inlet.pull_chunk(&mut got, 1000, Duration::from_millis(20));
    }
    assert!(!got.is_empty(), "samples arrive before the cut");

    proxy.cut();
    let cut_at = k;

    // Push until samples from after the cut arrive. The stale consumer can
    // still be registered in the outlet, so a wait for consumers proves
    // nothing here.
    let mut after = Vec::new();
    let end = Instant::now() + Duration::from_secs(10);
    while Instant::now() < end {
        outlet.push(&sample(k));
        k += 1;
        let mut block = Vec::new();
        let _ = inlet.pull_chunk(&mut block, 1000, Duration::from_millis(20));
        after.extend(block.into_iter().filter(|s| value(s) as usize > cut_at));
        if after.len() >= 10 {
            break;
        }
    }
    assert!(
        after.len() >= 10,
        "samples flow again after the cut, got {}",
        after.len()
    );
    assert!(inlet.recoveries() >= 1, "the inlet counts the recovery");
    assert!(inlet.is_connected());
    assert_eq!(inlet.current_info().uid, real.uid, "the same outlet");
    assert!(
        proxy.connections() >= 1,
        "the new connection went through the proxy"
    );
}

#[test]
fn a_restarted_outlet_is_found_and_the_clock_follows_it() {
    let make = || {
        let mut info = StreamInfo::new("RecoverLive2", "RecoverTest", 1, Format::Float32, 100.0);
        info.source_id = "RecoverLive2_src".to_string();
        Outlet::new(info)
    };
    let first = match make() {
        Ok(o) => o,
        Err(_) => {
            eprintln!("no local network; skipping");
            return;
        }
    };
    let old = first.info().clone();
    let mut inlet =
        Inlet::open_recovering(&old, Duration::from_secs(5), 4096).expect("the inlet connects");
    assert!(first.wait_for_consumers(Duration::from_secs(5)));
    assert!(
        inlet
            .wait_for_time_correction(Duration::from_secs(5))
            .is_some(),
        "the first source gives an offset"
    );
    drop(first);

    let second = make().expect("the second outlet opens");
    let new_uid = second.info().uid.clone();
    assert_ne!(new_uid, old.uid);

    let mut got = Vec::new();
    let end = Instant::now() + Duration::from_secs(20);
    let mut k = 0usize;
    while got.is_empty() && Instant::now() < end {
        second.push(&sample(k));
        k += 1;
        let _ = inlet.pull_chunk(&mut got, 1000, Duration::from_millis(20));
    }
    assert!(!got.is_empty(), "samples of the second outlet arrive");
    assert_eq!(inlet.current_info().uid, new_uid);
    assert!(
        inlet
            .wait_for_time_correction(Duration::from_secs(5))
            .is_some(),
        "the probes reach the service port of the second outlet"
    );
}
