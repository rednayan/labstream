//! Same-machine shared-memory transport (`shm/1`, `docs/extensions.md`,
//! section `shm`, `docs/roadmap.md`, step 9).
//!
//! # Ring algorithm
//!
//! Each connected `shm/1` consumer gets a lock-free single-producer
//! single-consumer (SPSC) ring buffer backed by an owner-only (`0600` on
//! Unix, `Local\` session namespace on Windows) shared-memory segment.
//!
//! Every slot starts with an `AtomicU64` seqlock version:
//! - Before writing slot `w % capacity`, the writer stores `2 * w + 1` (odd).
//! - After writing `sample_seq`, `timestamp_bits`, and raw native-endian
//!   channel bytes, the writer stores `2 * w + 2` (even) with `Release`
//!   ordering, then advances `write_idx` to `w + 1`.
//! - The reader checks `v1 == 2 * r + 2`, copies the slot out, fences, and
//!   re-checks `v2 == v1`. If the writer lapped the ring (`write_idx - r >
//!   capacity`), the reader advances `r = write_idx - capacity` and counts the
//!   skipped entries as local drops (matching LSL SPEC 8.7 drop-oldest).
//!
//! # Platform behavior
//!
//! - **Linux:** `shm_open(O_CREAT | O_EXCL | O_RDWR, 0o600)`, `fchmod(0o600)`,
//!   `ftruncate`, and `mmap(MAP_SHARED)`. Once the inlet confirms `shm-ok`, the
//!   outlet calls `shm_unlink` immediately so the segment name leaves `/dev/shm`
//!   while the kernel keeps the mapping alive until both processes exit (zero
//!   leaks even on `SIGKILL`). Cross-process wakeup uses `SYS_futex`
//!   (`FUTEX_WAIT` / `FUTEX_WAKE` without `FUTEX_PRIVATE_FLAG`).
//! - **macOS and other Unix:** `shm_open(0o600)` + `ftruncate` + `mmap` +
//!   `shm_unlink`. Wakeup uses a short spin followed by a 50 us sleep backoff.
//! - **Windows:** `CreateFileMappingW` / `OpenFileMappingW` in the session-only
//!   `Local\ls-shm-<id>` namespace + `MapViewOfFile`, paired with a named
//!   auto-reset `CreateEventW` / `OpenEventW` (`Local\ls-ev-<id>`) for
//!   zero-busy-wait cross-process wakeup.

#![allow(unsafe_code)]

use labstream_wire::{Format, PackedSample, PackedValues};
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{fence, AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const SHM_MAGIC: u64 = 0x4C53_5348_4D56_3031; // "LSSHMV01"
const SHM_VERSION: u32 = 1;
/// Upper memory bound per consumer SHM segment (8 MiB).
const MAX_SHM_BYTES: usize = 8 * 1024 * 1024;
/// Fixed per-slot header size: `ver (8) + sample_seq (8) + timestamp_bits (8)`.
const SLOT_HEADER_BYTES: usize = 24;

/// Metadata returned with each popped SHM sample.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ShmSampleMeta {
    /// Outlet-assigned `seq/1` sample number.
    pub sample_seq: u64,
    /// Full sample timestamp.
    pub timestamp: f64,
    /// Samples overwritten in the ring because the reader fell behind.
    pub lapped_drops: u64,
}

/// Shared-memory header laid out on three distinct 64-byte cache lines to
/// avoid false sharing between writer and reader cores.
#[repr(C, align(64))]
struct ShmHeader {
    // Cache line 0: immutable segment metadata + random handshake token.
    magic: u64,
    version: u32,
    format: u32,
    channels: u32,
    payload_bytes: u32,
    slot_stride: u32,
    capacity: u32,
    token: u64,
    _pad0: [u8; 24],

    // Cache line 1: writer-owned hot atomics.
    write_idx: AtomicU64,
    closed: AtomicU32,
    wake_word: AtomicU32,
    _pad1: [u8; 48],

    // Cache line 2: reader-owned hot atomics.
    read_idx: AtomicU64,
    waiters: AtomicU32,
    _pad2: [u8; 52],
}

const HEADER_SIZE: usize = std::mem::size_of::<ShmHeader>();

fn slot_stride_for(payload_bytes: usize) -> usize {
    (SLOT_HEADER_BYTES + payload_bytes + 7) & !7
}

fn ring_capacity_for(requested: usize, payload_bytes: usize) -> usize {
    let stride = slot_stride_for(payload_bytes).max(SLOT_HEADER_BYTES);
    let max_slots = ((MAX_SHM_BYTES.saturating_sub(HEADER_SIZE)) / stride).max(1);
    requested.clamp(1, max_slots)
}

fn format_to_u32(f: Format) -> u32 {
    match f {
        Format::Float32 => 1,
        Format::Double64 => 2,
        Format::String => 3,
        Format::Int32 => 4,
        Format::Int16 => 5,
        Format::Int8 => 6,
        Format::Int64 => 7,
        Format::Undefined => 0,
    }
}

/// True when the current OS implements `shm/1` shared-memory mappings.
pub(crate) fn supported() -> bool {
    cfg!(any(unix, windows))
}

/// Write `shm-ok <token_hex>\n` on TCP after opening and reading the segment.
pub(crate) fn write_shm_ok(stream: &mut TcpStream, token: u64) -> io::Result<()> {
    let line = format!("shm-ok {token:016x}\n");
    stream.write_all(line.as_bytes())?;
    stream.flush()
}

/// Write `shm-no\n` on TCP when the inlet cannot open the segment.
pub(crate) fn write_shm_no(stream: &mut TcpStream) -> io::Result<()> {
    stream.write_all(b"shm-no\n")?;
    stream.flush()
}

/// Read the inlet's `shm-ok <token>` or `shm-no` line with a timeout
/// (`docs/extensions.md`, section `shm`, step 5).
///
/// Returns `true` only when the line arrives inside `timeout` and its 64-bit
/// token matches `expected_token`.
pub(crate) fn read_shm_reply(
    stream: &mut TcpStream,
    expected_token: u64,
    timeout: Duration,
) -> bool {
    let prev = stream.read_timeout().ok().flatten();
    let deadline = Instant::now() + timeout;
    let mut line = Vec::with_capacity(64);
    let mut byte = [0u8; 1];
    let ok = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break false;
        }
        let _ = stream.set_read_timeout(Some(left.min(Duration::from_millis(100))));
        match stream.read(&mut byte) {
            Ok(0) => break false,
            Ok(_) => {
                if byte[0] == b'\n' {
                    break parse_shm_ok(&line) == Some(expected_token);
                }
                if byte[0] != b'\r' {
                    line.push(byte[0]);
                }
                if line.len() > 64 {
                    break false;
                }
            }
            Err(ref e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(_) => break false,
        }
    };
    let _ = stream.set_read_timeout(prev);
    ok
}

fn parse_shm_ok(line: &[u8]) -> Option<u64> {
    let s = std::str::from_utf8(line).ok()?.trim();
    let hex = s.strip_prefix("shm-ok ")?.trim();
    if hex.len() != 16 || !hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    u64::from_str_radix(hex, 16).ok()
}

// ---------------------------------------------------------------------------
// OS mapping backend
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod os {
    use super::*;
    use std::ffi::CString;

    pub(super) struct Mapping {
        pub(super) ptr: *mut u8,
        pub(super) len: usize,
        name: Option<CString>,
        unlinked: AtomicBool,
    }

    // SAFETY: The mapped region lives for the lifetime of `Mapping` and all
    // concurrent accesses use atomics or disjoint seqlock-guarded slots.
    unsafe impl Send for Mapping {}
    unsafe impl Sync for Mapping {}

    fn shm_name(id: u64) -> CString {
        // 24 chars + NUL = 25 bytes, safely below macOS PSHMNAMLEN (31).
        CString::new(format!("/ls-shm-{id:016x}")).expect("hex has no interior NUL")
    }

    pub(super) fn create(id: u64, len: usize) -> io::Result<Mapping> {
        let name = shm_name(id);
        // SAFETY: `name` is a valid NUL-terminated C string. Mode `0o600`
        // restricts access to the owning user (`docs/extensions.md:1104`).
        let fd = unsafe {
            libc::shm_open(
                name.as_ptr(),
                libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
                0o600,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is a valid open descriptor owned by this function.
        unsafe {
            let _ = libc::fchmod(fd, 0o600);
        }
        if unsafe { libc::ftruncate(fd, len as libc::off_t) } != 0 {
            let err = io::Error::last_os_error();
            unsafe {
                libc::close(fd);
                libc::shm_unlink(name.as_ptr());
            }
            return Err(err);
        }
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        unsafe {
            libc::close(fd);
        }
        if ptr == libc::MAP_FAILED {
            let err = io::Error::last_os_error();
            unsafe {
                libc::shm_unlink(name.as_ptr());
            }
            return Err(err);
        }
        // Zero-initialize the entire mapping so header atomics and slots start clean.
        unsafe {
            std::ptr::write_bytes(ptr as *mut u8, 0, len);
        }
        Ok(Mapping {
            ptr: ptr as *mut u8,
            len,
            name: Some(name),
            unlinked: AtomicBool::new(false),
        })
    }

    pub(super) fn open(id: u64) -> io::Result<Mapping> {
        let name = shm_name(id);
        // SAFETY: `name` is a valid NUL-terminated C string.
        let fd = unsafe { libc::shm_open(name.as_ptr(), libc::O_RDWR, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd, &mut st) } != 0 {
            let err = io::Error::last_os_error();
            unsafe {
                libc::close(fd);
            }
            return Err(err);
        }
        let len = st.st_size as usize;
        if len < HEADER_SIZE {
            unsafe {
                libc::close(fd);
            }
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "shared memory segment smaller than header",
            ));
        }
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        unsafe {
            libc::close(fd);
        }
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Mapping {
            ptr: ptr as *mut u8,
            len,
            name: None,
            unlinked: AtomicBool::new(true),
        })
    }

    impl Mapping {
        pub(super) fn unlink_name(&self) {
            if !self.unlinked.swap(true, Ordering::AcqRel) {
                if let Some(ref name) = self.name {
                    // SAFETY: `name` is a valid C string created by this writer.
                    unsafe {
                        libc::shm_unlink(name.as_ptr());
                    }
                }
            }
        }

        pub(super) fn wake(&self, wake_word: &AtomicU32) {
            wake_word.fetch_add(1, Ordering::Release);
            #[cfg(target_os = "linux")]
            {
                // SAFETY: `wake_word` resides in the live `MAP_SHARED` mapping.
                // Omitting `FUTEX_PRIVATE_FLAG` enables cross-process wakeups.
                unsafe {
                    libc::syscall(
                        libc::SYS_futex,
                        wake_word as *const AtomicU32,
                        libc::FUTEX_WAKE,
                        1i32,
                        std::ptr::null::<libc::timespec>(),
                        std::ptr::null::<u32>(),
                        0u32,
                    );
                }
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = wake_word;
            }
        }

        pub(super) fn wait(&self, wake_word: &AtomicU32, expected: u32, timeout: Duration) {
            #[cfg(target_os = "linux")]
            {
                let ts = libc::timespec {
                    tv_sec: timeout.as_secs() as libc::time_t,
                    tv_nsec: timeout.subsec_nanos() as libc::c_long,
                };
                // SAFETY: `wake_word` resides in the live `MAP_SHARED` mapping
                // and `ts` is a valid stack-allocated relative timeout.
                unsafe {
                    libc::syscall(
                        libc::SYS_futex,
                        wake_word as *const AtomicU32,
                        libc::FUTEX_WAIT,
                        expected,
                        &ts as *const libc::timespec,
                        std::ptr::null::<u32>(),
                        0u32,
                    );
                }
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = (wake_word, expected);
                std::thread::sleep(timeout.min(Duration::from_micros(50)));
            }
        }
    }

    impl Drop for Mapping {
        fn drop(&mut self) {
            self.unlink_name();
            // SAFETY: `ptr` and `len` came from a successful `mmap`.
            unsafe {
                libc::munmap(self.ptr as *mut libc::c_void, self.len);
            }
        }
    }
}

#[cfg(windows)]
mod os {
    use super::*;
    use std::ffi::c_void;

    const INVALID_HANDLE_VALUE: *mut c_void = -1isize as *mut c_void;
    const PAGE_READWRITE: u32 = 0x04;
    const FILE_MAP_ALL_ACCESS: u32 = 0x000F_001F;
    const EVENT_MODIFY_STATE: u32 = 0x0002;
    const SYNCHRONIZE: u32 = 0x0010_0000;

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateFileMappingW(
            hfile: *mut c_void,
            attrs: *const c_void,
            protect: u32,
            max_high: u32,
            max_low: u32,
            name: *const u16,
        ) -> *mut c_void;
        fn OpenFileMappingW(access: u32, inherit: i32, name: *const u16) -> *mut c_void;
        fn MapViewOfFile(
            mapping: *mut c_void,
            access: u32,
            off_high: u32,
            off_low: u32,
            bytes: usize,
        ) -> *mut c_void;
        fn UnmapViewOfFile(addr: *const c_void) -> i32;
        fn CloseHandle(handle: *mut c_void) -> i32;
        fn CreateEventW(
            attrs: *const c_void,
            manual_reset: i32,
            initial_state: i32,
            name: *const u16,
        ) -> *mut c_void;
        fn OpenEventW(access: u32, inherit: i32, name: *const u16) -> *mut c_void;
        fn SetEvent(handle: *mut c_void) -> i32;
        fn WaitForSingleObject(handle: *mut c_void, millis: u32) -> u32;
    }

    pub(super) struct Mapping {
        pub(super) ptr: *mut u8,
        pub(super) len: usize,
        handle: *mut c_void,
        event: *mut c_void,
    }

    // SAFETY: Handles and mapped view remain valid for the lifetime of `Mapping`.
    unsafe impl Send for Mapping {}
    unsafe impl Sync for Mapping {}

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub(super) fn create(id: u64, len: usize) -> io::Result<Mapping> {
        let name = wide(&format!("Local\\ls-shm-{id:016x}"));
        let ev_name = wide(&format!("Local\\ls-ev-{id:016x}"));
        let max_high = ((len as u64) >> 32) as u32;
        let max_low = (len as u64) as u32;
        let handle = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE,
                max_high,
                max_low,
                name.as_ptr(),
            )
        };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let ptr = unsafe { MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, len) };
        if ptr.is_null() {
            let err = io::Error::last_os_error();
            unsafe {
                CloseHandle(handle);
            }
            return Err(err);
        }
        let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, ev_name.as_ptr()) };
        unsafe {
            std::ptr::write_bytes(ptr as *mut u8, 0, len);
        }
        Ok(Mapping {
            ptr: ptr as *mut u8,
            len,
            handle,
            event,
        })
    }

    pub(super) fn open(id: u64) -> io::Result<Mapping> {
        let name = wide(&format!("Local\\ls-shm-{id:016x}"));
        let ev_name = wide(&format!("Local\\ls-ev-{id:016x}"));
        let handle = unsafe { OpenFileMappingW(FILE_MAP_ALL_ACCESS, 0, name.as_ptr()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        // Map header first to read exact segment length.
        let hdr_ptr = unsafe { MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, HEADER_SIZE) };
        if hdr_ptr.is_null() {
            let err = io::Error::last_os_error();
            unsafe {
                CloseHandle(handle);
            }
            return Err(err);
        }
        let hdr = unsafe { &*(hdr_ptr as *const ShmHeader) };
        let len = HEADER_SIZE + (hdr.capacity as usize).saturating_mul(hdr.slot_stride as usize);
        unsafe {
            UnmapViewOfFile(hdr_ptr);
        }
        if len < HEADER_SIZE || len > MAX_SHM_BYTES + HEADER_SIZE {
            unsafe {
                CloseHandle(handle);
            }
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid shared memory length",
            ));
        }
        let ptr = unsafe { MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, len) };
        if ptr.is_null() {
            let err = io::Error::last_os_error();
            unsafe {
                CloseHandle(handle);
            }
            return Err(err);
        }
        let event = unsafe { OpenEventW(SYNCHRONIZE | EVENT_MODIFY_STATE, 0, ev_name.as_ptr()) };
        Ok(Mapping {
            ptr: ptr as *mut u8,
            len,
            handle,
            event,
        })
    }

    impl Mapping {
        pub(super) fn unlink_name(&self) {}

        pub(super) fn wake(&self, wake_word: &AtomicU32) {
            wake_word.fetch_add(1, Ordering::Release);
            if !self.event.is_null() {
                unsafe {
                    SetEvent(self.event);
                }
            }
        }

        pub(super) fn wait(&self, _wake_word: &AtomicU32, _expected: u32, timeout: Duration) {
            if !self.event.is_null() {
                let ms = timeout.as_millis().min(u32::MAX as u128 - 1) as u32;
                unsafe {
                    WaitForSingleObject(self.event, ms.max(1));
                }
            } else {
                std::thread::sleep(timeout.min(Duration::from_micros(50)));
            }
        }
    }

    impl Drop for Mapping {
        fn drop(&mut self) {
            unsafe {
                UnmapViewOfFile(self.ptr as *const c_void);
                if !self.event.is_null() {
                    CloseHandle(self.event);
                }
                CloseHandle(self.handle);
            }
        }
    }
}

#[cfg(not(any(unix, windows)))]
mod os {
    use super::*;

    pub(super) struct Mapping {
        pub(super) ptr: *mut u8,
        pub(super) len: usize,
    }
    unsafe impl Send for Mapping {}
    unsafe impl Sync for Mapping {}

    pub(super) fn create(_id: u64, _len: usize) -> io::Result<Mapping> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "shm not supported on this platform",
        ))
    }

    pub(super) fn open(_id: u64) -> io::Result<Mapping> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "shm not supported on this platform",
        ))
    }

    impl Mapping {
        pub(super) fn unlink_name(&self) {}
        pub(super) fn wake(&self, _wake_word: &AtomicU32) {}
        pub(super) fn wait(&self, _wake_word: &AtomicU32, _expected: u32, _timeout: Duration) {}
    }
}

// ---------------------------------------------------------------------------
// Writer (outlet side)
// ---------------------------------------------------------------------------

/// Producer handle to one consumer's `shm/1` shared-memory ring buffer.
pub(crate) struct ShmWriter {
    mapping: os::Mapping,
    id: u64,
    token: u64,
    format: Format,
    #[cfg(test)]
    channels: usize,
    payload_bytes: usize,
    slot_stride: usize,
    capacity: usize,
    requested_capacity: usize,
    outlet_dropped: AtomicU64,
    /// Scratch buffer for projecting channel subsets without allocation.
    proj_scratch: std::sync::Mutex<Vec<u8>>,
}

impl std::fmt::Debug for ShmWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShmWriter")
            .field("id", &format_args!("{:016x}", self.id))
            .field("capacity", &self.capacity)
            .finish()
    }
}

impl ShmWriter {
    /// Allocate a new owner-only shared-memory ring segment for one consumer.
    pub(crate) fn create(
        format: Format,
        channels: usize,
        requested_capacity: usize,
    ) -> io::Result<Self> {
        let width = format.width().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "shm only supports fixed-width numeric formats",
            )
        })?;
        let payload_bytes = channels.checked_mul(width).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "channel payload size overflow")
        })?;
        let capacity = ring_capacity_for(requested_capacity, payload_bytes);
        let slot_stride = slot_stride_for(payload_bytes);
        let total_len =
            HEADER_SIZE
                .checked_add(capacity.checked_mul(slot_stride).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "shm size overflow")
                })?)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "shm size overflow"))?;

        let rand = crate::random_bytes();
        let id = u64::from_le_bytes([
            rand[0], rand[1], rand[2], rand[3], rand[4], rand[5], rand[6], rand[7],
        ]);
        let token = u64::from_le_bytes([
            rand[8], rand[9], rand[10], rand[11], rand[12], rand[13], rand[14], rand[15],
        ]);

        let mapping = os::create(id, total_len)?;
        // SAFETY: `mapping.ptr` is 64-byte page-aligned and holds `HEADER_SIZE`.
        let hdr = unsafe { &mut *(mapping.ptr as *mut ShmHeader) };
        hdr.magic = SHM_MAGIC;
        hdr.version = SHM_VERSION;
        hdr.format = format_to_u32(format);
        hdr.channels = channels as u32;
        hdr.payload_bytes = payload_bytes as u32;
        hdr.slot_stride = slot_stride as u32;
        hdr.capacity = capacity as u32;
        hdr.token = token;
        fence(Ordering::Release);

        Ok(ShmWriter {
            mapping,
            id,
            token,
            format,
            #[cfg(test)]
            channels,
            payload_bytes,
            slot_stride,
            capacity,
            requested_capacity: requested_capacity.max(1),
            outlet_dropped: AtomicU64::new(0),
            proj_scratch: std::sync::Mutex::new(Vec::new()),
        })
    }

    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    pub(crate) fn token(&self) -> u64 {
        self.token
    }

    pub(crate) fn requested_capacity(&self) -> usize {
        self.requested_capacity
    }

    /// Unlink the filesystem name on Unix once the inlet has opened the
    /// mapping (`shm-ok`), eliminating `/dev/shm` leaks even on `SIGKILL`.
    pub(crate) fn unlink_name(&self) {
        self.mapping.unlink_name();
    }

    #[inline]
    fn header(&self) -> &ShmHeader {
        // SAFETY: `mapping.ptr` stays valid and aligned for `self`'s lifetime.
        unsafe { &*(self.mapping.ptr as *const ShmHeader) }
    }

    #[inline]
    fn slot_ptr(&self, idx: u64) -> *mut u8 {
        let slot = (idx as usize) % self.capacity;
        // SAFETY: `slot < capacity` and `HEADER_SIZE + capacity * slot_stride <= mapping.len`.
        unsafe { self.mapping.ptr.add(HEADER_SIZE + slot * self.slot_stride) }
    }

    #[inline]
    fn write_slot(&self, w: u64, sample_seq: u64, timestamp: f64, bytes: &[u8]) {
        let ptr = self.slot_ptr(w);
        // SAFETY: Each slot is 8-byte aligned and holds `SLOT_HEADER_BYTES + payload_bytes`.
        unsafe {
            let ver = &*(ptr as *const AtomicU64);
            ver.store(w.wrapping_mul(2).wrapping_add(1), Ordering::Release);
            std::ptr::write(ptr.add(8) as *mut u64, sample_seq);
            std::ptr::write(ptr.add(16) as *mut u64, timestamp.to_bits());
            let copy_len = bytes.len().min(self.payload_bytes);
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.add(SLOT_HEADER_BYTES), copy_len);
            if copy_len < self.payload_bytes {
                std::ptr::write_bytes(
                    ptr.add(SLOT_HEADER_BYTES + copy_len),
                    0,
                    self.payload_bytes - copy_len,
                );
            }
            ver.store(w.wrapping_mul(2).wrapping_add(2), Ordering::Release);
        }
    }

    #[inline]
    fn wake_if_needed(&self) {
        let hdr = self.header();
        // Pairs with reader's `SeqCst` increment of `waiters` before re-checking `write_idx`.
        fence(Ordering::SeqCst);
        if hdr.waiters.load(Ordering::Relaxed) != 0 {
            self.mapping.wake(&hdr.wake_word);
        }
    }

    /// Push one numeric sample directly from raw native-endian bytes.
    pub(crate) fn push_raw_one(
        &self,
        sample_seq: u64,
        timestamp: f64,
        raw_bytes: &[u8],
        channels: Option<&[u16]>,
    ) {
        let hdr = self.header();
        if hdr.closed.load(Ordering::Relaxed) != 0 {
            return;
        }
        let w = hdr.write_idx.load(Ordering::Relaxed);
        match channels {
            Some(chs) if !chs.is_empty() => {
                let width = self.format.width().unwrap_or(1);
                let mut scratch = self.proj_scratch.lock().unwrap();
                scratch.clear();
                scratch.reserve(chs.len() * width);
                for &ch in chs {
                    let start = (ch as usize) * width;
                    let end = start + width;
                    if end <= raw_bytes.len() {
                        scratch.extend_from_slice(&raw_bytes[start..end]);
                    } else {
                        let new_len = scratch.len() + width;
                        scratch.resize(new_len, 0);
                    }
                }
                self.write_slot(w, sample_seq, timestamp, &scratch);
            }
            _ => {
                self.write_slot(w, sample_seq, timestamp, raw_bytes);
            }
        }
        hdr.write_idx.store(w.wrapping_add(1), Ordering::Release);
        self.wake_if_needed();
    }

    /// Push a batch of packed samples into the SHM ring with one wakeup.
    ///
    /// If `samples.len() > self.capacity`, only the newest `self.capacity`
    /// samples are written; the skipped prefix is recorded in
    /// `self.outlet_dropped` and surfaces on the inlet as a `seq/1` `Lost` gap.
    pub(crate) fn push_batch(
        &self,
        first_seq: u64,
        samples: &[PackedSample],
        full_timestamps: &[f64],
        channels: Option<&[u16]>,
    ) {
        if samples.is_empty() {
            return;
        }
        let hdr = self.header();
        if hdr.closed.load(Ordering::Relaxed) != 0 {
            return;
        }
        let skip = samples.len().saturating_sub(self.capacity);
        if skip > 0 {
            self.outlet_dropped
                .fetch_add(skip as u64, Ordering::Relaxed);
        }
        let mut w = hdr.write_idx.load(Ordering::Relaxed);
        let width = self.format.width().unwrap_or(1);
        for idx in skip..samples.len() {
            let s = &samples[idx];
            let seq = first_seq.wrapping_add(idx as u64);
            let ts = full_timestamps[idx];
            if let PackedValues::Numeric { format, bytes } = &s.values {
                if *format != self.format {
                    continue;
                }
                match channels {
                    Some(chs) if !chs.is_empty() => {
                        let mut scratch = self.proj_scratch.lock().unwrap();
                        scratch.clear();
                        scratch.reserve(chs.len() * width);
                        for &ch in chs {
                            let start = (ch as usize) * width;
                            let end = start + width;
                            if end <= bytes.len() {
                                scratch.extend_from_slice(&bytes[start..end]);
                            } else {
                                let new_len = scratch.len() + width;
                                scratch.resize(new_len, 0);
                            }
                        }
                        self.write_slot(w, seq, ts, &scratch);
                    }
                    _ => {
                        self.write_slot(w, seq, ts, bytes);
                    }
                }
                w = w.wrapping_add(1);
            }
        }
        hdr.write_idx.store(w, Ordering::Release);
        self.wake_if_needed();
    }

    /// Push a flat contiguous numeric chunk directly from raw native-endian
    /// bytes with one wakeup (zero intermediate `Arc` allocations).
    pub(crate) fn push_chunk_raw(
        &self,
        first_seq: u64,
        raw: &[u8],
        row_bytes: usize,
        full_timestamps: &[f64],
        channels: Option<&[u16]>,
    ) {
        let n = full_timestamps.len();
        if n == 0 || row_bytes == 0 || raw.len() < n * row_bytes {
            return;
        }
        let hdr = self.header();
        if hdr.closed.load(Ordering::Relaxed) != 0 {
            return;
        }
        let skip = n.saturating_sub(self.capacity);
        if skip > 0 {
            self.outlet_dropped
                .fetch_add(skip as u64, Ordering::Relaxed);
        }
        let mut w = hdr.write_idx.load(Ordering::Relaxed);
        let width = self.format.width().unwrap_or(1);
        for (idx, &ts) in full_timestamps.iter().enumerate().skip(skip) {
            let row = &raw[idx * row_bytes..(idx + 1) * row_bytes];
            let seq = first_seq.wrapping_add(idx as u64);
            match channels {
                Some(chs) if !chs.is_empty() => {
                    let mut scratch = self.proj_scratch.lock().unwrap();
                    scratch.clear();
                    scratch.reserve(chs.len() * width);
                    for &ch in chs {
                        let start = (ch as usize) * width;
                        let end = start + width;
                        if end <= row.len() {
                            scratch.extend_from_slice(&row[start..end]);
                        } else {
                            let new_len = scratch.len() + width;
                            scratch.resize(new_len, 0);
                        }
                    }
                    self.write_slot(w, seq, ts, &scratch);
                }
                _ => self.write_slot(w, seq, ts, row),
            }
            w = w.wrapping_add(1);
        }
        hdr.write_idx.store(w, Ordering::Release);
        self.wake_if_needed();
    }

    /// Mark the segment closed and wake any waiting reader.
    pub(crate) fn close(&self) {
        let hdr = self.header();
        hdr.closed.store(1, Ordering::Release);
        fence(Ordering::SeqCst);
        self.mapping.wake(&hdr.wake_word);
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.header().closed.load(Ordering::Acquire) != 0
    }

    /// Current unread sample count in the ring (capped at `capacity`).
    pub(crate) fn fill(&self) -> usize {
        let hdr = self.header();
        let w = hdr.write_idx.load(Ordering::Acquire);
        let r = hdr.read_idx.load(Ordering::Acquire);
        (w.saturating_sub(r) as usize).min(self.capacity)
    }

    /// Samples dropped by the outlet when a batch exceeded the ring capacity.
    pub(crate) fn outlet_dropped(&self) -> u64 {
        self.outlet_dropped.load(Ordering::Relaxed)
    }

    /// Output channels configured in this segment.
    #[cfg(test)]
    pub(crate) fn channels(&self) -> usize {
        self.channels
    }
}

impl Drop for ShmWriter {
    fn drop(&mut self) {
        self.close();
    }
}

// ---------------------------------------------------------------------------
// Reader (inlet side)
// ---------------------------------------------------------------------------

/// Consumer handle to one `shm/1` shared-memory ring buffer.
pub(crate) struct ShmReader {
    mapping: os::Mapping,
    format: Format,
    channels: usize,
    payload_bytes: usize,
    slot_stride: usize,
    capacity: usize,
    token: u64,
    /// Local read cursor (mirror of `header.read_idx`, avoids reading back).
    read_idx: AtomicU64,
    /// Cumulative drops observed from writer lapping this reader.
    dropped: AtomicU64,
    /// Local close flag set by `Inlet::drop` or TCP EOF watcher.
    local_closed: AtomicBool,
    /// When true (during `recover == true` TCP disconnect), `try_pop_into`
    /// stops returning buffered samples so `resume/1` replays from exact `next_expected`
    /// with zero duplicates.
    discard_closed: AtomicBool,
    /// `1` while the single consumer is inside `try_pop_into`.
    in_pop: AtomicU32,
    /// Exact next `seq/1` sample number after the last popped sample (`u64::MAX` = none).
    next_expected: AtomicU64,
    /// Monotonic timestamp (ns) of the last wait-wakeup arrival, for predictive
    /// pre-wake on regular high-rate streams.
    last_wake_ns: AtomicU64,
    /// Exponential moving average of inter-wakeup arrival period (ns).
    ema_period_ns: AtomicU64,
    /// Consecutive arrivals matching `ema_period_ns` within +/- 25%.
    steady_count: AtomicU32,
}

#[inline]
fn mono_ns() -> u64 {
    #[cfg(unix)]
    {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `ts` is a valid stack-allocated `timespec`.
        unsafe {
            libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
        }
        (ts.tv_sec as u64)
            .wrapping_mul(1_000_000_000)
            .wrapping_add(ts.tv_nsec as u64)
    }
    #[cfg(not(unix))]
    {
        use std::sync::OnceLock;
        static ORIGIN: OnceLock<Instant> = OnceLock::new();
        ORIGIN.get_or_init(Instant::now).elapsed().as_nanos() as u64
    }
}

#[cfg(target_os = "linux")]
fn set_low_timer_slack() {
    thread_local! {
        static SET: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    SET.with(|c| {
        if !c.get() {
            c.set(true);
            // SAFETY: `PR_SET_TIMERSLACK` takes a single `unsigned long`
            // nanosecond slack value and is permitted for unprivileged threads.
            unsafe {
                libc::prctl(libc::PR_SET_TIMERSLACK, 5_000usize, 0, 0, 0);
            }
        }
    });
}

impl std::fmt::Debug for ShmReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShmReader")
            .field("capacity", &self.capacity)
            .field("channels", &self.channels)
            .finish()
    }
}

impl ShmReader {
    /// Open and validate an existing shared-memory segment by its `shm/1` ID.
    pub(crate) fn open(
        id: u64,
        expected_format: Format,
        expected_channels: usize,
    ) -> io::Result<Self> {
        let mapping = os::open(id)?;
        // SAFETY: `os::open` verifies `mapping.len >= HEADER_SIZE`.
        let hdr = unsafe { &*(mapping.ptr as *const ShmHeader) };
        fence(Ordering::Acquire);

        if hdr.magic != SHM_MAGIC || hdr.version != SHM_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid shm header magic or version",
            ));
        }
        let width = expected_format
            .width()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "non-numeric shm format"))?;
        let expected_payload = expected_channels * width;
        if hdr.format != format_to_u32(expected_format)
            || hdr.channels as usize != expected_channels
            || hdr.payload_bytes as usize != expected_payload
            || hdr.capacity == 0
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "shm segment metadata does not match stream",
            ));
        }
        let capacity = hdr.capacity as usize;
        let slot_stride = hdr.slot_stride as usize;
        let needed = HEADER_SIZE.saturating_add(capacity.saturating_mul(slot_stride));
        if needed > mapping.len || slot_stride < SLOT_HEADER_BYTES + expected_payload {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "shm segment truncated",
            ));
        }

        Ok(ShmReader {
            token: hdr.token,
            mapping,
            format: expected_format,
            channels: expected_channels,
            payload_bytes: expected_payload,
            slot_stride,
            capacity,
            read_idx: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            local_closed: AtomicBool::new(false),
            discard_closed: AtomicBool::new(false),
            in_pop: AtomicU32::new(0),
            next_expected: AtomicU64::new(u64::MAX),
            last_wake_ns: AtomicU64::new(0),
            ema_period_ns: AtomicU64::new(0),
            steady_count: AtomicU32::new(0),
        })
    }

    pub(crate) fn token(&self) -> u64 {
        self.token
    }

    pub(crate) fn format(&self) -> Format {
        self.format
    }

    #[cfg(test)]
    pub(crate) fn capacity(&self) -> usize {
        self.capacity
    }

    #[inline]
    fn header(&self) -> &ShmHeader {
        // SAFETY: `mapping.ptr` stays valid for the lifetime of `self`.
        unsafe { &*(self.mapping.ptr as *const ShmHeader) }
    }

    #[inline]
    fn slot_ptr(&self, idx: u64) -> *const u8 {
        let slot = (idx as usize) % self.capacity;
        // SAFETY: Validated in `open`.
        unsafe { self.mapping.ptr.add(HEADER_SIZE + slot * self.slot_stride) }
    }

    /// Close this reader locally (e.g. on `Inlet::drop` or clean EOF) and
    /// wake any blocked `pull`. Unread buffered samples can still be drained.
    pub(crate) fn close_local(&self) {
        self.local_closed.store(true, Ordering::Release);
        let hdr = self.header();
        self.mapping.wake(&hdr.wake_word);
    }

    /// Freeze this reader during recovery (`recover == true`), waiting for any
    /// in-flight `try_pop_into` to complete so `next_expected()` is exact and
    /// no buffered SHM sample duplicates a `resume/1` resend.
    pub(crate) fn close_for_recovery(&self) -> Option<u64> {
        self.discard_closed.store(true, Ordering::SeqCst);
        self.local_closed.store(true, Ordering::SeqCst);
        let hdr = self.header();
        self.mapping.wake(&hdr.wake_word);
        while self.in_pop.load(Ordering::Acquire) != 0 {
            std::hint::spin_loop();
        }
        let exp = self.next_expected.load(Ordering::Acquire);
        (exp != u64::MAX).then_some(exp)
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.local_closed.load(Ordering::Acquire)
            || self.header().closed.load(Ordering::Acquire) != 0
    }

    /// Number of unread samples currently sitting in the ring.
    pub(crate) fn len(&self) -> usize {
        let w = self.header().write_idx.load(Ordering::Acquire);
        let r = self.read_idx.load(Ordering::Acquire);
        (w.saturating_sub(r) as usize).min(self.capacity)
    }

    /// Drops waiting in the ring that have not yet been observed by a `pop`.
    pub(crate) fn pending_drops(&self) -> u64 {
        let w = self.header().write_idx.load(Ordering::Acquire);
        let r = self.read_idx.load(Ordering::Acquire);
        w.saturating_sub(r).saturating_sub(self.capacity as u64)
    }

    /// Total local drops observed plus any currently unconsumed overflow.
    pub(crate) fn total_dropped(&self) -> u64 {
        self.dropped.load(Ordering::Acquire) + self.pending_drops()
    }

    /// Peek the timestamp of the newest sample written into the ring, without
    /// consuming it (used by `Inlet::health` before the first `pull`).
    pub(crate) fn peek_latest_timestamp(&self) -> Option<f64> {
        let w = self.header().write_idx.load(Ordering::Acquire);
        if w == 0 {
            return None;
        }
        let idx = w - 1;
        let ptr = self.slot_ptr(idx);
        // SAFETY: `slot_ptr` points inside the validated mapping.
        unsafe {
            let ver = &*(ptr as *const AtomicU64);
            let v1 = ver.load(Ordering::Acquire);
            if v1 != idx.wrapping_mul(2).wrapping_add(2) {
                return None;
            }
            let ts_bits = std::ptr::read(ptr.add(16) as *const u64);
            fence(Ordering::Acquire);
            if ver.load(Ordering::Acquire) != v1 {
                return None;
            }
            let ts = f64::from_bits(ts_bits);
            ts.is_finite().then_some(ts)
        }
    }

    /// Try to pop one sample directly into `dst` without blocking.
    ///
    /// Returns `Some(ShmSampleMeta)` if a sample was read, or `None` if the
    /// ring is currently empty.
    #[inline]
    pub(crate) fn try_pop_into(&self, dst: &mut [u8]) -> Option<ShmSampleMeta> {
        self.in_pop.store(1, Ordering::SeqCst);
        if self.discard_closed.load(Ordering::Acquire) {
            self.in_pop.store(0, Ordering::Release);
            return None;
        }
        let res = self.try_pop_into_inner(dst);
        if let Some(meta) = res {
            self.next_expected
                .store(meta.sample_seq.wrapping_add(1), Ordering::Release);
        }
        self.in_pop.store(0, Ordering::Release);
        res
    }

    #[inline]
    fn try_pop_into_inner(&self, dst: &mut [u8]) -> Option<ShmSampleMeta> {
        let hdr = self.header();
        let mut lapped_total = 0u64;
        loop {
            let mut r = self.read_idx.load(Ordering::Relaxed);
            let w = hdr.write_idx.load(Ordering::Acquire);
            if r >= w {
                return None;
            }
            let diff = w.wrapping_sub(r);
            if diff > self.capacity as u64 {
                let new_r = w.wrapping_sub(self.capacity as u64);
                let skipped = new_r.wrapping_sub(r);
                lapped_total = lapped_total.saturating_add(skipped);
                r = new_r;
            }
            let ptr = self.slot_ptr(r);
            // SAFETY: Validated slot bounds in `open`.
            let (v1, sample_seq, ts_bits, v2) = unsafe {
                let ver = &*(ptr as *const AtomicU64);
                let v1 = ver.load(Ordering::Acquire);
                if v1 != r.wrapping_mul(2).wrapping_add(2) {
                    // Writer is overwriting slot `r`; advance and retry.
                    let next_r = r.wrapping_add(1);
                    self.read_idx.store(next_r, Ordering::Relaxed);
                    lapped_total = lapped_total.saturating_add(1);
                    continue;
                }
                let sample_seq = std::ptr::read(ptr.add(8) as *const u64);
                let ts_bits = std::ptr::read(ptr.add(16) as *const u64);
                let copy_len = self.payload_bytes.min(dst.len());
                std::ptr::copy_nonoverlapping(
                    ptr.add(SLOT_HEADER_BYTES),
                    dst.as_mut_ptr(),
                    copy_len,
                );
                fence(Ordering::Acquire);
                let v2 = ver.load(Ordering::Acquire);
                (v1, sample_seq, ts_bits, v2)
            };
            if v2 != v1 {
                // Slot changed mid-read; retry from current writer head.
                let next_r = r.wrapping_add(1);
                self.read_idx.store(next_r, Ordering::Relaxed);
                lapped_total = lapped_total.saturating_add(1);
                continue;
            }
            let next_r = r.wrapping_add(1);
            self.read_idx.store(next_r, Ordering::Release);
            hdr.read_idx.store(next_r, Ordering::Release);
            if lapped_total > 0 {
                self.dropped.fetch_add(lapped_total, Ordering::Release);
            }
            return Some(ShmSampleMeta {
                sample_seq,
                timestamp: f64::from_bits(ts_bits),
                lapped_drops: lapped_total,
            });
        }
    }

    /// Update the adaptive period estimator when a sample arrives after waiting.
    #[inline]
    fn note_wait_arrival(&self) {
        let now = mono_ns();
        let prev = self.last_wake_ns.swap(now, Ordering::Relaxed);
        if prev == 0 || now <= prev {
            return;
        }
        let dt = now - prev;
        if !(50_000..=10_000_000).contains(&dt) {
            self.steady_count.store(0, Ordering::Relaxed);
            self.ema_period_ns.store(0, Ordering::Relaxed);
            return;
        }
        let cur = self.ema_period_ns.load(Ordering::Relaxed);
        if cur == 0 {
            self.ema_period_ns.store(dt, Ordering::Relaxed);
            self.steady_count.store(1, Ordering::Relaxed);
        } else {
            let diff = dt.abs_diff(cur);
            if diff <= cur / 3 {
                let next = (cur * 7 + dt) / 8;
                self.ema_period_ns.store(next, Ordering::Relaxed);
                let s = self.steady_count.load(Ordering::Relaxed);
                if s < 32 {
                    self.steady_count.store(s + 1, Ordering::Relaxed);
                }
            } else {
                self.ema_period_ns.store(dt, Ordering::Relaxed);
                self.steady_count.store(1, Ordering::Relaxed);
            }
        }
    }

    /// Wait until at least one sample is available, the segment closes, or
    /// `timeout` elapses.
    fn wait_until(&self, deadline: Instant) -> bool {
        let hdr = self.header();
        // Brief spin catches tight back-to-back pushes without entering the kernel.
        for _ in 0..32 {
            if hdr.write_idx.load(Ordering::Acquire) > self.read_idx.load(Ordering::Relaxed) {
                return true;
            }
            if self.is_closed() {
                return false;
            }
            std::hint::spin_loop();
        }

        // On Linux, for steady high-rate streams (>= 200 Hz), sleep in
        // `FUTEX_WAIT` until ~25 us before the predicted next arrival, then
        // catch the sample in a bounded <= 45 us spin window. This keeps
        // cross-process p50 latency around 1-3 us while skipping the writer's
        // `FUTEX_WAKE` syscall and staying at 0% CPU when the stream pauses.
        #[cfg(target_os = "linux")]
        {
            let steady = self.steady_count.load(Ordering::Relaxed);
            let period = self.ema_period_ns.load(Ordering::Relaxed);
            if steady >= 6 && (100_000..=5_000_000).contains(&period) {
                set_low_timer_slack();
                let last = self.last_wake_ns.load(Ordering::Relaxed);
                let now = mono_ns();
                let lead_ns = 25_000u64;
                let target = last.saturating_add(period);
                if target > now.saturating_add(lead_ns + 15_000) {
                    let sleep_ns = target - now - lead_ns;
                    let left = deadline.saturating_duration_since(Instant::now());
                    let wait = Duration::from_nanos(sleep_ns).min(left);
                    if !wait.is_zero() {
                        hdr.waiters.fetch_add(1, Ordering::SeqCst);
                        let wake_snap = hdr.wake_word.load(Ordering::Acquire);
                        if hdr.write_idx.load(Ordering::Acquire)
                            > self.read_idx.load(Ordering::Relaxed)
                        {
                            hdr.waiters.fetch_sub(1, Ordering::Release);
                            self.note_wait_arrival();
                            return true;
                        }
                        if self.is_closed() {
                            hdr.waiters.fetch_sub(1, Ordering::Release);
                            return false;
                        }
                        self.mapping.wait(&hdr.wake_word, wake_snap, wait);
                        hdr.waiters.fetch_sub(1, Ordering::Release);
                        if hdr.write_idx.load(Ordering::Acquire)
                            > self.read_idx.load(Ordering::Relaxed)
                        {
                            self.note_wait_arrival();
                            return true;
                        }
                        if self.is_closed() {
                            return false;
                        }
                    }
                }
                // Bounded short spin (<= 45 us) around the predicted arrival.
                let spin_end = mono_ns().saturating_add(45_000);
                while mono_ns() < spin_end {
                    for _ in 0..16 {
                        if hdr.write_idx.load(Ordering::Acquire)
                            > self.read_idx.load(Ordering::Relaxed)
                        {
                            self.note_wait_arrival();
                            return true;
                        }
                        if self.is_closed() {
                            return false;
                        }
                        std::hint::spin_loop();
                    }
                }
                // Missed prediction (stream paused or slowed): drop back to
                // zero-CPU blocking futex wait.
                self.steady_count.store(0, Ordering::Relaxed);
            }
        }

        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            hdr.waiters.fetch_add(1, Ordering::SeqCst);
            let wake_snap = hdr.wake_word.load(Ordering::Acquire);
            if hdr.write_idx.load(Ordering::Acquire) > self.read_idx.load(Ordering::Relaxed) {
                hdr.waiters.fetch_sub(1, Ordering::Release);
                self.note_wait_arrival();
                return true;
            }
            if self.is_closed() {
                hdr.waiters.fetch_sub(1, Ordering::Release);
                return false;
            }
            self.mapping.wait(&hdr.wake_word, wake_snap, left);
            hdr.waiters.fetch_sub(1, Ordering::Release);
            if hdr.write_idx.load(Ordering::Acquire) > self.read_idx.load(Ordering::Relaxed) {
                self.note_wait_arrival();
                return true;
            }
            if self.is_closed() {
                return false;
            }
        }
    }

    /// Pop one sample directly into `dst` (zero heap allocation), waiting up
    /// to `timeout`.
    pub(crate) fn pop_into(&self, dst: &mut [u8], timeout: Duration) -> Option<ShmSampleMeta> {
        if let Some(meta) = self.try_pop_into(dst) {
            return Some(meta);
        }
        if timeout.is_zero() || self.is_closed() {
            return None;
        }
        let deadline = Instant::now() + timeout;
        while self.wait_until(deadline) {
            if let Some(meta) = self.try_pop_into(dst) {
                return Some(meta);
            }
        }
        self.try_pop_into(dst)
    }

    /// Pop one [`PackedSample`], waiting up to `timeout`.
    pub(crate) fn pop_packed(&self, timeout: Duration) -> Option<(PackedSample, ShmSampleMeta)> {
        let mut buf = vec![0u8; self.payload_bytes];
        let meta = self.pop_into(&mut buf, timeout)?;
        let sample = PackedSample::from_raw_bytes(meta.timestamp, self.format, Arc::from(buf));
        Some((sample, meta))
    }

    /// Pop up to `max` waiting [`PackedSample`]s, waiting up to `timeout` for
    /// the first sample only.
    pub(crate) fn pop_many_packed(
        &self,
        out: &mut Vec<PackedSample>,
        metas: &mut Vec<ShmSampleMeta>,
        max: usize,
        timeout: Duration,
    ) -> usize {
        if max == 0 {
            return 0;
        }
        let Some((first_sample, first_meta)) = self.pop_packed(timeout) else {
            return 0;
        };
        out.push(first_sample);
        metas.push(first_meta);
        let mut taken = 1usize;
        while taken < max {
            let mut buf = vec![0u8; self.payload_bytes];
            match self.try_pop_into(&mut buf) {
                Some(meta) => {
                    out.push(PackedSample::from_raw_bytes(
                        meta.timestamp,
                        self.format,
                        Arc::from(buf),
                    ));
                    metas.push(meta);
                    taken += 1;
                }
                None => break,
            }
        }
        taken
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shm_ring_round_trips_exact_bits_and_drop_oldest_semantics() {
        let writer = ShmWriter::create(Format::Float32, 4, 8).expect("create shm");
        let reader = ShmReader::open(writer.id(), Format::Float32, 4).expect("open shm");
        assert_eq!(writer.token(), reader.token());
        assert_eq!(writer.channels(), 4);
        assert_eq!(reader.capacity(), 8);
        writer.unlink_name();

        // Push 4 samples and verify bit-exact round-trip including -0.0 and NaN.
        let nan = f32::from_bits(0x7fc0_4321);
        let row = [1.5f32, -0.0, nan, -42.25];
        let mut raw = Vec::new();
        for v in row {
            raw.extend_from_slice(&v.to_ne_bytes());
        }
        writer.push_raw_one(10, 123.456, &raw, None);

        let mut dst = vec![0u8; 16];
        let meta = reader.pop_into(&mut dst, Duration::from_secs(1)).unwrap();
        assert_eq!(meta.sample_seq, 10);
        assert_eq!(meta.timestamp, 123.456);
        assert_eq!(meta.lapped_drops, 0);
        assert_eq!(dst, raw);

        // Overflow the 8-slot ring with 20 individual pushes -> oldest 12 dropped.
        for k in 0..20u64 {
            let v = [k as f32, 0.0, 0.0, 0.0];
            let mut b = Vec::new();
            for x in v {
                b.extend_from_slice(&x.to_ne_bytes());
            }
            writer.push_raw_one(100 + k, 200.0 + k as f64, &b, None);
        }
        assert_eq!(reader.total_dropped(), 12);
        let first = reader.pop_into(&mut dst, Duration::ZERO).unwrap();
        assert_eq!(first.sample_seq, 112);
        assert_eq!(first.lapped_drops, 12);
        let f0 = f32::from_ne_bytes([dst[0], dst[1], dst[2], dst[3]]);
        assert_eq!(f0, 12.0);
    }

    #[test]
    fn shm_cross_thread_wakeup_wakes_promptly() {
        let writer = Arc::new(ShmWriter::create(Format::Float32, 64, 64).expect("create"));
        let reader = ShmReader::open(writer.id(), Format::Float32, 64).expect("open");
        writer.unlink_name();

        let w = Arc::clone(&writer);
        let producer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            let raw = vec![0u8; 64 * 4];
            w.push_raw_one(0, 99.5, &raw, None);
        });

        let mut dst = vec![0u8; 64 * 4];
        let meta = reader
            .pop_into(&mut dst, Duration::from_secs(2))
            .expect("woken sample");
        assert_eq!(meta.sample_seq, 0);
        assert_eq!(meta.timestamp, 99.5);
        producer.join().unwrap();
    }

    #[test]
    fn shm_reply_parser_accepts_only_exact_token() {
        assert_eq!(
            parse_shm_ok(b"shm-ok 0123456789abcdef"),
            Some(0x0123_4567_89ab_cdef)
        );
        assert_eq!(parse_shm_ok(b"shm-no"), None);
        assert_eq!(parse_shm_ok(b"shm-ok 1234"), None);
        assert_eq!(parse_shm_ok(b"shm-ok 0123456789ABCDEF"), None);
    }
}
