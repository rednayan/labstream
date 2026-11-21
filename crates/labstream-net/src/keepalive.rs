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
    set(stream, IDLE, INTERVAL, RETRIES, USER_TIMEOUT_SHORT);
}

/// The timers while the samples of a `dgram/1` connection go on UDP. The
/// datagrams then show that the peer is alive, and the TCP connection is
/// quiet. `docs/extensions.md`, section `dgram`, keepalive on UDP.
pub(crate) const UDP_IDLE: Duration = Duration::from_secs(30);
/// The time between two probes on UDP.
pub(crate) const UDP_INTERVAL: Duration = Duration::from_secs(5);
/// The count of probes on UDP.
pub(crate) const UDP_RETRIES: u32 = 6;
/// The acknowledgement timeout on UDP.
#[cfg_attr(not(any(target_os = "linux", target_os = "android")), allow(dead_code))]
pub(crate) const UDP_USER_TIMEOUT: Duration = Duration::from_secs(60);

#[cfg(any(target_os = "linux", target_os = "android"))]
const USER_TIMEOUT_SHORT: Duration = USER_TIMEOUT;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
const USER_TIMEOUT_SHORT: Duration = Duration::ZERO;

/// Use the long timers of UDP on this connection.
pub(crate) fn relax(stream: &TcpStream) {
    set(
        stream,
        UDP_IDLE,
        UDP_INTERVAL,
        UDP_RETRIES,
        UDP_USER_TIMEOUT,
    );
}

#[allow(unused_variables)]
fn set(stream: &TcpStream, idle: Duration, interval: Duration, retries: u32, user: Duration) {
    let sock = socket2::SockRef::from(stream);
    let params = socket2::TcpKeepalive::new()
        .with_time(idle)
        .with_interval(interval);
    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "windows",
    ))]
    let params = params.with_retries(retries);
    let _ = sock.set_tcp_keepalive(&params);
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let _ = sock.set_tcp_user_timeout(Some(user));
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

    #[test]
    fn relax_takes_the_long_timers_and_enable_the_short_ones() {
        let listener = match TcpListener::bind("127.0.0.1:0") {
            Ok(l) => l,
            Err(_) => {
                eprintln!("no local network; skipping");
                return;
            }
        };
        let stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        relax(&stream);
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            let sock = socket2::SockRef::from(&stream);
            assert_eq!(sock.tcp_keepalive_time().unwrap(), UDP_IDLE);
            assert_eq!(sock.tcp_keepalive_interval().unwrap(), UDP_INTERVAL);
            assert_eq!(sock.tcp_keepalive_retries().unwrap(), UDP_RETRIES);
            assert_eq!(sock.tcp_user_timeout().unwrap(), Some(UDP_USER_TIMEOUT));
            enable(&stream);
            assert_eq!(sock.tcp_keepalive_time().unwrap(), IDLE);
            assert_eq!(sock.tcp_user_timeout().unwrap(), Some(USER_TIMEOUT));
        }
    }
}
