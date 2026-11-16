//! The clock offset that an inlet measures against its outlet. SPEC.md 3.
//!
//! A thread sends a burst of probes, keeps the estimate with the lowest
//! round-trip time, and publishes it. The post-processor reads that value
//! through [`OffsetSource`], so the arithmetic stays in `labstream-time` where a test
//! can drive it with fixed numbers.

use labstream_proto::timesync::{self, Burst};
use labstream_time::OffsetSource;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

// The cadence below is the default. A configuration file changes it, and both
// libraries then move together. `crates/labstream-net/src/config.rs` reads the file.

/// How long between two bursts. `src/api_config.cpp:314`.
pub fn update_interval() -> Duration {
    Duration::from_secs_f64(crate::config::get().time_update_interval)
}
/// How many probes one burst sends. `src/api_config.cpp:316`.
pub fn probe_count() -> usize {
    crate::config::get().time_probe_count as usize
}
/// How long between two probes of one burst. `src/api_config.cpp:317`.
pub fn probe_interval() -> Duration {
    Duration::from_secs_f64(crate::config::get().time_probe_interval)
}
/// How long a burst waits for the last answer. `src/api_config.cpp:318`.
pub fn probe_max_rtt() -> Duration {
    Duration::from_secs_f64(crate::config::get().time_probe_max_rtt)
}
/// How many answers a burst needs before it publishes. `src/api_config.cpp:315`.
pub fn update_min_probes() -> usize {
    crate::config::get().time_update_min_probes as usize
}

/// How long the post-processor waits for the first offset.
///
/// liblsl passes the same value to `time_correction`
/// (`src/stream_inlet_impl.h:44`).
pub const FIRST_OFFSET_TIMEOUT: Duration = Duration::from_secs(5);

/// The gap between two probes while no offset exists.
///
/// The first burst sends its probes close together, so the first offset
/// arrives in tens of milliseconds and not in a whole burst of
/// [`probe_interval()`] slots. The kept estimate is still the one with the
/// lowest round-trip time. The later bursts use the configured cadence.
/// This is a local choice: the outlet answers each probe as it comes.
pub const FIRST_PROBE_INTERVAL: Duration = Duration::from_millis(4);

/// The wait before the next burst when a burst found no first offset.
///
/// A burst without enough answers tries again after this time and not after
/// a full [`update_interval()`]. The wait keeps an outlet that does not answer
/// from a flood of probes.
pub const FIRST_RETRY: Duration = Duration::from_millis(250);

/// The published result of the most recent burst.
#[derive(Debug)]
pub struct ClockState {
    /// The offset, as `f64` bits. `u64::MAX` means no measurement yet.
    offset_bits: AtomicU64,
    /// The round-trip time of the kept estimate, as `f64` bits.
    uncertainty_bits: AtomicU64,
    /// Set once when a burst first publishes a value.
    have: AtomicBool,
    /// Set when the connection reset. The post-processor clears it.
    reset: AtomicBool,
    /// Tells the thread to stop.
    stop: AtomicBool,
    /// True while a probe thread runs. A wait for an offset ends when no
    /// thread runs, because no value can then arrive.
    alive: AtomicBool,
    /// How many bursts have published.
    updates: AtomicU64,
    /// The service port that the probes go to. A recovery can change it.
    target: Mutex<Option<SocketAddr>>,
    /// Grows by one for each change of the target. A burst that started
    /// before a change publishes nothing.
    epoch: AtomicU64,
    /// Wakes a caller of [`ClockState::wait`].
    signal: (Mutex<()>, Condvar),
}

const NO_VALUE: u64 = u64::MAX;

impl Default for ClockState {
    fn default() -> Self {
        ClockState {
            offset_bits: AtomicU64::new(NO_VALUE),
            uncertainty_bits: AtomicU64::new(NO_VALUE),
            have: AtomicBool::new(false),
            reset: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            alive: AtomicBool::new(false),
            updates: AtomicU64::new(0),
            target: Mutex::new(None),
            epoch: AtomicU64::new(0),
            signal: (Mutex::new(()), Condvar::new()),
        }
    }
}

impl ClockState {
    /// The most recent offset, or `None` before the first burst finishes.
    pub fn offset(&self) -> Option<f64> {
        match self.offset_bits.load(Ordering::SeqCst) {
            NO_VALUE => None,
            b => Some(f64::from_bits(b)),
        }
    }

    /// The round-trip time of the kept estimate. liblsl calls this the
    /// uncertainty (`src/time_receiver.cpp:204`).
    pub fn uncertainty(&self) -> Option<f64> {
        match self.uncertainty_bits.load(Ordering::SeqCst) {
            NO_VALUE => None,
            b => Some(f64::from_bits(b)),
        }
    }

    /// How many bursts have published a value.
    pub fn updates(&self) -> u64 {
        self.updates.load(Ordering::SeqCst)
    }

    /// True while a probe thread runs and can publish a value.
    pub fn is_running(&self) -> bool {
        self.alive.load(Ordering::SeqCst) && !self.stop.load(Ordering::SeqCst)
    }

    /// Wait until a first measurement arrives.
    ///
    /// The wait ends early when no probe thread runs, because no value can
    /// then arrive. liblsl ends its wait when the connection is lost
    /// (`src/time_receiver.cpp:59-61`).
    pub fn wait(&self, timeout: Duration) -> Option<f64> {
        let end = Instant::now() + timeout;
        let (lock, cvar) = &self.signal;
        let mut guard = lock.lock().unwrap();
        loop {
            if let Some(v) = self.offset() {
                return Some(v);
            }
            if !self.is_running() {
                return None;
            }
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return self.offset();
            }
            // A short slice guards against a lost wake-up.
            guard = cvar
                .wait_timeout(guard, left.min(Duration::from_millis(50)))
                .unwrap()
                .0;
        }
    }

    /// Record that the connection reset, so the post-processor clears its fit.
    pub fn mark_reset(&self) {
        self.reset.store(true, Ordering::SeqCst);
    }

    /// Forget the offset after the inlet attached to a new source.
    ///
    /// The old offset describes a process that has gone. The reset flag goes
    /// up only if an offset existed, as in liblsl
    /// (`src/time_receiver.cpp:212-218`).
    pub fn reset_offset(&self) {
        if self.offset().is_some() {
            self.reset.store(true, Ordering::SeqCst);
        }
        self.offset_bits.store(NO_VALUE, Ordering::SeqCst);
        self.uncertainty_bits.store(NO_VALUE, Ordering::SeqCst);
    }

    /// Send the next bursts to a different service port.
    ///
    /// A burst that is in progress publishes nothing. liblsl sets the new
    /// address in its recovery callback (`src/time_receiver.cpp:26-33`).
    pub fn retarget(&self, target: SocketAddr) {
        *self.target.lock().unwrap() = Some(target);
        self.epoch.fetch_add(1, Ordering::SeqCst);
    }

    /// Stop the probe thread.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        self.notify();
    }

    fn notify(&self) {
        let _guard = self.signal.0.lock().unwrap();
        self.signal.1.notify_all();
    }

    fn publish(&self, offset: f64, uncertainty: f64) {
        self.offset_bits.store(offset.to_bits(), Ordering::SeqCst);
        self.uncertainty_bits
            .store(uncertainty.to_bits(), Ordering::SeqCst);
        self.have.store(true, Ordering::SeqCst);
        self.updates.fetch_add(1, Ordering::SeqCst);
        self.notify();
    }
}

/// Clears the running flag when the probe thread ends, also on a panic.
struct AliveGuard(Arc<ClockState>);

impl Drop for AliveGuard {
    fn drop(&mut self) {
        self.0.alive.store(false, Ordering::SeqCst);
        self.0.notify();
    }
}

/// Open a probe socket of the same protocol as the target.
fn probe_socket(target: SocketAddr) -> Option<UdpSocket> {
    let any: std::net::IpAddr = if target.is_ipv6() {
        std::net::Ipv6Addr::UNSPECIFIED.into()
    } else {
        std::net::Ipv4Addr::UNSPECIFIED.into()
    };
    let sock = UdpSocket::bind((any, 0)).ok()?;
    let _ = sock.set_read_timeout(Some(Duration::from_millis(40)));
    Some(sock)
}

/// Read answers into `burst` until `end`, or until it holds `enough` answers.
///
/// Each read waits only for the time that is left. A fixed read timeout
/// would end the slot early, or hold a short slot open for too long.
fn collect(sock: &UdpSocket, burst: &mut Burst, end: Instant, enough: Option<usize>) {
    let mut buf = [0u8; 4096];
    loop {
        if enough.is_some_and(|n| burst.accepted() >= n) {
            return;
        }
        let left = end.saturating_duration_since(Instant::now());
        // A read timeout of zero is an error, so the slot ends here.
        if left < Duration::from_micros(100) {
            return;
        }
        let _ = sock.set_read_timeout(Some(left));
        match sock.recv_from(&mut buf) {
            Ok((n, _)) => {
                let t3 = crate::clock();
                if let Some(a) = timesync::parse_answer(&buf[..n]) {
                    burst.accept(&a, t3);
                }
            }
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            // An ICMP error from an earlier probe can show as a read error.
            // The slot then waits out its time with a short pause.
            Err(_) => std::thread::sleep(left.min(Duration::from_millis(5))),
        }
    }
}

/// Start a thread that keeps the offset up to date.
///
/// The thread sends one burst every [`update_interval()`]. A burst that collects
/// fewer than [`update_min_probes()`] answers publishes nothing, so a lost packet
/// leaves the previous value in place instead of replacing it with a guess.
///
/// [`ClockState::retarget`] moves the probes to a different service port.
pub fn start(target: SocketAddr, state: Arc<ClockState>, rng: Arc<Mutex<u64>>) {
    *state.target.lock().unwrap() = Some(target);
    // The flag goes up before the thread starts. A caller that waits at once
    // therefore sees a running thread.
    state.alive.store(true, Ordering::SeqCst);
    std::thread::spawn(move || {
        let _alive = AliveGuard(Arc::clone(&state));
        let mut sock = match probe_socket(target) {
            Some(s) => s,
            None => return,
        };
        let mut bound_v6 = target.is_ipv6();

        while !state.stop.load(Ordering::SeqCst) {
            let epoch = state.epoch.load(Ordering::SeqCst);
            let target = match *state.target.lock().unwrap() {
                Some(t) => t,
                None => return,
            };
            // A recovery can move the stream from IPv4 to IPv6. The socket
            // must then change protocol too (`src/time_receiver.cpp:30-32`).
            if target.is_ipv6() != bound_v6 {
                sock = match probe_socket(target) {
                    Some(s) => s,
                    None => return,
                };
                bound_v6 = target.is_ipv6();
            }

            // A new identifier for each burst, so a late answer from an earlier burst
            // is dropped. SPEC.md 3.4.
            let wave_id = {
                let mut r = rng.lock().unwrap();
                *r ^= *r << 13;
                *r ^= *r >> 7;
                *r ^= *r << 17;
                (*r as u32 as i32).abs()
            };
            let mut burst = Burst::new(wave_id);
            // Without an offset, the burst is a fast one. A reset or a new
            // target clears the offset, so a recovery also gets a fast burst.
            let fast = state.offset().is_none();
            let gap = if fast {
                FIRST_PROBE_INTERVAL.min(probe_interval())
            } else {
                probe_interval()
            };
            let burst_start = Instant::now();
            let burst_end = burst_start + gap * probe_count() as u32 + probe_max_rtt();

            for _ in 0..probe_count() {
                if state.stop.load(Ordering::SeqCst) {
                    return;
                }
                let t0 = crate::clock();
                let msg = timesync::build_probe(wave_id, t0);
                let _ = sock.send_to(&msg, target);

                // Collect whatever arrives before the next probe goes out.
                collect(&sock, &mut burst, Instant::now() + gap, None);
            }

            // Collect the late answers. A fast burst stops when each probe
            // has its answer.
            let all = if fast { Some(probe_count()) } else { None };
            collect(&sock, &mut burst, burst_end, all);

            if std::env::var_os("LSL_TRACE_CLOCK").is_some() {
                eprintln!(
                    "TRACE clock wave={wave_id} accepted={} offset={:?} rtt={:?}",
                    burst.accepted(),
                    burst.time_offset(),
                    burst.uncertainty()
                );
            }
            // A burst that started before a change of target measured the old
            // source. It publishes nothing.
            if burst.accepted() >= update_min_probes()
                && state.epoch.load(Ordering::SeqCst) == epoch
            {
                if let (Some(o), Some(u)) = (burst.time_offset(), burst.uncertainty()) {
                    state.publish(o, u);
                }
            }

            // Wait out the rest of the interval. Without an offset, try again
            // soon.
            let mut wake = burst_start + update_interval();
            if state.offset().is_none() {
                wake = wake.min(Instant::now() + FIRST_RETRY);
            }
            while Instant::now() < wake {
                if state.stop.load(Ordering::SeqCst) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    });
}

/// Feeds the post-processor from a live connection.
pub struct LiveSource {
    state: Arc<ClockState>,
    srate: f64,
    /// True after one wait for the first offset ran out. Later calls then do
    /// not wait again until a reset clears the offset.
    waited_out: bool,
    /// The most recent offset that this source gave.
    last: f64,
}

impl LiveSource {
    /// Build a source for a stream with the given nominal rate.
    pub fn new(state: Arc<ClockState>, srate: f64) -> Self {
        LiveSource {
            state,
            srate,
            waited_out: false,
            last: 0.0,
        }
    }
}

impl OffsetSource for LiveSource {
    fn correction(&mut self) -> f64 {
        // Before the first burst finishes there is no measurement. liblsl
        // waits up to 5 s for it (`src/stream_inlet_impl.h:44`,
        // `src/time_receiver.cpp:57-76`). Without the wait, the first
        // timestamps and the start of the jitter fit use no offset, and the
        // real offset arrives later as a step.
        if let Some(v) = self.state.offset() {
            self.waited_out = false;
            self.last = v;
            return v;
        }
        if !self.waited_out {
            if let Some(v) = self.state.wait(FIRST_OFFSET_TIMEOUT) {
                self.last = v;
                return v;
            }
            // liblsl raises `timeout_error` here. This source gives the last
            // value and does not wait again, so the samples continue to flow.
            self.waited_out = true;
            if std::env::var_os("LSL_TRACE_CLOCK").is_some() {
                eprintln!("TRACE clock: no offset arrived, the timestamps stay uncorrected");
            }
        }
        self.last
    }

    fn srate(&mut self) -> f64 {
        self.srate
    }

    fn was_reset(&mut self) -> bool {
        // A reset clears the offset. The next call to `correction` then waits
        // again, because a received value set `waited_out` to false.
        self.state.reset.swap(false, Ordering::SeqCst)
    }

    fn clock(&mut self) -> f64 {
        crate::clock()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_state_holds_no_measurement() {
        let s = ClockState::default();
        assert_eq!(s.offset(), None);
        assert_eq!(s.uncertainty(), None);
        assert_eq!(s.updates(), 0);
    }

    #[test]
    fn a_published_value_comes_back() {
        let s = ClockState::default();
        s.publish(-1.25, 0.004);
        assert_eq!(s.offset(), Some(-1.25));
        assert_eq!(s.uncertainty(), Some(0.004));
        assert_eq!(s.updates(), 1);
    }

    #[test]
    fn a_reset_reads_once() {
        let s = Arc::new(ClockState::default());
        let mut src = LiveSource::new(Arc::clone(&s), 100.0);
        assert!(!src.was_reset());
        s.mark_reset();
        assert!(src.was_reset(), "the first read reports the reset");
        assert!(!src.was_reset(), "a second read must not report it again");
    }

    #[test]
    fn with_no_probe_thread_the_correction_does_not_wait() {
        // No thread runs, so no value can arrive. The call returns at once.
        let s = Arc::new(ClockState::default());
        let mut src = LiveSource::new(s, 100.0);
        let t = Instant::now();
        assert_eq!(src.correction(), 0.0);
        assert!(t.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn the_correction_waits_for_the_first_offset() {
        let s = Arc::new(ClockState::default());
        // Act as a running probe thread that publishes after 200 ms.
        s.alive.store(true, Ordering::SeqCst);
        let writer = Arc::clone(&s);
        let h = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            writer.publish(-3.5, 0.001);
        });
        let mut src = LiveSource::new(Arc::clone(&s), 100.0);
        assert_eq!(
            src.correction(),
            -3.5,
            "the first value is the measured offset"
        );
        h.join().unwrap();
    }

    #[test]
    fn the_post_processor_never_sees_a_zero_offset_first() {
        use labstream_time::{flags, PostProcessor};
        let s = Arc::new(ClockState::default());
        s.alive.store(true, Ordering::SeqCst);
        let writer = Arc::clone(&s);
        let h = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            writer.publish(10.0, 0.001);
        });
        let mut src = LiveSource::new(Arc::clone(&s), 100.0);
        let mut post = PostProcessor::new();
        post.set_options(flags::CLOCKSYNC);
        assert_eq!(post.process(1.0, &mut src), 11.0);
        h.join().unwrap();
    }

    #[test]
    fn a_stopped_thread_ends_the_wait() {
        let s = Arc::new(ClockState::default());
        s.alive.store(true, Ordering::SeqCst);
        let stopper = Arc::clone(&s);
        let h = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            stopper.stop();
        });
        let t = Instant::now();
        assert_eq!(s.wait(FIRST_OFFSET_TIMEOUT), None);
        assert!(t.elapsed() < Duration::from_secs(2));
        h.join().unwrap();
    }

    #[test]
    fn a_reset_offset_sets_the_flag_only_after_a_value() {
        let s = ClockState::default();
        s.reset_offset();
        assert!(!s.reset.load(Ordering::SeqCst), "no value, so no reset");
        s.publish(1.0, 0.001);
        s.reset_offset();
        assert!(s.reset.load(Ordering::SeqCst));
        assert_eq!(s.offset(), None);
        assert_eq!(s.uncertainty(), None);
    }

    #[test]
    fn a_new_source_needs_a_new_wait() {
        let s = Arc::new(ClockState::default());
        s.alive.store(true, Ordering::SeqCst);
        s.publish(2.0, 0.001);
        let mut src = LiveSource::new(Arc::clone(&s), 100.0);
        assert_eq!(src.correction(), 2.0);
        s.reset_offset();
        let writer = Arc::clone(&s);
        let h = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            writer.publish(7.0, 0.001);
        });
        assert!(src.was_reset());
        assert_eq!(src.correction(), 7.0, "the value of the new source");
        h.join().unwrap();
    }
}
