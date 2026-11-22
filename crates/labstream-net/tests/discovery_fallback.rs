//! Integration tests for fallback discovery and `labstream doctor`.

use labstream_net::doctor::{self, Options, Status};
use labstream_net::{Outlet, StreamInfo};
use labstream_wire::Format;
use std::time::Duration;

#[test]
fn doctor_finds_local_outlet_and_probes_loopback_peer() {
    let name = format!("DoctorLocal_{}", std::process::id());
    let info = StreamInfo::new(&name, "Test", 2, Format::Float32, 100.0);
    let outlet = match Outlet::new(info) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("skipping doctor live test (no local network): {e}");
            return;
        }
    };
    let expected_port = outlet.info().v4data_port;

    let opts = Options {
        peers: vec!["127.0.0.1".to_string()],
        timeout: Duration::from_millis(1200),
    };
    let report = doctor::diagnose(&opts);

    let text = report.format_text();
    assert!(text.contains("labstream doctor"), "text:\n{text}");
    assert!(
        report.streams.iter().any(|s| s.name == name),
        "expected stream {name} in report:\n{text}"
    );

    let peer_check = report
        .checks
        .iter()
        .find(|c| c.name == "peer:127.0.0.1")
        .expect("missing peer:127.0.0.1 check");
    assert_eq!(
        peer_check.status,
        Status::Ok,
        "peer check failed: {peer_check:?}\n{text}"
    );
    assert!(
        peer_check.summary.contains("TCP"),
        "expected TCP result in peer summary: {}",
        peer_check.summary
    );

    let json = report.to_json();
    assert!(json.starts_with('{') && json.ends_with('}'));
    assert!(json.contains(&name), "stream missing from JSON: {json}");
    assert!(
        json.contains(&format!("\"data_port\":{expected_port}")),
        "expected data port {expected_port} in JSON: {json}"
    );

    drop(outlet);
}
