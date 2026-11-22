//! Cross-platform network interface and neighbor enumeration.
//!
//! liblsl enumerates interfaces with `GetAdaptersAddresses` on Windows and
//! `getifaddrs` on Linux and macOS (`src/netinterfaces.cpp:21-114`). This
//! module gives the same interface list on Unix and Windows, plus the IPv4
//! directed broadcast address and local ARP neighbors for fallback discovery
//! (`docs/discovery.md`).

use std::net::Ipv4Addr;

/// One local IPv4 interface address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ipv4Interface {
    /// Operating system name of the interface, or an index label on Windows.
    pub name: String,
    /// Local IPv4 address.
    pub addr: Ipv4Addr,
    /// Subnet prefix length (0 to 32).
    pub prefix_len: u8,
    /// Directed subnet broadcast address, when the prefix is below 31.
    pub broadcast: Option<Ipv4Addr>,
    /// Interface index.
    pub ifindex: u32,
    /// True when the interface is up.
    pub is_up: bool,
    /// True when the interface is loopback.
    pub is_loopback: bool,
    /// True when the interface supports multicast.
    pub supports_multicast: bool,
}

impl Ipv4Interface {
    /// Host addresses in this subnet, excluding network, broadcast, and self.
    ///
    /// Returns an empty list for loopback, down interfaces, or subnets larger
    /// than `/24` (`prefix_len < 24`). A corporate `/16` is never swept.
    pub fn subnet_peers(&self) -> Vec<Ipv4Addr> {
        if !self.is_up || self.is_loopback || self.prefix_len < 24 || self.prefix_len >= 31 {
            return Vec::new();
        }
        let mask = prefix_to_mask(self.prefix_len);
        let ip = u32::from(self.addr);
        let net = ip & mask;
        let bcast = net | !mask;
        let mut out = Vec::new();
        for host in (net + 1)..bcast {
            if host != ip {
                out.push(Ipv4Addr::from(host));
            }
        }
        out
    }
}

/// Convert a prefix length (0 to 32) to a big-endian IPv4 bitmask.
pub fn prefix_to_mask(prefix_len: u8) -> u32 {
    if prefix_len == 0 {
        0
    } else if prefix_len >= 32 {
        u32::MAX
    } else {
        !0u32 << (32 - prefix_len)
    }
}

/// Compute the directed broadcast address from an IPv4 address and prefix.
pub fn directed_broadcast(addr: Ipv4Addr, prefix_len: u8) -> Option<Ipv4Addr> {
    if addr.is_loopback() || prefix_len == 0 || prefix_len >= 31 {
        return None;
    }
    let mask = prefix_to_mask(prefix_len);
    Some(Ipv4Addr::from(u32::from(addr) | !mask))
}

/// Every active IPv4 interface on this machine.
pub fn local_ipv4_interfaces() -> Vec<Ipv4Interface> {
    platform_ipv4_interfaces()
}

/// Every local IPv4 address on this machine, for multicast binds and sends.
pub fn local_ipv4_addresses() -> Vec<Ipv4Addr> {
    let mut out = Vec::new();
    for iface in local_ipv4_interfaces() {
        if iface.is_up && !iface.addr.is_broadcast() && !out.contains(&iface.addr) {
            out.push(iface.addr);
        }
    }
    out
}

/// Every interface index on this machine, with zero at the front.
pub fn local_interface_indexes() -> Vec<u32> {
    platform_interface_indexes()
}

/// Local IPv4 neighbors from the operating system ARP table.
pub fn local_ipv4_neighbors() -> Vec<Ipv4Addr> {
    platform_ipv4_neighbors()
}

/// Parse Linux `/proc/net/arp` text into valid unicast IPv4 neighbors.
pub fn parse_proc_arp(text: &str) -> Vec<Ipv4Addr> {
    let mut out = Vec::new();
    for line in text.lines().skip(1) {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 4 {
            continue;
        }
        // Flags column: 0x0 means incomplete.
        if cols[2] == "0x0" {
            continue;
        }
        // Hardware address column: all zeroes means unresolved.
        if cols[3] == "00:00:00:00:00:00" {
            continue;
        }
        if let Ok(ip) = cols[0].parse::<Ipv4Addr>() {
            if !ip.is_loopback()
                && !ip.is_multicast()
                && !ip.is_broadcast()
                && !ip.is_unspecified()
                && !out.contains(&ip)
            {
                out.push(ip);
            }
        }
    }
    out
}

#[cfg(unix)]
fn platform_ipv4_interfaces() -> Vec<Ipv4Interface> {
    use std::ffi::CStr;
    let mut out = Vec::new();
    #[allow(unsafe_code)]
    {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        // SAFETY: `getifaddrs` allocates a linked list of `ifaddrs` entries and
        // writes the head pointer into `ifap`. We free it with `freeifaddrs`.
        if unsafe { libc::getifaddrs(&mut ifap) } != 0 {
            return out;
        }
        let mut cur = ifap;
        while !cur.is_null() {
            // SAFETY: `cur` is a valid non-null node from `getifaddrs`.
            let ifa = unsafe { &*cur };
            cur = ifa.ifa_next;
            if ifa.ifa_addr.is_null() {
                continue;
            }
            // SAFETY: `ifa_addr` is non-null and holds a valid `sockaddr`.
            let family = unsafe { (*ifa.ifa_addr).sa_family } as i32;
            if family != libc::AF_INET {
                continue;
            }
            // SAFETY: `sa_family == AF_INET`, so `ifa_addr` points to `sockaddr_in`.
            let sin = unsafe { &*(ifa.ifa_addr as *const libc::sockaddr_in) };
            let addr = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
            let prefix_len = if !ifa.ifa_netmask.is_null() {
                // SAFETY: `ifa_netmask` is non-null for an AF_INET entry.
                let m = unsafe { &*(ifa.ifa_netmask as *const libc::sockaddr_in) };
                u32::from_be(m.sin_addr.s_addr).leading_ones() as u8
            } else {
                32
            };
            let flags = ifa.ifa_flags as i32;
            let is_up = (flags & libc::IFF_UP) != 0;
            let is_loopback = (flags & libc::IFF_LOOPBACK) != 0 || addr.is_loopback();
            let supports_multicast = (flags & libc::IFF_MULTICAST) != 0;
            let name = if !ifa.ifa_name.is_null() {
                // SAFETY: `ifa_name` is a NUL-terminated C string.
                unsafe { CStr::from_ptr(ifa.ifa_name) }
                    .to_string_lossy()
                    .into_owned()
            } else {
                String::new()
            };
            // SAFETY: `ifa_name` is either null or a valid C string.
            let ifindex = if !ifa.ifa_name.is_null() {
                unsafe { libc::if_nametoindex(ifa.ifa_name) }
            } else {
                0
            };
            let broadcast = if is_up && !is_loopback {
                directed_broadcast(addr, prefix_len)
            } else {
                None
            };
            out.push(Ipv4Interface {
                name,
                addr,
                prefix_len,
                broadcast,
                ifindex,
                is_up,
                is_loopback,
                supports_multicast,
            });
        }
        // SAFETY: `ifap` was returned by `getifaddrs` above.
        unsafe { libc::freeifaddrs(ifap) };
    }
    out
}

#[cfg(unix)]
fn platform_interface_indexes() -> Vec<u32> {
    let mut out = vec![0u32];
    #[allow(unsafe_code)]
    {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        // SAFETY: `getifaddrs` allocates a linked list freed with `freeifaddrs`.
        if unsafe { libc::getifaddrs(&mut ifap) } == 0 {
            let mut cur = ifap;
            while !cur.is_null() {
                // SAFETY: `cur` is a valid non-null node from `getifaddrs`.
                let ifa = unsafe { &*cur };
                cur = ifa.ifa_next;
                if ifa.ifa_name.is_null() {
                    continue;
                }
                let flags = ifa.ifa_flags as i32;
                if (flags & libc::IFF_UP) == 0 {
                    continue;
                }
                // SAFETY: `ifa_name` is a NUL-terminated C string.
                let idx = unsafe { libc::if_nametoindex(ifa.ifa_name) };
                if idx != 0 && !out.contains(&idx) {
                    out.push(idx);
                }
            }
            // SAFETY: `ifap` was returned by `getifaddrs` above.
            unsafe { libc::freeifaddrs(ifap) };
        }
    }
    out
}

#[cfg(unix)]
fn platform_ipv4_neighbors() -> Vec<Ipv4Addr> {
    if let Ok(text) = std::fs::read_to_string("/proc/net/arp") {
        return parse_proc_arp(&text);
    }
    Vec::new()
}

#[cfg(windows)]
mod win_iphlp {
    /// `MIB_IPADDRROW` from `ipmib.h`. Fixed 24 bytes on x86, x64, and ARM64.
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct MibIpAddrRow {
        pub dw_addr: u32,
        pub dw_index: u32,
        pub dw_mask: u32,
        pub dw_bcast_addr: u32,
        pub dw_reasm_size: u32,
        pub unused1: u16,
        pub w_type: u16,
    }

    /// `MIB_IPNETROW` from `ipmib.h`. Fixed 24 bytes on x86, x64, and ARM64.
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct MibIpNetRow {
        pub dw_index: u32,
        pub dw_phys_addr_len: u32,
        pub b_phys_addr: [u8; 8],
        pub dw_addr: u32,
        pub dw_type: u32,
    }

    pub const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
    pub const NO_ERROR: u32 = 0;
    pub const MIB_IPNET_TYPE_DYNAMIC: u32 = 3;
    pub const MIB_IPNET_TYPE_STATIC: u32 = 4;

    #[link(name = "iphlpapi")]
    extern "system" {
        pub fn GetIpAddrTable(p_ip_addr_table: *mut u8, pdw_size: *mut u32, b_order: i32) -> u32;
        pub fn GetIpNetTable(p_ip_net_table: *mut u8, pdw_size: *mut u32, b_order: i32) -> u32;
    }
}

#[cfg(windows)]
fn platform_ipv4_interfaces() -> Vec<Ipv4Interface> {
    use win_iphlp::*;
    let mut out = Vec::new();
    #[allow(unsafe_code)]
    {
        let mut size: u32 = 0;
        // SAFETY: query required buffer size with null output pointer.
        let rc = unsafe { GetIpAddrTable(std::ptr::null_mut(), &mut size, 0) };
        if rc != ERROR_INSUFFICIENT_BUFFER || size < 4 {
            return out;
        }
        let mut buf = vec![0u8; size as usize];
        // SAFETY: `buf` has at least `size` writable bytes.
        let rc = unsafe { GetIpAddrTable(buf.as_mut_ptr(), &mut size, 0) };
        if rc != NO_ERROR || size < 4 {
            return out;
        }
        let count = u32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        let row_size = std::mem::size_of::<MibIpAddrRow>();
        let avail = (buf.len().saturating_sub(4)) / row_size;
        let n = count.min(avail);
        for i in 0..n {
            let offset = 4 + i * row_size;
            // SAFETY: `offset + row_size <= buf.len()` and `MibIpAddrRow` is `Copy`.
            let row = unsafe {
                std::ptr::read_unaligned(buf.as_ptr().add(offset) as *const MibIpAddrRow)
            };
            let addr = Ipv4Addr::from(u32::from_be(row.dw_addr));
            if addr.is_unspecified() {
                continue;
            }
            let mask = u32::from_be(row.dw_mask);
            let prefix_len = mask.leading_ones() as u8;
            let is_loopback = addr.is_loopback();
            let broadcast = if !is_loopback {
                directed_broadcast(addr, prefix_len)
            } else {
                None
            };
            out.push(Ipv4Interface {
                name: format!("if{}", row.dw_index),
                addr,
                prefix_len,
                broadcast,
                ifindex: row.dw_index,
                is_up: true,
                is_loopback,
                supports_multicast: !is_loopback,
            });
        }
    }
    out
}

#[cfg(windows)]
fn platform_interface_indexes() -> Vec<u32> {
    let mut out = vec![0u32];
    for iface in platform_ipv4_interfaces() {
        if iface.ifindex != 0 && !out.contains(&iface.ifindex) {
            out.push(iface.ifindex);
        }
    }
    out
}

#[cfg(windows)]
fn platform_ipv4_neighbors() -> Vec<Ipv4Addr> {
    use win_iphlp::*;
    let mut out = Vec::new();
    #[allow(unsafe_code)]
    {
        let mut size: u32 = 0;
        // SAFETY: query required buffer size with null output pointer.
        let rc = unsafe { GetIpNetTable(std::ptr::null_mut(), &mut size, 0) };
        if rc != ERROR_INSUFFICIENT_BUFFER || size < 4 {
            return out;
        }
        let mut buf = vec![0u8; size as usize];
        // SAFETY: `buf` has at least `size` writable bytes.
        let rc = unsafe { GetIpNetTable(buf.as_mut_ptr(), &mut size, 0) };
        if rc != NO_ERROR || size < 4 {
            return out;
        }
        let count = u32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        let row_size = std::mem::size_of::<MibIpNetRow>();
        let avail = (buf.len().saturating_sub(4)) / row_size;
        let n = count.min(avail);
        for i in 0..n {
            let offset = 4 + i * row_size;
            // SAFETY: `offset + row_size <= buf.len()` and `MibIpNetRow` is `Copy`.
            let row =
                unsafe { std::ptr::read_unaligned(buf.as_ptr().add(offset) as *const MibIpNetRow) };
            if row.dw_type != MIB_IPNET_TYPE_DYNAMIC && row.dw_type != MIB_IPNET_TYPE_STATIC {
                continue;
            }
            let ip = Ipv4Addr::from(u32::from_be(row.dw_addr));
            if !ip.is_loopback()
                && !ip.is_multicast()
                && !ip.is_broadcast()
                && !ip.is_unspecified()
                && !out.contains(&ip)
            {
                out.push(ip);
            }
        }
    }
    out
}

#[cfg(not(any(unix, windows)))]
fn platform_ipv4_interfaces() -> Vec<Ipv4Interface> {
    Vec::new()
}

#[cfg(not(any(unix, windows)))]
fn platform_interface_indexes() -> Vec<u32> {
    vec![0]
}

#[cfg(not(any(unix, windows)))]
fn platform_ipv4_neighbors() -> Vec<Ipv4Addr> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subnet_peers_covers_slash_24_and_skips_larger_subnets() {
        let make = |addr: Ipv4Addr, prefix_len: u8, is_loopback: bool| Ipv4Interface {
            name: "eth0".to_string(),
            addr,
            prefix_len,
            broadcast: directed_broadcast(addr, prefix_len),
            ifindex: 2,
            is_up: true,
            is_loopback,
            supports_multicast: true,
        };
        let lan = make(Ipv4Addr::new(192, 168, 2, 10), 24, false);
        assert_eq!(lan.broadcast, Some(Ipv4Addr::new(192, 168, 2, 255)));
        let peers = lan.subnet_peers();
        assert_eq!(peers.len(), 253);
        assert!(peers.contains(&Ipv4Addr::new(192, 168, 2, 1)));
        assert!(peers.contains(&Ipv4Addr::new(192, 168, 2, 254)));
        assert!(!peers.contains(&Ipv4Addr::new(192, 168, 2, 10)));
        assert!(!peers.contains(&Ipv4Addr::new(192, 168, 2, 0)));
        assert!(!peers.contains(&Ipv4Addr::new(192, 168, 2, 255)));

        // A /16 corporate subnet must not be swept.
        let big = make(Ipv4Addr::new(10, 0, 1, 5), 16, false);
        assert_eq!(big.broadcast, Some(Ipv4Addr::new(10, 0, 255, 255)));
        assert!(big.subnet_peers().is_empty());

        // Loopback is never swept.
        let lo = make(Ipv4Addr::LOCALHOST, 8, true);
        assert!(lo.subnet_peers().is_empty());

        // A /30 has two usable hosts: 1 peer.
        let p2p = make(Ipv4Addr::new(10, 77, 0, 1), 30, false);
        assert_eq!(p2p.subnet_peers(), vec![Ipv4Addr::new(10, 77, 0, 2)]);
    }

    #[test]
    fn parse_proc_arp_keeps_valid_unicast_entries_only() {
        let sample = "\
IP address       HW type     Flags       HW address            Mask     Device
192.168.2.1      0x1         0x2         aa:bb:cc:dd:ee:01     *        wlan0
192.168.2.208    0x1         0x2         aa:bb:cc:dd:ee:02     *        wlan0
192.168.2.99     0x1         0x0         00:00:00:00:00:00     *        wlan0
127.0.0.1        0x1         0x2         00:00:00:00:00:01     *        lo
224.0.0.183      0x1         0x2         01:00:5e:00:00:b7     *        wlan0
";
        assert_eq!(
            parse_proc_arp(sample),
            vec![
                Ipv4Addr::new(192, 168, 2, 1),
                Ipv4Addr::new(192, 168, 2, 208),
            ]
        );
    }

    #[test]
    fn local_ipv4_interfaces_finds_loopback() {
        let list = local_ipv4_interfaces();
        assert!(
            list.iter().any(|i| i.addr == Ipv4Addr::LOCALHOST),
            "expected loopback in {list:?}"
        );
    }
}
