//! Find a dead TCP connection in seconds.
//!
//! A connection can die with no close: a cable is pulled, a Wi-Fi link drops,
//! or a NAT forgets the connection. Each side then waits for bytes that never
//! come. liblsl finds this only through its watchdog (15 s), and only when the
//! outlet is also gone from discovery.
//!
//! TCP keepalive sends a probe on a quiet connection, and the kernel ends the
//! connection when the peer does not answer. A peer that is alive answers each
//! probe, so a stream that is quiet but alive is not ended. The probes are an
//! option of the local socket and carry no data, so a liblsl peer sees no
//! difference.

use std::net::TcpStream;
use std::time::Duration;

/// How long a connection is quiet before the first keepalive probe.
pub(crate) const IDLE: Duration = Duration::from_secs(2);
/// The time between two keepalive probes.
pub(crate) const INTERVAL: Duration = Duration::from_secs(1);
/// How many probes go unanswered before the kernel ends the connection.
pub(crate) const RETRIES: u32 = 3;
/// How long sent data can stay without an acknowledgement on Linux.
///
/// Keepalive does not run while data waits for an acknowledgement. An outlet
/// that writes to a dead consumer is in that state. Without this value, Linux
/// retransmits for about 15 minutes before it gives up.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) const USER_TIMEOUT: Duration = Duration::from_secs(6);

/// Turn on keepalive with short timers on a connected stream.
///
/// An error is ignored. The connection then works as before, and the
/// watchdog still applies.
pub(crate) fn enable(stream: &TcpStream) {
    let sock = socket2::SockRef::from(stream);
    let params = socket2::TcpKeepalive::new()
        .with_time(IDLE)
        .with_interval(INTERVAL);
    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "windows",
    ))]
    let params = params.with_retries(RETRIES);
    let _ = sock.set_tcp_keepalive(&params);
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let _ = sock.set_tcp_user_timeout(Some(USER_TIMEOUT));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn a_connected_stream_takes_the_short_timers() {
        let listener = match TcpListener::bind("127.0.0.1:0") {
            Ok(l) => l,
            Err(_) => {
                eprintln!("no local network; skipping");
                return;
            }
        };
        let stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        enable(&stream);
        let sock = socket2::SockRef::from(&stream);
        assert!(sock.keepalive().unwrap());
        // The timers are read back only where this machine checked the
        // getters. Elsewhere the test checks that keepalive is on.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            assert_eq!(sock.tcp_keepalive_time().unwrap(), IDLE);
            assert_eq!(sock.tcp_keepalive_interval().unwrap(), INTERVAL);
            assert_eq!(sock.tcp_keepalive_retries().unwrap(), RETRIES);
        }
        #[cfg(any(target_os = "linux", target_os = "android"))]
        assert_eq!(sock.tcp_user_timeout().unwrap(), Some(USER_TIMEOUT));
    }
}
